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
    /// Phase 6 (D-DESIGN-7): the owning agent. Legacy records
    /// without the field read as `agent-001`.
    pub agent_id: String,
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
                    Some("queue_sla_expired") => {
                        format!("{tool} chờ quá lâu trong hàng đợi nên hết hạn — không chạy.")
                    }
                    Some("body_queue_full") => {
                        format!("Hàng đợi action đã đầy — {tool} bị từ chối ngay, không xếp hàng.")
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
                // F6-15: no raw reason in the friendly line;
                // the source names the button when it was one.
                let source = self.source.as_deref().unwrap_or("không rõ nguồn");
                let source_label = if source == "ui_stop" {
                    "nút DỪNG KHẨN CẤP"
                } else {
                    source
                };
                format!(
                    "Dừng khẩn cấp ({source_label}). Điều khiển đang bị khóa. Khởi động lại ứng dụng (hoặc khởi tạo lại bộ điều khiển) để điều khiển tiếp."
                )
            }
            "config_loaded" => {
                "Đã nạp file cấu hình lúc khởi động (hash SHA-256 đã ghi vào bản ghi audit).".to_string()
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
            // ---- Phase 6 kinds ----
            "code_execution" => self.code_execution_summary(),
            "mcp_server_connected" => {
                let server = self.detail_text("mcp_server").unwrap_or_default();
                let version = self.detail_text("protocol_version").unwrap_or_default();
                let tools = self.detail_count("tool_count").unwrap_or(0);
                let sandboxed = self.detail_bool("sandboxed").unwrap_or(true);
                format!(
                    "Đã nối MCP server {server} (protocol {version}, {tools} công cụ){}.",
                    if sandboxed { ", trong sandbox" } else { ", KHÔNG SANDBOX" }
                )
            }
            "mcp_server_failed" => {
                let server = self.detail_text("mcp_server").unwrap_or_default();
                let error = self.detail_text("error").unwrap_or_default();
                format!("MCP server {server} lỗi: {error}.")
            }
            "mcp_tool_rejected" => {
                let server = self.detail_text("mcp_server").unwrap_or_default();
                let reason = self.reason.clone().unwrap_or_default();
                format!("Công cụ MCP của server {server} bị loại: {reason}.")
            }
            "mcp_unsandboxed_confirmed" => {
                let server = self.detail_text("mcp_server").unwrap_or_default();
                format!("Đã xác nhận chạy MCP server {server} KHÔNG sandbox.")
            }
            "mcp_privacy_warning" => {
                "Cảnh báo: Chế độ riêng tư đang BẬT nhưng MCP server vẫn chạy — egress của server không bị chặn hoàn toàn."
                    .to_string()
            }
            "sandbox_preflight" => match self.decision.as_deref() {
                Some("ok") => "Kiểm tra sandbox lúc khởi động: đạt.".to_string(),
                _ => "Kiểm tra sandbox lúc khởi động: KHÔNG ĐẠT — code sẽ không chạy ngoài sandbox."
                    .to_string(),
            },
            "agent_failure_scope" => {
                let scope = self.detail_text("scope").unwrap_or_default();
                let cause = self.detail_text("cause").unwrap_or_default();
                let failed = self.detail_text("failed_agent").unwrap_or_default();
                let verified = self.detail_bool("end_verified").unwrap_or(false);
                format!(
                    "Lỗi phạm vi {scope} từ {failed}: {cause}. Kết thúc đã xác minh: {}.",
                    if verified { "đúng" } else { "KHÔNG" }
                )
            }
            "body_rebuilt" => {
                let generation = self.detail_count("generation").unwrap_or(0);
                format!("Bộ điều khiển đã khởi tạo lại (generation {generation}). Lượt sau sẽ chụp màn hình mới trước khi hành động.")
            }
            "body_rebuild_failed" => {
                "Khởi tạo lại bộ điều khiển thất bại. Bộ điều khiển chưa sẵn sàng — thử lại hoặc khởi động lại ứng dụng.".to_string()
            }
            "task_end_unverified" => {
                let source = self.source.clone().unwrap_or_default();
                format!(
                    "Không xác minh được agent đã dừng hẳn sau khi hủy (nguồn: {source}). Coordinator ở trạng thái Degraded tới khi khởi động lại."
                )
            }
            "delegate_clamped" => {
                let requested = self.detail_count("requested_tokens").unwrap_or(0);
                let clamped = self.detail_count("clamped_tokens").unwrap_or(0);
                format!(
                    "Ngân sách token của sub-agent bị giới hạn từ {requested} xuống {clamped} (không vượt ngân sách còn lại của agent cha)."
                )
            }
            "routing" => {
                let rule = self.detail_text("rule").unwrap_or_default();
                match rule.as_str() {
                    "r6_gui_batch_loop" => {
                        "Lượt dừng vì lặp action GUI giống nhau quá ngưỡng — gợi ý dùng code thay cho thao tác GUI hàng loạt."
                            .to_string()
                    }
                    _ => format!("Định tuyến: {rule}."),
                }
            }
            "provider_attempt" => match self.decision.as_deref() {
                Some("budget_exhausted") => {
                    let scope = self.detail_text("scope").unwrap_or_default();
                    format!("Hết ngân sách token ({scope}) trước khi gọi provider.")
                }
                _ => {
                    let charged = self.detail_count("charged_tokens").unwrap_or(0);
                    let kind = self.detail_text("charge_kind").unwrap_or_default();
                    let unmeasured = self.detail_bool("usage_unmeasured").unwrap_or(false);
                    format!(
                        "Lần gọi provider: {charged} token đã tính ({kind}){}.",
                        if unmeasured {
                            " — ƯỚC TÍNH, chưa đo được"
                        } else {
                            ""
                        }
                    )
                }
            },
            _ => {
                let tool = self.tool.as_deref().unwrap_or("-");
                let decision = self.decision.as_deref().unwrap_or("");
                format!("{} {} {}", self.kind, tool, decision)
                    .trim_end()
                    .to_string()
            }
        }
    }

    /// The `code_execution` summary (§15.2, post-run half): the
    /// executed hash is compared against the approved hash and
    /// the verdict is stated plainly. A record with no executed
    /// hash (denied/failed runs) makes no mismatch claim — the
    /// code never ran, so there is nothing to compare.
    fn code_execution_summary(&self) -> String {
        let language = self.detail_text("language").unwrap_or_default();
        let verdict = self.detail_text("sandbox_verdict").unwrap_or_default();
        let exit = self.detail_count("exit_code");
        let mut summary = match self.decision.as_deref() {
            Some("executed") => format!(
                "Đã chạy code {language} trong sandbox (verdict: {verdict}, mã thoát: {}).",
                exit.map_or("-".to_string(), |code| code.to_string())
            ),
            _ => format!("Chạy code {language} không thành (verdict: {verdict})."),
        };
        match self.detail_bool("code_hash_verified") {
            Some(true) => {
                summary.push_str(" Hash của code đã chạy khớp hash đã phê duyệt (đã kiểm chứng).");
            }
            Some(false) => {
                // Only a mismatch claim when both hashes exist
                // and differ; a missing executed hash means
                // the run never produced one (error path).
                let approved = self.detail_text("code_sha256");
                let executed = self.detail_text("executed_code_sha256");
                if let (Some(approved), Some(executed)) = (approved, executed) {
                    if approved != executed {
                        summary.push_str(
                            " CẢNH BÁO: hash của code đã chạy KHÔNG khớp hash đã phê duyệt.",
                        );
                    }
                }
            }
            None => {}
        }
        // Input-hash verification (F6-06): same shape as the
        // code-hash verdict. Records written before the fix
        // have no `input_hashes_verified` field and make no
        // claim; a verified run with no inputs stays silent
        // (nothing to compare).
        match self.detail_bool("input_hashes_verified") {
            Some(true) => {
                if let Some(Value::Array(list)) = self.detail_value("input_files_sha256") {
                    if !list.is_empty() {
                        summary
                            .push_str(" Hash của các file đầu vào đã chạy khớp hash đã phê duyệt.");
                    }
                }
            }
            Some(false) => {
                summary.push_str(" CẢNH BÁO: hash của file đầu vào KHÔNG khớp hash đã phê duyệt.");
            }
            None => {}
        }
        summary
    }

    /// One detail field from the full raw line (the 3-pair
    /// summary truncates). `None` when the line does not parse
    /// or the field is absent/not the right type.
    fn detail_value(&self, key: &str) -> Option<Value> {
        let value: Value = serde_json::from_str(&self.raw).ok()?;
        value.get("detail")?.get(key).cloned()
    }

    fn detail_text(&self, key: &str) -> Option<String> {
        self.detail_value(key)
            .and_then(|v| v.as_str().map(str::to_string))
    }

    fn detail_count(&self, key: &str) -> Option<u64> {
        self.detail_value(key).and_then(|v| v.as_u64())
    }

    fn detail_bool(&self, key: &str) -> Option<bool> {
        self.detail_value(key).and_then(|v| v.as_bool())
    }

    /// Learning-journal line (F5-12): `Some` for records about
    /// learning — skill validation, promotion, evolution, drift
    /// checks, recorded experience, import approvals — and
    /// `None` for everything else. The category (tried /
    /// verified / applied / not-improved) comes only from the
    /// record's kind + decision; the detail (skill name, pass
    /// counts, task id) is read from the record's full detail
    /// object, NOT from the truncated 3-pair summary — the
    /// summary's alphabetical cut hid the skill name behind
    /// the hash (demo catch). Nothing is inferred beyond what
    /// the record says: a record that does not exist is never
    /// shown as a result, and a missing field falls back to
    /// the summary string.
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
        let detail = self
            .learning_detail()
            .unwrap_or_else(|| self.detail.clone());
        if detail.is_empty() {
            Some(format!("[{category}]"))
        } else {
            Some(format!("[{category}] {detail}"))
        }
    }

    /// The informative part of a learning record, read from the
    /// full detail object in the raw line: skill name + pass
    /// counts, golden id + issue count, task id, trigger +
    /// skill names. `None` when the raw line does not parse or
    /// the kind carries no such fields (the caller falls back
    /// to the truncated summary).
    fn learning_detail(&self) -> Option<String> {
        let value: Value = serde_json::from_str(&self.raw).ok()?;
        let detail = value.get("detail")?.as_object()?;
        let text = |key: &str| detail.get(key).and_then(Value::as_str).map(str::to_string);
        let count = |key: &str| detail.get(key).and_then(Value::as_u64);
        match self.kind.as_str() {
            "skill_validation" => {
                let skill = text("skill")?;
                match (count("passed"), count("total")) {
                    (Some(passed), Some(total)) => {
                        Some(format!("kỹ năng {skill}: {passed}/{total} đạt"))
                    }
                    _ => Some(format!("kỹ năng {skill}")),
                }
            }
            "skill_promote" | "skill_import_approved" => {
                Some(format!("kỹ năng {}", text("skill")?))
            }
            "skill_evolve" => {
                let skill = text("skill")?;
                let scores = detail.get("scores").and_then(Value::as_array);
                let mut parts = vec![format!("kỹ năng {skill}")];
                if let Some(scores) = scores {
                    for score in scores {
                        let variant = score.get("variant").and_then(Value::as_str)?;
                        let passed = score.get("passed").and_then(Value::as_u64)?;
                        let total = score.get("total").and_then(Value::as_u64)?;
                        parts.push(format!("{variant} {passed}/{total}"));
                    }
                }
                Some(parts.join(" · "))
            }
            "drift_eval" => {
                let golden = text("golden_id")?;
                let issues = detail
                    .get("issues")
                    .and_then(Value::as_array)
                    .map(|issues| issues.len())
                    .unwrap_or(0);
                Some(format!("prompt {golden}: {issues} vấn đề"))
            }
            "experience_recorded" => {
                let task = text("task")?;
                match text("recorded") {
                    Some(recorded) => Some(format!("nhiệm vụ {task}: {recorded}")),
                    None => Some(format!("nhiệm vụ {task}")),
                }
            }
            "skill_trigger_conflict" => {
                let trigger = text("trigger")?;
                let skills = detail.get("skills").and_then(Value::as_array);
                let names: Vec<&str> = skills
                    .map(|names| names.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                Some(format!("trigger \"{trigger}\": {}", names.join(", ")))
            }
            _ => None,
        }
    }
}

/// Parse one JSONL line. Returns `None` for blank or malformed lines.
pub fn parse_audit_line(line: &str) -> Option<AuditRow> {
    let value: Value = serde_json::from_str(line.trim()).ok()?;
    let obj = value.as_object()?;
    let ts_ms = obj.get("ts_ms")?.as_u64()?;
    let turn_id = obj.get("turn_id")?.as_str()?.to_string();
    // D-DESIGN-7: legacy records without `agent_id` read as the
    // default agent; a non-string value is treated as missing.
    let agent_id = obj
        .get("agent_id")
        .and_then(Value::as_str)
        .unwrap_or("agent-001")
        .to_string();
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
        agent_id,
        kind,
        tool,
        decision,
        detail,
        reason,
        source,
        raw: line.trim().to_string(),
    })
}

/// The distinct agent ids in a set of rows, sorted. The audit
/// panel hides the agent column when this has one entry (§1.4).
pub fn distinct_agents(rows: &[AuditRow]) -> Vec<String> {
    let mut agents: Vec<String> = rows.iter().map(|row| row.agent_id.clone()).collect();
    agents.sort();
    agents.dedup();
    agents
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
    // F6-15: the banner names the source and the locked
    // state, never the raw reason string.
    let source = row.source.as_deref().unwrap_or("không rõ nguồn");
    let source_label = if source == "ui_stop" {
        "nút DỪNG KHẨN CẤP"
    } else {
        source
    };
    Some(format!(
        "Đã DỪNG KHẨN CẤP ({source_label}, lúc {} UTC) — điều khiển đang bị khóa. Khởi tạo lại bộ điều khiển (hoặc khởi động lại ứng dụng) để điều khiển tiếp.",
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
        // The detail names the skill and the counts even when
        // the truncated summary would hide them behind the hash
        // (demo catch: alphabetical cut).
        let full = line(
            "skill_validation",
            "promoted",
            r#"{"skill":"ghi-chu","passed":2,"total":2,"content_hash":"8e14ebfcb743c4aa","import_approved":false,"imported":false}"#,
        );
        assert!(full.contains("ghi-chu"), "{full}");
        assert!(full.contains("2/2"), "{full}");
        let evolve = line(
            "skill_evolve",
            "base_kept",
            r#"{"skill":"ghi-chu","scores":[{"variant":"base","passed":3,"total":3},{"variant":"short-answer","passed":3,"total":3}]}"#,
        );
        assert!(evolve.contains("ghi-chu"), "{evolve}");
        assert!(evolve.contains("base 3/3"), "{evolve}");
        let drift = line(
            "drift_eval",
            "drift",
            r#"{"golden_id":"V4-uncertainty","issues":["a","b"]}"#,
        );
        assert!(drift.contains("V4-uncertainty"), "{drift}");
        assert!(drift.contains("2 vấn đề"), "{drift}");
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

    #[test]
    fn legacy_records_without_agent_id_read_as_the_default_agent() {
        let row = parse_audit_line(
            r#"{"ts_ms":1,"turn_id":"t","kind":"decision","decision":"allow","detail":{}}"#,
        )
        .unwrap();
        assert_eq!(row.agent_id, "agent-001");
        let row = parse_audit_line(
            r#"{"ts_ms":1,"turn_id":"t","agent_id":"agent-002","kind":"decision","decision":"allow","detail":{}}"#,
        )
        .unwrap();
        assert_eq!(row.agent_id, "agent-002");
    }

    #[test]
    fn distinct_agents_reports_the_sorted_set() {
        let rows: Vec<AuditRow> = ["agent-002", "agent-001", "agent-002"]
            .iter()
            .map(|id| {
                parse_audit_line(&format!(
                    "{{\"ts_ms\":1,\"turn_id\":\"t\",\"agent_id\":\"{id}\",\"kind\":\"k\",\"detail\":{{}}}}"
                ))
                .unwrap()
            })
            .collect();
        assert_eq!(distinct_agents(&rows), vec!["agent-001", "agent-002"]);
        assert!(distinct_agents(&[]).is_empty());
    }

    #[test]
    fn phase_6_summaries_name_the_hash_check_and_the_new_kinds() {
        // Code execution with a verified hash: the summary says
        // so, plainly (§15.2 post-run half).
        let row = parse_audit_line(
            r#"{"ts_ms":1,"turn_id":"t","agent_id":"agent-002","kind":"code_execution","tool":"execute_code","decision":"executed","detail":{"language":"python3","code_len":14,"code_sha256":"aaaa","executed_code_sha256":"aaaa","code_hash_verified":true,"sandbox_verdict":"bwrap","exit_code":0,"duration_ms":12,"truncated":false}}"#,
        )
        .unwrap();
        let summary = row.friendly_summary();
        assert!(summary.contains("python3"), "{summary}");
        assert!(summary.contains("đã kiểm chứng"), "{summary}");
        assert!(!summary.contains("KHÔNG khớp"), "{summary}");
        // A real mismatch is a warning, never silent.
        let row = parse_audit_line(
            r#"{"ts_ms":1,"turn_id":"t","kind":"code_execution","tool":"execute_code","decision":"executed","detail":{"language":"sh","code_sha256":"aaaa","executed_code_sha256":"bbbb","code_hash_verified":false,"sandbox_verdict":"bwrap","exit_code":0}}"#,
        )
        .unwrap();
        assert!(row.friendly_summary().contains("KHÔNG khớp"));
        // A denied run has no executed hash: no mismatch claim.
        let row = parse_audit_line(
            r#"{"ts_ms":1,"turn_id":"t","kind":"code_execution","tool":"execute_code","decision":"error","detail":{"language":"python3","code_sha256":"aaaa","sandbox_verdict":"sandbox_unavailable"}}"#,
        )
        .unwrap();
        let summary = row.friendly_summary();
        assert!(summary.contains("không thành"), "{summary}");
        assert!(!summary.contains("KHÔNG khớp"), "{summary}");
        // MCP + scope + rebuild + routing summaries.
        let row = parse_audit_line(
            r#"{"ts_ms":1,"turn_id":"t","kind":"mcp_server_connected","detail":{"mcp_server":"notes","server_name":"n","server_version":"1","protocol_version":"2025-06-18","tool_count":3,"sandboxed":false}}"#,
        )
        .unwrap();
        assert!(row.friendly_summary().contains("KHÔNG SANDBOX"));
        let row = parse_audit_line(
            r#"{"ts_ms":1,"turn_id":"t","kind":"agent_failure_scope","decision":"all","detail":{"scope":"all","cause":"unknown_outcome: x","failed_agent":"agent-002","stop_latched":false,"actor_broken":true,"end_verified":false,"end_verification":"mixed","agents":[]}}"#,
        )
        .unwrap();
        assert!(row.friendly_summary().contains("agent-002"));
        let row = parse_audit_line(
            r#"{"ts_ms":1,"turn_id":"t","kind":"body_rebuilt","decision":"rebuilt","detail":{"reason":"fault","generation":2}}"#,
        )
        .unwrap();
        assert!(row.friendly_summary().contains("generation 2"));
        let row = parse_audit_line(
            r#"{"ts_ms":1,"turn_id":"t","kind":"routing","decision":"deny","detail":{"rule":"r6_gui_batch_loop"}}"#,
        )
        .unwrap();
        assert!(row.friendly_summary().contains("lặp action GUI"));
        // F6-06: input-hash verdicts in the code_execution
        // summary — verified inputs say so, a mismatch warns.
        let row = parse_audit_line(
            r#"{"ts_ms":1,"turn_id":"t","kind":"code_execution","tool":"execute_code","decision":"executed","detail":{"language":"python3","code_sha256":"aaaa","executed_code_sha256":"aaaa","code_hash_verified":true,"input_files_sha256":[{"name":"data.txt","sha256":"h1"}],"executed_input_files_sha256":[{"name":"data.txt","sha256":"h1"}],"input_hashes_verified":true,"sandbox_verdict":"bwrap","exit_code":0}}"#,
        )
        .unwrap();
        let summary = row.friendly_summary();
        assert!(summary.contains("file đầu vào"), "{summary}");
        assert!(!summary.contains("CẢNH BÁO"), "{summary}");
        let row = parse_audit_line(
            r#"{"ts_ms":1,"turn_id":"t","kind":"code_execution","tool":"execute_code","decision":"executed","detail":{"language":"python3","code_sha256":"aaaa","executed_code_sha256":"aaaa","code_hash_verified":true,"input_files_sha256":[{"name":"data.txt","sha256":"h1"}],"executed_input_files_sha256":[{"name":"data.txt","sha256":"h2"}],"input_hashes_verified":false,"sandbox_verdict":"bwrap","exit_code":0}}"#,
        )
        .unwrap();
        assert!(row.friendly_summary().contains("CẢNH BÁO"));
        // Queue decisions name the cause.
        let row = parse_audit_line(
            r#"{"ts_ms":1,"turn_id":"t","kind":"decision","tool":"body_click","decision":"deny","detail":{"reason":"queue_sla_expired"}}"#,
        )
        .unwrap();
        assert!(row.friendly_summary().contains("hết hạn"));
    }
}
