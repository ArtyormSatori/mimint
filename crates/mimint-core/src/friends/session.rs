//! Friends session: the 5-step handshake, AEAD channel, and
//! rekey (Phase 7, §2.4).
//!
//! [`Session`] owns a TCP stream and runs the handshake. The
//! cryptographic epoch state machine lives in [`SessionCrypto`]
//! (no sockets), so rekey interleavings — in-flight old-key
//! messages, old-key rejection after the switch — can be driven
//! precisely in tests.
//!
//! Handshake order (both sides, initiator first):
//!
//! 1. HELLO / HELLO (plaintext: device id, identity public key,
//!    ephemeral public key, nonce).
//! 2. Pin check BEFORE signing: the peer must be in the registry
//!    with the exact pinned key, and not revoked. Failure closes
//!    the connection WITHOUT sending AUTH.
//! 3. AUTH / AUTH (ECDSA signature over the transcript hash,
//!    verified against the PINNED key, never the received one).
//! 4. Session keys from ephemeral ECDH + HKDF (§2.4 step 4).
//! 5. SESSION_CONFIRM / SESSION_CONFIRM (first encrypted
//!    message each way; the claimed trust must match the trust
//!    this side granted the peer — a peer cannot claim a higher
//!    grant than it was given).
//!
//! Failure behaviour follows the §2.7 table: generic
//! `handshake_error` before AUTH verifies (except the version
//! code), silent close afterwards, audit with the specific kind.

use crate::audit::AuditLog;
use crate::error::MimintError;
use crate::friends::crypto::{
    agree_and_derive, device_id_from_public_key, fresh_nonce16, generate_ephemeral,
    identity_verify, make_nonce, transcript_hash, Direction, SessionKey, HELLO_NONCE_LEN,
    NONCE_LEN, PUBKEY_LEN,
};
use crate::friends::identity::Identity;
use crate::friends::registry::{DeviceRegistry, Trust};
use crate::friends::util::{ct_equal, hex_decode, hex_encode, Clock};
use crate::friends::wire::{
    kind_of, read_frame, version_of, write_frame, EncryptedHeader, MessageKind, PROTOCOL_VERSION,
};
use ring::agreement::EphemeralPrivateKey;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

/// Session configuration. Production defaults follow §2.4/§5.4;
/// tests override the rekey thresholds and timeouts.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// Rekey after this many sent messages (default 1_000_000).
    pub rekey_after_messages: u64,
    /// Rekey after this many sent ciphertext bytes (default 1 GiB).
    pub rekey_after_bytes: u64,
    /// Rekey after the session lives this long (default 24h).
    pub rekey_after_ms: u64,
    /// How long to wait for `rekey_ack` (default 10s).
    pub rekey_timeout_ms: u64,
    /// Whole-handshake timeout (default 10s).
    pub handshake_timeout_ms: u64,
    /// Idle session timeout (default 10 minutes, §5.4).
    pub idle_timeout_ms: u64,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            rekey_after_messages: 1_000_000,
            rekey_after_bytes: 1 << 30,
            rekey_after_ms: 24 * 60 * 60 * 1000,
            rekey_timeout_ms: 10_000,
            handshake_timeout_ms: 10_000,
            idle_timeout_ms: 10 * 60 * 1000,
        }
    }
}

/// How long the old receive key is kept after switching to a new
/// key: until the first new-key message, or 10 seconds (§2.4
/// step 6, R1).
pub const OLD_KEY_KEEP_MS: u64 = 10_000;

/// Rekey flood guard (F7-10): at most one peer-initiated
/// rekey per 5 seconds, at most 6 per rolling minute. A
/// legitimate peer rekeys at most once per 1M messages /
/// 1 GiB / 24h, so these limits never bind honest traffic;
/// excess requests are dropped + audited, never processed,
/// and the session stays open (fail-closed for simultaneous
/// rekey is unchanged).
pub const REKEY_MIN_INTERVAL_MS: u64 = 5_000;
pub const REKEY_MAX_PER_MINUTE: usize = 6;
const REKEY_RATE_WINDOW_MS: u64 = 60_000;

/// A HELLO message (plaintext, §2.4).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelloMessage {
    pub v: u32,
    pub kind: String,
    pub device_id: String,
    /// Identity public key, 130 hex.
    pub identity_pub: String,
    /// Ephemeral ECDH public key, 130 hex.
    pub ecdh_pub: String,
    /// 16-byte nonce, hex.
    pub nonce: String,
}

/// An AUTH message (plaintext, §2.4).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthMessage {
    pub v: u32,
    pub kind: String,
    pub device_id: String,
    /// ECDSA signature over the transcript hash, hex.
    pub sig: String,
}

/// A handshake error (plaintext, §2.7): one generic message, no
/// detail (except the version code).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandshakeErrorMessage {
    pub v: u32,
    pub kind: String,
    pub code: String,
}

/// Plaintext of `session_confirm` (§2.4 step 5).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionConfirm {
    pub device_id: String,
    pub paired_trust: String,
    pub proto: u32,
}

/// Plaintext of `rekey` / `rekey_ack` (§2.4 step 6).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RekeyMessage {
    /// Fresh ephemeral ECDH public key, 130 hex.
    pub ecdh_pub: String,
    /// Fresh 16-byte nonce, hex.
    pub nonce: String,
}

/// Errors from the crypto epoch machine. Kept distinct so the
/// session can audit the right §2.7 kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CryptoError {
    /// Sequence number or nonce counter mismatch (F9).
    Sequence,
    /// AEAD open failed (F10).
    Aead,
    /// A rekey must run before the next message.
    RekeyRequired,
    /// Rekey state machine violation.
    RekeyState,
    /// Malformed header/plaintext.
    Malformed,
}

impl std::fmt::Display for CryptoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CryptoError::Sequence => write!(f, "sequence mismatch"),
            CryptoError::Aead => write!(f, "AEAD open failed"),
            CryptoError::RekeyRequired => write!(f, "rekey required"),
            CryptoError::RekeyState => write!(f, "rekey state error"),
            CryptoError::Malformed => write!(f, "malformed message"),
        }
    }
}

/// One sealed message: header + ciphertext.
#[derive(Debug, Clone)]
pub struct SealedMessage {
    pub header: EncryptedHeader,
    pub ciphertext: Vec<u8>,
}

struct OldEpoch {
    key: SessionKey,
    seq_expected: u64,
    counter_expected: u64,
    deadline_ms: u64,
}

struct PendingRekey {
    private: EphemeralPrivateKey,
    my_nonce: [u8; HELLO_NONCE_LEN],
    started_ms: u64,
}

/// The AEAD epoch state machine: directional keys, sequence
/// numbers, nonce counters, and rekey transitions. No I/O.
pub struct SessionCrypto {
    local_device_id: String,
    peer_device_id: String,
    local_is_a: bool,
    send_key: SessionKey,
    recv_key: SessionKey,
    old_recv: Option<OldEpoch>,
    send_seq: u64,
    send_counter: u64,
    recv_seq: u64,
    recv_counter: u64,
    rekey_n: u64,
    pending_rekey: Option<PendingRekey>,
    sent_messages: u64,
    sent_bytes: u64,
    started_ms: u64,
    clock: Clock,
    config: SessionConfig,
}

impl SessionCrypto {
    /// Build from derived directional keys. `key_ab` encrypts
    /// A->B, `key_ba` encrypts B->A; each side picks its send
    /// key by direction.
    pub fn new(
        local_device_id: &str,
        peer_device_id: &str,
        key_ab: SessionKey,
        key_ba: SessionKey,
        clock: Clock,
        config: SessionConfig,
    ) -> Self {
        let local_is_a = local_device_id.as_bytes() < peer_device_id.as_bytes();
        let (send_key, recv_key) = if local_is_a {
            (key_ab, key_ba)
        } else {
            (key_ba, key_ab)
        };
        let started_ms = clock.now_ms();
        Self {
            local_device_id: local_device_id.to_string(),
            peer_device_id: peer_device_id.to_string(),
            local_is_a,
            send_key,
            recv_key,
            old_recv: None,
            send_seq: 1,
            send_counter: 0,
            recv_seq: 1,
            recv_counter: 0,
            rekey_n: 0,
            pending_rekey: None,
            sent_messages: 0,
            sent_bytes: 0,
            started_ms,
            clock,
            config,
        }
    }

    /// The number of completed rekeys in this session.
    pub fn rekey_count(&self) -> u64 {
        self.rekey_n
    }

    /// Next send sequence number (tests/diagnostics).
    pub fn next_send_seq(&self) -> u64 {
        self.send_seq
    }

    /// Whether a rekey is due by message/byte/age thresholds.
    pub fn rekey_due(&self) -> bool {
        self.pending_rekey.is_none()
            && (self.sent_messages >= self.config.rekey_after_messages
                || self.sent_bytes >= self.config.rekey_after_bytes
                || self.clock.now_ms().saturating_sub(self.started_ms)
                    >= self.config.rekey_after_ms
                || self.send_counter >= (1u64 << 63))
    }

    fn drop_expired_old_key(&mut self) {
        if let Some(old) = &self.old_recv {
            if self.clock.now_ms() >= old.deadline_ms {
                let mut old = self.old_recv.take().expect("old epoch");
                old.key.zeroize();
            }
        }
    }

    /// Seal one message. Returns [`CryptoError::RekeyRequired`]
    /// when a rekey must run first; the caller runs the rekey
    /// exchange and retries.
    pub fn seal(
        &mut self,
        kind: MessageKind,
        plaintext: &[u8],
    ) -> Result<SealedMessage, CryptoError> {
        self.seal_inner(kind, plaintext, true)
    }

    /// Seal a rekey-protocol message (the responder's
    /// RekeyAck): part of the rekey exchange itself, so the
    /// rekey-due gate must not block it. The responder is
    /// often due at the same moment as the initiator (same
    /// age threshold, same start); blocking the ack would
    /// deadlock the rekey — the initiator times out and both
    /// sides fail (m4).
    pub fn seal_rekey(
        &mut self,
        kind: MessageKind,
        plaintext: &[u8],
    ) -> Result<SealedMessage, CryptoError> {
        self.seal_inner(kind, plaintext, false)
    }

    fn seal_inner(
        &mut self,
        kind: MessageKind,
        plaintext: &[u8],
        check_due: bool,
    ) -> Result<SealedMessage, CryptoError> {
        self.drop_expired_old_key();
        if check_due && self.rekey_due() {
            return Err(CryptoError::RekeyRequired);
        }
        let direction = Direction::for_sender(self.local_is_a);
        let nonce = make_nonce(direction, self.send_counter);
        let header = EncryptedHeader {
            v: PROTOCOL_VERSION,
            kind: kind.as_str().to_string(),
            from: self.local_device_id.clone(),
            seq: self.send_seq,
            nonce: hex_encode(&nonce),
            payload_len: 0, // filled after sealing (tag adds 16)
        };
        // AAD is the header with the final payload length.
        let mut header = header;
        header.payload_len = plaintext.len() + 16;
        let aad = header.canonical_bytes();
        let ciphertext = self.send_key.seal(&nonce, &aad, plaintext);
        debug_assert_eq!(ciphertext.len(), header.payload_len);
        self.send_seq += 1;
        self.send_counter += 1;
        self.sent_messages += 1;
        self.sent_bytes += ciphertext.len() as u64;
        Ok(SealedMessage { header, ciphertext })
    }

    /// Open one received message. Tries the current key first,
    /// then the kept old key (rekey transition window). The
    /// first message that opens with the new key drops the old
    /// key at once (R1).
    pub fn open(
        &mut self,
        header: &EncryptedHeader,
        ciphertext: &[u8],
    ) -> Result<Vec<u8>, CryptoError> {
        self.drop_expired_old_key();
        if header.v != PROTOCOL_VERSION || header.from != self.peer_device_id {
            return Err(CryptoError::Malformed);
        }
        if header.payload_len != ciphertext.len() {
            return Err(CryptoError::Malformed);
        }
        let nonce_bytes = hex_decode(&header.nonce).ok_or(CryptoError::Malformed)?;
        if nonce_bytes.len() != NONCE_LEN {
            return Err(CryptoError::Malformed);
        }
        let mut nonce = [0u8; NONCE_LEN];
        nonce.copy_from_slice(&nonce_bytes);
        let direction = Direction::for_receiver(self.local_is_a);
        let aad = header.canonical_bytes();

        // Current epoch candidate?
        let current_nonce = make_nonce(direction, self.recv_counter);
        if header.seq == self.recv_seq && nonce == current_nonce {
            match self.recv_key.open(&nonce, &aad, ciphertext) {
                Some(plain) => {
                    self.recv_seq += 1;
                    self.recv_counter += 1;
                    // First new-key message ends the transition.
                    if let Some(mut old) = self.old_recv.take() {
                        old.key.zeroize();
                    }
                    return Ok(plain);
                }
                None => return Err(CryptoError::Aead),
            }
        }
        // Old epoch candidate (rekey transition window)?
        if let Some(old) = &mut self.old_recv {
            let old_nonce = make_nonce(direction, old.counter_expected);
            if header.seq == old.seq_expected && nonce == old_nonce {
                return match old.key.open(&nonce, &aad, ciphertext) {
                    Some(plain) => {
                        old.seq_expected += 1;
                        old.counter_expected += 1;
                        Ok(plain)
                    }
                    None => Err(CryptoError::Aead),
                };
            }
        }
        Err(CryptoError::Sequence)
    }

    /// Begin a rekey as the initiating side: generate a fresh
    /// ephemeral keypair + nonce and return the `rekey`
    /// plaintext (to be sealed with the OLD key by the caller).
    pub fn begin_rekey(&mut self) -> Result<RekeyMessage, CryptoError> {
        if self.pending_rekey.is_some() {
            return Err(CryptoError::RekeyState);
        }
        let (private, public) = generate_ephemeral();
        let my_nonce = fresh_nonce16();
        self.pending_rekey = Some(PendingRekey {
            private,
            my_nonce,
            started_ms: self.clock.now_ms(),
        });
        Ok(RekeyMessage {
            ecdh_pub: hex_encode(&public),
            nonce: hex_encode(&my_nonce),
        })
    }

    /// Whether a rekey initiated by this side is awaiting its ack.
    pub fn awaiting_rekey_ack(&self) -> bool {
        self.pending_rekey.is_some()
    }

    /// Milliseconds left to wait for `rekey_ack` (0 = expired).
    pub fn rekey_wait_remaining_ms(&self) -> u64 {
        match &self.pending_rekey {
            None => 0,
            Some(pending) => self
                .config
                .rekey_timeout_ms
                .saturating_sub(self.clock.now_ms().saturating_sub(pending.started_ms)),
        }
    }

    /// Abort a pending rekey (timeout / invalid ack): zeroize
    /// the pending ephemeral state. The session must close.
    pub fn abort_pending_rekey(&mut self) {
        self.pending_rekey = None;
    }

    /// Complete a rekey as the initiating side, given the peer's
    /// `rekey_ack` contents. Derives the new keys, switches the
    /// send key, and keeps the old receive key for the
    /// transition window.
    pub fn complete_rekey_as_initiator(&mut self, ack: &RekeyMessage) -> Result<(), CryptoError> {
        let pending = self.pending_rekey.take().ok_or(CryptoError::RekeyState)?;
        let peer_pub = parse_pubkey_hex(&ack.ecdh_pub)?;
        let peer_nonce = parse_nonce16_hex(&ack.nonce)?;
        let (nonce_a, nonce_b) = order_nonces(self.local_is_a, &pending.my_nonce, &peer_nonce);
        let (key_ab, key_ba) = agree_and_derive(
            pending.private,
            &peer_pub,
            &nonce_a,
            &nonce_b,
            Some(self.rekey_n + 1),
        )
        .map_err(|_| CryptoError::Malformed)?;
        self.switch_keys(key_ab, key_ba);
        Ok(())
    }

    /// Handle an incoming `rekey` as the responding side: derive
    /// the new keys at once and return the `rekey_ack`
    /// plaintext. The caller seals the ack with the OLD key,
    /// sends it, and then calls [`Self::switch_after_ack_sent`].
    pub fn prepare_rekey_response(
        &mut self,
        request: &RekeyMessage,
    ) -> Result<(RekeyMessage, (SessionKey, SessionKey)), CryptoError> {
        if self.pending_rekey.is_some() {
            // Simultaneous rekey from both sides: fail closed.
            return Err(CryptoError::RekeyState);
        }
        let peer_pub = parse_pubkey_hex(&request.ecdh_pub)?;
        let peer_nonce = parse_nonce16_hex(&request.nonce)?;
        let (private, public) = generate_ephemeral();
        let my_nonce = fresh_nonce16();
        let (nonce_a, nonce_b) = order_nonces(self.local_is_a, &my_nonce, &peer_nonce);
        let keys = agree_and_derive(
            private,
            &peer_pub,
            &nonce_a,
            &nonce_b,
            Some(self.rekey_n + 1),
        )
        .map_err(|_| CryptoError::Malformed)?;
        Ok((
            RekeyMessage {
                ecdh_pub: hex_encode(&public),
                nonce: hex_encode(&my_nonce),
            },
            keys,
        ))
    }

    /// Switch to new keys after the `rekey_ack` was sent with
    /// the old key (responder side).
    pub fn switch_after_ack_sent(&mut self, keys: (SessionKey, SessionKey)) {
        self.switch_keys(keys.0, keys.1);
    }

    fn switch_keys(&mut self, key_ab: SessionKey, key_ba: SessionKey) {
        let (new_send, new_recv) = if self.local_is_a {
            (key_ab, key_ba)
        } else {
            (key_ba, key_ab)
        };
        let old_recv_key = std::mem::replace(&mut self.recv_key, new_recv);
        let deadline = self.clock.now_ms() + OLD_KEY_KEEP_MS;
        self.old_recv = Some(OldEpoch {
            key: old_recv_key,
            seq_expected: self.recv_seq,
            counter_expected: self.recv_counter,
            deadline_ms: deadline,
        });
        self.send_key = new_send;
        self.send_seq = 1;
        self.send_counter = 0;
        self.recv_seq = 1;
        self.recv_counter = 0;
        self.rekey_n += 1;
        self.sent_messages = 0;
        self.sent_bytes = 0;
        self.started_ms = self.clock.now_ms();
    }

    /// Zeroize all key material now.
    pub fn zeroize(&mut self) {
        self.send_key.zeroize();
        self.recv_key.zeroize();
        if let Some(mut old) = self.old_recv.take() {
            old.key.zeroize();
        }
        self.pending_rekey = None;
    }
}

impl Drop for SessionCrypto {
    fn drop(&mut self) {
        self.zeroize();
    }
}

fn order_nonces(
    local_is_a: bool,
    my_nonce: &[u8; HELLO_NONCE_LEN],
    peer_nonce: &[u8; HELLO_NONCE_LEN],
) -> ([u8; HELLO_NONCE_LEN], [u8; HELLO_NONCE_LEN]) {
    if local_is_a {
        (*my_nonce, *peer_nonce)
    } else {
        (*peer_nonce, *my_nonce)
    }
}

fn parse_pubkey_hex(text: &str) -> Result<[u8; PUBKEY_LEN], CryptoError> {
    let bytes = hex_decode(text).ok_or(CryptoError::Malformed)?;
    if bytes.len() != PUBKEY_LEN || bytes.first() != Some(&0x04) {
        return Err(CryptoError::Malformed);
    }
    let mut out = [0u8; PUBKEY_LEN];
    out.copy_from_slice(&bytes);
    Ok(out)
}

fn parse_nonce16_hex(text: &str) -> Result<[u8; HELLO_NONCE_LEN], CryptoError> {
    let bytes = hex_decode(text).ok_or(CryptoError::Malformed)?;
    if bytes.len() != HELLO_NONCE_LEN {
        return Err(CryptoError::Malformed);
    }
    let mut out = [0u8; HELLO_NONCE_LEN];
    out.copy_from_slice(&bytes);
    Ok(out)
}

fn parse_sig_hex(text: &str) -> Result<Vec<u8>, CryptoError> {
    hex_decode(text).ok_or(CryptoError::Malformed)
}

/// TCP role in a session: who opened the connection. Used ONLY
/// to order handshake messages and transcript bytes — never in
/// any crypto string (R3: distinct from the A/B convention).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionRole {
    Initiator,
    Listener,
}

/// An established friends session over one TCP connection.
/// Single-threaded: one thread owns a session and calls
/// [`Self::send`] / [`Self::recv`] in turn.
pub struct Session {
    stream: TcpStream,
    crypto: SessionCrypto,
    role: SessionRole,
    peer_device_id: String,
    peer_trust: Trust,
    local_device_id: String,
    confirm_received: bool,
    confirm_sent: bool,
    pending: VecDeque<(MessageKind, Vec<u8>)>,
    /// Timestamps (clock ms) of peer-initiated rekeys this
    /// session accepted, for the F7-10 rate limit.
    rekey_request_times: VecDeque<u64>,
    clock: Clock,
    config: SessionConfig,
    audit: Option<Arc<AuditLog>>,
    closed: bool,
}

fn audit_event(audit: &Option<Arc<AuditLog>>, kind: &str, detail: serde_json::Value) {
    if let Some(log) = audit {
        let _ = log.log("friends-handshake", kind, Some("friends"), None, detail);
    }
}

fn send_handshake_error(stream: &mut TcpStream, code: &str) {
    let message = HandshakeErrorMessage {
        v: PROTOCOL_VERSION,
        kind: MessageKind::HandshakeError.as_str().to_string(),
        code: code.to_string(),
    };
    let json = serde_json::to_vec(&message).unwrap_or_default();
    let _ = write_frame(stream, &json, &[]);
}

fn set_timeouts(stream: &TcpStream, read_ms: u64, write_ms: u64) -> Result<(), MimintError> {
    stream
        .set_read_timeout(Some(Duration::from_millis(read_ms)))
        .map_err(|err| MimintError::Config(format!("cannot set read timeout: {err}")))?;
    stream
        .set_write_timeout(Some(Duration::from_millis(write_ms)))
        .map_err(|err| MimintError::Config(format!("cannot set write timeout: {err}")))?;
    Ok(())
}

/// Read one frame and parse it as a HELLO. Returns the raw JSON
/// bytes (for the transcript) and the parsed message.
fn read_hello(stream: &mut TcpStream) -> Result<(Vec<u8>, HelloMessage), MimintError> {
    let frame = read_frame(stream)?;
    if version_of(&frame.json)? != PROTOCOL_VERSION {
        return Err(MimintError::Config("version_mismatch".to_string()));
    }
    if kind_of(&frame.json)? != MessageKind::Hello {
        return Err(MimintError::Config("handshake_out_of_order".to_string()));
    }
    if !frame.payload.is_empty() {
        return Err(MimintError::Config("handshake_out_of_order".to_string()));
    }
    let hello: HelloMessage = serde_json::from_slice(&frame.json)
        .map_err(|err| MimintError::Config(format!("friends protocol error: bad HELLO: {err}")))?;
    if hello.kind != "hello" || hello.v != PROTOCOL_VERSION {
        return Err(MimintError::Config("handshake_out_of_order".to_string()));
    }
    Ok((frame.json, hello))
}

fn read_auth(stream: &mut TcpStream) -> Result<AuthMessage, MimintError> {
    let frame = read_frame(stream)?;
    if version_of(&frame.json)? != PROTOCOL_VERSION {
        return Err(MimintError::Config("version_mismatch".to_string()));
    }
    if kind_of(&frame.json)? != MessageKind::Auth {
        return Err(MimintError::Config("handshake_out_of_order".to_string()));
    }
    if !frame.payload.is_empty() {
        return Err(MimintError::Config("handshake_out_of_order".to_string()));
    }
    let auth: AuthMessage = serde_json::from_slice(&frame.json)
        .map_err(|err| MimintError::Config(format!("friends protocol error: bad AUTH: {err}")))?;
    if auth.kind != "auth" || auth.v != PROTOCOL_VERSION {
        return Err(MimintError::Config("handshake_out_of_order".to_string()));
    }
    Ok(auth)
}

/// Check the pin BEFORE signing (§2.4 step 1). Returns the peer
/// entry on success; on failure the caller sends the generic
/// error and closes without signing.
fn check_pin(
    registry: &DeviceRegistry,
    hello: &HelloMessage,
) -> Result<crate::friends::registry::PeerEntry, &'static str> {
    let entry = match registry.get_peer(&hello.device_id) {
        Ok(Some(entry)) => entry,
        _ => return Err("unknown_peer"),
    };
    if entry.revoked {
        return Err("revoked_peer_attempt");
    }
    let received = hex_decode(&hello.identity_pub).unwrap_or_default();
    let pinned = hex_decode(&entry.identity_pub_hex).unwrap_or_default();
    if !ct_equal(&received, &pinned) {
        return Err("pin_mismatch");
    }
    // The device id must be the hash of the pinned public key.
    if device_id_from_public_key(&pinned) != hello.device_id {
        return Err("pin_mismatch");
    }
    Ok(entry)
}

fn build_hello(
    identity: &Identity,
    ecdh_pub: &[u8; PUBKEY_LEN],
    nonce: &[u8; HELLO_NONCE_LEN],
) -> Vec<u8> {
    let hello = HelloMessage {
        v: PROTOCOL_VERSION,
        kind: MessageKind::Hello.as_str().to_string(),
        device_id: identity.device_id().to_string(),
        identity_pub: identity.public_key_hex(),
        ecdh_pub: hex_encode(ecdh_pub),
        nonce: hex_encode(nonce),
    };
    serde_json::to_vec(&hello).expect("HELLO serialization cannot fail")
}

fn derive_session_keys(
    mine: EphemeralPrivateKey,
    peer_hello: &HelloMessage,
    my_nonce: &[u8; HELLO_NONCE_LEN],
    local_is_a: bool,
) -> Result<(SessionKey, SessionKey), MimintError> {
    let peer_pub = parse_pubkey_hex(&peer_hello.ecdh_pub)
        .map_err(|_| MimintError::Config("invalid ECDH public key in HELLO".to_string()))?;
    let peer_nonce = parse_nonce16_hex(&peer_hello.nonce)
        .map_err(|_| MimintError::Config("invalid nonce in HELLO".to_string()))?;
    let (nonce_a, nonce_b) = order_nonces(local_is_a, my_nonce, &peer_nonce);
    agree_and_derive(mine, &peer_pub, &nonce_a, &nonce_b, None)
}

/// Run the handshake as the initiator (the side that opened the
/// TCP connection).
pub fn handshake_initiator(
    mut stream: TcpStream,
    identity: &Identity,
    registry: &DeviceRegistry,
    config: &SessionConfig,
    clock: &Clock,
    audit: Option<Arc<AuditLog>>,
) -> Result<Session, MimintError> {
    set_timeouts(
        &stream,
        config.handshake_timeout_ms,
        config.handshake_timeout_ms,
    )?;
    let (ephemeral, ecdh_pub) = generate_ephemeral();
    let my_nonce = fresh_nonce16();
    let my_hello_json = build_hello(identity, &ecdh_pub, &my_nonce);
    write_frame(&mut stream, &my_hello_json, &[])?;

    let (peer_hello_json, peer_hello) = match read_hello(&mut stream) {
        Ok(ok) => ok,
        Err(err) => {
            let code = if err.to_string().contains("version_mismatch") {
                audit_event(
                    &audit,
                    "handshake_version_mismatch",
                    serde_json::json!({"role": "initiator"}),
                );
                "version_mismatch"
            } else {
                audit_event(
                    &audit,
                    "handshake_out_of_order",
                    serde_json::json!({"role": "initiator"}),
                );
                "handshake_failed"
            };
            send_handshake_error(&mut stream, code);
            return Err(err);
        }
    };
    // Pin check BEFORE signing: no AUTH goes to a stranger.
    let peer_entry = match check_pin(registry, &peer_hello) {
        Ok(entry) => entry,
        Err(audit_kind) => {
            audit_event(
                &audit,
                audit_kind,
                serde_json::json!({"peer_device_id": peer_hello.device_id}),
            );
            send_handshake_error(&mut stream, "handshake_failed");
            return Err(MimintError::Config(format!(
                "friends handshake rejected: {audit_kind}"
            )));
        }
    };
    let transcript = transcript_hash(PROTOCOL_VERSION, &my_hello_json, &peer_hello_json);
    let signature = identity.sign(&transcript)?;
    let auth = AuthMessage {
        v: PROTOCOL_VERSION,
        kind: MessageKind::Auth.as_str().to_string(),
        device_id: identity.device_id().to_string(),
        sig: hex_encode(&signature),
    };
    write_frame(&mut stream, &serde_json::to_vec(&auth).unwrap(), &[])?;

    let peer_auth = match read_auth(&mut stream) {
        Ok(auth) => auth,
        Err(err) => {
            send_handshake_error(&mut stream, "handshake_failed");
            audit_event(&audit, "handshake_out_of_order", serde_json::json!({}));
            return Err(err);
        }
    };
    let pinned_pub = hex_decode(&peer_entry.identity_pub_hex).unwrap_or_default();
    let peer_sig = parse_sig_hex(&peer_auth.sig).unwrap_or_default();
    if peer_auth.device_id != peer_hello.device_id
        || !identity_verify(&pinned_pub, &transcript, &peer_sig)
    {
        send_handshake_error(&mut stream, "handshake_failed");
        audit_event(
            &audit,
            "auth_signature_invalid",
            serde_json::json!({"peer_device_id": peer_hello.device_id}),
        );
        return Err(MimintError::Config(
            "friends handshake rejected: auth_signature_invalid".to_string(),
        ));
    }
    let local_is_a = identity.device_id().as_bytes() < peer_hello.device_id.as_bytes();
    let (key_ab, key_ba) = derive_session_keys(ephemeral, &peer_hello, &my_nonce, local_is_a)?;
    let crypto = SessionCrypto::new(
        identity.device_id(),
        &peer_hello.device_id,
        key_ab,
        key_ba,
        clock.clone(),
        config.clone(),
    );
    let mut session = Session {
        stream,
        crypto,
        role: SessionRole::Initiator,
        peer_device_id: peer_hello.device_id.clone(),
        peer_trust: peer_entry.trust,
        local_device_id: identity.device_id().to_string(),
        confirm_received: false,
        confirm_sent: false,
        pending: VecDeque::new(),
        rekey_request_times: VecDeque::new(),
        clock: clock.clone(),
        config: config.clone(),
        audit: audit.clone(),
        closed: false,
    };
    finish_handshake(&mut session, identity, registry, &peer_entry, true)?;
    Ok(session)
}

/// Run the handshake as the listener (the side that accepted
/// the TCP connection).
pub fn handshake_listener(
    mut stream: TcpStream,
    identity: &Identity,
    registry: &DeviceRegistry,
    config: &SessionConfig,
    clock: &Clock,
    audit: Option<Arc<AuditLog>>,
) -> Result<Session, MimintError> {
    set_timeouts(
        &stream,
        config.handshake_timeout_ms,
        config.handshake_timeout_ms,
    )?;
    let (peer_hello_json, peer_hello) = match read_hello(&mut stream) {
        Ok(ok) => ok,
        Err(err) => {
            let code = if err.to_string().contains("version_mismatch") {
                audit_event(
                    &audit,
                    "handshake_version_mismatch",
                    serde_json::json!({"role": "listener"}),
                );
                "version_mismatch"
            } else {
                audit_event(
                    &audit,
                    "handshake_out_of_order",
                    serde_json::json!({"role": "listener"}),
                );
                "handshake_failed"
            };
            send_handshake_error(&mut stream, code);
            return Err(err);
        }
    };
    handshake_listener_after_hello(
        stream,
        peer_hello_json,
        peer_hello,
        identity,
        registry,
        config,
        clock,
        audit,
    )
}
#[allow(clippy::too_many_arguments)]
fn handshake_listener_after_hello(
    mut stream: TcpStream,
    peer_hello_json: Vec<u8>,
    peer_hello: HelloMessage,
    identity: &Identity,
    registry: &DeviceRegistry,
    config: &SessionConfig,
    clock: &Clock,
    audit: Option<Arc<AuditLog>>,
) -> Result<Session, MimintError> {
    // Pin check before sending anything but the generic error:
    // a stranger gets no HELLO and no AUTH from this side.
    let peer_entry = match check_pin(registry, &peer_hello) {
        Ok(entry) => entry,
        Err(audit_kind) => {
            audit_event(
                &audit,
                audit_kind,
                serde_json::json!({"peer_device_id": peer_hello.device_id}),
            );
            send_handshake_error(&mut stream, "handshake_failed");
            return Err(MimintError::Config(format!(
                "friends handshake rejected: {audit_kind}"
            )));
        }
    };
    let (ephemeral, ecdh_pub) = generate_ephemeral();
    let my_nonce = fresh_nonce16();
    let my_hello_json = build_hello(identity, &ecdh_pub, &my_nonce);
    write_frame(&mut stream, &my_hello_json, &[])?;

    // Transcript order is fixed: initiator HELLO first.
    let transcript = transcript_hash(PROTOCOL_VERSION, &peer_hello_json, &my_hello_json);
    let peer_auth = match read_auth(&mut stream) {
        Ok(auth) => auth,
        Err(err) => {
            send_handshake_error(&mut stream, "handshake_failed");
            audit_event(&audit, "handshake_out_of_order", serde_json::json!({}));
            return Err(err);
        }
    };
    let pinned_pub = hex_decode(&peer_entry.identity_pub_hex).unwrap_or_default();
    let peer_sig = parse_sig_hex(&peer_auth.sig).unwrap_or_default();
    if peer_auth.device_id != peer_hello.device_id
        || !identity_verify(&pinned_pub, &transcript, &peer_sig)
    {
        send_handshake_error(&mut stream, "handshake_failed");
        audit_event(
            &audit,
            "auth_signature_invalid",
            serde_json::json!({"peer_device_id": peer_hello.device_id}),
        );
        return Err(MimintError::Config(
            "friends handshake rejected: auth_signature_invalid".to_string(),
        ));
    }
    let signature = identity.sign(&transcript)?;
    let auth = AuthMessage {
        v: PROTOCOL_VERSION,
        kind: MessageKind::Auth.as_str().to_string(),
        device_id: identity.device_id().to_string(),
        sig: hex_encode(&signature),
    };
    write_frame(&mut stream, &serde_json::to_vec(&auth).unwrap(), &[])?;

    let local_is_a = identity.device_id().as_bytes() < peer_hello.device_id.as_bytes();
    let (key_ab, key_ba) = derive_session_keys(ephemeral, &peer_hello, &my_nonce, local_is_a)?;
    let crypto = SessionCrypto::new(
        identity.device_id(),
        &peer_hello.device_id,
        key_ab,
        key_ba,
        clock.clone(),
        config.clone(),
    );
    let mut session = Session {
        stream,
        crypto,
        role: SessionRole::Listener,
        peer_device_id: peer_hello.device_id.clone(),
        peer_trust: peer_entry.trust,
        local_device_id: identity.device_id().to_string(),
        confirm_received: false,
        confirm_sent: false,
        pending: VecDeque::new(),
        rekey_request_times: VecDeque::new(),
        clock: clock.clone(),
        config: config.clone(),
        audit: audit.clone(),
        closed: false,
    };
    finish_handshake(&mut session, identity, registry, &peer_entry, false)?;
    Ok(session)
}

/// Parse an already-read HELLO frame (the hub dispatcher
/// reads the first frame to route it). Validates version,
/// kind, and the empty payload exactly like `read_hello`.
fn parse_hello_json(json: &[u8]) -> Result<(Vec<u8>, HelloMessage), MimintError> {
    if version_of(json)? != PROTOCOL_VERSION {
        return Err(MimintError::Config("version_mismatch".to_string()));
    }
    if kind_of(json)? != MessageKind::Hello {
        return Err(MimintError::Config("handshake_out_of_order".to_string()));
    }
    let hello: HelloMessage = serde_json::from_slice(json)
        .map_err(|err| MimintError::Config(format!("friends protocol error: bad HELLO: {err}")))?;
    if hello.kind != "hello" || hello.v != PROTOCOL_VERSION {
        return Err(MimintError::Config("handshake_out_of_order".to_string()));
    }
    Ok((json.to_vec(), hello))
}

/// Listener handshake when the peer HELLO was already read
/// by the dispatcher. Identical to `handshake_listener`
/// from the pin check onward.
pub fn handshake_listener_with_hello(
    mut stream: TcpStream,
    hello_json: Vec<u8>,
    identity: &Identity,
    registry: &DeviceRegistry,
    config: &SessionConfig,
    clock: &Clock,
    audit: Option<Arc<AuditLog>>,
) -> Result<Session, MimintError> {
    set_timeouts(
        &stream,
        config.handshake_timeout_ms,
        config.handshake_timeout_ms,
    )?;
    let (peer_hello_json, peer_hello) = match parse_hello_json(&hello_json) {
        Ok(ok) => ok,
        Err(err) => {
            send_handshake_error(&mut stream, "handshake_failed");
            return Err(err);
        }
    };
    handshake_listener_after_hello(
        stream,
        peer_hello_json,
        peer_hello,
        identity,
        registry,
        config,
        clock,
        audit,
    )
}

/// Exchange SESSION_CONFIRM in both directions and switch to
/// idle timeouts. `send_first` is true for the initiator.
fn finish_handshake(
    session: &mut Session,
    identity: &Identity,
    registry: &DeviceRegistry,
    peer_entry: &crate::friends::registry::PeerEntry,
    send_first: bool,
) -> Result<(), MimintError> {
    // The claim is the trust this side believes the peer
    // granted it (learned in the ceremony); the receiver checks
    // it against the trust it granted. A peer cannot claim a
    // grant it was not given.
    let claim = peer_entry
        .trust_received
        .unwrap_or(peer_entry.trust)
        .as_str()
        .to_string();
    let confirm = SessionConfirm {
        device_id: identity.device_id().to_string(),
        paired_trust: claim,
        proto: PROTOCOL_VERSION,
    };
    let confirm_bytes = serde_json::to_vec(&confirm).unwrap();
    if send_first {
        session.send_encrypted(MessageKind::SessionConfirm, &confirm_bytes)?;
        let (kind, plain) = session.recv_encrypted()?;
        check_confirm(session, registry, kind, &plain)?;
    } else {
        let (kind, plain) = session.recv_encrypted()?;
        check_confirm(session, registry, kind, &plain)?;
        session.send_encrypted(MessageKind::SessionConfirm, &confirm_bytes)?;
    }
    session.confirm_sent = true;
    set_timeouts(
        &session.stream,
        session.config.idle_timeout_ms,
        session.config.handshake_timeout_ms,
    )?;
    let now = session.clock.now_ms();
    let _ = registry.touch_last_seen(&session.peer_device_id, now);
    let _ = registry.mark_pairing_complete(&session.peer_device_id);
    Ok(())
}

fn check_confirm(
    session: &mut Session,
    registry: &DeviceRegistry,
    kind: MessageKind,
    plain: &[u8],
) -> Result<(), MimintError> {
    if kind != MessageKind::SessionConfirm {
        audit_event(
            &session.audit,
            "handshake_out_of_order",
            serde_json::json!({"peer_device_id": session.peer_device_id}),
        );
        return Err(MimintError::Config(
            "friends handshake: expected session_confirm".to_string(),
        ));
    }
    let confirm: SessionConfirm = serde_json::from_slice(plain).map_err(|err| {
        MimintError::Config(format!(
            "friends protocol error: bad session_confirm: {err}"
        ))
    })?;
    let entry = registry.get_peer(&session.peer_device_id)?.ok_or_else(|| {
        MimintError::Config("friends handshake: peer vanished from registry".to_string())
    })?;
    if confirm.device_id != session.peer_device_id
        || confirm.proto != PROTOCOL_VERSION
        || confirm.paired_trust != entry.trust.as_str()
    {
        // Silent close inside the encrypted channel (F8).
        audit_event(
            &session.audit,
            "trust_mismatch",
            serde_json::json!({"peer_device_id": session.peer_device_id}),
        );
        return Err(MimintError::Config(
            "friends handshake rejected: trust_mismatch".to_string(),
        ));
    }
    session.confirm_received = true;
    Ok(())
}

impl Session {
    /// The peer's device id.
    pub fn peer_device_id(&self) -> &str {
        &self.peer_device_id
    }

    /// A clone of the underlying TCP stream (the hub's kill
    /// button shuts sessions down through it).
    pub fn stream_clone(&self) -> std::io::Result<TcpStream> {
        self.stream.try_clone()
    }

    /// The trust this side granted the peer.
    pub fn peer_trust(&self) -> Trust {
        self.peer_trust
    }

    /// The local device id.
    pub fn local_device_id(&self) -> &str {
        &self.local_device_id
    }

    /// TCP role of this session.
    pub fn role(&self) -> SessionRole {
        self.role
    }

    /// Completed rekeys in this session.
    pub fn rekey_count(&self) -> u64 {
        self.crypto.rekey_count()
    }

    /// Send one encrypted message. Runs a rekey first when a
    /// threshold is due (§2.4 step 6).
    pub fn send(&mut self, kind: MessageKind, plaintext: &[u8]) -> Result<(), MimintError> {
        if self.closed {
            return Err(MimintError::Config("friends session is closed".to_string()));
        }
        if kind.is_plaintext() {
            return Err(MimintError::Config(
                "plaintext kinds cannot be sent on an established session".to_string(),
            ));
        }
        if kind.requires_confirm() && !self.confirm_received {
            return Err(MimintError::Config(
                "friends session: confirm not exchanged yet".to_string(),
            ));
        }
        match self.send_encrypted(kind, plaintext) {
            Ok(()) => Ok(()),
            Err(err) if err.to_string().contains("rekey required") => {
                // Only side A initiates rekeys (§2.4 step 6):
                // both sides hit the threshold independently,
                // so B must not start its own rekey. B reports
                // the error and keeps the session open; the
                // caller retries after A rekeys (m4).
                if self.crypto.local_is_a {
                    self.run_rekey()?;
                    self.send_encrypted(kind, plaintext)
                } else {
                    Err(err)
                }
            }
            Err(err) => {
                self.fail_session(&err);
                Err(err)
            }
        }
    }

    /// Receive one encrypted message, processing `rekey` /
    /// `rekey_ack` internally. Returns the kind and plaintext.
    pub fn recv(&mut self) -> Result<(MessageKind, Vec<u8>), MimintError> {
        if let Some(queued) = self.pending.pop_front() {
            return Ok(queued);
        }
        loop {
            let (kind, plain) = self.recv_encrypted()?;
            match kind {
                MessageKind::Rekey => {
                    self.handle_rekey_request(&plain)?;
                }
                MessageKind::RekeyAck => {
                    if !self.crypto.awaiting_rekey_ack() {
                        self.fail_session_str("unexpected rekey_ack");
                        return Err(MimintError::Config(
                            "friends protocol error: unexpected rekey_ack".to_string(),
                        ));
                    }
                    let ack: RekeyMessage = serde_json::from_slice(&plain)
                        .map_err(|err| MimintError::Config(format!("bad rekey_ack: {err}")))?;
                    if let Err(err) = self.crypto.complete_rekey_as_initiator(&ack) {
                        self.fail_session_str("rekey_failed");
                        audit_event(
                            &self.audit,
                            "rekey_failed",
                            serde_json::json!({"peer_device_id": self.peer_device_id}),
                        );
                        return Err(MimintError::Config(format!("rekey failed: {err}")));
                    }
                }
                MessageKind::SessionConfirm => {
                    // A confirm arriving after the handshake is a
                    // protocol error (duplicate confirm).
                    self.fail_session_str("handshake_out_of_order");
                    return Err(MimintError::Config(
                        "friends protocol error: duplicate session_confirm".to_string(),
                    ));
                }
                _ => {
                    if kind.requires_confirm() && !self.confirm_received {
                        self.fail_session_str("handshake_out_of_order");
                        return Err(MimintError::Config(
                            "friends protocol error: message before confirm".to_string(),
                        ));
                    }
                    return Ok((kind, plain));
                }
            }
        }
    }

    /// Force a rekey now (tests, benchmarks, and explicit
    /// operator action). Blocks until `rekey_ack` arrives or
    /// the rekey timeout fires.
    pub fn initiate_rekey(&mut self) -> Result<(), MimintError> {
        self.run_rekey()
    }

    fn run_rekey(&mut self) -> Result<(), MimintError> {
        let request = self
            .crypto
            .begin_rekey()
            .map_err(|err| MimintError::Config(format!("rekey failed: {err}")))?;
        let request_bytes = serde_json::to_vec(&request).unwrap();
        // Seal with the OLD key (rekey_due is suppressed while
        // a rekey is pending).
        self.send_encrypted(MessageKind::Rekey, &request_bytes)?;
        let deadline = self.clock.now_ms() + self.config.rekey_timeout_ms;
        loop {
            let remaining = deadline.saturating_sub(self.clock.now_ms());
            if remaining == 0 {
                self.crypto.abort_pending_rekey();
                self.fail_session_str("rekey_failed");
                audit_event(
                    &self.audit,
                    "rekey_failed",
                    serde_json::json!({"peer_device_id": self.peer_device_id}),
                );
                return Err(MimintError::Config(
                    "friends rekey timed out waiting for rekey_ack".to_string(),
                ));
            }
            let _ = self
                .stream
                .set_read_timeout(Some(Duration::from_millis(remaining.min(5_000))));
            match self.recv_encrypted_timeout(remaining) {
                Ok((kind, plain)) => match kind {
                    MessageKind::RekeyAck => {
                        let ack: RekeyMessage = serde_json::from_slice(&plain)
                            .map_err(|err| MimintError::Config(format!("bad rekey_ack: {err}")))?;
                        self.crypto
                            .complete_rekey_as_initiator(&ack)
                            .map_err(|err| {
                                self.fail_session_str("rekey_failed");
                                MimintError::Config(format!("rekey failed: {err}"))
                            })?;
                        let _ = self.stream.set_read_timeout(Some(Duration::from_millis(
                            self.config.idle_timeout_ms,
                        )));
                        return Ok(());
                    }
                    MessageKind::Rekey => {
                        // Simultaneous rekey: fail closed.
                        self.fail_session_str("rekey_failed");
                        return Err(MimintError::Config(
                            "friends protocol error: simultaneous rekey".to_string(),
                        ));
                    }
                    other => {
                        // In-flight message: queue it for recv().
                        self.pending.push_back((other, plain));
                    }
                },
                Err(err) => {
                    self.crypto.abort_pending_rekey();
                    self.fail_session_str("rekey_failed");
                    return Err(err);
                }
            }
        }
    }

    fn handle_rekey_request(&mut self, plain: &[u8]) -> Result<(), MimintError> {
        // F7-10: cooldown + rate limit on peer-initiated
        // rekeys. Excess requests are dropped + audited; the
        // session is NOT failed (the peer's own rekey attempt
        // times out on its side if it keeps flooding).
        let now = self.clock.now_ms();
        while let Some(front) = self.rekey_request_times.front() {
            if now.saturating_sub(*front) >= REKEY_RATE_WINDOW_MS {
                self.rekey_request_times.pop_front();
            } else {
                break;
            }
        }
        let too_soon = self
            .rekey_request_times
            .back()
            .is_some_and(|last| now.saturating_sub(*last) < REKEY_MIN_INTERVAL_MS);
        if too_soon || self.rekey_request_times.len() >= REKEY_MAX_PER_MINUTE {
            audit_event(
                &self.audit,
                "rekey_rate_limited",
                serde_json::json!({
                    "peer_device_id": self.peer_device_id,
                    "rekeys_in_window": self.rekey_request_times.len(),
                }),
            );
            return Ok(());
        }
        self.rekey_request_times.push_back(now);
        let request: RekeyMessage = serde_json::from_slice(plain)
            .map_err(|err| MimintError::Config(format!("bad rekey: {err}")))?;
        let (ack, keys) = match self.crypto.prepare_rekey_response(&request) {
            Ok(ok) => ok,
            Err(err) => {
                self.fail_session_str("rekey_failed");
                audit_event(
                    &self.audit,
                    "rekey_failed",
                    serde_json::json!({"peer_device_id": self.peer_device_id}),
                );
                return Err(MimintError::Config(format!("rekey failed: {err}")));
            }
        };
        let ack_bytes = serde_json::to_vec(&ack).unwrap();
        // Ack with the OLD key, then switch the send key.
        // The ack bypasses the rekey-due gate (see
        // seal_rekey): the responder is often due itself
        // when the initiator rekeys.
        self.send_encrypted_rekey(MessageKind::RekeyAck, &ack_bytes)?;
        self.crypto.switch_after_ack_sent(keys);
        Ok(())
    }

    /// Seal + write one message (no rekey handling).
    fn send_encrypted(&mut self, kind: MessageKind, plaintext: &[u8]) -> Result<(), MimintError> {
        let sealed = self.crypto.seal(kind, plaintext).map_err(|err| match err {
            CryptoError::RekeyRequired => MimintError::Config("friends rekey required".to_string()),
            other => MimintError::Config(format!("friends seal failed: {other}")),
        })?;
        self.write_sealed(&sealed)
    }

    /// Seal + write a rekey-protocol message (RekeyAck): the
    /// rekey-due gate must not block it — see seal_rekey (m4).
    fn send_encrypted_rekey(
        &mut self,
        kind: MessageKind,
        plaintext: &[u8],
    ) -> Result<(), MimintError> {
        let sealed = self
            .crypto
            .seal_rekey(kind, plaintext)
            .map_err(|err| match err {
                CryptoError::RekeyRequired => {
                    MimintError::Config("friends rekey required".to_string())
                }
                other => MimintError::Config(format!("friends seal failed: {other}")),
            })?;
        self.write_sealed(&sealed)
    }

    fn write_sealed(&mut self, sealed: &SealedMessage) -> Result<(), MimintError> {
        let header_json = serde_json::to_vec(&sealed.header).unwrap();
        write_frame(&mut self.stream, &header_json, &sealed.ciphertext)?;
        Ok(())
    }

    /// Read + open one message (no rekey handling, no ordering
    /// checks beyond the crypto layer).
    fn recv_encrypted(&mut self) -> Result<(MessageKind, Vec<u8>), MimintError> {
        self.recv_encrypted_timeout(self.config.idle_timeout_ms)
    }

    fn recv_encrypted_timeout(
        &mut self,
        timeout_ms: u64,
    ) -> Result<(MessageKind, Vec<u8>), MimintError> {
        let _ = self
            .stream
            .set_read_timeout(Some(Duration::from_millis(timeout_ms)));
        let frame = read_frame(&mut self.stream).inspect_err(|_| {
            if !self.closed {
                self.fail_session_str("handshake_aborted");
            }
        })?;
        let kind = kind_of(&frame.json).inspect_err(|_| {
            audit_event(
                &self.audit,
                "unknown_message_kind",
                serde_json::json!({"peer_device_id": self.peer_device_id}),
            );
            self.fail_session_str("unknown_message_kind");
        })?;
        if kind.is_plaintext() {
            audit_event(
                &self.audit,
                "handshake_out_of_order",
                serde_json::json!({"peer_device_id": self.peer_device_id}),
            );
            self.fail_session_str("handshake_out_of_order");
            return Err(MimintError::Config(
                "friends protocol error: plaintext kind after handshake".to_string(),
            ));
        }
        let header: EncryptedHeader = serde_json::from_slice(&frame.json).map_err(|err| {
            self.fail_session_str("handshake_out_of_order");
            MimintError::Config(format!("friends protocol error: bad header: {err}"))
        })?;
        if header.kind != kind.as_str() {
            self.fail_session_str("handshake_out_of_order");
            return Err(MimintError::Config(
                "friends protocol error: header kind mismatch".to_string(),
            ));
        }
        let plain = self.crypto.open(&header, &frame.payload).map_err(|err| {
            let audit_kind = match err {
                CryptoError::Sequence => "sequence_gap_or_replay",
                CryptoError::Aead => "aead_open_failed",
                _ => "handshake_out_of_order",
            };
            audit_event(
                &self.audit,
                audit_kind,
                serde_json::json!({"peer_device_id": self.peer_device_id}),
            );
            self.fail_session_str(audit_kind);
            MimintError::Config(format!("friends session failed: {err}"))
        })?;
        Ok((kind, plain))
    }

    fn fail_session(&mut self, _err: &MimintError) {
        self.closed = true;
        self.crypto.zeroize();
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }

    fn fail_session_str(&mut self, _reason: &str) {
        self.closed = true;
        self.crypto.zeroize();
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }

    /// Close the session: zeroize keys and shut the socket down.
    pub fn close(mut self) {
        self.closed = true;
        self.crypto.zeroize();
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }

    /// Shut the socket down without consuming the session (the
    /// hub's kill switch).
    pub fn shutdown(&mut self) {
        self.closed = true;
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }

    /// True once the session has failed or been shut down.
    pub fn is_closed(&self) -> bool {
        self.closed
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.crypto.zeroize();
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}
