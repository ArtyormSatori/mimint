//! A deterministic in-memory body for conformance testing.
//!
//! `MockBody` pretends to be a display: its framebuffer is a pure
//! function of a content variant, clicks and keystrokes land in an
//! event log, and faults can be injected to prove the conformance
//! suite can detect a broken body. [`MockOracle`] shares the body's
//! state and plays the target side, so the full suite runs anywhere
//! with no OS.
//!
//! The mock also simulates the geometry contract: it can be built
//! with a scale factor and an origin offset ([`MockBody::with_geometry`]),
//! in which case clicks round-trip through the simulated OS space
//! (framebuffer → OS → framebuffer). A body that forgets the
//! transform records OS coordinates and fails the accuracy check —
//! exactly the Retina bug the contract exists to prevent.

use crate::action::{Key, MAX_HOLD_MILLIS};
use crate::body::{
    luminance_of, snapshot_crop_origin, snapshot_grid, Body, Framebuffer, FramebufferPool,
    PermissionState, Permissions, ScreenRect, TargetSnapshot, WindowIdentity,
};
use crate::conformance::TargetOracle;
use crate::error::BodyError;
use crate::forbidden::{check_forbidden_signals, check_forbidden_window, WindowSignals};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Events a target window would observe, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MockEvent {
    Click {
        x: i32,
        y: i32,
    },
    Move {
        x: i32,
        y: i32,
    },
    /// One typed character. Enter and Tab arrive as '\n' and '\t'.
    Char(char),
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

/// A fault injected into a mock body, to prove the conformance suite
/// detects exactly the broken behavior and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// Clicks land offset by (dx, dy) from the requested point.
    ClickOffset { dx: i32, dy: i32 },
    /// Screenshots never change, whatever the content variant is.
    FrozenFramebuffer,
    /// Typed text and keys are silently dropped.
    DropInput,
    /// The body disconnects mid-action: the event is recorded
    /// (the action partially landed) and the call returns
    /// [`BodyError::UnknownOutcome`]. The caller must not retry.
    DisconnectMidAction,
    /// The body never answers (blocks past any actor timeout).
    /// The actor reports `UnknownOutcome` and marks itself
    /// broken; the stuck thread is detached, never killed.
    Hang,
}

/// One scripted window on the mock display (F4-03/F4-07).
/// Windows are stored in stacking order: the LAST window in
/// the list is topmost. A window's `fill` color is what its
/// pixels look like, so snapshot tests can distinguish two
/// windows (or a moved window) by content as well as by id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MockWindow {
    pub id: u64,
    pub rect: ScreenRect,
    pub class: Option<String>,
    pub title: Option<String>,
    pub exe_basename: Option<String>,
    pub fill: (u8, u8, u8),
}

impl MockWindow {
    pub fn new(id: u64, rect: ScreenRect) -> Self {
        Self {
            id,
            rect,
            class: None,
            title: None,
            exe_basename: None,
            fill: (128, 128, 128),
        }
    }

    pub fn with_class(mut self, class: &str) -> Self {
        self.class = Some(class.to_string());
        self
    }

    pub fn with_title(mut self, title: &str) -> Self {
        self.title = Some(title.to_string());
        self
    }

    pub fn with_exe(mut self, exe_basename: &str) -> Self {
        self.exe_basename = Some(exe_basename.to_string());
        self
    }

    pub fn with_fill(mut self, fill: (u8, u8, u8)) -> Self {
        self.fill = fill;
        self
    }
}

/// Deterministic latency state (F4-13): a seeded LCG draws a
/// delay per input action, so load simulations are repeatable.
/// The draws are recorded for the bench to report.
#[derive(Debug)]
struct LatencyState {
    rng: u64,
    min_ms: u64,
    max_ms: u64,
    draws: Vec<u64>,
}

impl LatencyState {
    fn draw(&mut self) -> u64 {
        // LCG (Numerical Recipes constants); the high bits are
        // the random ones.
        self.rng = self
            .rng
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let span = self.max_ms.saturating_sub(self.min_ms) + 1;
        self.min_ms + (self.rng >> 33) % span
    }
}

/// Lock the shared mock state, recovering from poisoning (a test that
/// panicked mid-call must not wedge later tests).
fn lock_state(state: &Arc<Mutex<MockState>>) -> std::sync::MutexGuard<'_, MockState> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Debug)]
struct MockState {
    variant: u8,
    events: Vec<MockEvent>,
    fault: Option<Fault>,
    permissions: Permissions,
    width: u32,
    height: u32,
    scale: f64,
    origin: (i32, i32),
    excluded: Vec<ScreenRect>,
    focus_window: u64,
    expected_focus: Option<u64>,
    max_capture_dimension: Option<u32>,
    /// Fake active-window identity for the forbidden-window policy
    /// (P4.6.1): (class, title). `None` means undeterminable.
    active_window: Option<(Option<String>, Option<String>)>,
    /// Scripted windows for the point-target check and the
    /// re-verify snapshot (F4-03/F4-07). Empty by default:
    /// `window_at` then returns `None` (undeterminable) and the
    /// point check passes, exactly as before.
    windows: Vec<MockWindow>,
    /// Deterministic latency mode (F4-13). `None` by default.
    latency: Option<LatencyState>,
}

/// Deterministic in-memory body. Default display is 800x600 at scale
/// 1.0; the framebuffer is a pure function of the content variant
/// (see [`MockBody::pixels`]).
#[derive(Debug)]
pub struct MockBody {
    state: Arc<Mutex<MockState>>,
    pool: FramebufferPool,
}

/// The target side of a [`MockBody`]: observes what the body did.
/// Created by [`MockBody::oracle`]; shares state with the body.
#[derive(Debug)]
pub struct MockOracle {
    state: Arc<Mutex<MockState>>,
}

impl MockBody {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(MockState {
                variant: 0,
                events: Vec::new(),
                fault: None,
                permissions: Permissions {
                    screen_capture: PermissionState::Granted,
                    input_control: PermissionState::Granted,
                },
                width: 800,
                height: 600,
                scale: 1.0,
                origin: (0, 0),
                excluded: Vec::new(),
                focus_window: 1,
                expected_focus: None,
                max_capture_dimension: None,
                active_window: None,
                windows: Vec::new(),
                latency: None,
            })),
            pool: FramebufferPool::new(),
        }
    }

    /// A mock body with a display geometry: size, scale factor, and
    /// OS-space origin. Used by the `mock-scaled` conformance mode
    /// (scale 2, offset origin) to prove the suite and the body agree
    /// in framebuffer-pixel space.
    pub fn with_geometry(width: u32, height: u32, scale: f64, origin: (i32, i32)) -> Self {
        let body = Self::new();
        {
            let mut state = lock_state(&body.state);
            state.width = width;
            state.height = height;
            state.scale = scale;
            state.origin = origin;
        }
        body
    }

    /// A mock body with one injected fault.
    pub fn with_fault(fault: Fault) -> Self {
        let body = Self::new();
        lock_state(&body.state).fault = Some(fault);
        body
    }

    /// A mock body reporting the given permissions (for testing the
    /// permission-reporting check and the denial path: denied
    /// permissions make the operations fail with typed errors).
    pub fn with_permissions(permissions: Permissions) -> Self {
        let body = Self::new();
        lock_state(&body.state).permissions = permissions;
        body
    }

    /// Set the fake active-window identity the forbidden-window
    /// policy sees. Pass `None` for a field that is undeterminable.
    pub fn set_active_window(&self, class: Option<&str>, title: Option<&str>) {
        lock_state(&self.state).active_window =
            Some((class.map(str::to_string), title.map(str::to_string)));
    }

    /// Script the windows on the mock display, in stacking
    /// order (last = topmost). Drives `window_at`,
    /// `snapshot_target`, the point-target forbidden check,
    /// and the pixels screenshots show inside each window.
    pub fn set_windows(&self, windows: Vec<MockWindow>) {
        lock_state(&self.state).windows = windows;
    }

    /// Enable deterministic latency: every input action sleeps
    /// a seeded-LCG delay in `[min_ms, max_ms]`. The same seed
    /// always draws the same delays (see [`MockBody::latency_draws`]).
    pub fn set_latency(&self, seed: u64, min_ms: u64, max_ms: u64) {
        lock_state(&self.state).latency = Some(LatencyState {
            rng: seed,
            min_ms,
            max_ms,
            draws: Vec::new(),
        });
    }

    /// The latency delays drawn so far, in order.
    pub fn latency_draws(&self) -> Vec<u64> {
        lock_state(&self.state)
            .latency
            .as_ref()
            .map(|lat| lat.draws.clone())
            .unwrap_or_default()
    }

    /// The oracle sharing this body's state.
    pub fn oracle(&self) -> MockOracle {
        MockOracle {
            state: Arc::clone(&self.state),
        }
    }

    /// Events observed so far, in order.
    pub fn events(&self) -> Vec<MockEvent> {
        lock_state(&self.state).events.clone()
    }

    /// Simulate moving the keyboard focus to another window id (for
    /// focus-check tests).
    pub fn set_focus_window(&self, window: u64) {
        lock_state(&self.state).focus_window = window;
    }

    /// The deterministic pixel function: a gradient plus one solid
    /// rectangle whose color depends on the content variant. Two
    /// variants always differ in the rectangle's pixels. Variant 3
    /// is the pixel-pattern oracle pattern: four quadrants (red /
    /// green / blue / yellow), matching the test targets. The
    /// pattern variant ignores scripted windows (the C9 contract);
    /// other variants paint each scripted window's `fill` color
    /// inside its rect (topmost wins).
    ///
    /// Fills `data` (which must already be exactly
    /// `width * height * 4` bytes, e.g. from the framebuffer pool)
    /// instead of allocating: the mock captures through the same
    /// pool discipline as the real bodies.
    fn fill_pixels(data: &mut [u8], width: u32, height: u32, variant: u8, windows: &[MockWindow]) {
        for y in 0..height {
            for x in 0..width {
                let (r, g, b) = Self::pixel_color(x, y, width, height, variant, windows);
                let idx = (y as usize * width as usize + x as usize) * 4;
                data[idx..idx + 4].copy_from_slice(&[r, g, b, 255]);
            }
        }
    }

    /// One pixel of the deterministic frame, for screenshots and
    /// for the re-verify snapshot grid (same function, so the
    /// snapshot always agrees with what a screenshot would show).
    fn pixel_color(
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        variant: u8,
        windows: &[MockWindow],
    ) -> (u8, u8, u8) {
        if variant == crate::conformance::PATTERN_VARIANT {
            // Quadrants: top-left red, top-right green,
            // bottom-left blue, bottom-right yellow.
            return match (x < width / 2, y < height / 2) {
                (true, true) => (255u8, 0u8, 0u8),
                (false, true) => (0u8, 255u8, 0u8),
                (true, false) => (0u8, 0u8, 255u8),
                (false, false) => (255u8, 255u8, 0u8),
            };
        }
        if let Some(win) = windows
            .iter()
            .rev()
            .find(|w| w.rect.contains(x as i32, y as i32))
        {
            return win.fill;
        }
        // Rectangle color per variant: distinct, far apart.
        let rect = match variant % 3 {
            0 => (200u8, 40u8, 40u8),
            1 => (40u8, 200u8, 40u8),
            _ => (40u8, 40u8, 200u8),
        };
        let in_rect = (100..300).contains(&x) && (100..250).contains(&y);
        if in_rect {
            rect
        } else {
            // Gentle gradient so the frame is never uniform.
            ((x % 251) as u8, (y % 251) as u8, ((x + y) % 251) as u8)
        }
    }

    /// Fault gate for input actions, after the policy checks.
    /// `partial` is the event the action would have recorded:
    /// - `DropInput`: succeed without recording anything.
    /// - `DisconnectMidAction`: record `partial` (the action
    ///   partially landed) and return `UnknownOutcome`.
    /// - `Hang`: block past any actor timeout.
    /// - anything else (including no fault): apply the latency
    ///   draw, if latency mode is on, and let the caller record
    ///   the event. Returns `true` when the caller should
    ///   record the event and succeed.
    fn fault_gate(&self, partial: Option<MockEvent>) -> Result<bool, BodyError> {
        let fault = lock_state(&self.state).fault;
        match fault {
            Some(Fault::DropInput) => Ok(false),
            Some(Fault::DisconnectMidAction) => {
                if let Some(event) = partial {
                    lock_state(&self.state).events.push(event);
                }
                Err(BodyError::UnknownOutcome {
                    reason: "mock body disconnected mid-action (injected fault)".to_string(),
                })
            }
            Some(Fault::Hang) => {
                // Longer than any actor timeout used in tests;
                // the actor thread is detached, never joined.
                std::thread::sleep(Duration::from_secs(30));
                Ok(true)
            }
            _ => {
                self.apply_latency();
                Ok(true)
            }
        }
    }

    /// Sleep one deterministic latency draw, when latency mode
    /// is on. No-op otherwise.
    fn apply_latency(&self) {
        let mut state = lock_state(&self.state);
        if let Some(lat) = state.latency.as_mut() {
            let ms = lat.draw();
            lat.draws.push(ms);
            drop(state);
            std::thread::sleep(Duration::from_millis(ms));
        }
    }

    fn check_bounds(&self, x: i32, y: i32) -> Result<(), BodyError> {
        let state = lock_state(&self.state);
        if !(0..state.width as i32).contains(&x) || !(0..state.height as i32).contains(&y) {
            return Err(BodyError::OutOfBounds {
                x,
                y,
                width: state.width,
                height: state.height,
            });
        }
        Ok(())
    }

    fn check_input_permission(&self) -> Result<(), BodyError> {
        if lock_state(&self.state).permissions.input_control == PermissionState::Denied {
            return Err(BodyError::PermissionDenied {
                permission: "Accessibility",
                hint: "grant Accessibility to the process running Mimint, then restart it."
                    .to_string(),
            });
        }
        Ok(())
    }

    fn check_forbidden(&self) -> Result<(), BodyError> {
        let active = lock_state(&self.state).active_window.clone();
        match active {
            Some((class, title)) => check_forbidden_window(class.as_deref(), title.as_deref()),
            None => Ok(()),
        }
    }

    /// Refuse when the window UNDER THE TARGET POINT is
    /// forbidden (F4-03) — the unfocused-click attack: a
    /// forbidden window need not be focused to receive a
    /// click. Undeterminable (no scripted windows) allows.
    fn check_forbidden_at(&mut self, x: i32, y: i32) -> Result<(), BodyError> {
        if let Some(identity) = self.window_at(x, y) {
            check_forbidden_signals(
                &WindowSignals {
                    class: identity.class.as_deref(),
                    title: identity.title.as_deref(),
                    exe_basename: identity.exe_basename.as_deref(),
                },
                "target",
            )?;
        }
        Ok(())
    }

    /// Simulate the body's internal coordinate conversion:
    /// framebuffer pixels → OS space (divide by scale, subtract the
    /// origin) → back to framebuffer pixels. A correct body lands
    /// within rounding of the requested point; the conformance
    /// tolerance (2px) absorbs the rounding.
    fn round_trip(&self, x: i32, y: i32) -> (i32, i32) {
        let state = lock_state(&self.state);
        let os_x = (x - state.origin.0) as f64 / state.scale;
        let os_y = (y - state.origin.1) as f64 / state.scale;
        (
            (os_x * state.scale).round() as i32 + state.origin.0,
            (os_y * state.scale).round() as i32 + state.origin.1,
        )
    }
}

impl Default for MockBody {
    fn default() -> Self {
        Self::new()
    }
}

impl Body for MockBody {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn display_size(&mut self) -> Result<(u32, u32), BodyError> {
        let state = lock_state(&self.state);
        Ok((state.width, state.height))
    }

    fn screenshot(&mut self) -> Result<Framebuffer, BodyError> {
        if lock_state(&self.state).permissions.screen_capture == PermissionState::Denied {
            return Err(BodyError::PermissionDenied {
                permission: "Screen Recording",
                hint: "grant Screen Recording to the process running Mimint, then restart it."
                    .to_string(),
            });
        }
        let (width, height, scale, origin, max_dim, variant, windows) = {
            let state = lock_state(&self.state);
            let variant = if state.fault == Some(Fault::FrozenFramebuffer) {
                0
            } else {
                state.variant
            };
            (
                state.width,
                state.height,
                state.scale,
                state.origin,
                state.max_capture_dimension,
                variant,
                state.windows.clone(),
            )
        };
        let mut buffer = self.pool.acquire(width as usize * height as usize * 4);
        Self::fill_pixels(&mut buffer, width, height, variant, &windows);
        let frame = Framebuffer::with_geometry(width, height, buffer, scale, origin)?;
        Ok(match max_dim {
            Some(max) => frame.downscale(max),
            None => frame,
        })
    }

    fn click(&mut self, x: i32, y: i32) -> Result<(), BodyError> {
        self.check_bounds(x, y)?;
        self.check_target(x, y)?;
        self.check_input_permission()?;
        self.check_forbidden()?;
        self.check_forbidden_at(x, y)?;
        self.verify_focus()?;
        let state = lock_state(&self.state);
        let (x, y) = match state.fault {
            Some(Fault::ClickOffset { dx, dy }) => (x + dx, y + dy),
            _ => (x, y),
        };
        drop(state);
        let (x, y) = self.round_trip(x, y);
        if self.fault_gate(Some(MockEvent::Click { x, y }))? {
            lock_state(&self.state)
                .events
                .push(MockEvent::Click { x, y });
        }
        Ok(())
    }

    fn mouse_move(&mut self, x: i32, y: i32) -> Result<(), BodyError> {
        self.check_bounds(x, y)?;
        self.check_target(x, y)?;
        self.check_input_permission()?;
        self.check_forbidden()?;
        self.check_forbidden_at(x, y)?;
        if self.fault_gate(Some(MockEvent::Move { x, y }))? {
            lock_state(&self.state)
                .events
                .push(MockEvent::Move { x, y });
        }
        Ok(())
    }

    fn type_text(&mut self, text: &str) -> Result<(), BodyError> {
        self.check_input_permission()?;
        self.check_forbidden()?;
        self.verify_focus()?;
        // A half-way disconnect records the first character as
        // the partial event (something was typed, the rest is
        // unknown) — never the whole text.
        let partial = text.chars().next().map(MockEvent::Char);
        if self.fault_gate(partial)? {
            let mut state = lock_state(&self.state);
            for ch in text.chars() {
                state.events.push(MockEvent::Char(ch));
            }
        }
        Ok(())
    }

    fn key(&mut self, key: Key) -> Result<(), BodyError> {
        self.check_input_permission()?;
        self.check_forbidden()?;
        self.verify_focus()?;
        if self.fault_gate(Some(MockEvent::Key(key)))? {
            let mut state = lock_state(&self.state);
            // Mirror what a real target observes: Enter and Tab arrive
            // both as a key event and as text ('\n' / '\t').
            match key {
                Key::Enter => state.events.push(MockEvent::Char('\n')),
                Key::Tab => state.events.push(MockEvent::Char('\t')),
                _ => {}
            }
            state.events.push(MockEvent::Key(key));
        }
        Ok(())
    }

    fn scroll(&mut self, x: i32, y: i32, dx: i32, dy: i32) -> Result<(), BodyError> {
        self.check_bounds(x, y)?;
        self.check_target(x, y)?;
        self.check_input_permission()?;
        self.check_forbidden()?;
        self.check_forbidden_at(x, y)?;
        self.verify_focus()?;
        if self.fault_gate(Some(MockEvent::Scroll { x, y, dx, dy }))? {
            lock_state(&self.state)
                .events
                .push(MockEvent::Scroll { x, y, dx, dy });
        }
        Ok(())
    }

    fn drag(&mut self, from_x: i32, from_y: i32, to_x: i32, to_y: i32) -> Result<(), BodyError> {
        self.check_bounds(from_x, from_y)?;
        self.check_bounds(to_x, to_y)?;
        self.check_target(from_x, from_y)?;
        self.check_target(to_x, to_y)?;
        self.check_input_permission()?;
        self.check_forbidden()?;
        self.check_forbidden_at(from_x, from_y)?;
        self.check_forbidden_at(to_x, to_y)?;
        self.verify_focus()?;
        if self.fault_gate(Some(MockEvent::Drag {
            from_x,
            from_y,
            to_x,
            to_y,
        }))? {
            lock_state(&self.state).events.push(MockEvent::Drag {
                from_x,
                from_y,
                to_x,
                to_y,
            });
        }
        Ok(())
    }

    fn double_click(&mut self, x: i32, y: i32) -> Result<(), BodyError> {
        self.check_bounds(x, y)?;
        self.check_target(x, y)?;
        self.check_input_permission()?;
        self.check_forbidden()?;
        self.check_forbidden_at(x, y)?;
        self.verify_focus()?;
        if self.fault_gate(Some(MockEvent::DoubleClick { x, y }))? {
            lock_state(&self.state)
                .events
                .push(MockEvent::DoubleClick { x, y });
        }
        Ok(())
    }

    fn right_click(&mut self, x: i32, y: i32) -> Result<(), BodyError> {
        self.check_bounds(x, y)?;
        self.check_target(x, y)?;
        self.check_input_permission()?;
        self.check_forbidden()?;
        self.check_forbidden_at(x, y)?;
        self.verify_focus()?;
        if self.fault_gate(Some(MockEvent::RightClick { x, y }))? {
            lock_state(&self.state)
                .events
                .push(MockEvent::RightClick { x, y });
        }
        Ok(())
    }

    fn hold_key(&mut self, key: Key, millis: u32) -> Result<(), BodyError> {
        if millis == 0 || millis > MAX_HOLD_MILLIS {
            return Err(BodyError::InvalidArgument {
                reason: format!("hold duration must be 1..={MAX_HOLD_MILLIS} ms, got {millis}"),
            });
        }
        self.check_input_permission()?;
        self.check_forbidden()?;
        self.verify_focus()?;
        if self.fault_gate(Some(MockEvent::Hold { key, millis }))? {
            lock_state(&self.state)
                .events
                .push(MockEvent::Hold { key, millis });
        }
        Ok(())
    }

    fn permissions(&mut self) -> Permissions {
        lock_state(&self.state).permissions
    }

    fn excluded_rects(&self) -> &[ScreenRect] {
        // The rects live behind the mutex; returning a borrowed slice
        // is impossible. The mock overrides `check_target` instead
        // (below), and this returns the empty default. Callers that
        // need the list (the guarded suite) use bodies whose rects
        // are directly stored — `GuardedBody` keeps its own copy.
        &[]
    }

    fn check_target(&self, x: i32, y: i32) -> Result<(), BodyError> {
        for rect in lock_state(&self.state).excluded.clone() {
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
        Ok(())
    }

    fn set_excluded_rects(&mut self, rects: Vec<ScreenRect>) {
        lock_state(&self.state).excluded = rects;
    }

    fn active_window(&mut self) -> Option<u64> {
        Some(lock_state(&self.state).focus_window)
    }

    fn window_at(&mut self, x: i32, y: i32) -> Option<WindowIdentity> {
        let state = lock_state(&self.state);
        state
            .windows
            .iter()
            .rev()
            .find(|w| w.rect.contains(x, y))
            .map(|w| WindowIdentity {
                window_id: Some(w.id),
                class: w.class.clone(),
                title: w.title.clone(),
                exe_basename: w.exe_basename.clone(),
            })
    }

    fn snapshot_target(&mut self, x: i32, y: i32) -> Option<TargetSnapshot> {
        let (width, height, variant, windows) = {
            let state = lock_state(&self.state);
            (
                state.width,
                state.height,
                state.variant,
                state.windows.clone(),
            )
        };
        if x < 0 || y < 0 || x as u32 >= width || y as u32 >= height {
            return None;
        }
        let window_id = self.window_at(x, y).and_then(|id| id.window_id);
        let (ox, oy, cw, ch) = snapshot_crop_origin(x, y, width, height);
        let grid = snapshot_grid(
            |px, py| {
                let (r, g, b) =
                    Self::pixel_color(px as u32, py as u32, width, height, variant, &windows);
                luminance_of(r, g, b)
            },
            ox,
            oy,
            cw,
            ch,
        );
        Some(TargetSnapshot { window_id, grid })
    }

    fn expected_focus(&self) -> Option<u64> {
        lock_state(&self.state).expected_focus
    }

    fn set_expected_focus(&mut self, window: Option<u64>) {
        lock_state(&self.state).expected_focus = window;
    }

    fn max_capture_dimension(&self) -> Option<u32> {
        lock_state(&self.state).max_capture_dimension
    }

    fn set_max_capture_dimension(&mut self, max: Option<u32>) {
        lock_state(&self.state).max_capture_dimension = max;
    }

    fn framebuffer_pool(&self) -> Option<&FramebufferPool> {
        Some(&self.pool)
    }
}

impl MockOracle {
    /// All events observed so far, in order: the oracle's view
    /// of the input stream, for serialization tests (the
    /// stream must consist of whole actions, never interleaved
    /// events from two agents).
    pub fn events(&self) -> Vec<MockEvent> {
        lock_state(&self.state).events.clone()
    }

    /// Script the windows on the shared mock display (same
    /// stacking order as [`MockBody::set_windows`]). Lets a
    /// test change the world underneath a body that is owned
    /// by an actor thread — the TOCTOU simulation.
    pub fn set_windows(&self, windows: Vec<MockWindow>) {
        lock_state(&self.state).windows = windows;
    }
}

impl TargetOracle for MockOracle {
    fn received_clicks(&mut self) -> Vec<(i32, i32)> {
        self.state
            .lock()
            .unwrap()
            .events
            .iter()
            .filter_map(|e| match e {
                MockEvent::Click { x, y } => Some((*x, *y)),
                _ => None,
            })
            .collect()
    }

    fn received_text(&mut self) -> String {
        self.state
            .lock()
            .unwrap()
            .events
            .iter()
            .filter_map(|e| match e {
                MockEvent::Char(ch) => Some(*ch),
                _ => None,
            })
            .collect()
    }

    fn received_keys(&mut self) -> Vec<Key> {
        self.state
            .lock()
            .unwrap()
            .events
            .iter()
            .filter_map(|e| match e {
                MockEvent::Key(k) => Some(*k),
                _ => None,
            })
            .collect()
    }

    fn received_scrolls(&mut self) -> Vec<(i32, i32, i32, i32)> {
        lock_state(&self.state)
            .events
            .iter()
            .filter_map(|e| match e {
                MockEvent::Scroll { x, y, dx, dy } => Some((*x, *y, *dx, *dy)),
                _ => None,
            })
            .collect()
    }

    fn received_drags(&mut self) -> Vec<(i32, i32, i32, i32)> {
        lock_state(&self.state)
            .events
            .iter()
            .filter_map(|e| match e {
                MockEvent::Drag {
                    from_x,
                    from_y,
                    to_x,
                    to_y,
                } => Some((*from_x, *from_y, *to_x, *to_y)),
                _ => None,
            })
            .collect()
    }

    fn received_double_clicks(&mut self) -> Vec<(i32, i32)> {
        lock_state(&self.state)
            .events
            .iter()
            .filter_map(|e| match e {
                MockEvent::DoubleClick { x, y } => Some((*x, *y)),
                _ => None,
            })
            .collect()
    }

    fn received_right_clicks(&mut self) -> Vec<(i32, i32)> {
        lock_state(&self.state)
            .events
            .iter()
            .filter_map(|e| match e {
                MockEvent::RightClick { x, y } => Some((*x, *y)),
                _ => None,
            })
            .collect()
    }

    fn received_holds(&mut self) -> Vec<(Key, u32)> {
        lock_state(&self.state)
            .events
            .iter()
            .filter_map(|e| match e {
                MockEvent::Hold { key, millis } => Some((*key, *millis)),
                _ => None,
            })
            .collect()
    }

    fn target_rect(&mut self) -> Option<(i32, i32, u32, u32)> {
        let state = lock_state(&self.state);
        Some((0, 0, state.width, state.height))
    }

    fn set_content(&mut self, variant: u8) {
        lock_state(&self.state).variant = variant;
    }

    fn reset_events(&mut self) {
        lock_state(&self.state).events.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_framebuffer_differs_across_variants() {
        let mut body = MockBody::new();
        let mut oracle = body.oracle();
        let a = body.screenshot().unwrap();
        oracle.set_content(1);
        let b = body.screenshot().unwrap();
        assert_ne!(a.data, b.data);
        assert!(!a.is_uniform());
    }

    #[test]
    fn mock_click_out_of_bounds_is_typed() {
        let mut body = MockBody::new();
        let err = body.click(-1, 10).unwrap_err();
        assert_eq!(err.code(), "out_of_bounds");
        let err = body.click(800, 10).unwrap_err();
        assert_eq!(err.code(), "out_of_bounds");
        assert!(body.click(799, 599).is_ok());
        assert!(body.screenshot().is_ok(), "body still works after error");
    }

    #[test]
    fn mock_type_text_records_chars_in_order() {
        let mut body = MockBody::new();
        let mut oracle = body.oracle();
        body.type_text("aB\n\t").unwrap();
        assert_eq!(oracle.received_text(), "aB\n\t");
    }

    #[test]
    fn denied_permissions_fail_with_typed_errors_and_nothing_happens() {
        let mut body = MockBody::with_permissions(Permissions {
            screen_capture: PermissionState::Denied,
            input_control: PermissionState::Denied,
        });
        assert_eq!(body.screenshot().unwrap_err().code(), "permission_denied");
        assert_eq!(body.click(10, 10).unwrap_err().code(), "permission_denied");
        assert_eq!(body.type_text("x").unwrap_err().code(), "permission_denied");
        assert!(body.events().is_empty());
        assert!(body.screenshot().is_err(), "denial is stable, not flaky");
    }

    #[test]
    fn scaled_mock_reports_geometry_and_clicks_round_trip() {
        let mut body = MockBody::with_geometry(1600, 1200, 2.0, (100, 50));
        let frame = body.screenshot().unwrap();
        assert_eq!((frame.width, frame.height), (1600, 1200));
        assert_eq!(frame.scale_factor, 2.0);
        assert_eq!(frame.origin, (100, 50));
        body.click(400, 300).unwrap();
        let mut oracle = body.oracle();
        // Round-trip through simulated OS space lands back on the
        // requested framebuffer pixel (within 1px of rounding).
        let clicks = oracle.received_clicks();
        assert_eq!(clicks.len(), 1);
        assert!((clicks[0].0 - 400).abs() <= 1 && (clicks[0].1 - 300).abs() <= 1);
    }

    #[test]
    fn focus_mismatch_refuses_input_and_matching_focus_allows_it() {
        let mut body = MockBody::new();
        body.set_expected_focus(Some(7));
        // Focus window is 1 by default: mismatch.
        let err = body.type_text("x").unwrap_err();
        assert_eq!(err.code(), "focus_mismatch");
        assert!(body.events().is_empty());
        body.set_focus_window(7);
        assert!(body.type_text("x").is_ok());
        body.set_expected_focus(None);
        body.set_focus_window(99);
        assert!(body.type_text("y").is_ok(), "no expectation means no check");
    }

    #[test]
    fn new_actions_record_exact_events_and_respect_bounds() {
        let mut body = MockBody::new();
        let mut oracle = body.oracle();
        body.scroll(100, 100, 2, -3).unwrap();
        body.drag(100, 100, 200, 150).unwrap();
        body.double_click(300, 300).unwrap();
        body.right_click(400, 400).unwrap();
        body.hold_key(Key::F5, 150).unwrap();
        assert_eq!(oracle.received_scrolls(), vec![(100, 100, 2, -3)]);
        assert_eq!(oracle.received_drags(), vec![(100, 100, 200, 150)]);
        assert_eq!(oracle.received_double_clicks(), vec![(300, 300)]);
        assert_eq!(oracle.received_right_clicks(), vec![(400, 400)]);
        assert_eq!(oracle.received_holds(), vec![(Key::F5, 150)]);
        // Out-of-bounds endpoints are typed errors, nothing recorded.
        assert_eq!(
            body.drag(100, 100, 900, 100).unwrap_err().code(),
            "out_of_bounds"
        );
        assert_eq!(
            body.scroll(-1, 0, 0, 1).unwrap_err().code(),
            "out_of_bounds"
        );
        assert_eq!(oracle.received_drags().len(), 1);
        assert_eq!(oracle.received_scrolls().len(), 1);
    }

    #[test]
    fn hold_key_duration_is_validated_before_anything_happens() {
        let mut body = MockBody::new();
        assert_eq!(
            body.hold_key(Key::Enter, 0).unwrap_err().code(),
            "invalid_argument"
        );
        assert_eq!(
            body.hold_key(Key::Enter, MAX_HOLD_MILLIS + 1)
                .unwrap_err()
                .code(),
            "invalid_argument"
        );
        assert!(body.hold_key(Key::Enter, MAX_HOLD_MILLIS).is_ok());
        assert!(body.events().iter().any(|e| matches!(
            e,
            MockEvent::Hold {
                millis: MAX_HOLD_MILLIS,
                ..
            }
        )));
    }

    #[test]
    fn pool_reuse_is_measured_fifty_recycled_captures_one_allocation() {
        // P4.6.2 before/after, on the mock: without recycling, every
        // capture allocates; with recycling, one buffer serves all.
        let mut body = MockBody::new();
        for _ in 0..10 {
            let _frame = body.screenshot().unwrap();
        }
        let unrecycled = body.framebuffer_pool().unwrap().stats();
        assert_eq!(unrecycled.allocations, 10);
        assert_eq!(unrecycled.reuses, 0);

        let mut body = MockBody::new();
        for _ in 0..50 {
            let frame = body.screenshot().unwrap();
            frame.recycle(body.framebuffer_pool().unwrap());
        }
        let recycled = body.framebuffer_pool().unwrap().stats();
        assert_eq!(recycled.allocations, 1, "one buffer allocated");
        assert_eq!(recycled.reuses, 49, "49 captures reused it");
    }

    #[test]
    fn forbidden_active_window_refuses_input_but_not_screenshots() {
        let mut body = MockBody::new();
        body.set_active_window(Some("KeePassXC"), Some("KeePassXC"));
        for outcome in [
            body.click(10, 10),
            body.mouse_move(10, 10),
            body.type_text("x"),
            body.key(Key::Enter),
            body.scroll(10, 10, 0, 1),
            body.drag(10, 10, 20, 20),
            body.double_click(10, 10),
            body.right_click(10, 10),
            body.hold_key(Key::F5, 50),
        ] {
            assert_eq!(outcome.unwrap_err().code(), "forbidden_window");
        }
        assert!(
            body.screenshot().is_ok(),
            "the forbidden policy gates input only"
        );
        assert!(
            body.oracle().received_clicks().is_empty(),
            "no input reached the target"
        );

        body.set_active_window(Some("firefox"), Some("Inbox"));
        assert!(body.click(10, 10).is_ok());
        body.set_active_window(None, None);
        assert!(body.click(10, 10).is_ok(), "undeterminable allows");
    }

    #[test]
    fn capture_dimension_limit_downscales_at_the_source() {
        let mut body = MockBody::new();
        body.set_max_capture_dimension(Some(200));
        let frame = body.screenshot().unwrap();
        assert_eq!((frame.width, frame.height), (200, 150));
        body.set_max_capture_dimension(None);
        let frame = body.screenshot().unwrap();
        assert_eq!((frame.width, frame.height), (800, 600));
    }

    #[test]
    fn window_at_returns_the_topmost_window_at_the_point() {
        let mut body = MockBody::new();
        assert!(body.window_at(150, 150).is_none(), "no windows scripted");
        body.set_windows(vec![
            MockWindow::new(1, ScreenRect::new(100, 100, 300, 200)).with_class("firefox"),
            MockWindow::new(2, ScreenRect::new(150, 150, 100, 100)).with_class("editor"),
        ]);
        // Overlapping region: the later (topmost) window wins.
        let top = body.window_at(175, 175).unwrap();
        assert_eq!(top.window_id, Some(2));
        assert_eq!(top.class.as_deref(), Some("editor"));
        // Only the bottom window covers this point.
        let bottom = body.window_at(110, 110).unwrap();
        assert_eq!(bottom.window_id, Some(1));
        assert!(body.window_at(10, 10).is_none());
    }

    #[test]
    fn forbidden_target_window_refuses_even_when_unfocused() {
        let mut body = MockBody::new();
        // Focused window is innocent; the click target is not.
        body.set_active_window(Some("firefox"), Some("Inbox"));
        body.set_windows(vec![MockWindow::new(
            1,
            ScreenRect::new(100, 100, 200, 200),
        )
        .with_class("KeePassXC")
        .with_fill((10, 10, 10))]);
        let err = body.click(150, 150).unwrap_err();
        assert_eq!(err.code(), "forbidden_window");
        assert!(err.message().contains("target window"));
        assert!(body.events().is_empty());
        // A click outside the forbidden window still works.
        assert!(body.click(500, 500).is_ok());
        // Drags check BOTH endpoints.
        let err = body.drag(500, 500, 150, 150).unwrap_err();
        assert_eq!(err.code(), "forbidden_window");
    }

    #[test]
    fn spoofed_class_with_denylisted_exe_is_refused_at_the_point() {
        let mut body = MockBody::new();
        body.set_windows(vec![MockWindow::new(
            1,
            ScreenRect::new(100, 100, 200, 200),
        )
        .with_class("Firefox")
        .with_exe("keepassxc-probe")]);
        let err = body.click(150, 150).unwrap_err();
        assert_eq!(err.code(), "forbidden_window");
    }

    #[test]
    fn snapshot_matches_the_same_target_and_mismatches_a_swapped_one() {
        let mut body = MockBody::new();
        body.set_windows(vec![MockWindow::new(
            1,
            ScreenRect::new(100, 100, 300, 300),
        )
        .with_class("editor")
        .with_fill((200, 40, 40))]);
        let before = body.snapshot_target(200, 200).unwrap();
        assert_eq!(before.window_id, Some(1));
        // Unchanged: matches.
        let same = body.snapshot_target(200, 200).unwrap();
        assert!(before.matches(&same));
        // The window is replaced by another window with
        // different content at the same point: mismatch.
        body.set_windows(vec![MockWindow::new(
            2,
            ScreenRect::new(100, 100, 300, 300),
        )
        .with_class("browser")
        .with_fill((40, 40, 200))]);
        let after = body.snapshot_target(200, 200).unwrap();
        assert_eq!(after.window_id, Some(2));
        assert!(!before.matches(&after));
        // Same window id, changed fill: grid mismatch.
        body.set_windows(vec![MockWindow::new(
            2,
            ScreenRect::new(100, 100, 300, 300),
        )
        .with_class("browser")
        .with_fill((240, 240, 240))]);
        let changed = body.snapshot_target(200, 200).unwrap();
        assert!(!after.matches(&changed));
        // Out of bounds: no snapshot.
        assert!(body.snapshot_target(900, 900).is_none());
    }

    #[test]
    fn latency_draws_are_deterministic_for_a_seed() {
        let a = MockBody::new();
        a.set_latency(42, 1, 3);
        let b = MockBody::new();
        b.set_latency(42, 1, 3);
        let mut a = a;
        let mut b = b;
        for _ in 0..5 {
            a.click(10, 10).unwrap();
            b.click(10, 10).unwrap();
        }
        assert_eq!(a.latency_draws(), b.latency_draws());
        assert_eq!(a.latency_draws().len(), 5);
        assert!(a.latency_draws().iter().all(|d| (1..=3).contains(d)));
        // A different seed draws a different sequence.
        let c = MockBody::new();
        c.set_latency(7, 1, 3);
        let mut c = c;
        for _ in 0..5 {
            c.click(10, 10).unwrap();
        }
        assert_ne!(c.latency_draws(), a.latency_draws());
    }

    #[test]
    fn disconnect_mid_action_records_the_partial_event_and_reports_unknown() {
        let mut body = MockBody::with_fault(Fault::DisconnectMidAction);
        let err = body.click(10, 10).unwrap_err();
        assert_eq!(err.code(), "unknown_outcome");
        // The partial event IS recorded: the click may have
        // landed, and the caller must know that.
        assert_eq!(body.events(), vec![MockEvent::Click { x: 10, y: 10 }]);
        // Type: the first character is the partial event.
        let err = body.type_text("hello").unwrap_err();
        assert_eq!(err.code(), "unknown_outcome");
        assert!(body.events().contains(&MockEvent::Char('h')));
        assert!(!body.events().contains(&MockEvent::Char('e')));
    }
}
