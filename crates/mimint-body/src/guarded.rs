//! [`GuardedBody`]: the body-level safety wrapper (Phase 3 fix
//! pass, D-FIX3-2/D-FIX3-6/D-FIX3-9).
//!
//! One wrapper, four checks, all enforced BEFORE the inner body acts:
//!
//! 1. **Stop latch**: when the shared stop flag is set (emergency
//!    stop), no action starts. The flag is latching; only the owner
//!    clears it, by replacing the flag.
//! 2. **Self-exclusion zone**: clicks/moves into a configured
//!    rectangle are refused with [`BodyError::ForbiddenTarget`].
//!    When the approval-pending flag is set (an approval card is
//!    waiting), ANY consequential attempt also trips the stop latch:
//!    input aimed at the assistant's own window while it waits for a
//!    human decision is the self-approval attack, and it must halt
//!    the session, not just the one action.
//! 3. **Rate limits**: a per-session cap, a per-minute cap (sliding
//!    window), and repeat detection (the same action over and over
//!    is a runaway loop until proven otherwise). Refusals are
//!    [`BodyError::RateLimited`].
//! 4. **Focus verification**: delegated to the inner body
//!    ([`Body::verify_focus`]) before click/type/key, when a focus
//!    expectation is configured.
//!
//! The wrapper is transparent when no limit is hit: it adds no
//! behavior of its own to screenshots or coordinates. `bodyctl` and
//! the conformance runner wrap every real body in it. The brain-side
//! enforcement (same limits applied to the body tool) is Phase 4
//! work; the wrapper exists now so the CLI and harness paths — the
//! ones that bypass the brain — are already guarded.
//!
//! Wiring (Phase 4): the approval-pending flag is the SAME
//! `Arc<AtomicBool>` the `UiApprover` in `mimint-ui` sets while a
//! card waits, and the shared exclusion list is the
//! SAME `Arc<RwLock<Vec<ScreenRect>>>` the app updates every frame
//! from its window rectangle — so the guard always sees the live
//! app window, not a stale copy. Both shares are one-line
//! `with_*` calls; the mechanism — flag in, latch out — is
//! implemented and tested here.

use crate::action::Key;
use crate::body::{Body, Framebuffer, FramebufferPool, Permissions, ScreenRect};
use crate::error::BodyError;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

/// A shared, latching stop flag. The UI (Phase 4) and the wrapper
/// share one `Arc<StopFlag>`; `trip()` latches it, `is_tripped()`
/// reads it. There is no untrip: a new session uses a new flag.
#[derive(Debug, Default)]
pub struct StopFlag {
    tripped: AtomicBool,
}

impl StopFlag {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn trip(&self) {
        self.tripped.store(true, Ordering::SeqCst);
    }

    pub fn is_tripped(&self) -> bool {
        self.tripped.load(Ordering::SeqCst)
    }
}

/// Rate limits enforced by [`GuardedBody`].
#[derive(Debug, Clone, Copy)]
pub struct RateLimits {
    /// Maximum actions in this wrapper's lifetime (one session).
    pub max_per_session: usize,
    /// Maximum actions in any sliding 60-second window.
    pub max_per_minute: usize,
    /// Maximum identical consecutive actions before the wrapper
    /// refuses (runaway-loop detection).
    pub max_repeats: usize,
}

impl Default for RateLimits {
    fn default() -> Self {
        Self {
            max_per_session: 10_000,
            max_per_minute: 600,
            max_repeats: 20,
        }
    }
}

/// What an action is, for repeat detection. Text is fingerprinted by
/// length + a cheap hash — the wrapper never stores typed content.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Fingerprint {
    Screenshot,
    Click {
        x: i32,
        y: i32,
    },
    Move {
        x: i32,
        y: i32,
    },
    Type {
        len: usize,
        hash: u64,
    },
    Key(Key),
    Scroll {
        x: i32,
        y: i32,
        dx: i32,
        dy: i32,
    },
    Drag {
        from_x: i32,
        from_y: i32,
        to_x: i32,
        to_y: i32,
    },
    DoubleClick {
        x: i32,
        y: i32,
    },
    RightClick {
        x: i32,
        y: i32,
    },
    Hold {
        key: Key,
        millis: u32,
    },
}

fn fingerprint_text(text: &str) -> Fingerprint {
    // FNV-1a, content-free fingerprinting: equal texts fingerprint
    // equal; the text itself is never retained.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    Fingerprint::Type {
        len: text.len(),
        hash,
    }
}

/// A body wrapped in the body-level safety checks. See the module
/// docs for the four checks and their order.
pub struct GuardedBody<B: Body> {
    inner: B,
    limits: RateLimits,
    excluded: Vec<ScreenRect>,
    actions_in_session: usize,
    recent_actions: VecDeque<Instant>,
    last_fingerprint: Option<Fingerprint>,
    repeat_count: usize,
    stop_flag: Option<Arc<StopFlag>>,
    approval_pending: Option<Arc<AtomicBool>>,
    /// Live exclusion list shared with the UI thread (the app
    /// window rectangle, updated every frame). Consulted by
    /// `check_target` alongside the owned list.
    shared_excluded: Option<Arc<RwLock<Vec<ScreenRect>>>>,
}

impl<B: Body> GuardedBody<B> {
    pub fn new(inner: B, limits: RateLimits) -> Self {
        Self {
            inner,
            limits,
            excluded: Vec::new(),
            actions_in_session: 0,
            recent_actions: VecDeque::new(),
            last_fingerprint: None,
            repeat_count: 0,
            stop_flag: None,
            approval_pending: None,
            shared_excluded: None,
        }
    }

    /// Share a stop flag: when it trips, every action refuses with
    /// [`BodyError::Stopped`]. The same flag is tripped by a
    /// forbidden attempt while approval is pending.
    pub fn with_stop_flag(mut self, flag: Arc<StopFlag>) -> Self {
        self.stop_flag = Some(flag);
        self
    }

    /// Share the approval-pending flag: while it is set, a
    /// consequential attempt trips the stop latch (when a stop flag
    /// is also shared) and is refused.
    pub fn with_approval_pending(mut self, flag: Arc<AtomicBool>) -> Self {
        self.approval_pending = Some(flag);
        self
    }

    /// Share the live exclusion list: the UI thread writes the app
    /// window rectangle here every frame, and `check_target`
    /// refuses points inside it. A poisoned lock fails open to the
    /// owned list only (the lock is only ever written by the UI
    /// thread with a plain rectangle, so poisoning means the UI
    /// thread panicked while holding it — and then the app is
    /// already going down).
    pub fn with_shared_exclusion(mut self, rects: Arc<RwLock<Vec<ScreenRect>>>) -> Self {
        self.shared_excluded = Some(rects);
        self
    }

    /// The wrapped body, for callers that need it back.
    pub fn into_inner(self) -> B {
        self.inner
    }

    /// How many actions this wrapper has admitted so far.
    pub fn actions_in_session(&self) -> usize {
        self.actions_in_session
    }

    fn approval_is_pending(&self) -> bool {
        self.approval_pending
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::SeqCst))
    }

    fn trip_stop(&self) {
        if let Some(flag) = &self.stop_flag {
            flag.trip();
        }
    }

    /// The pre-action gate, in check order: stop, approval-pending
    /// trip, rate limits. `consequential` marks actions that change
    /// state (click/type/key/move); screenshots count toward rate
    /// limits but do not trip the stop on approval-pending (reading
    /// the screen while a card waits is not self-approval).
    fn gate(&mut self, fingerprint: Fingerprint, consequential: bool) -> Result<(), BodyError> {
        if let Some(flag) = &self.stop_flag {
            if flag.is_tripped() {
                return Err(BodyError::Stopped);
            }
        }
        if consequential && self.approval_is_pending() {
            self.trip_stop();
            return Err(BodyError::ApprovalPending);
        }
        if self.actions_in_session >= self.limits.max_per_session {
            return Err(BodyError::RateLimited {
                reason: format!(
                    "session cap reached ({} actions)",
                    self.limits.max_per_session
                ),
            });
        }
        let now = Instant::now();
        while self
            .recent_actions
            .front()
            .is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(60))
        {
            self.recent_actions.pop_front();
        }
        if self.recent_actions.len() >= self.limits.max_per_minute {
            return Err(BodyError::RateLimited {
                reason: format!(
                    "per-minute cap reached ({} actions/minute)",
                    self.limits.max_per_minute
                ),
            });
        }
        if self.last_fingerprint.as_ref() == Some(&fingerprint) {
            self.repeat_count += 1;
        } else {
            self.last_fingerprint = Some(fingerprint);
            self.repeat_count = 1;
        }
        if self.repeat_count > self.limits.max_repeats {
            return Err(BodyError::RateLimited {
                reason: format!(
                    "same action repeated {} times in a row (cap {})",
                    self.repeat_count, self.limits.max_repeats
                ),
            });
        }
        self.actions_in_session += 1;
        self.recent_actions.push_back(now);
        Ok(())
    }
}

impl<B: Body> Body for GuardedBody<B> {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn display_size(&mut self) -> Result<(u32, u32), BodyError> {
        self.inner.display_size()
    }

    fn screenshot(&mut self) -> Result<Framebuffer, BodyError> {
        self.gate(Fingerprint::Screenshot, false)?;
        self.inner.screenshot()
    }

    fn click(&mut self, x: i32, y: i32) -> Result<(), BodyError> {
        self.check_target(x, y)?;
        self.gate(Fingerprint::Click { x, y }, true)?;
        self.inner.verify_focus()?;
        self.inner.click(x, y)
    }

    fn mouse_move(&mut self, x: i32, y: i32) -> Result<(), BodyError> {
        self.check_target(x, y)?;
        self.gate(Fingerprint::Move { x, y }, true)?;
        self.inner.mouse_move(x, y)
    }

    fn type_text(&mut self, text: &str) -> Result<(), BodyError> {
        self.gate(fingerprint_text(text), true)?;
        self.inner.verify_focus()?;
        self.inner.type_text(text)
    }

    fn key(&mut self, key: Key) -> Result<(), BodyError> {
        self.gate(Fingerprint::Key(key), true)?;
        self.inner.verify_focus()?;
        self.inner.key(key)
    }

    fn scroll(&mut self, x: i32, y: i32, dx: i32, dy: i32) -> Result<(), BodyError> {
        self.check_target(x, y)?;
        self.gate(Fingerprint::Scroll { x, y, dx, dy }, true)?;
        self.inner.verify_focus()?;
        self.inner.scroll(x, y, dx, dy)
    }

    fn drag(&mut self, from_x: i32, from_y: i32, to_x: i32, to_y: i32) -> Result<(), BodyError> {
        self.check_target(from_x, from_y)?;
        self.check_target(to_x, to_y)?;
        self.gate(
            Fingerprint::Drag {
                from_x,
                from_y,
                to_x,
                to_y,
            },
            true,
        )?;
        self.inner.verify_focus()?;
        self.inner.drag(from_x, from_y, to_x, to_y)
    }

    fn double_click(&mut self, x: i32, y: i32) -> Result<(), BodyError> {
        self.check_target(x, y)?;
        self.gate(Fingerprint::DoubleClick { x, y }, true)?;
        self.inner.verify_focus()?;
        self.inner.double_click(x, y)
    }

    fn right_click(&mut self, x: i32, y: i32) -> Result<(), BodyError> {
        self.check_target(x, y)?;
        self.gate(Fingerprint::RightClick { x, y }, true)?;
        self.inner.verify_focus()?;
        self.inner.right_click(x, y)
    }

    fn hold_key(&mut self, key: Key, millis: u32) -> Result<(), BodyError> {
        self.gate(Fingerprint::Hold { key, millis }, true)?;
        self.inner.verify_focus()?;
        self.inner.hold_key(key, millis)
    }

    fn permissions(&mut self) -> Permissions {
        self.inner.permissions()
    }

    fn check_target(&self, x: i32, y: i32) -> Result<(), BodyError> {
        // Owned list: the same rule as the trait default.
        for rect in &self.excluded {
            if rect.contains(x, y) {
                return Err(BodyError::ForbiddenTarget {
                    x,
                    y,
                    reason: format!(
                        "point lies inside excluded rectangle ({}, {}) {}x{}",
                        rect.x, rect.y, rect.width, rect.height
                    ),
                });
            }
        }
        // Shared live list (the app window): refused with the
        // same typed error, naming the shared zone.
        if let Some(shared) = &self.shared_excluded {
            if let Ok(rects) = shared.read() {
                for rect in rects.iter() {
                    if rect.contains(x, y) {
                        return Err(BodyError::ForbiddenTarget {
                            x,
                            y,
                            reason: format!(
                                "point lies inside the shared exclusion zone ({}, {}) {}x{}",
                                rect.x, rect.y, rect.width, rect.height
                            ),
                        });
                    }
                }
            }
        }
        Ok(())
    }

    fn excluded_rects(&self) -> &[ScreenRect] {
        &self.excluded
    }

    fn set_excluded_rects(&mut self, rects: Vec<ScreenRect>) {
        self.inner.set_excluded_rects(rects.clone());
        self.excluded = rects;
    }

    fn active_window(&mut self) -> Option<u64> {
        self.inner.active_window()
    }

    fn expected_focus(&self) -> Option<u64> {
        self.inner.expected_focus()
    }

    fn set_expected_focus(&mut self, window: Option<u64>) {
        self.inner.set_expected_focus(window);
    }

    fn verify_focus(&mut self) -> Result<(), BodyError> {
        self.inner.verify_focus()
    }

    fn max_capture_dimension(&self) -> Option<u32> {
        self.inner.max_capture_dimension()
    }

    fn set_max_capture_dimension(&mut self, max: Option<u32>) {
        self.inner.set_max_capture_dimension(max);
    }

    fn framebuffer_pool(&self) -> Option<&FramebufferPool> {
        self.inner.framebuffer_pool()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::body::ScreenRect;
    use crate::mock::MockBody;

    fn guarded(limits: RateLimits) -> GuardedBody<MockBody> {
        GuardedBody::new(MockBody::new(), limits)
    }

    #[test]
    fn session_cap_refuses_and_counts_only_admitted_actions() {
        let mut body = guarded(RateLimits {
            max_per_session: 2,
            max_per_minute: 100,
            max_repeats: 100,
        });
        assert!(body.click(10, 10).is_ok());
        assert!(body.click(20, 20).is_ok());
        let err = body.click(30, 30).unwrap_err();
        assert_eq!(err.code(), "rate_limited");
        assert_eq!(body.actions_in_session(), 2);
        // The refused click never reached the inner body.
        assert_eq!(body.into_inner().events().len(), 2);
    }

    #[test]
    fn per_minute_cap_refuses() {
        let mut body = guarded(RateLimits {
            max_per_session: 100,
            max_per_minute: 2,
            max_repeats: 100,
        });
        assert!(body.screenshot().is_ok());
        assert!(body.screenshot().is_ok());
        let err = body.screenshot().unwrap_err();
        assert_eq!(err.code(), "rate_limited");
    }

    #[test]
    fn repeat_detection_refuses_a_runaway_loop() {
        let mut body = guarded(RateLimits {
            max_per_session: 100,
            max_per_minute: 100,
            max_repeats: 3,
        });
        for _ in 0..3 {
            assert!(body.click(10, 10).is_ok());
        }
        let err = body.click(10, 10).unwrap_err();
        assert_eq!(err.code(), "rate_limited");
        // A different action resets the repeat count.
        assert!(body.click(11, 10).is_ok());
    }

    #[test]
    fn exclusion_zone_refuses_before_the_inner_body_sees_it() {
        let mut body = guarded(RateLimits::default());
        body.set_excluded_rects(vec![ScreenRect::new(0, 0, 50, 50)]);
        let err = body.click(25, 25).unwrap_err();
        assert_eq!(err.code(), "forbidden_target");
        assert!(body.click(100, 100).is_ok());
        assert_eq!(body.into_inner().events().len(), 1);
    }

    #[test]
    fn new_actions_pass_through_exclusion_and_repeat_gates() {
        let mut body = guarded(RateLimits {
            max_per_session: 100,
            max_per_minute: 100,
            max_repeats: 2,
        });
        body.set_excluded_rects(vec![ScreenRect::new(0, 0, 50, 50)]);
        // Drag endpoints inside the zone are refused.
        assert_eq!(
            body.drag(25, 25, 100, 100).unwrap_err().code(),
            "forbidden_target"
        );
        assert_eq!(
            body.scroll(25, 25, 0, 1).unwrap_err().code(),
            "forbidden_target"
        );
        // Repeated identical scrolls trip the repeat gate.
        assert!(body.scroll(100, 100, 0, 1).is_ok());
        assert!(body.scroll(100, 100, 0, 1).is_ok());
        assert_eq!(
            body.scroll(100, 100, 0, 1).unwrap_err().code(),
            "rate_limited"
        );
        // Other new actions flow through the gate to the inner body.
        assert!(body.double_click(100, 100).is_ok());
        assert!(body.right_click(100, 100).is_ok());
        assert!(body.hold_key(Key::F5, 10).is_ok());
        assert!(body.drag(100, 100, 120, 120).is_ok());
    }

    #[test]
    fn stop_flag_blocks_everything_and_latches() {
        let flag = StopFlag::new();
        let mut body = guarded(RateLimits::default()).with_stop_flag(Arc::clone(&flag));
        assert!(body.click(10, 10).is_ok());
        flag.trip();
        assert_eq!(body.click(10, 10).unwrap_err().code(), "stopped");
        assert_eq!(body.screenshot().unwrap_err().code(), "stopped");
        assert_eq!(body.type_text("x").unwrap_err().code(), "stopped");
    }

    #[test]
    fn attempt_while_approval_pending_trips_the_stop() {
        let flag = StopFlag::new();
        let pending = Arc::new(AtomicBool::new(true));
        let mut body = guarded(RateLimits::default())
            .with_stop_flag(Arc::clone(&flag))
            .with_approval_pending(Arc::clone(&pending));
        // Screenshots do not trip: reading is not self-approval.
        assert!(body.screenshot().is_ok());
        assert!(!flag.is_tripped());
        // Input while a card waits trips the latch and is refused
        // with the typed approval-pending error (not a fake
        // forbidden rect: the refusal is about the waiting card).
        let err = body.click(100, 100).unwrap_err();
        assert_eq!(err.code(), "approval_pending");
        assert!(flag.is_tripped());
        // The latch holds even after the card clears.
        pending.store(false, Ordering::SeqCst);
        assert_eq!(body.click(100, 100).unwrap_err().code(), "stopped");
        assert!(body.into_inner().events().is_empty());
    }

    #[test]
    fn shared_exclusion_zone_is_live() {
        let shared = Arc::new(RwLock::new(Vec::new()));
        let mut body = guarded(RateLimits::default()).with_shared_exclusion(Arc::clone(&shared));
        // Empty shared list: clicks pass the guard.
        assert!(body.click(100, 100).is_ok());
        // The UI thread writes the app window rect: the same
        // point is now refused, with no guard rebuild.
        shared
            .write()
            .unwrap()
            .push(ScreenRect::new(50, 50, 200, 200));
        let err = body.click(100, 100).unwrap_err();
        assert_eq!(err.code(), "forbidden_target");
        assert!(err.message().contains("shared exclusion zone"));
        // Outside the shared rect still works.
        assert!(body.click(400, 400).is_ok());
        // The UI thread clears the list: the point works again.
        shared.write().unwrap().clear();
        assert!(body.click(100, 100).is_ok());
    }
}
