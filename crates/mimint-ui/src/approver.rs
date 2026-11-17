//! The desktop approval card as a guard [`Approver`].
//!
//! The worker thread calls [`UiApprover::approve`] from inside the
//! guard executor. The call blocks until the user answers on the
//! approval card — or until the UI side disconnects, which denies
//! (an error denies by guard policy; a disconnected UI must never
//! silently allow a consequential tool).
//!
//! [`Approver`]: mimint_core::Approver

use mimint_core::{Approver, MimintError};
use serde_json::Value;
use std::sync::mpsc::{Receiver, Sender};

/// One approval job travelling from the worker to the UI: the request
/// plus the one-shot reply channel for the decision.
pub struct ApprovalJob {
    pub tool: String,
    pub args: Value,
    pub reply: Sender<bool>,
}

/// Channel pair for approvals. The worker owns the [`UiApprover`]
/// (sender side); the UI thread owns the receiver and shows one card
/// per job, in order.
pub fn approval_channel() -> (Sender<ApprovalJob>, Receiver<ApprovalJob>) {
    std::sync::mpsc::channel()
}

/// Approver that asks the desktop UI. `approve` blocks the calling
/// (worker) thread until the user answers.
pub struct UiApprover {
    tx: Sender<ApprovalJob>,
}

impl UiApprover {
    pub fn new(tx: Sender<ApprovalJob>) -> Self {
        Self { tx }
    }
}

impl Approver for UiApprover {
    fn approve(&self, tool: &str, args: &Value) -> Result<bool, MimintError> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        let job = ApprovalJob {
            tool: tool.to_string(),
            args: args.clone(),
            reply: reply_tx,
        };
        // The UI is gone: deny by failing the approval.
        self.tx.send(job).map_err(|_| {
            MimintError::Tool("approval UI disconnected; denying the tool call".to_string())
        })?;
        reply_rx.recv().map_err(|_| {
            MimintError::Tool(
                "approval UI disconnected while waiting for a decision; denying the tool call"
                    .to_string(),
            )
        })
    }

    fn label(&self) -> &'static str {
        "desktop approval card"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::thread;

    #[test]
    fn approval_round_trips_through_channels() {
        let (tx, rx) = approval_channel();
        let approver = UiApprover::new(tx);
        let handle =
            thread::spawn(move || approver.approve("notebook_add", &json!({"title": "t"})));
        let job = rx.recv().unwrap();
        assert_eq!(job.tool, "notebook_add");
        job.reply.send(true).unwrap();
        assert!(handle.join().unwrap().unwrap());
    }

    #[test]
    fn disconnected_ui_denies_with_error() {
        let (tx, rx) = approval_channel();
        drop(rx);
        let approver = UiApprover::new(tx);
        let err = approver
            .approve("notebook_add", &json!({}))
            .expect_err("disconnected UI must fail the approval");
        assert!(err.to_string().contains("disconnected"), "{err}");
    }

    #[test]
    fn dropped_reply_denies_with_error() {
        let (tx, rx) = approval_channel();
        let approver = UiApprover::new(tx);
        let handle = thread::spawn(move || approver.approve("notebook_add", &json!({})));
        let job = rx.recv().unwrap();
        drop(job.reply);
        let err = handle
            .join()
            .unwrap()
            .expect_err("dropped reply must fail the approval");
        assert!(err.to_string().contains("disconnected"), "{err}");
    }
}
