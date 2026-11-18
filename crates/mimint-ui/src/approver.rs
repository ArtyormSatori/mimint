//! The desktop approval card as a guard [`Approver`].
//!
//! The worker thread calls [`UiApprover::approve_request`] from
//! inside the guard executor. The call blocks until the user answers
//! on the approval card, the timeout passes, or a cancellation
//! token trips — whichever comes first. The three non-allow
//! outcomes are distinct ([`ApprovalDecision::Deny`] /
//! `Cancelled` / `Timeout`), and the executor audits each one
//! distinctly: "the user said no" is not "nobody answered" is not
//! "the user stopped the turn".
//!
//! Timeout: 60 seconds by default ([`DEFAULT_APPROVAL_TIMEOUT`]),
//! configurable via [`UiApprover::with_timeout`] (the app reads
//! `MIMINT_APPROVAL_TIMEOUT_SECS`). A timeout denies: the tool
//! never runs. A decision that arrives at or after the deadline
//! is treated as a timeout even if the reply channel delivered
//! it: the deadline is re-checked on decision arrival, so a
//! late Allow can never slip through at the logic layer.
//!
//! Request-time target snapshot (F4-07): when the worker wires a
//! snapshotter, the approver probes the target BEFORE the card
//! is shown — the window identity (for the card) and a luminance
//! snapshot (for the re-verify at execution time, held in a
//! shared [`TargetSnapshotSlot`]). The slot is cleared on every
//! non-Allow resolution, so a snapshot can never leak into a
//! later action; on Allow it stays for the tool to consume.
//! The job also carries the age of the newest stored frame at
//! request time, so the card can warn when the observation the
//! model acted on is older than the TTL (F4-08).
//!
//! Cancellation: the approver watches two shared token slots — the
//! per-turn slot (the "Hủy lượt" button) and the latching stop slot
//! (the "DỪNG NGAY" button). Either one tripping while a card waits
//! resolves the request as `Cancelled` immediately, without waiting
//! for the timeout.
//!
//! The worker also pokes a [`RepaintSignal`] when a job arrives, so
//! the UI thread repaints promptly even when its frame loop is
//! throttled (degraded mode / FPS fuse).
//!
//! A disconnected UI denies by error, as before: an error denies by
//! guard policy, and a disconnected UI must never silently allow a
//! consequential tool.
//!
//! [`Approver`]: mimint_core::Approver

use mimint_body::{TargetProbe, TargetSnapshotSlot, WindowIdentity};
use mimint_control::DEFAULT_MAX_OBSERVATION_AGE;
use mimint_core::{
    ApprovalDecision, ApprovalOutcome, ApprovalRequest, Approver, CancelToken, MimintError,
};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Default approval timeout: the card waits one minute, then the
/// request is denied as a timeout.
pub const DEFAULT_APPROVAL_TIMEOUT: Duration = Duration::from_secs(60);

/// A shared, replaceable [`CancelToken`]. The UI thread trips or
/// replaces the token; the worker and approver read the current one.
/// Replacement (resume after STOP) never un-cancels a token already
/// handed to a running turn — a turn started while stopped stays
/// stopped; only new turns see the fresh token.
pub struct TokenSlot {
    inner: Mutex<CancelToken>,
}

impl TokenSlot {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(CancelToken::new()),
        })
    }

    /// The current token (a clone sharing the same flag).
    pub fn get(&self) -> CancelToken {
        self.inner.lock().unwrap().clone()
    }

    /// Replace the current token (resume after stop, new turn).
    pub fn replace(&self, token: CancelToken) {
        *self.inner.lock().unwrap() = token;
    }

    /// Cancel the current token. Latching until replaced.
    pub fn cancel_current(&self) {
        self.inner.lock().unwrap().cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.get().is_cancelled()
    }
}

/// A one-bit wake-up flag: the worker sets it when an approval job
/// arrives; the UI thread takes it each frame and repaints
/// immediately when set.
pub struct RepaintSignal {
    flag: AtomicBool,
}

impl RepaintSignal {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            flag: AtomicBool::new(false),
        })
    }

    pub fn request(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    /// True when a repaint was requested since the last take.
    pub fn take(&self) -> bool {
        self.flag.swap(false, Ordering::SeqCst)
    }
}

/// One approval job travelling from the worker to the UI: the request
/// (bound by id), its deadline, and the one-shot reply channel.
/// `target` is the window under the action point at request time
/// (for the card; `None` when the probe was unavailable or the
/// tool has no point). `observation_age` is the age of the newest
/// stored frame at request time (`None` when no frame exists);
/// the card warns when it exceeds `max_observation_age`.
pub struct ApprovalJob {
    pub id: u64,
    pub tool: String,
    pub args: Value,
    pub deadline: Instant,
    pub timeout: Duration,
    pub target: Option<WindowIdentity>,
    pub observation_age: Option<Duration>,
    pub max_observation_age: Duration,
    pub reply: Sender<ApprovalDecision>,
}

/// Probe the target at a screen point: the window identity plus
/// the re-verify snapshot, or `None` when the body cannot probe
/// (unsupported body, timeout, broken actor). A failed probe
/// never blocks the card: the request proceeds without a
/// snapshot, and execution-time re-verification is skipped.
pub type Snapshotter = Arc<dyn Fn(i32, i32) -> Option<TargetProbe> + Send + Sync>;

/// Channel pair for approvals. The worker owns the [`UiApprover`]
/// (sender side); the UI thread owns the receiver and shows one card
/// per job, in order.
pub fn approval_channel() -> (Sender<ApprovalJob>, Receiver<ApprovalJob>) {
    std::sync::mpsc::channel()
}

/// Approver that asks the desktop UI. `approve_request` blocks the
/// calling (worker) thread until the user answers, the timeout
/// passes, or a cancellation token trips.
pub struct UiApprover {
    tx: Sender<ApprovalJob>,
    timeout: Duration,
    stop_slot: Option<Arc<TokenSlot>>,
    turn_slot: Option<Arc<TokenSlot>>,
    repaint: Option<Arc<RepaintSignal>>,
    /// Set while a card waits, cleared on every exit path. The
    /// body guard shares this flag: input while it is set trips
    /// the emergency stop (self-approval defense, D-FIX3-2).
    pending_flag: Option<Arc<AtomicBool>>,
    /// Request-time target probe + the slot its snapshot is
    /// held in until execution (F4-07). Both `None` without a
    /// body: the card shows no target, and no re-verify runs.
    snapshotter: Option<Snapshotter>,
    snapshot_slot: Option<Arc<TargetSnapshotSlot>>,
    /// Age of the newest stored frame, read at request time
    /// (F4-08). `None` without a body.
    frame_age: Option<Arc<dyn Fn() -> Option<Duration> + Send + Sync>>,
    /// Observation TTL the card warns against (F4-08). The same
    /// value the body tools enforce at execution time.
    max_observation_age: Duration,
}

impl UiApprover {
    pub fn new(tx: Sender<ApprovalJob>) -> Self {
        Self {
            tx,
            timeout: DEFAULT_APPROVAL_TIMEOUT,
            stop_slot: None,
            turn_slot: None,
            repaint: None,
            pending_flag: None,
            snapshotter: None,
            snapshot_slot: None,
            frame_age: None,
            max_observation_age: DEFAULT_MAX_OBSERVATION_AGE,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_slots(mut self, stop_slot: Arc<TokenSlot>, turn_slot: Arc<TokenSlot>) -> Self {
        self.stop_slot = Some(stop_slot);
        self.turn_slot = Some(turn_slot);
        self
    }

    pub fn with_repaint(mut self, repaint: Arc<RepaintSignal>) -> Self {
        self.repaint = Some(repaint);
        self
    }

    /// Share the approval-pending flag with the body guard: the
    /// SAME `Arc<AtomicBool>` the guard consults. While a card
    /// waits, the flag is set; it clears when the request
    /// resolves, on every path (allow/deny/timeout/cancelled/
    /// disconnected).
    pub fn with_pending_flag(mut self, flag: Arc<AtomicBool>) -> Self {
        self.pending_flag = Some(flag);
        self
    }

    /// Wire the request-time target probe (F4-07): the SAME
    /// `Arc<TargetSnapshotSlot>` the body tools consume from.
    /// The snapshotter is called with the action point before
    /// the card is shown; its snapshot is held in the slot
    /// until the decision resolves.
    pub fn with_target_probe(
        mut self,
        snapshotter: Snapshotter,
        slot: Arc<TargetSnapshotSlot>,
    ) -> Self {
        self.snapshotter = Some(snapshotter);
        self.snapshot_slot = Some(slot);
        self
    }

    /// Wire the frame-age reader (F4-08): the card warns when
    /// the newest stored frame is older than the TTL.
    pub fn with_frame_age(
        mut self,
        frame_age: Arc<dyn Fn() -> Option<Duration> + Send + Sync>,
    ) -> Self {
        self.frame_age = Some(frame_age);
        self
    }

    /// The observation TTL the card warns against. Must match
    /// the value the body tools enforce at execution time.
    pub fn with_max_observation_age(mut self, max_age: Duration) -> Self {
        self.max_observation_age = max_age;
        self
    }

    fn cancelled(&self) -> bool {
        self.stop_slot
            .as_ref()
            .is_some_and(|slot| slot.is_cancelled())
            || self
                .turn_slot
                .as_ref()
                .is_some_and(|slot| slot.is_cancelled())
    }
}

/// The screen point a tool acts on, when it has one. A drag is
/// probed at its FROM point (where the press lands); the TO
/// point is covered by the target check at execution time.
/// Tools without a point (typing, keys, screenshots, notebook)
/// return `None`: no snapshot is taken for them.
pub(crate) fn approval_point(tool: &str, args: &Value) -> Option<(i32, i32)> {
    let pair = |x_key: &str, y_key: &str| {
        let x = args.get(x_key)?.as_i64()?;
        let y = args.get(y_key)?.as_i64()?;
        Some((x as i32, y as i32))
    };
    match tool {
        "body_click" | "body_move" | "body_scroll" | "body_double_click" | "body_right_click" => {
            pair("x", "y")
        }
        "body_drag" => pair("from_x", "from_y"),
        _ => None,
    }
}

impl Approver for UiApprover {
    fn approve(&self, tool: &str, args: &Value) -> Result<bool, MimintError> {
        // The legacy boolean path, kept for direct callers and
        // tests: delegate to the id-bound wait with a synthetic id.
        // Timeout/cancelled are errors here (they deny, distinctly,
        // when the caller uses `approve_request` instead).
        let request = ApprovalRequest {
            id: 0,
            tool: tool.to_string(),
            args: args.clone(),
        };
        match self.approve_request(&request)?.decision {
            ApprovalDecision::Allow => Ok(true),
            ApprovalDecision::Deny => Ok(false),
            ApprovalDecision::Cancelled => Err(MimintError::Tool(
                "approval cancelled while waiting for a decision".to_string(),
            )),
            ApprovalDecision::Timeout => Err(MimintError::Tool(
                "approval timed out while waiting for a decision".to_string(),
            )),
        }
    }

    fn approve_request(&self, request: &ApprovalRequest) -> Result<ApprovalOutcome, MimintError> {
        // Set the pending flag for the whole wait; the guard
        // clears it on every exit path, including early errors.
        let _pending = self
            .pending_flag
            .as_ref()
            .map(|flag| PendingGuard::new(flag));
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        let deadline = Instant::now() + self.timeout;
        // Request-time target probe (F4-07), BEFORE the card is
        // shown: the identity goes on the card, the snapshot
        // goes in the slot for the execution-time re-verify.
        // A failed probe leaves both empty; the request
        // proceeds without a snapshot, never blocked.
        let mut target = None;
        let slot_guard = match (&self.snapshotter, &self.snapshot_slot) {
            (Some(snapshotter), Some(slot)) => {
                if let Some((x, y)) = approval_point(&request.tool, &request.args) {
                    if let Some(probe) = snapshotter(x, y) {
                        target = probe.identity;
                        if let Some(snapshot) = probe.snapshot {
                            slot.set(x, y, snapshot);
                        }
                    }
                }
                Some(SnapshotSlotGuard::new(Arc::clone(slot)))
            }
            _ => None,
        };
        // The slot clears on drop unless the decision is Allow:
        // a snapshot must never leak into a later action.
        let mut slot_guard = slot_guard;
        let observation_age = self.frame_age.as_ref().and_then(|f| f());
        let job = ApprovalJob {
            id: request.id,
            tool: request.tool.clone(),
            args: request.args.clone(),
            deadline,
            timeout: self.timeout,
            target,
            observation_age,
            max_observation_age: self.max_observation_age,
            reply: reply_tx,
        };
        // The UI is gone: deny by failing the approval.
        self.tx.send(job).map_err(|_| {
            MimintError::Tool("approval UI disconnected; denying the tool call".to_string())
        })?;
        if let Some(repaint) = &self.repaint {
            repaint.request();
        }
        loop {
            if self.cancelled() {
                return Ok(ApprovalOutcome::cancelled(request.id));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(ApprovalOutcome::timeout(request.id));
            }
            match reply_rx.recv_timeout(remaining.min(Duration::from_millis(50))) {
                Ok(decision) => {
                    // Late-Allow re-check (F4-06): a decision
                    // that arrives at or after the deadline is
                    // a timeout, even if the channel delivered
                    // it. The card is gone by then; the tool
                    // must not run on a stale Allow.
                    if decision == ApprovalDecision::Allow && Instant::now() >= deadline {
                        return Ok(ApprovalOutcome::timeout(request.id));
                    }
                    if decision == ApprovalDecision::Allow {
                        if let Some(guard) = slot_guard.as_mut() {
                            guard.keep();
                        }
                    }
                    return Ok(ApprovalOutcome {
                        request_id: request.id,
                        decision,
                    });
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(MimintError::Tool(
                        "approval UI disconnected while waiting for a decision; denying the tool call"
                            .to_string(),
                    ));
                }
            }
        }
    }

    fn label(&self) -> &'static str {
        "desktop approval card"
    }
}

/// Sets the shared pending flag on creation, clears it on drop —
/// so the flag is set for exactly the duration of one approval
/// wait, on every exit path.
struct PendingGuard<'a> {
    flag: &'a AtomicBool,
}

impl<'a> PendingGuard<'a> {
    fn new(flag: &'a AtomicBool) -> Self {
        flag.store(true, Ordering::SeqCst);
        Self { flag }
    }
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.flag.store(false, Ordering::SeqCst);
    }
}

/// Clears the snapshot slot on drop, unless the decision was
/// Allow ([`SnapshotSlotGuard::keep`]). Every non-Allow exit
/// path — deny, timeout, cancelled, disconnected, early error —
/// drops the guard and clears the slot, so a snapshot taken for
/// one request can never be consumed by a later action.
struct SnapshotSlotGuard {
    slot: Arc<TargetSnapshotSlot>,
    keep: bool,
}

impl SnapshotSlotGuard {
    fn new(slot: Arc<TargetSnapshotSlot>) -> Self {
        Self { slot, keep: false }
    }

    fn keep(&mut self) {
        self.keep = true;
    }
}

impl Drop for SnapshotSlotGuard {
    fn drop(&mut self) {
        if !self.keep {
            self.slot.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::thread;

    #[test]
    fn approval_round_trips_through_channels() {
        let (tx, rx) = approval_channel();
        let approver = UiApprover::new(tx);
        let handle = thread::spawn(move || {
            approver.approve_request(&ApprovalRequest {
                id: 7,
                tool: "notebook_add".to_string(),
                args: json!({"title": "t"}),
            })
        });
        let job = rx.recv().unwrap();
        assert_eq!(job.id, 7);
        assert_eq!(job.tool, "notebook_add");
        job.reply.send(ApprovalDecision::Allow).unwrap();
        let outcome = handle.join().unwrap().unwrap();
        assert_eq!(outcome.request_id, 7);
        assert_eq!(outcome.decision, ApprovalDecision::Allow);
    }

    #[test]
    fn legacy_approve_maps_decisions_to_bool() {
        let (tx, rx) = approval_channel();
        let approver = UiApprover::new(tx);
        let handle =
            thread::spawn(move || approver.approve("notebook_add", &json!({"title": "t"})));
        let job = rx.recv().unwrap();
        job.reply.send(ApprovalDecision::Deny).unwrap();
        assert!(!handle.join().unwrap().unwrap());
    }

    #[test]
    fn timeout_denies_distinctly() {
        let (tx, rx) = approval_channel();
        let approver = UiApprover::new(tx).with_timeout(Duration::from_millis(50));
        let handle = thread::spawn(move || {
            approver.approve_request(&ApprovalRequest {
                id: 3,
                tool: "notebook_add".to_string(),
                args: json!({}),
            })
        });
        let job = rx.recv().unwrap();
        assert_eq!(job.id, 3);
        // Nobody answers: the outcome must be Timeout, not Deny.
        let outcome = handle.join().unwrap().unwrap();
        assert_eq!(outcome.decision, ApprovalDecision::Timeout);
    }

    #[test]
    fn stop_token_cancels_a_waiting_request() {
        let (tx, rx) = approval_channel();
        let stop = TokenSlot::new();
        let turn = TokenSlot::new();
        let approver = UiApprover::new(tx)
            .with_timeout(Duration::from_secs(60))
            .with_slots(Arc::clone(&stop), turn);
        let handle = thread::spawn(move || {
            approver.approve_request(&ApprovalRequest {
                id: 9,
                tool: "notebook_add".to_string(),
                args: json!({}),
            })
        });
        let _job = rx.recv().unwrap();
        stop.cancel_current();
        let outcome = handle.join().unwrap().unwrap();
        assert_eq!(outcome.decision, ApprovalDecision::Cancelled);
    }

    #[test]
    fn repaint_signal_is_poked_on_job_arrival() {
        let (tx, rx) = approval_channel();
        let repaint = RepaintSignal::new();
        let approver = UiApprover::new(tx).with_repaint(Arc::clone(&repaint));
        let handle = thread::spawn(move || {
            approver.approve_request(&ApprovalRequest {
                id: 1,
                tool: "notebook_add".to_string(),
                args: json!({}),
            })
        });
        let job = rx.recv().unwrap();
        // The approver pokes the signal right after the send; poll
        // briefly — recv() returns the moment the send lands, which
        // can precede the poke by a scheduling quantum.
        let mut poked = false;
        for _ in 0..20 {
            if repaint.take() {
                poked = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(poked, "repaint signal must be poked on job arrival");
        assert!(!repaint.take());
        job.reply.send(ApprovalDecision::Deny).unwrap();
        let _ = handle.join().unwrap();
    }

    #[test]
    fn disconnected_ui_denies_with_error() {
        let (tx, rx) = approval_channel();
        drop(rx);
        let approver = UiApprover::new(tx);
        let err = approver
            .approve("notebook_add", &json!({}))
            .expect_err("disconnected UI must fail the approval");
        assert!(err.to_string().contains("disconnected"), "{err}");
    }

    #[test]
    fn dropped_reply_denies_with_error() {
        let (tx, rx) = approval_channel();
        let approver = UiApprover::new(tx);
        let handle = thread::spawn(move || approver.approve("notebook_add", &json!({})));
        let job = rx.recv().unwrap();
        drop(job.reply);
        let err = handle
            .join()
            .unwrap()
            .expect_err("dropped reply must fail the approval");
        assert!(err.to_string().contains("disconnected"), "{err}");
    }

    #[test]
    fn pending_flag_is_set_while_a_card_waits() {
        let (tx, rx) = approval_channel();
        let flag = Arc::new(AtomicBool::new(false));
        let approver = UiApprover::new(tx).with_pending_flag(Arc::clone(&flag));
        let flag_seen = Arc::clone(&flag);
        let handle = thread::spawn(move || {
            approver.approve_request(&ApprovalRequest {
                id: 9,
                tool: "body_click".to_string(),
                args: json!({"x": 1, "y": 2}),
            })
        });
        let job = rx.recv().unwrap();
        // The job arrived: the wait is in progress, so the flag
        // the body guard consults must be set RIGHT NOW.
        assert!(flag_seen.load(Ordering::SeqCst));
        job.reply.send(ApprovalDecision::Deny).unwrap();
        let outcome = handle.join().unwrap().unwrap();
        assert_eq!(outcome.decision, ApprovalDecision::Deny);
        // Resolved: the flag clears.
        assert!(!flag.load(Ordering::SeqCst));
    }

    #[test]
    fn pending_flag_clears_on_timeout() {
        let (tx, _rx) = approval_channel();
        let flag = Arc::new(AtomicBool::new(false));
        let approver = UiApprover::new(tx)
            .with_timeout(Duration::from_millis(50))
            .with_pending_flag(Arc::clone(&flag));
        let outcome = approver
            .approve_request(&ApprovalRequest {
                id: 10,
                tool: "body_click".to_string(),
                args: json!({"x": 1, "y": 2}),
            })
            .unwrap();
        assert_eq!(outcome.decision, ApprovalDecision::Timeout);
        assert!(!flag.load(Ordering::SeqCst));
    }

    #[test]
    fn default_timeout_is_sixty_seconds() {
        assert_eq!(DEFAULT_APPROVAL_TIMEOUT, Duration::from_secs(60));
    }

    #[test]
    fn late_allow_is_a_timeout_not_an_allow() {
        let (tx, rx) = approval_channel();
        let approver = UiApprover::new(tx).with_timeout(Duration::from_millis(50));
        let handle = thread::spawn(move || {
            approver.approve_request(&ApprovalRequest {
                id: 11,
                tool: "body_click".to_string(),
                args: json!({"x": 1, "y": 2}),
            })
        });
        let job = rx.recv().unwrap();
        // The user answers AFTER the deadline: the outcome
        // must be Timeout, never Allow.
        thread::sleep(Duration::from_millis(150));
        let _ = job.reply.send(ApprovalDecision::Allow);
        let outcome = handle.join().unwrap().unwrap();
        assert_eq!(outcome.decision, ApprovalDecision::Timeout);
    }

    fn fixed_probe() -> TargetProbe {
        TargetProbe {
            identity: Some(WindowIdentity {
                window_id: Some(0xabc),
                class: Some("editor".to_string()),
                title: None,
                exe_basename: Some("code".to_string()),
            }),
            snapshot: Some(mimint_body::TargetSnapshot {
                window_id: Some(0xabc),
                grid: [128; 64],
            }),
        }
    }

    #[test]
    fn job_carries_target_and_observation_age() {
        let (tx, rx) = approval_channel();
        let slot = Arc::new(TargetSnapshotSlot::new());
        let approver = UiApprover::new(tx)
            .with_target_probe(Arc::new(|_, _| Some(fixed_probe())), Arc::clone(&slot))
            .with_frame_age(Arc::new(|| Some(Duration::from_secs(42))))
            .with_max_observation_age(Duration::from_secs(300));
        let handle = thread::spawn(move || {
            approver.approve_request(&ApprovalRequest {
                id: 12,
                tool: "body_click".to_string(),
                args: json!({"x": 10, "y": 20}),
            })
        });
        let job = rx.recv().unwrap();
        assert_eq!(job.timeout, DEFAULT_APPROVAL_TIMEOUT);
        let target = job.target.expect("probe identity on the job");
        assert_eq!(target.window_id, Some(0xabc));
        assert_eq!(target.class.as_deref(), Some("editor"));
        assert_eq!(job.observation_age, Some(Duration::from_secs(42)));
        assert_eq!(job.max_observation_age, Duration::from_secs(300));
        // The snapshot is in the slot, bound to the point.
        assert!(slot.take_for(10, 20).is_some());
        job.reply.send(ApprovalDecision::Deny).unwrap();
        let outcome = handle.join().unwrap().unwrap();
        assert_eq!(outcome.decision, ApprovalDecision::Deny);
    }

    #[test]
    fn snapshot_slot_survives_allow_and_clears_on_deny() {
        for (decision, expect_filled) in [
            (ApprovalDecision::Allow, true),
            (ApprovalDecision::Deny, false),
        ] {
            let (tx, rx) = approval_channel();
            let slot = Arc::new(TargetSnapshotSlot::new());
            let approver = UiApprover::new(tx)
                .with_target_probe(Arc::new(|_, _| Some(fixed_probe())), Arc::clone(&slot));
            let handle = thread::spawn(move || {
                approver.approve_request(&ApprovalRequest {
                    id: 13,
                    tool: "body_click".to_string(),
                    args: json!({"x": 5, "y": 6}),
                })
            });
            let job = rx.recv().unwrap();
            job.reply.send(decision).unwrap();
            let outcome = handle.join().unwrap().unwrap();
            assert_eq!(outcome.decision, decision);
            let filled = slot.take_for(5, 6).is_some();
            assert_eq!(filled, expect_filled, "decision {decision:?}");
        }
    }

    #[test]
    fn snapshot_slot_clears_on_timeout() {
        let (tx, rx) = approval_channel();
        let slot = Arc::new(TargetSnapshotSlot::new());
        let approver = UiApprover::new(tx)
            .with_timeout(Duration::from_millis(50))
            .with_target_probe(Arc::new(|_, _| Some(fixed_probe())), Arc::clone(&slot));
        let handle = thread::spawn(move || {
            approver.approve_request(&ApprovalRequest {
                id: 14,
                tool: "body_click".to_string(),
                args: json!({"x": 5, "y": 6}),
            })
        });
        let _job = rx.recv().unwrap();
        // Nobody answers: the wait times out, and the slot
        // must be cleared (the probe filled it before the
        // card was shown — see job_carries_target).
        let outcome = handle.join().unwrap().unwrap();
        assert_eq!(outcome.decision, ApprovalDecision::Timeout);
        assert!(slot.take_for(5, 6).is_none(), "timeout clears the slot");
    }

    #[test]
    fn approval_point_uses_the_drag_start_and_skips_pointless_tools() {
        assert_eq!(
            approval_point("body_click", &json!({"x": 3, "y": 4})),
            Some((3, 4))
        );
        assert_eq!(
            approval_point(
                "body_drag",
                &json!({"from_x": 1, "from_y": 2, "to_x": 9, "to_y": 9})
            ),
            Some((1, 2))
        );
        assert_eq!(
            approval_point("body_type_text", &json!({"text": "hi"})),
            None
        );
        assert_eq!(approval_point("body_screenshot", &json!({})), None);
        assert_eq!(approval_point("notebook_add", &json!({})), None);
    }
}
