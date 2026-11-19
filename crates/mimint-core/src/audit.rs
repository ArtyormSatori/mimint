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
//! Phase 8.1 (P8.1 audit, §4.2): every record is hash-chained to
//! the previous one (P0-22). Each record carries `prev_hash`
//! (SHA-256 hex of the previous record's hash; the first record
//! uses 64 zeroes) and `hash` (SHA-256 hex of this record's
//! canonical payload, which includes `prev_hash`). A hand-edited
//! line breaks the chain: [`verify_audit_chain`] recomputes every
//! hash and reports the first bad line. Records written before
//! P8.1 have neither field; the chain starts from the SHA-256 of
//! the last legacy line's raw bytes, so an old log stays
//! verifiable without rewriting history. The hash uses `ring`,
//! which was already a dependency — no new package was added
//! for the chain.

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

/// `prev_hash` of the first chained record in a log: 64 zeroes.
/// A legacy line has no hash, so the chain can also start from
/// the SHA-256 of a legacy line's raw bytes (see
/// [`verify_audit_chain`]).
pub const GENESIS_PREV_HASH: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

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
    /// SHA-256 hex of the previous record's `hash` (or
    /// [`GENESIS_PREV_HASH`] for the first chained record, or the
    /// SHA-256 of the last legacy line's raw bytes when the chain
    /// starts after pre-chain records). Empty on records written
    /// before P8.1.
    #[serde(default)]
    pub prev_hash: String,
    /// SHA-256 hex of this record's canonical chain payload (see
    /// [`ChainPayload`]). Empty on records written before P8.1.
    #[serde(default)]
    pub hash: String,
}

/// The canonical payload a record's chain hash covers: every
/// field of [`AuditRecord`] except `hash` itself, serialized in
/// declaration order. `detail` is a `serde_json::Value`, whose
/// object keys serialize sorted, so the payload is
/// deterministic for a given record.
#[derive(Serialize)]
struct ChainPayload<'a> {
    ts_ms: u64,
    turn_id: &'a str,
    kind: &'a str,
    tool: Option<&'a str>,
    decision: Option<&'a str>,
    detail: &'a serde_json::Value,
    agent_id: &'a str,
    prev_hash: &'a str,
}

/// SHA-256 hex of a record's canonical payload under `prev_hash`.
fn chain_hash(record: &AuditRecord, prev_hash: &str) -> String {
    let payload = ChainPayload {
        ts_ms: record.ts_ms,
        turn_id: &record.turn_id,
        kind: &record.kind,
        tool: record.tool.as_deref(),
        decision: record.decision.as_deref(),
        detail: &record.detail,
        agent_id: &record.agent_id,
        prev_hash,
    };
    let json = serde_json::to_string(&payload).expect("chain payload must serialize");
    sha256_hex(json.as_bytes())
}

/// Outcome of [`verify_audit_chain`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainVerifyReport {
    /// Lines that parsed as records (blank lines are skipped).
    pub records: usize,
    /// Records carrying a hash that checked out.
    pub chained: usize,
    /// Records written before P8.1 (no hash fields).
    pub legacy: usize,
}

/// Verify a whole audit file's hash chain.
///
/// Every chained record's `prev_hash` must equal the previous
/// record's `hash` (or [`GENESIS_PREV_HASH`] for the first line,
/// or the SHA-256 of the last legacy line's raw bytes when the
/// chain starts after pre-chain records), and its `hash` must
/// equal the SHA-256 of its canonical payload. The first line
/// that breaks either check fails the whole verification with
/// the line number and the reason — a hand-edited line is
/// detected, not silently accepted. Legacy lines (no hash
/// fields) are counted, not rejected: they cannot be proven
/// unmodified, only chained over.
pub fn verify_audit_chain(path: impl AsRef<Path>) -> Result<ChainVerifyReport, MimintError> {
    let path = path.as_ref();
    let text = std::fs::read_to_string(path).map_err(|e| {
        MimintError::Audit(format!("cannot read audit log {}: {e}", path.display()))
    })?;
    let mut expected_prev = GENESIS_PREV_HASH.to_string();
    let mut report = ChainVerifyReport {
        records: 0,
        chained: 0,
        legacy: 0,
    };
    for (index, line) in text.lines().enumerate() {
        let lineno = index + 1;
        if line.trim().is_empty() {
            continue;
        }
        report.records += 1;
        let record: AuditRecord = serde_json::from_str(line).map_err(|e| {
            MimintError::Audit(format!(
                "audit chain: line {lineno} is not a valid record: {e}"
            ))
        })?;
        if record.hash.is_empty() {
            report.legacy += 1;
            expected_prev = sha256_hex(line.as_bytes());
            continue;
        }
        if record.prev_hash != expected_prev {
            return Err(MimintError::Audit(format!(
                "audit chain: line {lineno} prev_hash mismatch (expected {expected_prev}, found {})",
                record.prev_hash
            )));
        }
        let recomputed = chain_hash(&record, &record.prev_hash);
        if recomputed != record.hash {
            return Err(MimintError::Audit(format!(
                "audit chain: line {lineno} hash mismatch (record was modified after it was written)"
            )));
        }
        report.chained += 1;
        expected_prev = record.hash.clone();
    }
    Ok(report)
}

/// Append-only JSONL audit log.
///
/// Internally synchronized so a shared reference can be used by the
/// guard executor and the brain loop within one turn.
pub struct AuditLog {
    path: PathBuf,
    file: Mutex<File>,
    /// `hash` of the last chained record written to this log (or
    /// the SHA-256 of the last legacy line, or
    /// [`GENESIS_PREV_HASH`] when the log is new). Updated under
    /// the file lock in [`AuditLog::record`], so the chain order
    /// matches the line order in the file.
    last_hash: Mutex<String>,
}

/// Seed for a log's chain head: the `hash` of the last chained
/// record already in the file, the SHA-256 of the last legacy
/// line's raw bytes, or [`GENESIS_PREV_HASH`] when the file does
/// not exist, is empty, or cannot be read. Only regular files
/// are read here: device paths such as `/dev/full` are never
/// opened for reading (a read would block or fail), and a log
/// that cannot be read still opens for append with the genesis
/// seed rather than failing the whole startup.
fn seed_chain_head(path: &Path) -> String {
    let is_file = std::fs::metadata(path)
        .map(|m| m.is_file())
        .unwrap_or(false);
    if !is_file {
        return GENESIS_PREV_HASH.to_string();
    }
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(_) => return GENESIS_PREV_HASH.to_string(),
    };
    for line in text.lines().rev() {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        if let Ok(record) = serde_json::from_str::<AuditRecord>(line) {
            if !record.hash.is_empty() {
                return record.hash;
            }
        }
        return sha256_hex(line.as_bytes());
    }
    GENESIS_PREV_HASH.to_string()
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
        let last_hash = seed_chain_head(&path);
        Ok(Self {
            path,
            file: Mutex::new(file),
            last_hash: Mutex::new(last_hash),
        })
    }

    /// Path of the audit file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one record and flush. The record is hash-chained
    /// to the previous one before it is written: `prev_hash` is
    /// the chain head and `hash` covers this record's canonical
    /// payload. A write failure leaves the chain head unchanged,
    /// so a retried record chains onto the same predecessor.
    pub fn record(&self, mut record: AuditRecord) -> Result<(), MimintError> {
        let mut file = match self.file.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut last = match self.last_hash.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        record.prev_hash = last.clone();
        record.hash = chain_hash(&record, &record.prev_hash);
        let mut line = serde_json::to_string(&record)
            .map_err(|e| MimintError::Audit(format!("cannot serialize audit record: {e}")))?;
        line.push('\n');
        file.write_all(line.as_bytes())
            .map_err(|e| MimintError::Audit(format!("cannot write audit log: {e}")))?;
        file.flush()
            .map_err(|e| MimintError::Audit(format!("cannot flush audit log: {e}")))?;
        *last = record.hash.clone();
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
            prev_hash: String::new(),
            hash: String::new(),
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

    fn chain_temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "mimint-audit-chain-{}-{nanos}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// P8.1 (§4.2): three records written through `log` verify as
    /// one chain: the first chains from the genesis hash, each
    /// next chains from the previous record's hash.
    #[test]
    fn audit_chain_roundtrip_and_verify() {
        let dir = chain_temp_dir("roundtrip");
        let path = dir.join("audit.jsonl");
        let audit = AuditLog::open(&path).unwrap();
        for i in 0..3 {
            audit
                .log(
                    "t",
                    "decision",
                    None,
                    Some("allow"),
                    serde_json::json!({"i": i}),
                )
                .unwrap();
        }
        drop(audit);
        let report = verify_audit_chain(&path).expect("chain must verify");
        assert_eq!(
            report,
            ChainVerifyReport {
                records: 3,
                chained: 3,
                legacy: 0
            },
            "{report:?}"
        );
        let text = std::fs::read_to_string(&path).unwrap();
        let first: AuditRecord = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(first.prev_hash, GENESIS_PREV_HASH);
        assert_eq!(first.hash.len(), 64);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P8.1 (§4.3): editing one line by hand breaks the chain and
    /// the verifier names the edited line.
    #[test]
    fn audit_chain_tamper_detected() {
        let dir = chain_temp_dir("tamper");
        let path = dir.join("audit.jsonl");
        let audit = AuditLog::open(&path).unwrap();
        for i in 0..3 {
            audit
                .log(
                    "t",
                    "decision",
                    None,
                    Some("allow"),
                    serde_json::json!({"i": i}),
                )
                .unwrap();
        }
        drop(audit);
        let mut lines: Vec<String> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        lines[1] = lines[1].replace("\"i\":1", "\"i\":99");
        assert_ne!(
            lines[1],
            std::fs::read_to_string(&path)
                .unwrap()
                .lines()
                .nth(1)
                .unwrap()
        );
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        let err = verify_audit_chain(&path).expect_err("tampered chain must fail");
        assert!(err.to_string().contains("line 2"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P8.1 (§4.2): reopening a log continues the chain from the
    /// last record's hash instead of restarting at genesis.
    #[test]
    fn audit_chain_continues_across_reopen() {
        let dir = chain_temp_dir("reopen");
        let path = dir.join("audit.jsonl");
        let audit = AuditLog::open(&path).unwrap();
        audit
            .log("t", "decision", None, Some("allow"), serde_json::json!({}))
            .unwrap();
        audit
            .log("t", "execution", None, None, serde_json::json!({}))
            .unwrap();
        drop(audit);
        let audit = AuditLog::open(&path).unwrap();
        audit
            .log("t", "verification", None, None, serde_json::json!({}))
            .unwrap();
        drop(audit);
        let report = verify_audit_chain(&path).expect("reopened chain must verify");
        assert_eq!(report.records, 3);
        assert_eq!(report.chained, 3);
        let text = std::fs::read_to_string(&path).unwrap();
        let records: Vec<AuditRecord> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records[2].prev_hash, records[1].hash);
        assert_ne!(records[2].prev_hash, GENESIS_PREV_HASH);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P8.1 (§4.2): a log that starts with pre-chain (legacy)
    /// lines still verifies: the chain starts from the SHA-256 of
    /// the last legacy line's raw bytes, without rewriting history.
    #[test]
    fn audit_chain_legacy_prefix() {
        let dir = chain_temp_dir("legacy");
        let path = dir.join("audit.jsonl");
        let legacy = "{\"ts_ms\":1,\"turn_id\":\"t\",\"kind\":\"decision\",\"tool\":null,\"decision\":\"allow\",\"detail\":{},\"agent_id\":\"agent-001\"}\n";
        std::fs::write(&path, legacy).unwrap();
        let audit = AuditLog::open(&path).unwrap();
        audit
            .log("t", "execution", None, None, serde_json::json!({}))
            .unwrap();
        drop(audit);
        let report = verify_audit_chain(&path).expect("legacy-prefixed chain must verify");
        assert_eq!(
            report,
            ChainVerifyReport {
                records: 2,
                chained: 1,
                legacy: 1
            },
            "{report:?}"
        );
        let text = std::fs::read_to_string(&path).unwrap();
        let records: Vec<AuditRecord> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            records[1].prev_hash,
            sha256_hex(legacy.trim_end().as_bytes())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
