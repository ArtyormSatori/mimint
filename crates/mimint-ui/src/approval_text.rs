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
        (Some(x), Some(y)) => ("Vị trí".to_string(), format!("({x}, {y})")),
        _ => ("Vị trí".to_string(), "(không rõ)".to_string()),
    }
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
            rows: vec![point_row(args)],
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
                ],
                reversible: "Có thể xóa văn bản đã gõ, nhưng không tự động hoàn tác.".to_string(),
            }
        }
        "body_key" => ApprovalDetail {
            title: "Nhấn phím".to_string(),
            rows: vec![(
                "Phím".to_string(),
                arg_str(args, "key").unwrap_or("(không rõ)").to_string(),
            )],
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
                rows: vec![("Từ".to_string(), from), ("Đến".to_string(), to)],
                reversible: "Không hoàn tác được — có thể di chuyển hoặc chọn thứ gì đó."
                    .to_string(),
            }
        }
        "body_double_click" => ApprovalDetail {
            title: "Nhấn đúp chuột".to_string(),
            rows: vec![point_row(args)],
            reversible: "Không hoàn tác được — nhấn đúp có thể mở thứ gì đó.".to_string(),
        },
        "body_right_click" => ApprovalDetail {
            title: "Nhấn chuột phải".to_string(),
            rows: vec![point_row(args)],
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

    #[test]
    fn body_click_card_shows_the_exact_point() {
        let detail = approval_detail("body_click", &json!({"x": 260, "y": 80}));
        assert_eq!(detail.title, "Nhấn chuột");
        assert!(detail
            .rows
            .iter()
            .any(|(k, v)| k == "Vị trí" && v == "(260, 80)"));
        assert!(detail.reversible.contains("Không hoàn tác"));
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
            .any(|(k, v)| k == "Từ" && v == "(10, 20)"));
        assert!(detail
            .rows
            .iter()
            .any(|(k, v)| k == "Đến" && v == "(30, 40)"));
    }
}
