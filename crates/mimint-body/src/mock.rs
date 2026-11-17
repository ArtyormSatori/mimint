//! A deterministic in-memory body for conformance testing.
//!
//! `MockBody` pretends to be a display: its framebuffer is a pure
//! function of a content variant, clicks and keystrokes land in an
//! event log, and faults can be injected to prove the conformance
//! suite can detect a broken body. [`MockOracle`] shares the body's
//! state and plays the target side, so the full suite runs anywhere
//! with no OS.

use crate::action::Key;
use crate::body::{Body, Framebuffer, PermissionState, Permissions};
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
}

/// Deterministic in-memory body. Display is 800x600; the framebuffer
/// is a pure function of the content variant (see [`MockBody::pixels`]).
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
            })),
        }
    }

    /// A mock body with one injected fault.
    pub fn with_fault(fault: Fault) -> Self {
        let body = Self::new();
        lock_state(&body.state).fault = Some(fault);
        body
    }

    /// A mock body reporting the given permissions (for testing the
    /// permission-reporting check).
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

    /// The deterministic pixel function: a gradient plus one solid
    /// rectangle whose color depends on the content variant. Two
    /// variants always differ in the rectangle's pixels.
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
                let in_rect = (100..300).contains(&x) && (100..250).contains(&y);
                let (r, g, b) = if in_rect {
                    rect
                } else {
                    // Gentle gradient so the frame is never uniform.
                    ((x % 251) as u8, (y % 251) as u8, ((x + y) % 251) as u8)
                };
                data.extend_from_slice(&[r, g, b, 255]);
            }
        }
        data
    }

    fn check_bounds(x: i32, y: i32) -> Result<(), BodyError> {
        if !(0..800).contains(&x) || !(0..600).contains(&y) {
            return Err(BodyError::OutOfBounds {
                x,
                y,
                width: 800,
                height: 600,
            });
        }
        Ok(())
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
        Ok((800, 600))
    }

    fn screenshot(&mut self) -> Result<Framebuffer, BodyError> {
        let state = lock_state(&self.state);
        let variant = if state.fault == Some(Fault::FrozenFramebuffer) {
            0
        } else {
            state.variant
        };
        Framebuffer::new(800, 600, Self::pixels(800, 600, variant))
    }

    fn click(&mut self, x: i32, y: i32) -> Result<(), BodyError> {
        Self::check_bounds(x, y)?;
        let mut state = lock_state(&self.state);
        if state.fault == Some(Fault::DropInput) {
            return Ok(());
        }
        let (x, y) = match state.fault {
            Some(Fault::ClickOffset { dx, dy }) => (x + dx, y + dy),
            _ => (x, y),
        };
        state.events.push(MockEvent::Click { x, y });
        Ok(())
    }

    fn mouse_move(&mut self, x: i32, y: i32) -> Result<(), BodyError> {
        Self::check_bounds(x, y)?;
        let mut state = lock_state(&self.state);
        if state.fault == Some(Fault::DropInput) {
            return Ok(());
        }
        state.events.push(MockEvent::Move { x, y });
        Ok(())
    }

    fn type_text(&mut self, text: &str) -> Result<(), BodyError> {
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
        Some((0, 0, 800, 600))
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
}
