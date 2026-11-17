//! Typed errors for body operations. A body never panics on bad input:
//! out-of-bounds coordinates, missing permissions, and unsupported text
//! are all values the caller can match on.

use std::fmt;

/// Errors from [`crate::Body`] operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyError {
    /// Coordinates outside the display.
    OutOfBounds {
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    },
    /// The OS refused the operation: a permission is missing.
    /// `hint` tells the user what to grant, in plain words.
    PermissionDenied {
        permission: &'static str,
        hint: String,
    },
    /// The platform backend failed (connection lost, API error, ...).
    /// The message carries the backend's own words; it never carries
    /// screen content or keystrokes.
    Backend(String),
    /// This body cannot do the requested thing on this platform.
    Unsupported(&'static str),
    /// Text that cannot be typed (for example a character with no key
    /// mapping on this keyboard layout). The caller decides what to do;
    /// nothing was typed for this call.
    UntypeableText { text: String, reason: String },
}

impl BodyError {
    /// Short machine-readable code for logs and conformance reports.
    pub fn code(&self) -> &'static str {
        match self {
            BodyError::OutOfBounds { .. } => "out_of_bounds",
            BodyError::PermissionDenied { .. } => "permission_denied",
            BodyError::Backend(_) => "backend",
            BodyError::Unsupported(_) => "unsupported",
            BodyError::UntypeableText { .. } => "untypeable_text",
        }
    }

    /// Human-readable message (no secrets, no screen content).
    pub fn message(&self) -> String {
        match self {
            BodyError::OutOfBounds {
                x,
                y,
                width,
                height,
            } => {
                format!("coordinates ({x}, {y}) are outside the {width}x{height} display")
            }
            BodyError::PermissionDenied { permission, hint } => {
                format!("permission denied: {permission}. {hint}")
            }
            BodyError::Backend(msg) => format!("body backend error: {msg}"),
            BodyError::Unsupported(what) => format!("unsupported on this body: {what}"),
            BodyError::UntypeableText { text, reason } => {
                format!("cannot type text {text:?}: {reason}")
            }
        }
    }
}

impl fmt::Display for BodyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for BodyError {}
