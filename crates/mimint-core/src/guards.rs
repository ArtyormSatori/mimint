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
//! - Phase 6 (§3.1): approval is bound by the PAIR
//!   (`agent_id`, `request_id`). A decision for another agent or
//!   another id denies with the same reason,
//!   `approval_id_mismatch`. Audit records are attributed to the
//!   acting agent through `log_for`. `execute_code` arguments and
//!   outputs are redacted in the audit trail: only the code hash,
//!   language, length, sandbox verdict, exit code, and runtime are
//!   recorded — never the code text.

use crate::audit::{sha256_hex, AuditLog};
use crate::cancel::CancelToken;
use crate::error::MimintError;
use crate::tools::{Registry, Tool};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};

/// Audit-safe form of a tool's arguments. For `execute_code` the
/// code text is replaced by its hash + language + length; every
/// other tool's arguments are recorded as-is (the pre-Phase-6
/// behaviour).
fn audit_args(tool_name: &str, args: &Value) -> Value {
    if tool_name != "execute_code" {
        return args.clone();
    }
    let code = args.get("code").and_then(|c| c.as_str()).unwrap_or("");
    json!({
        "language": args.get("language").cloned().unwrap_or(Value::Null),
        "code_sha256": sha256_hex(code.as_bytes()),
        "code_len": code.len(),
        "input_files_sha256": json!(crate::sandbox::execute_code_input_hashes(args)),
    })
}

/// Audit-safe form of a tool's output. For `execute_code` only a
/// summary is recorded (exit code, verdict, duration, output
/// lengths); every other tool's output is recorded as-is.
fn audit_output(tool_name: &str, output: &Value) -> Value {
    if tool_name != "execute_code" {
        return output.clone();
    }
    let stdout_len = output.get("stdout").and_then(|s| s.as_str()).map(str::len);
    let stderr_len = output.get("stderr").and_then(|s| s.as_str()).map(str::len);
    json!({
        "exit_code": output.get("exit_code").cloned().unwrap_or(Value::Null),
        "sandbox_verdict": output.get("sandbox_verdict").cloned().unwrap_or(Value::Null),
        "duration_ms": output.get("duration_ms").cloned().unwrap_or(Value::Null),
        "truncated": output.get("truncated").cloned().unwrap_or(Value::Null),
        "code_sha256": output.get("code_sha256").cloned().unwrap_or(Value::Null),
        "input_files_sha256": output.get("input_files_sha256").cloned().unwrap_or(Value::Null),
        "stdout_len": stdout_len,
        "stderr_len": stderr_len,
    })
}

/// Merge a tool's [`Tool::audit_fields`] into an audit
/// detail object (Phase 6, §6.3). Tools without fields
/// leave the detail untouched.
fn with_audit_fields(tool: &dyn Tool, mut detail: Value) -> Value {
    if let Some(Value::Object(extra)) = tool.audit_fields() {
        if let Value::Object(base) = &mut detail {
            base.extend(extra);
        }
    }
    detail
}

/// Merge a tool's audit fields into a detail map built
/// incrementally (the `run_and_audit` path).
fn merge_audit_fields_into(tool: &dyn Tool, detail: &mut serde_json::Map<String, Value>) {
    if let Some(Value::Object(extra)) = tool.audit_fields() {
        detail.extend(extra);
    }
}

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
///
/// Phase 6 (§3.1): the binding is the pair
/// (`agent_id`, `request_id`). Request ids are monotonic per
/// executor, so the pair is what makes a decision unambiguous
/// across agents.
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalRequest {
    /// Monotonic within one executor; unique per request.
    pub id: u64,
    /// The agent asking. Part of the binding pair.
    pub agent_id: String,
    pub tool: String,
    pub args: Value,
    /// Extra context for the approval card, from the tool's
    /// `audit_fields` (MCP server/tool/version, when any).
    /// An empty object when the tool carries none.
    pub context: Value,
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
/// different action). Phase 6 (§3.1): the binding is the pair
/// (`agent_id`, `request_id`); a mismatch on EITHER denies with
/// the same reason, `approval_id_mismatch`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalOutcome {
    pub request_id: u64,
    pub agent_id: String,
    pub decision: ApprovalDecision,
}

impl ApprovalOutcome {
    pub fn allow(request_id: u64, agent_id: impl Into<String>) -> Self {
        Self {
            request_id,
            agent_id: agent_id.into(),
            decision: ApprovalDecision::Allow,
        }
    }

    pub fn deny(request_id: u64, agent_id: impl Into<String>) -> Self {
        Self {
            request_id,
            agent_id: agent_id.into(),
            decision: ApprovalDecision::Deny,
        }
    }

    pub fn cancelled(request_id: u64, agent_id: impl Into<String>) -> Self {
        Self {
            request_id,
            agent_id: agent_id.into(),
            decision: ApprovalDecision::Cancelled,
        }
    }

    pub fn timeout(request_id: u64, agent_id: impl Into<String>) -> Self {
        Self {
            request_id,
            agent_id: agent_id.into(),
            decision: ApprovalDecision::Timeout,
        }
    }
}

/// Decides whether one consequential tool call may run.
///
/// `Send + Sync` (Phase 6, §2): the approver is shared across
/// agent tasks through `Arc`, and the brain's `run` future must
/// be `Send` to spawn on the multi-thread runtime. Every
/// implementor in the tree is already `Send + Sync` (unit
/// structs, mutexes, channels); the bound only says so.
pub trait Approver: Send + Sync {
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
            ApprovalOutcome::allow(request.id, request.agent_id.clone())
        } else {
            ApprovalOutcome::deny(request.id, request.agent_id.clone())
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
    /// The agent this executor serves (Phase 6, §1.3). Every
    /// audit record is attributed to it, and every approval is
    /// bound to the pair (`agent_id`, `request_id`).
    agent_id: String,
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
        agent_id: impl Into<String>,
    ) -> Self {
        Self {
            registry,
            approver,
            audit,
            turn_id: turn_id.into(),
            agent_id: agent_id.into(),
            next_request_id: AtomicU64::new(1),
            cancel_tokens: Vec::new(),
            profile: None,
        }
    }

    /// The agent this executor serves.
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// Set the active profile name (Phase 5, P5.5). The name is
    /// tagged onto every audit record this executor writes.
    pub fn set_profile(&mut self, name: impl Into<String>) {
        self.profile = Some(name.into());
    }

    /// Audit helper with the same shape as [`AuditLog::log_for`],
    /// but tagging the record's detail object with the active
    /// profile name when one is set. Records are attributed to
    /// this executor's agent (§1.3).
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
        self.audit
            .log_for(&self.agent_id, turn_id, kind, tool, result, detail)
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
                json!({"reason": "unknown_tool", "args": audit_args(tool_name, args)}),
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
                        with_audit_fields(tool, json!({"reason": "cancelled_before_execution", "request_id": request_id, "args": audit_args(tool_name, args)})),
                    )?;
                    return Err(err);
                }
                self.log(
                    &self.turn_id,
                    "decision",
                    Some(tool_name),
                    Some("allow"),
                    with_audit_fields(tool, json!({"reason": "read_only_no_approval_required", "request_id": request_id, "args": audit_args(tool_name, args)})),
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
                        with_audit_fields(
                            tool,
                            json!({"reason": "no_approver", "args": audit_args(tool_name, args)}),
                        ),
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
                        with_audit_fields(tool, json!({"reason": "cancelled_before_approval", "args": audit_args(tool_name, args)})),
                    )?;
                    return Err(err);
                }
                let request_id = self.next_request_id.fetch_add(1, Ordering::SeqCst);
                let request = ApprovalRequest {
                    id: request_id,
                    agent_id: self.agent_id.clone(),
                    tool: tool_name.to_string(),
                    args: args.clone(),
                    context: tool.audit_fields().unwrap_or_else(|| json!({})),
                };
                match approver.approve_request(&request) {
                    Ok(outcome)
                        if outcome.request_id != request_id
                            || outcome.agent_id != self.agent_id =>
                    {
                        self.log(
                            &self.turn_id,
                            "decision",
                            Some(tool_name),
                            Some("deny"),
                            with_audit_fields(tool, json!({"reason": "approval_id_mismatch", "request_id": request_id, "outcome_request_id": outcome.request_id, "outcome_agent_id": outcome.agent_id, "approver": approver.label(), "args": audit_args(tool_name, args)})),
                        )?;
                        Err(MimintError::Guard(format!(
                            "approval decision carried ({}, {}), expected ({}, {request_id}); denying",
                            outcome.agent_id, outcome.request_id, self.agent_id
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
                                    with_audit_fields(tool, json!({"reason": "cancelled_after_approval", "request_id": request_id, "approver": approver.label(), "args": audit_args(tool_name, args)})),
                                )?;
                                return Err(err);
                            }
                            self.log(
                                &self.turn_id,
                                "decision",
                                Some(tool_name),
                                Some("allow"),
                                with_audit_fields(tool, json!({"reason": "approved", "request_id": request_id, "approver": approver.label(), "args": audit_args(tool_name, args)})),
                            )?;
                            self.run_and_audit(tool_name, tool, args, request_id)
                        }
                        ApprovalDecision::Deny => {
                            self.log(
                                &self.turn_id,
                                "decision",
                                Some(tool_name),
                                Some("deny"),
                                with_audit_fields(tool, json!({"reason": "approval_refused", "request_id": request_id, "approver": approver.label(), "args": audit_args(tool_name, args)})),
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
                                with_audit_fields(tool, json!({"reason": "approval_cancelled", "request_id": request_id, "approver": approver.label(), "args": audit_args(tool_name, args)})),
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
                                with_audit_fields(tool, json!({"reason": "approval_timeout", "request_id": request_id, "approver": approver.label(), "args": audit_args(tool_name, args)})),
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
                            with_audit_fields(tool, json!({"reason": "approver_error", "request_id": request_id, "approver": approver.label(), "error": err.to_string(), "args": audit_args(tool_name, args)})),
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
        let redacted = audit_args(tool_name, args);
        let result = tool.execute(args);
        // Queue wait (§9): body tools report how long the action
        // waited for the actor; the execution record carries it
        // (0 when it ran at once is reported as Some(0) by the
        // tool, None when the tool has no queue).
        let queued_ms = tool.last_queue_wait_ms();
        match &result {
            Ok(output) => {
                let mut detail = serde_json::Map::new();
                detail.insert("request_id".to_string(), json!(request_id));
                detail.insert("args".to_string(), redacted.clone());
                if let Some(ms) = queued_ms {
                    detail.insert("queued_ms".to_string(), json!(ms));
                }
                detail.insert("output".to_string(), audit_output(tool_name, output));
                merge_audit_fields_into(tool, &mut detail);
                self.log(
                    &self.turn_id,
                    "execution",
                    Some(tool_name),
                    Some("executed"),
                    Value::Object(detail),
                )?;
                if tool_name == "execute_code" {
                    self.log_code_execution(tool_name, args, Some(output), None)?;
                }
                result
            }
            Err(err) => {
                let mut detail = serde_json::Map::new();
                detail.insert("request_id".to_string(), json!(request_id));
                detail.insert("args".to_string(), redacted.clone());
                if let Some(ms) = queued_ms {
                    detail.insert("queued_ms".to_string(), json!(ms));
                }
                detail.insert("error".to_string(), json!(err.to_string()));
                merge_audit_fields_into(tool, &mut detail);
                self.log(
                    &self.turn_id,
                    "execution",
                    Some(tool_name),
                    Some("error"),
                    Value::Object(detail),
                )?;
                if tool_name == "execute_code" {
                    self.log_code_execution(tool_name, args, None, Some(err))?;
                }
                result
            }
        }
    }

    /// The dedicated `code_execution` record (§5.4): hash,
    /// language, length, sandbox verdict, exit code, runtime,
    /// outcome — never the code text.
    fn log_code_execution(
        &self,
        tool_name: &str,
        args: &Value,
        output: Option<&Value>,
        error: Option<&MimintError>,
    ) -> Result<(), MimintError> {
        let code = args.get("code").and_then(|c| c.as_str()).unwrap_or("");
        let approved_hash = sha256_hex(code.as_bytes());
        let approved_inputs = json!(crate::sandbox::execute_code_input_hashes(args));
        let mut detail = serde_json::Map::new();
        detail.insert(
            "language".to_string(),
            args.get("language").cloned().unwrap_or(Value::Null),
        );
        detail.insert("code_len".to_string(), json!(code.len()));
        detail.insert("code_sha256".to_string(), json!(approved_hash));
        detail.insert("input_files_sha256".to_string(), approved_inputs.clone());
        if let Some(output) = output {
            // Hash verification (§15.2): the code that ran must
            // be the code that was approved. The tool reports
            // the hash it hashed before execution; a mismatch
            // is recorded, never silent. A missing executed
            // hash verifies as false.
            let executed = output.get("code_sha256").and_then(|h| h.as_str());
            detail.insert(
                "executed_code_sha256".to_string(),
                executed.map(Value::from).unwrap_or(Value::Null),
            );
            detail.insert(
                "code_hash_verified".to_string(),
                json!(executed == Some(approved_hash.as_str())),
            );
            // Input-hash verification (F6-06): the executed
            // input list must equal the approved one, in
            // order. A missing executed list verifies only
            // when nothing was approved (both empty is a
            // match; an approved input with no executed list
            // is a mismatch, never silent).
            let executed_inputs = output.get("input_files_sha256").cloned();
            detail.insert(
                "executed_input_files_sha256".to_string(),
                executed_inputs.clone().unwrap_or(Value::Null),
            );
            let inputs_verified = match &executed_inputs {
                Some(Value::Array(list)) => Value::Array(list.clone()) == approved_inputs,
                _ => approved_inputs
                    .as_array()
                    .is_some_and(|list| list.is_empty()),
            };
            detail.insert("input_hashes_verified".to_string(), json!(inputs_verified));
            detail.insert(
                "sandbox_verdict".to_string(),
                output
                    .get("sandbox_verdict")
                    .cloned()
                    .unwrap_or(Value::Null),
            );
            detail.insert(
                "exit_code".to_string(),
                output.get("exit_code").cloned().unwrap_or(Value::Null),
            );
            detail.insert(
                "duration_ms".to_string(),
                output.get("duration_ms").cloned().unwrap_or(Value::Null),
            );
            detail.insert(
                "truncated".to_string(),
                output.get("truncated").cloned().unwrap_or(Value::Null),
            );
        } else if let Some(err) = error {
            // Denied/failed runs carry no tool output; the verdict
            // is the reason prefix of the tool error's inner
            // message ("sandbox_unavailable",
            // "unsupported_language", "timeout", ...), which
            // the tool prefixes by construction. Never the
            // code text. `message()` strips the kind prefix
            // that `Display` adds.
            let verdict = err.message();
            let verdict = verdict.split(':').next().unwrap_or("unknown");
            detail.insert("sandbox_verdict".to_string(), json!(verdict));
        } else {
            detail.insert("sandbox_verdict".to_string(), json!("unknown"));
        }
        if let Some(err) = error {
            detail.insert("error".to_string(), json!(err.to_string()));
        }
        let outcome = if error.is_some() { "error" } else { "executed" };
        self.log(
            &self.turn_id,
            "code_execution",
            Some(tool_name),
            Some(outcome),
            Value::Object(detail),
        )
    }
}
