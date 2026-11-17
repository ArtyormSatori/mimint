//! The body contract: one action API for every OS (ACI pattern).
//!
//! A body is the OS-specific half of the assistant: it sees the screen
//! and moves the pointer and keyboard. The brain never touches OS APIs
//! directly; it talks to a [`Body`]. Every body, on every platform,
//! implements this one trait, and every body passes the same
//! conformance suite (`crate::conformance`) before it ships.
//!
//! Coordinates are absolute display pixels, origin at the top-left of
//! the primary display. Screenshots are RGBA8, row-major, top row
//! first. Bodies report their permission state honestly: a body that
//! cannot see the screen says so instead of returning a blank image.

use crate::action::{action_specs, Action, ActionSpec};
use crate::error::BodyError;

/// A captured frame: RGBA8 pixels, row-major, top row first.
/// `data.len()` is always `width * height * 4`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Framebuffer {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

impl Framebuffer {
    /// Build a framebuffer, rejecting a buffer of the wrong length
    /// instead of silently truncating or padding it.
    pub fn new(width: u32, height: u32, data: Vec<u8>) -> Result<Self, BodyError> {
        let expected = width as usize * height as usize * 4;
        if data.len() != expected {
            return Err(BodyError::Backend(format!(
                "framebuffer is {} bytes, expected {expected} for {width}x{height} RGBA",
                data.len()
            )));
        }
        Ok(Self {
            width,
            height,
            data,
        })
    }

    /// True when every pixel is identical (a cheap "is this blank?"
    /// signal for conformance checks; a real screen is never uniform).
    pub fn is_uniform(&self) -> bool {
        let (chunks, remainder) = self.data.as_chunks::<4>();
        if !remainder.is_empty() {
            return false;
        }
        match chunks.first() {
            None => true,
            Some(first) => chunks.iter().all(|px| px == first),
        }
    }
}

/// Write a framebuffer as a binary PPM (P6, RGB): the mechanical
/// evidence format for screenshots. Alpha is dropped.
pub fn write_ppm(frame: &Framebuffer, path: &std::path::Path) -> std::io::Result<()> {
    use std::io::Write;
    let mut out = std::fs::File::create(path)?;
    writeln!(out, "P6\n{} {}\n255", frame.width, frame.height)?;
    let mut rgb = Vec::with_capacity((frame.width * frame.height * 3) as usize);
    for px in frame.data.as_chunks::<4>().0 {
        rgb.extend_from_slice(&[px[0], px[1], px[2]]);
    }
    out.write_all(&rgb)
}

/// State of one OS permission, as probed — never assumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionState {
    /// The OS reports the permission is granted.
    Granted,
    /// The OS reports the permission is denied or not yet granted.
    Denied,
    /// This platform has no probe for the permission (for example X11,
    /// which has no permission model); the operation is attempted and
    /// its own error is the signal.
    Unknown,
}

/// What the body may currently do, as probed from the OS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Permissions {
    /// Reading the screen (macOS Screen Recording; X11: always
    /// available when connected).
    pub screen_capture: PermissionState,
    /// Moving the pointer and typing (macOS Accessibility).
    pub input_control: PermissionState,
}

/// The result of dispatching one [`Action`] through [`Body::act`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActOutcome {
    /// A screenshot action produced a frame.
    Frame(Framebuffer),
    /// A pointer/keyboard action completed with no value.
    Done,
}

/// One action API for every OS. Implementations are thin over native
/// APIs; all policy (guards, approvals, audit) lives above this trait,
/// never inside a body.
pub trait Body: Send {
    /// Short stable name, for example `"mock"`, `"x11"`, `"macos"`.
    fn name(&self) -> &'static str;

    /// Size of the primary display in pixels.
    fn display_size(&mut self) -> Result<(u32, u32), BodyError>;

    /// Capture the full primary display.
    fn screenshot(&mut self) -> Result<Framebuffer, BodyError>;

    /// Move the pointer to (x, y) and click the primary button.
    fn click(&mut self, x: i32, y: i32) -> Result<(), BodyError>;

    /// Move the pointer to (x, y) without clicking.
    fn mouse_move(&mut self, x: i32, y: i32) -> Result<(), BodyError>;

    /// Type text as keystrokes. `\n` means Enter, `\t` means Tab.
    /// Implementations validate the whole string first: a call that
    /// fails types nothing.
    fn type_text(&mut self, text: &str) -> Result<(), BodyError>;

    /// Press and release one special key.
    fn key(&mut self, key: crate::action::Key) -> Result<(), BodyError>;

    /// Permission state, probed from the OS on each call.
    fn permissions(&mut self) -> Permissions;

    /// The action surface of this body, generated from the
    /// implementation. The default is the shared surface; bodies with
    /// a genuinely smaller surface override it — and the conformance
    /// suite checks the override is honest.
    fn actions(&self) -> Vec<ActionSpec> {
        action_specs()
    }

    /// Dispatch one [`Action`] to the matching primitive. The default
    /// implementation is exhaustive over the enum: adding a variant
    /// without handling it here is a compile error.
    fn act(&mut self, action: &Action) -> Result<ActOutcome, BodyError> {
        match action {
            Action::Screenshot => Ok(ActOutcome::Frame(self.screenshot()?)),
            Action::Click { x, y } => {
                self.click(*x, *y)?;
                Ok(ActOutcome::Done)
            }
            Action::Move { x, y } => {
                self.mouse_move(*x, *y)?;
                Ok(ActOutcome::Done)
            }
            Action::TypeText { text } => {
                self.type_text(text)?;
                Ok(ActOutcome::Done)
            }
            Action::Key { key } => {
                self.key(*key)?;
                Ok(ActOutcome::Done)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framebuffer_rejects_wrong_length() {
        assert!(Framebuffer::new(2, 2, vec![0u8; 16]).is_ok());
        assert!(Framebuffer::new(2, 2, vec![0u8; 15]).is_err());
        assert!(Framebuffer::new(2, 2, vec![0u8; 17]).is_err());
        assert!(Framebuffer::new(0, 0, vec![]).is_ok());
    }

    #[test]
    fn framebuffer_uniform_detection() {
        let flat = Framebuffer::new(2, 1, vec![9, 9, 9, 255, 9, 9, 9, 255]).unwrap();
        assert!(flat.is_uniform());
        let varied = Framebuffer::new(2, 1, vec![9, 9, 9, 255, 8, 9, 9, 255]).unwrap();
        assert!(!varied.is_uniform());
    }
}
