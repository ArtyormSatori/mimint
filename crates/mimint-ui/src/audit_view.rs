//! Audit viewer: read the append-only JSONL audit file the worker
//! writes, and show the newest records. Read-only: this module never
//! writes to the audit file.
//!
//! Malformed lines are skipped, never fatal: the viewer must keep
//! working even if a record is half-written while the worker appends.

use serde_json::Value;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

/// One audit record, as displayed.
#[derive(Debug, Clone, PartialEq)]
pub struct AuditRow {
    pub ts_ms: u64,
    pub turn_id: String,
    pub kind: String,
    pub tool: Option<String>,
    pub decision: Option<String>,
    /// One-line summary of the detail object.
    pub detail: String,
}

/// Parse one JSONL line. Returns `None` for blank or malformed lines.
pub fn parse_audit_line(line: &str) -> Option<AuditRow> {
    let value: Value = serde_json::from_str(line.trim()).ok()?;
    let obj = value.as_object()?;
    let ts_ms = obj.get("ts_ms")?.as_u64()?;
    let turn_id = obj.get("turn_id")?.as_str()?.to_string();
    let kind = obj.get("kind")?.as_str()?.to_string();
    let tool = obj.get("tool").and_then(Value::as_str).map(str::to_string);
    let decision = obj
        .get("decision")
        .and_then(Value::as_str)
        .map(str::to_string);
    let detail = match obj.get("detail") {
        Some(Value::Object(map)) => {
            // Keep the summary short: up to 3 key=value pairs.
            let mut parts = Vec::new();
            for (key, val) in map.iter().take(3) {
                let rendered = match val {
                    Value::String(s) => format!("{key}={s}"),
                    other => format!("{key}={other}"),
                };
                parts.push(rendered);
            }
            parts.join(" ")
        }
        Some(other) => other.to_string(),
        None => String::new(),
    };
    Some(AuditRow {
        ts_ms,
        turn_id,
        kind,
        tool,
        decision,
        detail,
    })
}

/// Read the last `max_lines` records from the audit file (oldest
/// first). A missing file reads as empty; an unreadable file also
/// reads as empty — the viewer shows "no records yet" either way,
/// and the worker owns the real failure modes.
pub fn read_audit_tail(path: &Path, max_lines: usize) -> Vec<AuditRow> {
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    let reader = BufReader::new(file);
    let mut rows: Vec<AuditRow> = Vec::new();
    for line in reader.lines() {
        let Ok(line) = line else { continue };
        if let Some(row) = parse_audit_line(&line) {
            rows.push(row);
        }
    }
    if rows.len() > max_lines {
        rows.drain(..rows.len() - max_lines);
    }
    rows
}

/// Format epoch milliseconds as `HH:MM:SS` (UTC). No date library is
/// pulled in for one clock; the viewer labels the column UTC.
pub fn format_time_ms(ts_ms: u64) -> String {
    let secs = ts_ms / 1000;
    let hh = (secs / 3600) % 24;
    let mm = (secs / 60) % 60;
    let ss = secs % 60;
    format!("{hh:02}:{mm:02}:{ss:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_record_shape() {
        let line = r#"{"ts_ms":1794720000000,"turn_id":"turn-1-2","kind":"decision","tool":"notebook_add","decision":"approved","detail":{"risk":"consequential","approver":"desktop approval card"}}"#;
        let row = parse_audit_line(line).unwrap();
        assert_eq!(row.ts_ms, 1794720000000);
        assert_eq!(row.kind, "decision");
        assert_eq!(row.tool.as_deref(), Some("notebook_add"));
        assert_eq!(row.decision.as_deref(), Some("approved"));
        assert!(row.detail.contains("risk=consequential"), "{}", row.detail);
    }

    #[test]
    fn malformed_lines_are_skipped() {
        assert!(parse_audit_line("").is_none());
        assert!(parse_audit_line("not json").is_none());
        assert!(parse_audit_line(r#"{"ts_ms":"x"}"#).is_none());
        assert!(parse_audit_line(r#"{"ts_ms":1,"turn_id":"t"}"#).is_none());
    }

    #[test]
    fn tail_reads_last_n_and_missing_file_is_empty() {
        let dir = std::env::temp_dir().join(format!("mimint-audit-view-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("audit.jsonl");
        assert!(read_audit_tail(&path, 10).is_empty());
        let mut content = String::new();
        for i in 0..5 {
            content.push_str(&format!(
                "{{\"ts_ms\":{i},\"turn_id\":\"t{i}\",\"kind\":\"k\",\"tool\":null,\"decision\":null,\"detail\":{{}}}}\n"
            ));
        }
        content.push_str("half-written garbage\n");
        std::fs::write(&path, content).unwrap();
        let rows = read_audit_tail(&path, 3);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].turn_id, "t2");
        assert_eq!(rows[2].turn_id, "t4");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn time_format_is_hh_mm_ss_utc() {
        assert_eq!(format_time_ms(0), "00:00:00");
        assert_eq!(format_time_ms(3_661_000), "01:01:01");
        assert_eq!(format_time_ms(86_399_000), "23:59:59");
    }
}
