//! Chat state machine: messages, input, pending approval, and the
//! computer-use control lamp. Pure Rust, no egui dependency.
//!
//! The brain starts a fresh message list on every turn (Phase 1/2
//! behavior); this module only DISPLAYS the conversation history.
//! Multi-turn context is Phase 5 work.
//!
//! The control lamp has six states (F3-21). In Phase 3 no
//! body-driving code exists, so `Controlling` / `Stopping` are
//! unreachable in practice — but the state machine is implemented
//! and tested now so Phase 4 only has to drive it, and the lamp
//! honestly shows `Idle` ("Chưa điều khiển") until then.

use crate::worker::FailureKind;
use serde_json::Value;
use std::time::Instant;

/// Who a displayed message is from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatRole {
    User,
    Assistant,
    /// System notices (backend errors, onboarding suggestions).
    System,
}

/// One displayed message.
#[derive(Debug, Clone, PartialEq)]
pub struct UiMessage {
    pub role: ChatRole,
    pub text: String,
    /// Tool names the turn executed, for assistant messages.
    pub tools_used: Vec<String>,
}

/// A guard approval waiting on the user. The `id` is the guard
/// executor's request id — the same id binds the request, the card,
/// the decision, and the audit record.
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalRequest {
    pub id: u64,
    pub tool: String,
    pub args: Value,
    /// When the worker times this request out (the card shows a
    /// countdown; an expired card is cleared, never answered late).
    pub deadline: Option<Instant>,
}

/// What a running turn is doing, as far as the UI can honestly tell.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum TurnStatus {
    #[default]
    Idle,
    /// Submitted, no backend attempt reported yet.
    Connecting,
    /// The chain is trying this backend (attempts arrive in order).
    TryingBackend(String),
}

/// The six-state control lamp (F3-21).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlState {
    /// No computer-use session. The honest Phase 3 default.
    Idle,
    /// A guard approval card is waiting for a decision.
    AwaitingApproval,
    /// A computer-use session is actively driving the machine.
    Controlling,
    /// STOP was pressed; the session is winding down.
    Stopping,
    /// Stopped, latched until the user actively resumes.
    Stopped,
    /// A control/worker fault (panic, disconnect).
    Error,
}

impl ControlState {
    /// The lamp label, with its icon. Text + icon, always visible.
    pub fn label(&self) -> &'static str {
        match self {
            ControlState::Idle => "● Chưa điều khiển thiết bị",
            ControlState::AwaitingApproval => "● Chờ xác nhận",
            ControlState::Controlling => "● Đang điều khiển",
            ControlState::Stopping => "● Đang dừng",
            ControlState::Stopped => "● Đã dừng",
            ControlState::Error => "● Lỗi",
        }
    }
}

/// Chat + control state for one window.
#[derive(Debug, Default)]
pub struct ChatState {
    pub messages: Vec<UiMessage>,
    pub input: String,
    /// A turn is running in the worker.
    pub busy: bool,
    /// What the running turn is doing (for the status line).
    pub status: TurnStatus,
    /// The approval currently on screen, if any.
    pub pending_approval: Option<ApprovalRequest>,
    /// Computer-use session active (lamp lit). Never true in Phase 3.
    pub control_active: bool,
    /// The user pressed STOP during an active session.
    pub stop_requested: bool,
    /// STOP NOW latch: set until the user actively resumes. Blocks
    /// new turns while set.
    pub stopped: bool,
    /// The BODY's emergency stop tripped (hotkey or input while a
    /// card waited). Unlike `stopped`, this does NOT clear on
    /// resume: the body's flag latches until the app restarts.
    /// Blocks new turns while set, same as `stopped`.
    pub body_stopped: bool,
    /// A control/worker fault is showing (lamp Error state).
    pub control_error: bool,
    /// Last turn error, shown until the next submit.
    pub last_error: Option<String>,
    /// Display classification of the last error, when known.
    pub last_error_kind: Option<FailureKind>,
    /// The last submitted prompt, for Retry / "Reply in Vietnamese".
    /// Kept across errors and cancellations: the content is never
    /// silently lost.
    pub last_prompt: Option<String>,
}

impl ChatState {
    pub fn new() -> Self {
        Self::default()
    }

    /// The lamp state, derived from the fields above. Priority:
    /// fault > stopped > awaiting approval > stopping > controlling
    /// > idle.
    pub fn control_state(&self) -> ControlState {
        if self.control_error {
            ControlState::Error
        } else if self.stopped || self.body_stopped {
            ControlState::Stopped
        } else if self.pending_approval.is_some() {
            ControlState::AwaitingApproval
        } else if self.stop_requested {
            ControlState::Stopping
        } else if self.control_active {
            ControlState::Controlling
        } else {
            ControlState::Idle
        }
    }

    /// Submit the input box. Returns the prompt when a turn may start:
    /// non-empty input, no turn running, no approval pending, not
    /// stopped. The input is cleared and a user message is appended.
    pub fn submit_input(&mut self) -> Option<String> {
        if self.busy || self.pending_approval.is_some() || self.stopped || self.body_stopped {
            return None;
        }
        let prompt = self.input.trim().to_string();
        if prompt.is_empty() {
            return None;
        }
        self.input.clear();
        self.last_error = None;
        self.last_error_kind = None;
        self.last_prompt = Some(prompt.clone());
        self.messages.push(UiMessage {
            role: ChatRole::User,
            text: prompt.clone(),
            tools_used: Vec::new(),
        });
        self.busy = true;
        self.status = TurnStatus::Connecting;
        Some(prompt)
    }

    /// Re-run the last prompt (Retry / "Reply in Vietnamese"). The
    /// prompt is NOT appended again as a user message — it is
    /// already in the history; the app adds a short system notice
    /// instead. Returns the prompt to send, or `None` when a turn
    /// cannot start.
    pub fn resubmit(&mut self, prompt: String) -> Option<String> {
        if self.busy || self.pending_approval.is_some() || self.stopped {
            return None;
        }
        self.last_error = None;
        self.last_error_kind = None;
        self.busy = true;
        self.status = TurnStatus::Connecting;
        Some(prompt)
    }

    /// The prompt for the Retry button: the last prompt, verbatim.
    pub fn retry_prompt(&self) -> Option<String> {
        self.last_prompt.clone()
    }

    /// The prompt for the "Trả lời bằng tiếng Việt" button. The brain
    /// has no multi-turn context, so the original prompt is re-asked
    /// with a Vietnamese instruction — the previous answer is NOT
    /// silently translated or edited (there is no translation step).
    pub fn vietnamese_prompt(&self) -> Option<String> {
        self.last_prompt
            .as_ref()
            .map(|prompt| format!("Trả lời bằng tiếng Việt: {prompt}"))
    }

    /// A turn finished cleanly: append the assistant message.
    pub fn on_reply(&mut self, text: String, tools_used: Vec<String>) {
        self.messages.push(UiMessage {
            role: ChatRole::Assistant,
            text,
            tools_used,
        });
        self.busy = false;
        self.status = TurnStatus::Idle;
    }

    /// A turn failed: record the error, keep the user message and the
    /// last prompt (Retry stays available).
    pub fn on_error(&mut self, error: String, kind: FailureKind) {
        self.last_error = Some(error);
        self.last_error_kind = Some(kind);
        self.busy = false;
        self.status = TurnStatus::Idle;
    }

    /// A turn was cancelled (turn cancel or STOP): not a failure.
    /// The prompt stays available for retry.
    pub fn on_cancelled(&mut self) {
        self.busy = false;
        self.status = TurnStatus::Idle;
    }

    /// Add a system notice (backend status, onboarding suggestion).
    pub fn add_system(&mut self, text: String) {
        self.messages.push(UiMessage {
            role: ChatRole::System,
            text,
            tools_used: Vec::new(),
        });
    }

    /// Show an approval card for the guard's request id. While a card
    /// is on screen, `submit_input` refuses new turns: the user
    /// answers the guard question first.
    pub fn request_approval(&mut self, id: u64, tool: String, args: Value, deadline: Instant) {
        self.pending_approval = Some(ApprovalRequest {
            id,
            tool,
            args,
            deadline: Some(deadline),
        });
    }

    /// Resolve the pending approval. Returns false when the id does
    /// not match the card on screen (stale decision, ignored).
    pub fn resolve_approval(&mut self, id: u64) -> bool {
        match &self.pending_approval {
            Some(req) if req.id == id => {
                self.pending_approval = None;
                true
            }
            _ => false,
        }
    }

    /// Clear an expired card (the worker already timed it out).
    /// Returns true when a card was showing.
    pub fn expire_approval(&mut self) -> bool {
        self.pending_approval.take().is_some()
    }

    /// Enter or leave a computer-use session. Leaving clears a stop
    /// request.
    pub fn set_control_active(&mut self, active: bool) {
        self.control_active = active;
        if !active {
            self.stop_requested = false;
        }
    }

    /// Press STOP. Only meaningful while a session is active.
    pub fn request_stop(&mut self) {
        if self.control_active {
            self.stop_requested = true;
        }
    }

    /// STOP NOW: latch the stop. Blocks new turns until [`ChatState::resume`].
    pub fn request_stop_now(&mut self) {
        self.stopped = true;
        self.stop_requested = true;
    }

    /// The body's emergency stop tripped. Latches like STOP NOW,
    /// but resume does NOT clear it: the body's flag only clears
    /// on restart.
    pub fn request_body_stop(&mut self) {
        self.stopped = true;
        self.body_stopped = true;
    }

    /// The user actively re-enables after STOP NOW. A body stop
    /// is NOT cleared here: only a restart clears the body's
    /// latched flag.
    pub fn resume(&mut self) {
        self.stopped = false;
        self.stop_requested = false;
        self.control_error = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn submit_requires_text_and_idle_state() {
        let mut chat = ChatState::new();
        assert_eq!(chat.submit_input(), None);
        chat.input = "   ".to_string();
        assert_eq!(chat.submit_input(), None);
        chat.input = "hello".to_string();
        assert_eq!(chat.submit_input().as_deref(), Some("hello"));
        assert!(chat.input.is_empty());
        assert!(chat.busy);
        assert_eq!(chat.status, TurnStatus::Connecting);
        assert_eq!(chat.messages.len(), 1);
        // A second submit while busy is ignored.
        chat.input = "again".to_string();
        assert_eq!(chat.submit_input(), None);
        assert_eq!(chat.messages.len(), 1);
    }

    #[test]
    fn reply_and_error_clear_busy_and_keep_prompt() {
        let mut chat = ChatState::new();
        chat.input = "hi".to_string();
        chat.submit_input();
        chat.on_reply(
            "hello back".to_string(),
            vec!["notebook_search".to_string()],
        );
        assert!(!chat.busy);
        assert_eq!(chat.status, TurnStatus::Idle);
        assert_eq!(chat.messages.len(), 2);
        assert_eq!(chat.messages[1].tools_used, vec!["notebook_search"]);
        chat.input = "boom".to_string();
        chat.submit_input();
        chat.on_error("provider down".to_string(), FailureKind::Other);
        assert!(!chat.busy);
        assert_eq!(chat.last_error.as_deref(), Some("provider down"));
        assert_eq!(chat.retry_prompt().as_deref(), Some("boom"));
        chat.input = "ok".to_string();
        chat.submit_input();
        assert!(chat.last_error.is_none());
    }

    #[test]
    fn approval_blocks_submits_and_resolves_by_id() {
        let mut chat = ChatState::new();
        chat.request_approval(
            42,
            "notebook_add".to_string(),
            json!({"title": "x"}),
            Instant::now(),
        );
        assert_eq!(chat.control_state(), ControlState::AwaitingApproval);
        chat.input = "wait".to_string();
        assert_eq!(chat.submit_input(), None);
        assert!(!chat.resolve_approval(43));
        assert!(chat.pending_approval.is_some());
        assert!(chat.resolve_approval(42));
        assert!(chat.pending_approval.is_none());
        assert_eq!(chat.submit_input().as_deref(), Some("wait"));
    }

    #[test]
    fn stop_only_counts_while_control_is_active() {
        let mut chat = ChatState::new();
        chat.request_stop();
        assert!(!chat.stop_requested);
        chat.set_control_active(true);
        chat.request_stop();
        assert!(chat.stop_requested);
        assert_eq!(chat.control_state(), ControlState::Stopping);
        chat.set_control_active(false);
        assert!(!chat.stop_requested);
        assert!(!chat.control_active);
    }

    #[test]
    fn stop_now_latches_and_blocks_turns_until_resume() {
        let mut chat = ChatState::new();
        chat.request_stop_now();
        assert_eq!(chat.control_state(), ControlState::Stopped);
        chat.input = "hi".to_string();
        assert_eq!(chat.submit_input(), None);
        assert_eq!(chat.resubmit("hi".to_string()), None);
        chat.resume();
        assert_eq!(chat.control_state(), ControlState::Idle);
        assert_eq!(chat.submit_input().as_deref(), Some("hi"));
    }

    #[test]
    fn body_stop_latches_through_resume() {
        let mut chat = ChatState::new();
        chat.request_body_stop();
        assert_eq!(chat.control_state(), ControlState::Stopped);
        chat.input = "hi".to_string();
        assert_eq!(chat.submit_input(), None);
        // Resume clears the UI latch but NOT the body latch: the
        // lamp stays Stopped and turns stay blocked until restart.
        chat.resume();
        assert_eq!(chat.control_state(), ControlState::Stopped);
        assert_eq!(chat.submit_input(), None);
    }

    #[test]
    fn vietnamese_prompt_reasks_with_instruction() {
        let mut chat = ChatState::new();
        assert_eq!(chat.vietnamese_prompt(), None);
        chat.input = "Explain gravity".to_string();
        chat.submit_input();
        assert_eq!(
            chat.vietnamese_prompt().as_deref(),
            Some("Trả lời bằng tiếng Việt: Explain gravity")
        );
        // Cancelling keeps the prompt for retry.
        chat.on_cancelled();
        assert_eq!(chat.retry_prompt().as_deref(), Some("Explain gravity"));
    }

    #[test]
    fn control_error_wins_the_lamp() {
        let mut chat = ChatState::new();
        chat.control_error = true;
        assert_eq!(chat.control_state(), ControlState::Error);
        chat.resume();
        assert_eq!(chat.control_state(), ControlState::Idle);
    }
}
