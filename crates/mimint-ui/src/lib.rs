//! Mimint desktop UI (Phase 3): the ball avatar + chat, wired to the
//! Phase 1/2 brain loop through the [`Provider`] trait, the guard
//! [`Approver`] trait, and the append-only audit log. The brain is
//! NOT rewritten here: a worker thread owns the tokio runtime, the
//! provider chain, and the [`Brain`], and the UI thread only renders
//! state and forwards decisions.
//!
//! Framework choice is TEMPORARY (D3): egui is the only framework
//! with full Phase 0 measurements under Xvfb; the final choice waits
//! for the spike on the real MacBookPro11,1. The UI layer is kept
//! swappable: face parameters, the chat state machine, the approval
//! flow, and the audit reader are pure Rust with no egui dependency,
//! and only [`app`] touches eframe.
//!
//! [`Provider`]: mimint_core::Provider
//! [`Approver`]: mimint_core::Approver
//! [`Brain`]: mimint_core::Brain

pub mod app;
pub mod approver;
pub mod audit_view;
pub mod chat;
pub mod face;
pub mod worker;

pub use app::MimintApp;
pub use approver::{ApprovalJob, UiApprover};
pub use audit_view::{format_time_ms, parse_audit_line, read_audit_tail, AuditRow};
pub use chat::{ApprovalRequest, ChatRole, ChatState, UiMessage};
pub use face::{FaceMachine, FaceParams, FaceState};
pub use worker::{WorkerEvent, WorkerHandle, WorkerRequest};
