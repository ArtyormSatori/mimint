//! User-facing error messages (§15.4, F6D-19): the five
//! situations, each in the mandatory three parts — (1) what
//! happened, (2) what it affects, (3) what to do next. The
//! wording is fixed by the design; the technical code stays in
//! the audit log and the expandable detail, never as the main
//! content. No message ever suggests disabling the sandbox, and
//! no message shows a precise-looking USD figure when only a
//! token reserve exists.

/// The five error situations (§15.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorSituation {
    /// `sandbox_unavailable`: code cannot run.
    SandboxUnavailable,
    /// `body_queue_full`: the action queue is full.
    QueueFull,
    /// `budget_exhausted`: the token budget ran out.
    BudgetExhausted(BudgetScope),
    /// `queue_sla_expired`: an action waited out its SLA.
    QueueSlaExpired,
    /// `task_end_unverified`: a cancelled task's end could not
    /// be verified.
    TaskEndUnverified,
}

/// Which budget ran out (§15.4 situation 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetScope {
    /// Only this agent's budget.
    Agent,
    /// The global cap: every agent stops.
    Global,
    /// Consumption could not be measured: stop cautiously.
    Unmeasured,
}

/// One rendered message: the three mandatory parts plus the
/// technical code for the expandable detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorMessage {
    pub title: &'static str,
    /// (1) What happened.
    pub what: &'static str,
    /// (2) What it affects.
    pub impact: &'static str,
    /// (3) What to do next.
    pub next: &'static str,
    /// Technical code: audit + expandable detail only.
    pub tech_code: &'static str,
}

/// The fixed message for one situation. Wording is verbatim
/// from §15.4 (the bracketed alternatives resolved per scope).
pub fn error_message(situation: ErrorSituation) -> ErrorMessage {
    match situation {
        ErrorSituation::SandboxUnavailable => ErrorMessage {
            title: "Sandbox không khả dụng",
            what: "Không chạy được code vì sandbox chưa sẵn sàng trên máy này.",
            impact: "Lệnh `execute_code` đã bị từ chối; không có code nào chạy ngoài sandbox.",
            next: "Kiểm tra cài đặt sandbox (Linux: bubblewrap) rồi thử lại; trên macOS/Windows, sandbox chưa được kiểm chứng — xem cổng mở §13. Không có đường tắt nào.",
            tech_code: "sandbox_unavailable",
        },
        ErrorSituation::QueueFull => ErrorMessage {
            title: "Hàng đợi đầy",
            what: "Hàng đợi action đã đầy.",
            impact: "Action mới đã bị từ chối ngay, không xếp hàng thêm; các action đang chờ không bị mất.",
            next: "Chờ actor xử lý bớt rồi gửi lại, hoặc dừng bớt agent đang chạy.",
            tech_code: "body_queue_full",
        },
        ErrorSituation::BudgetExhausted(BudgetScope::Agent) => ErrorMessage {
            title: "Hết ngân sách",
            what: "Ngân sách token đã hết của agent này.",
            impact: "Chỉ agent này dừng. Số hiển thị là ƯỚC TÍNH, không phải hóa đơn.",
            next: "Tăng ngân sách trong cấu hình rồi chạy lượt mới, hoặc chấp nhận dừng tại đây.",
            tech_code: "budget_exhausted",
        },
        ErrorSituation::BudgetExhausted(BudgetScope::Global) => ErrorMessage {
            title: "Hết ngân sách",
            what: "Ngân sách token đã hết của toàn hệ thống.",
            impact: "Tất cả agent dừng. Số hiển thị là ƯỚC TÍNH, không phải hóa đơn.",
            next: "Tăng ngân sách trong cấu hình rồi chạy lượt mới, hoặc chấp nhận dừng tại đây.",
            tech_code: "budget_exhausted",
        },
        ErrorSituation::BudgetExhausted(BudgetScope::Unmeasured) => ErrorMessage {
            title: "Hết ngân sách",
            what: "Ngân sách token đã hết: không đo được mức tiêu thụ nên phải dừng thận trọng.",
            impact: "Lượt dừng để tránh vượt chi phí không kiểm soát. Số hiển thị là ƯỚC TÍNH, không phải hóa đơn.",
            next: "Tăng ngân sách trong cấu hình rồi chạy lượt mới, hoặc chấp nhận dừng tại đây.",
            tech_code: "budget_exhausted",
        },
        ErrorSituation::QueueSlaExpired => ErrorMessage {
            title: "Hết hạn SLA",
            what: "Action đã chờ quá lâu trong hàng đợi nên hết hạn.",
            impact: "Action không chạy (chạy muộn lên màn hình đã đổi sẽ sai); agent sẽ chụp lại màn hình mới.",
            next: "Không cần làm gì — lượt sẽ tự tiếp tục với trạng thái mới; nếu lặp lại, giảm số agent hoặc tăng SLA trong cấu hình (thời gian chờ tối đa, §12).",
            tech_code: "queue_sla_expired",
        },
        ErrorSituation::TaskEndUnverified => ErrorMessage {
            title: "Kết thúc không xác minh được",
            what: "Không xác minh được agent đã dừng hẳn sau lệnh hủy.",
            impact: "Hệ thống chuyển sang Degraded: không spawn agent mới cho tới khi khởi động lại; kết quả của agent đó là \"chưa xác định\".",
            next: "Khởi động lại ứng dụng để về trạng thái sạch; kiểm tra audit để biết action cuối cùng đã biết chắc tới đâu.",
            tech_code: "task_end_unverified",
        },
    }
}

/// Map a technical error code (from an audit record or a tool
/// error prefix) to its situation, when the code is one of the
/// five. Budget scope defaults to `Agent`: the caller refines
/// it from the ledger record when the scope is known.
pub fn situation_for_code(code: &str) -> Option<ErrorSituation> {
    match code {
        "sandbox_unavailable" => Some(ErrorSituation::SandboxUnavailable),
        "body_queue_full" => Some(ErrorSituation::QueueFull),
        "budget_exhausted" => Some(ErrorSituation::BudgetExhausted(BudgetScope::Agent)),
        "queue_sla_expired" => Some(ErrorSituation::QueueSlaExpired),
        "task_end_unverified" => Some(ErrorSituation::TaskEndUnverified),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_situations() -> Vec<ErrorSituation> {
        vec![
            ErrorSituation::SandboxUnavailable,
            ErrorSituation::QueueFull,
            ErrorSituation::BudgetExhausted(BudgetScope::Agent),
            ErrorSituation::BudgetExhausted(BudgetScope::Global),
            ErrorSituation::BudgetExhausted(BudgetScope::Unmeasured),
            ErrorSituation::QueueSlaExpired,
            ErrorSituation::TaskEndUnverified,
        ]
    }

    /// §15.6 `ui_error_messages` (F6D-19): all five situations
    /// carry all three parts; no message suggests disabling the
    /// sandbox; no message shows USD when only a reserve exists.
    #[test]
    fn ui_error_messages() {
        for situation in all_situations() {
            let message = error_message(situation);
            assert!(!message.title.is_empty(), "{situation:?}");
            assert!(!message.what.is_empty(), "{situation:?}");
            assert!(!message.impact.is_empty(), "{situation:?}");
            assert!(!message.next.is_empty(), "{situation:?}");
            assert!(!message.tech_code.is_empty(), "{situation:?}");
            let joined = format!(
                "{} {} {} {}",
                message.title, message.what, message.impact, message.next
            );
            // Never suggest disabling the sandbox.
            for forbidden in ["tắt sandbox", "tắt bubblewrap", "disable the sandbox"] {
                assert!(
                    !joined.to_lowercase().contains(forbidden),
                    "{situation:?} suggests disabling the sandbox: {joined}"
                );
            }
            // Never a precise-looking USD figure: no dollar
            // sign, no USD string in the user-facing parts.
            assert!(
                !joined.contains('$'),
                "{situation:?} shows a dollar figure: {joined}"
            );
            assert!(!joined.contains("USD"), "{situation:?} shows USD: {joined}");
        }
        // The budget message always carries the ESTIMATE label.
        for scope in [
            BudgetScope::Agent,
            BudgetScope::Global,
            BudgetScope::Unmeasured,
        ] {
            let message = error_message(ErrorSituation::BudgetExhausted(scope));
            assert!(
                message.impact.contains("ƯỚC TÍNH"),
                "budget impact must carry the estimate label: {}",
                message.impact
            );
        }
        // The sandbox message states the fail-closed fact.
        let sandbox = error_message(ErrorSituation::SandboxUnavailable);
        assert!(sandbox
            .impact
            .contains("không có code nào chạy ngoài sandbox"));
        assert!(sandbox.next.contains("Không có đường tắt nào"));
        // Code mapping covers all five technical codes.
        for code in [
            "sandbox_unavailable",
            "body_queue_full",
            "budget_exhausted",
            "queue_sla_expired",
            "task_end_unverified",
        ] {
            assert!(situation_for_code(code).is_some(), "{code}");
        }
        assert!(situation_for_code("something_else").is_none());
    }

    /// Gate: the production part of this file (before the test
    /// module) must not contain a suggestion to disable the
    /// sandbox, even in a comment — the prohibition is total.
    #[test]
    fn ui_error_messages_source_has_no_sandbox_disable_suggestion() {
        let path = format!("{}/src/error_messages.rs", env!("CARGO_MANIFEST_DIR"));
        let source = std::fs::read_to_string(&path).expect("read own source");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("production part");
        for forbidden in [
            "tắt sandbox",
            "tắt bubblewrap",
            "disable the sandbox",
            "disable sandbox",
        ] {
            assert!(
                !production.to_lowercase().contains(forbidden),
                "production source suggests disabling the sandbox: {forbidden}"
            );
        }
    }
}
