//! Cooperative cancellation tokens (Phase 3 fix pass, D-FIX3-6).
//!
//! A [`CancelToken`] is a shared, latching boolean: once cancelled it
//! stays cancelled until the owner replaces the token. There are two
//! distinct uses, and they must not be confused:
//!
//! - The **turn token** cancels one chat turn (the UI "Hủy lượt"
//!   button). The worker creates a fresh token per turn.
//! - The **stop token** is the latching emergency stop (the UI
//!   "DỪNG NGAY" button). It stays set until the user explicitly
//!   resumes, across turns, and it blocks consequential actions at
//!   the guard step even after an approval was granted.
//!
//! Cancellation is cooperative: it is checked at defined points —
//! before each provider call in the brain loop, and immediately
//! before a guarded tool executes, in the same step as the guard
//! decision. It does NOT interrupt an HTTP request already in
//! flight; in-flight requests are bounded by the per-request
//! timeout baked into the guarded HTTP client. See
//! `docs/stop-and-approval.md` for the three stop levels and which
//! ones are implemented.

use crate::error::MimintError;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// A shared, latching cancellation flag. Clones share the flag.
#[derive(Debug, Clone, Default)]
pub struct CancelToken {
    cancelled: Arc<AtomicBool>,
}

impl CancelToken {
    /// A new, uncancelled token.
    pub fn new() -> Self {
        Self::default()
    }

    /// Cancel. Latching: there is no un-cancel; replace the token
    /// to start a fresh, uncancelled scope.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    /// True once [`CancelToken::cancel`] has been called.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Fail with [`MimintError::Cancelled`] when cancelled, so
    /// callers can use `?` at a check point.
    pub fn check(&self) -> Result<(), MimintError> {
        if self.is_cancelled() {
            Err(MimintError::Cancelled("cancelled by the user".to_string()))
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_starts_uncancelled_and_latches() {
        let token = CancelToken::new();
        assert!(!token.is_cancelled());
        assert!(token.check().is_ok());
        let shared = token.clone();
        token.cancel();
        assert!(token.is_cancelled());
        assert!(shared.is_cancelled());
        assert!(token.check().is_err());
        assert!(shared.check().unwrap_err().is_cancelled());
        // No un-cancel: a fresh token is the only reset.
        let fresh = CancelToken::new();
        assert!(!fresh.is_cancelled());
    }
}
