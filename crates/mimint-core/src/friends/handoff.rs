//! Handoff (Phase 7, §7): a task moves from one body to
//! another with a 1-actuator lease.
//!
//! The package carries task text, done steps, the next step,
//! file/tab references, and SHARED notebook context — never
//! secrets, never the vault, never history. It is signed by
//! the sender's identity key and verified against the pinned
//! peer key at receipt. The lease lives in memory only: it
//! does NOT survive a restart (§7.2). Clock skew tolerance
//! is 2 minutes, checked at receipt.
//!
//! The sender stops its own work once the receiver accepts:
//! [`HandoffManager::is_handed_off`] answers that question
//! for the app layer, which must check it before acting.

use crate::audit::AuditLog;
use crate::error::MimintError;
use crate::friends::crypto::{fresh_nonce16, identity_verify};
use crate::friends::texts;
use crate::friends::util::{hex_decode, hex_encode, Clock};
use crate::friends::wire::PROTOCOL_VERSION;
use crate::notebook::Notebook;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Handoff lease TTL (§7.2): 10 minutes.
pub const HANDOFF_LEASE_TTL_MS: u64 = 10 * 60 * 1000;
/// Maximum package size (§7.2): 64 KiB.
pub const HANDOFF_PACKAGE_MAX_BYTES: usize = 64 * 1024;
/// Clock-skew tolerance (§7.2): 2 minutes, checked at receipt.
pub const HANDOFF_CLOCK_SKEW_MS: u64 = 2 * 60 * 1000;

/// The 7 handoff states (§7.4), with their exact UI labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandoffState {
    NotHandedOff,
    AwaitingAccept,
    HandedOff,
    HoldingControl,
    Rejected,
    Expired,
    Unknown,
}

impl HandoffState {
    pub fn as_text(self) -> &'static str {
        match self {
            HandoffState::NotHandedOff => texts::HANDOFF_STATE_NOT_HANDED_OFF,
            HandoffState::AwaitingAccept => texts::HANDOFF_STATE_AWAITING_ACCEPT,
            HandoffState::HandedOff => texts::HANDOFF_STATE_HANDED_OFF,
            HandoffState::HoldingControl => texts::HANDOFF_STATE_HOLDING_CONTROL,
            HandoffState::Rejected => texts::HANDOFF_STATE_REJECTED,
            HandoffState::Expired => texts::HANDOFF_STATE_EXPIRED,
            HandoffState::Unknown => texts::HANDOFF_STATE_UNKNOWN,
        }
    }
}

/// A handoff package: the task, its progress, and the context
/// the receiver needs — signed, versioned, expiry-bounded.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffPackage {
    pub v: u32,
    pub kind: String,
    pub package_id: String,
    pub from_device_id: String,
    pub to_device_id: String,
    pub task_text: String,
    pub done_steps: Vec<String>,
    pub next_step: String,
    pub file_refs: Vec<String>,
    pub tab_refs: Vec<String>,
    /// Shared-notebook context only. Never secrets.
    pub notebook_context: String,
    pub lease_id: String,
    pub actuator: String,
    pub created_ms: u64,
    pub expires_ms: u64,
    pub version: u32,
    /// ECDSA signature over the canonical package bytes, hex.
    pub sig: String,
}

/// `handoff_offer` wraps a package.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffOffer {
    pub v: u32,
    pub kind: String,
    pub package: HandoffPackage,
}

/// `handoff_accept` returns the lease.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffAccept {
    pub v: u32,
    pub kind: String,
    pub package_id: String,
    pub lease_id: String,
}

/// `handoff_reject` returns the lease without taking it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffReject {
    pub v: u32,
    pub kind: String,
    pub package_id: String,
    pub reason: String,
}

/// `handoff_complete` releases the lease.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffComplete {
    pub v: u32,
    pub kind: String,
    pub package_id: String,
    pub lease_id: String,
}

/// A verified handoff package waiting for the receiver's user
/// to press "Tiếp tục ở đây" / "Từ chối" (§7.3). No lease is
/// held while a package is pending: the sender keeps the lease
/// until the receiver accepts, rejects, or the lease TTL runs
/// out.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingHandoff {
    pub package: HandoffPackage,
    /// When this body received the offer (receiver clock).
    pub received_ms: u64,
}

impl PendingHandoff {
    /// Milliseconds of lease life left on the receiver clock
    /// (saturating; 0 once expired).
    pub fn remaining_ms(&self, now_ms: u64) -> u64 {
        self.package.expires_ms.saturating_sub(now_ms)
    }
}

/// A user decision on a pending handoff, written by the CLI
/// (`friends handoff accept|reject`) into the decisions
/// directory for the hub sweeper to pick up (§7.3: the deciding
/// user may run in a different process from the hub).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffDecision {
    pub package_id: String,
    pub accept: bool,
    pub decided_ms: u64,
}

/// One actuator lease (in memory only).
#[derive(Debug, Clone)]
pub struct Lease {
    pub lease_id: String,
    pub package_id: String,
    pub holder_device_id: String,
    pub actuator: String,
    pub acquired_ms: u64,
    pub expires_ms: u64,
}

/// Handoff state for one body: leases + handed-off tracking.
pub struct HandoffManager {
    leases: Mutex<HashMap<String, Lease>>,
    handed_off: Mutex<HashSet<String>>,
    /// Verified packages waiting for the receiver's user (§7.3).
    pending: Mutex<HashMap<String, PendingHandoff>>,
    /// Package ids the sender side learned were rejected (§7.2).
    rejected: Mutex<HashSet<String>>,
    /// Data dir for the pending-file bridge (None in tests).
    pending_dir: Mutex<Option<PathBuf>>,
    clock: Clock,
    audit: Option<Arc<AuditLog>>,
}

impl HandoffManager {
    pub fn new(clock: Clock, audit: Option<Arc<AuditLog>>) -> Self {
        Self {
            leases: Mutex::new(HashMap::new()),
            handed_off: Mutex::new(HashSet::new()),
            pending: Mutex::new(HashMap::new()),
            rejected: Mutex::new(HashSet::new()),
            pending_dir: Mutex::new(None),
            clock,
            audit,
        }
    }

    fn audit(&self, kind: &str, detail: serde_json::Value) {
        if let Some(log) = &self.audit {
            let _ = log.log("friends-handoff", kind, Some("friends"), None, detail);
        }
    }

    /// Build + sign a package. The caller supplies the shared
    /// notebook context (see [`shared_notebook_context`]).
    #[allow(clippy::too_many_arguments)]
    pub fn build_package(
        &self,
        identity: &crate::friends::identity::Identity,
        to_device_id: &str,
        task_text: String,
        done_steps: Vec<String>,
        next_step: String,
        file_refs: Vec<String>,
        tab_refs: Vec<String>,
        notebook_context: String,
        actuator: &str,
    ) -> Result<HandoffPackage, MimintError> {
        let now = self.clock.now_ms();
        let mut package = HandoffPackage {
            v: PROTOCOL_VERSION,
            kind: "handoff_offer".to_string(),
            package_id: hex_encode(&fresh_nonce16()),
            from_device_id: identity.device_id().to_string(),
            to_device_id: to_device_id.to_string(),
            task_text,
            done_steps,
            next_step,
            file_refs,
            tab_refs,
            notebook_context,
            lease_id: hex_encode(&fresh_nonce16()),
            actuator: actuator.to_string(),
            created_ms: now,
            expires_ms: now + HANDOFF_LEASE_TTL_MS,
            version: 1,
            sig: String::new(),
        };
        let canonical = canonical_package_bytes(&package)?;
        let signature = identity.sign(&canonical)?;
        package.sig = hex_encode(&signature);
        Ok(package)
    }

    /// Verify a received package against the sender's pinned
    /// public key. The seven checks (§7.3): version, size,
    /// recipient, signature, expiry, clock skew at receipt,
    /// and package shape.
    pub fn verify_package(
        &self,
        package: &HandoffPackage,
        sender_public_key: &[u8],
        own_device_id: &str,
    ) -> Result<(), MimintError> {
        let now = self.clock.now_ms();
        if package.v != PROTOCOL_VERSION || package.version != 1 {
            return Err(MimintError::Config(
                "handoff package version mismatch".to_string(),
            ));
        }
        if package.to_device_id != own_device_id {
            return Err(MimintError::Config(
                "handoff package is addressed to a different device".to_string(),
            ));
        }
        let bytes = serde_json::to_vec(package).map_err(|err| {
            MimintError::Config(format!("handoff package cannot serialize: {err}"))
        })?;
        if bytes.len() > HANDOFF_PACKAGE_MAX_BYTES {
            return Err(MimintError::Config(
                "handoff package exceeds the 64 KiB cap".to_string(),
            ));
        }
        // Clock skew at receipt: the package must be fresh
        // within ±2 minutes of this device's clock.
        if package.created_ms > now + HANDOFF_CLOCK_SKEW_MS
            || now > package.created_ms + HANDOFF_CLOCK_SKEW_MS
        {
            return Err(MimintError::Config(
                "handoff package clock skew exceeds 2 minutes".to_string(),
            ));
        }
        if now > package.expires_ms {
            return Err(MimintError::Config("handoff package expired".to_string()));
        }
        let canonical = canonical_package_bytes(package)?;
        let signature = hex_decode(&package.sig).unwrap_or_default();
        if !identity_verify(sender_public_key, &canonical, &signature) {
            return Err(MimintError::Config(
                "handoff package signature invalid".to_string(),
            ));
        }
        Ok(())
    }

    /// Take the 1-actuator lease for a verified package.
    /// Fails with `lease_busy` when the actuator is already
    /// leased (unexpired) to another package.
    pub fn acquire_lease(&self, package: &HandoffPackage) -> Result<Lease, MimintError> {
        let now = self.clock.now_ms();
        let mut leases = self.leases.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(existing) = leases.get(&package.actuator) {
            if now < existing.expires_ms {
                return Err(MimintError::Config(format!(
                    "lease_busy: actuator {} is leased to package {}",
                    package.actuator, existing.package_id
                )));
            }
        }
        let lease = Lease {
            lease_id: package.lease_id.clone(),
            package_id: package.package_id.clone(),
            holder_device_id: package.from_device_id.clone(),
            actuator: package.actuator.clone(),
            acquired_ms: now,
            expires_ms: package.expires_ms,
        };
        leases.insert(package.actuator.clone(), lease.clone());
        self.audit(
            "handoff_lease_acquired",
            serde_json::json!({
                "package_id": package.package_id,
                "actuator": package.actuator,
            }),
        );
        Ok(lease)
    }

    /// Release a lease (complete / reject / expiry). Returns
    /// true when a matching lease was held.
    pub fn release_lease(&self, actuator: &str, lease_id: &str) -> bool {
        let mut leases = self.leases.lock().unwrap_or_else(|p| p.into_inner());
        match leases.get(actuator) {
            Some(lease) if lease.lease_id == lease_id => {
                leases.remove(actuator);
                self.audit(
                    "handoff_lease_released",
                    serde_json::json!({"actuator": actuator, "lease_id": lease_id}),
                );
                true
            }
            _ => false,
        }
    }

    /// Release a lease by its id, without knowing the actuator
    /// (the `handoff_complete` message carries no actuator).
    /// Returns true when a matching lease was held.
    pub fn release_lease_by_id(&self, lease_id: &str) -> bool {
        let mut leases = self.leases.lock().unwrap_or_else(|p| p.into_inner());
        let actuator = leases
            .iter()
            .find(|(_, lease)| lease.lease_id == lease_id)
            .map(|(actuator, _)| actuator.clone());
        match actuator {
            Some(actuator) => {
                leases.remove(&actuator);
                self.audit(
                    "handoff_lease_released",
                    serde_json::json!({"actuator": actuator, "lease_id": lease_id}),
                );
                true
            }
            None => false,
        }
    }

    /// The current lease for an actuator (expired leases are
    /// dropped lazily and read as absent).
    pub fn lease_for(&self, actuator: &str) -> Option<Lease> {
        let now = self.clock.now_ms();
        let mut leases = self.leases.lock().unwrap_or_else(|p| p.into_inner());
        match leases.get(actuator) {
            Some(lease) if now < lease.expires_ms => Some(lease.clone()),
            Some(_) => {
                leases.remove(actuator);
                None
            }
            None => None,
        }
    }

    /// The sender marks a package handed off once the receiver
    /// accepts. The app layer must refuse to act on a task
    /// whose package id is handed off.
    pub fn mark_handed_off(&self, package_id: &str) {
        let mut handed = self.handed_off.lock().unwrap_or_else(|p| p.into_inner());
        handed.insert(package_id.to_string());
        self.audit(
            "handoff_accepted_by_peer",
            serde_json::json!({"package_id": package_id}),
        );
    }

    pub fn is_handed_off(&self, package_id: &str) -> bool {
        let handed = self.handed_off.lock().unwrap_or_else(|p| p.into_inner());
        handed.contains(package_id)
    }

    /// Forget handed-off state (e.g. after a restart — leases
    /// do not survive restarts, and neither does this).
    /// Clear the handed-off mark for ONE package (the
    /// `handoff_complete` ack returns the lease to the sender:
    /// the sender may act on this task again). Unlike
    /// `clear_handed_off`, this touches nothing else.
    pub fn clear_handed_off_package(&self, package_id: &str) {
        let mut handed = self.handed_off.lock().unwrap_or_else(|p| p.into_inner());
        handed.remove(package_id);
        self.audit(
            "handoff_lease_returned",
            serde_json::json!({"package_id": package_id}),
        );
    }

    pub fn clear_handed_off(&self) {
        let mut handed = self.handed_off.lock().unwrap_or_else(|p| p.into_inner());
        handed.clear();
        let mut leases = self.leases.lock().unwrap_or_else(|p| p.into_inner());
        leases.clear();
    }

    /// Point the pending-handoff file bridge at a data dir
    /// (§7.3): the hub process writes `pending-handoffs.json`
    /// there, and the CLI decision commands write one file per
    /// decision into `handoff-decisions/`. A separate CLI
    /// process cannot see the hub's in-memory pending set, so
    /// the files are the bridge. Nothing is deleted here: a
    /// short-lived CLI process shares the dir with the running
    /// hub and must never wipe its pending file — the hub
    /// clears stale files when it starts instead.
    pub fn set_pending_dir(&self, dir: PathBuf) {
        let mut slot = self.pending_dir.lock().unwrap_or_else(|p| p.into_inner());
        *slot = Some(dir);
    }

    /// Remove the pending file and any decision files left by
    /// a previous process. Called by the hub at start: leases
    /// do not survive a restart (§7.2), and neither does a
    /// pending offer — the sender sees the TTL expire.
    pub fn clear_stale_pending_files(&self) {
        if let Some(path) = self.pending_file_path() {
            let _ = std::fs::remove_file(path);
        }
        if let Some(dir) = self.decisions_dir() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    fn pending_dir_opt(&self) -> Option<PathBuf> {
        self.pending_dir
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// `<data-dir>/pending-handoffs.json`, or None when no dir
    /// is set (tests).
    pub fn pending_file_path(&self) -> Option<PathBuf> {
        self.pending_dir_opt()
            .map(|dir| dir.join("pending-handoffs.json"))
    }

    /// `<data-dir>/handoff-decisions/`, or None when no dir is
    /// set (tests).
    pub fn decisions_dir(&self) -> Option<PathBuf> {
        self.pending_dir_opt()
            .map(|dir| dir.join("handoff-decisions"))
    }

    /// Store a verified package as pending (§7.3): no lease is
    /// taken and no accept is sent until the receiver's user
    /// decides. Re-offering the same package replaces the
    /// stored copy.
    pub fn store_pending(&self, package: &HandoffPackage) {
        let received_ms = self.clock.now_ms();
        {
            let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
            pending.insert(
                package.package_id.clone(),
                PendingHandoff {
                    package: package.clone(),
                    received_ms,
                },
            );
        }
        self.audit(
            "handoff_pending",
            serde_json::json!({
                "package_id": package.package_id,
                "from_device_id": package.from_device_id,
            }),
        );
        self.write_pending_file();
    }

    /// Snapshot of pending packages (oldest first).
    pub fn pending_handoffs(&self) -> Vec<PendingHandoff> {
        let mut out = self.pending_snapshot();
        out.sort_by_key(|item| item.received_ms);
        out
    }

    fn pending_snapshot(&self) -> Vec<PendingHandoff> {
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .cloned()
            .collect()
    }

    /// Take a pending package out of the queue (user decided).
    pub fn take_pending(&self, package_id: &str) -> Option<PendingHandoff> {
        let taken = self
            .pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(package_id);
        if taken.is_some() {
            self.write_pending_file();
        }
        taken
    }

    /// Drop pending packages whose lease the receiver would
    /// already consider expired (§7.3 step 5, conservative:
    /// expired once now + 2 minutes reaches `expires_ms`).
    /// Returns the dropped packages so the hub can tell the
    /// sender with a `handoff_reject` (reason `expired`) when
    /// its session is still live.
    pub fn sweep_expired_pending(&self) -> Vec<PendingHandoff> {
        let now = self.clock.now_ms();
        let mut expired = Vec::new();
        let changed = {
            let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
            let before = pending.len();
            pending.retain(|_, item| {
                if now + HANDOFF_CLOCK_SKEW_MS >= item.package.expires_ms {
                    expired.push(item.clone());
                    false
                } else {
                    true
                }
            });
            pending.len() != before
        };
        for item in &expired {
            self.audit(
                "handoff_pending_expired",
                serde_json::json!({"package_id": item.package.package_id}),
            );
        }
        if changed {
            self.write_pending_file();
        }
        expired
    }

    /// Remember that the sender's hub reported this package
    /// rejected, so the sender side can tell the user the lease
    /// is back (§7.2).
    pub fn mark_rejected(&self, package_id: &str) {
        self.rejected
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(package_id.to_string());
    }

    pub fn was_rejected(&self, package_id: &str) -> bool {
        self.rejected
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains(package_id)
    }

    /// Mirror the pending set to `<data-dir>/pending-handoffs.json`
    /// (temp file + rename, so a reader never sees a torn
    /// file). Best-effort: a failed write must never break the
    /// hub — the in-memory set is the truth.
    fn write_pending_file(&self) {
        let Some(path) = self.pending_file_path() else {
            return;
        };
        let snapshot = self.pending_snapshot();
        let Ok(bytes) = serde_json::to_vec(&snapshot) else {
            return;
        };
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, bytes).is_err() {
            return;
        }
        let _ = std::fs::rename(&tmp, &path);
    }

    /// Read the pending file (the CLI `handoff list` command).
    /// Missing or unparsable → empty.
    pub fn read_pending_file(&self) -> Vec<PendingHandoff> {
        let Some(path) = self.pending_file_path() else {
            return Vec::new();
        };
        let Ok(bytes) = std::fs::read(path) else {
            return Vec::new();
        };
        serde_json::from_slice(&bytes).unwrap_or_default()
    }

    /// Record a user decision for the hub sweeper to pick up
    /// (the CLI `handoff accept|reject` commands). One file
    /// per decision, temp + rename.
    pub fn write_decision(&self, decision: &HandoffDecision) -> Result<(), MimintError> {
        let Some(dir) = self.decisions_dir() else {
            return Err(MimintError::Config(
                "no pending dir configured for handoff decisions".to_string(),
            ));
        };
        std::fs::create_dir_all(&dir).map_err(|err| {
            MimintError::Io(format!("cannot create handoff decisions dir: {err}"))
        })?;
        let path = dir.join(format!("{}.json", decision.package_id));
        let bytes = serde_json::to_vec(decision).map_err(|err| {
            MimintError::Config(format!("cannot serialize handoff decision: {err}"))
        })?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &bytes)
            .map_err(|err| MimintError::Io(format!("cannot write handoff decision: {err}")))?;
        std::fs::rename(&tmp, &path)
            .map_err(|err| MimintError::Io(format!("cannot store handoff decision: {err}")))?;
        Ok(())
    }

    /// Take every decision file (parsed or not — an
    /// unparsable file is dropped + audited so it cannot
    /// poison the queue forever).
    pub fn take_decision_files(&self) -> Vec<HandoffDecision> {
        let Some(dir) = self.decisions_dir() else {
            return Vec::new();
        };
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let parsed: Option<HandoffDecision> = std::fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok());
            let _ = std::fs::remove_file(&path);
            match parsed {
                Some(decision) => out.push(decision),
                None => self.audit(
                    "handoff_decision_unparsable",
                    serde_json::json!({"file": path.display().to_string()}),
                ),
            }
        }
        out
    }
}

/// The canonical bytes a package signature covers: the package
/// serialized with an empty signature field.
pub fn canonical_package_bytes(package: &HandoffPackage) -> Result<Vec<u8>, MimintError> {
    let mut unsigned = package.clone();
    unsigned.sig = String::new();
    serde_json::to_vec(&unsigned)
        .map_err(|err| MimintError::Config(format!("cannot serialize handoff package: {err}")))
}

/// Shared-notebook context for a package: shared notes only,
/// titles + bodies, capped at 8 KiB. Private notes never enter
/// a package.
pub fn shared_notebook_context(notebook: &Notebook) -> Result<String, MimintError> {
    let mut out = String::new();
    for (note, _sync_id) in notebook.list_shared_notes_with_sync_ids()? {
        let line = format!("## {}\n{}\n", note.title, note.body);
        if out.len() + line.len() > 8 * 1024 {
            break;
        }
        out.push_str(&line);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::friends::identity::Identity;

    fn test_identity() -> Identity {
        Identity::generate()
    }

    fn package(manager: &HandoffManager, identity: &Identity, to: &str) -> HandoffPackage {
        manager
            .build_package(
                identity,
                to,
                "Write the report".to_string(),
                vec!["step 1 done".to_string()],
                "step 2".to_string(),
                vec!["notes.md".to_string()],
                vec![],
                "shared context".to_string(),
                "default",
            )
            .unwrap()
    }

    #[test]
    fn handoff_package_signs_and_verifies() {
        let manager = HandoffManager::new(Clock::fixed(5000), None);
        let identity = test_identity();
        let pkg = package(&manager, &identity, "device-b");
        assert!(manager
            .verify_package(&pkg, identity.public_key().as_slice(), "device-b")
            .is_ok());
        // Wrong recipient.
        assert!(manager
            .verify_package(&pkg, identity.public_key().as_slice(), "device-c")
            .is_err());
        // Tampered task text breaks the signature.
        let mut forged = pkg.clone();
        forged.task_text = "Do something else".to_string();
        assert!(manager
            .verify_package(&forged, identity.public_key().as_slice(), "device-b")
            .is_err());
        // No secret fields in the serialized package.
        let json = serde_json::to_string(&pkg).unwrap();
        for forbidden in ["api_key", "vault", "secret", "password", "token"] {
            assert!(!json.contains(forbidden), "package contains {forbidden}");
        }
    }

    #[test]
    fn handoff_clock_skew_beyond_tolerance_rejected() {
        let manager = HandoffManager::new(Clock::fixed(5000), None);
        let identity = test_identity();
        let mut pkg = package(&manager, &identity, "device-b");
        // A package "created" 3 minutes in the future.
        pkg.created_ms = 5000 + 3 * 60 * 1000;
        pkg.expires_ms = pkg.created_ms + HANDOFF_LEASE_TTL_MS;
        let canonical = canonical_package_bytes(&pkg).unwrap();
        let signature = identity.sign(&canonical).unwrap();
        pkg.sig = hex_encode(&signature);
        assert!(manager
            .verify_package(&pkg, identity.public_key().as_slice(), "device-b")
            .is_err());
    }

    #[test]
    fn handoff_lease_conflict_and_release() {
        let manager = HandoffManager::new(Clock::fixed(5000), None);
        let identity = test_identity();
        let pkg1 = package(&manager, &identity, "device-b");
        let pkg2 = package(&manager, &identity, "device-b");
        let lease = manager.acquire_lease(&pkg1).unwrap();
        assert!(manager.acquire_lease(&pkg2).is_err());
        assert!(manager.lease_for("default").is_some());
        assert!(manager.release_lease("default", &lease.lease_id));
        assert!(manager.acquire_lease(&pkg2).is_ok());
    }
}
