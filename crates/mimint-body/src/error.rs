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
    /// The target point lies inside a self-exclusion zone (the
    /// assistant's own window, or another configured forbidden
    /// rectangle). The action was refused before anything happened;
    /// when an approval card is waiting, the refusal also trips the
    /// emergency stop (see `GuardedBody`).
    ForbiddenTarget { x: i32, y: i32, reason: String },
    /// The focused window at execution time is not the window the
    /// action was aimed at (focus-stealing / TOCTOU guard). Nothing
    /// was typed or clicked. `None` on either side means "unknown".
    FocusMismatch {
        expected: Option<u64>,
        actual: Option<u64>,
    },
    /// Windows UIPI: the target window runs at a higher integrity
    /// level, so injected input would be silently dropped. No
    /// Windows body exists yet (Phase 4+); the variant exists so the
    /// failure mode has a typed name in the contract and the docs,
    /// instead of becoming a silent success.
    UipiBlocked { target: String },
    /// A rate limit refused the action (session cap, per-minute cap,
    /// or repeated-action detection). Nothing was executed.
    RateLimited { reason: String },
    /// The body was stopped (emergency stop latched). Nothing starts
    /// after a stop until the stop is explicitly cleared.
    Stopped,
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
            BodyError::ForbiddenTarget { .. } => "forbidden_target",
            BodyError::FocusMismatch { .. } => "focus_mismatch",
            BodyError::UipiBlocked { .. } => "uipi_blocked",
            BodyError::RateLimited { .. } => "rate_limited",
            BodyError::Stopped => "stopped",
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
            BodyError::ForbiddenTarget { x, y, reason } => {
                format!("target ({x}, {y}) is forbidden: {reason}")
            }
            BodyError::FocusMismatch { expected, actual } => {
                format!("focused window changed before the action (expected {expected:?}, actual {actual:?}); refusing to type or click into the wrong window")
            }
            BodyError::UipiBlocked { target } => {
                format!("cannot inject input into higher-integrity window {target} (Windows UIPI)")
            }
            BodyError::RateLimited { reason } => {
                format!("rate limit: {reason}")
            }
            BodyError::Stopped => {
                "body stopped: no action starts after an emergency stop".to_string()
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
