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
    /// level, so injected input would be silently dropped. The
    /// Phase 4 `WindowsBody` probes the target's integrity level
    /// before input and returns this instead of a silent success;
    /// no Windows run has verified the probe yet (see
    /// `windows-verify/`).
    UipiBlocked { target: String },
    /// A rate limit refused the action (session cap, per-minute cap,
    /// or repeated-action detection). Nothing was executed.
    RateLimited { reason: String },
    /// The body was stopped (emergency stop latched). Nothing starts
    /// after a stop until the stop is explicitly cleared.
    Stopped,
    /// The platform session behind the body expired (for example a
    /// Wayland portal session closed by the compositor). The caller
    /// must restore or re-create the session before acting again;
    /// retrying the same action against the dead session cannot
    /// succeed.
    SessionExpired { reason: String },
    /// The action's arguments are invalid for this body (for
    /// example a key-hold duration outside the supported range).
    /// Nothing was executed.
    InvalidArgument { reason: String },
    /// The active window is on the forbidden list (password
    /// manager, authentication dialog, banking app, OS permission
    /// prompt, payment flow — see `forbidden.rs`). Unlike
    /// [`BodyError::ForbiddenTarget`], this refusal is about the
    /// window, not a point, so it carries no coordinates. Nothing
    /// was executed.
    ForbiddenWindow { reason: String },
    /// An approval card is waiting for the user's decision, so no
    /// input action may run: input now could click the card
    /// itself. The refusal also trips the emergency stop (see
    /// `GuardedBody`). Nothing was executed.
    ApprovalPending,
    /// The action may or may not have executed: the body
    /// disconnected, timed out, or panicked mid-action, so the
    /// outcome is unknowable. The caller MUST NOT retry
    /// automatically (a retry could execute the action twice)
    /// and MUST surface the uncertainty to the user. See
    /// `BodyActor` (timeout/disconnect/panic) and the Android
    /// body's transport rule (input methods never retry).
    UnknownOutcome { reason: String },
    /// The target changed between the approval snapshot and
    /// execution: the window under the point, or the pixels
    /// around it, no longer match what the user approved
    /// (see `TargetSnapshot`). Nothing was executed; the user
    /// must re-approve against the current screen.
    TargetChanged { reason: String },
    /// Another action ran after the approval snapshot was
    /// taken (F6-03): the body mutation generation moved from
    /// `snapshot_seq` to `current_seq`, so the approval no
    /// longer binds the world the action would touch — even
    /// when the pixels under the point look unchanged.
    /// Nothing was executed; the user must re-approve
    /// against the current screen. This is a recoverable
    /// refusal, NOT a body fault: it never trips the
    /// fail-all path.
    TargetChangedSinceApproval { snapshot_seq: u64, current_seq: u64 },
    /// The observation the action was planned from is older
    /// than the allowed age: the screen may have changed since
    /// the screenshot the model saw. Nothing was executed; the
    /// caller must take a fresh screenshot first.
    StaleObservation { age_secs: u64, max_secs: u64 },
    /// The actor's queue was full when this request reached
    /// the front of the actor loop (Phase 6, §9): the action
    /// was denied without executing, and the actor is NOT
    /// broken. The caller may retry after the queue drains.
    QueueFull {
        agent_id: String,
        queued: usize,
        max: usize,
    },
    /// The request waited in the actor's queue past the queue
    /// SLA (Phase 6, §9): it was denied without executing,
    /// and the actor is NOT broken. Never runs late.
    QueueExpired {
        agent_id: String,
        queued_ms: u64,
        sla_secs: u64,
    },
    /// The owning agent was cancelled while the request was
    /// queued or waiting for its reply (Phase 6, §10). The
    /// action never started, or its outcome was discarded;
    /// the actor is NOT broken.
    Cancelled { agent_id: String },
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
            BodyError::SessionExpired { .. } => "session_expired",
            BodyError::InvalidArgument { .. } => "invalid_argument",
            BodyError::ForbiddenWindow { .. } => "forbidden_window",
            BodyError::ApprovalPending => "approval_pending",
            BodyError::UnknownOutcome { .. } => "unknown_outcome",
            BodyError::TargetChanged { .. } => "target_changed",
            BodyError::TargetChangedSinceApproval { .. } => "target_changed_since_approval",
            BodyError::StaleObservation { .. } => "stale_observation",
            BodyError::QueueFull { .. } => "body_queue_full",
            BodyError::QueueExpired { .. } => "queue_sla_expired",
            BodyError::Cancelled { .. } => "cancelled",
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
            BodyError::SessionExpired { reason } => {
                format!("body session expired: {reason}")
            }
            BodyError::InvalidArgument { reason } => {
                format!("invalid action argument: {reason}")
            }
            BodyError::ForbiddenWindow { reason } => {
                format!("active window is forbidden: {reason}")
            }
            BodyError::ApprovalPending => {
                "an approval card is waiting: refusing input until the user decides".to_string()
            }
            BodyError::UnknownOutcome { reason } => {
                format!("action outcome unknown (not auto-retried): {reason}")
            }
            BodyError::TargetChanged { reason } => {
                format!(
                    "target changed since approval, refusing to act on a stale approval: {reason}"
                )
            }
            BodyError::TargetChangedSinceApproval {
                snapshot_seq,
                current_seq,
            } => {
                format!(
                    "target changed since approval: another action ran after the approval (mutation {snapshot_seq} -> {current_seq}); re-approval is required"
                )
            }
            BodyError::StaleObservation { age_secs, max_secs } => {
                format!("observation is {age_secs}s old (limit {max_secs}s): take a fresh screenshot first")
            }
            BodyError::QueueFull {
                agent_id,
                queued,
                max,
            } => {
                format!("body queue is full for {agent_id} ({queued} queued, max {max}): the action was denied without executing")
            }
            BodyError::QueueExpired {
                agent_id,
                queued_ms,
                sla_secs,
            } => {
                format!("body request for {agent_id} waited {queued_ms}ms in the queue (SLA {sla_secs}s): denied without executing, never runs late")
            }
            BodyError::Cancelled { agent_id } => {
                format!("body request for {agent_id} was cancelled before it completed")
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
