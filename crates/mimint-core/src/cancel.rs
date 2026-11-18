//! Cooperative cancellation tokens (Phase 3 fix pass, D-FIX3-6;
//! async waiters and cancel context added in Phase 6, §10).
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
//! decision. Phase 6 adds level-3 cancellation (§10):
//!
//! - **Provider calls** are dropped by racing the chat future
//!   against [`CancelToken::wait_cancelled`] in a `tokio::select!`.
//!   Dropping the future drops the in-flight request (measured at
//!   P6.4; see `docs/stop-and-approval.md` for what was measured
//!   and what was not).
//! - **Child processes** (sandboxed code, MCP servers) are killed
//!   as a process group; the kills are recorded in a
//!   [`KillLedger`] so the audit trail can say what was killed.
//! - **Agent tasks** are aborted and joined with a 5s bound by the
//!   coordinator; past the bound the end is unverified
//!   (`task_end_unverified`) and the coordinator degrades.
//!
//! The body actor thread can NEVER be killed (§10.1): it is a
//! plain thread, and Rust has no safe way to stop one. A stuck
//! actor thread outlives every cancel; only a restart clears it.
//! That limit is documented, never hidden.

use crate::error::MimintError;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Instant;

/// Shared state behind a [`CancelToken`]. Clones of a token share
/// one inner state.
#[derive(Debug, Default)]
struct CancelInner {
    cancelled: AtomicBool,
    notify: tokio::sync::Notify,
    /// When the first cancel landed, for cancel-latency audit
    /// (§10.2). First-cancel-wins.
    cancelled_at: Mutex<Option<Instant>>,
    /// Why the cancel was requested (`ui_stop`, `turn_cancel`,
    /// `timeout`, `queue_sla`, `parent`, `coordinator`,
    /// `budget_exhausted` — §10.2). First-cancel-wins; `None`
    /// when cancelled through plain [`CancelToken::cancel`].
    source: Mutex<Option<String>>,
}

/// A shared, latching cancellation flag. Clones share the flag.
#[derive(Debug, Clone, Default)]
pub struct CancelToken {
    inner: Arc<CancelInner>,
}

impl CancelToken {
    /// A new, uncancelled token.
    pub fn new() -> Self {
        Self::default()
    }

    /// Cancel. Latching: there is no un-cancel; replace the token
    /// to start a fresh, uncancelled scope.
    ///
    /// Sets the flag BEFORE waking waiters, so a waiter that
    /// registers after the flag is set can never miss the wakeup
    /// (it re-checks the flag after registering).
    pub fn cancel(&self) {
        self.cancel_with_source_internal(None);
    }

    /// Cancel, recording the §10.2 source string. The first
    /// cancel wins for both the timestamp and the source.
    pub fn cancel_with_source(&self, source: &str) {
        self.cancel_with_source_internal(Some(source.to_string()));
    }

    fn cancel_with_source_internal(&self, source: Option<String>) {
        // Flag first, then wake: see the waiter loop in
        // `wait_cancelled` for why the order matters.
        let already = self.inner.cancelled.swap(true, Ordering::SeqCst);
        if !already {
            if let Ok(mut slot) = self.inner.cancelled_at.lock() {
                if slot.is_none() {
                    *slot = Some(Instant::now());
                }
            }
            if let Some(source) = source {
                if let Ok(mut slot) = self.inner.source.lock() {
                    if slot.is_none() {
                        *slot = Some(source);
                    }
                }
            }
        }
        self.inner.notify.notify_waiters();
    }

    /// True once [`CancelToken::cancel`] has been called.
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    /// When the first cancel landed, if any.
    pub fn cancelled_at(&self) -> Option<Instant> {
        self.inner.cancelled_at.lock().ok().and_then(|slot| *slot)
    }

    /// The §10.2 source recorded by the first cancel, if any.
    pub fn cancel_source(&self) -> Option<String> {
        self.inner.source.lock().ok().and_then(|slot| slot.clone())
    }

    /// A future that resolves when this token is cancelled. It
    /// resolves immediately when the token is already cancelled.
    /// No polling: waiters sleep on a [`tokio::sync::Notify`].
    pub fn wait_cancelled(&self) -> impl Future<Output = ()> + Send + 'static {
        let inner = Arc::clone(&self.inner);
        async move {
            loop {
                if inner.cancelled.load(Ordering::SeqCst) {
                    return;
                }
                let notified = inner.notify.notified();
                tokio::pin!(notified);
                // Register the waiter BEFORE re-checking the flag:
                // a cancel that lands after this point wakes us
                // through `notify_waiters`, and a cancel that
                // landed before it is visible in the flag.
                if notified.as_mut().enable() {
                    return;
                }
                if inner.cancelled.load(Ordering::SeqCst) {
                    return;
                }
                notified.await;
            }
        }
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

/// A future that resolves when ANY of the given tokens is
/// cancelled. An empty token list never resolves.
pub struct WaitAnyCancelled {
    futures: Vec<Pin<Box<dyn Future<Output = ()> + Send>>>,
    tokens: Vec<CancelToken>,
}

impl WaitAnyCancelled {
    /// Watch all given tokens. At least one token is expected;
    /// an empty list yields a future that never resolves.
    pub fn new(tokens: &[CancelToken]) -> Self {
        Self {
            futures: tokens
                .iter()
                .map(|t| Box::pin(t.wait_cancelled()) as _)
                .collect(),
            tokens: tokens.to_vec(),
        }
    }
}

impl Future for WaitAnyCancelled {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        // Fast path: a token cancelled before we were polled.
        if self.tokens.iter().any(|t| t.is_cancelled()) {
            return Poll::Ready(());
        }
        for fut in self.futures.iter_mut() {
            if fut.as_mut().poll(cx).is_ready() {
                return Poll::Ready(());
            }
        }
        // Re-check after polling: a cancel that landed while we
        // polled the later futures must not be missed.
        if self.tokens.iter().any(|t| t.is_cancelled()) {
            return Poll::Ready(());
        }
        Poll::Pending
    }
}

/// One recorded child-process kill (§10.2 `killed` context).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KillEntry {
    /// Process id that was signalled.
    pub pid: u32,
    /// How it was killed: `term` (SIGTERM sufficed), `kill`
    /// (SIGKILL was needed), or `already_exited`.
    pub method: &'static str,
}

/// Shared record of child processes killed for one turn, so the
/// audit trail can name what the cancel actually killed. Tools
/// record their kills here; the brain reads it when writing the
/// `turn_end` / `cancelled` record (§10.2).
#[derive(Debug, Clone, Default)]
pub struct KillLedger {
    entries: Arc<Mutex<Vec<KillEntry>>>,
}

impl KillLedger {
    /// An empty ledger.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a kill. Order of recording is preserved.
    pub fn record(&self, pid: u32, method: &'static str) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.push(KillEntry { pid, method });
        }
    }

    /// A snapshot of all recorded kills, in recording order.
    pub fn entries(&self) -> Vec<KillEntry> {
        self.entries.lock().map(|e| e.clone()).unwrap_or_default()
    }
}

/// What the coordinator/worker knows about a cancel request, shared
/// with the brain so the `turn_end` record (§10.2) can carry the
/// full cancel context: when it was requested, whether the cancel
/// was pushed into the actor/approver (F6D-02), and the source.
#[derive(Debug, Default)]
pub struct CancelReport {
    requested_at: Mutex<Option<Instant>>,
    push_cancelled: AtomicBool,
    source: Mutex<Option<String>>,
}

impl CancelReport {
    /// A fresh report: no cancel requested yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark a cancel request with its §10.2 source. First
    /// request wins.
    pub fn mark_requested(&self, source: &str) {
        if let Ok(mut slot) = self.requested_at.lock() {
            if slot.is_none() {
                *slot = Some(Instant::now());
            }
        }
        if let Ok(mut slot) = self.source.lock() {
            if slot.is_none() {
                *slot = Some(source.to_string());
            }
        }
    }

    /// Mark that the cancel was pushed into the BodyActor and the
    /// approver (F6D-02), so in-flight waits return
    /// `Err(Cancelled)` at once instead of polling.
    pub fn mark_push_cancelled(&self) {
        self.push_cancelled.store(true, Ordering::SeqCst);
    }

    /// True when the cancel was pushed into the actor/approver.
    pub fn push_cancelled(&self) -> bool {
        self.push_cancelled.load(Ordering::SeqCst)
    }

    /// The §10.2 source of the cancel request, if any.
    pub fn source(&self) -> Option<String> {
        self.source.lock().ok().and_then(|slot| slot.clone())
    }

    /// Milliseconds from the cancel request to now, if a request
    /// was recorded.
    pub fn latency_ms(&self) -> Option<u128> {
        self.requested_at
            .lock()
            .ok()
            .and_then(|slot| slot.map(|at| at.elapsed().as_millis()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

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

    #[test]
    fn cancel_records_timestamp_and_source_first_wins() {
        let token = CancelToken::new();
        assert!(token.cancelled_at().is_none());
        assert!(token.cancel_source().is_none());
        token.cancel_with_source("ui_stop");
        assert!(token.cancelled_at().is_some());
        assert_eq!(token.cancel_source().as_deref(), Some("ui_stop"));
        token.cancel_with_source("turn_cancel");
        assert_eq!(token.cancel_source().as_deref(), Some("ui_stop"));
    }

    #[tokio::test]
    async fn wait_cancelled_resolves_when_cancelled() {
        let token = CancelToken::new();
        let shared = token.clone();
        let waiter = tokio::spawn(async move { shared.wait_cancelled().await });
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(!waiter.is_finished());
        token.cancel();
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("waiter must resolve promptly after cancel")
            .expect("waiter task must not panic");
    }

    #[tokio::test]
    async fn wait_cancelled_resolves_immediately_when_already_cancelled() {
        let token = CancelToken::new();
        token.cancel();
        tokio::time::timeout(Duration::from_millis(100), token.wait_cancelled())
            .await
            .expect("already-cancelled waiter must resolve at once");
    }

    #[tokio::test]
    async fn wait_any_resolves_on_first_token() {
        let a = CancelToken::new();
        let b = CancelToken::new();
        let waiter = tokio::spawn(WaitAnyCancelled::new(&[a.clone(), b.clone()]));
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(!waiter.is_finished());
        b.cancel();
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("wait-any must resolve when any token cancels")
            .expect("waiter task must not panic");
    }

    #[test]
    fn kill_ledger_records_in_order() {
        let ledger = KillLedger::new();
        ledger.record(12, "term");
        ledger.record(34, "kill");
        assert_eq!(
            ledger.entries(),
            vec![
                KillEntry {
                    pid: 12,
                    method: "term"
                },
                KillEntry {
                    pid: 34,
                    method: "kill"
                },
            ]
        );
    }

    #[test]
    fn cancel_report_tracks_push_and_latency() {
        let report = CancelReport::new();
        assert!(!report.push_cancelled());
        assert!(report.latency_ms().is_none());
        report.mark_requested("turn_cancel");
        report.mark_push_cancelled();
        assert!(report.push_cancelled());
        assert_eq!(report.source().as_deref(), Some("turn_cancel"));
        assert!(report.latency_ms().is_some());
    }
}
