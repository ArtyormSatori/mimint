//! Wire framing + message kinds for the friends protocol
//! (Phase 7, §2.2, §2.5).
//!
//! Framing reuses the shape of the Android body protocol (u32
//! big-endian JSON length + JSON + payload bytes); the contents
//! are new. Every message is one frame:
//!
//! ```text
//! [u32 big-endian: JSON length][JSON bytes][payload bytes]
//! ```
//!
//! The JSON carries `payload_len` (0 when absent) naming the
//! payload that follows. Plaintext messages (handshake and
//! ceremony kinds) put their whole body in the JSON and have no
//! payload. Encrypted messages put an [`EncryptedHeader`] in the
//! JSON and the AES-256-GCM ciphertext in the payload; the AAD
//! is the header re-encoded from its parsed fields in fixed
//! field order (never the raw received bytes).
//!
//! Kind is a CLOSED allow-list (§2.5). Unknown kinds, unknown
//! header fields, wrong versions, and oversize frames are
//! protocol errors: the session closes.
//!
//! Build note (deviation D-BUILD-1, see CROSS-CHECK): §2.5
//! names no wire kind for remote driver methods (§5.3), yet
//! §5.3/§5.5 require driver actions to flow over an
//! authenticated session. This module adds exactly two kinds,
//! `driver_request` / `driver_response`, to the otherwise
//! closed allow-list. No other kind exists.

use crate::error::MimintError;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

/// Protocol version. Anything else is a version error.
pub const PROTOCOL_VERSION: u32 = 2;

/// Maximum JSON header size (§2.2): 64 KiB.
pub const MAX_HEADER_LEN: usize = 64 * 1024;

/// Maximum payload of one frame (§2.2): 4 MiB (one sync batch).
pub const MAX_FRAME_PAYLOAD: usize = 4 * 1024 * 1024;

/// Maximum size of one sync record (§4.3): 256 KiB.
pub const MAX_RECORD_BYTES: usize = 256 * 1024;
/// Maximum records in one `sync_push` batch (§4.3).
pub const MAX_SYNC_BATCH: usize = 100;
/// Maximum total bytes in one `sync_push` batch (§4.3).
pub const MAX_SYNC_BATCH_BYTES: usize = 4 * 1024 * 1024;

/// Every wire kind. The list is closed: [`MessageKind::parse`]
/// rejects anything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MessageKind {
    Hello,
    Auth,
    HandshakeError,
    SessionConfirm,
    Rekey,
    RekeyAck,
    PairRequest,
    PairResponse,
    PairConfirm,
    PairAccept,
    PairingError,
    SyncOffer,
    SyncRequest,
    SyncPush,
    SyncAck,
    ApprovalCard,
    ApprovalDecision,
    HandoffOffer,
    HandoffAccept,
    HandoffReject,
    HandoffComplete,
    Ping,
    Pong,
    RevokeNotice,
    /// Build addition D-BUILD-1 (see module docs).
    DriverRequest,
    /// Build addition D-BUILD-1 (see module docs).
    DriverResponse,
}

impl MessageKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MessageKind::Hello => "hello",
            MessageKind::Auth => "auth",
            MessageKind::HandshakeError => "handshake_error",
            MessageKind::SessionConfirm => "session_confirm",
            MessageKind::Rekey => "rekey",
            MessageKind::RekeyAck => "rekey_ack",
            MessageKind::PairRequest => "pair_request",
            MessageKind::PairResponse => "pair_response",
            MessageKind::PairConfirm => "pair_confirm",
            MessageKind::PairAccept => "pair_accept",
            MessageKind::PairingError => "pairing_error",
            MessageKind::SyncOffer => "sync_offer",
            MessageKind::SyncRequest => "sync_request",
            MessageKind::SyncPush => "sync_push",
            MessageKind::SyncAck => "sync_ack",
            MessageKind::ApprovalCard => "approval_card",
            MessageKind::ApprovalDecision => "approval_decision",
            MessageKind::HandoffOffer => "handoff_offer",
            MessageKind::HandoffAccept => "handoff_accept",
            MessageKind::HandoffReject => "handoff_reject",
            MessageKind::HandoffComplete => "handoff_complete",
            MessageKind::Ping => "ping",
            MessageKind::Pong => "pong",
            MessageKind::RevokeNotice => "revoke_notice",
            MessageKind::DriverRequest => "driver_request",
            MessageKind::DriverResponse => "driver_response",
        }
    }

    /// Parse a kind string; unknown strings are rejected.
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "hello" => MessageKind::Hello,
            "auth" => MessageKind::Auth,
            "handshake_error" => MessageKind::HandshakeError,
            "session_confirm" => MessageKind::SessionConfirm,
            "rekey" => MessageKind::Rekey,
            "rekey_ack" => MessageKind::RekeyAck,
            "pair_request" => MessageKind::PairRequest,
            "pair_response" => MessageKind::PairResponse,
            "pair_confirm" => MessageKind::PairConfirm,
            "pair_accept" => MessageKind::PairAccept,
            "pairing_error" => MessageKind::PairingError,
            "sync_offer" => MessageKind::SyncOffer,
            "sync_request" => MessageKind::SyncRequest,
            "sync_push" => MessageKind::SyncPush,
            "sync_ack" => MessageKind::SyncAck,
            "approval_card" => MessageKind::ApprovalCard,
            "approval_decision" => MessageKind::ApprovalDecision,
            "handoff_offer" => MessageKind::HandoffOffer,
            "handoff_accept" => MessageKind::HandoffAccept,
            "handoff_reject" => MessageKind::HandoffReject,
            "handoff_complete" => MessageKind::HandoffComplete,
            "ping" => MessageKind::Ping,
            "pong" => MessageKind::Pong,
            "revoke_notice" => MessageKind::RevokeNotice,
            "driver_request" => MessageKind::DriverRequest,
            "driver_response" => MessageKind::DriverResponse,
            _ => return None,
        })
    }

    /// True for the 24 kinds named in §2.5 (excludes the two
    /// build-added driver kinds).
    pub fn is_design_kind(self) -> bool {
        !matches!(
            self,
            MessageKind::DriverRequest | MessageKind::DriverResponse
        )
    }

    /// True for kinds that travel as plaintext JSON (handshake
    /// and ceremony). Everything else is AEAD-encrypted.
    pub fn is_plaintext(self) -> bool {
        matches!(
            self,
            MessageKind::Hello
                | MessageKind::Auth
                | MessageKind::HandshakeError
                | MessageKind::PairRequest
                | MessageKind::PairResponse
                | MessageKind::PairConfirm
                | MessageKind::PairAccept
                | MessageKind::PairingError
        )
    }

    /// True for kinds allowed only after `SESSION_CONFIRM` has
    /// been exchanged in both directions (§2.5).
    pub fn requires_confirm(self) -> bool {
        !matches!(
            self,
            MessageKind::Hello
                | MessageKind::Auth
                | MessageKind::HandshakeError
                | MessageKind::SessionConfirm
                | MessageKind::PairRequest
                | MessageKind::PairResponse
                | MessageKind::PairConfirm
                | MessageKind::PairAccept
                | MessageKind::PairingError
        )
    }

    /// All kinds, in §2.5 table order plus the driver kinds.
    pub fn all() -> &'static [MessageKind] {
        &[
            MessageKind::Hello,
            MessageKind::Auth,
            MessageKind::HandshakeError,
            MessageKind::SessionConfirm,
            MessageKind::Rekey,
            MessageKind::RekeyAck,
            MessageKind::PairRequest,
            MessageKind::PairResponse,
            MessageKind::PairConfirm,
            MessageKind::PairAccept,
            MessageKind::PairingError,
            MessageKind::SyncOffer,
            MessageKind::SyncRequest,
            MessageKind::SyncPush,
            MessageKind::SyncAck,
            MessageKind::ApprovalCard,
            MessageKind::ApprovalDecision,
            MessageKind::HandoffOffer,
            MessageKind::HandoffAccept,
            MessageKind::HandoffReject,
            MessageKind::HandoffComplete,
            MessageKind::Ping,
            MessageKind::Pong,
            MessageKind::RevokeNotice,
            MessageKind::DriverRequest,
            MessageKind::DriverResponse,
        ]
    }
}

/// Header of an encrypted message (§2.2). Field order is the
/// canonical AAD order: `v`, `kind`, `from`, `seq`, `nonce`,
/// `payload_len`. Unknown fields are rejected.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EncryptedHeader {
    pub v: u32,
    pub kind: String,
    pub from: String,
    pub seq: u64,
    /// 12-byte nonce, hex.
    pub nonce: String,
    pub payload_len: usize,
}

impl EncryptedHeader {
    /// Canonical bytes for AAD: the header re-encoded from its
    /// parsed fields (serde preserves struct field order).
    pub fn canonical_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("header serialization cannot fail")
    }
}

/// One raw frame: the exact JSON bytes received plus the payload
/// bytes named by `payload_len`.
#[derive(Debug, Clone)]
pub struct Frame {
    pub json: Vec<u8>,
    pub payload: Vec<u8>,
}

/// Read one frame: u32 length, JSON bytes, payload bytes.
/// Enforces the §2.2 caps. `io` errors (including timeouts and
/// EOF) surface as [`MimintError::Config`] with a protocol
/// prefix so callers can close the session uniformly.
pub fn read_frame(reader: &mut impl Read) -> Result<Frame, MimintError> {
    let mut len_buf = [0u8; 4];
    reader
        .read_exact(&mut len_buf)
        .map_err(|err| MimintError::Config(format!("friends protocol read error: {err}")))?;
    let json_len = u32::from_be_bytes(len_buf) as usize;
    if json_len == 0 || json_len > MAX_HEADER_LEN {
        return Err(MimintError::Config(format!(
            "friends protocol error: header length {json_len} out of range"
        )));
    }
    let mut json = vec![0u8; json_len];
    reader
        .read_exact(&mut json)
        .map_err(|err| MimintError::Config(format!("friends protocol read error: {err}")))?;
    let payload_len = payload_len_of(&json)?;
    if payload_len > MAX_FRAME_PAYLOAD {
        return Err(MimintError::Config(format!(
            "friends protocol error: payload length {payload_len} exceeds the limit"
        )));
    }
    let mut payload = vec![0u8; payload_len];
    reader
        .read_exact(&mut payload)
        .map_err(|err| MimintError::Config(format!("friends protocol read error: {err}")))?;
    Ok(Frame { json, payload })
}

/// The `payload_len` named by a JSON header/message (0 when the
/// field is absent, as in HELLO and ceremony messages).
pub fn payload_len_of(json: &[u8]) -> Result<usize, MimintError> {
    let value: serde_json::Value = serde_json::from_slice(json)
        .map_err(|err| MimintError::Config(format!("friends protocol error: bad JSON: {err}")))?;
    match value.get("payload_len") {
        None => Ok(0),
        Some(serde_json::Value::Number(n)) => n.as_u64().map(|v| v as usize).ok_or_else(|| {
            MimintError::Config("friends protocol error: payload_len is not a number".to_string())
        }),
        Some(_) => Err(MimintError::Config(
            "friends protocol error: payload_len is not a number".to_string(),
        )),
    }
}

/// The `kind` named by a JSON header/message.
pub fn kind_of(json: &[u8]) -> Result<MessageKind, MimintError> {
    let value: serde_json::Value = serde_json::from_slice(json)
        .map_err(|err| MimintError::Config(format!("friends protocol error: bad JSON: {err}")))?;
    let kind_text = value
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| MimintError::Config("friends protocol error: missing kind".to_string()))?;
    MessageKind::parse(kind_text).ok_or_else(|| {
        MimintError::Config(format!(
            "friends protocol error: unknown message kind {kind_text}"
        ))
    })
}

/// The `v` named by a JSON header/message.
pub fn version_of(json: &[u8]) -> Result<u32, MimintError> {
    let value: serde_json::Value = serde_json::from_slice(json)
        .map_err(|err| MimintError::Config(format!("friends protocol error: bad JSON: {err}")))?;
    value
        .get("v")
        .and_then(serde_json::Value::as_u64)
        .map(|v| v as u32)
        .ok_or_else(|| MimintError::Config("friends protocol error: missing v".to_string()))
}

/// Write one frame: u32 length + JSON + payload.
pub fn write_frame(
    writer: &mut impl Write,
    json: &[u8],
    payload: &[u8],
) -> Result<(), MimintError> {
    if json.is_empty() || json.len() > MAX_HEADER_LEN {
        return Err(MimintError::Config(
            "friends protocol error: header length out of range".to_string(),
        ));
    }
    if payload.len() > MAX_FRAME_PAYLOAD {
        return Err(MimintError::Config(
            "friends protocol error: payload exceeds the limit".to_string(),
        ));
    }
    let len = u32::try_from(json.len())
        .map_err(|_| MimintError::Config("friends protocol error: header too large".to_string()))?;
    writer
        .write_all(&len.to_be_bytes())
        .map_err(|err| MimintError::Config(format!("friends protocol write error: {err}")))?;
    writer
        .write_all(json)
        .map_err(|err| MimintError::Config(format!("friends protocol write error: {err}")))?;
    writer
        .write_all(payload)
        .map_err(|err| MimintError::Config(format!("friends protocol write error: {err}")))?;
    writer
        .flush()
        .map_err(|err| MimintError::Config(format!("friends protocol write error: {err}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn kind_allow_list_is_closed_and_complete() {
        // The 24 design kinds + the 2 documented driver kinds.
        assert_eq!(MessageKind::all().len(), 26);
        assert_eq!(
            MessageKind::all()
                .iter()
                .filter(|k| k.is_design_kind())
                .count(),
            24
        );
        for kind in MessageKind::all() {
            assert_eq!(MessageKind::parse(kind.as_str()), Some(*kind));
        }
        assert_eq!(MessageKind::parse("sync"), None);
        assert_eq!(MessageKind::parse(""), None);
        assert_eq!(MessageKind::parse("HELLO"), None);
        // Reason codes are NOT kinds (§2.5 note).
        for code in [
            "lease_busy",
            "forbidden_role",
            "unknown_method",
            "sas_mismatch",
        ] {
            assert_eq!(MessageKind::parse(code), None, "{code} must not be a kind");
        }
    }

    #[test]
    fn header_canonical_order_is_fixed() {
        let header = EncryptedHeader {
            v: 2,
            kind: "sync_push".to_string(),
            from: "00112233445566778899aabbccddeeff".to_string(),
            seq: 41,
            nonce: "412d3e420000000000000000".to_string(),
            payload_len: 1832,
        };
        let bytes = header.canonical_bytes();
        let text = String::from_utf8(bytes).unwrap();
        assert!(
            text.starts_with(r#"{"v":2,"kind":"sync_push","from":"#),
            "{text}"
        );
        // Re-parsing and re-encoding is stable (AAD safety).
        let parsed: EncryptedHeader = serde_json::from_slice(text.as_bytes()).unwrap();
        assert_eq!(parsed.canonical_bytes(), header.canonical_bytes());
        // Unknown fields are rejected.
        let bad =
            r#"{"v":2,"kind":"ping","from":"00","seq":1,"nonce":"00","payload_len":0,"extra":1}"#;
        assert!(serde_json::from_str::<EncryptedHeader>(bad).is_err());
    }

    #[test]
    fn frame_round_trip_and_caps() {
        let mut buf = Vec::new();
        write_frame(
            &mut buf,
            br#"{"v":2,"kind":"ping","payload_len":3}"#,
            b"abc",
        )
        .unwrap();
        let mut cursor = Cursor::new(buf);
        let frame = read_frame(&mut cursor).unwrap();
        assert_eq!(frame.payload, b"abc");
        assert_eq!(kind_of(&frame.json).unwrap(), MessageKind::Ping);
        assert_eq!(version_of(&frame.json).unwrap(), 2);
        // Absent payload_len means 0.
        assert_eq!(payload_len_of(br#"{"v":2,"kind":"hello"}"#).unwrap(), 0);
        // Oversize header is refused on write.
        let big = vec![b'x'; MAX_HEADER_LEN + 1];
        assert!(write_frame(&mut Vec::new(), &big, &[]).is_err());
    }
}
