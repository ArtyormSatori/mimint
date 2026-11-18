//! Append-only audit log (JSONL).
//!
//! Every record is one JSON object per line. The file is opened in append
//! mode and never truncated or overwritten by this module.
//!
//! Audit writes FAIL CLOSED: every caller propagates the `Result` with
//! `?`, so a record that cannot be written fails the turn, the guard
//! decision, or the provider attempt it belongs to. Nothing runs
//! unrecorded. This is load-bearing for the guard and egress guarantees,
//! not a logging nicety.
//!
//! Phase 6 (§1.3, D-DESIGN-7): every record carries a first-class
//! `agent_id`. Records written through [`AuditLog::log`] — the
//! system path (startup, config, rebuilds, unattributed stops) —
//! carry `agent-000`. Turn-scoped writers use
//! [`AuditLog::log_for`] with the acting agent. Legacy records
//! written before Phase 6 have no `agent_id` field; they still
//! deserialize, defaulting to `agent-001`, because only the
//! default agent could have produced them.
//!
//! TODO (Phase 8): hash-chain each record to the previous one (P0-22).
//! Deliberately not done here; no hashing dependency is added in Phase 1.

use crate::agents::{AGENT_DEFAULT, AGENT_SYSTEM};
use crate::error::MimintError;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Default for `agent_id` when a legacy record has no such field
/// (D-DESIGN-7): only the default agent could have written it.
fn default_agent_id() -> String {
    AGENT_DEFAULT.to_string()
}

/// One audit record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditRecord {
    /// Epoch milliseconds, as an integer.
    pub ts_ms: u64,
    pub turn_id: String,
    /// `decision`, `execution`, `verification`, `reflection`, `turn_end`, ...
    pub kind: String,
    pub tool: Option<String>,
    pub decision: Option<String>,
    pub detail: serde_json::Value,
    /// The agent that produced this record (`agent-000` = system,
    /// `agent-001` = default agent). Missing in legacy records:
    /// deserializes as `agent-001` (D-DESIGN-7).
    #[serde(default = "default_agent_id")]
    pub agent_id: String,
}

/// Append-only JSONL audit log.
///
/// Internally synchronized so a shared reference can be used by the
/// guard executor and the brain loop within one turn.
pub struct AuditLog {
    path: PathBuf,
    file: Mutex<File>,
}

impl AuditLog {
    /// Open (or create) the audit file in append mode. Never truncates.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, MimintError> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| {
                MimintError::Audit(format!("cannot open audit log {}: {e}", path.display()))
            })?;
        Ok(Self {
            path,
            file: Mutex::new(file),
        })
    }

    /// Path of the audit file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one record and flush.
    pub fn record(&self, record: AuditRecord) -> Result<(), MimintError> {
        let mut line = serde_json::to_string(&record)
            .map_err(|e| MimintError::Audit(format!("cannot serialize audit record: {e}")))?;
        line.push('\n');
        let mut file = match self.file.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        file.write_all(line.as_bytes())
            .map_err(|e| MimintError::Audit(format!("cannot write audit log: {e}")))?;
        file.flush()
            .map_err(|e| MimintError::Audit(format!("cannot flush audit log: {e}")))?;
        Ok(())
    }

    /// Build a record with the current timestamp and append it.
    ///
    /// System path: the record carries `agent-000`. Turn-scoped
    /// writers must use [`AuditLog::log_for`] with the acting
    /// agent instead — see the `audit_record_requires_agent_id`
    /// gate test.
    pub fn log(
        &self,
        turn_id: &str,
        kind: &str,
        tool: Option<&str>,
        decision: Option<&str>,
        detail: serde_json::Value,
    ) -> Result<(), MimintError> {
        self.log_for(AGENT_SYSTEM, turn_id, kind, tool, decision, detail)
    }

    /// Build a record with the current timestamp, attributed to
    /// `agent_id`, and append it. All turn-scoped writers use this.
    pub fn log_for(
        &self,
        agent_id: &str,
        turn_id: &str,
        kind: &str,
        tool: Option<&str>,
        decision: Option<&str>,
        detail: serde_json::Value,
    ) -> Result<(), MimintError> {
        self.record(AuditRecord {
            ts_ms: now_ms(),
            turn_id: turn_id.to_string(),
            kind: kind.to_string(),
            tool: tool.map(str::to_string),
            decision: decision.map(str::to_string),
            detail,
            agent_id: agent_id.to_string(),
        })
    }
}

/// Current time as epoch milliseconds.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// SHA-256 hex of the given bytes, for audit-safe hashing
/// (code hashes, artifact re-hashes). The hashed content itself
/// never enters the audit log; only the hash does.
/// SHA-256 hex of `data`, shared by the audit log, the
/// code-execution verifier, and the approval card (which shows
/// the approved code's hash). Public so the UI crate can hash
/// the same way the executor does.
pub fn sha256_hex(data: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, data);
    digest
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes to `/dev/full` always fail (ENOSPC): the audit log must
    /// surface that as an error, never swallow it. Linux-only: `/dev/full`
    /// does not exist on other platforms.
    #[cfg(target_os = "linux")]
    #[test]
    fn record_fails_closed_when_the_device_refuses_writes() {
        let audit = AuditLog::open("/dev/full").expect("open /dev/full");
        let err = audit
            .log("t", "decision", None, Some("allow"), serde_json::json!({}))
            .expect_err("write to /dev/full must fail");
        assert!(err.to_string().contains("audit"), "{err}");
    }

    /// Single-writer integrity (Phase 3 fix pass F3-23/F3-26): one
    /// shared `AuditLog` written concurrently from many threads
    /// produces exactly one valid JSON record per write, with no
    /// interleaved or torn lines. The `Mutex<File>` inside `record`
    /// is the single-writer mechanism; the app holds exactly one
    /// `Arc<AuditLog>`, and the UI viewer only reads.
    #[test]
    fn concurrent_writes_produce_whole_json_lines_only() {
        use std::sync::Arc;
        let dir =
            std::env::temp_dir().join(format!("mimint-audit-concurrent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("audit.jsonl");
        let audit = Arc::new(AuditLog::open(&path).unwrap());
        let mut handles = Vec::new();
        for t in 0..8 {
            let audit = Arc::clone(&audit);
            handles.push(std::thread::spawn(move || {
                for i in 0..25 {
                    audit
                        .log(
                            "concurrent",
                            "probe",
                            None,
                            None,
                            serde_json::json!({"thread": t, "i": i}),
                        )
                        .unwrap();
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 200, "one line per write");
        for line in &lines {
            let value: serde_json::Value =
                serde_json::from_str(line).expect("every line is whole JSON");
            assert_eq!(value["kind"], "probe");
        }
        assert!(text.ends_with('\n'), "file ends with a newline");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// D-DESIGN-7: a legacy record without `agent_id` still reads,
    /// defaulting to the default agent; a new record round-trips.
    #[test]
    fn audit_legacy_records_readable() {
        let legacy = r#"{"ts_ms":1,"turn_id":"t","kind":"decision","tool":null,"decision":"allow","detail":{}}"#;
        let record: AuditRecord = serde_json::from_str(legacy).expect("legacy record reads");
        assert_eq!(record.agent_id, "agent-001");
        let fresh = r#"{"ts_ms":1,"turn_id":"t","kind":"decision","tool":null,"decision":"allow","detail":{},"agent_id":"agent-003"}"#;
        let record: AuditRecord = serde_json::from_str(fresh).expect("new record reads");
        assert_eq!(record.agent_id, "agent-003");
    }

    /// `log` attributes to the system id; `log_for` attributes to
    /// the given agent. Both write the field: new records must
    /// always carry `agent_id`.
    #[test]
    fn log_and_log_for_attribute_agent_id() {
        let dir = std::env::temp_dir().join(format!("mimint-audit-agent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("audit.jsonl");
        let audit = AuditLog::open(&path).unwrap();
        audit
            .log("t", "decision", None, Some("allow"), serde_json::json!({}))
            .unwrap();
        audit
            .log_for(
                "agent-002",
                "t",
                "decision",
                None,
                Some("allow"),
                serde_json::json!({}),
            )
            .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: AuditRecord = serde_json::from_str(lines[0]).unwrap();
        let second: AuditRecord = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(first.agent_id, "agent-000");
        assert_eq!(second.agent_id, "agent-002");
        assert!(lines[0].contains("\"agent_id\""));
        assert!(lines[1].contains("\"agent_id\""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Gate (§1.4): the turn-scoped writers — the brain loop and
    /// the guard executor — must attribute through `log_for`, never
    /// through the system path `log`. This test reads the crate's
    /// own sources and fails if either module calls the audit
    /// log's system path directly.
    #[test]
    fn audit_record_requires_agent_id() {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        for module in ["brain.rs", "guards.rs"] {
            let path = manifest.join("src").join(module);
            let source =
                std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {module}: {e}"));
            assert!(
                source.contains("log_for("),
                "{module} must attribute audit writes through log_for"
            );
            assert!(
                !source.contains(".audit.log(") && !source.contains("audit.log("),
                "{module} must not write audit records through the system path"
            );
        }
    }
}
