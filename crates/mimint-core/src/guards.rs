//! Guards: deny-by-default policy decision point (P0-17).
//!
//! - Unknown tools (not in the registry allow-list) are denied.
//! - Consequential tools require an approver; no approver, an approver
//!   error, or a refusal all deny. There is no implicit allow.
//! - Every allow/deny decision is written to the audit log, and every
//!   execution (success or failure) is recorded as well.

use crate::audit::AuditLog;
use crate::error::MimintError;
use crate::tools::{Registry, Tool};
use serde_json::{json, Value};

/// Consequence level of a tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Risk {
    /// Reads only; no state change, no external effect.
    ReadOnly,
    /// Changes state or has an external effect; requires approval.
    Consequential,
}

/// Decides whether one consequential tool call may run.
pub trait Approver {
    /// Return `Ok(true)` to allow, `Ok(false)` to deny.
    /// An error denies (deny-by-default).
    fn approve(&self, tool: &str, args: &Value) -> Result<bool, MimintError>;
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
        }
    }

    /// Run one tool call through the guard policy.
    ///
    /// Denials return an error and never execute the tool. Audit write
    /// failures also fail closed: nothing runs unless its decision is
    /// recorded.
    pub fn execute(&self, tool_name: &str, args: &Value) -> Result<Value, MimintError> {
        let Some(tool) = self.registry.get(tool_name) else {
            self.audit.log(
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
                self.audit.log(
                    &self.turn_id,
                    "decision",
                    Some(tool_name),
                    Some("allow"),
                    json!({"reason": "read_only_no_approval_required", "args": args}),
                )?;
                self.run_and_audit(tool_name, tool, args)
            }
            Risk::Consequential => {
                let Some(approver) = self.approver else {
                    self.audit.log(
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
                match approver.approve(tool_name, args) {
                    Ok(true) => {
                        self.audit.log(
                            &self.turn_id,
                            "decision",
                            Some(tool_name),
                            Some("allow"),
                            json!({"reason": "approved", "approver": approver.label(), "args": args}),
                        )?;
                        self.run_and_audit(tool_name, tool, args)
                    }
                    Ok(false) => {
                        self.audit.log(
                            &self.turn_id,
                            "decision",
                            Some(tool_name),
                            Some("deny"),
                            json!({"reason": "approval_refused", "approver": approver.label(), "args": args}),
                        )?;
                        Err(MimintError::Guard(format!(
                            "approval refused for tool: {tool_name}"
                        )))
                    }
                    Err(err) => {
                        self.audit.log(
                            &self.turn_id,
                            "decision",
                            Some(tool_name),
                            Some("deny"),
                            json!({"reason": "approver_error", "approver": approver.label(), "error": err.to_string(), "args": args}),
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
    ) -> Result<Value, MimintError> {
        match tool.execute(args) {
            Ok(output) => {
                self.audit.log(
                    &self.turn_id,
                    "execution",
                    Some(tool_name),
                    Some("executed"),
                    json!({"args": args, "output": output}),
                )?;
                Ok(output)
            }
            Err(err) => {
                self.audit.log(
                    &self.turn_id,
                    "execution",
                    Some(tool_name),
                    Some("error"),
                    json!({"args": args, "error": err.to_string()}),
                )?;
                Err(err)
            }
        }
    }
}
