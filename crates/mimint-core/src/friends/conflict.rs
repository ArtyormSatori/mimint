//! Concurrent-change detection (§4.4, D-DESIGN7-4).
//!
//! This is the ONLY decision procedure for an incoming record:
//! apply, ignore, keep-local, or conflict. It uses causality
//! data — the local version, its `based_on`, the remote
//! version, its `based_on` / `supersedes_version`, and the
//! `last_seen` baseline — and NEVER compares Lamport numbers to
//! decide. The rejected rule ("equal Lamport → conflict,
//! larger Lamport → update") is last-write-wins in disguise
//! and appears nowhere in this module: search this file for
//! `lamport` and you will find only field plumbing, no
//! ordering comparison.
//!
//! Rules, in order, first match wins (§4.4):
//!
//! 1. Already seen → ignore.
//! 2. A conflict-resolution version superseding local → apply.
//! 3. A successor (remote `based_on` == local version) → apply.
//! 4. Local is the successor (local `based_on` == remote) → keep local.
//! 5. Anything else (including missing history) → conflict.
//!
//! Concurrent if and only if neither side has seen the other's
//! version (per the "seen" definition in §4.4).

use crate::friends::records::SyncRecord;
use crate::friends::registry::{SeenVersion, VersionId};

/// What the local side currently holds for a record, with the
/// causality data the algorithm needs.
#[derive(Debug, Clone)]
pub struct LocalVersion {
    pub version: VersionId,
    pub based_on: Option<VersionId>,
    pub hash: String,
}

/// The algorithm's verdict for one incoming record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictDecision {
    /// The version is already known: ignore, ack, no conflict.
    Ignore,
    /// The remote version is a successor or a resolution:
    /// apply it (overwrite local) and advance `last_seen`.
    Apply,
    /// Local is the successor: keep local, no conflict; the
    /// sender will apply the local version when it sees it.
    KeepLocal,
    /// Concurrent: keep both versions, record a conflict for
    /// the user, do NOT advance `last_seen`.
    Conflict,
}

/// Decide the fate of an incoming record.
///
/// * `local`: what this side holds (`None` when the record does
///   not exist locally and no tombstone covers it — the caller
///   handles the no-local-record and tombstone cases before
///   calling this).
/// * `remote`: the validated incoming record.
/// * `last_seen`: the §4.4 baseline for (peer, sync_id), if any.
pub fn decide(
    local: &LocalVersion,
    remote: &SyncRecord,
    last_seen: Option<&SeenVersion>,
) -> ConflictDecision {
    let remote_version = remote.version();
    let version_and_hash_equal =
        |version: &VersionId, hash: &str| *version == remote_version && hash == remote.body_sha256;

    // Rule 1: already seen.
    if version_and_hash_equal(&local.version, &local.hash) {
        return ConflictDecision::Ignore;
    }
    if let Some(seen) = last_seen {
        if version_and_hash_equal(&seen.version, &seen.hash) {
            return ConflictDecision::Ignore;
        }
    }
    // Rule 2: a resolution superseding exactly the local version.
    if let Some(supersedes) = &remote.supersedes_version {
        if *supersedes == local.version {
            return ConflictDecision::Apply;
        }
    }
    // Rule 3: remote is a successor of local.
    if let Some(based_on) = &remote.based_on {
        if *based_on == local.version {
            return ConflictDecision::Apply;
        }
    }
    // Rule 4: local is a successor of remote.
    if let Some(local_based_on) = &local.based_on {
        if *local_based_on == remote_version {
            return ConflictDecision::KeepLocal;
        }
    }
    // Rule 5: missing history or no causal link → conflict.
    // (Rule 6 in the design is the completeness statement: no
    // other case exists.)
    ConflictDecision::Conflict
}

/// Whether an incoming record is covered by a tombstone (§4.7):
/// its version is the tombstone's version or the version the
/// tombstone was created from. Covered records are dropped
/// silently (they are the deleted past), never resurrected
/// while the tombstone lives.
pub fn covered_by_tombstone(
    record: &SyncRecord,
    tombstone_version: &VersionId,
    tombstone_based_on: &Option<VersionId>,
) -> bool {
    let version = record.version();
    version == *tombstone_version
        || tombstone_based_on
            .as_ref()
            .is_some_and(|base| version == *base)
}

/// Build the resolution version for a conflict (§4.4): a new
/// version with `based_on` = local version and
/// `supersedes_version` = remote version, so the other side
/// applies it under rule 2 and the pair converges.
pub fn resolution_record(
    local: &SyncRecord,
    remote: &SyncRecord,
    new_lamport: u64,
    deciding_device: &str,
    updated_ms: u64,
    resolved_body: Option<serde_json::Value>,
) -> SyncRecord {
    SyncRecord::build(
        local.sync_id.clone(),
        local.record_kind().expect("validated kind"),
        VersionId::new(new_lamport, deciding_device),
        Some(local.version()),
        Some(remote.version()),
        local.origin_device.clone(),
        updated_ms,
        resolved_body,
    )
}

/// Human-readable diff between two record bodies for the
/// conflict screen (§4.4): line diffs for text bodies
/// (SOUL/note/skill content), field diffs for
/// profile/todo/plan. Returns (local_lines, remote_lines,
/// changed_fields) — callers render them.
pub fn diff_bodies(
    kind: &str,
    local: &serde_json::Value,
    remote: &serde_json::Value,
) -> Vec<String> {
    let mut lines = Vec::new();
    match kind {
        "soul_file" | "notebook_note" | "skill" => {
            let local_text = text_of(kind, local);
            let remote_text = text_of(kind, remote);
            let local_set: std::collections::HashSet<&str> = local_text.lines().collect();
            let remote_set: std::collections::HashSet<&str> = remote_text.lines().collect();
            for line in local_text.lines() {
                if !remote_set.contains(line) {
                    lines.push(format!("- {line}"));
                }
            }
            for line in remote_text.lines() {
                if !local_set.contains(line) {
                    lines.push(format!("+ {line}"));
                }
            }
            if lines.is_empty() && local_text != remote_text {
                lines.push(
                    "(nội dung khác nhau nhưng không có dòng nào khác biệt rõ ràng)".to_string(),
                );
            }
            if lines.is_empty() {
                lines.push("(hai bản giống nhau)".to_string());
            }
        }
        _ => {
            let empty = serde_json::Map::new();
            let local_obj = local.as_object().unwrap_or(&empty);
            let remote_obj = remote.as_object().unwrap_or(&empty);
            let mut keys: Vec<&String> = local_obj.keys().chain(remote_obj.keys()).collect();
            keys.sort();
            keys.dedup();
            for key in keys {
                let left = local_obj.get(key);
                let right = remote_obj.get(key);
                if left != right {
                    lines.push(format!(
                        "{key}: {} → {}",
                        left.map_or("—".to_string(), |v| v.to_string()),
                        right.map_or("—".to_string(), |v| v.to_string())
                    ));
                }
            }
            if lines.is_empty() {
                lines.push("(hai bản giống nhau)".to_string());
            }
        }
    }
    lines
}

fn text_of(kind: &str, body: &serde_json::Value) -> String {
    match kind {
        "soul_file" => body
            .get("text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        "notebook_note" => {
            let title = body
                .get("title")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let text = body
                .get("text")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            format!("{title}\n{text}")
        }
        "skill" => body
            .get("skill_json")
            .and_then(|s| s.get("content"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        _ => body.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::friends::records::RecordKind;

    fn record(
        lamport: u64,
        device: &str,
        based_on: Option<VersionId>,
        supersedes: Option<VersionId>,
        text: &str,
    ) -> SyncRecord {
        SyncRecord::build(
            "aabbccddeeff00112233445566778899".to_string(),
            RecordKind::NotebookNote,
            VersionId::new(lamport, device),
            based_on,
            supersedes,
            device.to_string(),
            100,
            Some(serde_json::json!({"title": "T", "text": text, "created_ms": 1, "shared": true})),
        )
    }

    fn local_of(record: &SyncRecord) -> LocalVersion {
        LocalVersion {
            version: record.version(),
            based_on: record.based_on.clone(),
            hash: record.body_sha256.clone(),
        }
    }

    /// Worked example 1 (§4.4): a true successor applies, no
    /// conflict, `last_seen` advances (the caller advances it;
    /// the decision is Apply).
    #[test]
    fn sync_concurrent_detection_successor_applies() {
        let baseline = record(5, "device-a", None, None, "v5");
        let local = local_of(&baseline);
        let seen = SeenVersion {
            version: baseline.version(),
            hash: baseline.body_sha256.clone(),
        };
        let remote = record(6, "device-b", Some(baseline.version()), None, "v6");
        assert_eq!(
            decide(&local, &remote, Some(&seen)),
            ConflictDecision::Apply
        );
    }

    /// Worked example 2 (§4.4): concurrent edits from the same
    /// baseline conflict on both sides; `last_seen` stays.
    #[test]
    fn sync_concurrent_detection_concurrent_conflicts() {
        let baseline = record(5, "device-a", None, None, "v5");
        let local_record = record(6, "device-a", Some(baseline.version()), None, "mine");
        let local = local_of(&local_record);
        let seen = SeenVersion {
            version: baseline.version(),
            hash: baseline.body_sha256.clone(),
        };
        let remote = record(6, "device-b", Some(baseline.version()), None, "theirs");
        assert_eq!(
            decide(&local, &remote, Some(&seen)),
            ConflictDecision::Conflict
        );
        // Symmetric: from the other side's view, also conflict.
        let other_local = local_of(&remote);
        assert_eq!(
            decide(&other_local, &local_record, Some(&seen)),
            ConflictDecision::Conflict
        );
    }

    /// The rejected rule, locked out: a LARGER Lamport never
    /// wins. A (7) vs B (6), both from baseline (5), is still
    /// concurrent — B's edit must not be overwritten.
    #[test]
    fn sync_concurrent_detection_lamport_larger_does_not_win() {
        let baseline = record(5, "device-a", None, None, "v5");
        let first = record(6, "device-a", Some(baseline.version()), None, "edit1");
        let local_record = record(7, "device-a", Some(first.version()), None, "edit2");
        let local = local_of(&local_record);
        let seen = SeenVersion {
            version: baseline.version(),
            hash: baseline.body_sha256.clone(),
        };
        let remote = record(6, "device-b", Some(baseline.version()), None, "theirs");
        assert_eq!(
            decide(&local, &remote, Some(&seen)),
            ConflictDecision::Conflict,
            "Lamport 7 must not overwrite Lamport 6"
        );
    }

    /// Missing history is a conservative conflict (§4.4 rule 5):
    /// no `last_seen` row and differing versions → conflict,
    /// never overwrite, never drop.
    #[test]
    fn sync_concurrent_detection_missing_history_is_conflict() {
        let local_record = record(9, "device-a", None, None, "mine");
        let local = local_of(&local_record);
        let remote = record(4, "device-b", None, None, "theirs");
        assert_eq!(decide(&local, &remote, None), ConflictDecision::Conflict);
        // Even a record missing based_on conflicts.
        let remote_no_base = record(10, "device-b", None, None, "theirs2");
        assert_eq!(
            decide(&local, &remote_no_base, None),
            ConflictDecision::Conflict
        );
    }

    #[test]
    fn decide_ignores_seen_and_keeps_local_successor() {
        let baseline = record(5, "device-a", None, None, "v5");
        let local = local_of(&baseline);
        // Same version + hash → ignore.
        let same = record(5, "device-a", None, None, "v5");
        assert_eq!(decide(&local, &same, None), ConflictDecision::Ignore);
        // Same version, different hash → conflict (not "seen").
        let same_version_other_hash = record(5, "device-a", None, None, "different");
        assert_eq!(
            decide(&local, &same_version_other_hash, None),
            ConflictDecision::Conflict
        );
        // Local is the successor → keep local, no conflict.
        let remote_old = record(5, "device-a", None, None, "v5");
        let local_new = record(6, "device-b", Some(remote_old.version()), None, "v6");
        let local_v = local_of(&local_new);
        assert_eq!(
            decide(&local_v, &remote_old, None),
            ConflictDecision::KeepLocal
        );
        // Resolution superseding local → apply.
        let resolution = record(
            8,
            "device-b",
            Some(local_v.version.clone()),
            Some(local_v.version.clone()),
            "resolved",
        );
        assert_eq!(decide(&local_v, &resolution, None), ConflictDecision::Apply);
    }
}
