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
//! Stop integration: the actor checks the shared [`StopFlag`]
//! before executing each action. A tripped flag refuses the
//! action with a typed error and never touches the body.

use mimint_body::{ActOutcome, Action, Body, BodyError, StopFlag};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Default per-action timeout: generous for a local body, but
/// bounded. A body that cannot answer in 30s is wedged.
pub const DEFAULT_ACTION_TIMEOUT: Duration = Duration::from_secs(30);

type Reply = Result<ActOutcome, BodyError>;

struct Request {
    action: Action,
    reply: Sender<Reply>,
}

struct ActorInner {
    tx: Sender<Request>,
    /// Set once a request times out (or the thread dies): all
    /// later requests fail fast without touching the channel.
    broken: Mutex<bool>,
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
        let thread_stop = Arc::clone(&stop);
        std::thread::spawn(move || actor_loop(body, rx, thread_stop));
        BodyActor {
            inner: Arc::new(ActorInner {
                tx,
                broken: Mutex::new(false),
            }),
            stop,
            timeout,
        }
    }

    /// Execute one action, bounded by the per-action timeout.
    /// A tripped stop flag refuses before the body is touched.
    pub fn act(&self, action: Action) -> Result<ActOutcome, BodyError> {
        if self.stop.is_tripped() {
            return Err(BodyError::Backend(
                "emergency stop is latched; the body refuses all actions".to_string(),
            ));
        }
        if *self.inner.broken.lock().unwrap_or_else(|p| p.into_inner()) {
            return Err(BodyError::Backend(
                "body actor is broken (a previous action timed out); rebuild the actor".to_string(),
            ));
        }
        let (reply_tx, reply_rx) = channel::<Reply>();
        if self
            .inner
            .tx
            .send(Request {
                action,
                reply: reply_tx,
            })
            .is_err()
        {
            self.mark_broken();
            return Err(BodyError::Backend("body actor thread is gone".to_string()));
        }
        match reply_rx.recv_timeout(self.timeout) {
            Ok(outcome) => outcome,
            Err(RecvTimeoutError::Timeout) => {
                self.mark_broken();
                Err(BodyError::Backend(format!(
                    "body action timed out after {:?}; actor marked broken",
                    self.timeout
                )))
            }
            Err(RecvTimeoutError::Disconnected) => {
                self.mark_broken();
                Err(BodyError::Backend(
                    "body actor thread died while executing".to_string(),
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

fn actor_loop(mut body: Box<dyn Body>, rx: Receiver<Request>, stop: Arc<StopFlag>) {
    for request in rx {
        if stop.is_tripped() {
            let _ = request.reply.send(Err(BodyError::Backend(
                "emergency stop is latched; the body refuses all actions".to_string(),
            )));
            continue;
        }
        // A panicking body must not kill the loop silently: catch
        // the panic, report it as a backend error, and keep the
        // thread alive for the next request. (A body that panics
        // repeatedly is the caller's problem to notice via the
        // errors; the actor does not guess at body state.)
        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body.act(&request.action)));
        let reply = match outcome {
            Ok(result) => result,
            Err(_) => Err(BodyError::Backend(
                "body panicked while executing the action".to_string(),
            )),
        };
        if request.reply.send(reply).is_err() {
            // The requester went away (timed out): the loop keeps
            // running — the NEXT request will still be served if
            // the caller builds a fresh actor... but this actor
            // is marked broken on the caller side, so in practice
            // the loop idles until the channel closes.
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
        assert!(err.message().contains("timed out"), "{}", err.message());
        assert!(actor.is_broken());
        // Fail-fast: the second call returns immediately.
        let start = std::time::Instant::now();
        let err = actor.act(Action::Click { x: 1, y: 1 }).unwrap_err();
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(err.message().contains("broken"));
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
