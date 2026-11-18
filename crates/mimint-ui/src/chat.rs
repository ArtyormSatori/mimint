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

use crate::approver::ApprovalJob;
use crate::worker::FailureKind;
use mimint_body::WindowIdentity;
use mimint_core::CostDisplay;
use serde_json::Value;
use std::time::{Duration, Instant};

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
/// the decision, and the audit record. The pair (`agent_id`, `id`)
/// is the binding key (§3.1): request ids restart at 1 per turn,
/// so the agent id is part of the identity.
/// `target` is the window under the action point at request time
/// (`None` when the probe was unavailable or the tool has no
/// point). `observation_age` is the age of the newest stored
/// frame at request time; the card warns when it exceeds
/// `max_observation_age`. `timeout` is the full wait the worker
/// allows, for the expiry message (never a hardcoded number).
/// The Phase 6 fields carry the card context of §15.2: queue
/// position, wait time, the agent + task label, the tool's audit
/// fields (MCP server/tool/version), and the cost estimate.
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalRequest {
    pub id: u64,
    pub agent_id: String,
    pub tool: String,
    pub args: Value,
    /// When the worker times this request out (the card shows a
    /// countdown; an expired card is cleared, never answered late).
    pub deadline: Option<Instant>,
    pub timeout: Duration,
    pub target: Option<WindowIdentity>,
    pub observation_age: Option<Duration>,
    pub max_observation_age: Duration,
    /// Position in the card queue at insertion time (1-based).
    pub queue_position: usize,
    /// Cards waiting at insertion time.
    pub queue_total: usize,
    /// When the worker created the card (elapsed wait on display).
    pub created_at: Instant,
    /// The tool's audit fields (MCP server/tool/version when the
    /// tool is an MCP tool; empty otherwise).
    pub context: Value,
    /// The asking agent's cost display at request time, when the
    /// ledger is wired.
    pub cost: Option<CostDisplay>,
    /// True when any of the estimate is unmeasured (§3.3).
    pub unmeasured: bool,
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
    /// The brain is thinking (provider call in flight).
    Analyzing,
    /// A body tool is executing (tool name, e.g. `body_click`).
    Acting(String),
    /// The step verifier is checking the result on screen.
    Verifying,
    /// A tool failed: the tool name and the (already friendly)
    /// failure text. The turn continues or ends on the brain's
    /// own logic; this status only reports what happened.
    ToolFailed { tool: String, error: String },
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

/// A short Vietnamese reading of a tool failure, for the status
/// line. Matches the machine-readable error codes the body tools
/// prefix their errors with (`code: message`); anything
/// unrecognized is shown truncated, never hidden. Display only:
/// the raw error is always kept in the audit trail.
pub fn friendly_tool_error(error: &str) -> String {
    let lower = error.to_lowercase();
    if lower.contains("target_changed") {
        "mục tiêu dưới con trỏ đã đổi so với lúc phê duyệt".to_string()
    } else if lower.contains("stale_observation") {
        "ảnh chụp Mimint dựa vào đã quá cũ".to_string()
    } else if lower.contains("unknown_outcome") {
        "không rõ hành động có chạy hay không (không tự thử lại)".to_string()
    } else if lower.contains("body_queue_full") {
        "hàng đợi action đã đầy, action bị từ chối ngay".to_string()
    } else if lower.contains("queue_sla_expired") {
        "action chờ quá lâu trong hàng đợi nên hết hạn, không chạy".to_string()
    } else if lower.contains("budget_exhausted") {
        "ngân sách token đã hết".to_string()
    } else if lower.contains("sandbox_unavailable") {
        "sandbox chưa sẵn sàng, không chạy code ngoài sandbox".to_string()
    } else if lower.contains("sandbox_unverified") {
        "sandbox chưa được kiểm chứng trên hệ điều hành này".to_string()
    } else if lower.contains("task_end_unverified") {
        "không xác minh được agent đã dừng hẳn".to_string()
    } else if lower.contains("forbidden") {
        "mục tiêu bị cấm".to_string()
    } else if lower.contains("rate_limited") {
        "vượt giới hạn thao tác".to_string()
    } else if lower.contains("stopped") || lower.contains("emergency stop") {
        "phiên điều khiển đã dừng".to_string()
    } else if lower.contains("out_of_bounds") {
        "tọa độ nằm ngoài màn hình".to_string()
    } else if lower.contains("permission_denied") {
        "thiếu quyền truy cập".to_string()
    } else {
        const MAX: usize = 80;
        if error.chars().count() > MAX {
            let kept: String = error.chars().take(MAX).collect();
            format!("{kept}…")
        } else {
            error.to_string()
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
    /// When the running turn started, for the elapsed-time status
    /// line. Set on submit/resubmit, cleared when the turn ends.
    pub turn_started: Option<Instant>,
    /// Multi-agent mode (§15): when true, submitting while a
    /// turn runs starts (or queues) another agent instead of
    /// being refused. Set by the app from the config flag.
    pub multi_agent: bool,
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
    /// non-empty input, no turn running (or multi-agent mode, where
    /// a new turn starts another agent), no approval pending, not
    /// stopped. The input is cleared and a user message is appended.
    pub fn submit_input(&mut self) -> Option<String> {
        if (self.busy && !self.multi_agent)
            || self.pending_approval.is_some()
            || self.stopped
            || self.body_stopped
        {
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
        self.turn_started = Some(Instant::now());
        Some(prompt)
    }

    /// Re-run the last prompt (Retry / "Reply in Vietnamese"). The
    /// prompt is NOT appended again as a user message — it is
    /// already in the history; the app adds a short system notice
    /// instead. Returns the prompt to send, or `None` when a turn
    /// cannot start.
    pub fn resubmit(&mut self, prompt: String) -> Option<String> {
        if (self.busy && !self.multi_agent) || self.pending_approval.is_some() || self.stopped {
            return None;
        }
        self.last_error = None;
        self.last_error_kind = None;
        self.busy = true;
        self.status = TurnStatus::Connecting;
        self.turn_started = Some(Instant::now());
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
        self.turn_started = None;
    }

    /// A turn failed: record the error, keep the user message and the
    /// last prompt (Retry stays available).
    pub fn on_error(&mut self, error: String, kind: FailureKind) {
        self.last_error = Some(error);
        self.last_error_kind = Some(kind);
        self.busy = false;
        self.status = TurnStatus::Idle;
        self.turn_started = None;
    }

    /// A turn was cancelled (turn cancel or STOP): not a failure.
    /// The prompt stays available for retry.
    pub fn on_cancelled(&mut self) {
        self.busy = false;
        self.status = TurnStatus::Idle;
        self.turn_started = None;
    }

    /// Elapsed seconds of the running turn, for the status line.
    /// `None` when no turn is running.
    pub fn turn_elapsed_secs(&self) -> Option<u64> {
        self.turn_started.map(|started| started.elapsed().as_secs())
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
    /// answers the guard question first. The Phase 6 card fields
    /// default to the single-agent reading (agent-001, alone in
    /// the queue); [`ChatState::request_approval_job`] carries
    /// the full context from a worker job.
    #[allow(clippy::too_many_arguments)]
    pub fn request_approval(
        &mut self,
        id: u64,
        tool: String,
        args: Value,
        deadline: Instant,
        timeout: Duration,
        target: Option<WindowIdentity>,
        observation_age: Option<Duration>,
        max_observation_age: Duration,
    ) {
        self.pending_approval = Some(ApprovalRequest {
            id,
            agent_id: mimint_core::AGENT_DEFAULT.to_string(),
            tool,
            args,
            deadline: Some(deadline),
            timeout,
            target,
            observation_age,
            max_observation_age,
            queue_position: 1,
            queue_total: 1,
            created_at: Instant::now(),
            context: Value::Null,
            cost: None,
            unmeasured: false,
        });
    }

    /// Show an approval card from a worker job, carrying the full
    /// §15.2 context: queue position, wait time, agent, tool
    /// audit fields, and cost estimate.
    pub fn request_approval_job(&mut self, job: &ApprovalJob) {
        self.pending_approval = Some(ApprovalRequest {
            id: job.id,
            agent_id: job.agent_id.clone(),
            tool: job.tool.clone(),
            args: job.args.clone(),
            deadline: Some(job.deadline),
            timeout: job.timeout,
            target: job.target.clone(),
            observation_age: job.observation_age,
            max_observation_age: job.max_observation_age,
            queue_position: job.queue_position,
            queue_total: job.queue_total,
            created_at: job.created_at,
            context: job.context.clone(),
            cost: job.cost.clone(),
            unmeasured: job.unmeasured,
        });
    }

    /// Resolve the pending approval. The key is the (`agent_id`,
    /// `id`) pair (§3.1): request ids restart at 1 per turn, so
    /// the id alone cannot bind a decision. Returns false when
    /// the pair does not match the card on screen (stale
    /// decision, ignored).
    pub fn resolve_approval(&mut self, agent_id: &str, id: u64) -> bool {
        match &self.pending_approval {
            Some(req) if req.id == id && req.agent_id == agent_id => {
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
            Duration::from_secs(60),
            None,
            None,
            Duration::from_secs(300),
        );
        assert_eq!(chat.control_state(), ControlState::AwaitingApproval);
        chat.input = "wait".to_string();
        assert_eq!(chat.submit_input(), None);
        assert!(!chat.resolve_approval("agent-001", 43));
        assert!(chat.pending_approval.is_some());
        // The binding key is (agent_id, id): the same id from
        // another agent does not resolve this card.
        assert!(!chat.resolve_approval("agent-002", 42));
        assert!(chat.pending_approval.is_some());
        assert!(chat.resolve_approval("agent-001", 42));
        assert!(chat.pending_approval.is_none());
        assert_eq!(chat.submit_input().as_deref(), Some("wait"));
    }

    #[test]
    fn multi_agent_submit_is_allowed_while_busy() {
        let mut chat = ChatState::new();
        chat.multi_agent = true;
        chat.input = "one".to_string();
        assert_eq!(chat.submit_input().as_deref(), Some("one"));
        assert!(chat.busy);
        chat.input = "two".to_string();
        assert_eq!(chat.submit_input().as_deref(), Some("two"));
        assert_eq!(chat.messages.len(), 2);
    }

    #[test]
    fn friendly_tool_error_maps_phase_6_codes() {
        assert!(friendly_tool_error("body_queue_full: 64 queued").contains("đã đầy"));
        assert!(friendly_tool_error("queue_sla_expired: waited 60s").contains("hết hạn"));
        assert!(friendly_tool_error("budget_exhausted: no tokens left").contains("ngân sách"));
        assert!(friendly_tool_error("sandbox_unavailable: no bwrap").contains("sandbox"));
        assert!(friendly_tool_error("task_end_unverified: join timed out")
            .contains("không xác minh được"));
    }

    #[test]
    fn turn_started_tracks_the_running_turn_only() {
        let mut chat = ChatState::new();
        assert_eq!(chat.turn_elapsed_secs(), None);
        chat.input = "hi".to_string();
        chat.submit_input();
        assert!(chat.turn_started.is_some());
        assert_eq!(chat.turn_elapsed_secs(), Some(0));
        chat.on_reply("done".to_string(), Vec::new());
        assert_eq!(chat.turn_elapsed_secs(), None);
        chat.input = "again".to_string();
        chat.submit_input();
        assert!(chat.turn_started.is_some());
        chat.on_cancelled();
        assert_eq!(chat.turn_elapsed_secs(), None);
    }

    #[test]
    fn friendly_tool_error_maps_known_codes() {
        assert!(friendly_tool_error("target_changed: window differs").contains("đã đổi"));
        assert!(friendly_tool_error("stale_observation: age 400s").contains("quá cũ"));
        assert!(friendly_tool_error("unknown_outcome: maybe ran").contains("không rõ"));
        assert!(friendly_tool_error("forbidden_target: x").contains("bị cấm"));
        let long = "x".repeat(200);
        let shown = friendly_tool_error(&long);
        assert!(shown.ends_with('…'));
        assert_eq!(shown.chars().count(), 81);
        assert_eq!(friendly_tool_error("short"), "short");
    }

    #[test]
    fn approval_carries_target_and_observation_context() {
        let mut chat = ChatState::new();
        let target = WindowIdentity {
            window_id: Some(7),
            class: Some("editor".to_string()),
            title: None,
            exe_basename: Some("code".to_string()),
        };
        chat.request_approval(
            1,
            "body_click".to_string(),
            json!({"x": 1, "y": 2}),
            Instant::now(),
            Duration::from_secs(60),
            Some(target),
            Some(Duration::from_secs(42)),
            Duration::from_secs(300),
        );
        let req = chat.pending_approval.as_ref().unwrap();
        assert_eq!(req.timeout, Duration::from_secs(60));
        assert_eq!(req.target.as_ref().unwrap().window_id, Some(7));
        assert_eq!(req.observation_age, Some(Duration::from_secs(42)));
        assert_eq!(req.max_observation_age, Duration::from_secs(300));
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

    /// §15.6 `ui_approval_card_contents` (F6D-17): the card's
    /// data layer carries every §15.2 field — the requesting
    /// agent, the queue position, the wait clock, the MCP
    /// context, the cost estimate + unmeasured flag — and the
    /// detail helpers render the tool-specific content (MCP
    /// server/tool/unsandboxed mode, execute_code hash, delegate
    /// limits, impact labels). The egui render itself is
    /// untestable headless; it reads exactly these fields.
    #[test]
    fn ui_approval_card_contents() {
        use crate::approval_text::{
            approval_detail_with_context, execute_code_hash, impact_labels, short_hash,
        };
        use crate::approver::ApprovalJob;
        use mimint_core::CostDisplay;
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        use std::time::Instant;

        let (reply, _rx) = std::sync::mpsc::channel();
        let created_at = Instant::now();
        let job = ApprovalJob {
            id: 7,
            agent_id: "agent-002".to_string(),
            tool: "mcp_notes_search".to_string(),
            args: json!({"query": "lunch"}),
            deadline: created_at + Duration::from_secs(60),
            timeout: Duration::from_secs(60),
            target: None,
            observation_age: None,
            max_observation_age: Duration::from_secs(300),
            reply,
            withdrawn: Arc::new(AtomicBool::new(false)),
            created_at,
            queue_position: 2,
            queue_total: 2,
            context: json!({
                "mcp_server": "notes",
                "mcp_tool": "search",
                "protocol_version": "2025-06-18",
                "unsandboxed": true,
            }),
            cost: Some(CostDisplay::EstimateTokens(4096)),
            unmeasured: true,
        };
        let mut chat = ChatState::new();
        chat.request_approval_job(&job);
        let req = chat.pending_approval.as_ref().expect("card on screen");
        assert_eq!(req.agent_id, "agent-002");
        assert_eq!(req.queue_position, 2);
        assert_eq!(req.queue_total, 2);
        assert_eq!(req.created_at, created_at);
        assert_eq!(req.context["mcp_server"], "notes");
        assert_eq!(req.cost, Some(CostDisplay::EstimateTokens(4096)));
        assert!(req.unmeasured);

        // The MCP detail names the server, the tool, the
        // protocol version, and the unsandboxed mode.
        let detail = approval_detail_with_context(&req.tool, &req.args, &req.context);
        let rows: Vec<String> = detail.rows.iter().map(|(_, v)| v.clone()).collect();
        assert!(rows.iter().any(|v| v == "notes"), "{rows:?}");
        assert!(rows.iter().any(|v| v == "search"), "{rows:?}");
        assert!(rows.iter().any(|v| v == "2025-06-18"), "{rows:?}");
        assert!(rows.iter().any(|v| v.contains("KHÔNG SANDBOX")), "{rows:?}");
        // MCP tools always carry at least "thực thi".
        let (primary, _) = impact_labels(&req.tool, &req.context);
        assert_eq!(primary, "thực thi");

        // Execute_code: the short hash in the rows, the full
        // hash one expand away (the render's CollapsingHeader
        // reads the same helper).
        let detail = approval_detail_with_context(
            "execute_code",
            &json!({"language": "python3", "code": "print('hello')"}),
            &serde_json::Value::Null,
        );
        let full = execute_code_hash("print('hello')");
        assert_eq!(full.len(), 64);
        assert!(
            detail.rows.iter().any(|(_, v)| *v == short_hash(&full)),
            "{:?}",
            detail.rows
        );
        assert!(detail.rows.iter().any(|(_, v)| v == "python3"));
        assert!(detail.rows.iter().any(|(_, v)| v == "14 ký tự"));
    }
}
