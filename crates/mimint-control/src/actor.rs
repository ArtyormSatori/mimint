//! The body actor: one dedicated thread owns the body.
//!
//! The `Body` trait is synchronous and `!Sync` in spirit (bodies
//! own OS connections and threads of their own); the brain is
//! async and its tools are `Send + Sync`. The actor bridges
//! the two: tools send [`Action`]s over a channel, the actor
//! thread executes them against the body, and replies come
//! back over a per-request channel.
//!
//! Per-action timeout (P4.6.3): every request waits at most
//! `timeout` for its reply. On timeout the actor is marked
//! broken and fails fast afterwards: the body thread may still
//! be stuck inside a platform call, and sending it more work
//! would pile up behind the stuck call. The timeout bounds
//! the WAIT, not the platform call — a stuck body thread is
//! detached, never killed (there is no safe way to kill a
//! thread stuck in a foreign OS call). The broken state is
//! terminal for the actor: the caller builds a fresh actor
//! for a fresh body.
//!
//! Unknown outcomes (F4-14): a timeout, a dead thread, or a
//! panicking body reports [`BodyError::UnknownOutcome`], never
//! a generic backend error — the action may or may not have
//! executed, and the caller must not retry automatically. A
//! panicking body ALSO breaks the actor and ends the loop: the
//! body's state after a panic is unknown, and serving more
//! requests against it would be guessing.
//!
//! Stop integration: the actor checks the shared [`StopFlag`]
//! before executing each action. A tripped flag refuses the
//! action with a typed error and never touches the body.
//!
//! Target probes (F4-07): [`BodyActor::probe_target`] asks the
//! body, through the same serialized channel, for the window
//! identity and re-verify snapshot under a point — so the
//! approver's snapshot and the action's execution can never
//! interleave with another action.

use mimint_body::{ActOutcome, Action, Body, BodyError, StopFlag, TargetProbe};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Default per-action timeout: generous for a local body, but
/// bounded. A body that cannot answer in 30s is wedged.
pub const DEFAULT_ACTION_TIMEOUT: Duration = Duration::from_secs(30);

/// What the actor thread can be asked to do. Actions and probes
/// share one channel, so a probe can never run concurrently
/// with an action against the same body.
enum RequestKind {
    Act(Action),
    ProbeTarget { x: i32, y: i32 },
}

/// A reply from the actor thread, matching the request kind.
enum RequestReply {
    Action(Result<ActOutcome, BodyError>),
    ProbeTarget(Result<TargetProbe, BodyError>),
}

struct Request {
    kind: RequestKind,
    reply: Sender<RequestReply>,
}

struct ActorInner {
    tx: Sender<Request>,
    /// Set once a request times out, the thread dies, or the
    /// body panics: all later requests fail fast without
    /// touching the channel. Shared with the loop thread so a
    /// panic inside the loop breaks the actor too.
    broken: Arc<Mutex<bool>>,
}

/// A body behind a dedicated thread, with a per-action timeout.
pub struct BodyActor {
    inner: Arc<ActorInner>,
    stop: Arc<StopFlag>,
    timeout: Duration,
}

impl BodyActor {
    /// Spawn the actor thread owning `body`. The thread runs
    /// until the last `BodyActor` handle is dropped (the
    /// channel closes) or a request times out (the thread is
    /// detached and the actor fails fast).
    pub fn spawn(body: Box<dyn Body>, stop: Arc<StopFlag>) -> BodyActor {
        Self::spawn_with_timeout(body, stop, DEFAULT_ACTION_TIMEOUT)
    }

    pub fn spawn_with_timeout(
        body: Box<dyn Body>,
        stop: Arc<StopFlag>,
        timeout: Duration,
    ) -> BodyActor {
        let (tx, rx) = channel::<Request>();
        let broken = Arc::new(Mutex::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread_broken = Arc::clone(&broken);
        std::thread::spawn(move || actor_loop(body, rx, thread_stop, thread_broken));
        BodyActor {
            inner: Arc::new(ActorInner { tx, broken }),
            stop,
            timeout,
        }
    }

    /// Execute one action, bounded by the per-action timeout.
    /// A tripped stop flag refuses before the body is touched.
    /// A timeout or a dead thread reports `UnknownOutcome`:
    /// the action may have executed, and must not be retried.
    pub fn act(&self, action: Action) -> Result<ActOutcome, BodyError> {
        if self.stop.is_tripped() {
            return Err(BodyError::Backend(
                "emergency stop is latched; the body refuses all actions".to_string(),
            ));
        }
        if self.is_broken() {
            return Err(BodyError::Backend(
                "body actor is broken (a previous action timed out); rebuild the actor".to_string(),
            ));
        }
        let (reply_tx, reply_rx) = channel::<RequestReply>();
        if self
            .inner
            .tx
            .send(Request {
                kind: RequestKind::Act(action),
                reply: reply_tx,
            })
            .is_err()
        {
            // The thread is gone BEFORE receiving: the action
            // never started, so this is a known non-execution,
            // not an unknown outcome.
            self.mark_broken();
            return Err(BodyError::Backend("body actor thread is gone".to_string()));
        }
        match reply_rx.recv_timeout(self.timeout) {
            Ok(RequestReply::Action(outcome)) => outcome,
            Ok(RequestReply::ProbeTarget(_)) => {
                self.mark_broken();
                Err(BodyError::Backend(
                    "body actor replied with the wrong kind; actor marked broken".to_string(),
                ))
            }
            Err(RecvTimeoutError::Timeout) => {
                self.mark_broken();
                Err(BodyError::UnknownOutcome {
                    reason: format!(
                        "body action timed out after {:?}; it may or may not have executed, and the actor is marked broken",
                        self.timeout
                    ),
                })
            }
            Err(RecvTimeoutError::Disconnected) => {
                self.mark_broken();
                Err(BodyError::UnknownOutcome {
                    reason: "body actor thread died while executing; the action may or may not have executed".to_string(),
                })
            }
        }
    }

    /// Probe the target at (`x`, `y`): the window identity and
    /// re-verify snapshot, taken in the actor's serialized
    /// order. A tripped stop or a broken actor fails with a
    /// backend error (a probe is a read: nothing may have
    /// executed, so no `UnknownOutcome`).
    ///
    /// A probe TIMEOUT does NOT break the actor, unlike an
    /// action timeout: a probe changes no state, so the late
    /// reply is dropped harmlessly and the loop keeps serving
    /// (the next action gets its own verdict). Only a dead
    /// thread breaks the actor on the probe path.
    pub fn probe_target(&self, x: i32, y: i32) -> Result<TargetProbe, BodyError> {
        self.probe_target_with_timeout(x, y, self.timeout)
    }

    /// Like [`BodyActor::probe_target`], with an explicit wait
    /// bound. The approval card uses a short bound (a snapshot
    /// must never delay the card): on timeout the caller simply
    /// proceeds without a snapshot.
    pub fn probe_target_with_timeout(
        &self,
        x: i32,
        y: i32,
        timeout: Duration,
    ) -> Result<TargetProbe, BodyError> {
        if self.stop.is_tripped() {
            return Err(BodyError::Backend(
                "emergency stop is latched; the body refuses all actions".to_string(),
            ));
        }
        if self.is_broken() {
            return Err(BodyError::Backend(
                "body actor is broken (a previous action timed out); rebuild the actor".to_string(),
            ));
        }
        let (reply_tx, reply_rx) = channel::<RequestReply>();
        if self
            .inner
            .tx
            .send(Request {
                kind: RequestKind::ProbeTarget { x, y },
                reply: reply_tx,
            })
            .is_err()
        {
            self.mark_broken();
            return Err(BodyError::Backend("body actor thread is gone".to_string()));
        }
        match reply_rx.recv_timeout(timeout) {
            Ok(RequestReply::ProbeTarget(result)) => result,
            Ok(RequestReply::Action(_)) => {
                self.mark_broken();
                Err(BodyError::Backend(
                    "body actor replied with the wrong kind; actor marked broken".to_string(),
                ))
            }
            Err(RecvTimeoutError::Timeout) => {
                // No mark_broken: the probe is a read, the
                // late reply is dropped, the loop keeps
                // serving. See `probe_target`.
                Err(BodyError::Backend(format!(
                    "body target probe timed out after {timeout:?}; no snapshot taken"
                )))
            }
            Err(RecvTimeoutError::Disconnected) => {
                self.mark_broken();
                Err(BodyError::Backend(
                    "body actor thread died while probing".to_string(),
                ))
            }
        }
    }

    /// True once the actor is broken (timeout or dead thread).
    pub fn is_broken(&self) -> bool {
        *self.inner.broken.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The shared stop flag this actor consults.
    pub fn stop_flag(&self) -> &Arc<StopFlag> {
        &self.stop
    }

    fn mark_broken(&self) {
        *self.inner.broken.lock().unwrap_or_else(|p| p.into_inner()) = true;
    }
}

impl Clone for BodyActor {
    fn clone(&self) -> BodyActor {
        BodyActor {
            inner: Arc::clone(&self.inner),
            stop: Arc::clone(&self.stop),
            timeout: self.timeout,
        }
    }
}

fn actor_loop(
    mut body: Box<dyn Body>,
    rx: Receiver<Request>,
    stop: Arc<StopFlag>,
    broken: Arc<Mutex<bool>>,
) {
    for request in rx {
        if stop.is_tripped() {
            let reply = match request.kind {
                RequestKind::Act(_) => RequestReply::Action(Err(BodyError::Backend(
                    "emergency stop is latched; the body refuses all actions".to_string(),
                ))),
                RequestKind::ProbeTarget { .. } => {
                    RequestReply::ProbeTarget(Err(BodyError::Backend(
                        "emergency stop is latched; the body refuses all actions".to_string(),
                    )))
                }
            };
            let _ = request.reply.send(reply);
            continue;
        }
        // A panicking body must not kill the loop silently — and
        // must not keep serving either: the body's state after a
        // panic is unknown. Catch the panic, report it as
        // `UnknownOutcome` (the action may have partially
        // executed), mark the actor broken, and end the loop.
        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match request.kind {
                RequestKind::Act(ref action) => RequestReply::Action(body.act(action)),
                RequestKind::ProbeTarget { x, y } => {
                    let identity = body.window_at(x, y);
                    let snapshot = body.snapshot_target(x, y);
                    RequestReply::ProbeTarget(Ok(TargetProbe { identity, snapshot }))
                }
            }));
        match outcome {
            Ok(reply) => {
                if request.reply.send(reply).is_err() {
                    // The requester went away (timed out): the
                    // loop keeps running — the NEXT request will
                    // still be served if the caller builds a
                    // fresh actor... but this actor is marked
                    // broken on the caller side, so in practice
                    // the loop idles until the channel closes.
                }
            }
            Err(_) => {
                *broken.lock().unwrap_or_else(|p| p.into_inner()) = true;
                let reply = match request.kind {
                    RequestKind::Act(_) => RequestReply::Action(Err(BodyError::UnknownOutcome {
                        reason: "body panicked while executing the action; the action may have partially executed".to_string(),
                    })),
                    RequestKind::ProbeTarget { .. } => {
                        RequestReply::ProbeTarget(Err(BodyError::Backend(
                            "body panicked while probing the target".to_string(),
                        )))
                    }
                };
                let _ = request.reply.send(reply);
                break;
            }
        }
    }
}

/// Bridge a [`StopFlag`] to a [`CancelToken`]: a background
/// thread polls the flag and cancels the token when the flag
/// trips, so a body-level stop (global hotkey, self-exclusion
/// trip) aborts the brain turn that is driving the body.
///
/// The bridge polls every 50ms — cheap, and it keeps the two
/// crates decoupled (mimint-body knows nothing about
/// cancellation tokens). The thread exits when the flag trips
/// (its job is done — it cancels the token first) or when the
/// returned handle is dropped (the handle requests exit and
/// joins the thread).
pub mod bridge {
    use mimint_body::StopFlag;
    use mimint_core::CancelToken;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    /// A running stop bridge. Dropping it requests the bridge
    /// thread to exit (it notices within one poll interval).
    pub struct StopBridge {
        exit: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Drop for StopBridge {
        fn drop(&mut self) {
            self.exit.store(true, Ordering::SeqCst);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    /// Start bridging `flag` → `token`. When the flag trips,
    /// the token is cancelled. Returns the bridge handle: keep
    /// it alive for as long as the turn runs.
    pub fn stop_bridge(flag: Arc<StopFlag>, token: CancelToken) -> StopBridge {
        let exit = Arc::new(AtomicBool::new(false));
        let thread_exit = Arc::clone(&exit);
        let thread = std::thread::spawn(move || {
            while !thread_exit.load(Ordering::SeqCst) {
                if flag.is_tripped() {
                    token.cancel();
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        StopBridge {
            exit,
            thread: Some(thread),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mimint_body::conformance::TargetOracle;
    use mimint_body::{GuardedBody, MockBody, RateLimits};

    fn actor_with_mock() -> BodyActor {
        BodyActor::spawn(Box::new(MockBody::new()), StopFlag::new())
    }

    #[test]
    fn actor_executes_actions_and_returns_frames() {
        let actor = actor_with_mock();
        let outcome = actor.act(Action::Screenshot).unwrap();
        assert!(matches!(outcome, ActOutcome::Frame(_)));
        let outcome = actor.act(Action::Click { x: 10, y: 10 }).unwrap();
        assert!(matches!(outcome, ActOutcome::Done));
    }

    #[test]
    fn actor_reports_body_errors() {
        let actor = actor_with_mock();
        let err = actor.act(Action::Click { x: 9999, y: 9999 }).unwrap_err();
        assert_eq!(err.code(), "out_of_bounds");
    }

    #[test]
    fn tripped_stop_refuses_before_the_body() {
        let stop = StopFlag::new();
        let body = MockBody::new();
        let mut oracle = body.oracle();
        let actor = BodyActor::spawn(Box::new(body), Arc::clone(&stop));
        stop.trip();
        let err = actor.act(Action::Click { x: 10, y: 10 }).unwrap_err();
        assert!(err.message().contains("emergency stop"));
        assert!(oracle.received_clicks().is_empty());
    }

    #[test]
    fn guarded_body_inside_the_actor_enforces_its_limits() {
        let inner = MockBody::new();
        let mut guarded = GuardedBody::new(inner, RateLimits::default());
        guarded.set_excluded_rects(vec![mimint_body::ScreenRect {
            x: 0,
            y: 0,
            width: 40,
            height: 40,
        }]);
        let actor = BodyActor::spawn(Box::new(guarded), StopFlag::new());
        let err = actor.act(Action::Click { x: 10, y: 10 }).unwrap_err();
        assert_eq!(err.code(), "forbidden_target");
        assert!(actor.act(Action::Click { x: 100, y: 100 }).is_ok());
    }

    #[test]
    fn timeout_marks_the_actor_broken_and_fails_fast() {
        struct SleepingBody;
        impl Body for SleepingBody {
            fn name(&self) -> &'static str {
                "sleeping"
            }
            fn display_size(&mut self) -> Result<(u32, u32), BodyError> {
                Ok((800, 600))
            }
            fn screenshot(&mut self) -> Result<mimint_body::Framebuffer, BodyError> {
                std::thread::sleep(Duration::from_secs(30));
                Ok(mimint_body::Framebuffer::new(800, 600, vec![0u8; 800 * 600 * 4]).unwrap())
            }
            fn click(&mut self, _x: i32, _y: i32) -> Result<(), BodyError> {
                Ok(())
            }
            fn mouse_move(&mut self, _x: i32, _y: i32) -> Result<(), BodyError> {
                Ok(())
            }
            fn type_text(&mut self, _text: &str) -> Result<(), BodyError> {
                Ok(())
            }
            fn key(&mut self, _key: mimint_body::Key) -> Result<(), BodyError> {
                Ok(())
            }
            fn permissions(&mut self) -> mimint_body::Permissions {
                mimint_body::Permissions {
                    screen_capture: mimint_body::PermissionState::Granted,
                    input_control: mimint_body::PermissionState::Granted,
                }
            }
        }
        let actor = BodyActor::spawn_with_timeout(
            Box::new(SleepingBody),
            StopFlag::new(),
            Duration::from_millis(100),
        );
        let err = actor.act(Action::Screenshot).unwrap_err();
        assert_eq!(err.code(), "unknown_outcome");
        assert!(err.message().contains("timed out"), "{}", err.message());
        assert!(actor.is_broken());
        // Fail-fast: the second call returns immediately.
        let start = std::time::Instant::now();
        let err = actor.act(Action::Click { x: 1, y: 1 }).unwrap_err();
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(err.message().contains("broken"));
    }

    #[test]
    fn hanging_body_reports_unknown_outcome_and_breaks_the_actor() {
        use mimint_body::Fault;
        let body = MockBody::with_fault(Fault::Hang);
        let actor = BodyActor::spawn_with_timeout(
            Box::new(body),
            StopFlag::new(),
            Duration::from_millis(100),
        );
        let err = actor.act(Action::Click { x: 10, y: 10 }).unwrap_err();
        assert_eq!(err.code(), "unknown_outcome");
        assert!(actor.is_broken());
    }

    #[test]
    fn panicking_body_reports_unknown_outcome_and_breaks_the_actor() {
        struct PanicBody;
        impl Body for PanicBody {
            fn name(&self) -> &'static str {
                "panic"
            }
            fn display_size(&mut self) -> Result<(u32, u32), BodyError> {
                Ok((800, 600))
            }
            fn screenshot(&mut self) -> Result<mimint_body::Framebuffer, BodyError> {
                Ok(mimint_body::Framebuffer::new(2, 2, vec![0u8; 16]).unwrap())
            }
            fn click(&mut self, _x: i32, _y: i32) -> Result<(), BodyError> {
                panic!("injected body panic");
            }
            fn mouse_move(&mut self, _x: i32, _y: i32) -> Result<(), BodyError> {
                Ok(())
            }
            fn type_text(&mut self, _text: &str) -> Result<(), BodyError> {
                Ok(())
            }
            fn key(&mut self, _key: mimint_body::Key) -> Result<(), BodyError> {
                Ok(())
            }
            fn permissions(&mut self) -> mimint_body::Permissions {
                mimint_body::Permissions {
                    screen_capture: mimint_body::PermissionState::Granted,
                    input_control: mimint_body::PermissionState::Granted,
                }
            }
        }
        let actor = BodyActor::spawn(Box::new(PanicBody), StopFlag::new());
        let err = actor.act(Action::Click { x: 10, y: 10 }).unwrap_err();
        assert_eq!(err.code(), "unknown_outcome");
        assert!(err.message().contains("panicked"), "{}", err.message());
        assert!(actor.is_broken(), "a panic breaks the actor");
        // Fail-fast afterwards: the loop ended, no more work.
        let err = actor.act(Action::Click { x: 10, y: 10 }).unwrap_err();
        assert!(err.message().contains("broken"));
    }

    #[test]
    fn probe_target_returns_identity_and_snapshot_from_the_mock() {
        use mimint_body::{MockWindow, ScreenRect};
        let body = MockBody::new();
        body.set_windows(vec![MockWindow::new(
            7,
            ScreenRect::new(100, 100, 200, 200),
        )
        .with_class("editor")
        .with_fill((200, 40, 40))]);
        let actor = BodyActor::spawn(Box::new(body), StopFlag::new());
        let probe = actor.probe_target(150, 150).unwrap();
        let identity = probe.identity.unwrap();
        assert_eq!(identity.window_id, Some(7));
        assert_eq!(identity.class.as_deref(), Some("editor"));
        let snapshot = probe.snapshot.unwrap();
        assert_eq!(snapshot.window_id, Some(7));
        // Outside every window: no identity, but the snapshot
        // still describes the pixels under the point.
        let probe = actor.probe_target(10, 10).unwrap();
        assert!(probe.identity.is_none());
        assert!(probe.snapshot.is_some());
    }

    #[test]
    fn probe_target_on_a_body_without_windows_reports_none() {
        let actor = actor_with_mock();
        let probe = actor.probe_target(10, 10).unwrap();
        assert!(probe.identity.is_none(), "no windows scripted");
        assert!(probe.snapshot.is_some(), "the mock always snapshots");
    }

    #[test]
    fn probe_timeout_does_not_break_the_actor() {
        use mimint_body::{Framebuffer, Key, Permissions};
        use std::sync::atomic::{AtomicBool, Ordering};
        /// Body whose first `window_at` sleeps, then answers.
        struct SlowProbeBody {
            slept: AtomicBool,
        }
        impl mimint_body::Body for SlowProbeBody {
            fn name(&self) -> &'static str {
                "slow-probe"
            }
            fn display_size(&mut self) -> Result<(u32, u32), mimint_body::BodyError> {
                Ok((800, 600))
            }
            fn screenshot(&mut self) -> Result<Framebuffer, mimint_body::BodyError> {
                Framebuffer::new(800, 600, vec![0u8; 800 * 600 * 4])
            }
            fn click(&mut self, _x: i32, _y: i32) -> Result<(), mimint_body::BodyError> {
                Ok(())
            }
            fn mouse_move(&mut self, _x: i32, _y: i32) -> Result<(), mimint_body::BodyError> {
                Ok(())
            }
            fn type_text(&mut self, _text: &str) -> Result<(), mimint_body::BodyError> {
                Ok(())
            }
            fn key(&mut self, _key: Key) -> Result<(), mimint_body::BodyError> {
                Ok(())
            }
            fn permissions(&mut self) -> Permissions {
                Permissions {
                    screen_capture: mimint_body::PermissionState::Granted,
                    input_control: mimint_body::PermissionState::Granted,
                }
            }
            fn window_at(&mut self, _x: i32, _y: i32) -> Option<mimint_body::WindowIdentity> {
                if !self.slept.swap(true, Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(300));
                }
                None
            }
        }
        let actor = BodyActor::spawn(
            Box::new(SlowProbeBody {
                slept: AtomicBool::new(false),
            }),
            StopFlag::new(),
        );
        // The first probe times out on the short bound...
        let err = actor
            .probe_target_with_timeout(10, 10, Duration::from_millis(50))
            .unwrap_err();
        assert_eq!(err.code(), "backend");
        assert!(err.message().contains("timed out"), "{}", err.message());
        assert!(
            !actor.is_broken(),
            "a probe timeout must not break the actor"
        );
        // ...and the actor keeps serving: the next probe,
        // with a patient bound, succeeds.
        let probe = actor
            .probe_target_with_timeout(10, 10, Duration::from_secs(5))
            .unwrap();
        assert!(probe.identity.is_none());
    }

    #[test]
    fn stop_bridge_cancels_the_token_when_the_flag_trips() {
        let stop = StopFlag::new();
        let token = mimint_core::CancelToken::new();
        let _bridge = bridge::stop_bridge(Arc::clone(&stop), token.clone());
        assert!(!token.is_cancelled());
        stop.trip();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !token.is_cancelled() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(token.is_cancelled());
    }
}
