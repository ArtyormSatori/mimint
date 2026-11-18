//! Audit viewer: read the append-only JSONL audit file the worker
//! writes, and show the newest records. Read-only: this module never
//! writes to the audit file.
//!
//! Malformed lines are skipped, never fatal: the viewer must keep
//! working even if a record is half-written while the worker appends.
//! A trailing line WITHOUT a newline terminator is dropped outright,
//! even when it parses: it may be mid-append, and the next refresh
//! picks it up once the writer finishes the line. The audit log has
//! exactly one writer (the worker's `Arc<AuditLog>` behind a mutex);
//! the viewer only ever reads.

use serde_json::Value;
use std::fs::File;
use std::io::Read;
use std::path::Path;

/// One audit record, as displayed. Two layers: the friendly summary
/// ([`AuditRow::friendly_summary`]) for everyday reading, and the
/// raw JSON line for technical inspection.
#[derive(Debug, Clone, PartialEq)]
pub struct AuditRow {
    pub ts_ms: u64,
    pub turn_id: String,
    pub kind: String,
    pub tool: Option<String>,
    pub decision: Option<String>,
    /// One-line summary of the detail object.
    pub detail: String,
    /// The `reason` inside the detail object, when present (it
    /// distinguishes refused / timed-out / cancelled decisions).
    pub reason: Option<String>,
    /// The `source` inside the detail object, when present (the
    /// emergency-stop mechanism: hotkey, approval_pending, ...).
    pub source: Option<String>,
    /// The raw JSONL line, for the technical layer.
    pub raw: String,
}

impl AuditRow {
    /// The readable layer: one plain sentence per record, in
    /// Vietnamese, with no jargon. Unknown kinds fall back to the
    /// kind/tool/decision triple, never to silence.
    pub fn friendly_summary(&self) -> String {
        match self.kind.as_str() {
            "turn_start" => "Bắt đầu một lượt trò chuyện.".to_string(),
            "turn_end" => match self.decision.as_deref() {
                Some("completed") => "Lượt trò chuyện xong.".to_string(),
                Some("cancelled") => "Lượt trò chuyện đã bị hủy.".to_string(),
                Some("partial") => {
                    "Lượt trò chuyện dừng giữa chừng (hết ngân sách bước).".to_string()
                }
                Some(other) => format!("Lượt trò chuyện kết thúc: {other}."),
                None => "Lượt trò chuyện kết thúc.".to_string(),
            },
            "decision" => {
                let tool = self.tool.as_deref().unwrap_or("công cụ");
                match self.reason.as_deref() {
                    Some("approved") => format!("Đã cho phép {tool}."),
                    Some("approval_refused") => format!("Đã từ chối {tool}."),
                    Some("approval_cancelled") => {
                        format!("Phê duyệt {tool} đã bị hủy (DỪNG / hủy lượt).")
                    }
                    Some("approval_timeout") => {
                        format!("Phê duyệt {tool} hết thời gian chờ — đã từ chối.")
                    }
                    Some("approval_id_mismatch") => {
                        format!("Quyết định phê duyệt {tool} sai mã yêu cầu — đã từ chối.")
                    }
                    Some("cancelled_after_approval") => {
                        format!("{tool} đã được duyệt nhưng bị DỪNG trước khi chạy — không chạy.")
                    }
                    Some("cancelled_before_approval") | Some("cancelled_before_execution") => {
                        format!("{tool} bị DỪNG trước khi chạy — không chạy.")
                    }
                    Some("approver_error") => {
                        format!("Thẻ phê duyệt {tool} gặp lỗi — đã từ chối cho an toàn.")
                    }
                    _ => match self.decision.as_deref() {
                        Some("allow") => format!("Đã cho phép {tool}."),
                        Some("deny") => format!("Đã từ chối {tool}."),
                        Some("cancelled") => format!("{tool} đã bị hủy."),
                        Some("timeout") => {
                            format!("Phê duyệt {tool} hết thời gian chờ — đã từ chối.")
                        }
                        Some(other) => format!("Quyết định cho {tool}: {other}."),
                        None => format!("Quyết định cho {tool}."),
                    },
                }
            }
            "approval_timeout" => "Một yêu cầu phê duyệt hết thời gian chờ.".to_string(),
            "execution" => {
                let tool = self.tool.as_deref().unwrap_or("công cụ");
                match self.decision.as_deref() {
                    Some("executed") => format!("Đã chạy {tool}."),
                    Some("error") => format!("Chạy {tool} gặp lỗi."),
                    Some(other) => format!("Chạy {tool}: {other}."),
                    None => format!("Chạy {tool}."),
                }
            }
            "verification" => {
                let tool = self.tool.as_deref().unwrap_or("công cụ");
                match self.decision.as_deref() {
                    Some("verified") => format!("Đã kiểm tra {tool}: kết quả khớp."),
                    Some("failed") => {
                        format!("Đã kiểm tra {tool}: kết quả không khớp hoặc gặp lỗi.")
                    }
                    Some(other) => format!("Kiểm tra {tool}: {other}."),
                    None => format!("Kiểm tra {tool}."),
                }
            }
            "reflection" => "Mimint tự xem lại tiến độ của lượt.".to_string(),
            "emergency_stop" => {
                let source = self.source.as_deref().unwrap_or("không rõ nguồn");
                let reason = self.reason.as_deref().unwrap_or("dừng khẩn cấp");
                format!(
                    "Dừng khẩn cấp ({source}): {reason}. Khởi động lại ứng dụng để điều khiển tiếp."
                )
            }
            "privacy_mode" => match self.decision.as_deref() {
                Some("on") => "Chế độ riêng tư: BẬT.".to_string(),
                Some("off") => "Chế độ riêng tư: TẮT.".to_string(),
                _ => "Chế độ riêng tư thay đổi.".to_string(),
            },
            "provider_call" => {
                let tool = self.tool.as_deref().unwrap_or("backend");
                format!("Đã gọi {tool}.")
            }
            "body_action" => {
                let tool = self.tool.as_deref().unwrap_or("bodyctl");
                format!("Hành động {tool} (công cụ phát triển).")
            }
            _ => {
                let tool = self.tool.as_deref().unwrap_or("-");
                let decision = self.decision.as_deref().unwrap_or("");
                format!("{} {} {}", self.kind, tool, decision)
                    .trim_end()
                    .to_string()
            }
        }
    }

    /// Learning-journal line (F5-12): `Some` for records about
    /// learning — skill validation, promotion, evolution, drift
    /// checks, recorded experience, import approvals — and
    /// `None` for everything else. The category (tried /
    /// verified / applied / not-improved) comes only from the
    /// record's kind + decision; the detail string carries the
    /// skill/task name and counts. Nothing is inferred beyond
    /// what the record says: a record that does not exist is
    /// never shown as a result.
    pub fn learning_summary(&self) -> Option<String> {
        let category = match (self.kind.as_str(), self.decision.as_deref()) {
            ("skill_validation", Some("promoted")) => "Đã xác minh",
            ("skill_validation", Some("quarantined")) => {
                if self.reason.as_deref() == Some("negative_probe") {
                    "Đã thử, chưa đạt — checker luôn-pass nên bị loại"
                } else {
                    "Đã thử, chưa đạt"
                }
            }
            ("skill_validation", Some("awaiting_import_approval")) => {
                "Đã xác minh, chờ bạn duyệt import"
            }
            ("skill_validation", _) => "Đã thử nghiệm",
            ("skill_promote", _) => "Đã áp dụng",
            ("skill_import_approved", _) => "Bạn đã duyệt import",
            ("skill_evolve", Some("winner")) => "Đã cải thiện",
            ("skill_evolve", Some("base_kept")) => "Chưa cải thiện — giữ bản gốc",
            ("skill_evolve", Some("inconclusive_ceiling")) => {
                "Chưa kết luận được — mọi bản đều chạm trần"
            }
            ("skill_evolve", Some("budget_exhausted")) => "Chưa xong — hết ngân sách thời gian",
            ("skill_evolve", _) => "Đã thử nghiệm",
            ("drift_eval", Some("clean")) => "Kiểm tra drift: sạch",
            ("drift_eval", Some("drift")) => "Drift bị bắt",
            ("drift_eval", _) => "Kiểm tra drift",
            ("experience_recorded", Some("passed")) => "Kinh nghiệm đã ghi (có checker): đạt",
            ("experience_recorded", Some("failed")) => "Kinh nghiệm đã ghi (có checker): chưa đạt",
            ("experience_recorded", _) => "Kinh nghiệm đã ghi (có checker)",
            ("skill_trigger_conflict", _) => "Cảnh báo: trigger trùng nhau",
            _ => return None,
        };
        if self.detail.is_empty() {
            Some(format!("[{category}]"))
        } else {
            Some(format!("[{category}] {}", self.detail))
        }
    }
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
    let reason = obj
        .get("detail")
        .and_then(|d| d.get("reason"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let source = obj
        .get("detail")
        .and_then(|d| d.get("source"))
        .and_then(Value::as_str)
        .map(str::to_string);
    Some(AuditRow {
        ts_ms,
        turn_id,
        kind,
        tool,
        decision,
        detail,
        reason,
        source,
        raw: line.trim().to_string(),
    })
}

/// Read the last `max_lines` records from the audit file (oldest
/// first). A missing file reads as empty; an unreadable file also
/// reads as empty — the viewer shows "no records yet" either way,
/// and the worker owns the real failure modes.
///
/// Only newline-terminated lines are read: a trailing unterminated
/// line is dropped even when it parses, because the worker may
/// still be appending to it. The next refresh reads it once the
/// newline lands.
pub fn read_audit_tail(path: &Path, max_lines: usize) -> Vec<AuditRow> {
    let Ok(mut file) = File::open(path) else {
        return Vec::new();
    };
    let mut content = String::new();
    if file.read_to_string(&mut content).is_err() {
        return Vec::new();
    }
    let complete = match content.rfind('\n') {
        Some(pos) => &content[..=pos],
        None => return Vec::new(),
    };
    let mut rows: Vec<AuditRow> = Vec::new();
    for line in complete.lines() {
        if let Some(row) = parse_audit_line(line) {
            rows.push(row);
        }
    }
    if rows.len() > max_lines {
        rows.drain(..rows.len() - max_lines);
    }
    rows
}

/// The restart banner for the audit panel: when the newest
/// `emergency_stop` record is newer than the newest app startup
/// (`ui-startup` turn id), the latch is still in force and the
/// panel says so, with the stop's source, reason, and time.
/// `None` when no stop is recorded, or a restart happened after
/// the last stop (the latch cleared with the old process).
pub fn stop_banner(rows: &[AuditRow]) -> Option<String> {
    let last_stop = rows.iter().rposition(|row| row.kind == "emergency_stop")?;
    if let Some(last_startup) = rows.iter().rposition(|row| row.turn_id == "ui-startup") {
        if last_startup > last_stop {
            return None;
        }
    }
    let row = &rows[last_stop];
    let source = row.source.as_deref().unwrap_or("không rõ nguồn");
    let reason = row.reason.as_deref().unwrap_or("dừng khẩn cấp");
    Some(format!(
        "Đã DỪNG KHẨN CẤP ({source}, lúc {} UTC) — {reason}. Khởi động lại ứng dụng để điều khiển tiếp.",
        format_time_ms(row.ts_ms)
    ))
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
    fn trailing_unterminated_line_is_dropped_even_when_it_parses() {
        let dir =
            std::env::temp_dir().join(format!("mimint-audit-view-partial-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("audit.jsonl");
        // One complete record, then a valid-looking record with no
        // trailing newline (a write in progress): only the first
        // must show.
        let content = "{\"ts_ms\":1,\"turn_id\":\"done\",\"kind\":\"k\",\"detail\":{}}\n\
             {\"ts_ms\":2,\"turn_id\":\"pending\",\"kind\":\"k\",\"detail\":{}}";
        std::fs::write(&path, content).unwrap();
        let rows = read_audit_tail(&path, 10);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].turn_id, "done");
        // Once the newline lands, the second record appears.
        std::fs::write(&path, format!("{content}\n")).unwrap();
        let rows = read_audit_tail(&path, 10);
        assert_eq!(rows.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn friendly_summary_reads_plainly() {
        let row = parse_audit_line(
            r#"{"ts_ms":1,"turn_id":"t","kind":"decision","tool":"notebook_add","decision":"timeout","detail":{"reason":"approval_timeout"}}"#,
        )
        .unwrap();
        assert!(row.friendly_summary().contains("hết thời gian chờ"));
        assert!(row.raw.contains("approval_timeout"));
        let row = parse_audit_line(
            r#"{"ts_ms":1,"turn_id":"t","kind":"privacy_mode","decision":"on","detail":{}}"#,
        )
        .unwrap();
        assert_eq!(row.friendly_summary(), "Chế độ riêng tư: BẬT.");
    }

    #[test]
    fn friendly_summary_covers_execution_verification_reflection_and_stop() {
        let row = parse_audit_line(
            r#"{"ts_ms":1,"turn_id":"t","kind":"execution","tool":"body_click","decision":"executed","detail":{}}"#,
        )
        .unwrap();
        assert_eq!(row.friendly_summary(), "Đã chạy body_click.");
        let row = parse_audit_line(
            r#"{"ts_ms":1,"turn_id":"t","kind":"execution","tool":"body_click","decision":"error","detail":{"error":"boom"}}"#,
        )
        .unwrap();
        assert_eq!(row.friendly_summary(), "Chạy body_click gặp lỗi.");
        let row = parse_audit_line(
            r#"{"ts_ms":1,"turn_id":"t","kind":"verification","tool":"body_click","decision":"verified","detail":{}}"#,
        )
        .unwrap();
        assert!(row.friendly_summary().contains("kết quả khớp"));
        let row = parse_audit_line(
            r#"{"ts_ms":1,"turn_id":"t","kind":"verification","tool":"body_click","decision":"failed","detail":{"reason":"tool_error"}}"#,
        )
        .unwrap();
        assert!(row.friendly_summary().contains("không khớp"));
        let row = parse_audit_line(
            r#"{"ts_ms":1,"turn_id":"t","kind":"reflection","detail":{"step":2}}"#,
        )
        .unwrap();
        assert!(row.friendly_summary().contains("xem lại tiến độ"));
        let row = parse_audit_line(
            r#"{"ts_ms":3661000,"turn_id":"ui-control","kind":"emergency_stop","decision":"stopped","detail":{"source":"hotkey","reason":"global STOP hotkey pressed","trip_ts_ms":3661000}}"#,
        )
        .unwrap();
        assert_eq!(row.source.as_deref(), Some("hotkey"));
        assert!(row.friendly_summary().contains("Dừng khẩn cấp (hotkey)"));
        assert!(row.friendly_summary().contains("Khởi động lại"));
    }

    #[test]
    fn stop_banner_shows_until_a_restart_supersedes_it() {
        let stop = parse_audit_line(
            r#"{"ts_ms":3661000,"turn_id":"ui-control","kind":"emergency_stop","decision":"stopped","detail":{"source":"hotkey","reason":"chord pressed","trip_ts_ms":3661000}}"#,
        )
        .unwrap();
        let startup = parse_audit_line(
            r#"{"ts_ms":3662000,"turn_id":"ui-startup","kind":"privacy_mode","decision":"on","detail":{}}"#,
        )
        .unwrap();
        assert!(stop_banner(&[]).is_none());
        let banner = stop_banner(std::slice::from_ref(&stop)).expect("banner for a live latch");
        assert!(banner.contains("01:01:01"), "{banner}");
        assert!(banner.contains("hotkey"), "{banner}");
        // A restart after the stop clears the banner.
        assert!(stop_banner(&[stop.clone(), startup]).is_none());
        // A stop after the restart shows again.
        let startup2 = parse_audit_line(
            r#"{"ts_ms":3662000,"turn_id":"ui-startup","kind":"privacy_mode","decision":"on","detail":{}}"#,
        )
        .unwrap();
        assert!(stop_banner(&[startup2, stop]).is_some());
    }

    #[test]
    fn learning_summary_covers_tried_verified_applied_and_not_improved() {
        let line = |kind: &str, decision: &str, detail: &str| {
            parse_audit_line(&format!(
                "{{\"ts_ms\":1,\"turn_id\":\"t\",\"kind\":\"{kind}\",\"decision\":\"{decision}\",\"detail\":{detail}}}"
            ))
            .unwrap()
            .learning_summary()
            .unwrap()
        };
        assert!(line(
            "skill_validation",
            "promoted",
            r#"{"skill":"ghi-chu","passed":2,"total":2}"#
        )
        .starts_with("[Đã xác minh]"));
        assert!(line(
            "skill_validation",
            "quarantined",
            r#"{"skill":"x","reason":"negative_probe"}"#
        )
        .contains("luôn-pass"));
        assert!(line(
            "skill_validation",
            "awaiting_import_approval",
            r#"{"skill":"x"}"#
        )
        .contains("chờ bạn duyệt import"));
        assert!(
            line("skill_promote", "promoted", r#"{"skill":"ghi-chu"}"#).starts_with("[Đã áp dụng]")
        );
        assert!(
            line("skill_evolve", "base_kept", r#"{"skill":"ghi-chu"}"#).contains("Chưa cải thiện")
        );
        assert!(line(
            "skill_evolve",
            "inconclusive_ceiling",
            r#"{"skill":"ghi-chu"}"#
        )
        .contains("chạm trần"));
        assert!(line(
            "drift_eval",
            "drift",
            r#"{"golden_id":"G1","issues":["x"]}"#
        )
        .contains("Drift bị bắt"));
        assert!(line("experience_recorded", "passed", r#"{"task":"H1"}"#).contains("có checker"));
        // Non-learning records stay out of the journal.
        let row = parse_audit_line(r#"{"ts_ms":1,"turn_id":"t","kind":"turn_start","detail":{}}"#)
            .unwrap();
        assert!(row.learning_summary().is_none());
    }

    #[test]
    fn time_format_is_hh_mm_ss_utc() {
        assert_eq!(format_time_ms(0), "00:00:00");
        assert_eq!(format_time_ms(3_661_000), "01:01:01");
        assert_eq!(format_time_ms(86_399_000), "23:59:59");
    }
}
