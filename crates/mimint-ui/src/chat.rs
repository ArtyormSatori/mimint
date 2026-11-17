//! Chat state machine: messages, input, pending approval, and the
//! computer-use control lamp. Pure Rust, no egui dependency.
//!
//! The brain starts a fresh message list on every turn (Phase 1/2
//! behavior); this module only DISPLAYS the conversation history.
//! Multi-turn context is Phase 5 work.
//!
//! The control lamp lights only while a computer-use session is
//! active. No body-driving code exists in Phase 3, so the lamp never
//! lights and the STOP button stays disabled — the state machine is
//! implemented and tested now so Phase 4 only has to drive it.

use serde_json::Value;

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

/// A guard approval waiting on the user.
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalRequest {
    pub id: u64,
    pub tool: String,
    pub args: Value,
}

/// Chat + control state for one window.
#[derive(Debug, Default)]
pub struct ChatState {
    pub messages: Vec<UiMessage>,
    pub input: String,
    /// A turn is running in the worker.
    pub busy: bool,
    /// The approval currently on screen, if any.
    pub pending_approval: Option<ApprovalRequest>,
    /// Computer-use session active (lamp lit). Never true in Phase 3.
    pub control_active: bool,
    /// The user pressed STOP during an active session.
    pub stop_requested: bool,
    /// Last turn error, shown until the next submit.
    pub last_error: Option<String>,
    next_approval_id: u64,
}

impl ChatState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Submit the input box. Returns the prompt when a turn may start:
    /// non-empty input, no turn running, no approval pending. The
    /// input is cleared and a user message is appended.
    pub fn submit_input(&mut self) -> Option<String> {
        if self.busy || self.pending_approval.is_some() {
            return None;
        }
        let prompt = self.input.trim().to_string();
        if prompt.is_empty() {
            return None;
        }
        self.input.clear();
        self.last_error = None;
        self.messages.push(UiMessage {
            role: ChatRole::User,
            text: prompt.clone(),
            tools_used: Vec::new(),
        });
        self.busy = true;
        Some(prompt)
    }

    /// A turn finished cleanly: append the assistant message.
    pub fn on_reply(&mut self, text: String, tools_used: Vec<String>) {
        self.messages.push(UiMessage {
            role: ChatRole::Assistant,
            text,
            tools_used,
        });
        self.busy = false;
    }

    /// A turn failed: record the error, keep the user message.
    pub fn on_error(&mut self, error: String) {
        self.last_error = Some(error);
        self.busy = false;
    }

    /// Add a system notice (backend status, onboarding suggestion).
    pub fn add_system(&mut self, text: String) {
        self.messages.push(UiMessage {
            role: ChatRole::System,
            text,
            tools_used: Vec::new(),
        });
    }

    /// Show an approval card. Returns the request id. While a card is
    /// on screen, `submit_input` refuses new turns: the user answers
    /// the guard question first.
    pub fn request_approval(&mut self, tool: String, args: Value) -> u64 {
        self.next_approval_id += 1;
        let id = self.next_approval_id;
        self.pending_approval = Some(ApprovalRequest { id, tool, args });
        id
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
        assert_eq!(chat.messages.len(), 1);
        // A second submit while busy is ignored.
        chat.input = "again".to_string();
        assert_eq!(chat.submit_input(), None);
        assert_eq!(chat.messages.len(), 1);
    }

    #[test]
    fn reply_and_error_clear_busy() {
        let mut chat = ChatState::new();
        chat.input = "hi".to_string();
        chat.submit_input();
        chat.on_reply(
            "hello back".to_string(),
            vec!["notebook_search".to_string()],
        );
        assert!(!chat.busy);
        assert_eq!(chat.messages.len(), 2);
        assert_eq!(chat.messages[1].tools_used, vec!["notebook_search"]);
        chat.input = "boom".to_string();
        chat.submit_input();
        chat.on_error("provider down".to_string());
        assert!(!chat.busy);
        assert_eq!(chat.last_error.as_deref(), Some("provider down"));
        chat.input = "ok".to_string();
        chat.submit_input();
        assert!(chat.last_error.is_none());
    }

    #[test]
    fn approval_blocks_submits_and_resolves_by_id() {
        let mut chat = ChatState::new();
        let id = chat.request_approval("notebook_add".to_string(), json!({"title": "x"}));
        chat.input = "wait".to_string();
        assert_eq!(chat.submit_input(), None);
        assert!(!chat.resolve_approval(id + 1));
        assert!(chat.pending_approval.is_some());
        assert!(chat.resolve_approval(id));
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
        chat.set_control_active(false);
        assert!(!chat.stop_requested);
        assert!(!chat.control_active);
    }
}
