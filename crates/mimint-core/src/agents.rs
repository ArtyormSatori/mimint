//! Agent identities (Phase 6, §1).
//!
//! - Format: `agent-NNN`, zero-padded to 3 digits, starting at `001`.
//! - `agent-000` is the system/coordinator: records that belong to no
//!   turn (startup, config changes, rebuilds, unattributed stops).
//! - `agent-001` is the default agent: the only agent when the
//!   parallelism flag is off, and the only parent when hierarchy
//!   is on.
//! - `agent-002+` are spawned agents (parallel turns, sub-agents).
//!
//! Ids are allocated centrally by the coordinator (§2.4) from one
//! monotonic counter and are never reused within a process. This
//! module only owns the format, the reserved constants, and
//! validation — allocation lives with the coordinator so no other
//! module can mint an id on its own.

/// The system/coordinator id: records that belong to no agent turn.
pub const AGENT_SYSTEM: &str = "agent-000";
/// The default agent: the only agent when parallelism is off.
pub const AGENT_DEFAULT: &str = "agent-001";

/// Format a 1-based sequence number as an agent id (`1` →
/// `agent-001`). Numbers past 999 keep growing (`1000` →
/// `agent-1000`): the format is a minimum width, not a cap.
pub fn format_agent_id(sequence: u64) -> String {
    format!("agent-{sequence:03}")
}

/// True when `id` is a well-formed agent id: `agent-` followed by
/// at least 3 ASCII digits. The system id (`agent-000`) is
/// well-formed; callers that need a *turn* agent must additionally
/// reject it.
pub fn is_valid_agent_id(id: &str) -> bool {
    let Some(digits) = id.strip_prefix("agent-") else {
        return false;
    };
    digits.len() >= 3 && digits.bytes().all(|b| b.is_ascii_digit())
}

/// True when `id` names a real turn-taking agent (well-formed and
/// not the system id).
pub fn is_turn_agent_id(id: &str) -> bool {
    is_valid_agent_id(id) && id != AGENT_SYSTEM
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_pads_to_three_digits() {
        assert_eq!(format_agent_id(1), "agent-001");
        assert_eq!(format_agent_id(2), "agent-002");
        assert_eq!(format_agent_id(42), "agent-042");
        assert_eq!(format_agent_id(999), "agent-999");
        assert_eq!(format_agent_id(1000), "agent-1000");
    }

    #[test]
    fn validation_accepts_only_the_agent_shape() {
        assert!(is_valid_agent_id("agent-001"));
        assert!(is_valid_agent_id("agent-000"));
        assert!(is_valid_agent_id("agent-1234"));
        assert!(!is_valid_agent_id("agent-01"));
        assert!(!is_valid_agent_id("agent-abc"));
        assert!(!is_valid_agent_id("agent-"));
        assert!(!is_valid_agent_id("agent001"));
        assert!(!is_valid_agent_id(""));
        assert!(!is_valid_agent_id("AGENT-001"));
    }

    #[test]
    fn system_id_is_not_a_turn_agent() {
        assert!(!is_turn_agent_id(AGENT_SYSTEM));
        assert!(is_turn_agent_id(AGENT_DEFAULT));
        assert!(is_turn_agent_id("agent-007"));
    }
}
