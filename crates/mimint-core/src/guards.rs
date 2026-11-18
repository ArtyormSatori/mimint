//! Guards: deny-by-default policy decision point (P0-17).
//!
//! - Unknown tools (not in the registry allow-list) are denied.
//! - Consequential tools require an approver; no approver, an approver
//!   error, or a refusal all deny. There is no implicit allow.
//! - Every allow/deny decision is written to the audit log, and every
//!   execution (success or failure) is recorded as well.
//! - Approval lifecycle (Phase 3 fix pass, D-FIX3-5/D-FIX3-6): every
//!   request carries a `request_id` binding it to exactly one
//!   decision; a decision for another id is denied. Timeout,
//!   cancellation, and refusal are distinct audit outcomes
//!   (`timeout` / `cancelled` / `deny`), never conflated. A
//!   cancellation checked immediately before execution — in the
//!   same step as the guard decision — blocks even an approved tool.

use crate::audit::AuditLog;
use crate::cancel::CancelToken;
use crate::error::MimintError;
use crate::tools::{Registry, Tool};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};

/// Consequence level of a tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Risk {
    /// Reads only; no state change, no external effect.
    ReadOnly,
    /// Changes state or has an external effect; requires approval.
    Consequential,
}

/// One approval request: the tool, its exact arguments, and the id
/// binding this request to its decision. The arguments shown to the
/// user and the arguments executed are this same value — never
/// reconstructed from a string.
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalRequest {
    /// Monotonic within one executor; unique per request.
    pub id: u64,
    pub tool: String,
    pub args: Value,
}

/// The outcome of one approval request. Timeouts and cancellations
/// are distinct from refusals: the audit log and the UI must be able
/// to tell "the user said no" from "nobody answered" from "the user
/// stopped the turn".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    /// The user allowed this exact request.
    Allow,
    /// The user refused this request.
    Deny,
    /// The request was cancelled (emergency stop / turn cancel
    /// while the card was waiting).
    Cancelled,
    /// Nobody answered before the approver's timeout.
    Timeout,
}

/// A decision bound to its request by id. The executor denies any
/// outcome whose `request_id` does not match the request it asked
/// about (a stale or mismatched decision must never allow a
/// different action).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApprovalOutcome {
    pub request_id: u64,
    pub decision: ApprovalDecision,
}

impl ApprovalOutcome {
    pub fn allow(request_id: u64) -> Self {
        Self {
            request_id,
            decision: ApprovalDecision::Allow,
        }
    }

    pub fn deny(request_id: u64) -> Self {
        Self {
            request_id,
            decision: ApprovalDecision::Deny,
        }
    }

    pub fn cancelled(request_id: u64) -> Self {
        Self {
            request_id,
            decision: ApprovalDecision::Cancelled,
        }
    }

    pub fn timeout(request_id: u64) -> Self {
        Self {
            request_id,
            decision: ApprovalDecision::Timeout,
        }
    }
}

/// Decides whether one consequential tool call may run.
pub trait Approver {
    /// Return `Ok(true)` to allow, `Ok(false)` to deny.
    /// An error denies (deny-by-default).
    fn approve(&self, tool: &str, args: &Value) -> Result<bool, MimintError>;

    /// Decide one [`ApprovalRequest`], binding the decision to the
    /// request id. The default implementation delegates to
    /// [`Approver::approve`] and echoes the id; approvers with a
    /// timeout or a cancellation channel (the desktop card) override
    /// this to return [`ApprovalDecision::Timeout`] /
    /// [`ApprovalDecision::Cancelled`] distinctly.
    fn approve_request(&self, request: &ApprovalRequest) -> Result<ApprovalOutcome, MimintError> {
        let allowed = self.approve(&request.tool, &request.args)?;
        Ok(if allowed {
            ApprovalOutcome::allow(request.id)
        } else {
            ApprovalOutcome::deny(request.id)
        })
    }

    /// Short label recorded in the audit log for approvals from this approver.
    fn label(&self) -> &'static str {
        "manual approval"
    }
}

/// Executes tools only through the guard policy.
pub struct GuardedExecutor<'a> {
    registry: &'a Registry,
    approver: Option<&'a dyn Approver>,
    audit: &'a AuditLog,
    turn_id: String,
    next_request_id: AtomicU64,
    cancel_tokens: Vec<CancelToken>,
    /// Active profile name (Phase 5, P5.5), tagged onto every
    /// audit record this executor writes.
    profile: Option<String>,
}

impl<'a> GuardedExecutor<'a> {
    /// New executor borrowing the registry, an optional approver, and the audit log.
    pub fn new(
        registry: &'a Registry,
        approver: Option<&'a dyn Approver>,
        audit: &'a AuditLog,
        turn_id: impl Into<String>,
    ) -> Self {
        Self {
            registry,
            approver,
            audit,
            turn_id: turn_id.into(),
            next_request_id: AtomicU64::new(1),
            cancel_tokens: Vec::new(),
            profile: None,
        }
    }

    /// Set the active profile name (Phase 5, P5.5). The name is
    /// tagged onto every audit record this executor writes.
    pub fn set_profile(&mut self, name: impl Into<String>) {
        self.profile = Some(name.into());
    }

    /// Audit helper with the same shape as [`AuditLog::log`], but
    /// tagging the record's detail object with the active profile
    /// name when one is set.
    fn log(
        &self,
        turn_id: &str,
        kind: &str,
        tool: Option<&str>,
        result: Option<&str>,
        detail: Value,
    ) -> Result<(), MimintError> {
        let mut detail = detail;
        if let Some(profile) = &self.profile {
            if let Value::Object(map) = &mut detail {
                map.insert("profile".to_string(), Value::String(profile.clone()));
            }
        }
        self.audit.log(turn_id, kind, tool, result, detail)
    }

    /// Add a cancellation token checked immediately before any tool
    /// executes (turn token and/or the latching stop token). The
    /// check runs in the same step as the guard decision, after the
    /// approval: an approval granted before the user pressed STOP
    /// does not let the tool run.
    pub fn add_cancel_token(&mut self, token: CancelToken) {
        self.cancel_tokens.push(token);
    }

    fn check_cancelled(&self) -> Result<(), MimintError> {
        for token in &self.cancel_tokens {
            token.check()?;
        }
        Ok(())
    }

    /// Run one tool call through the guard policy.
    ///
    /// Denials return an error and never execute the tool. Audit write
    /// failures also fail closed: nothing runs unless its decision is
    /// recorded.
    pub fn execute(&self, tool_name: &str, args: &Value) -> Result<Value, MimintError> {
        let Some(tool) = self.registry.get(tool_name) else {
            self.log(
                &self.turn_id,
                "decision",
                Some(tool_name),
                Some("deny"),
                json!({"reason": "unknown_tool", "args": args}),
            )?;
            return Err(MimintError::Guard(format!("unknown tool: {tool_name}")));
        };
        match tool.risk() {
            Risk::ReadOnly => {
                // Read-only tools need no approval, but they still
                // get a request id: the execution entry binds to
                // the decision entry by id, uniformly, so the
                // audit trail can pair them without guessing.
                let request_id = self.next_request_id.fetch_add(1, Ordering::SeqCst);
                if let Err(err) = self.check_cancelled() {
                    self.log(
                        &self.turn_id,
                        "decision",
                        Some(tool_name),
                        Some("cancelled"),
                        json!({"reason": "cancelled_before_execution", "request_id": request_id, "args": args}),
                    )?;
                    return Err(err);
                }
                self.log(
                    &self.turn_id,
                    "decision",
                    Some(tool_name),
                    Some("allow"),
                    json!({"reason": "read_only_no_approval_required", "request_id": request_id, "args": args}),
                )?;
                self.run_and_audit(tool_name, tool, args, request_id)
            }
            Risk::Consequential => {
                let Some(approver) = self.approver else {
                    self.log(
                        &self.turn_id,
                        "decision",
                        Some(tool_name),
                        Some("deny"),
                        json!({"reason": "no_approver", "args": args}),
                    )?;
                    return Err(MimintError::Guard(format!(
                        "no approver for consequential tool: {tool_name}"
                    )));
                };
                // A stop that landed before the question was even
                // asked denies without showing a card.
                if let Err(err) = self.check_cancelled() {
                    self.log(
                        &self.turn_id,
                        "decision",
                        Some(tool_name),
                        Some("cancelled"),
                        json!({"reason": "cancelled_before_approval", "args": args}),
                    )?;
                    return Err(err);
                }
                let request_id = self.next_request_id.fetch_add(1, Ordering::SeqCst);
                let request = ApprovalRequest {
                    id: request_id,
                    tool: tool_name.to_string(),
                    args: args.clone(),
                };
                match approver.approve_request(&request) {
                    Ok(outcome) if outcome.request_id != request_id => {
                        self.log(
                            &self.turn_id,
                            "decision",
                            Some(tool_name),
                            Some("deny"),
                            json!({"reason": "approval_id_mismatch", "request_id": request_id, "outcome_request_id": outcome.request_id, "approver": approver.label(), "args": args}),
                        )?;
                        Err(MimintError::Guard(format!(
                            "approval decision carried request id {}, expected {request_id}; denying",
                            outcome.request_id
                        )))
                    }
                    Ok(outcome) => match outcome.decision {
                        ApprovalDecision::Allow => {
                            // The DỪNG NGAY check: immediately
                            // before execution, in the same step as
                            // the guard decision. An approval granted
                            // before the stop does not let the tool
                            // run.
                            if let Err(err) = self.check_cancelled() {
                                self.log(
                                    &self.turn_id,
                                    "decision",
                                    Some(tool_name),
                                    Some("cancelled"),
                                    json!({"reason": "cancelled_after_approval", "request_id": request_id, "approver": approver.label(), "args": args}),
                                )?;
                                return Err(err);
                            }
                            self.log(
                                &self.turn_id,
                                "decision",
                                Some(tool_name),
                                Some("allow"),
                                json!({"reason": "approved", "request_id": request_id, "approver": approver.label(), "args": args}),
                            )?;
                            self.run_and_audit(tool_name, tool, args, request_id)
                        }
                        ApprovalDecision::Deny => {
                            self.log(
                                &self.turn_id,
                                "decision",
                                Some(tool_name),
                                Some("deny"),
                                json!({"reason": "approval_refused", "request_id": request_id, "approver": approver.label(), "args": args}),
                            )?;
                            Err(MimintError::Guard(format!(
                                "approval refused for tool: {tool_name}"
                            )))
                        }
                        ApprovalDecision::Cancelled => {
                            self.log(
                                &self.turn_id,
                                "decision",
                                Some(tool_name),
                                Some("cancelled"),
                                json!({"reason": "approval_cancelled", "request_id": request_id, "approver": approver.label(), "args": args}),
                            )?;
                            Err(MimintError::Cancelled(format!(
                                "approval for tool {tool_name} was cancelled"
                            )))
                        }
                        ApprovalDecision::Timeout => {
                            self.log(
                                &self.turn_id,
                                "decision",
                                Some(tool_name),
                                Some("timeout"),
                                json!({"reason": "approval_timeout", "request_id": request_id, "approver": approver.label(), "args": args}),
                            )?;
                            Err(MimintError::Guard(format!(
                                "approval for tool {tool_name} timed out; denying"
                            )))
                        }
                    },
                    Err(err) => {
                        self.log(
                            &self.turn_id,
                            "decision",
                            Some(tool_name),
                            Some("deny"),
                            json!({"reason": "approver_error", "request_id": request_id, "approver": approver.label(), "error": err.to_string(), "args": args}),
                        )?;
                        Err(MimintError::Guard(format!("approver error: {err}")))
                    }
                }
            }
        }
    }

    fn run_and_audit(
        &self,
        tool_name: &str,
        tool: &dyn Tool,
        args: &Value,
        request_id: u64,
    ) -> Result<Value, MimintError> {
        match tool.execute(args) {
            Ok(output) => {
                self.log(
                    &self.turn_id,
                    "execution",
                    Some(tool_name),
                    Some("executed"),
                    json!({"request_id": request_id, "args": args, "output": output}),
                )?;
                Ok(output)
            }
            Err(err) => {
                self.log(
                    &self.turn_id,
                    "execution",
                    Some(tool_name),
                    Some("error"),
                    json!({"request_id": request_id, "args": args, "error": err.to_string()}),
                )?;
                Err(err)
            }
        }
    }
}
