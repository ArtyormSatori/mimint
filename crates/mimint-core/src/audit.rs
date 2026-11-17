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
//! TODO (Phase 8): hash-chain each record to the previous one (P0-22).
//! Deliberately not done here; no hashing dependency is added in Phase 1.

use crate::error::MimintError;
use serde::Serialize;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// One audit record.
#[derive(Debug, Clone, Serialize)]
pub struct AuditRecord {
    /// Epoch milliseconds, as an integer.
    pub ts_ms: u64,
    pub turn_id: String,
    /// `decision`, `execution`, `verification`, `reflection`, `turn_end`, ...
    pub kind: String,
    pub tool: Option<String>,
    pub decision: Option<String>,
    pub detail: serde_json::Value,
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
    pub fn log(
        &self,
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
}
