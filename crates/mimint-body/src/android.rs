//! Android body (P4.3): Rust core talking protocol v2 to a thin
//! Kotlin shell on the device.
//!
//! # Architecture
//!
//! The Rust side owns the contract, the guards, the audit trail
//! and the protocol. The Kotlin side (`android-shell/`) is a thin
//! shell: an `AccessibilityService` for input, `MediaProjection`
//! for screenshots, and a loopback server that speaks this
//! protocol and nothing else. The shell holds no policy of its
//! own beyond role enforcement and the kill switch; every other
//! decision lives here.
//!
//! This module has **never talked to a real device**. The fake
//! server below implements the same wire format and runs the
//! full conformance suite at the protocol level; on-device runs
//! are BLOCKED (see `android-verify/`). The protocol, the client
//! and the fake are verified against each other — not against
//! Android.
//!
//! # Wire protocol v2 (normative)
//!
//! Transport: TCP on loopback (`127.0.0.1`), optionally wrapped
//! in TLS with a pinned self-signed certificate (see below).
//! Framing: `u32` big-endian JSON length, JSON header bytes,
//! then `payload_len` raw bytes named by the header. Requests
//! and responses share the shape:
//!
//! ```text
//! Request:  {"v":2,"id":7,"method":"click",
//!            "params":{"x":100,"y":200},"auth":"<token>",
//!            "role":"owner","payload_len":0}
//! Response: {"v":2,"id":7,"ok":true,"result":{...},"payload_len":0}
//! Error:    {"v":2,"id":7,"ok":false,
//!            "error":{"code":"out_of_bounds","message":"..."},
//!            "payload_len":0}
//! ```
//!
//! Methods: `display_size`, `screenshot`, `click`, `move`,
//! `type_text`, `key`, `scroll`, `drag`, `double_click`,
//! `right_click`, `hold_key`, `permissions`, `focused_window`,
//! `kill`.
//! `screenshot` returns `{width,height,scale_factor,origin:[x,y]}`
//! plus `width*height*4` RGBA bytes as the payload. `key` and
//! `hold_key` take the stable wire names from [`Key::name`].
//! `focused_window` returns
//! `{window_id, class, title}` or JSON null when the shell cannot
//! determine the focused window; `window_id` must be STABLE for
//! the same window across calls (the shell derives it as FNV-1a
//! 64-bit over `package + "\0" + class + "\0" + title`).
//!
//! Rules both sides enforce:
//!
//! * `v` must be `2`; anything else is `version_mismatch`.
//! * `auth` must equal the pairing token; anything else is
//!   `unauthorized`. Tokens are compared without early exit.
//! * `role` is `owner` or `guest`. Guests may call only
//!   `display_size`, `screenshot`, `permissions` and
//!   `focused_window`; anything else is `forbidden_role`. The
//!   server enforces this — the client never relies on its own
//!   role claim.
//! * Unknown methods are `unknown_method`; the client maps them
//!   to [`BodyError::Unsupported`].
//! * JSON headers are capped at 1 MiB, payloads at 64 MiB.
//!   Oversize frames are a protocol error, and the connection
//!   is dropped.
//! * Response `id` must equal the request `id`; a mismatch is a
//!   protocol error.
//!
//! # Security properties
//!
//! * **Loopback only.** The client refuses non-loopback
//!   addresses outright; the shell binds `127.0.0.1` only.
//!   Reaching the shell from another machine requires an
//!   explicit `adb reverse` forward, which is a deliberate,
//!   physical-access act by the device owner.
//! * **Pairing auth.** The token is generated on the device at
//!   pairing time and shown to the owner, who enters it on the
//!   Rust side. There is no default token and no tokenless mode.
//! * **TLS with certificate pinning.** The shell serves a
//!   self-signed certificate generated on first run; the client
//!   pins the SHA-256 of the certificate DER, configured
//!   out-of-band at pairing time (the shell shows the
//!   fingerprint next to the token). The pin *is* the identity:
//!   no CA, no hostname check, no trust store. Plain mode exists
//!   only for the fake server and for debugging on loopback, and
//!   the client logs nothing about which mode is in use beyond
//!   the config the caller supplied.
//! * **No pairing with original andee (D7).** This protocol is
//!   v2-only, wire-incompatible with anything else by design
//!   (the `v` field, the method names, the framing). The shell
//!   pairs only with this client.
//! * **Kill switch.** The `kill` method stops the shell's server
//!   and service immediately. The shell also exposes a
//!   persistent on-screen kill affordance; either path ends the
//!   session, and the client treats a dead connection as a
//!   terminal body failure, never as a silent retry loop.
//!
//! # What the shell cannot do
//!
//! An `AccessibilityService` cannot inject arbitrary key events
//! without root or an IME. The shell implements `click`,
//! `move`, `scroll`, `drag`, `double_click` and `right_click`
//! via `dispatchGesture`, `type_text` via `ACTION_SET_TEXT` on
//! the focused editable node (unsupported when no editable node
//! is focused), and reports `key` / `hold_key` as
//! `unsupported` except where a global action exists. The
//! protocol carries the methods so a future IME-based shell can
//! implement them; the current shell must not pretend to. See
//! `docs/android-body.md` and `android-shell/README.md`.

use crate::action::Key;
use crate::body::{Body, Framebuffer, PermissionState, Permissions, ScreenRect};
use crate::conformance::TargetOracle;
use crate::error::BodyError;
use crate::mock::MockEvent;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Protocol version. The only version this client speaks.
pub const PROTOCOL_VERSION: u32 = 2;
/// Maximum JSON header size: 1 MiB.
pub const MAX_JSON_LEN: usize = 1 << 20;
/// Maximum payload size: 64 MiB (an 8K RGBA frame is ~132 MB, so
/// very large displays must downscale server-side; the shell
/// caps its capture dimension and reports the real size).
pub const MAX_PAYLOAD_LEN: usize = 64 << 20;
/// Connect timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Read/write timeout per request.
const IO_TIMEOUT: Duration = Duration::from_secs(10);

/// The caller's role. The server enforces the allow-list; the
/// client sends the claim honestly and never widens it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Owner,
    Guest,
}

impl Role {
    fn name(self) -> &'static str {
        match self {
            Role::Owner => "owner",
            Role::Guest => "guest",
        }
    }

    fn parse(name: &str) -> Option<Role> {
        Some(match name {
            "owner" => Role::Owner,
            "guest" => Role::Guest,
            _ => return None,
        })
    }
}

/// Transport security for the protocol connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TlsMode {
    /// Plain TCP. Loopback only, for the fake server and for
    /// debugging. Never use on any other interface (the client
    /// refuses non-loopback addresses regardless of mode).
    Plain,
    /// TLS with a pinned leaf certificate: the SHA-256 of the
    /// certificate DER, configured at pairing time.
    PinnedSha256([u8; 32]),
}

/// Configuration for an Android body. Construct with
/// [`AndroidConfig::loopback`], which enforces the loopback rule.
#[derive(Debug, Clone)]
pub struct AndroidConfig {
    addr: SocketAddr,
    token: String,
    role: Role,
    tls: TlsMode,
}

impl AndroidConfig {
    /// A config for a shell on this machine's loopback interface.
    /// Non-loopback addresses are refused: the protocol must
    /// never cross a network interface.
    pub fn loopback(
        addr: SocketAddr,
        token: impl Into<String>,
        role: Role,
        tls: TlsMode,
    ) -> Result<AndroidConfig, BodyError> {
        if !addr.ip().is_loopback() {
            return Err(BodyError::InvalidArgument {
                reason: format!(
                    "Android protocol is loopback-only; refusing non-loopback address {addr}"
                ),
            });
        }
        let token = token.into();
        if token.is_empty() {
            return Err(BodyError::InvalidArgument {
                reason: "pairing token must not be empty".to_string(),
            });
        }
        Ok(AndroidConfig {
            addr,
            token,
            role,
            tls,
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn role(&self) -> Role {
        self.role
    }

    pub fn tls(&self) -> &TlsMode {
        &self.tls
    }
}

/// Compare two tokens without early exit on the first differing
/// byte. Lengths are compared first (pairing tokens are fixed
/// length in practice, so this leaks nothing useful).
fn tokens_equal(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Map a server error code to the typed body error. Unknown
/// codes become `Backend` with the code preserved — never a
/// silent success, never a guessed variant.
fn map_server_error(code: &str, message: &str) -> BodyError {
    match code {
        "out_of_bounds" => BodyError::Backend(format!("server reported out_of_bounds: {message}")),
        "forbidden_target" => BodyError::ForbiddenTarget {
            x: 0,
            y: 0,
            reason: format!("server refused the target: {message}"),
        },
        "forbidden_window" => BodyError::ForbiddenWindow {
            reason: format!("server refused the active window: {message}"),
        },
        "permission_denied" => BodyError::PermissionDenied {
            permission: "Android shell permission",
            hint: message.to_string(),
        },
        "unauthorized" => BodyError::PermissionDenied {
            permission: "Android pairing",
            hint: "the pairing token was rejected by the device; re-pair to continue".to_string(),
        },
        "forbidden_role" => BodyError::PermissionDenied {
            permission: "Android owner role",
            hint: "guest role may only read the screen; pair as owner to send input".to_string(),
        },
        "unsupported" | "unknown_method" => {
            BodyError::Unsupported("not supported by the Android shell")
        }
        "untypeable_text" => BodyError::UntypeableText {
            text: String::new(),
            reason: message.to_string(),
        },
        "invalid_argument" => BodyError::InvalidArgument {
            reason: message.to_string(),
        },
        "session_expired" => BodyError::SessionExpired {
            reason: message.to_string(),
        },
        "focus_mismatch" => BodyError::FocusMismatch {
            expected: None,
            actual: None,
        },
        "rate_limited" => BodyError::RateLimited {
            reason: message.to_string(),
        },
        "stopped" => BodyError::Stopped,
        _ => BodyError::Backend(format!("server error {code}: {message}")),
    }
}

fn permission_from_name(name: &str) -> PermissionState {
    match name {
        "granted" => PermissionState::Granted,
        "denied" => PermissionState::Denied,
        "not_applicable" => PermissionState::NotApplicable,
        _ => PermissionState::NotDeterminable,
    }
}

fn permission_name(state: PermissionState) -> &'static str {
    match state {
        PermissionState::Granted => "granted",
        PermissionState::Denied => "denied",
        PermissionState::NotDeterminable => "not_determinable",
        PermissionState::NotApplicable => "not_applicable",
    }
}

// ---------------------------------------------------------------------------
// Pinned-certificate TLS (rustls, ring)
// ---------------------------------------------------------------------------

/// A verifier that trusts exactly one certificate: the pinned
/// SHA-256 of its DER. No CA, no hostname check, no expiry check
/// beyond what the handshake itself requires — the pin replaces
/// the entire PKI. The fingerprint must come from the pairing
/// flow (shown on the device), never from the network.
#[derive(Debug)]
struct PinnedVerifier {
    fingerprint: [u8; 32],
    algorithms: rustls::crypto::WebPkiSupportedAlgorithms,
}

impl PinnedVerifier {
    fn new(fingerprint: [u8; 32]) -> PinnedVerifier {
        PinnedVerifier {
            fingerprint,
            algorithms: rustls::crypto::ring::default_provider().signature_verification_algorithms,
        }
    }
}

impl rustls::client::danger::ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let digest = ring::digest::digest(&ring::digest::SHA256, end_entity.as_ref());
        if digest.as_ref() == self.fingerprint {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::UnknownIssuer,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

fn pinned_client_config(fingerprint: [u8; 32]) -> Result<Arc<rustls::ClientConfig>, BodyError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .map_err(|err| BodyError::Backend(format!("TLS config failed: {err}")))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedVerifier::new(fingerprint)))
        .with_no_client_auth();
    Ok(Arc::new(config))
}

/// SHA-256 of a certificate's DER encoding, for pairing: the
/// shell shows this fingerprint, the owner enters it here.
pub fn fingerprint_of_cert_der(der: &[u8]) -> [u8; 32] {
    let digest = ring::digest::digest(&ring::digest::SHA256, der);
    let mut out = [0u8; 32];
    out.copy_from_slice(digest.as_ref());
    out
}

// ---------------------------------------------------------------------------
// Protocol client
// ---------------------------------------------------------------------------

enum Transport {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Read for Transport {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Transport::Plain(stream) => stream.read(buf),
            Transport::Tls(stream) => stream.read(buf),
        }
    }
}

impl Write for Transport {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Transport::Plain(stream) => stream.write(buf),
            Transport::Tls(stream) => stream.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Transport::Plain(stream) => stream.flush(),
            Transport::Tls(stream) => stream.flush(),
        }
    }
}

/// A single-connection protocol v2 client. One request at a
/// time; the caller (`AndroidBody`) owns reconnect behavior.
pub struct ProtocolClient {
    transport: Transport,
    token: String,
    role: Role,
    next_id: u64,
}

impl ProtocolClient {
    pub fn connect(config: &AndroidConfig) -> Result<ProtocolClient, BodyError> {
        let sock = TcpStream::connect_timeout(&config.addr, CONNECT_TIMEOUT)
            .map_err(|err| BodyError::Backend(format!("cannot connect to Android shell: {err}")))?;
        sock.set_read_timeout(Some(IO_TIMEOUT))
            .map_err(|err| BodyError::Backend(format!("cannot set read timeout: {err}")))?;
        sock.set_write_timeout(Some(IO_TIMEOUT))
            .map_err(|err| BodyError::Backend(format!("cannot set write timeout: {err}")))?;
        let transport = match &config.tls {
            TlsMode::Plain => Transport::Plain(sock),
            TlsMode::PinnedSha256(fingerprint) => {
                let tls_config = pinned_client_config(*fingerprint)?;
                let server_name = rustls::pki_types::ServerName::IpAddress(
                    rustls::pki_types::IpAddr::from(config.addr.ip()),
                );
                let mut conn = rustls::ClientConnection::new(tls_config, server_name)
                    .map_err(|err| BodyError::Backend(format!("TLS setup failed: {err}")))?;
                // Drive the handshake to completion now, so a pin
                // mismatch fails the connect — not the first
                // request. (A bare flush would only send the
                // ClientHello; the peer's certificate is only
                // checked once its flight is read.)
                let mut sock = sock;
                while conn.is_handshaking() {
                    conn.complete_io(&mut sock).map_err(|err| {
                        BodyError::Backend(format!("TLS handshake failed: {err}"))
                    })?;
                }
                let stream = rustls::StreamOwned::new(conn, sock);
                Transport::Tls(Box::new(stream))
            }
        };
        Ok(ProtocolClient {
            transport,
            token: config.token.clone(),
            role: config.role,
            next_id: 1,
        })
    }

    /// One request/response round trip. `payload` is sent after
    /// the header (always empty for requests today); the returned
    /// payload belongs to the response.
    pub fn request(&mut self, method: &str, params: Value) -> Result<(Value, Vec<u8>), BodyError> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let header = json!({
            "v": PROTOCOL_VERSION,
            "id": id,
            "method": method,
            "params": params,
            "auth": self.token,
            "role": self.role.name(),
            "payload_len": 0,
        });
        let bytes = serde_json::to_vec(&header)
            .map_err(|err| BodyError::Backend(format!("cannot encode request: {err}")))?;
        let len = u32::try_from(bytes.len()).map_err(|_| {
            BodyError::Backend("request header exceeds the framing limit".to_string())
        })?;
        self.transport
            .write_all(&len.to_be_bytes())
            .map_err(io_err)?;
        self.transport.write_all(&bytes).map_err(io_err)?;
        self.transport.flush().map_err(io_err)?;

        let mut len_buf = [0u8; 4];
        self.transport.read_exact(&mut len_buf).map_err(io_err)?;
        let json_len = u32::from_be_bytes(len_buf) as usize;
        if json_len == 0 || json_len > MAX_JSON_LEN {
            return Err(BodyError::Backend(format!(
                "protocol error: response header length {json_len} out of range"
            )));
        }
        let mut json_buf = vec![0u8; json_len];
        self.transport.read_exact(&mut json_buf).map_err(io_err)?;
        let reply: Value = serde_json::from_slice(&json_buf).map_err(|err| {
            BodyError::Backend(format!("protocol error: bad response JSON: {err}"))
        })?;
        let reply_id = reply.get("id").and_then(Value::as_u64);
        if reply_id != Some(id) {
            return Err(BodyError::Backend(format!(
                "protocol error: response id {reply_id:?} does not match request id {id}"
            )));
        }
        if reply.get("v").and_then(Value::as_u64) != Some(u64::from(PROTOCOL_VERSION)) {
            return Err(BodyError::Backend(
                "protocol error: response version is not 2".to_string(),
            ));
        }
        let payload_len = reply
            .get("payload_len")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        if payload_len > MAX_PAYLOAD_LEN {
            return Err(BodyError::Backend(format!(
                "protocol error: response payload length {payload_len} exceeds the limit"
            )));
        }
        let mut payload = vec![0u8; payload_len];
        self.transport.read_exact(&mut payload).map_err(io_err)?;
        if reply.get("ok").and_then(Value::as_bool) == Some(true) {
            Ok((reply.get("result").cloned().unwrap_or(Value::Null), payload))
        } else {
            let error = reply.get("error").cloned().unwrap_or(Value::Null);
            let code = error
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("no message");
            Err(map_server_error(code, message))
        }
    }
}

fn io_err(err: std::io::Error) -> BodyError {
    match err.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => BodyError::Backend(
            "Android shell did not respond in time (body connection lost?)".to_string(),
        ),
        std::io::ErrorKind::UnexpectedEof
        | std::io::ErrorKind::ConnectionReset
        | std::io::ErrorKind::BrokenPipe
        | std::io::ErrorKind::ConnectionAborted => {
            BodyError::Backend(format!("Android shell connection lost: {err}"))
        }
        _ => BodyError::Backend(format!("Android protocol I/O error: {err}")),
    }
}

// ---------------------------------------------------------------------------
// AndroidBody
// ---------------------------------------------------------------------------

/// The body side: a [`Body`] implemented over protocol v2. The
/// connection is lazy (first action connects) and is re-made
/// once after a transport failure on an idempotent read only;
/// input calls never retry after a transport failure (see
/// `call`). State that belongs to the body contract (exclusion
/// zones, capture cap) is enforced here,
/// not trusted to the shell.
pub struct AndroidBody {
    config: AndroidConfig,
    client: Option<ProtocolClient>,
    excluded: Vec<ScreenRect>,
    max_capture_dimension: Option<u32>,
    cached_size: Option<(u32, u32)>,
    expected_focus: Option<u64>,
}

impl AndroidBody {
    pub fn new(config: AndroidConfig) -> AndroidBody {
        AndroidBody {
            config,
            client: None,
            excluded: Vec::new(),
            max_capture_dimension: None,
            cached_size: None,
            expected_focus: None,
        }
    }

    /// The kill switch: ask the shell to stop its server and
    /// service immediately. Best-effort delivery — a dead
    /// connection means the shell is already gone, which is the
    /// outcome the kill wants anyway.
    pub fn kill(&mut self) {
        if let Ok(client) = self.ensure_client() {
            let _ = client.request("kill", json!({}));
        }
        self.client = None;
    }

    fn ensure_client(&mut self) -> Result<&mut ProtocolClient, BodyError> {
        if self.client.is_none() {
            self.client = Some(ProtocolClient::connect(&self.config)?);
        }
        Ok(self.client.as_mut().unwrap())
    }

    /// Run one request. On a transport-level failure the client
    /// is dropped (the connection state is unknown) and:
    ///
    /// * idempotent reads (`display_size`, `screenshot`,
    ///   `permissions`, `focused_window`) reconnect once and
    ///   re-issue the request — re-reading cannot
    ///   double-execute anything;
    /// * input methods are NEVER retried: the request may have
    ///   reached the server, so the outcome is unknown. The
    ///   caller gets `UnknownOutcome` and must NOT retry
    ///   either — the verifier layer reports UNKNOWN and
    ///   waits for the human, per the unknown-outcome rule.
    ///
    /// Server-meaningful errors (typed refusals) are returned
    /// as-is, never retried: a refusal is an answer.
    fn call(&mut self, method: &str, params: Value) -> Result<(Value, Vec<u8>), BodyError> {
        let first = self.ensure_client()?.request(method, params.clone());
        match first {
            Ok(outcome) => Ok(outcome),
            Err(err) => {
                if is_transport_error(&err) {
                    self.client = None;
                    if is_idempotent_read(method) {
                        self.ensure_client()?.request(method, params)
                    } else {
                        Err(BodyError::UnknownOutcome {
                            reason: format!(
                                "transport failure during {method}; the action may or may not \
                                 have executed, and was not retried: {err}"
                            ),
                        })
                    }
                } else {
                    Err(err)
                }
            }
        }
    }

    fn check_target(&self, x: i32, y: i32) -> Result<(), BodyError> {
        for rect in &self.excluded {
            if rect.contains(x, y) {
                return Err(BodyError::ForbiddenTarget {
                    x,
                    y,
                    reason: "point is inside a self-exclusion zone".to_string(),
                });
            }
        }
        Ok(())
    }

    fn size(&mut self) -> Result<(u32, u32), BodyError> {
        if let Some(size) = self.cached_size {
            return Ok(size);
        }
        let (result, _) = self.call("display_size", json!({}))?;
        let width = result
            .get("width")
            .and_then(Value::as_u64)
            .ok_or_else(|| BodyError::Backend("display_size reply lacks width".to_string()))?
            as u32;
        let height = result
            .get("height")
            .and_then(Value::as_u64)
            .ok_or_else(|| BodyError::Backend("display_size reply lacks height".to_string()))?
            as u32;
        self.cached_size = Some((width, height));
        Ok((width, height))
    }

    fn check_bounds(&mut self, x: i32, y: i32) -> Result<(), BodyError> {
        let (width, height) = self.size()?;
        if !(0..width as i32).contains(&x) || !(0..height as i32).contains(&y) {
            return Err(BodyError::OutOfBounds {
                x,
                y,
                width,
                height,
            });
        }
        Ok(())
    }
}

/// Transport failures are the only retriable kind: a dropped
/// connection, a timeout, a framing error. Everything the server
/// said deliberately (typed refusals) is final.
fn is_transport_error(err: &BodyError) -> bool {
    match err {
        BodyError::Backend(msg) => {
            msg.contains("connection lost")
                || msg.contains("did not respond in time")
                || msg.contains("protocol error")
                || msg.contains("I/O error")
        }
        _ => false,
    }
}

/// Methods whose retry is safe: pure reads with no side
/// effects. Every other method (all input, `kill`) must never
/// be re-issued after a transport failure — see `call`.
fn is_idempotent_read(method: &str) -> bool {
    matches!(
        method,
        "display_size" | "screenshot" | "permissions" | "focused_window"
    )
}

impl Body for AndroidBody {
    fn name(&self) -> &'static str {
        "android"
    }

    fn display_size(&mut self) -> Result<(u32, u32), BodyError> {
        self.size()
    }

    fn screenshot(&mut self) -> Result<Framebuffer, BodyError> {
        let (result, payload) = self.call("screenshot", json!({}))?;
        let width = result
            .get("width")
            .and_then(Value::as_u64)
            .ok_or_else(|| BodyError::Backend("screenshot reply lacks width".to_string()))?
            as u32;
        let height = result
            .get("height")
            .and_then(Value::as_u64)
            .ok_or_else(|| BodyError::Backend("screenshot reply lacks height".to_string()))?
            as u32;
        let scale = result
            .get("scale_factor")
            .and_then(Value::as_f64)
            .unwrap_or(1.0);
        let origin = result
            .get("origin")
            .and_then(Value::as_array)
            .and_then(|arr| {
                let x = arr.first()?.as_i64()? as i32;
                let y = arr.get(1)?.as_i64()? as i32;
                Some((x, y))
            })
            .unwrap_or((0, 0));
        if payload.len() != width as usize * height as usize * 4 {
            return Err(BodyError::Backend(format!(
                "screenshot payload is {} bytes, expected {} for {width}x{height}",
                payload.len(),
                width as usize * height as usize * 4
            )));
        }
        let frame = Framebuffer::with_geometry(width, height, payload, scale, origin)?;
        Ok(match self.max_capture_dimension {
            Some(max) => frame.downscale(max),
            None => frame,
        })
    }

    fn click(&mut self, x: i32, y: i32) -> Result<(), BodyError> {
        self.check_bounds(x, y)?;
        self.check_target(x, y)?;
        self.call("click", json!({"x": x, "y": y}))?;
        Ok(())
    }

    fn mouse_move(&mut self, x: i32, y: i32) -> Result<(), BodyError> {
        self.check_bounds(x, y)?;
        self.check_target(x, y)?;
        self.call("move", json!({"x": x, "y": y}))?;
        Ok(())
    }

    fn type_text(&mut self, text: &str) -> Result<(), BodyError> {
        self.verify_focus()?;
        self.call("type_text", json!({"text": text}))?;
        Ok(())
    }

    fn key(&mut self, key: Key) -> Result<(), BodyError> {
        self.verify_focus()?;
        self.call("key", json!({"key": key.name()}))?;
        Ok(())
    }

    fn scroll(&mut self, x: i32, y: i32, dx: i32, dy: i32) -> Result<(), BodyError> {
        self.check_bounds(x, y)?;
        self.check_target(x, y)?;
        self.call("scroll", json!({"x": x, "y": y, "dx": dx, "dy": dy}))?;
        Ok(())
    }

    fn drag(&mut self, from_x: i32, from_y: i32, to_x: i32, to_y: i32) -> Result<(), BodyError> {
        self.check_bounds(from_x, from_y)?;
        self.check_bounds(to_x, to_y)?;
        self.check_target(from_x, from_y)?;
        self.check_target(to_x, to_y)?;
        self.call(
            "drag",
            json!({"from_x": from_x, "from_y": from_y, "to_x": to_x, "to_y": to_y}),
        )?;
        Ok(())
    }

    fn double_click(&mut self, x: i32, y: i32) -> Result<(), BodyError> {
        self.check_bounds(x, y)?;
        self.check_target(x, y)?;
        self.call("double_click", json!({"x": x, "y": y}))?;
        Ok(())
    }

    fn right_click(&mut self, x: i32, y: i32) -> Result<(), BodyError> {
        self.check_bounds(x, y)?;
        self.check_target(x, y)?;
        self.call("right_click", json!({"x": x, "y": y}))?;
        Ok(())
    }

    fn hold_key(&mut self, key: Key, millis: u32) -> Result<(), BodyError> {
        if millis == 0 || millis > crate::action::MAX_HOLD_MILLIS {
            return Err(BodyError::InvalidArgument {
                reason: format!(
                    "hold duration must be 1..={} ms, got {millis}",
                    crate::action::MAX_HOLD_MILLIS
                ),
            });
        }
        self.verify_focus()?;
        self.call("hold_key", json!({"key": key.name(), "millis": millis}))?;
        Ok(())
    }

    fn permissions(&mut self) -> Permissions {
        match self.call("permissions", json!({})) {
            Ok((result, _)) => Permissions {
                screen_capture: result
                    .get("screen_capture")
                    .and_then(Value::as_str)
                    .map(permission_from_name)
                    .unwrap_or(PermissionState::NotDeterminable),
                input_control: result
                    .get("input_control")
                    .and_then(Value::as_str)
                    .map(permission_from_name)
                    .unwrap_or(PermissionState::NotDeterminable),
            },
            // A body that cannot be asked reports "not
            // determinable", never a guessed grant.
            Err(_) => Permissions {
                screen_capture: PermissionState::NotDeterminable,
                input_control: PermissionState::NotDeterminable,
            },
        }
    }

    fn excluded_rects(&self) -> &[ScreenRect] {
        &self.excluded
    }

    fn set_excluded_rects(&mut self, rects: Vec<ScreenRect>) {
        self.excluded = rects;
    }

    fn max_capture_dimension(&self) -> Option<u32> {
        self.max_capture_dimension
    }

    fn set_max_capture_dimension(&mut self, max: Option<u32>) {
        self.max_capture_dimension = max;
    }

    /// The focused window's stable id, from the shell's
    /// `focused_window` query. A failed query or a null reply
    /// yields None: the trait's `verify_focus` then refuses any
    /// action taken with an expectation set (fail-closed), and
    /// costs nothing when no expectation is set.
    fn active_window(&mut self) -> Option<u64> {
        let (result, _) = self.call("focused_window", json!({})).ok()?;
        result.get("window_id")?.as_u64()
    }

    fn expected_focus(&self) -> Option<u64> {
        self.expected_focus
    }

    fn set_expected_focus(&mut self, window: Option<u64>) {
        self.expected_focus = window;
    }
}

// ---------------------------------------------------------------------------
// Fake server + oracle (protocol-level conformance)
// ---------------------------------------------------------------------------

/// Shared fake-device state. The fake server and the fake oracle
/// share it, so the conformance suite observes exactly what the
/// server did — the same arrangement as `MockBody`/`MockOracle`,
/// with a real socket and real framing in between.
#[derive(Debug)]
pub struct FakeState {
    variant: u8,
    events: Vec<MockEvent>,
    permissions: Permissions,
    width: u32,
    height: u32,
    scale: f64,
    origin: (i32, i32),
    /// Recorded audit lines, in the shell's JSONL shape (method
    /// + outcome, never pixels or text content).
    audit: Vec<String>,
    /// Set by the `kill` method.
    killed: bool,
    /// Connections served (for the reconnect test).
    connections: u64,
    /// When set, the next dispatched request is executed but
    /// its reply is never written: the connection is dropped
    /// instead, modelling "the request reached the server and
    /// the response was lost". Consumed by the first dispatch
    /// after it is set. Test hook for the retry rules.
    drop_response_once: bool,
    /// (window id, class, title) of the focused window, or None
    /// when the shell cannot determine it.
    focused: Option<(u64, String, String)>,
}

impl FakeState {
    fn new() -> FakeState {
        FakeState {
            variant: 0,
            events: Vec::new(),
            permissions: Permissions {
                screen_capture: PermissionState::Granted,
                input_control: PermissionState::Granted,
            },
            width: 800,
            height: 600,
            scale: 1.0,
            origin: (0, 0),
            audit: Vec::new(),
            killed: false,
            connections: 0,
            drop_response_once: false,
            focused: Some((
                0xA11CE,
                "com.example.app/.MainActivity".to_string(),
                "Example".to_string(),
            )),
        }
    }

    /// Deterministic pixels: the same contract as the mock body —
    /// solid variant colors, and the quadrant pattern for
    /// variant 3, so C9 grades the fake the same way.
    fn pixels(&self) -> Vec<u8> {
        let mut data = vec![0u8; self.width as usize * self.height as usize * 4];
        for y in 0..self.height {
            for x in 0..self.width {
                let (r, g, b) = match self.variant {
                    // Variant 0 is a gradient (non-uniform, so
                    // C1 passes on the very first screenshot).
                    0 => ((x % 256) as u8, (y % 256) as u8, 128u8),
                    1 => (0, 255, 0),
                    2 => (0, 0, 255),
                    3 => {
                        let hw = self.width / 2;
                        let hh = self.height / 2;
                        match (x < hw, y < hh) {
                            (true, true) => (255, 0, 0),
                            (false, true) => (0, 255, 0),
                            (true, false) => (0, 0, 255),
                            (false, false) => (255, 255, 0),
                        }
                    }
                    _ => (128, 128, 128),
                };
                let idx = (y as usize * self.width as usize + x as usize) * 4;
                data[idx..idx + 4].copy_from_slice(&[r, g, b, 255]);
            }
        }
        data
    }
}

fn lock_fake(state: &Arc<Mutex<FakeState>>) -> std::sync::MutexGuard<'_, FakeState> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// What the fake server requires: the pairing token, and
/// optionally a TLS identity (PEM cert + PEM key) for the pinned
/// mode.
pub struct FakeServerConfig {
    pub token: String,
    /// (cert PEM, key PEM) for TLS mode; `None` serves plain TCP.
    pub tls_identity: Option<(Vec<u8>, Vec<u8>)>,
}

/// A fake Android shell: a real TCP server on loopback speaking
/// protocol v2, sharing state with its oracle. Public so
/// integration tests and the conformance binary can run the
/// suite against the wire format. This is test support, not a
/// product server: it has no Android APIs behind it.
pub struct FakeAndroidServer {
    addr: SocketAddr,
    state: Arc<Mutex<FakeState>>,
    shutdown: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl FakeAndroidServer {
    pub fn start(config: FakeServerConfig) -> Result<FakeAndroidServer, BodyError> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .map_err(|err| BodyError::Backend(format!("fake server cannot bind: {err}")))?;
        let addr = listener
            .local_addr()
            .map_err(|err| BodyError::Backend(format!("fake server has no address: {err}")))?;
        let state = Arc::new(Mutex::new(FakeState::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let tls_config = match config.tls_identity {
            Some((cert_pem, key_pem)) => Some(fake_tls_config(&cert_pem, &key_pem)?),
            None => None,
        };
        let thread_state = Arc::clone(&state);
        let thread_shutdown = Arc::clone(&shutdown);
        let token = config.token;
        let thread = std::thread::spawn(move || {
            serve(listener, thread_state, thread_shutdown, token, tls_config);
        });
        Ok(FakeAndroidServer {
            addr,
            state,
            shutdown,
            thread: Some(thread),
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn state(&self) -> Arc<Mutex<FakeState>> {
        Arc::clone(&self.state)
    }

    /// Connections served so far (for the retry tests).
    pub fn connections_served(&self) -> u64 {
        lock_fake(&self.state).connections
    }

    /// Arm the drop hook: the next request is executed, but
    /// its reply is never written and the connection is
    /// dropped. Models a lost response after execution.
    pub fn drop_next_response(&self) {
        lock_fake(&self.state).drop_response_once = true;
    }

    /// The oracle sharing this server's state.
    pub fn oracle(&self) -> FakeAndroidOracle {
        FakeAndroidOracle {
            state: Arc::clone(&self.state),
        }
    }

    pub fn stop(mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // Wake the accept loop: connect once and drop.
        let _ = TcpStream::connect(self.addr);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for FakeAndroidServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn fake_tls_config(
    cert_pem: &[u8],
    key_pem: &[u8],
) -> Result<Arc<rustls::ServerConfig>, BodyError> {
    use rustls_pki_types::pem::PemObject;
    let cert = rustls_pki_types::CertificateDer::from_pem_slice(cert_pem)
        .map_err(|err| BodyError::Backend(format!("test cert PEM unreadable: {err}")))?;
    let key = rustls_pki_types::PrivateKeyDer::from_pem_slice(key_pem)
        .map_err(|err| BodyError::Backend(format!("test key PEM unreadable: {err}")))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .map_err(|err| BodyError::Backend(format!("fake TLS config failed: {err}")))?
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .map_err(|err| BodyError::Backend(format!("fake TLS identity failed: {err}")))?;
    Ok(Arc::new(config))
}

fn serve(
    listener: TcpListener,
    state: Arc<Mutex<FakeState>>,
    shutdown: Arc<AtomicBool>,
    token: String,
    tls_config: Option<Arc<rustls::ServerConfig>>,
) {
    let _ = listener.set_nonblocking(false);
    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        let (stream, _) = match listener.accept() {
            Ok(pair) => pair,
            Err(_) => {
                if shutdown.load(Ordering::SeqCst) {
                    break;
                }
                continue;
            }
        };
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        lock_fake(&state).connections += 1;
        let conn_state = Arc::clone(&state);
        let conn_token = token.clone();
        let conn_tls = tls_config.clone();
        let conn_shutdown = Arc::clone(&shutdown);
        std::thread::spawn(move || {
            let _ = serve_connection(stream, conn_state, conn_token, conn_tls, conn_shutdown);
        });
    }
}

enum ServerTransport {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ServerConnection, TcpStream>>),
}

impl Read for ServerTransport {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            ServerTransport::Plain(s) => s.read(buf),
            ServerTransport::Tls(s) => s.read(buf),
        }
    }
}

impl Write for ServerTransport {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            ServerTransport::Plain(s) => s.write(buf),
            ServerTransport::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            ServerTransport::Plain(s) => s.flush(),
            ServerTransport::Tls(s) => s.flush(),
        }
    }
}

fn serve_connection(
    stream: TcpStream,
    state: Arc<Mutex<FakeState>>,
    token: String,
    tls_config: Option<Arc<rustls::ServerConfig>>,
    shutdown: Arc<AtomicBool>,
) -> std::io::Result<()> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
    let mut transport = match tls_config {
        Some(config) => {
            let conn = rustls::ServerConnection::new(config)
                .map_err(|err| std::io::Error::other(err.to_string()))?;
            // The handshake is driven lazily by the first frame
            // read below (StreamOwned completes it before
            // returning application data).
            let stream = rustls::StreamOwned::new(conn, stream);
            ServerTransport::Tls(Box::new(stream))
        }
        None => ServerTransport::Plain(stream),
    };
    loop {
        if shutdown.load(Ordering::SeqCst) || lock_fake(&state).killed {
            return Ok(());
        }
        let mut len_buf = [0u8; 4];
        match transport.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(err) => return Err(err),
        }
        let json_len = u32::from_be_bytes(len_buf) as usize;
        if json_len == 0 || json_len > MAX_JSON_LEN {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "oversize frame",
            ));
        }
        let mut json_buf = vec![0u8; json_len];
        transport.read_exact(&mut json_buf)?;
        let request: Value = match serde_json::from_slice(&json_buf) {
            Ok(value) => value,
            Err(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "bad request JSON",
                ));
            }
        };
        let payload_len = request
            .get("payload_len")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        if payload_len > MAX_PAYLOAD_LEN {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "oversize payload",
            ));
        }
        if payload_len > 0 {
            let mut sink = vec![0u8; payload_len];
            transport.read_exact(&mut sink)?;
        }
        let (reply, payload) = dispatch(&state, &request, &token);
        // Test hook for the retry rules: the request was
        // executed, but the reply is dropped — the client
        // sees a bare transport failure.
        if std::mem::replace(&mut lock_fake(&state).drop_response_once, false) {
            return Ok(());
        }
        let reply_bytes = serde_json::to_vec(&reply).unwrap();
        let len = u32::try_from(reply_bytes.len()).unwrap_or(u32::MAX);
        transport.write_all(&len.to_be_bytes())?;
        transport.write_all(&reply_bytes)?;
        transport.write_all(&payload)?;
        transport.flush()?;
        if lock_fake(&state).killed {
            return Ok(());
        }
    }
}

/// Dispatch one request against the fake state. Returns the reply
/// header and the response payload. Every branch audits: the
/// audit lines are part of what the conformance hook inspects.
fn dispatch(
    state: &Arc<Mutex<FakeState>>,
    request: &Value,
    expected_token: &str,
) -> (Value, Vec<u8>) {
    let id = request.get("id").and_then(Value::as_u64).unwrap_or(0);
    let error_reply = |code: &str, message: &str| {
        (
            json!({"v": PROTOCOL_VERSION, "id": id, "ok": false,
                   "error": {"code": code, "message": message}, "payload_len": 0}),
            Vec::new(),
        )
    };
    let ok_reply = |result: Value, payload: Vec<u8>| {
        let payload_len = payload.len();
        (
            json!({"v": PROTOCOL_VERSION, "id": id, "ok": true,
                   "result": result, "payload_len": payload_len}),
            payload,
        )
    };
    if request.get("v").and_then(Value::as_u64) != Some(u64::from(PROTOCOL_VERSION)) {
        return error_reply("version_mismatch", "protocol version must be 2");
    }
    let auth = request.get("auth").and_then(Value::as_str).unwrap_or("");
    if !tokens_equal(auth, expected_token) {
        audit(state, "auth", "unauthorized");
        return error_reply("unauthorized", "pairing token rejected");
    }
    let role = request
        .get("role")
        .and_then(Value::as_str)
        .and_then(Role::parse);
    let Some(role) = role else {
        return error_reply("invalid_argument", "role must be owner or guest");
    };
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    // Role allow-list, enforced server-side.
    if role == Role::Guest
        && !matches!(
            method,
            "display_size" | "screenshot" | "permissions" | "focused_window"
        )
    {
        audit(state, method, "forbidden_role");
        return error_reply("forbidden_role", "guest role may only read the screen");
    }
    let params = request.get("params").cloned().unwrap_or(Value::Null);
    let outcome = handle_method(state, method, &params, role);
    match outcome {
        Ok((result, payload)) => {
            audit(state, method, "ok");
            ok_reply(result, payload)
        }
        Err((code, message)) => {
            audit(state, method, &code);
            error_reply(&code, &message)
        }
    }
}

fn audit(state: &Arc<Mutex<FakeState>>, method: &str, outcome: &str) {
    lock_fake(state)
        .audit
        .push(format!("{{\"method\":{method:?},\"outcome\":{outcome:?}}}"));
}

fn param_i32(params: &Value, name: &str) -> Result<i32, (String, String)> {
    params
        .get(name)
        .and_then(Value::as_i64)
        .map(|v| v as i32)
        .ok_or_else(|| {
            (
                "invalid_argument".to_string(),
                format!("missing integer param {name}"),
            )
        })
}

fn param_key(params: &Value, name: &str) -> Result<Key, (String, String)> {
    let raw = params.get(name).and_then(Value::as_str).unwrap_or("");
    Key::parse(raw).ok_or_else(|| {
        (
            "invalid_argument".to_string(),
            format!("unknown key name {raw:?}"),
        )
    })
}

/// Execute one method against the fake state. Errors are
/// (code, message) pairs, mapped to wire errors by `dispatch`.
fn handle_method(
    state: &Arc<Mutex<FakeState>>,
    method: &str,
    params: &Value,
    _role: Role,
) -> Result<(Value, Vec<u8>), (String, String)> {
    let mut guard = lock_fake(state);
    // Denied permissions fail with typed errors, never silence.
    let input_denied = guard.permissions.input_control == PermissionState::Denied;
    let capture_denied = guard.permissions.screen_capture == PermissionState::Denied;
    match method {
        "display_size" => Ok((
            json!({"width": guard.width, "height": guard.height}),
            Vec::new(),
        )),
        "permissions" => Ok((
            json!({
                "screen_capture": permission_name(guard.permissions.screen_capture),
                "input_control": permission_name(guard.permissions.input_control),
            }),
            Vec::new(),
        )),
        "screenshot" => {
            if capture_denied {
                return Err((
                    "permission_denied".to_string(),
                    "Screen Recording is denied on the fake device".to_string(),
                ));
            }
            let pixels = guard.pixels();
            let result = json!({
                "width": guard.width,
                "height": guard.height,
                "scale_factor": guard.scale,
                "origin": [guard.origin.0, guard.origin.1],
            });
            Ok((result, pixels))
        }
        "focused_window" => match &guard.focused {
            Some((id, class, title)) => Ok((
                json!({"window_id": id, "class": class, "title": title}),
                Vec::new(),
            )),
            None => Ok((Value::Null, Vec::new())),
        },
        "kill" => {
            guard.killed = true;
            Ok((json!({}), Vec::new()))
        }
        "click" | "move" | "type_text" | "key" | "scroll" | "drag" | "double_click"
        | "right_click" | "hold_key" => {
            if input_denied {
                return Err((
                    "permission_denied".to_string(),
                    "Accessibility input is denied on the fake device".to_string(),
                ));
            }
            // Bounds are enforced server-side too (defense in
            // depth: the client also checks).
            let (width, height) = (guard.width as i32, guard.height as i32);
            let in_bounds = |x: i32, y: i32| (0..width).contains(&x) && (0..height).contains(&y);
            match method {
                "click" => {
                    let (x, y) = (param_i32(params, "x")?, param_i32(params, "y")?);
                    if !in_bounds(x, y) {
                        return Err((
                            "out_of_bounds".into(),
                            format!("({x}, {y}) outside display"),
                        ));
                    }
                    guard.events.push(MockEvent::Click { x, y });
                }
                "move" => {
                    let (x, y) = (param_i32(params, "x")?, param_i32(params, "y")?);
                    if !in_bounds(x, y) {
                        return Err((
                            "out_of_bounds".into(),
                            format!("({x}, {y}) outside display"),
                        ));
                    }
                    guard.events.push(MockEvent::Move { x, y });
                }
                "type_text" => {
                    let text = params.get("text").and_then(Value::as_str).ok_or_else(|| {
                        (
                            "invalid_argument".to_string(),
                            "missing text param".to_string(),
                        )
                    })?;
                    for ch in text.chars() {
                        guard.events.push(MockEvent::Char(ch));
                    }
                }
                "key" => {
                    let key = param_key(params, "key")?;
                    match key {
                        Key::Enter => guard.events.push(MockEvent::Char('\n')),
                        Key::Tab => guard.events.push(MockEvent::Char('\t')),
                        _ => {}
                    }
                    guard.events.push(MockEvent::Key(key));
                }
                "scroll" => {
                    let (x, y) = (param_i32(params, "x")?, param_i32(params, "y")?);
                    let (dx, dy) = (param_i32(params, "dx")?, param_i32(params, "dy")?);
                    if !in_bounds(x, y) {
                        return Err((
                            "out_of_bounds".into(),
                            format!("({x}, {y}) outside display"),
                        ));
                    }
                    guard.events.push(MockEvent::Scroll { x, y, dx, dy });
                }
                "drag" => {
                    let (from_x, from_y) =
                        (param_i32(params, "from_x")?, param_i32(params, "from_y")?);
                    let (to_x, to_y) = (param_i32(params, "to_x")?, param_i32(params, "to_y")?);
                    if !in_bounds(from_x, from_y) || !in_bounds(to_x, to_y) {
                        return Err((
                            "out_of_bounds".into(),
                            "drag endpoint outside display".into(),
                        ));
                    }
                    guard.events.push(MockEvent::Drag {
                        from_x,
                        from_y,
                        to_x,
                        to_y,
                    });
                }
                "double_click" => {
                    let (x, y) = (param_i32(params, "x")?, param_i32(params, "y")?);
                    if !in_bounds(x, y) {
                        return Err((
                            "out_of_bounds".into(),
                            format!("({x}, {y}) outside display"),
                        ));
                    }
                    guard.events.push(MockEvent::DoubleClick { x, y });
                }
                "right_click" => {
                    let (x, y) = (param_i32(params, "x")?, param_i32(params, "y")?);
                    if !in_bounds(x, y) {
                        return Err((
                            "out_of_bounds".into(),
                            format!("({x}, {y}) outside display"),
                        ));
                    }
                    guard.events.push(MockEvent::RightClick { x, y });
                }
                "hold_key" => {
                    let key = param_key(params, "key")?;
                    let millis = params
                        .get("millis")
                        .and_then(Value::as_u64)
                        .ok_or_else(|| {
                            (
                                "invalid_argument".to_string(),
                                "missing millis param".to_string(),
                            )
                        })? as u32;
                    if millis == 0 || millis > crate::action::MAX_HOLD_MILLIS {
                        return Err((
                            "invalid_argument".into(),
                            format!("hold duration {millis} out of range"),
                        ));
                    }
                    guard.events.push(MockEvent::Hold { key, millis });
                }
                _ => unreachable!(),
            }
            Ok((json!({}), Vec::new()))
        }
        _ => Err((
            "unknown_method".to_string(),
            format!("no such method: {method}"),
        )),
    }
}

/// The target side of a [`FakeAndroidServer`]: observes what the
/// server did, through the shared state.
#[derive(Debug)]
pub struct FakeAndroidOracle {
    state: Arc<Mutex<FakeState>>,
}

impl FakeAndroidOracle {
    /// Test control: report these permissions from now on.
    pub fn set_permissions(&self, permissions: Permissions) {
        lock_fake(&self.state).permissions = permissions;
    }

    /// The audit lines the server recorded.
    pub fn audit_lines(&self) -> Vec<String> {
        lock_fake(&self.state).audit.clone()
    }
}

impl TargetOracle for FakeAndroidOracle {
    fn set_content(&mut self, variant: u8) {
        lock_fake(&self.state).variant = variant;
    }

    fn reset_events(&mut self) {
        lock_fake(&self.state).events.clear();
    }

    fn received_clicks(&mut self) -> Vec<(i32, i32)> {
        lock_fake(&self.state)
            .events
            .iter()
            .filter_map(|e| match e {
                MockEvent::Click { x, y } => Some((*x, *y)),
                _ => None,
            })
            .collect()
    }

    fn received_text(&mut self) -> String {
        lock_fake(&self.state)
            .events
            .iter()
            .filter_map(|e| match e {
                MockEvent::Char(ch) => Some(*ch),
                _ => None,
            })
            .collect()
    }

    fn received_keys(&mut self) -> Vec<Key> {
        lock_fake(&self.state)
            .events
            .iter()
            .filter_map(|e| match e {
                MockEvent::Key(key) => Some(*key),
                _ => None,
            })
            .collect()
    }

    fn received_scrolls(&mut self) -> Vec<(i32, i32, i32, i32)> {
        lock_fake(&self.state)
            .events
            .iter()
            .filter_map(|e| match e {
                MockEvent::Scroll { x, y, dx, dy } => Some((*x, *y, *dx, *dy)),
                _ => None,
            })
            .collect()
    }

    fn received_drags(&mut self) -> Vec<(i32, i32, i32, i32)> {
        lock_fake(&self.state)
            .events
            .iter()
            .filter_map(|e| match e {
                MockEvent::Drag {
                    from_x,
                    from_y,
                    to_x,
                    to_y,
                } => Some((*from_x, *from_y, *to_x, *to_y)),
                _ => None,
            })
            .collect()
    }

    fn received_double_clicks(&mut self) -> Vec<(i32, i32)> {
        lock_fake(&self.state)
            .events
            .iter()
            .filter_map(|e| match e {
                MockEvent::DoubleClick { x, y } => Some((*x, *y)),
                _ => None,
            })
            .collect()
    }

    fn received_right_clicks(&mut self) -> Vec<(i32, i32)> {
        lock_fake(&self.state)
            .events
            .iter()
            .filter_map(|e| match e {
                MockEvent::RightClick { x, y } => Some((*x, *y)),
                _ => None,
            })
            .collect()
    }

    fn received_holds(&mut self) -> Vec<(Key, u32)> {
        lock_fake(&self.state)
            .events
            .iter()
            .filter_map(|e| match e {
                MockEvent::Hold { key, millis } => Some((*key, *millis)),
                _ => None,
            })
            .collect()
    }

    fn target_rect(&mut self) -> Option<(i32, i32, u32, u32)> {
        let state = lock_fake(&self.state);
        Some((0, 0, state.width, state.height))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(addr: SocketAddr, token: &str) -> AndroidConfig {
        AndroidConfig::loopback(addr, token, Role::Owner, TlsMode::Plain).unwrap()
    }

    #[test]
    fn non_loopback_addresses_are_refused() {
        let addr: SocketAddr = "8.8.8.8:9999".parse().unwrap();
        let err = AndroidConfig::loopback(addr, "tok", Role::Owner, TlsMode::Plain).unwrap_err();
        assert_eq!(err.code(), "invalid_argument");
        let addr: SocketAddr = "127.0.0.1:9999".parse().unwrap();
        assert!(AndroidConfig::loopback(addr, "tok", Role::Owner, TlsMode::Plain).is_ok());
    }

    #[test]
    fn empty_token_is_refused() {
        let addr: SocketAddr = "127.0.0.1:9999".parse().unwrap();
        assert!(AndroidConfig::loopback(addr, "", Role::Owner, TlsMode::Plain).is_err());
    }

    #[test]
    fn token_compare_is_exact() {
        assert!(tokens_equal("abc", "abc"));
        assert!(!tokens_equal("abc", "abd"));
        assert!(!tokens_equal("abc", "abcd"));
        assert!(!tokens_equal("", "a"));
    }

    #[test]
    fn wrong_token_is_a_typed_refusal() {
        let server = FakeAndroidServer::start(FakeServerConfig {
            token: "correct-token".to_string(),
            tls_identity: None,
        })
        .unwrap();
        let mut body = AndroidBody::new(test_config(server.addr(), "wrong-token"));
        let err = body.display_size().unwrap_err();
        assert_eq!(err.code(), "permission_denied");
        assert!(err.message().contains("pairing"));
        server.stop();
    }

    #[test]
    fn guest_role_cannot_send_input() {
        let server = FakeAndroidServer::start(FakeServerConfig {
            token: "tok".to_string(),
            tls_identity: None,
        })
        .unwrap();
        let config =
            AndroidConfig::loopback(server.addr(), "tok", Role::Guest, TlsMode::Plain).unwrap();
        let mut body = AndroidBody::new(config);
        assert!(body.display_size().is_ok());
        assert!(body.screenshot().is_ok());
        let err = body.click(10, 10).unwrap_err();
        assert_eq!(err.code(), "permission_denied");
        assert!(err.message().contains("owner role"));
        server.stop();
    }

    #[test]
    fn unknown_method_is_unsupported() {
        let server = FakeAndroidServer::start(FakeServerConfig {
            token: "tok".to_string(),
            tls_identity: None,
        })
        .unwrap();
        let mut client = ProtocolClient::connect(&test_config(server.addr(), "tok")).unwrap();
        let err = client.request("teleport", json!({})).unwrap_err();
        assert_eq!(err.code(), "unsupported");
        server.stop();
    }

    #[test]
    fn kill_switch_sets_the_flag_and_drops_the_body() {
        let server = FakeAndroidServer::start(FakeServerConfig {
            token: "tok".to_string(),
            tls_identity: None,
        })
        .unwrap();
        let state = server.state();
        let mut body = AndroidBody::new(test_config(server.addr(), "tok"));
        assert!(body.display_size().is_ok());
        body.kill();
        assert!(lock_fake(&state).killed);
        server.stop();
    }

    #[test]
    fn read_retries_once_after_a_lost_response() {
        let server = FakeAndroidServer::start(FakeServerConfig {
            token: "tok".to_string(),
            tls_identity: None,
        })
        .unwrap();
        let mut body = AndroidBody::new(test_config(server.addr(), "tok"));
        assert!(body.display_size().is_ok());
        assert_eq!(server.connections_served(), 1);
        // The next reply is lost: screenshot is a read, so
        // the client reconnects once and the call succeeds.
        server.drop_next_response();
        assert!(body.screenshot().is_ok());
        assert_eq!(server.connections_served(), 2);
        server.stop();
    }

    #[test]
    fn input_is_never_retried_after_a_lost_response() {
        let server = FakeAndroidServer::start(FakeServerConfig {
            token: "tok".to_string(),
            tls_identity: None,
        })
        .unwrap();
        let mut body = AndroidBody::new(test_config(server.addr(), "tok"));
        assert!(body.display_size().is_ok());
        assert_eq!(server.connections_served(), 1);
        // The click executes on the server, but the reply is
        // lost. The client must NOT retry: the outcome is
        // unknown, exactly one click was recorded, and no
        // second connection was opened for a retry.
        server.drop_next_response();
        let err = body.click(10, 10).unwrap_err();
        assert_eq!(err.code(), "unknown_outcome");
        assert!(err.message().contains("not retried"), "{}", err.message());
        assert_eq!(server.connections_served(), 1);
        let mut oracle = server.oracle();
        assert_eq!(oracle.received_clicks(), vec![(10, 10)]);
        server.stop();
    }

    #[test]
    fn version_mismatch_is_refused() {
        let server = FakeAndroidServer::start(FakeServerConfig {
            token: "tok".to_string(),
            tls_identity: None,
        })
        .unwrap();
        // Speak the framing by hand with v=1.
        let mut sock = TcpStream::connect(server.addr()).unwrap();
        let header = json!({"v": 1, "id": 1, "method": "display_size",
                            "params": {}, "auth": "tok", "role": "owner",
                            "payload_len": 0});
        let bytes = serde_json::to_vec(&header).unwrap();
        sock.write_all(&(bytes.len() as u32).to_be_bytes()).unwrap();
        sock.write_all(&bytes).unwrap();
        sock.flush().unwrap();
        let mut len_buf = [0u8; 4];
        sock.read_exact(&mut len_buf).unwrap();
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut buf = vec![0u8; len];
        sock.read_exact(&mut buf).unwrap();
        let reply: Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(reply["ok"], false);
        assert_eq!(reply["error"]["code"], "version_mismatch");
        server.stop();
    }

    #[test]
    fn focus_verification_uses_the_focused_window_query() {
        let server = FakeAndroidServer::start(FakeServerConfig {
            token: "tok".to_string(),
            tls_identity: None,
        })
        .unwrap();
        let mut body = AndroidBody::new(test_config(server.addr(), "tok"));
        let active = body.active_window().expect("fake reports a window");
        assert_eq!(active, 0xA11CE);
        body.set_expected_focus(Some(active));
        body.type_text("z").expect("matching focus types");
        body.set_expected_focus(Some(active ^ 0xFFFF));
        let err = body.type_text("z").unwrap_err();
        assert_eq!(err.code(), "focus_mismatch");
        body.set_expected_focus(None);
        body.type_text("z").expect("no expectation types");
        // Two "z": the matching-focus and no-expectation calls.
        // The mismatched-focus call was refused and typed
        // nothing.
        assert_eq!(server.oracle().received_text(), "zz");
        server.stop();
    }

    #[test]
    fn server_audits_every_call() {
        let server = FakeAndroidServer::start(FakeServerConfig {
            token: "tok".to_string(),
            tls_identity: None,
        })
        .unwrap();
        let mut body = AndroidBody::new(test_config(server.addr(), "tok"));
        body.click(10, 10).unwrap();
        body.display_size().unwrap();
        let audit = server.oracle().audit_lines();
        // The click checks bounds first (one display_size round
        // trip), then clicks; the explicit display_size is served
        // from the client's cache and never reaches the server.
        assert_eq!(audit.len(), 2);
        assert!(audit[0].contains("\"display_size\""));
        assert!(audit[1].contains("\"click\""));
        assert!(audit[1].contains("\"ok\""));
        server.stop();
    }
}
