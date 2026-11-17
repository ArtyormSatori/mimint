//! Append-only audit log (JSONL).
//!
//! Every record is one JSON object per line. The file is opened in append
//! mode and never truncated or overwritten by this module.
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
