//! OTA server for XiaoZhi devices (Phase 7, §8.2).
//!
//! One law, one exception. By default the response builder
//! returns ONLY safe URLs: `websocket.url` starts with
//! `wss://` (future firmware URLs would be `https://`,
//! future MQTT would be port 8883). The builder does NOT
//! take a URL from free-form config: URLs are built from the
//! hub address + a fixed scheme; config may change host/port
//! only, never the scheme. The response contains NO `mqtt`
//! object (its absence is what makes the firmware choose
//! WebSocket) and NO `firmware` object (no firmware upgrades
//! through OTA in v1).
//!
//! The single exception is lab mode (`lab_mode=true`,
//! explicitly enabled by the user in `[hub.xiaozhi]`,
//! loopback/tailnet only): the response may carry `ws://` to
//! a loopback/tailnet address, and the UI shows a continuous
//! warning while lab mode is on.
//!
//! OTA auth is weak by design of the firmware (Device-Id +
//! Client-Id headers, no signature — both spoofable on a
//! LAN), so the endpoint additionally requires: a registered
//! UUID (allow-list, user-entered in the hub UI), at most one
//! check per UUID per 60 seconds, and a session token that
//! expires after 1 hour and is good for exactly one WS
//! connection. The token grants no driver rights.

use crate::audit::AuditLog;
use crate::error::MimintError;
use crate::friends::address::{is_accepted_address, is_cgnat, is_unique_local};
use crate::friends::registry::DeviceRegistry;
use crate::friends::util::{hex_encode, random_bytes, Clock};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{IpAddr, TcpStream};
use std::sync::{Arc, Mutex};

/// OTA check rate limit (§8.2): 1 check per 60s per UUID.
pub const OTA_RATE_LIMIT_MS: u64 = 60_000;
/// OTA session token lifetime (§8.2): 1 hour.
pub const OTA_TOKEN_TTL_MS: u64 = 60 * 60 * 1000;
/// OTA HTTP header block cap.
pub const OTA_MAX_HEADER: usize = 16 * 1024;

/// The OTA response contract: `websocket { url, token }` and
/// nothing else — no `mqtt`, no `firmware`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OtaResponse {
    pub websocket: OtaWebsocket,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OtaWebsocket {
    pub url: String,
    pub token: String,
}

/// Build the OTA websocket URL from the hub address + a fixed
/// scheme. `lab_mode` allows `ws://` ONLY for loopback/tailnet
/// addresses; anything else (including a public address, or
/// lab mode aimed at a public address) is refused.
pub fn build_websocket_url(hub_addr: &str, lab_mode: bool) -> Result<String, MimintError> {
    let (host, port) = hub_addr
        .rsplit_once(':')
        .ok_or_else(|| MimintError::Config(format!("bad hub address {hub_addr}")))?;
    let ip: IpAddr = host
        .parse()
        .map_err(|_| MimintError::Config(format!("hub address host is not an IP: {hub_addr}")))?;
    if !is_accepted_address(ip) {
        return Err(MimintError::Config(format!(
            "OTA refuses hub address {hub_addr}: not a private/tailnet address"
        )));
    }
    // Lab mode lifts the TLS requirement ONLY for
    // loopback/tailnet addresses (§8.2): a private LAN
    // address still gets wss:// even in lab mode (the hub
    // LAN certificate is BLOCKED work — XiaoZhi LAN outside
    // the lab is not served in Phase 7).
    let lab_plain = lab_mode
        && match ip {
            IpAddr::V4(v4) => v4.is_loopback() || is_cgnat(v4),
            IpAddr::V6(v6) => v6.is_loopback() || is_unique_local(v6),
        };
    let scheme = if lab_plain { "ws" } else { "wss" };
    let _ = port;
    Ok(format!("{scheme}://{hub_addr}/xiaozhi"))
}

/// One session token: 1 hour, single use, one WS connection.
#[derive(Debug, Clone)]
struct OtaToken {
    uuid: String,
    expires_ms: u64,
    used: bool,
}

/// The OTA server state: token store + per-UUID rate limits.
pub struct OtaServer {
    registry: Arc<DeviceRegistry>,
    tokens: Mutex<HashMap<String, OtaToken>>,
    last_check_ms: Mutex<HashMap<String, u64>>,
    clock: Clock,
    audit: Option<Arc<AuditLog>>,
}

impl OtaServer {
    pub fn new(registry: Arc<DeviceRegistry>, clock: Clock, audit: Option<Arc<AuditLog>>) -> Self {
        Self {
            registry,
            tokens: Mutex::new(HashMap::new()),
            last_check_ms: Mutex::new(HashMap::new()),
            clock,
            audit,
        }
    }

    fn audit(&self, kind: &str, detail: serde_json::Value) {
        if let Some(log) = &self.audit {
            let _ = log.log("friends-ota", kind, Some("friends"), None, detail);
        }
    }

    /// Issue a session token for a UUID, enforcing the
    /// allow-list and the 1-check/60s rate limit.
    pub fn issue_token(&self, uuid: &str) -> Result<String, MimintError> {
        let now = self.clock.now_ms();
        if !self.registry.xiaozhi_uuid_known(uuid)? {
            self.audit("ota_unknown_uuid", serde_json::json!({"uuid": uuid}));
            return Err(MimintError::Config("unknown device UUID".to_string()));
        }
        {
            let mut last = self.last_check_ms.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(previous) = last.get(uuid) {
                if now.saturating_sub(*previous) < OTA_RATE_LIMIT_MS {
                    self.audit("ota_rate_limited", serde_json::json!({"uuid": uuid}));
                    return Err(MimintError::Config(
                        "OTA check rate limited: one check per 60 seconds per device".to_string(),
                    ));
                }
            }
            last.insert(uuid.to_string(), now);
        }
        let mut bytes = [0u8; 16];
        random_bytes(&mut bytes);
        let token = hex_encode(&bytes);
        let mut tokens = self.tokens.lock().unwrap_or_else(|p| p.into_inner());
        tokens.insert(
            token.clone(),
            OtaToken {
                uuid: uuid.to_string(),
                expires_ms: now + OTA_TOKEN_TTL_MS,
                used: false,
            },
        );
        self.audit("ota_token_issued", serde_json::json!({"uuid": uuid}));
        Ok(token)
    }

    /// Consume a token for one WS connection: valid iff it
    /// exists, is unexpired, and was never used.
    pub fn consume_token(&self, token: &str) -> Option<String> {
        let now = self.clock.now_ms();
        let mut tokens = self.tokens.lock().unwrap_or_else(|p| p.into_inner());
        let entry = tokens.get_mut(token)?;
        if entry.used || now >= entry.expires_ms {
            return None;
        }
        entry.used = true;
        Some(entry.uuid.clone())
    }

    /// Build the OTA response for a UUID: allow-list + rate
    /// limit + token + safe-URL law, in that order.
    pub fn check(
        &self,
        uuid: &str,
        hub_addr: &str,
        lab_mode: bool,
    ) -> Result<OtaResponse, MimintError> {
        let token = self.issue_token(uuid)?;
        let url = build_websocket_url(hub_addr, lab_mode)?;
        Ok(OtaResponse {
            websocket: OtaWebsocket {
                url: format!("{url}?token={token}"),
                token,
            },
        })
    }
}

/// Extract the `token` query parameter from a WS request path
/// (`/xiaozhi?token=...`).
pub fn extract_token_from_path(path: &str) -> Option<String> {
    let query = path.split_once('?')?.1;
    for pair in query.split('&') {
        if let Some(value) = pair.strip_prefix("token=") {
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// Handle ONE OTA HTTP connection (GET /ota/check). Reads the
/// headers, routes, and writes the JSON response. Unknown
/// UUIDs get a generic error (no detail about why); unknown
/// paths get 404. The connection is closed afterwards.
pub fn handle_ota_connection(
    mut stream: TcpStream,
    server: &OtaServer,
    hub_addr: &str,
    lab_mode: bool,
) {
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(10)));
    let mut buf = Vec::with_capacity(1024);
    let mut byte = [0u8; 1];
    let mut ok = false;
    while buf.len() < OTA_MAX_HEADER {
        match stream.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {
                buf.push(byte[0]);
                if buf.len() >= 4 && &buf[buf.len() - 4..] == b"\r\n\r\n" {
                    ok = true;
                    break;
                }
            }
            Err(_) => break,
        }
    }
    if !ok {
        return;
    }
    let text = match String::from_utf8(buf) {
        Ok(text) => text,
        Err(_) => {
            write_http(&mut stream, 400, "bad request");
            return;
        }
    };
    let mut lines = text.lines();
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");
    let mut device_id: Option<String> = None;
    let mut client_id: Option<String> = None;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            match name.trim().to_ascii_lowercase().as_str() {
                "device-id" => device_id = Some(value.trim().to_string()),
                "client-id" => client_id = Some(value.trim().to_string()),
                _ => {}
            }
        }
    }
    if method != "GET" || path != "/ota/check" {
        write_http(&mut stream, 404, "not found");
        return;
    }
    let uuid = match client_id {
        Some(uuid) if !uuid.is_empty() => uuid,
        _ => {
            write_http(&mut stream, 400, "missing Client-Id");
            return;
        }
    };
    let _ = device_id;
    match server.check(&uuid, hub_addr, lab_mode) {
        Ok(response) => {
            let body = serde_json::to_string(&response).unwrap_or_default();
            write_http_json(&mut stream, 200, &body);
        }
        Err(_) => {
            // Generic error: no detail about whether the UUID
            // is unknown or the check was rate limited.
            write_http(&mut stream, 403, "forbidden");
        }
    }
}

fn write_http(stream: &mut TcpStream, status: u16, body: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Error",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

fn write_http_json(stream: &mut TcpStream, status: u16, body: &str) {
    let response = format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_registry() -> Arc<DeviceRegistry> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path =
            std::env::temp_dir().join(format!("mimint-ota-{}-{nanos}.db", std::process::id()));
        Arc::new(DeviceRegistry::open(path).expect("open registry"))
    }

    #[test]
    fn ota_only_safe_urls() {
        // Production: wss only, never ws, never http.
        let url = build_websocket_url("192.168.1.10:19121", false).unwrap();
        assert!(url.starts_with("wss://"), "got {url}");
        // Lab mode: ws allowed, loopback/tailnet only.
        let lab = build_websocket_url("127.0.0.1:19121", true).unwrap();
        assert!(lab.starts_with("ws://"), "got {lab}");
        let lab_tailscale = build_websocket_url("100.64.0.5:19121", true).unwrap();
        assert!(lab_tailscale.starts_with("ws://"));
        // Lab mode aimed at a public address: refused.
        assert!(build_websocket_url("8.8.8.8:19121", true).is_err());
        // Production aimed at a public address: refused.
        assert!(build_websocket_url("8.8.8.8:19121", false).is_err());
        // Lab mode on a LAN address: the address is
        // accepted, but the lab ws:// exception covers only
        // loopback/tailnet (§8.2) — LAN still gets wss://.
        let lab_lan = build_websocket_url("192.168.1.10:19121", true).unwrap();
        assert!(lab_lan.starts_with("wss://"), "got {lab_lan}");
    }

    #[test]
    fn ota_forces_websocket() {
        let registry = temp_registry();
        registry
            .register_xiaozhi_uuid("uuid-1", "Board 1", 1000)
            .unwrap();
        let server = OtaServer::new(registry, Clock::fixed(10_000), None);
        let response = server.check("uuid-1", "192.168.1.10:19121", false).unwrap();
        let json = serde_json::to_value(&response).unwrap();
        assert!(json.get("mqtt").is_none(), "OTA must not carry mqtt");
        assert!(
            json.get("firmware").is_none(),
            "OTA must not carry firmware"
        );
        assert!(json["websocket"]["url"]
            .as_str()
            .unwrap()
            .starts_with("wss://"));
        assert!(!json["websocket"]["token"].as_str().unwrap().is_empty());
        // The token is single-use.
        let token = json["websocket"]["token"].as_str().unwrap().to_string();
        assert!(server.consume_token(&token).is_some());
        assert!(server.consume_token(&token).is_none());
    }

    #[test]
    fn ota_unknown_uuid_rejected() {
        let registry = temp_registry();
        let server = OtaServer::new(registry, Clock::fixed(10_000), None);
        assert!(server
            .check("uuid-nobody", "192.168.1.10:19121", false)
            .is_err());
        // Rate limit: one check per 60s per UUID.
        let registry = temp_registry();
        registry
            .register_xiaozhi_uuid("uuid-2", "Board 2", 1000)
            .unwrap();
        let (clock, cell) = Clock::manual(10_000);
        let server = OtaServer::new(registry, clock, None);
        assert!(server.check("uuid-2", "192.168.1.10:19121", false).is_ok());
        assert!(server.check("uuid-2", "192.168.1.10:19121", false).is_err());
        cell.fetch_add(61_000, std::sync::atomic::Ordering::SeqCst);
        assert!(server.check("uuid-2", "192.168.1.10:19121", false).is_ok());
    }

    #[test]
    fn ota_token_expires_after_one_hour() {
        let registry = temp_registry();
        registry
            .register_xiaozhi_uuid("uuid-3", "Board 3", 1000)
            .unwrap();
        let (clock, cell) = Clock::manual(10_000);
        let server = OtaServer::new(registry, clock, None);
        let response = server.check("uuid-3", "192.168.1.10:19121", false).unwrap();
        cell.fetch_add(OTA_TOKEN_TTL_MS + 1, std::sync::atomic::Ordering::SeqCst);
        assert!(server.consume_token(&response.websocket.token).is_none());
    }
}
