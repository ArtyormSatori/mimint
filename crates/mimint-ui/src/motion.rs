//! Motion tiers + the FPS fuse (F3-12/F3-14, D-FIX3-8).
//!
//! Three tiers, highest first:
//! 1. The user's hand: `--degraded` / `MIMINT_UI_DEGRADED=1` forces
//!    the static tier, always.
//! 2. The OS reduced-motion signal at startup: on Linux, the
//!    `MIMINT_REDUCED_MOTION=1` env hook (there is no single
//!    desktop-wide flag to read); on macOS the NSWorkspace wiring
//!    is a Mac-checklist item — no hand-rolled ObjC FFI is shipped
//!    unverified.
//! 3. The FPS fuse: when the P90 frame time over a 5-second window
//!    exceeds 50ms, the fuse trips, latches for the rest of the
//!    run, and motion drops to the reduced tier with a notice. It
//!    never auto-re-enables: flapping between tiers is worse than
//!    staying reduced.
//!
//! Repaint scheduling follows the tier: full motion repaints at
//! ~15fps (`request_repaint_after(~66ms)`, the breathing animation's
//! pace); reduced motion repaints at 4fps; the static tier schedules
//! no repaints at all (the UI repaints on input and on the worker's
//! repaint signal). The approval card's countdown always forces a
//! 500ms repaint while a card is on screen, whatever the tier.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How much motion the UI may show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MotionTier {
    /// Full animation (breathing, bounce, blink).
    Full,
    /// Static face, no idle animation. The fuse lands here.
    Reduced,
    /// Static face, no scheduled repaints. The user's hand lands here.
    Static,
}

/// The FPS fuse: watches frame times, trips once, latches.
#[derive(Debug)]
pub struct FpsFuse {
    /// (frame end time, frame duration), newest last.
    samples: VecDeque<(Instant, Duration)>,
    tripped: bool,
}

/// P90 frame time above this, over a full window, trips the fuse.
pub const FUSE_P90_THRESHOLD: Duration = Duration::from_millis(50);
/// The fuse only judges a full window of samples.
pub const FUSE_WINDOW: Duration = Duration::from_secs(5);

impl FpsFuse {
    pub fn new() -> Self {
        Self {
            samples: VecDeque::new(),
            tripped: false,
        }
    }

    pub fn is_tripped(&self) -> bool {
        self.tripped
    }

    /// Record one frame, ending now.
    pub fn record_frame(&mut self, frame_time: Duration) {
        self.record_at(Instant::now(), frame_time);
    }

    /// Record one frame ending at `now` (the testable core: tests
    /// drive simulated frame times through here).
    pub fn record_at(&mut self, now: Instant, frame_time: Duration) {
        if self.tripped {
            return;
        }
        self.samples.push_back((now, frame_time));
        let Some(window_start) = now.checked_sub(FUSE_WINDOW) else {
            return;
        };
        // Judge only after a full window of time has been observed:
        // the oldest sample on record must predate the window start.
        // (Pruning happens AFTER judging, so the sample proving a
        // full window elapsed is never removed before it counts.)
        let full_window = self
            .samples
            .front()
            .is_some_and(|(t, _)| *t <= window_start);
        if full_window {
            let mut times: Vec<Duration> = self
                .samples
                .iter()
                .filter(|(t, _)| *t >= window_start)
                .map(|(_, d)| *d)
                .collect();
            if times.len() >= 10 {
                times.sort_unstable();
                let idx = ((times.len() as f64 * 0.9) as usize).min(times.len() - 1);
                if times[idx] > FUSE_P90_THRESHOLD {
                    self.tripped = true;
                }
            }
        }
        while let Some((t, _)) = self.samples.front() {
            if *t < window_start {
                self.samples.pop_front();
            } else {
                break;
            }
        }
    }
}

impl Default for FpsFuse {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolves the motion tier from the three inputs, in priority order.
#[derive(Debug)]
pub struct MotionController {
    user_degraded: bool,
    os_reduced_motion: bool,
    fuse: FpsFuse,
}

impl MotionController {
    pub fn new(user_degraded: bool, os_reduced_motion: bool) -> Self {
        Self {
            user_degraded,
            os_reduced_motion,
            fuse: FpsFuse::new(),
        }
    }

    /// The OS reduced-motion signal at startup. Linux: the
    /// `MIMINT_REDUCED_MOTION` env hook. macOS NSWorkspace wiring is
    /// a Mac-checklist item (see the module docs).
    pub fn os_reduced_motion_from_env() -> bool {
        std::env::var("MIMINT_REDUCED_MOTION").is_ok_and(|v| v == "1")
    }

    pub fn tier(&self) -> MotionTier {
        if self.user_degraded {
            MotionTier::Static
        } else if self.os_reduced_motion || self.fuse.is_tripped() {
            MotionTier::Reduced
        } else {
            MotionTier::Full
        }
    }

    /// Feed one frame time to the fuse. Returns true on the exact
    /// frame the fuse trips (for the one-time notice).
    pub fn record_frame(&mut self, frame_time: Duration) -> bool {
        let was = self.fuse.is_tripped();
        self.fuse.record_frame(frame_time);
        !was && self.fuse.is_tripped()
    }

    pub fn fuse_tripped(&self) -> bool {
        self.fuse.is_tripped()
    }

    /// Scheduled repaint interval for the current tier, or `None`
    /// when the tier schedules no repaints (static: input + worker
    /// signals only).
    pub fn repaint_interval(&self) -> Option<Duration> {
        match self.tier() {
            MotionTier::Full => Some(Duration::from_millis(66)),
            MotionTier::Reduced => Some(Duration::from_millis(250)),
            MotionTier::Static => None,
        }
    }

    /// Whether the face may animate (breathing, bounce) this frame.
    pub fn animate_face(&self) -> bool {
        self.tier() == MotionTier::Full
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drive(fuse: &mut FpsFuse, frame_ms: u64, count: usize, step_ms: u64) {
        let mut now = Instant::now();
        for _ in 0..count {
            fuse.record_at(now, Duration::from_millis(frame_ms));
            now += Duration::from_millis(step_ms);
        }
    }

    #[test]
    fn fast_frames_never_trip() {
        let mut fuse = FpsFuse::new();
        drive(&mut fuse, 16, 400, 16);
        assert!(!fuse.is_tripped());
    }

    #[test]
    fn slow_frames_trip_after_a_full_window_and_latch() {
        let mut fuse = FpsFuse::new();
        drive(&mut fuse, 80, 100, 60);
        assert!(fuse.is_tripped());
        // Fast frames afterwards do not untrip the fuse.
        drive(&mut fuse, 10, 400, 16);
        assert!(fuse.is_tripped());
    }

    #[test]
    fn short_burst_does_not_trip() {
        let mut fuse = FpsFuse::new();
        drive(&mut fuse, 200, 5, 60);
        assert!(!fuse.is_tripped());
    }

    #[test]
    fn p90_ignores_a_slow_minority() {
        let mut fuse = FpsFuse::new();
        // 5% slow, 95% fast: P90 stays fast (a minority under 10%
        // cannot move the 90th percentile).
        let mut now = Instant::now();
        for i in 0..400 {
            let ms = if i % 20 == 0 { 120 } else { 10 };
            fuse.record_at(now, Duration::from_millis(ms));
            now += Duration::from_millis(16);
        }
        assert!(!fuse.is_tripped());
    }

    #[test]
    fn tier_priority_is_user_then_os_then_fuse() {
        let full = MotionController::new(false, false);
        assert_eq!(full.tier(), MotionTier::Full);
        let os = MotionController::new(false, true);
        assert_eq!(os.tier(), MotionTier::Reduced);
        let user = MotionController::new(true, true);
        assert_eq!(user.tier(), MotionTier::Static);
        let mut fused = MotionController::new(false, false);
        let mut now = Instant::now();
        for _ in 0..100 {
            fused.fuse.record_at(now, Duration::from_millis(80));
            now += Duration::from_millis(60);
        }
        assert_eq!(fused.tier(), MotionTier::Reduced);
    }

    #[test]
    fn repaint_intervals_follow_the_tier() {
        assert_eq!(
            MotionController::new(false, false).repaint_interval(),
            Some(Duration::from_millis(66))
        );
        assert_eq!(
            MotionController::new(false, true).repaint_interval(),
            Some(Duration::from_millis(250))
        );
        assert_eq!(MotionController::new(true, false).repaint_interval(), None);
    }
}
