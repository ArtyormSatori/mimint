//! Per-agent approval snapshot slots (Phase 6, §3).
//!
//! The single [`TargetSnapshotSlot`] of Phase 4 becomes one
//! slot per agent: the approver stores the approval-time
//! snapshot in the acting agent's slot, and that agent's
//! body tools consume it at execution. Slots are created on
//! demand and removed when the agent is reaped, so a
//! snapshot can never authorize a different agent's action
//! — or a later incarnation of the same id (ids are never
//! reused within a process, and the slot is gone at reap
//! regardless).

use mimint_body::TargetSnapshotSlot;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// One approval snapshot slot per agent.
#[derive(Debug, Default)]
pub struct AgentSnapshotSlots {
    slots: Mutex<HashMap<String, Arc<TargetSnapshotSlot>>>,
}

impl AgentSnapshotSlots {
    pub fn new() -> Self {
        Self::default()
    }

    /// The slot for `agent_id`, creating it on first use.
    /// The same `Arc` is returned for repeat calls, so the
    /// approver and the agent's tools share one slot.
    pub fn slot_for(&self, agent_id: &str) -> Arc<TargetSnapshotSlot> {
        let mut slots = self.slots.lock().unwrap_or_else(|p| p.into_inner());
        Arc::clone(
            slots
                .entry(agent_id.to_string())
                .or_insert_with(|| Arc::new(TargetSnapshotSlot::new())),
        )
    }

    /// Remove the agent's slot (reap). Any snapshot it held
    /// is dropped with it.
    pub fn remove(&self, agent_id: &str) {
        self.slots
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(agent_id);
    }

    /// How many slots exist (agents that ever had a slot and
    /// have not been reaped).
    pub fn len(&self) -> usize {
        self.slots.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_slot_reaped_with_agent() {
        let slots = AgentSnapshotSlots::new();
        assert!(slots.is_empty());
        let a = slots.slot_for("agent-001");
        let b = slots.slot_for("agent-002");
        assert!(!Arc::ptr_eq(&a, &b));
        assert!(Arc::ptr_eq(&a, &slots.slot_for("agent-001")));
        assert_eq!(slots.len(), 2);
        slots.remove("agent-001");
        assert_eq!(slots.len(), 1);
        // A fresh slot after removal: no snapshot survives.
        let a2 = slots.slot_for("agent-001");
        assert!(!Arc::ptr_eq(&a, &a2));
    }
}
