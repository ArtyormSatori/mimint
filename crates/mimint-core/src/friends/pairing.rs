//! Pairing ceremony (Phase 7, §3.3–§3.4).
//!
//! Two SAS flows share steps 1–2 and 5–7 and differ only in how
//! the SAS is compared:
//!
//! * Compare (default): BOTH sides display the SAS; the user
//!   compares them with their eyes and confirms separately on
//!   each side. The machine CANNOT detect a user who confirms
//!   despite a mismatch (T12, stated honestly — R4); the
//!   fingerprint check + per-side confirmation mitigate it.
//! * Typing (fallback, e.g. screen-less devices): side A
//!   displays the SAS, the user types it into side B, and B
//!   compares the typed code with its own computed SAS.
//!
//! Naming warning (R3): the letters A/B in this module mean the
//! ceremony sides (A = initiator, B = listener). They are NOT
//! the §2.2 A/B convention (smaller/larger device_id), which
//! governs crypto strings. Ceremony A/B never appears in any
//! crypto string or wire field.
//!
//! Pin storage steps (F7D-04): the INITIATOR stores the peer's
//! pin when it sends `pair_accept` (step 6); the LISTENER stores
//! the peer's pin when it receives `pair_accept` (step 7). A
//! connection drop between the two leaves the initiator with a
//! pin and the listener without one: the pairing shows as
//! incomplete until a fresh ceremony converges.

use crate::audit::AuditLog;
use crate::error::MimintError;
use crate::friends::crypto::{
    fresh_nonce16, generate_ephemeral, identity_verify, sas_from_transcript, transcript_hash,
};
use crate::friends::identity::Identity;
use crate::friends::registry::{DeviceRegistry, PeerEntry, Trust, LOCKOUT_FAILURES};
use crate::friends::util::{ct_equal, hex_decode, hex_encode, Clock};
use crate::friends::wire::{
    kind_of, read_frame, version_of, write_frame, MessageKind, PROTOCOL_VERSION,
};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Ceremony lifetime (§3.3): 5 minutes from `pair_request`.
pub const CEREMONY_TIMEOUT_MS: u64 = 5 * 60 * 1000;
/// Pairing mode lifetime (§3.3): 5 minutes or one ceremony.
pub const PAIRING_MODE_MS: u64 = 5 * 60 * 1000;
/// Maximum `pair_request`s per minute from all sources (§3.4).
pub const MAX_PAIR_REQUESTS_PER_MINUTE: usize = 10;

/// Which SAS flow a ceremony uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SasFlow {
    /// Both sides display; the user compares (default).
    Compare,
    /// A displays; the user types the code into B (fallback).
    Typing,
}

/// `pair_request` (plaintext, ceremony step 1).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairRequest {
    pub v: u32,
    pub kind: String,
    pub device_id: String,
    pub identity_pub: String,
    pub ecdh_pub: String,
    pub nonce: String,
    pub wanted_trust: String,
}

/// `pair_response` (plaintext, ceremony step 2).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairResponse {
    pub v: u32,
    pub kind: String,
    pub device_id: String,
    pub identity_pub: String,
    pub ecdh_pub: String,
    pub nonce: String,
}

/// `pair_confirm` (plaintext, ceremony step 5).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairConfirm {
    pub v: u32,
    pub kind: String,
    pub device_id: String,
    /// ECDSA signature over the ceremony transcript hash, hex.
    pub sig: String,
    /// Trust the listener grants the initiator.
    pub granted_trust: String,
}

/// `pair_accept` (plaintext, ceremony step 6).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairAccept {
    pub v: u32,
    pub kind: String,
    pub device_id: String,
    /// ECDSA signature over the ceremony transcript hash, hex.
    pub sig: String,
    /// Trust the initiator grants the listener.
    pub granted_trust: String,
}

/// Generic ceremony error (no detail about what failed).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairingErrorMessage {
    pub v: u32,
    pub kind: String,
    pub code: String,
}

fn send_pairing_error(stream: &mut TcpStream) {
    let message = PairingErrorMessage {
        v: PROTOCOL_VERSION,
        kind: MessageKind::PairingError.as_str().to_string(),
        code: "pairing_failed".to_string(),
    };
    let json = serde_json::to_vec(&message).unwrap_or_default();
    let _ = write_frame(stream, &json, &[]);
}

fn audit_pairing(audit: &Option<Arc<AuditLog>>, kind: &str, detail: serde_json::Value) {
    if let Some(log) = audit {
        let _ = log.log("friends-pairing", kind, Some("friends"), None, detail);
    }
}

/// What the user decided on the LISTENER side (B).
#[derive(Debug, Clone)]
pub enum SasOutcome {
    /// Compare flow: the user reports the two SAS codes match.
    Matched,
    /// Compare flow: the user reports the two SAS codes differ.
    Mismatched,
    /// Typing flow: the code the user typed into B.
    Typed(String),
}

/// Context shown to the user on the listener side.
#[derive(Debug, Clone)]
pub struct ListenerDecisionCtx {
    /// The SAS to display (compare flow only; `None` in the
    /// typing flow, where B must NOT display its SAS).
    pub sas: Option<String>,
    pub peer_device_id: String,
    pub peer_fingerprint: String,
    pub wanted_trust: Trust,
    pub flow: SasFlow,
}

/// The listener-side decision (trust + SAS + fingerprint).
#[derive(Debug, Clone)]
pub struct ListenerDecision {
    pub sas_outcome: SasOutcome,
    pub trust_granted: Trust,
    pub fingerprint_confirmed: bool,
    pub fingerprint_skipped_with_warning: bool,
}

/// User interaction on the listener side. The GUI/CLI
/// implements this with real prompts; tests use scripted sims.
pub trait ListenerUi: Send + Sync {
    fn decide(&self, ctx: &ListenerDecisionCtx) -> ListenerDecision;
}

/// Context shown to the user on the initiator side (A).
#[derive(Debug, Clone)]
pub struct InitiatorDecisionCtx {
    pub sas: String,
    pub peer_device_id: String,
    pub peer_fingerprint: String,
    /// Trust the listener granted this side.
    pub trust_received: Trust,
    pub flow: SasFlow,
}

/// The initiator-side decision.
#[derive(Debug, Clone)]
pub struct InitiatorDecision {
    pub confirmed: bool,
    pub trust_granted: Trust,
    pub fingerprint_confirmed: bool,
    pub fingerprint_skipped_with_warning: bool,
}

/// User interaction on the initiator side.
pub trait InitiatorUi: Send + Sync {
    /// Display the SAS + peer fingerprint (both flows: in the
    /// typing flow the user reads the SAS here to type it into
    /// B; in the compare flow the user compares the two
    /// displays). Called before `pair_confirm` arrives.
    fn show_sas(&self, sas: &str, peer_fingerprint: &str, peer_device_id: &str);
    /// Final confirmation after `pair_confirm` verified.
    fn decide(&self, ctx: &InitiatorDecisionCtx) -> InitiatorDecision;
}

/// Pairing-mode + ceremony-guard + rate-limit state, shared by
/// the hub's ceremony handler. Lockout counts live in the
/// registry (persistent across restarts); mode and rate-limit
/// state are in-memory.
pub struct PairingController {
    mode_until_ms: Mutex<Option<u64>>,
    ceremony_active: Mutex<bool>,
    request_times_ms: Mutex<VecDeque<u64>>,
    clock: Clock,
}

impl PairingController {
    pub fn new(clock: Clock) -> Self {
        Self {
            mode_until_ms: Mutex::new(None),
            ceremony_active: Mutex::new(false),
            request_times_ms: Mutex::new(VecDeque::new()),
            clock,
        }
    }

    /// Enable pairing mode for `PAIRING_MODE_MS` (user action).
    pub fn enable_pairing_mode(&self) {
        let until = self.clock.now_ms() + PAIRING_MODE_MS;
        *self.mode_until_ms.lock().unwrap_or_else(|p| p.into_inner()) = Some(until);
    }

    /// Disable pairing mode immediately.
    pub fn disable_pairing_mode(&self) {
        *self.mode_until_ms.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }

    /// True while pairing mode is enabled and unexpired.
    pub fn is_pairing_mode(&self) -> bool {
        match *self.mode_until_ms.lock().unwrap_or_else(|p| p.into_inner()) {
            Some(until) => self.clock.now_ms() < until,
            None => false,
        }
    }

    /// Milliseconds left in pairing mode (0 when off).
    pub fn pairing_mode_remaining_ms(&self) -> u64 {
        match *self.mode_until_ms.lock().unwrap_or_else(|p| p.into_inner()) {
            Some(until) => until.saturating_sub(self.clock.now_ms()),
            None => 0,
        }
    }

    /// Try to claim the single-ceremony slot. `None` when a
    /// ceremony is already running.
    fn try_begin_ceremony(&self) -> Option<CeremonyGuard<'_>> {
        let mut slot = self
            .ceremony_active
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if *slot {
            return None;
        }
        *slot = true;
        Some(CeremonyGuard { controller: self })
    }

    /// Sliding-window rate limit: at most 10 `pair_request`s
    /// per minute from all sources. Returns false when the
    /// request must be refused.
    fn check_rate_limit(&self) -> bool {
        let now = self.clock.now_ms();
        let mut times = self
            .request_times_ms
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        while let Some(front) = times.front() {
            if now.saturating_sub(*front) >= 60_000 {
                times.pop_front();
            } else {
                break;
            }
        }
        if times.len() >= MAX_PAIR_REQUESTS_PER_MINUTE {
            return false;
        }
        times.push_back(now);
        true
    }
}

struct CeremonyGuard<'a> {
    controller: &'a PairingController,
}

impl Drop for CeremonyGuard<'_> {
    fn drop(&mut self) {
        let mut slot = self
            .controller
            .ceremony_active
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        *slot = false;
        // One ceremony per pairing-mode enable (§3.3): any
        // ceremony ending disables the mode.
        self.controller.disable_pairing_mode();
    }
}

/// Build the registry entry for a freshly paired peer. The name
/// defaults to the first 8 fingerprint characters (the ceremony
/// carries no name field); the user renames it in device
/// management. Re-pairing the same device id bumps
/// `key_generation`; a brand-new device id starts at 1.
pub fn build_peer_entry(
    registry: &DeviceRegistry,
    device_id: &str,
    identity_pub_hex: &str,
    fingerprint_hex: &str,
    trust_granted: Trust,
    trust_received: Trust,
    now_ms: u64,
) -> PeerEntry {
    let previous = registry.get_peer(device_id).ok().flatten();
    PeerEntry {
        device_id: device_id.to_string(),
        name: previous
            .as_ref()
            .map(|p| p.name.clone())
            .unwrap_or_else(|| format!("Thiết bị {}", &fingerprint_hex[..8])),
        identity_pub_hex: identity_pub_hex.to_string(),
        fingerprint_hex: fingerprint_hex.to_string(),
        trust: trust_granted,
        trust_received: Some(trust_received),
        paired_at_ms: now_ms,
        last_seen_ms: now_ms,
        key_generation: previous.as_ref().map(|p| p.key_generation + 1).unwrap_or(1),
        revoked: false,
        revoked_at_ms: None,
        last_addr: previous.and_then(|p| p.last_addr),
        pairing_incomplete: true,
    }
}

/// Run the ceremony as the INITIATOR (side A). On success the
/// peer's pin is stored (step 6) and the entry is returned; the
/// caller must still prove the pairing with one ordinary
/// session handshake before treating it as complete.
#[allow(clippy::too_many_arguments)]
pub fn run_ceremony_initiator(
    stream: &mut TcpStream,
    identity: &Identity,
    registry: &DeviceRegistry,
    ui: &dyn InitiatorUi,
    flow: SasFlow,
    wanted_trust: Trust,
    clock: &Clock,
    audit: Option<Arc<AuditLog>>,
) -> Result<PeerEntry, MimintError> {
    let started_ms = clock.now_ms();
    let deadline_ms = started_ms + CEREMONY_TIMEOUT_MS;
    let set_remaining_timeout = |stream: &TcpStream| -> Result<(), MimintError> {
        let remaining = deadline_ms.saturating_sub(clock.now_ms());
        stream
            .set_read_timeout(Some(Duration::from_millis(remaining.max(1))))
            .map_err(|err| MimintError::Config(format!("cannot set read timeout: {err}")))?;
        Ok(())
    };
    set_remaining_timeout(stream)?;

    let (ephemeral, ecdh_pub) = generate_ephemeral();
    let _ = ephemeral; // Ceremony ECDH binds the transcript only;
                       // no session keys derive from the ceremony exchange.
    let my_nonce = fresh_nonce16();
    let request = PairRequest {
        v: PROTOCOL_VERSION,
        kind: MessageKind::PairRequest.as_str().to_string(),
        device_id: identity.device_id().to_string(),
        identity_pub: identity.public_key_hex(),
        ecdh_pub: hex_encode(&ecdh_pub),
        nonce: hex_encode(&my_nonce),
        wanted_trust: wanted_trust.as_str().to_string(),
    };
    let request_json = serde_json::to_vec(&request).unwrap();
    write_frame(stream, &request_json, &[])?;

    let response_frame = read_frame(stream).inspect_err(|_| {
        audit_pairing(
            &audit,
            "pairing_failed",
            serde_json::json!({"reason": "no_response"}),
        );
    })?;
    if clock.now_ms() >= deadline_ms {
        audit_pairing(&audit, "pairing_expired", serde_json::json!({}));
        return Err(MimintError::Config("pairing ceremony expired".to_string()));
    }
    if kind_of(&response_frame.json)? == MessageKind::PairingError {
        audit_pairing(
            &audit,
            "pairing_failed",
            serde_json::json!({"reason": "peer_rejected"}),
        );
        return Err(MimintError::Config(
            "pairing rejected by the other device".to_string(),
        ));
    }
    if version_of(&response_frame.json)? != PROTOCOL_VERSION
        || kind_of(&response_frame.json)? != MessageKind::PairResponse
    {
        return Err(MimintError::Config(
            "pairing ceremony: expected pair_response".to_string(),
        ));
    }
    let response: PairResponse = serde_json::from_slice(&response_frame.json).map_err(|err| {
        MimintError::Config(format!("pairing ceremony: bad pair_response: {err}"))
    })?;
    let response_json = response_frame.json.clone();
    let transcript = transcript_hash(PROTOCOL_VERSION, &request_json, &response_json);
    let sas = sas_from_transcript(&transcript);
    let peer_pub_bytes = hex_decode(&response.identity_pub).unwrap_or_default();
    let peer_fingerprint = crate::friends::crypto::fingerprint_from_public_key(&peer_pub_bytes);
    ui.show_sas(&sas, &peer_fingerprint, &response.device_id);

    set_remaining_timeout(stream)?;
    let confirm_frame = read_frame(stream).inspect_err(|_| {
        audit_pairing(
            &audit,
            "pairing_failed",
            serde_json::json!({"reason": "no_confirm"}),
        );
    })?;
    if clock.now_ms() >= deadline_ms {
        audit_pairing(&audit, "pairing_expired", serde_json::json!({}));
        return Err(MimintError::Config("pairing ceremony expired".to_string()));
    }
    if kind_of(&confirm_frame.json)? == MessageKind::PairingError {
        audit_pairing(
            &audit,
            "pairing_failed",
            serde_json::json!({"reason": "peer_rejected"}),
        );
        return Err(MimintError::Config(
            "pairing rejected by the other device".to_string(),
        ));
    }
    if version_of(&confirm_frame.json)? != PROTOCOL_VERSION
        || kind_of(&confirm_frame.json)? != MessageKind::PairConfirm
    {
        return Err(MimintError::Config(
            "pairing ceremony: expected pair_confirm".to_string(),
        ));
    }
    let confirm: PairConfirm = serde_json::from_slice(&confirm_frame.json)
        .map_err(|err| MimintError::Config(format!("pairing ceremony: bad pair_confirm: {err}")))?;
    let confirm_sig = hex_decode(&confirm.sig).unwrap_or_default();
    if confirm.device_id != response.device_id
        || !identity_verify(&peer_pub_bytes, &transcript, &confirm_sig)
    {
        audit_pairing(
            &audit,
            "pairing_failed",
            serde_json::json!({"reason": "signature_invalid"}),
        );
        return Err(MimintError::Config(
            "pairing ceremony: pair_confirm signature invalid".to_string(),
        ));
    }
    let trust_received = Trust::parse(&confirm.granted_trust)
        .ok_or_else(|| MimintError::Config("pairing ceremony: bad granted_trust".to_string()))?;

    let decision = ui.decide(&InitiatorDecisionCtx {
        sas: sas.clone(),
        peer_device_id: response.device_id.clone(),
        peer_fingerprint: peer_fingerprint.clone(),
        trust_received,
        flow,
    });
    if clock.now_ms() >= deadline_ms {
        audit_pairing(&audit, "pairing_expired", serde_json::json!({}));
        return Err(MimintError::Config("pairing ceremony expired".to_string()));
    }
    if !decision.confirmed {
        send_pairing_error(stream);
        audit_pairing(
            &audit,
            "pairing_failed",
            serde_json::json!({"reason": "user_declined"}),
        );
        return Err(MimintError::Config(
            "pairing declined by the user".to_string(),
        ));
    }
    if !decision.fingerprint_confirmed && !decision.fingerprint_skipped_with_warning {
        send_pairing_error(stream);
        audit_pairing(
            &audit,
            "pairing_failed",
            serde_json::json!({"reason": "fingerprint_not_confirmed"}),
        );
        return Err(MimintError::Config(
            "pairing ceremony: fingerprint was neither confirmed nor explicitly skipped"
                .to_string(),
        ));
    }
    if decision.fingerprint_skipped_with_warning {
        audit_pairing(
            &audit,
            "fingerprint_check_skipped",
            serde_json::json!({"peer_device_id": response.device_id}),
        );
    }

    let signature = identity.sign(&transcript)?;
    let accept = PairAccept {
        v: PROTOCOL_VERSION,
        kind: MessageKind::PairAccept.as_str().to_string(),
        device_id: identity.device_id().to_string(),
        sig: hex_encode(&signature),
        granted_trust: decision.trust_granted.as_str().to_string(),
    };
    write_frame(stream, &serde_json::to_vec(&accept).unwrap(), &[])?;
    // STEP 6: the initiator stores the pin when sending
    // pair_accept — not later.
    let entry = build_peer_entry(
        registry,
        &response.device_id,
        &response.identity_pub,
        &peer_fingerprint,
        decision.trust_granted,
        trust_received,
        clock.now_ms(),
    );
    registry.upsert_peer(&entry)?;
    let _ = registry.reset_pairing_failures(&response.device_id);
    audit_pairing(
        &audit,
        "pairing_completed",
        serde_json::json!({
            "peer_device_id": response.device_id,
            "fingerprint": peer_fingerprint,
            "trust_granted": decision.trust_granted.as_str(),
            "trust_received": trust_received.as_str(),
        }),
    );
    let _ = sas;
    Ok(entry)
}

/// Run the ceremony as the LISTENER (side B). The first frame
/// has already been read by the dispatcher and must be a
/// `pair_request`; this function enforces pairing mode, lockout,
/// and rate limits before answering.
#[allow(clippy::too_many_arguments)]
pub fn run_ceremony_listener(
    stream: &mut TcpStream,
    peer_addr: &str,
    request_frame: Vec<u8>,
    identity: &Identity,
    registry: &DeviceRegistry,
    controller: &PairingController,
    ui: &dyn ListenerUi,
    flow: SasFlow,
    clock: &Clock,
    audit: Option<Arc<AuditLog>>,
) -> Result<PeerEntry, MimintError> {
    let started_ms = clock.now_ms();
    let deadline_ms = started_ms + CEREMONY_TIMEOUT_MS;
    let request: PairRequest = serde_json::from_slice(&request_frame)
        .map_err(|err| MimintError::Config(format!("pairing ceremony: bad pair_request: {err}")))?;
    let source = request.device_id.clone();

    // Pairing mode is mandatory (§3.3).
    if !controller.is_pairing_mode() {
        send_pairing_error(stream);
        audit_pairing(
            &audit,
            "pairing_failed",
            serde_json::json!({"reason": "not_in_pairing_mode", "peer_device_id": source}),
        );
        return Err(MimintError::Config(
            "pairing rejected: pairing mode is not enabled on this device".to_string(),
        ));
    }
    // Lockout (§3.4): persistent, per source.
    if let Ok(Some(locked_until)) = registry.lockout_until(&source, clock.now_ms()) {
        send_pairing_error(stream);
        audit_pairing(
            &audit,
            "pairing_locked",
            serde_json::json!({"peer_device_id": source, "locked_until_ms": locked_until}),
        );
        return Err(MimintError::Config(
            "pairing locked for this source after repeated failures".to_string(),
        ));
    }
    // One ceremony at a time + rate limit.
    let _ceremony_guard = match controller.try_begin_ceremony() {
        Some(guard) => guard,
        None => {
            send_pairing_error(stream);
            return Err(MimintError::Config(
                "a pairing ceremony is already running".to_string(),
            ));
        }
    };
    if !controller.check_rate_limit() {
        send_pairing_error(stream);
        audit_pairing(
            &audit,
            "pairing_failed",
            serde_json::json!({"reason": "rate_limited"}),
        );
        return Err(MimintError::Config("pairing rate limited".to_string()));
    }
    let _ = peer_addr;

    let set_remaining_timeout = |stream: &TcpStream| -> Result<(), MimintError> {
        let remaining = deadline_ms.saturating_sub(clock.now_ms());
        stream
            .set_read_timeout(Some(Duration::from_millis(remaining.max(1))))
            .map_err(|err| MimintError::Config(format!("cannot set read timeout: {err}")))?;
        Ok(())
    };

    let (ephemeral, ecdh_pub) = generate_ephemeral();
    let _ = ephemeral; // Transcript binding only; see initiator.
    let my_nonce = fresh_nonce16();
    let response = PairResponse {
        v: PROTOCOL_VERSION,
        kind: MessageKind::PairResponse.as_str().to_string(),
        device_id: identity.device_id().to_string(),
        identity_pub: identity.public_key_hex(),
        ecdh_pub: hex_encode(&ecdh_pub),
        nonce: hex_encode(&my_nonce),
    };
    let response_json = serde_json::to_vec(&response).unwrap();
    write_frame(stream, &response_json, &[])?;

    let transcript = transcript_hash(PROTOCOL_VERSION, &request_frame, &response_json);
    let sas = sas_from_transcript(&transcript);
    let peer_pub_bytes = hex_decode(&request.identity_pub).unwrap_or_default();
    let peer_fingerprint = crate::friends::crypto::fingerprint_from_public_key(&peer_pub_bytes);
    let wanted = Trust::parse(&request.wanted_trust).unwrap_or(Trust::Guest);

    let decision = ui.decide(&ListenerDecisionCtx {
        sas: match flow {
            SasFlow::Compare => Some(sas.clone()),
            SasFlow::Typing => None,
        },
        peer_device_id: request.device_id.clone(),
        peer_fingerprint: peer_fingerprint.clone(),
        wanted_trust: wanted,
        flow,
    });
    if clock.now_ms() >= deadline_ms {
        send_pairing_error(stream);
        audit_pairing(&audit, "pairing_expired", serde_json::json!({}));
        return Err(MimintError::Config("pairing ceremony expired".to_string()));
    }

    // SAS check (step 5). Failures count toward lockout (§3.4),
    // identically for both flows.
    let sas_ok = match (&decision.sas_outcome, flow) {
        (SasOutcome::Matched, SasFlow::Compare) => true,
        (SasOutcome::Mismatched, SasFlow::Compare) => false,
        (SasOutcome::Typed(typed), SasFlow::Typing) => ct_equal(typed.as_bytes(), sas.as_bytes()),
        _ => false,
    };
    if !sas_ok {
        let reason = match flow {
            SasFlow::Compare => "sas_mismatch_reported",
            SasFlow::Typing => "sas_mismatch",
        };
        let (count, locked) = registry
            .record_pairing_failure(&source, clock.now_ms())
            .unwrap_or((0, None));
        audit_pairing(
            &audit,
            "pairing_failed",
            serde_json::json!({
                "peer_device_id": source,
                "reason": reason,
                "fail_count": count,
                "locked": locked.is_some(),
            }),
        );
        if locked.is_some() {
            audit_pairing(
                &audit,
                "pairing_locked",
                serde_json::json!({"peer_device_id": source, "locked_until_ms": locked}),
            );
        }
        send_pairing_error(stream);
        return Err(MimintError::Config(format!(
            "pairing failed: SAS check failed ({LOCKOUT_FAILURES} failures lock pairing)"
        )));
    }
    if !decision.fingerprint_confirmed && !decision.fingerprint_skipped_with_warning {
        send_pairing_error(stream);
        audit_pairing(
            &audit,
            "pairing_failed",
            serde_json::json!({"reason": "fingerprint_not_confirmed"}),
        );
        return Err(MimintError::Config(
            "pairing ceremony: fingerprint was neither confirmed nor explicitly skipped"
                .to_string(),
        ));
    }
    if decision.fingerprint_skipped_with_warning {
        audit_pairing(
            &audit,
            "fingerprint_check_skipped",
            serde_json::json!({"peer_device_id": source}),
        );
    }

    let signature = identity.sign(&transcript)?;
    let confirm = PairConfirm {
        v: PROTOCOL_VERSION,
        kind: MessageKind::PairConfirm.as_str().to_string(),
        device_id: identity.device_id().to_string(),
        sig: hex_encode(&signature),
        granted_trust: decision.trust_granted.as_str().to_string(),
    };
    write_frame(stream, &serde_json::to_vec(&confirm).unwrap(), &[])?;

    set_remaining_timeout(stream)?;
    let accept_frame = read_frame(stream).inspect_err(|_| {
        audit_pairing(
            &audit,
            "pairing_failed",
            serde_json::json!({"reason": "no_accept"}),
        );
    })?;
    if clock.now_ms() >= deadline_ms {
        audit_pairing(&audit, "pairing_expired", serde_json::json!({}));
        return Err(MimintError::Config("pairing ceremony expired".to_string()));
    }
    if kind_of(&accept_frame.json)? == MessageKind::PairingError {
        audit_pairing(
            &audit,
            "pairing_failed",
            serde_json::json!({"reason": "peer_aborted"}),
        );
        return Err(MimintError::Config(
            "pairing aborted by the other device".to_string(),
        ));
    }
    if version_of(&accept_frame.json)? != PROTOCOL_VERSION
        || kind_of(&accept_frame.json)? != MessageKind::PairAccept
    {
        return Err(MimintError::Config(
            "pairing ceremony: expected pair_accept".to_string(),
        ));
    }
    let accept: PairAccept = serde_json::from_slice(&accept_frame.json)
        .map_err(|err| MimintError::Config(format!("pairing ceremony: bad pair_accept: {err}")))?;
    let accept_sig = hex_decode(&accept.sig).unwrap_or_default();
    if accept.device_id != request.device_id
        || !identity_verify(&peer_pub_bytes, &transcript, &accept_sig)
    {
        audit_pairing(
            &audit,
            "pairing_failed",
            serde_json::json!({"reason": "signature_invalid"}),
        );
        return Err(MimintError::Config(
            "pairing ceremony: pair_accept signature invalid".to_string(),
        ));
    }
    let trust_received = Trust::parse(&accept.granted_trust)
        .ok_or_else(|| MimintError::Config("pairing ceremony: bad granted_trust".to_string()))?;

    // STEP 7: the listener stores the pin when it receives
    // pair_accept — not before.
    let entry = build_peer_entry(
        registry,
        &request.device_id,
        &request.identity_pub,
        &peer_fingerprint,
        decision.trust_granted,
        trust_received,
        clock.now_ms(),
    );
    registry.upsert_peer(&entry)?;
    let _ = registry.reset_pairing_failures(&source);
    audit_pairing(
        &audit,
        "pairing_completed",
        serde_json::json!({
            "peer_device_id": source,
            "fingerprint": peer_fingerprint,
            "trust_granted": decision.trust_granted.as_str(),
            "trust_received": trust_received.as_str(),
        }),
    );
    Ok(entry)
}

/// Read the first frame of a ceremony connection and confirm it
/// is a `pair_request`. Used by the hub dispatcher.
pub fn read_pair_request(stream: &mut TcpStream) -> Result<Vec<u8>, MimintError> {
    let frame = read_frame(stream)?;
    if version_of(&frame.json)? != PROTOCOL_VERSION {
        return Err(MimintError::Config("version_mismatch".to_string()));
    }
    if kind_of(&frame.json)? != MessageKind::PairRequest {
        return Err(MimintError::Config("expected pair_request".to_string()));
    }
    if !frame.payload.is_empty() {
        return Err(MimintError::Config(
            "pair_request must not carry a payload".to_string(),
        ));
    }
    Ok(frame.json)
}
