//! Brain loop (Phase 1): at most N steps, sequential tools, with two
//! mandatory coded phases per iteration: verify-previous + reflect.
//!
//! - No regex parsing of model output or coordinates, by design.
//! - Tools run one at a time, in order. Parallelism is Phase 6.
//! - The loop stops when the model answers with text, or when the step
//!   budget runs out (partial result, recorded in the audit log).

use crate::audit::AuditLog;
use crate::cancel::CancelToken;
use crate::error::MimintError;
use crate::guards::{Approver, GuardedExecutor};
use crate::notebook::Notebook;
use crate::provider::{CallContext, ChatMessage, Provider};
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

/// A step verifier: re-reads state after a tool executes and
/// reports whether the tool's claimed effect is observable.
///
/// Body-free by design: mimint-core never depends on a body.
/// The control crate registers a verifier that re-screenshots
/// after body tools and records frame metadata in the
/// verification audit record.
///
/// Contract:
/// - Return `Ok(None)` for tools the verifier has no opinion
///   on: the brain falls back to status verification.
/// - Return `Ok(Some((verified, detail)))` to decide: `detail`
///   becomes the verification audit record's data, so it must
///   be small, JSON-serializable, and free of secrets and
///   pixels (frame metadata, not frame bytes).
/// - Return `Err` when verification itself fails: the step is
///   recorded as failed with reason `verifier_error`. A
///   verifier error never aborts the turn.
/// - `verify` is synchronous and must be bounded: it runs on
///   the worker thread between steps. A verifier that talks
///   to hardware must enforce its own timeout.
/// - `notebook_add` keeps its built-in re-read and is never
///   offered to the verifier.
pub trait StepVerifier: Send + Sync {
    fn verify(
        &self,
        tool: &str,
        args: &Value,
        output: &Value,
    ) -> Result<Option<(bool, Value)>, MimintError>;
}

/// One-turn brain loop over a provider, a tool registry, and a notebook.
/// The provider is any [`Provider`] implementation: a single backend or
/// a fallback chain. The loop itself is protocol-agnostic.
pub struct Brain<'a> {
    provider: &'a dyn Provider,
    registry: &'a Registry,
    approver: Option<&'a dyn Approver>,
    audit: &'a AuditLog,
    notebook: Arc<Notebook>,
    turn_id: String,
    max_steps: usize,
    cancel_tokens: Vec<CancelToken>,
    verifier: Option<Arc<dyn StepVerifier>>,
}

impl<'a> Brain<'a> {
    /// New brain loop. `max_steps` must be at least 1.
    pub fn new(
        provider: &'a dyn Provider,
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
            cancel_tokens: Vec::new(),
            verifier: None,
        })
    }

    /// Register a step verifier (see [`StepVerifier`]). A brain
    /// without one behaves exactly as before: status
    /// verification for every tool except `notebook_add`.
    pub fn set_step_verifier(&mut self, verifier: Arc<dyn StepVerifier>) {
        self.verifier = Some(verifier);
    }

    /// Add a cancellation token: checked before every provider call
    /// and, through the guard executor, immediately before every tool
    /// executes. The worker adds the per-turn token and the latching
    /// stop token; a brain without tokens behaves exactly as before.
    pub fn add_cancel_token(&mut self, token: CancelToken) {
        self.cancel_tokens.push(token);
    }

    fn check_cancelled(&self) -> Result<(), MimintError> {
        for token in &self.cancel_tokens {
            token.check()?;
        }
        Ok(())
    }

    /// Run one turn for a user prompt.
    ///
    /// Cancellation aborts the turn: a cancelled check, or a
    /// cancelled guard decision, returns [`MimintError::Cancelled`]
    /// after recording a `turn_end`/`cancelled` audit record. It is
    /// never fed back to the model as a tool error, and the loop
    /// never starts another provider call or tool after it.
    pub async fn run(&self, user_prompt: &str) -> Result<TurnOutcome, MimintError> {
        let mut executor = GuardedExecutor::new(
            self.registry,
            self.approver,
            self.audit,
            self.turn_id.clone(),
        );
        for token in &self.cancel_tokens {
            executor.add_cancel_token(token.clone());
        }
        let executor = executor;
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
            // Cancellation check at the top of every step: no new
            // provider call and no new tool starts after a stop.
            // (A request already in flight is bounded by its
            // per-request timeout; see docs/stop-and-approval.md.)
            if let Err(err) = self.check_cancelled() {
                self.audit.log(
                    &self.turn_id,
                    "turn_end",
                    None,
                    Some("cancelled"),
                    json!({"reason": "cancelled_before_step", "step": step + 1}),
                )?;
                return Err(err);
            }
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

            let ctx = CallContext::new(&self.turn_id, Some(self.audit));
            let reply = self.provider.chat(&ctx, &messages, &tool_schemas).await?;
            let calls = reply.tool_calls.clone().unwrap_or_default();
            if calls.is_empty() {
                let text = reply.content.clone().unwrap_or_default();
                // An empty reply (no text, no tool calls) is a provider
                // malfunction, not an answer: failing loudly beats
                // reporting silent success with nothing to show.
                if text.trim().is_empty() {
                    return Err(MimintError::Provider(
                        "provider returned an empty response (no text, no tool calls)".to_string(),
                    ));
                }
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
                // A stop between two tool calls in one step stops
                // the rest of the step's tools.
                if let Err(err) = self.check_cancelled() {
                    self.audit.log(
                        &self.turn_id,
                        "turn_end",
                        None,
                        Some("cancelled"),
                        json!({"reason": "cancelled_between_tools", "step": step + 1}),
                    )?;
                    return Err(err);
                }
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
                        Err(err) if err.is_cancelled() => {
                            // Cancellation is not a tool error: abort
                            // the turn instead of telling the model.
                            self.audit.log(
                                &self.turn_id,
                                "turn_end",
                                None,
                                Some("cancelled"),
                                json!({"reason": "cancelled_during_tool", "tool": call.function.name}),
                            )?;
                            return Err(err);
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
    /// comparing its title and body. Other tools are offered to the
    /// registered [`StepVerifier`] when one exists (its `None` means
    /// "no opinion", falling back to status verification); without a
    /// verifier, status verification applies (the executor returned
    /// success). A failed verification is recorded and reflected to
    /// the model on the next round; it does not abort the turn.
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
        } else if let Some(verifier) = &self.verifier {
            match verifier.verify(&prev.tool, &prev.args, output) {
                Ok(Some((verified, detail))) => {
                    self.audit.log(
                        &self.turn_id,
                        "verification",
                        Some(&prev.tool),
                        Some(if verified { "verified" } else { "failed" }),
                        detail,
                    )?;
                    Ok(verified)
                }
                Ok(None) => self.verify_by_status(prev),
                Err(err) => {
                    self.audit.log(
                        &self.turn_id,
                        "verification",
                        Some(&prev.tool),
                        Some("failed"),
                        json!({"reason": "verifier_error", "error": err.to_string()}),
                    )?;
                    Ok(false)
                }
            }
        } else {
            self.verify_by_status(prev)
        }
    }

    /// Status verification: the executor returned success, so the
    /// step counts as verified. Used when no verifier is
    /// registered, or the verifier has no opinion on the tool.
    fn verify_by_status(&self, prev: &PendingVerification) -> Result<bool, MimintError> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{BoxFuture, CallContext};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Provider that always returns the same canned message.
    struct FixedProvider {
        message: ChatMessage,
    }

    impl Provider for FixedProvider {
        fn name(&self) -> &str {
            "fixed"
        }

        fn model(&self) -> &str {
            "fixed-model"
        }

        fn endpoint_url(&self) -> &str {
            "http://127.0.0.1:1/v1/chat/completions"
        }

        fn is_paid(&self) -> bool {
            false
        }

        fn chat<'a>(
            &'a self,
            _ctx: &'a CallContext<'a>,
            _messages: &'a [ChatMessage],
            _tools: &'a [Value],
        ) -> BoxFuture<'a, Result<ChatMessage, MimintError>> {
            let message = self.message.clone();
            Box::pin(async move { Ok(message) })
        }
    }

    fn temp_stem(tag: &str) -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!(
            "{}/mimint-brain-{}-{nanos}-{tag}",
            std::env::temp_dir().display(),
            std::process::id()
        )
    }

    async fn run_with(
        message: ChatMessage,
        audit_path: &str,
        db_path: &str,
    ) -> Result<TurnOutcome, MimintError> {
        let notebook = Arc::new(Notebook::open(db_path)?);
        let audit = AuditLog::open(audit_path)?;
        let registry = crate::tools::default_registry(Arc::clone(&notebook));
        let provider = FixedProvider { message };
        let brain = Brain::new(
            &provider,
            &registry,
            None,
            &audit,
            Arc::clone(&notebook),
            "turn-brain-test",
            3,
        )?;
        brain.run("hello").await
    }

    #[tokio::test]
    async fn empty_response_is_an_error_not_silent_success() {
        let stem = temp_stem("empty");
        let db_path = format!("{stem}.db");
        let audit_path = format!("{stem}.jsonl");
        let empty = ChatMessage {
            role: "assistant".to_string(),
            content: None,
            tool_calls: None,
            tool_call_id: None,
        };
        let err = run_with(empty, &audit_path, &db_path)
            .await
            .expect_err("empty reply must fail");
        assert!(err.to_string().contains("empty response"), "{err}");

        let blank = ChatMessage {
            role: "assistant".to_string(),
            content: Some("   ".to_string()),
            tool_calls: None,
            tool_call_id: None,
        };
        let err = run_with(blank, &audit_path, &db_path)
            .await
            .expect_err("whitespace-only reply must fail");
        assert!(err.to_string().contains("empty response"), "{err}");

        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(&audit_path);
    }

    #[tokio::test]
    async fn text_response_still_succeeds() {
        let stem = temp_stem("text");
        let db_path = format!("{stem}.db");
        let audit_path = format!("{stem}.jsonl");
        let outcome = run_with(
            ChatMessage {
                role: "assistant".to_string(),
                content: Some("An answer.".to_string()),
                tool_calls: None,
                tool_call_id: None,
            },
            &audit_path,
            &db_path,
        )
        .await
        .expect("text reply succeeds");
        assert_eq!(outcome.text, "An answer.");
        assert!(!outcome.partial);
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(&audit_path);
    }

    /// Provider that returns a scripted sequence of replies, then a
    /// final text reply forever.
    struct ScriptProvider {
        replies: std::sync::Mutex<std::collections::VecDeque<ChatMessage>>,
    }

    impl Provider for ScriptProvider {
        fn name(&self) -> &str {
            "script"
        }

        fn model(&self) -> &str {
            "script-model"
        }

        fn endpoint_url(&self) -> &str {
            "http://127.0.0.1:1/v1/chat/completions"
        }

        fn is_paid(&self) -> bool {
            false
        }

        fn chat<'a>(
            &'a self,
            _ctx: &'a CallContext<'a>,
            _messages: &'a [ChatMessage],
            _tools: &'a [Value],
        ) -> BoxFuture<'a, Result<ChatMessage, MimintError>> {
            let message = self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(ChatMessage {
                    role: "assistant".to_string(),
                    content: Some("Done.".to_string()),
                    tool_calls: None,
                    tool_call_id: None,
                });
            Box::pin(async move { Ok(message) })
        }
    }

    struct RecordingVerifier {
        calls: std::sync::Mutex<Vec<(String, Value, Value)>>,
        verdict: Option<(bool, Value)>,
    }

    impl StepVerifier for RecordingVerifier {
        fn verify(
            &self,
            tool: &str,
            args: &Value,
            output: &Value,
        ) -> Result<Option<(bool, Value)>, MimintError> {
            self.calls
                .lock()
                .unwrap()
                .push((tool.to_string(), args.clone(), output.clone()));
            Ok(self.verdict.clone())
        }
    }

    fn tool_call_message(tool: &str, args: &str) -> ChatMessage {
        use crate::provider::{FunctionCall, ToolCall};
        ChatMessage {
            role: "assistant".to_string(),
            content: None,
            tool_calls: Some(vec![ToolCall {
                id: "call_1".to_string(),
                kind: "function".to_string(),
                function: FunctionCall {
                    name: tool.to_string(),
                    arguments: args.to_string(),
                },
            }]),
            tool_call_id: None,
        }
    }

    fn audit_records(path: &str) -> Vec<Value> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn step_verifier_decides_and_its_detail_is_audited() {
        let stem = temp_stem("verifier");
        let db_path = format!("{stem}.db");
        let audit_path = format!("{stem}.jsonl");
        let notebook = Arc::new(Notebook::open(&db_path).unwrap());
        let audit = AuditLog::open(&audit_path).unwrap();
        let registry = crate::tools::default_registry(Arc::clone(&notebook));
        let provider = ScriptProvider {
            replies: std::sync::Mutex::new(std::collections::VecDeque::from([
                tool_call_message("notebook_search", r#"{"query":"nothing"}"#),
                ChatMessage {
                    role: "assistant".to_string(),
                    content: Some("Done.".to_string()),
                    tool_calls: None,
                    tool_call_id: None,
                },
            ])),
        };
        let verifier = Arc::new(RecordingVerifier {
            calls: std::sync::Mutex::new(Vec::new()),
            verdict: Some((true, json!({"re_read": "state_matches", "frame": 7}))),
        });
        let mut brain = Brain::new(
            &provider,
            &registry,
            None,
            &audit,
            Arc::clone(&notebook),
            "turn-verifier-test",
            3,
        )
        .unwrap();
        brain.set_step_verifier(verifier.clone());
        let outcome = brain.run("search").await.expect("turn succeeds");
        assert_eq!(outcome.tools_used, vec!["notebook_search"]);
        let calls = verifier.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "notebook_search");
        assert_eq!(
            calls[0].1.get("query").and_then(Value::as_str),
            Some("nothing")
        );
        let records = audit_records(&audit_path);
        let verification = records
            .iter()
            .find(|r| r["kind"] == "verification")
            .expect("a verification record");
        assert_eq!(verification["decision"], "verified");
        assert_eq!(verification["detail"]["re_read"], "state_matches");
        assert_eq!(verification["detail"]["frame"], 7);
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(&audit_path);
    }

    #[tokio::test]
    async fn step_verifier_no_opinion_falls_back_to_status() {
        let stem = temp_stem("verifier-none");
        let db_path = format!("{stem}.db");
        let audit_path = format!("{stem}.jsonl");
        let notebook = Arc::new(Notebook::open(&db_path).unwrap());
        let audit = AuditLog::open(&audit_path).unwrap();
        let registry = crate::tools::default_registry(Arc::clone(&notebook));
        let provider = ScriptProvider {
            replies: std::sync::Mutex::new(std::collections::VecDeque::from([tool_call_message(
                "notebook_search",
                r#"{"query":"nothing"}"#,
            )])),
        };
        let verifier = Arc::new(RecordingVerifier {
            calls: std::sync::Mutex::new(Vec::new()),
            verdict: None,
        });
        let mut brain = Brain::new(
            &provider,
            &registry,
            None,
            &audit,
            Arc::clone(&notebook),
            "turn-verifier-none-test",
            3,
        )
        .unwrap();
        brain.set_step_verifier(verifier.clone());
        brain.run("search").await.expect("turn succeeds");
        assert_eq!(verifier.calls.lock().unwrap().len(), 1);
        let records = audit_records(&audit_path);
        let verification = records
            .iter()
            .find(|r| r["kind"] == "verification")
            .expect("a verification record");
        assert_eq!(verification["decision"], "verified");
        assert_eq!(verification["detail"]["reason"], "status_ok");
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(&audit_path);
    }

    #[tokio::test]
    async fn step_verifier_failure_is_recorded_not_fatal() {
        let stem = temp_stem("verifier-false");
        let db_path = format!("{stem}.db");
        let audit_path = format!("{stem}.jsonl");
        let notebook = Arc::new(Notebook::open(&db_path).unwrap());
        let audit = AuditLog::open(&audit_path).unwrap();
        let registry = crate::tools::default_registry(Arc::clone(&notebook));
        let provider = ScriptProvider {
            replies: std::sync::Mutex::new(std::collections::VecDeque::from([tool_call_message(
                "notebook_search",
                r#"{"query":"nothing"}"#,
            )])),
        };
        let verifier = Arc::new(RecordingVerifier {
            calls: std::sync::Mutex::new(Vec::new()),
            verdict: Some((false, json!({"re_read": "state_differs"}))),
        });
        let mut brain = Brain::new(
            &provider,
            &registry,
            None,
            &audit,
            Arc::clone(&notebook),
            "turn-verifier-false-test",
            3,
        )
        .unwrap();
        brain.set_step_verifier(verifier.clone());
        let outcome = brain.run("search").await.expect("turn still succeeds");
        assert_eq!(outcome.text, "Done.");
        let records = audit_records(&audit_path);
        let verification = records
            .iter()
            .find(|r| r["kind"] == "verification")
            .expect("a verification record");
        assert_eq!(verification["decision"], "failed");
        // The reflection fed back to the model carries verified=false.
        let reflection = records
            .iter()
            .find(|r| {
                r["kind"] == "reflection"
                    && r["detail"]["verified"]
                        .as_array()
                        .is_some_and(|v| !v.is_empty())
            })
            .expect("a reflection with verification results");
        assert_eq!(reflection["detail"]["verified"][0]["verified"], false);
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(&audit_path);
    }

    /// Audit write failure fails the turn closed: the brain's first
    /// reflection record cannot be written, so nothing runs. Linux-only:
    /// `/dev/full` (writes fail with ENOSPC) does not exist elsewhere.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn audit_write_failure_fails_the_turn_closed() {
        let stem = temp_stem("auditfail");
        let db_path = format!("{stem}.db");
        let err = run_with(
            ChatMessage {
                role: "assistant".to_string(),
                content: Some("An answer.".to_string()),
                tool_calls: None,
                tool_call_id: None,
            },
            "/dev/full",
            &db_path,
        )
        .await
        .expect_err("unwritable audit must fail the turn");
        assert!(err.to_string().contains("audit"), "{err}");
        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn pre_cancelled_token_aborts_before_any_provider_call() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct CountingProvider {
            calls: Arc<AtomicUsize>,
        }
        impl Provider for CountingProvider {
            fn name(&self) -> &str {
                "counting"
            }
            fn model(&self) -> &str {
                "counting-model"
            }
            fn endpoint_url(&self) -> &str {
                "http://127.0.0.1:1/v1/chat/completions"
            }
            fn is_paid(&self) -> bool {
                false
            }
            fn chat<'a>(
                &'a self,
                _ctx: &'a CallContext<'a>,
                _messages: &'a [ChatMessage],
                _tools: &'a [Value],
            ) -> BoxFuture<'a, Result<ChatMessage, MimintError>> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move {
                    Ok(ChatMessage {
                        role: "assistant".to_string(),
                        content: Some("never reached".to_string()),
                        tool_calls: None,
                        tool_call_id: None,
                    })
                })
            }
        }

        let stem = temp_stem("precancel");
        let db_path = format!("{stem}.db");
        let audit_path = format!("{stem}.jsonl");
        let notebook = Arc::new(Notebook::open(&db_path).unwrap());
        let audit = AuditLog::open(&audit_path).unwrap();
        let registry = crate::tools::default_registry(Arc::clone(&notebook));
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = CountingProvider {
            calls: Arc::clone(&calls),
        };
        let mut brain = Brain::new(
            &provider,
            &registry,
            None,
            &audit,
            Arc::clone(&notebook),
            "turn-brain-precancel",
            3,
        )
        .unwrap();
        let token = CancelToken::new();
        token.cancel();
        brain.add_cancel_token(token);
        let err = brain
            .run("hello")
            .await
            .expect_err("cancelled turn must fail");
        assert!(err.is_cancelled(), "{err}");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "provider must not be called"
        );
        let audit_text = std::fs::read_to_string(&audit_path).unwrap();
        assert!(
            audit_text.contains("\"cancelled\""),
            "cancelled turn_end must be audited: {audit_text}"
        );
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(&audit_path);
    }
}
