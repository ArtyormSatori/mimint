//! Cross-device approval relay (Phase 7, §6).
//!
//! A card is created LOCALLY on the device running the agent,
//! relayed to ONE paired peer, and answered with a decision
//! bound to the triple (device_id, agent_id, request_id).
//! The binding is checked on every decision; a decision is
//! single-use; expired cards deny; a peer can never create a
//! card (inbound `approval_card` messages are refused by the
//! session dispatcher — see [`crate::friends::hub`]).
//!
//! Peer approval is OFF by default, per peer
//! ([`PeerPrefs::approve_allowed`]). The relay refuses to
//! create a card when the peer has not opted in. Audit
//! attribution lives inside the `detail` object — no new
//! first-class audit field.

use crate::audit::AuditLog;
use crate::error::MimintError;
use crate::friends::util::Clock;
use crate::friends::wire::PROTOCOL_VERSION;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

/// Approval timeout (§6): 60 seconds.
pub const APPROVAL_TIMEOUT_MS: u64 = 60_000;

/// An approval card, relayed to the peer for a decision. The
/// origin fields are shown in full on the peer's card (§6.4).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalCard {
    pub v: u32,
    pub kind: String,
    /// Origin device (the device running the agent).
    pub device_id: String,
    pub device_name: String,
    pub agent_id: String,
    pub request_id: String,
    pub action: String,
    /// Action detail (human-readable + structured, no secrets).
    pub detail: serde_json::Value,
    pub created_ms: u64,
    pub expires_ms: u64,
}

/// A decision from the peer. Bound to the card's triple.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalDecision {
    pub v: u32,
    pub kind: String,
    /// The ORIGIN device id (must match the card's).
    pub device_id: String,
    pub agent_id: String,
    pub request_id: String,
    /// `approve` or `deny` — nothing else.
    pub decision: String,
    pub decided_ms: u64,
}

/// The outcome delivered to the waiter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionOutcome {
    Approved,
    Denied,
}

/// What created the card (origin attribution).
#[derive(Debug, Clone)]
pub struct CardOrigin {
    pub device_id: String,
    pub device_name: String,
    pub agent_id: String,
    pub request_id: String,
    pub action: String,
    pub detail: serde_json::Value,
}

struct PendingCard {
    card: ApprovalCard,
    target_peer: String,
    sender: Sender<DecisionOutcome>,
    consumed: bool,
}

/// Display-name lookup for a deciding peer, used only
/// for the `deciding_device_name` audit field (§6.3).
pub type PeerNameLookup = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// The relay: pending cards + decision verification.
pub struct ApprovalRelay {
    pending: Mutex<HashMap<String, PendingCard>>,
    clock: Clock,
    audit: Option<Arc<AuditLog>>,
    /// Optional display-name lookup for the deciding peer
    /// (registry-backed in production). Used only for the
    /// `deciding_device_name` audit field (§6.3).
    peer_name_lookup: Option<PeerNameLookup>,
}

impl ApprovalRelay {
    pub fn new(clock: Clock, audit: Option<Arc<AuditLog>>) -> Self {
        Self::new_with_lookup(clock, audit, None)
    }

    pub fn new_with_lookup(
        clock: Clock,
        audit: Option<Arc<AuditLog>>,
        peer_name_lookup: Option<PeerNameLookup>,
    ) -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            clock,
            audit,
            peer_name_lookup,
        }
    }

    /// Create a card for relay to `target_peer`. Refuses when
    /// the peer has not opted in (`approve_allowed == false`)
    /// and when a card with the same request id is pending.
    /// Returns the card plus a receiver for the decision.
    pub fn create_card(
        &self,
        origin: CardOrigin,
        target_peer: &str,
        approve_allowed: bool,
    ) -> Result<(ApprovalCard, Receiver<DecisionOutcome>), MimintError> {
        if !approve_allowed {
            return Err(MimintError::Config(format!(
                "peer approval is not enabled for peer {target_peer}"
            )));
        }
        let now = self.clock.now_ms();
        let card = ApprovalCard {
            v: PROTOCOL_VERSION,
            kind: "approval_card".to_string(),
            device_id: origin.device_id.clone(),
            device_name: origin.device_name,
            agent_id: origin.agent_id,
            request_id: origin.request_id.clone(),
            action: origin.action,
            detail: origin.detail,
            created_ms: now,
            expires_ms: now + APPROVAL_TIMEOUT_MS,
        };
        let (sender, receiver) = channel();
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        if pending.contains_key(&card.request_id) {
            return Err(MimintError::Config(format!(
                "an approval card for request {} is already pending",
                card.request_id
            )));
        }
        pending.insert(
            card.request_id.clone(),
            PendingCard {
                card: card.clone(),
                target_peer: target_peer.to_string(),
                sender,
                consumed: false,
            },
        );
        Ok((card, receiver))
    }

    /// Apply an inbound decision from `from_peer`. Verifies,
    /// in order: the card exists, the triple binds, the
    /// decision came from the peer the card went to, the card
    /// is unused, and the card is unexpired. Any failure
    /// rejects the decision (the action stays unapproved).
    /// A valid decision consumes the card (single use).
    pub fn apply_decision(
        &self,
        decision: &ApprovalDecision,
        from_peer: &str,
    ) -> Result<DecisionOutcome, MimintError> {
        if decision.decision != "approve" && decision.decision != "deny" {
            return Err(MimintError::Config(format!(
                "approval decision '{}' is outside the allow-list",
                decision.decision
            )));
        }
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        let entry = pending.get_mut(&decision.request_id).ok_or_else(|| {
            MimintError::Config(format!(
                "no pending approval card for request {}",
                decision.request_id
            ))
        })?;
        // Triple binding: (device_id, agent_id, request_id).
        if entry.card.device_id != decision.device_id || entry.card.agent_id != decision.agent_id {
            return Err(MimintError::Config(
                "approval decision does not bind to the pending card".to_string(),
            ));
        }
        // Cross-device replay resistance: the decision must
        // come from the peer the card was sent to.
        if entry.target_peer != from_peer {
            return Err(MimintError::Config(
                "approval decision came from a different peer than the card target".to_string(),
            ));
        }
        if entry.consumed {
            return Err(MimintError::Config(
                "approval card was already decided (single use)".to_string(),
            ));
        }
        let now = self.clock.now_ms();
        if now > entry.card.expires_ms {
            entry.consumed = true;
            let _ = entry.sender.send(DecisionOutcome::Denied);
            self.audit_decision(&entry.card, from_peer, "expired");
            return Err(MimintError::Config(
                "approval card expired; the action is denied".to_string(),
            ));
        }
        entry.consumed = true;
        let outcome = if decision.decision == "approve" {
            DecisionOutcome::Approved
        } else {
            DecisionOutcome::Denied
        };
        let _ = entry.sender.send(outcome);
        self.audit_decision(&entry.card, from_peer, &decision.decision);
        Ok(outcome)
    }

    /// Cancel a pending card (the waiter timed out locally).
    /// Returns true when a card was pending.
    pub fn cancel(&self, request_id: &str) -> bool {
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        pending.remove(request_id).is_some()
    }

    /// Pending (undecided, unexpired) cards for one peer, for
    /// hub delivery (§6.2).
    pub fn pending_cards_for(&self, target_peer: &str) -> Vec<ApprovalCard> {
        let now = self.clock.now_ms();
        let pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        pending
            .values()
            .filter(|entry| {
                entry.target_peer == target_peer && !entry.consumed && now <= entry.card.expires_ms
            })
            .map(|entry| entry.card.clone())
            .collect()
    }

    /// Pending (undecided, unexpired) cards, for the UI.
    pub fn pending_cards(&self) -> Vec<ApprovalCard> {
        let now = self.clock.now_ms();
        let pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        pending
            .values()
            .filter(|entry| !entry.consumed && now <= entry.card.expires_ms)
            .map(|entry| entry.card.clone())
            .collect()
    }

    /// Pending cards with the peer that must decide them
    /// (F7-13): the panel shows both the hub that created the
    /// card (the origin) and the responding device, so the user
    /// never has to guess who is being asked.
    pub fn pending_cards_detailed(&self) -> Vec<(ApprovalCard, String)> {
        let now = self.clock.now_ms();
        let pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        pending
            .values()
            .filter(|entry| !entry.consumed && now <= entry.card.expires_ms)
            .map(|entry| (entry.card.clone(), entry.target_peer.clone()))
            .collect()
    }

    /// Drop expired cards, delivering Denied to their waiters.
    /// Returns the expired request ids.
    pub fn expire_stale(&self) -> Vec<String> {
        let now = self.clock.now_ms();
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        let mut expired = Vec::new();
        let ids: Vec<String> = pending.keys().cloned().collect();
        for id in ids {
            let stale = pending
                .get(&id)
                .is_some_and(|entry| !entry.consumed && now > entry.card.expires_ms);
            if stale {
                if let Some(entry) = pending.remove(&id) {
                    let _ = entry.sender.send(DecisionOutcome::Denied);
                    self.audit_decision(&entry.card, &entry.target_peer, "expired");
                    expired.push(id);
                }
            }
        }
        expired
    }

    /// Audit with full attribution INSIDE `detail` (§6.2): no
    /// new first-class field, the origin triple travels in the
    /// detail object.
    fn audit_decision(&self, card: &ApprovalCard, from_peer: &str, decision: &str) {
        if let Some(log) = &self.audit {
            let _ = log.log(
                "friends-approval",
                "peer_approval_decision",
                Some("friends"),
                None,
                serde_json::json!({
                    "origin_device_id": card.device_id,
                    "origin_device_name": card.device_name,
                    "origin_agent_id": card.agent_id,
                    "origin_request_id": card.request_id,
                    // §6.3 attribution: who decided, by name,
                    // and on which channel. The name falls back
                    // to the device id when no lookup is set.
                    "deciding_device_id": from_peer,
                    "deciding_device_name": self
                        .peer_name_lookup
                        .as_ref()
                        .and_then(|lookup| lookup(from_peer))
                        .unwrap_or_else(|| from_peer.to_string()),
                    "decision_channel": "peer",
                    "action": card.action,
                    "decision": decision,
                }),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin(request_id: &str) -> CardOrigin {
        CardOrigin {
            device_id: "device-a".to_string(),
            device_name: "Máy A".to_string(),
            agent_id: "agent-001".to_string(),
            request_id: request_id.to_string(),
            action: "run shell".to_string(),
            detail: serde_json::json!({"command": "ls"}),
        }
    }

    fn decision(request_id: &str, value: &str) -> ApprovalDecision {
        ApprovalDecision {
            v: PROTOCOL_VERSION,
            kind: "approval_decision".to_string(),
            device_id: "device-a".to_string(),
            agent_id: "agent-001".to_string(),
            request_id: request_id.to_string(),
            decision: value.to_string(),
            decided_ms: 1000,
        }
    }

    #[test]
    fn approve_from_peer_runs_action() {
        let relay = ApprovalRelay::new(Clock::fixed(1000), None);
        let (card, rx) = relay
            .create_card(origin("req-1"), "device-b", true)
            .unwrap();
        assert_eq!(card.expires_ms, 1000 + APPROVAL_TIMEOUT_MS);
        assert!(card.device_name == "Máy A" && !card.action.is_empty());
        let outcome = relay
            .apply_decision(&decision("req-1", "approve"), "device-b")
            .unwrap();
        assert_eq!(outcome, DecisionOutcome::Approved);
        assert_eq!(rx.try_recv().unwrap(), DecisionOutcome::Approved);
    }

    #[test]
    fn deny_from_peer_blocks_action() {
        let relay = ApprovalRelay::new(Clock::fixed(1000), None);
        let (_card, rx) = relay
            .create_card(origin("req-1"), "device-b", true)
            .unwrap();
        assert_eq!(
            relay
                .apply_decision(&decision("req-1", "deny"), "device-b")
                .unwrap(),
            DecisionOutcome::Denied
        );
        assert_eq!(rx.try_recv().unwrap(), DecisionOutcome::Denied);
    }

    #[test]
    fn approve_replay_rejected() {
        let relay = ApprovalRelay::new(Clock::fixed(1000), None);
        let _ = relay
            .create_card(origin("req-1"), "device-b", true)
            .unwrap();
        relay
            .apply_decision(&decision("req-1", "approve"), "device-b")
            .unwrap();
        // The same decision again: single use.
        assert!(relay
            .apply_decision(&decision("req-1", "approve"), "device-b")
            .is_err());
    }

    #[test]
    fn approve_cross_agent_rejected() {
        let relay = ApprovalRelay::new(Clock::fixed(1000), None);
        let _ = relay
            .create_card(origin("req-1"), "device-b", true)
            .unwrap();
        let mut forged = decision("req-1", "approve");
        forged.agent_id = "agent-999".to_string();
        assert!(relay.apply_decision(&forged, "device-b").is_err());
        // The card is still pending and decidable by the real triple.
        assert!(relay
            .apply_decision(&decision("req-1", "approve"), "device-b")
            .is_ok());
    }

    #[test]
    fn approve_cross_device_replay_rejected() {
        let relay = ApprovalRelay::new(Clock::fixed(1000), None);
        let _ = relay
            .create_card(origin("req-1"), "device-b", true)
            .unwrap();
        // A decision arriving from a different peer is refused,
        // even with the right triple.
        assert!(relay
            .apply_decision(&decision("req-1", "approve"), "device-c")
            .is_err());
    }

    #[test]
    fn approve_after_expiry_denied() {
        let (clock, cell) = Clock::manual(1000);
        let relay = ApprovalRelay::new(clock, None);
        let (_card, rx) = relay
            .create_card(origin("req-1"), "device-b", true)
            .unwrap();
        cell.fetch_add(APPROVAL_TIMEOUT_MS + 1, std::sync::atomic::Ordering::SeqCst);
        assert!(relay
            .apply_decision(&decision("req-1", "approve"), "device-b")
            .is_err());
        assert_eq!(rx.try_recv().unwrap(), DecisionOutcome::Denied);
    }

    #[test]
    fn approve_requires_explicit_peer_opt_in() {
        let relay = ApprovalRelay::new(Clock::fixed(1000), None);
        assert!(relay
            .create_card(origin("req-1"), "device-b", false)
            .is_err());
        assert!(relay.pending_cards().is_empty());
    }

    #[test]
    fn approve_no_batch() {
        let relay = ApprovalRelay::new(Clock::fixed(1000), None);
        let _ = relay
            .create_card(origin("req-1"), "device-b", true)
            .unwrap();
        let (_card2, rx2) = relay
            .create_card(origin("req-2"), "device-b", true)
            .unwrap();
        // One decision applies to exactly one card.
        relay
            .apply_decision(&decision("req-1", "approve"), "device-b")
            .unwrap();
        assert_eq!(relay.pending_cards().len(), 1);
        assert_eq!(relay.pending_cards()[0].request_id, "req-2");
        assert!(rx2.try_recv().is_err());
    }

    #[test]
    fn approve_card_shows_full_origin() {
        let relay = ApprovalRelay::new(Clock::fixed(1000), None);
        let (card, _rx) = relay
            .create_card(origin("req-1"), "device-b", true)
            .unwrap();
        let json = serde_json::to_value(&card).unwrap();
        for field in [
            "device_id",
            "device_name",
            "agent_id",
            "request_id",
            "action",
            "detail",
            "created_ms",
            "expires_ms",
        ] {
            assert!(json.get(field).is_some(), "card missing {field}");
        }
    }
}
