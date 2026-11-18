//! Approval card content: what the user sees before deciding.
//!
//! The card shows ACTION-SPECIFIC content, not a raw JSON dump: what
//! will happen, to what target, with what scope, what consequence to
//! expect, and whether it can be undone. The values shown are the
//! values executed — the card renders the same immutable `args` the
//! executor will run, so "what you see" cannot drift from "what
//! runs".
//!
//! Phase 4 adds explicit cards for the ten `body_*` tools: each
//! card names the action, the exact coordinates/text/key that will
//! run, and the consequence. Unknown tools fall back to the generic
//! rendering, which lists every argument — never hidden.

use mimint_body::{Framebuffer, WindowIdentity};
use mimint_core::{execute_code_input_hashes, sha256_hex};
use serde_json::Value;

/// One card's content: a title plus label/value rows, and whether the
/// action can be undone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalDetail {
    pub title: String,
    pub rows: Vec<(String, String)>,
    /// Human-readable reversibility note.
    pub reversible: String,
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)?.as_str()
}

fn arg_int(args: &Value, key: &str) -> Option<i64> {
    args.get(key)?.as_i64()
}

fn point_row(args: &Value) -> (String, String) {
    match (arg_int(args, "x"), arg_int(args, "y")) {
        (Some(x), Some(y)) => ("Tọa độ màn hình".to_string(), format!("({x}, {y})")),
        _ => ("Tọa độ màn hình".to_string(), "(không rõ)".to_string()),
    }
}

/// One-shot scope row: every approval covers exactly one
/// execution of the action shown — never an open-ended
/// permission.
fn scope_row(text: &str) -> (String, String) {
    ("Phạm vi".to_string(), text.to_string())
}

/// The target-identity row for the card: window class + owning
/// program + window id. The window TITLE is deliberately never
/// shown (titles can contain secrets; the class + program
/// identify the target for the decision). Matches the
/// no-title-echo rule of the forbidden checks.
pub fn target_row(identity: &WindowIdentity) -> (String, String) {
    let class = identity.class.as_deref().unwrap_or("(không rõ lớp cửa sổ)");
    let exe = identity
        .exe_basename
        .as_deref()
        .unwrap_or("(không rõ chương trình)");
    let id = identity
        .window_id
        .map(|w| format!(" · id: 0x{w:x}"))
        .unwrap_or_default();
    ("Cửa sổ đích".to_string(), format!("{class} · {exe}{id}"))
}

/// Crop a `size`×`size` preview around a screen point from a
/// stored frame, for the approval card. Display only: the crop
/// never feeds the decision logic. Returns RGBA bytes plus the
/// actual crop dimensions (the crop shrinks at frame edges),
/// or `None` when the frame has no usable pixels. Coordinates
/// are screen points; the frame's origin and scale factor map
/// them into frame pixels (identity on X11).
pub fn crop_preview(frame: &Framebuffer, x: i32, y: i32, size: u32) -> Option<(Vec<u8>, u32, u32)> {
    let (width, height) = (frame.width, frame.height);
    if width == 0 || height == 0 || size == 0 {
        return None;
    }
    if frame.data.len() < width as usize * height as usize * 4 {
        return None;
    }
    let scale = if frame.scale_factor.is_finite() && frame.scale_factor > 0.0 {
        frame.scale_factor
    } else {
        1.0
    };
    let center_x = ((i64::from(x) - i64::from(frame.origin.0)) as f64 * scale).round() as i64;
    let center_x = center_x.max(0);
    let center_y = ((i64::from(y) - i64::from(frame.origin.1)) as f64 * scale).round() as i64;
    let center_y = center_y.max(0);
    let half = i64::from(size / 2);
    let max_ox = i64::from(width).saturating_sub(i64::from(size)).max(0);
    let max_oy = i64::from(height).saturating_sub(i64::from(size)).max(0);
    let ox = (center_x - half).clamp(0, max_ox);
    let oy = (center_y - half).clamp(0, max_oy);
    let cw = (i64::from(size)).min(i64::from(width) - ox).max(0) as u32;
    let ch = (i64::from(size)).min(i64::from(height) - oy).max(0) as u32;
    if cw == 0 || ch == 0 {
        return None;
    }
    let mut out = vec![0u8; cw as usize * ch as usize * 4];
    for row in 0..ch {
        let src_y = oy as usize + row as usize;
        let src_start = (src_y * width as usize + ox as usize) * 4;
        let src_end = src_start + cw as usize * 4;
        let dst_start = row as usize * cw as usize * 4;
        out[dst_start..dst_start + cw as usize * 4]
            .copy_from_slice(&frame.data[src_start..src_end]);
    }
    Some((out, cw, ch))
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() > max_chars {
        let kept: String = text.chars().take(max_chars).collect();
        format!("{kept}…")
    } else {
        text.to_string()
    }
}

/// True when the tool acts on a screen point (the card shows a
/// target row for these, and the approver probes the point).
/// Mirrors the approver's point extraction: same tool list.
pub fn is_point_tool(tool: &str) -> bool {
    matches!(
        tool,
        "body_click"
            | "body_move"
            | "body_scroll"
            | "body_drag"
            | "body_double_click"
            | "body_right_click"
    )
}

/// Impact labels (§15.2): one primary label plus secondary
/// labels, from the fixed set đọc / ghi / thực thi / gửi ra
/// ngoài. MCP tools always carry at least "thực thi" — the
/// server's behavior is unknown (§6.2). Unknown tools fail
/// closed to "thực thi". `context` is the card's audit-fields
/// object (it names the MCP server when the tool is an MCP
/// tool); the `mcp_` name prefix is a second signal.
pub fn impact_labels(tool: &str, context: &Value) -> (&'static str, Vec<&'static str>) {
    let is_mcp = context.get("mcp_server").is_some() || tool.starts_with("mcp_");
    if is_mcp {
        return ("thực thi", vec!["gửi ra ngoài"]);
    }
    match tool {
        "body_screenshot" | "notebook_search" | "notebook_read" => ("đọc", Vec::new()),
        "notebook_add" => ("ghi", Vec::new()),
        "execute_code" => ("thực thi", vec!["ghi"]),
        "delegate" => ("thực thi", Vec::new()),
        _ if tool.starts_with("body_") => ("ghi", vec!["thực thi"]),
        _ => ("thực thi", Vec::new()),
    }
}

/// The full SHA-256 of the code an `execute_code` card approves
/// (§15.2): the card shows the shortened form, the expanded
/// view shows this in full, and the post-run audit record
/// (`code_execution`) carries the executed hash plus the
/// verified flag for comparison.
pub fn execute_code_hash(code: &str) -> String {
    sha256_hex(code.as_bytes())
}

/// Shortened hash for the card row: 16 hex characters + an
/// ellipsis. The full hash is one expansion away.
pub fn short_hash(full_hash: &str) -> String {
    if full_hash.len() > 16 {
        format!("{}…", &full_hash[..16])
    } else {
        full_hash.to_string()
    }
}

/// Render the approval card content for one tool call, with the
/// card's audit-fields context (MCP server/tool/version, when
/// the tool is an MCP tool). Phase 6 tools get explicit cards;
/// everything else uses [`approval_detail`].
pub fn approval_detail_with_context(tool: &str, args: &Value, context: &Value) -> ApprovalDetail {
    match tool {
        "execute_code" => {
            let language = arg_str(args, "language").unwrap_or("(không rõ)");
            let code = arg_str(args, "code").unwrap_or("");
            let hash = execute_code_hash(code);
            // F6-06: `inputs` is an ARRAY of {name, content}
            // (the schema in sandbox.rs, enforced by the
            // tool). The old code read it as an OBJECT, so
            // the card always showed "không có" even when
            // inputs were approved. Show each input's name
            // with the short hash of its approved content.
            let input_hashes = execute_code_input_hashes(args);
            let inputs = if input_hashes.is_empty() {
                "không có".to_string()
            } else {
                input_hashes
                    .iter()
                    .map(|h| format!("{} ({})", h.name, short_hash(&h.sha256)))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            ApprovalDetail {
                title: "Chạy code trong sandbox".to_string(),
                rows: vec![
                    ("Ngôn ngữ".to_string(), language.to_string()),
                    ("Độ dài code".to_string(), format!("{} ký tự", code.chars().count())),
                    ("Hash SHA-256".to_string(), short_hash(&hash)),
                    ("Đầu vào".to_string(), truncate(&inputs, 120)),
                    scope_row("Chỉ chạy một lần trong sandbox biệt lập: không mạng, không đọc file ngoài vùng làm việc."),
                    (
                        "Kiểm chứng".to_string(),
                        "Hash code và hash từng file đầu vào là hash đã phê duyệt; sau khi chạy, bản ghi audit xác nhận hash thực thi có khớp hay không.".to_string(),
                    ),
                ],
                reversible: "Code chạy trong sandbox biệt lập; file tạo ra chỉ nằm trong vùng làm việc của lượt này.".to_string(),
            }
        }
        "delegate" => {
            let task = arg_str(args, "task").unwrap_or("(không rõ nhiệm vụ)");
            let steps = args
                .get("max_steps")
                .map(|v| v.to_string())
                .unwrap_or_else(|| "mặc định".to_string());
            let budget = args
                .get("token_budget")
                .map(|v| format!("{} token", v))
                .unwrap_or_else(|| "mặc định (1/4 ngân sách còn lại của cha)".to_string());
            let time = args
                .get("time_limit_secs")
                .map(|v| format!("{v} giây"))
                .unwrap_or_else(|| "mặc định".to_string());
            ApprovalDetail {
                title: "Giao việc cho sub-agent".to_string(),
                rows: vec![
                    ("Nhiệm vụ".to_string(), truncate(task, 200).replace('\n', " ")),
                    ("Số bước tối đa".to_string(), steps),
                    ("Ngân sách token".to_string(), budget),
                    ("Giới hạn thời gian".to_string(), time),
                    scope_row("Chỉ giao một lần; sub-agent chạy dưới cùng hệ thống phê duyệt."),
                ],
                reversible: "Sub-agent chạy trong cùng hệ thống phê duyệt; kết quả của nó chỉ là báo cáo — agent cha đánh giá độc lập trước khi dùng.".to_string(),
            }
        }
        _ if context.get("mcp_server").is_some() || tool.starts_with("mcp_") => {
            let server = context
                .get("mcp_server")
                .and_then(|v| v.as_str())
                .unwrap_or("(không rõ server)");
            let mcp_tool = context
                .get("mcp_tool")
                .and_then(|v| v.as_str())
                .unwrap_or(tool);
            let version = context
                .get("protocol_version")
                .and_then(|v| v.as_str())
                .unwrap_or("(chưa nối)");
            let unsandboxed = context
                .get("unsandboxed")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let mut rows = vec![
                ("Server MCP".to_string(), server.to_string()),
                ("Công cụ".to_string(), mcp_tool.to_string()),
                ("Phiên bản protocol".to_string(), version.to_string()),
                (
                    "Chế độ".to_string(),
                    if unsandboxed {
                        "KHÔNG SANDBOX — server chạy trực tiếp trên máy này".to_string()
                    } else {
                        "Chạy trong sandbox".to_string()
                    },
                ),
            ];
            // Arguments, listed, never hidden.
            if let Some(obj) = args.as_object() {
                let mut keys: Vec<&String> = obj.keys().collect();
                keys.sort();
                for key in keys {
                    let rendered = match &obj[key] {
                        Value::String(s) => truncate(s, 80),
                        other => truncate(&other.to_string(), 80),
                    };
                    rows.push((key.clone(), rendered));
                }
            }
            rows.push(scope_row("Chỉ gọi một lần với các tham số trên."));
            ApprovalDetail {
                title: "Gọi công cụ MCP".to_string(),
                rows,
                reversible: "Không biết server MCP làm gì bên trong — coi như không hoàn tác được."
                    .to_string(),
            }
        }
        _ => approval_detail(tool, args),
    }
}

/// Render the approval card content for one tool call.
pub fn approval_detail(tool: &str, args: &Value) -> ApprovalDetail {
    match tool {
        "notebook_add" => {
            let title = arg_str(args, "title").unwrap_or("(không tiêu đề)");
            let body = arg_str(args, "body").unwrap_or("");
            ApprovalDetail {
                title: "Lưu một ghi chú".to_string(),
                rows: vec![
                    ("Tiêu đề".to_string(), truncate(title, 80)),
                    (
                        "Nội dung".to_string(),
                        truncate(body, 200).replace('\n', " "),
                    ),
                    (
                        "Nơi lưu".to_string(),
                        "Sổ tay trên máy này (cơ sở dữ liệu cục bộ)".to_string(),
                    ),
                ],
                reversible: "Có thể xóa sau trong sổ tay.".to_string(),
            }
        }
        "notebook_search" => ApprovalDetail {
            title: "Tìm trong sổ tay".to_string(),
            rows: vec![
                (
                    "Từ khóa".to_string(),
                    truncate(arg_str(args, "query").unwrap_or(""), 80),
                ),
                (
                    "Phạm vi".to_string(),
                    "Chỉ đọc sổ tay trên máy này; không thay đổi gì.".to_string(),
                ),
            ],
            reversible: "Chỉ đọc — không có gì để hoàn tác.".to_string(),
        },
        "notebook_read" => ApprovalDetail {
            title: "Đọc một ghi chú".to_string(),
            rows: vec![
                (
                    "Ghi chú".to_string(),
                    args.get("id")
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "(không rõ)".to_string()),
                ),
                (
                    "Phạm vi".to_string(),
                    "Chỉ đọc sổ tay trên máy này; không thay đổi gì.".to_string(),
                ),
            ],
            reversible: "Chỉ đọc — không có gì để hoàn tác.".to_string(),
        },
        "body_screenshot" => ApprovalDetail {
            title: "Chụp màn hình".to_string(),
            rows: vec![(
                "Phạm vi".to_string(),
                "Toàn màn hình chính; ảnh chỉ dùng cho lượt này, không gửi đi đâu.".to_string(),
            )],
            reversible: "Chỉ đọc — không có gì để hoàn tác.".to_string(),
        },
        "body_click" => ApprovalDetail {
            title: "Nhấn chuột".to_string(),
            rows: vec![
                point_row(args),
                scope_row("Chỉ một lần nhấn duy nhất tại tọa độ trên."),
                (
                    "Hậu quả".to_string(),
                    "Con trỏ sẽ nhấn tại vị trí đã cho, như một cú nhấn chuột thật.".to_string(),
                ),
            ],
            reversible: "Không hoàn tác được — cú nhấn có thể mở hoặc kích hoạt thứ gì đó."
                .to_string(),
        },
        "body_move" => ApprovalDetail {
            title: "Di chuyển con trỏ".to_string(),
            rows: vec![
                point_row(args),
                scope_row("Chỉ một lần di chuyển tới tọa độ trên."),
            ],
            reversible: "Chỉ di chuyển — không nhấn, không thay đổi gì.".to_string(),
        },
        "body_type_text" => {
            let text = arg_str(args, "text").unwrap_or("");
            ApprovalDetail {
                title: "Gõ văn bản".to_string(),
                rows: vec![
                    (
                        "Văn bản".to_string(),
                        truncate(text, 200).replace('\n', " "),
                    ),
                    ("Số ký tự".to_string(), text.chars().count().to_string()),
                    (
                        "Nơi gõ".to_string(),
                        "Nơi đang được chọn (cửa sổ đang có focus).".to_string(),
                    ),
                    scope_row("Chỉ gõ một lần vào nơi đang được chọn."),
                ],
                reversible: "Có thể xóa văn bản đã gõ, nhưng không tự động hoàn tác.".to_string(),
            }
        }
        "body_key" => ApprovalDetail {
            title: "Nhấn phím".to_string(),
            rows: vec![
                (
                    "Phím".to_string(),
                    arg_str(args, "key").unwrap_or("(không rõ)").to_string(),
                ),
                scope_row("Chỉ một lần nhấn phím này."),
            ],
            reversible: "Một lần nhấn phím — có thể kích hoạt phím tắt.".to_string(),
        },
        "body_scroll" => {
            let dx = arg_int(args, "dx").unwrap_or(0);
            let dy = arg_int(args, "dy").unwrap_or(0);
            let vertical = if dy > 0 {
                format!("xuống {dy} nấc")
            } else if dy < 0 {
                format!("lên {} nấc", -dy)
            } else {
                "không cuộn dọc".to_string()
            };
            let horizontal = if dx > 0 {
                format!("phải {dx} nấc")
            } else if dx < 0 {
                format!("trái {} nấc", -dx)
            } else {
                "không cuộn ngang".to_string()
            };
            ApprovalDetail {
                title: "Cuộn".to_string(),
                rows: vec![
                    point_row(args),
                    ("Dọc".to_string(), vertical),
                    ("Ngang".to_string(), horizontal),
                    scope_row("Chỉ một lần cuộn tại tọa độ trên."),
                ],
                reversible: "Chỉ cuộn nội dung — không thay đổi gì.".to_string(),
            }
        }
        "body_drag" => {
            let from = match (arg_int(args, "from_x"), arg_int(args, "from_y")) {
                (Some(x), Some(y)) => format!("({x}, {y})"),
                _ => "(không rõ)".to_string(),
            };
            let to = match (arg_int(args, "to_x"), arg_int(args, "to_y")) {
                (Some(x), Some(y)) => format!("({x}, {y})"),
                _ => "(không rõ)".to_string(),
            };
            ApprovalDetail {
                title: "Kéo chuột".to_string(),
                rows: vec![
                    ("Từ (tọa độ màn hình)".to_string(), from),
                    ("Đến (tọa độ màn hình)".to_string(), to),
                    scope_row("Chỉ một lần kéo từ điểm bắt đầu tới điểm kết thúc."),
                ],
                reversible: "Không hoàn tác được — có thể di chuyển hoặc chọn thứ gì đó."
                    .to_string(),
            }
        }
        "body_double_click" => ApprovalDetail {
            title: "Nhấn đúp chuột".to_string(),
            rows: vec![
                point_row(args),
                scope_row("Chỉ một lần nhấn đúp tại tọa độ trên."),
            ],
            reversible: "Không hoàn tác được — nhấn đúp có thể mở thứ gì đó.".to_string(),
        },
        "body_right_click" => ApprovalDetail {
            title: "Nhấn chuột phải".to_string(),
            rows: vec![
                point_row(args),
                scope_row("Chỉ một lần nhấn phải tại tọa độ trên."),
            ],
            reversible: "Thường mở menu ngữ cảnh — không thay đổi gì cho tới khi chọn.".to_string(),
        },
        "body_hold_key" => ApprovalDetail {
            title: "Giữ phím".to_string(),
            rows: vec![
                (
                    "Phím".to_string(),
                    arg_str(args, "key").unwrap_or("(không rõ)").to_string(),
                ),
                (
                    "Thời gian".to_string(),
                    match arg_int(args, "millis") {
                        Some(millis) => format!("{millis} ms"),
                        None => "(không rõ)".to_string(),
                    },
                ),
                scope_row("Chỉ một lần giữ phím trong thời gian trên."),
            ],
            reversible: "Phím được giữ rồi nhả — có thể kích hoạt phím tắt giữ phím.".to_string(),
        },
        _ => body_action_detail(tool, args),
    }
}

/// Generic rendering for future body actions and unknown tools: the
/// action, its target, its scope, and the expected consequence, from
/// the args themselves. Unknown args are listed as key/value rows
/// (truncated), never hidden.
fn body_action_detail(tool: &str, args: &Value) -> ApprovalDetail {
    let action = arg_str(args, "action").unwrap_or(tool);
    let (title, consequence, reversible) = match action {
        "screenshot" => (
            "Chụp màn hình",
            "Một ảnh chụp màn hình sẽ được tạo để Mimint nhìn thấy.",
            "Ảnh chỉ dùng cho lượt này; không gửi đi đâu.",
        ),
        "click" => (
            "Nhấn chuột",
            "Con trỏ sẽ nhấn tại vị trí đã cho, như một cú nhấn chuột thật.",
            "Không hoàn tác được — cú nhấn có thể mở hoặc kích hoạt thứ gì đó.",
        ),
        "type" => (
            "Gõ văn bản",
            "Văn bản sẽ được gõ vào nơi đang được chọn, như gõ phím thật.",
            "Có thể xóa văn bản đã gõ, nhưng không tự động hoàn tác.",
        ),
        "delete" => (
            "Xóa",
            "Mục tiêu sẽ bị xóa.",
            "Không hoàn tác được — hãy chắc chắn trước khi cho phép.",
        ),
        "send" => (
            "Gửi",
            "Nội dung sẽ được gửi đi.",
            "Không thu hồi được sau khi gửi.",
        ),
        "config" => (
            "Đổi cài đặt",
            "Một cài đặt sẽ bị thay đổi.",
            "Có thể đổi lại sau, nhưng không tự động hoàn tác.",
        ),
        _ => (
            "Hành động của công cụ",
            "Công cụ sẽ chạy với các tham số dưới đây.",
            "Chưa rõ có hoàn tác được không — hãy đọc kỹ tham số.",
        ),
    };
    let mut rows = vec![("Hành động".to_string(), action.to_string())];
    if let Some(target) = arg_str(args, "target") {
        rows.push(("Mục tiêu".to_string(), truncate(target, 80)));
    }
    if let (Some(x), Some(y)) = (args.get("x"), args.get("y")) {
        rows.push(("Tọa độ màn hình".to_string(), format!("({x}, {y})")));
    }
    if let Some(text) = arg_str(args, "text") {
        rows.push((
            "Văn bản".to_string(),
            format!("{} ký tự", text.chars().count()),
        ));
    }
    rows.push(("Hậu quả".to_string(), consequence.to_string()));
    rows.push(scope_row("Chỉ chạy một lần với các tham số trên."));
    // Any other args, listed, never hidden.
    if let Some(obj) = args.as_object() {
        for (key, value) in obj {
            if ["action", "target", "x", "y", "text"].contains(&key.as_str()) {
                continue;
            }
            let rendered = match value {
                Value::String(s) => truncate(s, 80),
                other => truncate(&other.to_string(), 80),
            };
            rows.push((key.clone(), rendered));
        }
    }
    ApprovalDetail {
        title: title.to_string(),
        rows,
        reversible: reversible.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn notebook_add_card_shows_title_and_body() {
        let detail = approval_detail(
            "notebook_add",
            &json!({"title": "Mua sữa", "body": "2 hộp, loại ít đường"}),
        );
        assert_eq!(detail.title, "Lưu một ghi chú");
        assert!(detail
            .rows
            .iter()
            .any(|(k, v)| k == "Tiêu đề" && v == "Mua sữa"));
        assert!(detail
            .rows
            .iter()
            .any(|(k, v)| k == "Nội dung" && v.contains("2 hộp")));
    }

    #[test]
    fn read_only_tools_say_so() {
        let detail = approval_detail("notebook_search", &json!({"query": "sữa"}));
        assert!(detail.reversible.contains("Chỉ đọc"));
    }

    #[test]
    fn click_shape_names_the_consequence() {
        let detail = approval_detail("body_act", &json!({"action": "click", "x": 100, "y": 200}));
        assert_eq!(detail.title, "Nhấn chuột");
        assert!(detail
            .rows
            .iter()
            .any(|(k, v)| k == "Tọa độ màn hình" && v == "(100, 200)"));
        assert!(detail.reversible.contains("Không hoàn tác"));
    }

    #[test]
    fn unknown_args_are_listed_not_hidden() {
        let detail = approval_detail("mystery_tool", &json!({"frobnicate": true}));
        assert!(detail.rows.iter().any(|(k, _)| k == "frobnicate"));
    }

    #[test]
    fn body_click_card_shows_the_exact_point() {
        let detail = approval_detail("body_click", &json!({"x": 260, "y": 80}));
        assert_eq!(detail.title, "Nhấn chuột");
        assert!(detail
            .rows
            .iter()
            .any(|(k, v)| k == "Tọa độ màn hình" && v == "(260, 80)"));
        assert!(detail.reversible.contains("Không hoàn tác"));
        assert!(detail
            .rows
            .iter()
            .any(|(k, v)| k == "Phạm vi" && v.contains("một lần")));
    }

    #[test]
    fn body_type_text_card_shows_the_actual_text() {
        let detail = approval_detail("body_type_text", &json!({"text": "Xin chào"}));
        assert_eq!(detail.title, "Gõ văn bản");
        assert!(detail
            .rows
            .iter()
            .any(|(k, v)| k == "Văn bản" && v == "Xin chào"));
        assert!(detail.rows.iter().any(|(k, v)| k == "Số ký tự" && v == "8"));
    }

    #[test]
    fn body_scroll_card_names_the_directions() {
        let detail = approval_detail("body_scroll", &json!({"x": 1, "y": 2, "dx": -1, "dy": 3}));
        assert_eq!(detail.title, "Cuộn");
        assert!(detail
            .rows
            .iter()
            .any(|(k, v)| k == "Dọc" && v == "xuống 3 nấc"));
        assert!(detail
            .rows
            .iter()
            .any(|(k, v)| k == "Ngang" && v == "trái 1 nấc"));
    }

    #[test]
    fn body_hold_key_card_shows_key_and_duration() {
        let detail = approval_detail("body_hold_key", &json!({"key": "f5", "millis": 150}));
        assert_eq!(detail.title, "Giữ phím");
        assert!(detail.rows.iter().any(|(k, v)| k == "Phím" && v == "f5"));
        assert!(detail
            .rows
            .iter()
            .any(|(k, v)| k == "Thời gian" && v == "150 ms"));
    }

    #[test]
    fn body_drag_card_shows_both_ends() {
        let detail = approval_detail(
            "body_drag",
            &json!({"from_x": 10, "from_y": 20, "to_x": 30, "to_y": 40}),
        );
        assert_eq!(detail.title, "Kéo chuột");
        assert!(detail
            .rows
            .iter()
            .any(|(k, v)| k == "Từ (tọa độ màn hình)" && v == "(10, 20)"));
        assert!(detail
            .rows
            .iter()
            .any(|(k, v)| k == "Đến (tọa độ màn hình)" && v == "(30, 40)"));
    }

    #[test]
    fn target_row_names_class_and_program_but_never_the_title() {
        let identity = WindowIdentity {
            window_id: Some(0x2a),
            class: Some("editor".to_string()),
            title: Some("secret-note.txt — mật khẩu ngân hàng".to_string()),
            exe_basename: Some("code".to_string()),
        };
        let (label, value) = target_row(&identity);
        assert_eq!(label, "Cửa sổ đích");
        assert!(value.contains("editor"), "{value}");
        assert!(value.contains("code"), "{value}");
        assert!(value.contains("0x2a"), "{value}");
        assert!(!value.contains("mật khẩu"), "{value}");
        assert!(!value.contains("secret-note"), "{value}");
    }

    #[test]
    fn crop_preview_centers_clamps_and_scales() {
        // 4x4 frame, pixel (x, y) = R=x, G=y, B=0, A=255.
        let mut data = vec![0u8; 4 * 4 * 4];
        for y in 0..4u8 {
            for x in 0..4u8 {
                let i = (usize::from(y) * 4 + usize::from(x)) * 4;
                data[i] = x;
                data[i + 1] = y;
                data[i + 3] = 255;
            }
        }
        let frame = Framebuffer::new(4, 4, data).unwrap();
        // Centered 2x2 crop at (2, 2): pixels (1,1),(2,1),(1,2),(2,2).
        let (crop, w, h) = crop_preview(&frame, 2, 2, 2).unwrap();
        assert_eq!((w, h), (2, 2));
        assert_eq!(&crop[0..4], &[1, 1, 0, 255]);
        assert_eq!(&crop[4..8], &[2, 1, 0, 255]);
        assert_eq!(&crop[8..12], &[1, 2, 0, 255]);
        assert_eq!(&crop[12..16], &[2, 2, 0, 255]);
        // Edge clamp: crop at (0, 0) stays inside the frame.
        let (crop, w, h) = crop_preview(&frame, 0, 0, 2).unwrap();
        assert_eq!((w, h), (2, 2));
        assert_eq!(&crop[0..4], &[0, 0, 0, 255]);
        // Oversize request shrinks to the frame.
        let (_, w, h) = crop_preview(&frame, 2, 2, 120).unwrap();
        assert_eq!((w, h), (4, 4));
        // Scale 2.0: point (1, 1) maps to pixel (2, 2).
        let mut scaled = Framebuffer::new(4, 4, vec![7u8; 4 * 4 * 4]).unwrap();
        scaled.scale_factor = 2.0;
        let (crop, w, h) = crop_preview(&scaled, 1, 1, 2).unwrap();
        assert_eq!((w, h), (2, 2));
        assert_eq!(crop.len(), 16);
        // Empty frame: no preview.
        let empty = Framebuffer::new(0, 0, Vec::new()).unwrap();
        assert!(crop_preview(&empty, 0, 0, 120).is_none());
    }

    #[test]
    fn impact_labels_follow_the_fixed_set() {
        let empty = json!({});
        assert_eq!(
            impact_labels("body_screenshot", &empty),
            ("đọc", Vec::new())
        );
        assert_eq!(
            impact_labels("notebook_search", &empty),
            ("đọc", Vec::new())
        );
        assert_eq!(impact_labels("notebook_add", &empty), ("ghi", Vec::new()));
        assert_eq!(
            impact_labels("body_click", &empty),
            ("ghi", vec!["thực thi"])
        );
        assert_eq!(
            impact_labels("execute_code", &empty),
            ("thực thi", vec!["ghi"])
        );
        assert_eq!(impact_labels("delegate", &empty), ("thực thi", Vec::new()));
        // Unknown tools fail closed to "thực thi".
        assert_eq!(
            impact_labels("mystery_tool", &empty),
            ("thực thi", Vec::new())
        );
        // MCP tools always carry at least "thực thi", via the
        // context or via the name prefix.
        let mcp_ctx = json!({"mcp_server": "files", "mcp_tool": "read"});
        assert_eq!(
            impact_labels("mcp_files_read", &mcp_ctx),
            ("thực thi", vec!["gửi ra ngoài"])
        );
        assert_eq!(
            impact_labels("mcp_files_read", &empty),
            ("thực thi", vec!["gửi ra ngoài"])
        );
        assert_eq!(
            impact_labels("something_else", &mcp_ctx),
            ("thực thi", vec!["gửi ra ngoài"])
        );
    }

    #[test]
    fn execute_code_card_shows_language_length_and_short_hash() {
        let code = "print('hello')";
        let detail = approval_detail_with_context(
            "execute_code",
            &json!({"language": "python3", "code": code}),
            &json!({}),
        );
        assert_eq!(detail.title, "Chạy code trong sandbox");
        assert!(detail
            .rows
            .iter()
            .any(|(k, v)| k == "Ngôn ngữ" && v == "python3"));
        assert!(detail
            .rows
            .iter()
            .any(|(k, v)| k == "Độ dài code" && v == "14 ký tự"));
        let full = execute_code_hash(code);
        assert_eq!(full.len(), 64);
        assert!(detail
            .rows
            .iter()
            .any(|(k, v)| k == "Hash SHA-256" && *v == short_hash(&full) && v.len() < full.len()));
        // The full hash is the real SHA-256 of the approved code.
        assert_eq!(full, sha256_hex(code.as_bytes()));
        assert!(detail.rows.iter().any(|(k, _)| k == "Kiểm chứng"));
    }

    #[test]
    fn execute_code_card_shows_input_names_and_hashes() {
        // F6-06 regression: inputs arrive as an array; the
        // card must name each input with its content hash,
        // not "không có".
        let detail = approval_detail_with_context(
            "execute_code",
            &json!({
                "language": "python3",
                "code": "print(open('data.txt').read())",
                "inputs": [{"name": "data.txt", "content": "hello"}],
            }),
            &json!({}),
        );
        let inputs_row = detail
            .rows
            .iter()
            .find(|(k, _)| k == "Đầu vào")
            .expect("Đầu vào row");
        assert!(inputs_row.1.contains("data.txt"), "{}", inputs_row.1);
        let expected = short_hash(&sha256_hex(b"hello"));
        assert!(inputs_row.1.contains(&expected), "{}", inputs_row.1);
        assert!(!inputs_row.1.contains("không có"));
        // No inputs: still "không có".
        let detail = approval_detail_with_context(
            "execute_code",
            &json!({"language": "sh", "code": "true"}),
            &json!({}),
        );
        assert!(detail
            .rows
            .iter()
            .any(|(k, v)| k == "Đầu vào" && v == "không có"));
    }

    #[test]
    fn delegate_card_shows_the_task_and_limits() {
        let detail = approval_detail_with_context(
            "delegate",
            &json!({"task": "tìm tài liệu", "max_steps": 5}),
            &json!({}),
        );
        assert_eq!(detail.title, "Giao việc cho sub-agent");
        assert!(detail
            .rows
            .iter()
            .any(|(k, v)| k == "Nhiệm vụ" && v == "tìm tài liệu"));
        assert!(detail
            .rows
            .iter()
            .any(|(k, v)| k == "Số bước tối đa" && v == "5"));
        assert!(detail.reversible.contains("chỉ là báo cáo"));
    }

    #[test]
    fn mcp_card_names_server_tool_and_unsandboxed_mode() {
        let ctx = json!({
            "mcp_server": "files",
            "mcp_tool": "read",
            "protocol_version": "2025-06-18",
            "unsandboxed": true,
        });
        let detail =
            approval_detail_with_context("mcp_files_read", &json!({"path": "/tmp/x"}), &ctx);
        assert_eq!(detail.title, "Gọi công cụ MCP");
        assert!(detail
            .rows
            .iter()
            .any(|(k, v)| k == "Server MCP" && v == "files"));
        assert!(detail
            .rows
            .iter()
            .any(|(k, v)| k == "Công cụ" && v == "read"));
        assert!(detail
            .rows
            .iter()
            .any(|(k, v)| k == "Phiên bản protocol" && v == "2025-06-18"));
        assert!(detail
            .rows
            .iter()
            .any(|(k, v)| k == "Chế độ" && v.contains("KHÔNG SANDBOX")));
        assert!(detail
            .rows
            .iter()
            .any(|(k, v)| k == "path" && v == "/tmp/x"));
        // Sandboxed servers say so.
        let ctx = json!({
            "mcp_server": "files",
            "mcp_tool": "read",
            "protocol_version": "2025-06-18",
            "unsandboxed": false,
        });
        let detail = approval_detail_with_context("mcp_files_read", &json!({}), &ctx);
        assert!(detail
            .rows
            .iter()
            .any(|(k, v)| k == "Chế độ" && v == "Chạy trong sandbox"));
    }
}
