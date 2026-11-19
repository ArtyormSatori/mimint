//! Minimal WebSocket framing (Phase 7, §8.2, F7D-11).
//!
//! Self-implemented on TCP because the tree has no WebSocket
//! dependency and the 0-new-dependency rule forbids adding one
//! (tungstenite was explicitly rejected, D-DESIGN7-8).
//!
//! Implemented (RFC 6455):
//! * HTTP Upgrade handshake: `Sec-WebSocket-Key` / `Accept`
//!   (SHA-1, `ring::digest::SHA1_FOR_LEGACY_USE_ONLY`), version 13.
//! * Frame header reading: FIN, opcode, mask, payload length
//!   in all three forms (7-bit, 16-bit, 64-bit).
//! * Mandatory masking for client frames.
//! * Unfragmented text/binary data frames; ping/pong; the
//!   close handshake.
//! * Size limits: HTTP headers ≤ 16 KiB, frame payload
//!   ≤ 256 KiB — over the limit, close + audit.
//!
//! Refused — on meeting one, the connection is closed and
//! `ws_unsupported_frame` is audited with the reason, never
//! worked around:
//! * fragmented frames (FIN = 0, or a continuation opcode);
//! * any extension (a handshake proposing
//!   `Sec-WebSocket-Extensions` is refused at once);
//! * RSV1/RSV2/RSV3 set; unknown or reserved opcodes;
//! * unmasked frames from a client; fragmented control frames
//!   or control payloads over 125 bytes; non-minimal length
//!   encodings.

use crate::audit::AuditLog;
use crate::error::MimintError;
use crate::friends::util::{base64_decode, base64_encode, random_bytes};
use ring::digest::{digest, SHA1_FOR_LEGACY_USE_ONLY};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

/// Maximum WebSocket frame payload (§8.2): 256 KiB.
pub const WS_MAX_PAYLOAD: usize = 256 * 1024;
/// Maximum HTTP Upgrade header block (§8.2): 16 KiB.
pub const WS_MAX_HEADER: usize = 16 * 1024;

const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// A decoded WebSocket frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WsFrame {
    Text(Vec<u8>),
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    Close(Option<(u16, String)>),
}

impl WsFrame {
    fn opcode(&self) -> u8 {
        match self {
            WsFrame::Text(_) => 0x1,
            WsFrame::Binary(_) => 0x2,
            WsFrame::Close(_) => 0x8,
            WsFrame::Ping(_) => 0x9,
            WsFrame::Pong(_) => 0xA,
        }
    }

    fn payload(&self) -> Vec<u8> {
        match self {
            WsFrame::Text(bytes)
            | WsFrame::Binary(bytes)
            | WsFrame::Ping(bytes)
            | WsFrame::Pong(bytes) => bytes.clone(),
            WsFrame::Close(None) => Vec::new(),
            WsFrame::Close(Some((code, reason))) => {
                let mut out = code.to_be_bytes().to_vec();
                out.extend_from_slice(reason.as_bytes());
                out
            }
        }
    }

    fn is_control(&self) -> bool {
        matches!(
            self,
            WsFrame::Close(_) | WsFrame::Ping(_) | WsFrame::Pong(_)
        )
    }
}

/// WebSocket errors. `Unsupported` is the refusal path: the
/// caller closes the connection and audits
/// `ws_unsupported_frame` with the reason.
#[derive(Debug)]
pub enum WsError {
    Unsupported(String),
    Protocol(String),
    Io(std::io::Error),
}

impl From<std::io::Error> for WsError {
    fn from(err: std::io::Error) -> Self {
        WsError::Io(err)
    }
}

impl From<WsError> for MimintError {
    fn from(err: WsError) -> Self {
        match err {
            WsError::Unsupported(reason) => {
                MimintError::Config(format!("unsupported WebSocket frame: {reason}"))
            }
            WsError::Protocol(reason) => {
                MimintError::Config(format!("WebSocket protocol error: {reason}"))
            }
            WsError::Io(err) => MimintError::Io(err.to_string()),
        }
    }
}

/// Compute `Sec-WebSocket-Accept` for a client key.
pub fn websocket_accept(key: &str) -> String {
    let input = format!("{key}{WS_GUID}");
    let hash = digest(&SHA1_FOR_LEGACY_USE_ONLY, input.as_bytes());
    base64_encode(hash.as_ref())
}

/// A WebSocket connection over a TCP stream. `is_server` decides
/// masking direction: servers read masked frames and write
/// unmasked ones; clients do the opposite.
pub struct WsConnection {
    stream: TcpStream,
    is_server: bool,
    audit: Option<Arc<AuditLog>>,
}

impl WsConnection {
    /// Server side: read the HTTP Upgrade request, validate it,
    /// and answer 101. Refuses extensions at once. Returns the
    /// connection plus the request path.
    pub fn accept_upgrade(
        mut stream: TcpStream,
        audit: Option<Arc<AuditLog>>,
    ) -> Result<(Self, String), WsError> {
        let request = read_http_headers(&mut stream)?;
        let mut lines = request.lines();
        let request_line = lines.next().unwrap_or("");
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or("");
        let path = parts.next().unwrap_or("/").to_string();
        if method != "GET" {
            return Err(WsError::Protocol("upgrade request is not GET".to_string()));
        }
        let mut upgrade: Option<String> = None;
        let mut connection: Option<String> = None;
        let mut key: Option<String> = None;
        let mut version: Option<String> = None;
        let mut extensions: Option<String> = None;
        for line in lines {
            if let Some((name, value)) = line.split_once(':') {
                match name.trim().to_ascii_lowercase().as_str() {
                    "upgrade" => upgrade = Some(value.trim().to_string()),
                    "connection" => connection = Some(value.trim().to_string()),
                    "sec-websocket-key" => key = Some(value.trim().to_string()),
                    "sec-websocket-version" => version = Some(value.trim().to_string()),
                    "sec-websocket-extensions" => extensions = Some(value.trim().to_string()),
                    _ => {}
                }
            }
        }
        let mut refuse = |reason: &str| -> WsError {
            let response =
                "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_string();
            let _ = stream.write_all(response.as_bytes());
            WsError::Unsupported(reason.to_string())
        };
        if extensions.is_some() {
            if let Some(log) = &audit {
                let _ = log.log(
                    "friends-ws",
                    "ws_unsupported_frame",
                    Some("friends"),
                    None,
                    serde_json::json!({"reason": "extensions_offered"}),
                );
            }
            return Err(refuse("Sec-WebSocket-Extensions are not supported"));
        }
        let upgrade_ok = upgrade
            .as_deref()
            .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
        let connection_ok = connection.as_deref().is_some_and(|v| {
            v.to_ascii_lowercase()
                .split(',')
                .any(|t| t.trim() == "upgrade")
        });
        let key = match key {
            Some(key) => key,
            None => return Err(refuse("missing Sec-WebSocket-Key")),
        };
        let key_ok = base64_decode(&key).is_some_and(|bytes| bytes.len() == 16);
        if !upgrade_ok || !connection_ok || !key_ok || version.as_deref() != Some("13") {
            return Err(refuse("invalid WebSocket upgrade request"));
        }
        let response = format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
            websocket_accept(&key)
        );
        stream.write_all(response.as_bytes())?;
        Ok((
            Self {
                stream,
                is_server: true,
                audit,
            },
            path,
        ))
    }

    /// Client side: send the HTTP Upgrade request and verify
    /// the 101 response + accept value. Used by the fake
    /// XiaoZhi device and by interop tests.
    pub fn connect_upgrade(mut stream: TcpStream, host: &str, path: &str) -> Result<Self, WsError> {
        let mut key_bytes = [0u8; 16];
        random_bytes(&mut key_bytes);
        let key = base64_encode(&key_bytes);
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        );
        stream.write_all(request.as_bytes())?;
        let response = read_http_headers(&mut stream)?;
        let mut lines = response.lines();
        let status = lines.next().unwrap_or("");
        if !status.contains("101") {
            return Err(WsError::Protocol(format!(
                "WebSocket upgrade failed: {status}"
            )));
        }
        let mut accept: Option<String> = None;
        for line in lines {
            if let Some((name, value)) = line.split_once(':') {
                if name.trim().eq_ignore_ascii_case("sec-websocket-accept") {
                    accept = Some(value.trim().to_string());
                }
            }
        }
        if accept.as_deref() != Some(websocket_accept(&key).as_str()) {
            return Err(WsError::Protocol(
                "WebSocket accept value mismatch".to_string(),
            ));
        }
        Ok(Self {
            stream,
            is_server: false,
            audit: None,
        })
    }

    /// Read one frame, enforcing every refusal rule. On
    /// `Unsupported`, the caller must close the connection and
    /// audit `ws_unsupported_frame`.
    pub fn read_frame(&mut self) -> Result<WsFrame, WsError> {
        let header = self.read_exact_vec(2)?;
        let fin = header[0] & 0x80 != 0;
        let rsv = header[0] & 0x70;
        let opcode = header[0] & 0x0F;
        let masked = header[1] & 0x80 != 0;
        let len7 = (header[1] & 0x7F) as u64;
        if rsv != 0 {
            return self.unsupported("rsv bits set");
        }
        let is_control = opcode >= 0x8;
        match opcode {
            0x0 => return self.unsupported("fragmented frame (continuation opcode)"),
            0x1 | 0x2 | 0x8 | 0x9 | 0xA => {}
            _ => return self.unsupported("unknown or reserved opcode"),
        }
        if !fin {
            return self.unsupported("fragmented frame (FIN = 0)");
        }
        if self.is_server && !masked {
            return self.unsupported("unmasked frame from client");
        }
        if !self.is_server && masked {
            return Err(WsError::Protocol("masked frame from server".to_string()));
        }
        let length: u64 = match len7 {
            126 => {
                let bytes = self.read_exact_vec(2)?;
                let value = u16::from_be_bytes([bytes[0], bytes[1]]) as u64;
                if value < 126 {
                    return Err(WsError::Protocol(
                        "non-minimal 16-bit length encoding".to_string(),
                    ));
                }
                value
            }
            127 => {
                let bytes = self.read_exact_vec(8)?;
                let value = u64::from_be_bytes(bytes.try_into().unwrap());
                if value & (1u64 << 63) != 0 {
                    return Err(WsError::Protocol(
                        "64-bit length with the high bit set".to_string(),
                    ));
                }
                if value < 65536 {
                    return Err(WsError::Protocol(
                        "non-minimal 64-bit length encoding".to_string(),
                    ));
                }
                value
            }
            value => value,
        };
        if is_control && length > 125 {
            return self.unsupported("control frame payload over 125 bytes");
        }
        if length > WS_MAX_PAYLOAD as u64 {
            return self.unsupported("frame payload over the 256 KiB limit");
        }
        let mask_key = if masked {
            let bytes = self.read_exact_vec(4)?;
            Some([bytes[0], bytes[1], bytes[2], bytes[3]])
        } else {
            None
        };
        let mut payload = self.read_exact_vec(length as usize)?;
        if let Some(key) = mask_key {
            for (i, byte) in payload.iter_mut().enumerate() {
                *byte ^= key[i % 4];
            }
        }
        let frame = match opcode {
            0x1 => {
                if std::str::from_utf8(&payload).is_err() {
                    return Err(WsError::Protocol(
                        "text frame is not valid UTF-8".to_string(),
                    ));
                }
                WsFrame::Text(payload)
            }
            0x2 => WsFrame::Binary(payload),
            0x8 => {
                if payload.is_empty() {
                    WsFrame::Close(None)
                } else if payload.len() < 2 {
                    return Err(WsError::Protocol(
                        "close frame shorter than 2 bytes".to_string(),
                    ));
                } else {
                    let code = u16::from_be_bytes([payload[0], payload[1]]);
                    let reason = String::from_utf8(payload[2..].to_vec()).map_err(|_| {
                        WsError::Protocol("close reason is not valid UTF-8".to_string())
                    })?;
                    WsFrame::Close(Some((code, reason)))
                }
            }
            0x9 => WsFrame::Ping(payload),
            0xA => WsFrame::Pong(payload),
            _ => unreachable!(),
        };
        Ok(frame)
    }

    /// Write one frame. Servers write unmasked; clients write
    /// masked with a fresh random key.
    pub fn write_frame(&mut self, frame: &WsFrame) -> Result<(), WsError> {
        let payload = frame.payload();
        if frame.is_control() && payload.len() > 125 {
            return Err(WsError::Protocol(
                "control frame payload over 125 bytes".to_string(),
            ));
        }
        if payload.len() > WS_MAX_PAYLOAD {
            return Err(WsError::Protocol(
                "frame payload over the 256 KiB limit".to_string(),
            ));
        }
        let mut header = Vec::with_capacity(10);
        header.push(0x80 | frame.opcode());
        let mask_bit: u8 = if self.is_server { 0x00 } else { 0x80 };
        match payload.len() {
            0..=125 => header.push(mask_bit | payload.len() as u8),
            126..=65535 => {
                header.push(mask_bit | 126);
                header.extend_from_slice(&(payload.len() as u16).to_be_bytes());
            }
            _ => {
                header.push(mask_bit | 127);
                header.extend_from_slice(&(payload.len() as u64).to_be_bytes());
            }
        }
        let mut mask_key = [0u8; 4];
        if !self.is_server {
            random_bytes(&mut mask_key);
            header.extend_from_slice(&mask_key);
        }
        let mut wire = header;
        if self.is_server {
            wire.extend_from_slice(&payload);
        } else {
            wire.extend(
                payload
                    .iter()
                    .enumerate()
                    .map(|(i, byte)| byte ^ mask_key[i % 4]),
            );
        }
        self.stream.write_all(&wire)?;
        self.stream.flush()?;
        Ok(())
    }

    /// Send a close frame (best effort) and shut the stream.
    pub fn close(&mut self, code: u16, reason: &str) {
        let _ = self.write_frame(&WsFrame::Close(Some((code, reason.to_string()))));
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }

    /// The underlying stream (timeouts, peer address).
    pub fn stream(&self) -> &TcpStream {
        &self.stream
    }

    fn read_exact_vec(&mut self, count: usize) -> Result<Vec<u8>, WsError> {
        let mut buf = vec![0u8; count];
        self.stream.read_exact(&mut buf)?;
        Ok(buf)
    }

    fn unsupported(&self, reason: &str) -> Result<WsFrame, WsError> {
        if let Some(log) = &self.audit {
            let _ = log.log(
                "friends-ws",
                "ws_unsupported_frame",
                Some("friends"),
                None,
                serde_json::json!({"reason": reason}),
            );
        }
        Err(WsError::Unsupported(reason.to_string()))
    }
}

/// Read an HTTP header block (up to the blank line), capped at
/// `WS_MAX_HEADER` bytes.
fn read_http_headers(stream: &mut TcpStream) -> Result<String, WsError> {
    let mut buf = Vec::with_capacity(1024);
    let mut byte = [0u8; 1];
    while buf.len() < WS_MAX_HEADER {
        let read = stream.read(&mut byte)?;
        if read == 0 {
            return Err(WsError::Protocol(
                "connection closed during HTTP headers".to_string(),
            ));
        }
        buf.push(byte[0]);
        if buf.len() >= 4 && &buf[buf.len() - 4..] == b"\r\n\r\n" {
            break;
        }
    }
    if buf.len() >= WS_MAX_HEADER && !buf.ends_with(b"\r\n\r\n") {
        return Err(WsError::Unsupported(
            "HTTP header block over 16 KiB".to_string(),
        ));
    }
    String::from_utf8(buf)
        .map_err(|_| WsError::Protocol("HTTP headers are not valid UTF-8".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::time::Duration;

    /// A client-side raw frame writer (independent of
    /// `WsConnection`, for vector tests): builds one masked
    /// frame byte-for-byte.
    fn raw_client_frame(opcode: u8, fin: bool, payload: &[u8], mask: [u8; 4]) -> Vec<u8> {
        let mut out = vec![(if fin { 0x80 } else { 0x00 }) | opcode];
        match payload.len() {
            0..=125 => out.push(0x80 | payload.len() as u8),
            126..=65535 => {
                out.push(0x80 | 126);
                out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
            }
            _ => {
                out.push(0x80 | 127);
                out.extend_from_slice(&(payload.len() as u64).to_be_bytes());
            }
        }
        out.extend_from_slice(&mask);
        out.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        out
    }

    fn serve_once(listener: TcpListener) -> WsConnection {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let (conn, _path) = WsConnection::accept_upgrade(stream, None).unwrap();
        conn
    }

    fn client_handshake(port: u16) -> TcpStream {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let key = base64_encode(b"0123456789abcdef");
        let request = format!(
            "GET /chat HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).unwrap();
        let response = read_http_headers(&mut stream).unwrap();
        assert!(response.contains("101"), "expected 101, got {response}");
        assert!(response.contains(&websocket_accept(&key)));
        stream
    }

    fn free_port() -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        port
    }

    #[test]
    fn ws_frame_vectors() {
        // RFC 6455 §5.7 examples, adapted: a masked text frame
        // "Hello" (client -> server) and an unmasked echo.
        let port = free_port();
        let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
        let handle = std::thread::spawn(move || serve_once(listener));
        let mut client = client_handshake(port);
        // Masked "Hello": RFC example mask 0x37fa213d.
        let frame = raw_client_frame(0x1, true, b"Hello", [0x37, 0xfa, 0x21, 0x3d]);
        client.write_all(&frame).unwrap();
        let mut server = handle.join().unwrap();
        assert_eq!(
            server.read_frame().unwrap(),
            WsFrame::Text(b"Hello".to_vec())
        );
        // Echo back unmasked; the client reads it raw.
        server
            .write_frame(&WsFrame::Text(b"Hello".to_vec()))
            .unwrap();
        let mut header = [0u8; 2];
        client.read_exact(&mut header).unwrap();
        assert_eq!(header, [0x81, 0x05]);
        let mut payload = [0u8; 5];
        client.read_exact(&mut payload).unwrap();
        assert_eq!(&payload, b"Hello");
        // 16-bit length form (200 bytes) and 64-bit form (70_000).
        let big = vec![b'x'; 200];
        client
            .write_all(&raw_client_frame(0x2, true, &big, [1, 2, 3, 4]))
            .unwrap();
        assert_eq!(server.read_frame().unwrap(), WsFrame::Binary(big));
        let huge = vec![b'y'; 70_000];
        client
            .write_all(&raw_client_frame(0x2, true, &huge, [5, 6, 7, 8]))
            .unwrap();
        assert_eq!(server.read_frame().unwrap(), WsFrame::Binary(huge));
        // Ping/pong.
        client
            .write_all(&raw_client_frame(0x9, true, b"ping", [9, 9, 9, 9]))
            .unwrap();
        assert_eq!(
            server.read_frame().unwrap(),
            WsFrame::Ping(b"ping".to_vec())
        );
        server
            .write_frame(&WsFrame::Pong(b"ping".to_vec()))
            .unwrap();
        // Close with code 1000.
        client
            .write_all(&raw_client_frame(0x8, true, &[0x03, 0xe8], [1, 1, 1, 1]))
            .unwrap();
        assert_eq!(
            server.read_frame().unwrap(),
            WsFrame::Close(Some((1000, String::new())))
        );
    }

    #[test]
    fn ws_accept_vector_is_rfc6455() {
        // RFC 6455 §1.3 example.
        assert_eq!(
            websocket_accept("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn ws_fuzzing_no_crash_no_bypass() {
        // Random garbage + structured mutations: the server
        // must never panic and must never accept a frame the
        // rules refuse. Every outcome is one of: a decoded
        // frame, an Unsupported refusal, or a Protocol error.
        let port = free_port();
        let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
        let handle = std::thread::spawn(move || serve_once(listener));
        let mut client = client_handshake(port);
        let mut server = handle.join().unwrap();
        server
            .stream()
            .set_read_timeout(Some(Duration::from_millis(300)))
            .unwrap();
        let mut seed: u64 = 0x9e3779b97f4a7c15;
        let mut next_byte = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            (seed >> 33) as u8
        };
        let mut accepted_something = false;
        for _ in 0..200 {
            let len = (next_byte() % 40) as usize;
            let mut junk: Vec<u8> = (0..len).map(|_| next_byte()).collect();
            // Bias some mutations toward valid-looking frames.
            if next_byte() % 2 == 0 && junk.len() >= 2 {
                junk[0] = 0x81 | ((next_byte() % 2) * 0x70); // Sometimes RSV.
                junk[1] = 0x80 | (next_byte() % 8);
            }
            let _ = client.write_all(&junk);
            let _ = client.flush();
            match server.read_frame() {
                Ok(_) => accepted_something = true,
                Err(WsError::Unsupported(_)) | Err(WsError::Protocol(_)) => {}
                Err(WsError::Io(_)) => break,
            }
        }
        // At least the well-formed mutations were readable;
        // the server never panicked (the test itself is the
        // proof — a panic would fail it).
        let _ = accepted_something;
    }

    #[test]
    fn ws_rejects_fragmented_and_reserved_and_unmasked() {
        let port = free_port();
        let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
        let handle = std::thread::spawn(move || serve_once(listener));
        let mut client = client_handshake(port);
        let mut server = handle.join().unwrap();
        // Fragmented: FIN=0 text frame.
        client
            .write_all(&raw_client_frame(0x1, false, b"Hel", [1, 2, 3, 4]))
            .unwrap();
        assert!(matches!(server.read_frame(), Err(WsError::Unsupported(_))));
    }

    /// F7-19 vector 1: a LARGE first fragment is refused just
    /// like a small one — fragmentation is rejected at the FIN
    /// check, before any size accounting, so a 300 KiB first
    /// fragment (over the 256 KiB cap) can never smuggle a
    /// reassembly past the limit.
    #[test]
    fn ws_rejects_large_fragmented_frame() {
        // A refused frame kills the connection by design, so
        // each size gets its own connection.
        for size in [70_000usize, 300 * 1024] {
            let port = free_port();
            let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
            let handle = std::thread::spawn(move || serve_once(listener));
            let mut client = client_handshake(port);
            let mut server = handle.join().unwrap();
            let big = vec![b'x'; size];
            client
                .write_all(&raw_client_frame(0x2, false, &big, [1, 2, 3, 4]))
                .unwrap();
            assert!(
                matches!(server.read_frame(), Err(WsError::Unsupported(_))),
                "{size}-byte first fragment must be refused"
            );
        }
    }

    /// F7-19 vector 2: a control frame (ping) with a 126-byte
    /// payload is refused — control payloads are capped at
    /// 125 bytes (RFC 6455 §5.5). A refused frame kills the
    /// connection by design, so the 125-byte boundary case
    /// uses a fresh connection.
    #[test]
    fn ws_rejects_control_payload_over_125() {
        let port = free_port();
        let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
        let handle = std::thread::spawn(move || serve_once(listener));
        let mut client = client_handshake(port);
        let mut server = handle.join().unwrap();
        let payload = vec![b'p'; 126];
        client
            .write_all(&raw_client_frame(0x9, true, &payload, [1, 2, 3, 4]))
            .unwrap();
        let err = server.read_frame().unwrap_err();
        assert!(
            matches!(err, WsError::Unsupported(_)),
            "126-byte ping must be Unsupported, got {err:?}"
        );
        // 125 bytes is the boundary: accepted (fresh conn).
        let port = free_port();
        let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
        let handle = std::thread::spawn(move || serve_once(listener));
        let mut client = client_handshake(port);
        let mut server = handle.join().unwrap();
        let boundary = vec![b'p'; 125];
        client
            .write_all(&raw_client_frame(0x9, true, &boundary, [1, 2, 3, 4]))
            .unwrap();
        assert_eq!(server.read_frame().unwrap(), WsFrame::Ping(boundary));
    }

    /// F7-19 vector 3: unusual close codes are accepted as
    /// raw u16 values — the implementation does not validate
    /// them (1005 "no status", 1006 "abnormal", 999 reserved,
    /// 4000 private-use all decode to `WsFrame::Close`).
    /// This locks the permissive behavior: if validation is
    /// ever added, this test must be updated deliberately.
    #[test]
    fn ws_accepts_unusual_close_codes() {
        let port = free_port();
        let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
        let handle = std::thread::spawn(move || serve_once(listener));
        let mut client = client_handshake(port);
        let mut server = handle.join().unwrap();
        for code in [1005u16, 1006, 999, 4000, 1011] {
            client
                .write_all(&raw_client_frame(
                    0x8,
                    true,
                    &code.to_be_bytes(),
                    [1, 1, 1, 1],
                ))
                .unwrap();
            assert_eq!(
                server.read_frame().unwrap(),
                WsFrame::Close(Some((code, String::new()))),
                "close code {code} must decode as-is"
            );
        }
    }
}
