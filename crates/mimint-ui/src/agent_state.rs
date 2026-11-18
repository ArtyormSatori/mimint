//! Display-state model for agents (§15.1) and stop transitions
//! (§15.3): pure functions from internal facts to the one
//! display state an agent shows at a time. No egui here, so the
//! model is testable without a UI harness.
//!
//! The nine display states of §15.1, plus `Running` (a turn in
//! progress: the row shows the turn phase) and `Completed` (a
//! turn that ended fully verified). "Đã hủy" is shown only for
//! a verified end; every unverified case is "Kết quả chưa xác
//! định", never "Đã hủy" — the precedence below enforces that
//! by construction.

/// The internal facts one agent row is derived from. Every
/// field names a real internal state (§1.1 lifecycle, §7.2
/// verification, §4 fault scope, §3.3 budget, §10 degraded).
#[derive(Debug, Clone, Default)]
pub struct AgentView {
    /// A turn is currently running for this agent.
    pub turn_running: bool,
    /// The running turn is blocked on an approval card.
    pub pending_approval: bool,
    /// A cancel was requested and its end is not yet verified.
    pub cancel_requested: bool,
    /// The last end was verified (joined within the bound).
    pub end_verified: bool,
    /// The last end was a cancellation.
    pub cancelled: bool,
    /// An `UnknownOutcome` was recorded for this agent (a body
    /// fault, or a cancel whose end could not be verified).
    pub unknown_outcome: bool,
    /// A turn ended (any terminal outcome).
    pub turn_ended: bool,
    /// The last turn ended with every step verified (R4).
    pub all_verified: bool,
    /// The coordinator is degraded (§10): no new spawns.
    pub degraded: bool,
    /// The parent continued after a child failed (§8.1): the
    /// result is missing the child's part.
    pub partial: bool,
    /// The agent stopped on its own token budget.
    pub budget_exhausted: bool,
}

/// One display state (§15.1). Exactly one applies at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentDisplayState {
    /// 1 — Spawned, no turn running yet.
    Waiting,
    /// A turn is running; the row shows the turn phase.
    Running,
    /// 2 — Blocked on an approval card.
    AwaitingApproval,
    /// 3 — Cancel requested, end not yet verified.
    Cancelling,
    /// 4 — Cancelled, end verified.
    Cancelled,
    /// 5 — `UnknownOutcome`: the result is unknown.
    UnknownOutcome,
    /// 6 — The turn ended but not every step verified.
    EndedUnverified,
    /// 7 — The coordinator is degraded.
    Degraded,
    /// 8 — The parent continued after a child failure.
    PartiallyDone,
    /// 9 — Stopped on the agent's token budget.
    AwaitingBudget,
    /// The last turn ended fully verified.
    Completed,
}

impl AgentDisplayState {
    /// The exact display label (§15.1).
    pub fn label(self) -> &'static str {
        match self {
            AgentDisplayState::Waiting => "Đang chờ",
            AgentDisplayState::Running => "Đang chạy",
            AgentDisplayState::AwaitingApproval => "Đang chờ phê duyệt",
            AgentDisplayState::Cancelling => "Đang hủy",
            AgentDisplayState::Cancelled => "Đã hủy",
            AgentDisplayState::UnknownOutcome => "Kết quả chưa xác định",
            AgentDisplayState::EndedUnverified => "Kết thúc chưa xác minh",
            AgentDisplayState::Degraded => "Degraded",
            AgentDisplayState::PartiallyDone => "Hoàn thành một phần",
            AgentDisplayState::AwaitingBudget => "Chờ ngân sách",
            AgentDisplayState::Completed => "Hoàn thành",
        }
    }
}

/// The single display state for one agent. Precedence is the
/// design's rule set, in order:
///
/// 1. Anything unverified is "Kết quả chưa xác định" — never
///    "Đã hủy", even when a cancel was requested.
/// 2. A requested-but-unverified cancel is "Đang hủy".
/// 3. A verified cancel is "Đã hủy".
/// 4. A running turn blocked on a card is "Đang chờ phê duyệt".
/// 5. Any other running turn is "Đang chạy" (phase shown).
/// 6. A degraded coordinator shows "Degraded" on idle agents.
/// 7. An ended turn shows its budget/partial/verification
///    outcome, in that order.
/// 8. Otherwise the agent is "Đang chờ" — or "Hoàn thành" when
///    its last turn ended fully verified.
pub fn display_state(view: &AgentView) -> AgentDisplayState {
    if view.unknown_outcome {
        return AgentDisplayState::UnknownOutcome;
    }
    if view.cancel_requested && !view.end_verified {
        return AgentDisplayState::Cancelling;
    }
    if view.cancelled && view.end_verified {
        return AgentDisplayState::Cancelled;
    }
    if view.turn_running && view.pending_approval {
        return AgentDisplayState::AwaitingApproval;
    }
    if view.turn_running {
        return AgentDisplayState::Running;
    }
    if view.degraded {
        return AgentDisplayState::Degraded;
    }
    if view.budget_exhausted {
        return AgentDisplayState::AwaitingBudget;
    }
    if view.partial {
        return AgentDisplayState::PartiallyDone;
    }
    if view.turn_ended && !view.all_verified {
        return AgentDisplayState::EndedUnverified;
    }
    if view.turn_ended && view.all_verified {
        return AgentDisplayState::Completed;
    }
    AgentDisplayState::Waiting
}

/// The global body state (§15.1): separate from every agent
/// row. When the body is not ready, no agent may send a new
/// action, and waiting agents show the reason instead of
/// waiting silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyDisplayState {
    Ready,
    NotReady(BodyNotReadyReason),
}

/// Why the body is not ready (§15.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyNotReadyReason {
    /// The actor broke (a body fault); a rebuild is required.
    Broken,
    /// A rebuild is in progress.
    Rebuilding,
    /// The body stack has not finished starting.
    Starting,
}

impl BodyDisplayState {
    pub fn label(self) -> &'static str {
        match self {
            BodyDisplayState::Ready => "Body sẵn sàng",
            BodyDisplayState::NotReady(_) => "Body chưa sẵn sàng",
        }
    }

    pub fn reason_label(self) -> Option<&'static str> {
        match self {
            BodyDisplayState::Ready => None,
            BodyDisplayState::NotReady(BodyNotReadyReason::Broken) => {
                Some("actor đã hỏng — cần rebuild body")
            }
            BodyDisplayState::NotReady(BodyNotReadyReason::Rebuilding) => Some("đang rebuild body"),
            BodyDisplayState::NotReady(BodyNotReadyReason::Starting) => {
                Some("body chưa khởi động xong")
            }
        }
    }
}

/// The three agent kinds after a fail-all (§4.1c, §15.1): the
/// display must tell them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailAllRole {
    /// The agent whose body fault caused the fail-all.
    Causer,
    /// An agent stopped by system propagation.
    StoppedBySystem,
    /// An agent whose result is unknown.
    UnknownOutcome,
}

impl FailAllRole {
    pub fn label(self) -> &'static str {
        match self {
            FailAllRole::Causer => "agent gây lỗi",
            FailAllRole::StoppedBySystem => "bị dừng theo hệ thống",
            FailAllRole::UnknownOutcome => "kết quả chưa xác định",
        }
    }
}

/// Stop-transition phases (§15.3): request sent → awaiting end
/// → confirmed / unverifiable. "Đã hủy" appears only after a
/// verified end; calling `abort()` alone never confirms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopPhase {
    Idle,
    Requested,
    AwaitingEnd,
    Confirmed,
    Unverified,
}

impl StopPhase {
    pub fn label(self) -> &'static str {
        match self {
            StopPhase::Idle => "",
            StopPhase::Requested => "Đã gửi yêu cầu dừng",
            StopPhase::AwaitingEnd => "Đang chờ kết thúc",
            StopPhase::Confirmed => "Đã xác nhận dừng",
            StopPhase::Unverified => "Không xác minh được",
        }
    }
}

/// Events that move a stop transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopEvent {
    /// The user asked to stop (button pressed).
    Requested,
    /// The task was aborted; the join is still pending.
    AbortCalled,
    /// The task joined within the bound: the end is verified.
    EndVerified,
    /// The join timed out: the end is unverifiable.
    JoinTimeout,
}

/// The next stop phase. `AbortCalled` never yields `Confirmed`:
/// only a verified join does (§15.3). `Confirmed` and
/// `Unverified` are terminal.
pub fn next_stop_phase(current: StopPhase, event: StopEvent) -> StopPhase {
    // Fully qualified: both enums have a `Requested` variant.
    match (current, event) {
        (StopPhase::Idle, StopEvent::Requested) => StopPhase::Requested,
        (StopPhase::Requested, StopEvent::AbortCalled) => StopPhase::AwaitingEnd,
        (StopPhase::Requested, StopEvent::EndVerified) => StopPhase::Confirmed,
        (StopPhase::Requested, StopEvent::JoinTimeout) => StopPhase::Unverified,
        (StopPhase::AwaitingEnd, StopEvent::EndVerified) => StopPhase::Confirmed,
        (StopPhase::AwaitingEnd, StopEvent::JoinTimeout) => StopPhase::Unverified,
        // Abort alone never confirms, at any phase.
        (StopPhase::AwaitingEnd, StopEvent::AbortCalled) => StopPhase::AwaitingEnd,
        // Terminal phases stay; anything else is a no-op.
        (current, _) => current,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §15.6 `ui_agent_state_model`: every internal state maps
    /// to exactly one display state; all nine §15.1 states plus
    /// "Body chưa sẵn sàng" are reachable; Cancelled and
    /// UnknownOutcome never display the same.
    #[test]
    fn ui_agent_state_model() {
        // 1 — spawned, nothing running.
        let view = AgentView::default();
        assert_eq!(display_state(&view), AgentDisplayState::Waiting);
        assert_eq!(display_state(&view).label(), "Đang chờ");
        // A running turn shows Running (the phase is separate).
        let view = AgentView {
            turn_running: true,
            ..Default::default()
        };
        assert_eq!(display_state(&view), AgentDisplayState::Running);
        // 2 — blocked on a card.
        let view = AgentView {
            turn_running: true,
            pending_approval: true,
            ..Default::default()
        };
        assert_eq!(display_state(&view), AgentDisplayState::AwaitingApproval);
        assert_eq!(display_state(&view).label(), "Đang chờ phê duyệt");
        // 3 — cancel requested, end unverified.
        let view = AgentView {
            cancel_requested: true,
            ..Default::default()
        };
        assert_eq!(display_state(&view), AgentDisplayState::Cancelling);
        assert_eq!(display_state(&view).label(), "Đang hủy");
        // 4 — verified cancel.
        let view = AgentView {
            cancel_requested: true,
            end_verified: true,
            cancelled: true,
            ..Default::default()
        };
        assert_eq!(display_state(&view), AgentDisplayState::Cancelled);
        assert_eq!(display_state(&view).label(), "Đã hủy");
        // 5 — unverified, even with a cancel requested: never
        // "Đã hủy".
        let view = AgentView {
            cancel_requested: true,
            unknown_outcome: true,
            ..Default::default()
        };
        assert_eq!(display_state(&view), AgentDisplayState::UnknownOutcome);
        assert_eq!(display_state(&view).label(), "Kết quả chưa xác định");
        assert_ne!(
            AgentDisplayState::Cancelled.label(),
            AgentDisplayState::UnknownOutcome.label()
        );
        // 6 — ended, not all verified.
        let view = AgentView {
            turn_ended: true,
            all_verified: false,
            ..Default::default()
        };
        assert_eq!(display_state(&view), AgentDisplayState::EndedUnverified);
        assert_eq!(display_state(&view).label(), "Kết thúc chưa xác minh");
        // A fully verified end is "Hoàn thành", never state 6.
        let view = AgentView {
            turn_ended: true,
            all_verified: true,
            ..Default::default()
        };
        assert_eq!(display_state(&view), AgentDisplayState::Completed);
        // 7 — degraded, idle.
        let view = AgentView {
            degraded: true,
            ..Default::default()
        };
        assert_eq!(display_state(&view), AgentDisplayState::Degraded);
        assert_eq!(display_state(&view).label(), "Degraded");
        // A running turn keeps showing its phase during
        // degraded: degraded blocks new spawns, not live turns.
        let view = AgentView {
            turn_running: true,
            degraded: true,
            ..Default::default()
        };
        assert_eq!(display_state(&view), AgentDisplayState::Running);
        // 8 — partial: the parent continued after a child failed.
        let view = AgentView {
            turn_ended: true,
            all_verified: true,
            partial: true,
            ..Default::default()
        };
        assert_eq!(display_state(&view), AgentDisplayState::PartiallyDone);
        assert_eq!(display_state(&view).label(), "Hoàn thành một phần");
        // 9 — stopped on the agent's budget.
        let view = AgentView {
            turn_ended: true,
            budget_exhausted: true,
            ..Default::default()
        };
        assert_eq!(display_state(&view), AgentDisplayState::AwaitingBudget);
        assert_eq!(display_state(&view).label(), "Chờ ngân sách");
        // Budget beats partial: the budget state names the
        // action the user must take.
        let view = AgentView {
            turn_ended: true,
            budget_exhausted: true,
            partial: true,
            ..Default::default()
        };
        assert_eq!(display_state(&view), AgentDisplayState::AwaitingBudget);
        // Body not ready: reachable, distinct, separate from
        // every agent state.
        let body = BodyDisplayState::NotReady(BodyNotReadyReason::Broken);
        assert_eq!(body.label(), "Body chưa sẵn sàng");
        assert!(body.reason_label().is_some());
        assert_eq!(BodyDisplayState::Ready.label(), "Body sẵn sàng");
        // The three fail-all roles display distinctly.
        assert_ne!(
            FailAllRole::Causer.label(),
            FailAllRole::StoppedBySystem.label()
        );
        assert_ne!(
            FailAllRole::StoppedBySystem.label(),
            FailAllRole::UnknownOutcome.label()
        );
    }

    /// §15.6 `ui_stop_transitions` (state machine half): the
    /// phases run in order, and `abort()` alone never shows
    /// "Đã dừng" — only a verified join confirms.
    #[test]
    fn ui_stop_transitions() {
        // Fully qualified: both enums have a `Requested` variant.
        use StopEvent::{AbortCalled, EndVerified, JoinTimeout};
        use StopPhase::{AwaitingEnd, Confirmed, Idle, Unverified};
        assert_eq!(
            next_stop_phase(Idle, StopEvent::Requested),
            StopPhase::Requested
        );
        // Abort called, join still pending: still awaiting.
        assert_eq!(
            next_stop_phase(StopPhase::Requested, AbortCalled),
            AwaitingEnd
        );
        assert_eq!(next_stop_phase(AwaitingEnd, AbortCalled), AwaitingEnd);
        assert_ne!(next_stop_phase(AwaitingEnd, AbortCalled), Confirmed);
        // A verified join confirms.
        assert_eq!(next_stop_phase(AwaitingEnd, EndVerified), Confirmed);
        assert_eq!(
            next_stop_phase(StopPhase::Requested, EndVerified),
            Confirmed
        );
        // A join timeout leaves the end unverified.
        assert_eq!(next_stop_phase(AwaitingEnd, JoinTimeout), Unverified);
        assert_eq!(
            next_stop_phase(StopPhase::Requested, JoinTimeout),
            Unverified
        );
        // Terminal phases stay terminal.
        assert_eq!(next_stop_phase(Confirmed, AbortCalled), Confirmed);
        assert_eq!(next_stop_phase(Unverified, EndVerified), Unverified);
        // Labels: no phase claims "Đã dừng" before verification.
        assert_eq!(AwaitingEnd.label(), "Đang chờ kết thúc");
        assert_eq!(Confirmed.label(), "Đã xác nhận dừng");
        assert_eq!(Unverified.label(), "Không xác minh được");
    }
}
