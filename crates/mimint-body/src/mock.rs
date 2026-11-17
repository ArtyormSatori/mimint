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

use crate::action::Key;
use crate::body::{Body, Framebuffer, PermissionState, Permissions, ScreenRect};
use crate::conformance::TargetOracle;
use crate::error::BodyError;
use std::sync::{Arc, Mutex};

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
}

/// Deterministic in-memory body. Default display is 800x600 at scale
/// 1.0; the framebuffer is a pure function of the content variant
/// (see [`MockBody::pixels`]).
#[derive(Debug)]
pub struct MockBody {
    state: Arc<Mutex<MockState>>,
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
            })),
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
    /// green / blue / yellow), matching the test targets.
    fn pixels(width: u32, height: u32, variant: u8) -> Vec<u8> {
        let mut data = Vec::with_capacity(width as usize * height as usize * 4);
        // Rectangle color per variant: distinct, far apart.
        let rect = match variant % 3 {
            0 => (200u8, 40u8, 40u8),
            1 => (40u8, 200u8, 40u8),
            _ => (40u8, 40u8, 200u8),
        };
        for y in 0..height {
            for x in 0..width {
                let (r, g, b) = if variant == crate::conformance::PATTERN_VARIANT {
                    // Quadrants: top-left red, top-right green,
                    // bottom-left blue, bottom-right yellow.
                    match (x < width / 2, y < height / 2) {
                        (true, true) => (255u8, 0u8, 0u8),
                        (false, true) => (0u8, 255u8, 0u8),
                        (true, false) => (0u8, 0u8, 255u8),
                        (false, false) => (255u8, 255u8, 0u8),
                    }
                } else {
                    let in_rect = (100..300).contains(&x) && (100..250).contains(&y);
                    if in_rect {
                        rect
                    } else {
                        // Gentle gradient so the frame is never uniform.
                        ((x % 251) as u8, (y % 251) as u8, ((x + y) % 251) as u8)
                    }
                };
                data.extend_from_slice(&[r, g, b, 255]);
            }
        }
        data
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
        let (width, height, scale, origin, max_dim, variant) = {
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
            )
        };
        let frame = Framebuffer::with_geometry(
            width,
            height,
            Self::pixels(width, height, variant),
            scale,
            origin,
        )?;
        Ok(match max_dim {
            Some(max) => frame.downscale(max),
            None => frame,
        })
    }

    fn click(&mut self, x: i32, y: i32) -> Result<(), BodyError> {
        self.check_bounds(x, y)?;
        self.check_target(x, y)?;
        self.check_input_permission()?;
        self.verify_focus()?;
        let state = lock_state(&self.state);
        if state.fault == Some(Fault::DropInput) {
            return Ok(());
        }
        let (x, y) = match state.fault {
            Some(Fault::ClickOffset { dx, dy }) => (x + dx, y + dy),
            _ => (x, y),
        };
        drop(state);
        let (x, y) = self.round_trip(x, y);
        lock_state(&self.state)
            .events
            .push(MockEvent::Click { x, y });
        Ok(())
    }

    fn mouse_move(&mut self, x: i32, y: i32) -> Result<(), BodyError> {
        self.check_bounds(x, y)?;
        self.check_target(x, y)?;
        self.check_input_permission()?;
        let mut state = lock_state(&self.state);
        if state.fault == Some(Fault::DropInput) {
            return Ok(());
        }
        state.events.push(MockEvent::Move { x, y });
        Ok(())
    }

    fn type_text(&mut self, text: &str) -> Result<(), BodyError> {
        self.check_input_permission()?;
        self.verify_focus()?;
        let mut state = lock_state(&self.state);
        if state.fault == Some(Fault::DropInput) {
            return Ok(());
        }
        for ch in text.chars() {
            state.events.push(MockEvent::Char(ch));
        }
        Ok(())
    }

    fn key(&mut self, key: Key) -> Result<(), BodyError> {
        self.check_input_permission()?;
        self.verify_focus()?;
        let mut state = lock_state(&self.state);
        if state.fault == Some(Fault::DropInput) {
            return Ok(());
        }
        // Mirror what a real target observes: Enter and Tab arrive
        // both as a key event and as text ('\n' / '\t').
        match key {
            Key::Enter => state.events.push(MockEvent::Char('\n')),
            Key::Tab => state.events.push(MockEvent::Char('\t')),
            _ => {}
        }
        state.events.push(MockEvent::Key(key));
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
    fn capture_dimension_limit_downscales_at_the_source() {
        let mut body = MockBody::new();
        body.set_max_capture_dimension(Some(200));
        let frame = body.screenshot().unwrap();
        assert_eq!((frame.width, frame.height), (200, 150));
        body.set_max_capture_dimension(None);
        let frame = body.screenshot().unwrap();
        assert_eq!((frame.width, frame.height), (800, 600));
    }
}
