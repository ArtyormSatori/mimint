//! Brain loop (Phase 1): at most N steps, sequential tools, with two
//! mandatory coded phases per iteration: verify-previous + reflect.
//!
//! - No regex parsing of model output or coordinates, by design.
//! - Tools run one at a time, in order. Parallelism is Phase 6.
//! - The loop stops when the model answers with text, or when the step
//!   budget runs out (partial result, recorded in the audit log).

use crate::audit::AuditLog;
use crate::error::MimintError;
use crate::guards::{Approver, GuardedExecutor};
use crate::notebook::Notebook;
use crate::provider::{ChatMessage, ProviderClient};
use crate::tools::Registry;
use serde_json::{json, Value};
use std::sync::Arc;

/// Outcome of one turn.
#[derive(Debug, Clone)]
pub struct TurnOutcome {
    /// Final text from the model (empty when the budget ran out mid-task).
    pub text: String,
    /// Provider calls made.
    pub steps: usize,
    /// Tool names executed, in order.
    pub tools_used: Vec<String>,
    /// True when the step budget ran out before a final answer.
    pub partial: bool,
}

struct PendingVerification {
    tool: String,
    args: Value,
    output: Option<Value>,
    error: Option<String>,
}

/// One-turn brain loop over a provider, a tool registry, and a notebook.
pub struct Brain<'a> {
    provider: &'a ProviderClient,
    registry: &'a Registry,
    approver: Option<&'a dyn Approver>,
    audit: &'a AuditLog,
    notebook: Arc<Notebook>,
    turn_id: String,
    max_steps: usize,
}

impl<'a> Brain<'a> {
    /// New brain loop. `max_steps` must be at least 1.
    pub fn new(
        provider: &'a ProviderClient,
        registry: &'a Registry,
        approver: Option<&'a dyn Approver>,
        audit: &'a AuditLog,
        notebook: Arc<Notebook>,
        turn_id: impl Into<String>,
        max_steps: usize,
    ) -> Result<Self, MimintError> {
        if max_steps == 0 {
            return Err(MimintError::Config(
                "max-steps must be at least 1".to_string(),
            ));
        }
        Ok(Self {
            provider,
            registry,
            approver,
            audit,
            notebook,
            turn_id: turn_id.into(),
            max_steps,
        })
    }

    /// Run one turn for a user prompt.
    pub async fn run(&self, user_prompt: &str) -> Result<TurnOutcome, MimintError> {
        let executor = GuardedExecutor::new(
            self.registry,
            self.approver,
            self.audit,
            self.turn_id.clone(),
        );
        let tool_schemas = self.registry.tool_schemas();
        let mut messages = vec![
            ChatMessage::system(
                "You are Mimint, a local-first personal assistant. Use the provided tools when they help answer. Keep answers short.",
            ),
            ChatMessage::user(user_prompt),
        ];
        let mut tools_used: Vec<String> = Vec::new();
        let mut verified: Vec<(String, bool)> = Vec::new();
        let mut pending: Vec<PendingVerification> = Vec::new();

        for step in 0..self.max_steps {
            // Phase 1 (mandatory): verify the previous tool results by code.
            for prev in pending.drain(..) {
                let ok = self.verify_previous(&prev)?;
                verified.push((prev.tool, ok));
            }
            // Phase 2 (mandatory): structured reflection, audited and fed back.
            let remaining = self.max_steps - step;
            let reflection = json!({
                "step": step + 1,
                "done": tools_used.clone(),
                "verified": verified.iter().map(|(tool, ok)| json!({"tool": tool, "verified": ok})).collect::<Vec<_>>(),
                "remaining_steps": remaining,
            });
            self.audit
                .log(&self.turn_id, "reflection", None, None, reflection.clone())?;
            messages.push(ChatMessage::system(format!("Reflection: {reflection}")));

            let reply = self.provider.chat(&messages, &tool_schemas).await?;
            let calls = reply.tool_calls.clone().unwrap_or_default();
            if calls.is_empty() {
                let text = reply.content.clone().unwrap_or_default();
                messages.push(reply);
                return Ok(TurnOutcome {
                    text,
                    steps: step + 1,
                    tools_used,
                    partial: false,
                });
            }
            messages.push(reply);
            // Tools run strictly sequentially, in the order requested.
            for call in calls {
                tools_used.push(call.function.name.clone());
                let parsed: Result<Value, MimintError> =
                    serde_json::from_str(&call.function.arguments).map_err(|err| {
                        MimintError::Tool(format!("tool arguments are not valid JSON: {err}"))
                    });
                match parsed {
                    Ok(args) => match executor.execute(&call.function.name, &args) {
                        Ok(output) => {
                            messages.push(ChatMessage::tool_result(&call.id, output.to_string()));
                            pending.push(PendingVerification {
                                tool: call.function.name.clone(),
                                args,
                                output: Some(output),
                                error: None,
                            });
                        }
                        Err(err) => {
                            let detail = json!({"error": err.to_string()});
                            messages.push(ChatMessage::tool_result(&call.id, detail.to_string()));
                            pending.push(PendingVerification {
                                tool: call.function.name.clone(),
                                args,
                                output: None,
                                error: Some(err.to_string()),
                            });
                        }
                    },
                    Err(err) => {
                        let detail = json!({"error": err.to_string()});
                        messages.push(ChatMessage::tool_result(&call.id, detail.to_string()));
                        pending.push(PendingVerification {
                            tool: call.function.name.clone(),
                            args: Value::Null,
                            output: None,
                            error: Some(err.to_string()),
                        });
                    }
                }
            }
        }

        // Verify the last step's tools too, so every execution is checked.
        for prev in pending.drain(..) {
            let ok = self.verify_previous(&prev)?;
            verified.push((prev.tool, ok));
        }
        let _ = &verified;
        self.audit.log(
            &self.turn_id,
            "turn_end",
            None,
            Some("partial"),
            json!({
                "reason": "step_budget_exhausted",
                "steps": self.max_steps,
                "tools_used": tools_used.clone(),
            }),
        )?;
        Ok(TurnOutcome {
            text: String::new(),
            steps: self.max_steps,
            tools_used,
            partial: true,
        })
    }

    /// Verify one previous tool result by code and audit the outcome.
    ///
    /// `notebook_add` is verified by re-reading the row just written and
    /// comparing its title and body. Other tools are verified by status
    /// (the executor returned success); Phase 1 has no further verifier
    /// hook. A failed verification is recorded and reflected to the model
    /// on the next round; it does not abort the turn.
    fn verify_previous(&self, prev: &PendingVerification) -> Result<bool, MimintError> {
        if let Some(error) = &prev.error {
            self.audit.log(
                &self.turn_id,
                "verification",
                Some(&prev.tool),
                Some("failed"),
                json!({"reason": "tool_error", "error": error}),
            )?;
            return Ok(false);
        }
        let Some(output) = &prev.output else {
            self.audit.log(
                &self.turn_id,
                "verification",
                Some(&prev.tool),
                Some("failed"),
                json!({"reason": "no_output"}),
            )?;
            return Ok(false);
        };
        if prev.tool == "notebook_add" {
            let id = output.get("id").and_then(Value::as_i64);
            let verified = match id {
                Some(id) => match self.notebook.get(id)? {
                    Some(note) => {
                        let title_ok = prev.args.get("title").and_then(Value::as_str)
                            == Some(note.title.as_str());
                        let body_ok = prev.args.get("body").and_then(Value::as_str)
                            == Some(note.body.as_str());
                        title_ok && body_ok
                    }
                    None => false,
                },
                None => false,
            };
            self.audit.log(
                &self.turn_id,
                "verification",
                Some(&prev.tool),
                Some(if verified { "verified" } else { "failed" }),
                json!({"id": id, "re_read_row": verified}),
            )?;
            Ok(verified)
        } else {
            self.audit.log(
                &self.turn_id,
                "verification",
                Some(&prev.tool),
                Some("verified"),
                json!({"reason": "status_ok"}),
            )?;
            Ok(true)
        }
    }
}
