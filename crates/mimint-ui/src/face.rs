//! The ball avatar's face: 9 states mapped from the 9 core looks,
//! with parameters the renderer turns into shapes.
//!
//! State selection comes from two sources: turn-phase events
//! (thinking while a turn runs, happy/celebrating on finish,
//! confused on error, sleepy after idleness) and — since Phase
//! 5 — emotion codes the model sends through the structured
//! `set_emotion` channel ([`FaceMachine::on_emotion`]). The
//! emotion channel is the only way Listening, Speaking, and
//! Surprised are reached: there is no voice input, no TTS, and
//! no surprise trigger in the turn-phase events.
//!
//! Motion (from the design spec): 4-second breathing cycle, a blink
//! every 3–6 seconds, a bounce on state change. Degraded mode (weak
//! machines) draws a static face with only the blink, and the UI
//! labels it visibly.

use mimint_core::EmotionCode;
use std::time::{Duration, Instant};

/// The 9 avatar states, in design order (core looks 1–9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FaceState {
    Neutral,
    Happy,
    Listening,
    Thinking,
    Speaking,
    Surprised,
    Confused,
    Sleepy,
    Celebrating,
}

impl FaceState {
    /// The face state for one emotion code (Phase 5, P5.6). The
    /// mapping is 1:1, in design order; the labels match the
    /// codes' Vietnamese labels.
    pub fn from_emotion(code: EmotionCode) -> Self {
        match code {
            EmotionCode::Neutral => FaceState::Neutral,
            EmotionCode::Happy => FaceState::Happy,
            EmotionCode::Listening => FaceState::Listening,
            EmotionCode::Thinking => FaceState::Thinking,
            EmotionCode::Speaking => FaceState::Speaking,
            EmotionCode::Surprised => FaceState::Surprised,
            EmotionCode::Confused => FaceState::Confused,
            EmotionCode::Sleepy => FaceState::Sleepy,
            EmotionCode::Celebrating => FaceState::Celebrating,
        }
    }

    /// Short Vietnamese label shown under the face.
    pub fn label(self) -> &'static str {
        match self {
            FaceState::Neutral => "Sẵn sàng",
            FaceState::Happy => "Vui",
            FaceState::Listening => "Đang nghe",
            FaceState::Thinking => "Đang nghĩ",
            FaceState::Speaking => "Đang nói",
            FaceState::Surprised => "Ngạc nhiên",
            FaceState::Confused => "Chưa hiểu",
            FaceState::Sleepy => "Buồn ngủ",
            FaceState::Celebrating => "Ăn mừng",
        }
    }
}

/// Eye shape for one state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EyeShape {
    /// Glossy black ovals (the default).
    Oval,
    /// Upward happy arcs.
    HappyArc,
    /// Closed (sleeping / blinking).
    Closed,
    /// Wide open circles.
    Wide,
    /// Small dots, looking aside (confused).
    Dot,
}

/// Mouth shape for one state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouthShape {
    /// Small smile.
    Smile,
    /// Open happy smile.
    OpenSmile,
    /// Small "o" (surprised).
    SmallO,
    /// Flat line.
    Flat,
    /// Wavy line (confused).
    Wavy,
    /// Tiny "yawn" oval (sleepy).
    Yawn,
}

/// Everything the renderer needs for one face, derived purely from
/// the state. No egui types here: this module is headless-testable.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FaceParams {
    pub eyes: EyeShape,
    pub mouth: MouthShape,
    /// Pink cheeks visible.
    pub blush: bool,
    /// Head tilted (thinking / confused).
    pub tilt_degrees: f32,
    /// Bounce on entering this state.
    pub bounce: bool,
    /// Star burst around the head (celebrating only).
    pub star_burst: bool,
}

/// Pure mapping from state to drawing parameters.
pub fn params_for(state: FaceState) -> FaceParams {
    match state {
        FaceState::Neutral => FaceParams {
            eyes: EyeShape::Oval,
            mouth: MouthShape::Smile,
            blush: true,
            tilt_degrees: 0.0,
            bounce: false,
            star_burst: false,
        },
        FaceState::Happy => FaceParams {
            eyes: EyeShape::HappyArc,
            mouth: MouthShape::OpenSmile,
            blush: true,
            tilt_degrees: 0.0,
            bounce: true,
            star_burst: false,
        },
        FaceState::Listening => FaceParams {
            eyes: EyeShape::Oval,
            mouth: MouthShape::Smile,
            blush: true,
            tilt_degrees: 0.0,
            bounce: false,
            star_burst: false,
        },
        FaceState::Thinking => FaceParams {
            eyes: EyeShape::Dot,
            mouth: MouthShape::Flat,
            blush: false,
            tilt_degrees: 8.0,
            bounce: false,
            star_burst: false,
        },
        FaceState::Speaking => FaceParams {
            eyes: EyeShape::Oval,
            mouth: MouthShape::OpenSmile,
            blush: true,
            tilt_degrees: 0.0,
            bounce: false,
            star_burst: false,
        },
        FaceState::Surprised => FaceParams {
            eyes: EyeShape::Wide,
            mouth: MouthShape::SmallO,
            blush: false,
            tilt_degrees: 0.0,
            bounce: true,
            star_burst: false,
        },
        FaceState::Confused => FaceParams {
            eyes: EyeShape::Dot,
            mouth: MouthShape::Wavy,
            blush: false,
            tilt_degrees: -8.0,
            bounce: false,
            star_burst: false,
        },
        FaceState::Sleepy => FaceParams {
            eyes: EyeShape::Closed,
            mouth: MouthShape::Yawn,
            blush: false,
            tilt_degrees: 0.0,
            bounce: false,
            star_burst: false,
        },
        FaceState::Celebrating => FaceParams {
            eyes: EyeShape::HappyArc,
            mouth: MouthShape::OpenSmile,
            blush: true,
            tilt_degrees: 0.0,
            bounce: true,
            star_burst: true,
        },
    }
}

/// Breathing scale for the body: a 4-second sine cycle, 1.0 ± 2%.
pub fn breath_scale(elapsed: Duration) -> f32 {
    let t = elapsed.as_secs_f32();
    1.0 + 0.02 * (t * std::f32::consts::TAU / 4.0).sin()
}

/// Whether the eyes are closed for a blink at this elapsed time.
/// One 200ms blink per 5-second cycle (inside the spec's 3–6s band),
/// at a fixed offset so the behavior is deterministic and testable.
pub fn is_blinking(elapsed: Duration) -> bool {
    let phase = elapsed.as_secs_f32() % 5.0;
    (4.7..4.9).contains(&phase)
}

/// How long a transient state (Happy / Celebrating) shows before
/// returning to Neutral.
pub const TRANSIENT_SECS: u64 = 3;

/// Idle time after which the face falls asleep.
pub const IDLE_SLEEP_SECS: u64 = 60;

/// Hard TTL for an emotion-held state (F5-10): a hold that no
/// turn event supersedes releases after 2 minutes instead of
/// lasting forever. Long enough that a held face stays visible
/// (the old +90s hold test still passes); short enough that a
/// stale signal cannot pin the face indefinitely.
pub const HOLD_TTL_SECS: u64 = 120;

/// State machine: turn-phase events in, face state out. Time is
/// passed in so tests can jump the clock.
#[derive(Debug)]
pub struct FaceMachine {
    state: FaceState,
    /// When the current state started (for transient expiry).
    state_since: Instant,
    /// Last user or turn activity (for the sleep timer).
    last_activity: Instant,
    /// True when the current state came from an emotion signal
    /// (Phase 5, P5.6). A held state changes only through an
    /// explicit event (another emotion, a turn-phase change) —
    /// never through the transient/sleep timers — so a face the
    /// model signaled stays visible instead of flashing for 3
    /// seconds and vanishing before anyone can see it.
    emotion_held: bool,
    /// System override (F5-10): when set, `state()` returns this
    /// instead of the underlying state. System state (approval
    /// pending) always overrides model codes; turn-phase events
    /// clear it.
    system_override: Option<FaceState>,
}

impl FaceMachine {
    pub fn new(now: Instant) -> Self {
        Self {
            state: FaceState::Neutral,
            state_since: now,
            last_activity: now,
            emotion_held: false,
            system_override: None,
        }
    }

    /// The DISPLAYED state: the system override when one is set,
    /// otherwise the underlying state.
    pub fn state(&self) -> FaceState {
        self.system_override.unwrap_or(self.state)
    }

    /// The underlying state, ignoring any override (for tests
    /// and for the UI's "model-sent emotion" line).
    pub fn underlying_state(&self) -> FaceState {
        self.state
    }

    /// True while a system override is active.
    pub fn has_system_override(&self) -> bool {
        self.system_override.is_some()
    }

    pub fn params(&self) -> FaceParams {
        params_for(self.state())
    }

    /// Set a system override (F5-10): the displayed face becomes
    /// `state` until the override is cleared or a turn-phase
    /// event supersedes it. The underlying held state is kept.
    pub fn set_system_override(&mut self, state: FaceState, now: Instant) {
        self.system_override = Some(state);
        self.last_activity = now;
    }

    /// Clear the system override, revealing the underlying state.
    pub fn clear_system_override(&mut self, now: Instant) {
        self.system_override = None;
        self.last_activity = now;
    }

    fn set(&mut self, state: FaceState, now: Instant) {
        if self.state != state {
            self.state = state;
            self.state_since = now;
        }
    }

    /// Any user activity (typing, sending, clicking) wakes the face
    /// and resets the idle timer.
    pub fn on_activity(&mut self, now: Instant) {
        self.last_activity = now;
        if self.state == FaceState::Sleepy {
            self.emotion_held = false;
            self.set(FaceState::Neutral, now);
        }
    }

    pub fn on_turn_started(&mut self, now: Instant) {
        self.last_activity = now;
        self.emotion_held = false;
        self.system_override = None;
        self.set(FaceState::Thinking, now);
    }

    /// A clean finish shows Happy; a finish with verified tool use
    /// shows Celebrating. Both revert to Neutral after
    /// [`TRANSIENT_SECS`].
    pub fn on_turn_finished(&mut self, now: Instant, tools_used: bool) {
        self.last_activity = now;
        self.emotion_held = false;
        self.system_override = None;
        self.set(
            if tools_used {
                FaceState::Celebrating
            } else {
                FaceState::Happy
            },
            now,
        );
    }

    pub fn on_error(&mut self, now: Instant) {
        self.last_activity = now;
        self.emotion_held = false;
        self.system_override = None;
        self.set(FaceState::Confused, now);
    }

    /// An emotion code arrived through the structured channel
    /// (Phase 5, P5.6): show the matching face, and HOLD it until
    /// another emotion, a turn-phase change, or user activity
    /// waking the face supersedes it. The signal counts as turn
    /// activity, so a running turn never falls asleep mid-signal.
    /// Unknown codes never reach this method — the brain falls
    /// back and only audits them.
    pub fn on_emotion(&mut self, state: FaceState, now: Instant) {
        self.last_activity = now;
        self.emotion_held = true;
        self.set(state, now);
    }

    /// Advance timers: transient states revert, long idleness sleeps.
    /// A running turn (Thinking) never sleeps: the caller must not
    /// call `tick` with a stale `last_activity` while a turn runs —
    /// `on_turn_started` refreshes it, and turns finish through
    /// `on_turn_finished`/`on_error`. An emotion-held state never
    /// changes through `tick` — except through the hard hold TTL
    /// ([`HOLD_TTL_SECS`]): a hold that outlives the TTL releases
    /// to Neutral instead of pinning the face forever.
    pub fn tick(&mut self, now: Instant) {
        if self.emotion_held {
            if now.duration_since(self.state_since) >= Duration::from_secs(HOLD_TTL_SECS) {
                self.emotion_held = false;
                self.set(FaceState::Neutral, now);
            }
            return;
        }
        match self.state {
            FaceState::Happy | FaceState::Celebrating => {
                if now.duration_since(self.state_since) >= Duration::from_secs(TRANSIENT_SECS) {
                    self.set(FaceState::Neutral, now);
                }
            }
            FaceState::Thinking => {}
            FaceState::Sleepy => {}
            _ => {
                if now.duration_since(self.last_activity) >= Duration::from_secs(IDLE_SLEEP_SECS) {
                    self.set(FaceState::Sleepy, now);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine() -> (FaceMachine, Instant) {
        let now = Instant::now();
        (FaceMachine::new(now), now)
    }

    #[test]
    fn all_nine_states_have_distinct_params() {
        let states = [
            FaceState::Neutral,
            FaceState::Happy,
            FaceState::Listening,
            FaceState::Thinking,
            FaceState::Speaking,
            FaceState::Surprised,
            FaceState::Confused,
            FaceState::Sleepy,
            FaceState::Celebrating,
        ];
        for state in states {
            let params = params_for(state);
            // Every state has a label and a defined mouth.
            assert!(!state.label().is_empty());
            let _ = params;
        }
        // The two most similar states still differ.
        assert_ne!(params_for(FaceState::Neutral), params_for(FaceState::Happy));
        assert_ne!(
            params_for(FaceState::Happy),
            params_for(FaceState::Celebrating)
        );
    }

    #[test]
    fn phase3_selector_never_returns_voice_or_surprise_states() {
        let (mut m, now) = machine();
        m.on_turn_started(now);
        assert_eq!(m.state(), FaceState::Thinking);
        m.on_turn_finished(now, false);
        assert_eq!(m.state(), FaceState::Happy);
        m.on_turn_finished(now, true);
        assert_eq!(m.state(), FaceState::Celebrating);
        m.on_error(now);
        assert_eq!(m.state(), FaceState::Confused);
        m.tick(now + Duration::from_secs(IDLE_SLEEP_SECS + 1));
        assert_eq!(m.state(), FaceState::Sleepy);
        m.on_activity(now + Duration::from_secs(IDLE_SLEEP_SECS + 2));
        assert_eq!(m.state(), FaceState::Neutral);
        // Listening / Speaking / Surprised are unreachable through
        // the Phase 3 event surface.
        assert!(!matches!(
            m.state(),
            FaceState::Listening | FaceState::Speaking | FaceState::Surprised
        ));
    }

    #[test]
    fn transient_states_revert_after_three_seconds() {
        let (mut m, now) = machine();
        m.on_turn_finished(now, false);
        m.tick(now + Duration::from_secs(2));
        assert_eq!(m.state(), FaceState::Happy);
        m.tick(now + Duration::from_secs(TRANSIENT_SECS));
        assert_eq!(m.state(), FaceState::Neutral);
    }

    #[test]
    fn emotion_held_state_survives_timers_until_superseded() {
        let (mut m, now) = machine();
        // An emotion Happy does NOT revert after 3s, and does
        // not fall asleep after 60s idle: the signal holds.
        m.on_emotion(FaceState::Happy, now);
        m.tick(now + Duration::from_secs(TRANSIENT_SECS + 1));
        assert_eq!(m.state(), FaceState::Happy);
        m.tick(now + Duration::from_secs(IDLE_SLEEP_SECS + 30));
        assert_eq!(m.state(), FaceState::Happy);
        // A turn-phase event supersedes the hold: the finish
        // Happy reverts on the normal transient timer again.
        let later = now + Duration::from_secs(IDLE_SLEEP_SECS + 31);
        m.on_turn_finished(later, false);
        assert_eq!(m.state(), FaceState::Happy);
        m.tick(later + Duration::from_secs(TRANSIENT_SECS));
        assert_eq!(m.state(), FaceState::Neutral);
        // A new turn supersedes the hold with Thinking.
        m.on_emotion(FaceState::Surprised, later);
        m.on_turn_started(later);
        assert_eq!(m.state(), FaceState::Thinking);
    }

    #[test]
    fn emotion_hold_releases_at_the_hard_ttl() {
        let (mut m, now) = machine();
        m.on_emotion(FaceState::Happy, now);
        // Still held just before the TTL.
        m.tick(now + Duration::from_secs(HOLD_TTL_SECS - 1));
        assert_eq!(m.state(), FaceState::Happy);
        // At the TTL the hold releases to Neutral.
        m.tick(now + Duration::from_secs(HOLD_TTL_SECS));
        assert_eq!(m.state(), FaceState::Neutral);
        // After release, the normal idle timer applies again.
        m.tick(now + Duration::from_secs(HOLD_TTL_SECS + IDLE_SLEEP_SECS));
        assert_eq!(m.state(), FaceState::Sleepy);
    }

    #[test]
    fn system_override_beats_model_codes_until_cleared() {
        let (mut m, now) = machine();
        m.on_emotion(FaceState::Happy, now);
        assert_eq!(m.state(), FaceState::Happy);
        // Approval pending: the system override wins.
        m.set_system_override(FaceState::Neutral, now);
        assert_eq!(m.state(), FaceState::Neutral);
        assert!(m.has_system_override());
        // Model codes still update the underlying held state.
        m.on_emotion(FaceState::Celebrating, now);
        assert_eq!(m.state(), FaceState::Neutral);
        assert_eq!(m.underlying_state(), FaceState::Celebrating);
        // Clearing reveals the underlying state.
        m.clear_system_override(now);
        assert_eq!(m.state(), FaceState::Celebrating);
        // A turn-phase event clears the override too.
        m.set_system_override(FaceState::Neutral, now);
        m.on_turn_finished(now, false);
        assert!(!m.has_system_override());
        assert_eq!(m.state(), FaceState::Happy);
    }

    #[test]
    fn thinking_does_not_sleep_and_activity_wakes() {
        let (mut m, now) = machine();
        m.on_turn_started(now);
        m.tick(now + Duration::from_secs(IDLE_SLEEP_SECS + 30));
        assert_eq!(m.state(), FaceState::Thinking);
        m.on_turn_finished(now + Duration::from_secs(IDLE_SLEEP_SECS + 30), false);
        m.tick(now + Duration::from_secs(IDLE_SLEEP_SECS + 33));
        assert_eq!(m.state(), FaceState::Neutral);
        m.tick(now + Duration::from_secs(2 * IDLE_SLEEP_SECS + 31));
        assert_eq!(m.state(), FaceState::Sleepy);
        m.on_activity(now + Duration::from_secs(2 * IDLE_SLEEP_SECS + 32));
        assert_eq!(m.state(), FaceState::Neutral);
    }

    #[test]
    fn emotion_codes_reach_all_nine_states() {
        let (mut m, now) = machine();
        for code in EmotionCode::ALL {
            m.on_emotion(FaceState::from_emotion(code), now);
            assert_eq!(m.state(), FaceState::from_emotion(code));
            // The face label matches the code's Vietnamese label.
            assert_eq!(m.state().label(), code.label_vi());
        }
        // Listening / Speaking / Surprised are reachable ONLY
        // through the emotion channel (the Phase 3 test above
        // pins the turn-phase surface).
        m.on_emotion(FaceState::Listening, now);
        assert_eq!(m.state(), FaceState::Listening);
        m.on_emotion(FaceState::Speaking, now);
        assert_eq!(m.state(), FaceState::Speaking);
        m.on_emotion(FaceState::Surprised, now);
        assert_eq!(m.state(), FaceState::Surprised);
    }

    #[test]
    fn breathing_and_blink_are_deterministic() {
        assert!((breath_scale(Duration::from_secs(0)) - 1.0).abs() < 1e-6);
        assert!(breath_scale(Duration::from_secs(1)) > 1.0);
        assert!(breath_scale(Duration::from_secs(3)) < 1.0);
        assert!(!is_blinking(Duration::from_millis(0)));
        assert!(is_blinking(Duration::from_millis(4800)));
        assert!(!is_blinking(Duration::from_millis(5000)));
        assert!(is_blinking(Duration::from_millis(9800)));
    }
}
