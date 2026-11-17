//! Approval card content: what the user sees before deciding.
//!
//! The card shows ACTION-SPECIFIC content, not a raw JSON dump: what
//! will happen, to what target, with what scope, what consequence to
//! expect, and whether it can be undone. The values shown are the
//! values executed — the card renders the same immutable `args` the
//! executor will run, so "what you see" cannot drift from "what
//! runs".
//!
//! Only three tools exist in Phase 3 (`notebook_search`,
//! `notebook_read`, `notebook_add`); the body-action shapes
//! (screenshot/click/type/delete/send/config) are rendered
//! generically from an `action` field so the Phase 4 body tool can
//! reuse this module without a rewrite. Nothing here claims a body
//! tool exists today.

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

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() > max_chars {
        let kept: String = text.chars().take(max_chars).collect();
        format!("{kept}…")
    } else {
        text.to_string()
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
        rows.push(("Vị trí".to_string(), format!("({x}, {y})")));
    }
    if let Some(text) = arg_str(args, "text") {
        rows.push((
            "Văn bản".to_string(),
            format!("{} ký tự", text.chars().count()),
        ));
    }
    rows.push(("Hậu quả".to_string(), consequence.to_string()));
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
            .any(|(k, v)| k == "Vị trí" && v == "(100, 200)"));
        assert!(detail.reversible.contains("Không hoàn tác"));
    }

    #[test]
    fn unknown_args_are_listed_not_hidden() {
        let detail = approval_detail("mystery_tool", &json!({"frobnicate": true}));
        assert!(detail.rows.iter().any(|(k, _)| k == "frobnicate"));
    }
}
