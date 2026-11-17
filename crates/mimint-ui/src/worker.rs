//! Worker thread: owns the tokio runtime, the provider chain, the
//! notebook, the audit log, and the [`Brain`]. The UI thread never
//! touches a provider or the brain directly; it sends requests and
//! receives events over channels.
//!
//! Rebuilds (Privacy toggle, backend switch) re-run [`build_chain`]
//! with the new policy/order and swap the whole chain: the egress
//! policy is fixed at client construction, so a rebuild is the only
//! way to change it, and the old chain is kept when the new one
//! fails to build.
//!
//! Cancellation runs on shared memory, not on the request queue:
//! the UI trips the per-turn or stop token slots directly, so a
//! cancel lands even while the worker is blocked inside a provider
//! call. The worker creates a fresh turn token per turn and hands
//! the brain both the turn token and the current stop token. A
//! cancelled turn reports [`WorkerEvent::TurnCancelled`], distinct
//! from a failure.
//!
//! Durability: a panic inside a turn is caught, reported as
//! [`WorkerEvent::WorkerFault`], and the loop keeps serving — the UI
//! must never stick on "thinking" because the worker died silently.
//! If the UI disconnects, the loop exits.
//!
//! [`Brain`]: mimint_core::Brain
//! [`build_chain`]: mimint_core::build_chain

use crate::approver::{ApprovalJob, RepaintSignal, TokenSlot, UiApprover};
use mimint_core::audit::now_ms;
use mimint_core::{
    default_registry, default_vault, AuditLog, BackendSpec, Brain, CancelToken, CostCap,
    EgressPolicy, FallbackChain, Notebook, Provider, Registry, RetryPolicy,
};
use serde_json::json;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

/// Requests from the UI thread to the worker.
#[derive(Debug)]
pub enum WorkerRequest {
    /// Run one brain turn for this prompt.
    RunTurn { prompt: String },
    /// Rebuild the provider chain with a new Privacy Mode and order.
    Rebuild {
        privacy_on: bool,
        order: Vec<String>,
    },
    /// Stop the worker thread.
    Shutdown,
}

/// Why a turn failed, classified best-effort from the error text for
/// UI wording only. The classification never changes behavior; the
/// raw error is always kept alongside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// Authentication/authorization failure (401/403, bad key).
    Auth,
    /// Blocked by the egress policy (Privacy Mode).
    PolicyBlocked,
    /// Every backend in the chain failed.
    OutOfBackends,
    /// Anything else.
    Other,
}

/// Classify an error message for display. Best-effort string
/// matching: providers surface HTTP status and policy denials in
/// their messages, and the UI only uses the kind to pick a heading.
pub fn classify_error(error: &str) -> FailureKind {
    let lower = error.to_lowercase();
    if lower.contains("privacy mode is on")
        || lower.contains("blocked by the egress policy")
        || lower.contains("egress denied")
    {
        FailureKind::PolicyBlocked
    } else if lower.contains("401")
        || lower.contains("403")
        || lower.contains("unauthorized")
        || lower.contains("authentication")
        || lower.contains("invalid api key")
        || lower.contains("incorrect api key")
    {
        FailureKind::Auth
    } else if lower.contains("all ") && lower.contains("provider(s) failed")
        || lower.contains("no backend")
        || lower.contains("no provider")
    {
        FailureKind::OutOfBackends
    } else {
        FailureKind::Other
    }
}

/// Events from the worker to the UI thread.
#[derive(Debug)]
pub enum WorkerEvent {
    /// A turn finished cleanly. `backend` is the provider that
    /// answered the last call, when known.
    TurnDone {
        text: String,
        tools_used: Vec<String>,
        partial: bool,
        backend: Option<String>,
    },
    /// A turn failed (provider error, guard error, audit failure).
    /// `kind` is the display classification; `error` is the raw text.
    TurnFailed { error: String, kind: FailureKind },
    /// The turn was cancelled (turn cancel or STOP), before or
    /// during execution. Not a failure: nothing ran that should not
    /// have, and the prompt stays available for retry.
    TurnCancelled,
    /// The chain is trying a backend (one event per attempt, in
    /// chain order, including retries). The UI shows which backend
    /// is being tried, instead of a generic spinner.
    TryingBackend { name: String },
    /// The chain was (re)built, or the attempt failed. `names` is
    /// empty when the build failed; the previous chain (if any) is
    /// kept on a failed rebuild.
    ChainRebuilt {
        names: Vec<String>,
        error: Option<String>,
    },
    /// The worker thread caught a panic inside a turn. The loop
    /// keeps serving; the UI shows the control error state.
    WorkerFault { error: String },
}

/// What a rebuild needs: the specs to build from, the retry policy,
/// and the cost cap. Absent in tests, where rebuilds are rejected.
pub struct RebuildConfig {
    pub specs: Vec<BackendSpec>,
    pub retry: RetryPolicy,
    pub cost_cap: CostCap,
}

/// Everything the worker thread owns.
pub struct WorkerDeps {
    /// Providers for the initial chain (built by the caller, so the
    /// caller decides the initial Privacy Mode and cost cap).
    pub initial_providers: Vec<Box<dyn Provider>>,
    pub retry: RetryPolicy,
    pub rebuild: Option<RebuildConfig>,
    pub notebook: Arc<Notebook>,
    pub audit: Arc<AuditLog>,
    pub max_steps: usize,
    /// Where approval jobs go (the UI thread's receiver).
    pub approval_tx: Sender<ApprovalJob>,
    /// Latching stop slot, shared with the UI (DỪNG NGAY / resume).
    pub stop_slot: Arc<TokenSlot>,
    /// Per-turn cancel slot, shared with the UI (Hủy lượt).
    pub turn_slot: Arc<TokenSlot>,
    /// Wake-up flag the approver pokes when a job arrives.
    pub repaint: Arc<RepaintSignal>,
    /// How long an approval card may wait before timing out.
    pub approval_timeout: Duration,
}

/// Handle for the UI thread: send requests, poll events, trip the
/// shared cancellation slots.
pub struct WorkerHandle {
    request_tx: Sender<WorkerRequest>,
    event_rx: Receiver<WorkerEvent>,
    stop_slot: Arc<TokenSlot>,
    turn_slot: Arc<TokenSlot>,
}

impl WorkerHandle {
    pub fn run_turn(&self, prompt: String) {
        let _ = self.request_tx.send(WorkerRequest::RunTurn { prompt });
    }

    pub fn rebuild(&self, privacy_on: bool, order: Vec<String>) {
        let _ = self
            .request_tx
            .send(WorkerRequest::Rebuild { privacy_on, order });
    }

    /// Cancel the running turn (cooperative: checked before the next
    /// provider call / tool execution). Safe to call when idle.
    pub fn cancel_turn(&self) {
        self.turn_slot.cancel_current();
    }

    /// Trip the latching emergency stop. Stays tripped until
    /// [`WorkerHandle::resume`] replaces the token.
    pub fn stop_now(&self) {
        self.stop_slot.cancel_current();
        self.turn_slot.cancel_current();
    }

    /// Clear the stop latch (the user actively re-enables). Turns
    /// already cancelled stay cancelled; new turns start fresh.
    pub fn resume(&self) {
        self.stop_slot.replace(CancelToken::new());
    }

    pub fn is_stopped(&self) -> bool {
        self.stop_slot.is_cancelled()
    }

    /// Non-blocking event poll, for the UI frame loop.
    pub fn try_recv_event(&self) -> Option<WorkerEvent> {
        self.event_rx.try_recv().ok()
    }
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        let _ = self.request_tx.send(WorkerRequest::Shutdown);
    }
}

/// Spawn the worker thread. The initial chain state is reported as
/// the first [`WorkerEvent::ChainRebuilt`].
pub fn spawn_worker(deps: WorkerDeps) -> WorkerHandle {
    let (request_tx, request_rx) = std::sync::mpsc::channel();
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let stop_slot = Arc::clone(&deps.stop_slot);
    let turn_slot = Arc::clone(&deps.turn_slot);
    std::thread::spawn(move || worker_loop(deps, request_rx, event_tx));
    WorkerHandle {
        request_tx,
        event_rx,
        stop_slot,
        turn_slot,
    }
}

/// Report every fallback attempt to the UI as [`WorkerEvent::TryingBackend`].
fn attach_progress_hook(chain: &mut FallbackChain, event_tx: &Sender<WorkerEvent>) {
    let tx = event_tx.clone();
    chain.set_progress_hook(Arc::new(move |name| {
        let _ = tx.send(WorkerEvent::TryingBackend {
            name: name.to_string(),
        });
    }));
}

fn worker_loop(
    deps: WorkerDeps,
    request_rx: Receiver<WorkerRequest>,
    event_tx: Sender<WorkerEvent>,
) {
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(err) => {
            let _ = event_tx.send(WorkerEvent::ChainRebuilt {
                names: Vec::new(),
                error: Some(format!("cannot start async runtime: {err}")),
            });
            return;
        }
    };
    let WorkerDeps {
        initial_providers,
        retry,
        rebuild,
        notebook,
        audit,
        max_steps,
        approval_tx,
        stop_slot,
        turn_slot,
        repaint,
        approval_timeout,
    } = deps;
    let registry = default_registry(Arc::clone(&notebook));
    let approver = UiApprover::new(approval_tx)
        .with_timeout(approval_timeout)
        .with_slots(Arc::clone(&stop_slot), Arc::clone(&turn_slot))
        .with_repaint(repaint);
    let mut chain = match FallbackChain::new(initial_providers, retry) {
        Ok(mut chain) => {
            attach_progress_hook(&mut chain, &event_tx);
            let _ = event_tx.send(WorkerEvent::ChainRebuilt {
                names: vec![chain.name().to_string()],
                error: None,
            });
            Some(chain)
        }
        Err(err) => {
            let _ = event_tx.send(WorkerEvent::ChainRebuilt {
                names: Vec::new(),
                error: Some(err.to_string()),
            });
            None
        }
    };

    while let Ok(request) = request_rx.recv() {
        match request {
            WorkerRequest::Shutdown => break,
            WorkerRequest::RunTurn { prompt } => {
                let ctx = TurnCtx {
                    registry: &registry,
                    approver: &approver,
                    notebook: &notebook,
                    audit: &audit,
                    max_steps,
                    stop_slot: &stop_slot,
                    turn_slot: &turn_slot,
                };
                // A panic in a turn must not kill the worker or
                // leave the UI stuck on "thinking": catch it,
                // report it, keep serving.
                let event = match catch_unwind(AssertUnwindSafe(|| {
                    run_one_turn(&runtime, chain.as_ref(), &ctx, &prompt)
                })) {
                    Ok(event) => event,
                    Err(payload) => {
                        let message = payload
                            .downcast_ref::<&str>()
                            .map(|s| s.to_string())
                            .or_else(|| payload.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "unknown panic".to_string());
                        WorkerEvent::WorkerFault {
                            error: format!("worker thread panicked during the turn: {message}"),
                        }
                    }
                };
                if event_tx.send(event).is_err() {
                    break;
                }
            }
            WorkerRequest::Rebuild { privacy_on, order } => {
                let event =
                    rebuild_chain(&mut chain, &rebuild, &audit, &event_tx, privacy_on, &order);
                if event_tx.send(event).is_err() {
                    break;
                }
            }
        }
    }
}

/// What one turn borrows from the worker thread's owned state.
struct TurnCtx<'a> {
    registry: &'a Registry,
    approver: &'a UiApprover,
    notebook: &'a Arc<Notebook>,
    audit: &'a Arc<AuditLog>,
    max_steps: usize,
    stop_slot: &'a Arc<TokenSlot>,
    turn_slot: &'a Arc<TokenSlot>,
}

fn run_one_turn(
    runtime: &tokio::runtime::Runtime,
    chain: Option<&FallbackChain>,
    ctx: &TurnCtx,
    prompt: &str,
) -> WorkerEvent {
    let Some(chain) = chain else {
        return WorkerEvent::TurnFailed {
            error: "no backend is configured: add a backend to the config file or start Ollama/LM Studio, then restart the app".to_string(),
            kind: FailureKind::OutOfBackends,
        };
    };
    // Fresh turn token: the UI cancels THIS turn through the slot,
    // and the approver watches the same slot while a card waits.
    let turn_token = CancelToken::new();
    ctx.turn_slot.replace(turn_token.clone());
    let turn_id = format!("ui-turn-{}-{}", std::process::id(), now_ms());
    let mut brain = match Brain::new(
        chain,
        ctx.registry,
        Some(ctx.approver),
        ctx.audit,
        Arc::clone(ctx.notebook),
        turn_id,
        ctx.max_steps,
    ) {
        Ok(brain) => brain,
        Err(err) => {
            return WorkerEvent::TurnFailed {
                error: err.to_string(),
                kind: classify_error(&err.to_string()),
            };
        }
    };
    brain.add_cancel_token(turn_token);
    brain.add_cancel_token(ctx.stop_slot.get());
    match runtime.block_on(brain.run(prompt)) {
        Ok(outcome) => WorkerEvent::TurnDone {
            text: outcome.text,
            tools_used: outcome.tools_used,
            partial: outcome.partial,
            backend: chain.last_used_provider(),
        },
        Err(err) if err.is_cancelled() => WorkerEvent::TurnCancelled,
        Err(err) => {
            let text = err.to_string();
            WorkerEvent::TurnFailed {
                kind: classify_error(&text),
                error: text,
            }
        }
    }
}

fn rebuild_chain(
    chain: &mut Option<FallbackChain>,
    rebuild: &Option<RebuildConfig>,
    audit: &Arc<AuditLog>,
    event_tx: &Sender<WorkerEvent>,
    privacy_on: bool,
    order: &[String],
) -> WorkerEvent {
    let Some(rebuild) = rebuild else {
        return WorkerEvent::ChainRebuilt {
            names: Vec::new(),
            error: Some("rebuild is not supported in this configuration".to_string()),
        };
    };
    // Record the mode change before rebuilding: the audit trail must
    // show every Privacy Mode change, including failed rebuilds.
    // A failed audit write fails the rebuild (fail-closed).
    if let Err(err) = audit.log(
        "ui-config",
        "privacy_mode",
        None,
        Some(if privacy_on { "on" } else { "off" }),
        json!({
            "enabled": privacy_on,
            "source": "ui toggle",
            "order": order,
        }),
    ) {
        return WorkerEvent::ChainRebuilt {
            names: Vec::new(),
            error: Some(format!("cannot record the Privacy Mode change: {err}")),
        };
    }
    let policy = EgressPolicy::new(privacy_on);
    let vault = default_vault();
    match mimint_core::build_chain(&rebuild.specs, order, &vault, policy, rebuild.cost_cap) {
        Ok(providers) => match FallbackChain::new(providers, rebuild.retry) {
            Ok(mut new_chain) => {
                attach_progress_hook(&mut new_chain, event_tx);
                let names = vec![new_chain.name().to_string()];
                *chain = Some(new_chain);
                WorkerEvent::ChainRebuilt { names, error: None }
            }
            Err(err) => WorkerEvent::ChainRebuilt {
                names: Vec::new(),
                error: Some(err.to_string()),
            },
        },
        Err(err) => WorkerEvent::ChainRebuilt {
            names: Vec::new(),
            error: Some(err.to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approver::approval_channel;
    use mimint_core::{
        ApprovalDecision, BoxFuture, CallContext, ChatMessage, FunctionCall, MimintError, ToolCall,
    };
    use std::time::Duration;

    /// Provider that answers with a fixed script of messages, one per
    /// call, then repeats the last one.
    struct FakeProvider {
        script: std::sync::Mutex<Vec<ChatMessage>>,
    }

    impl FakeProvider {
        fn answering(text: &str) -> Self {
            Self {
                script: std::sync::Mutex::new(vec![ChatMessage {
                    role: "assistant".to_string(),
                    content: Some(text.to_string()),
                    tool_calls: None,
                    tool_call_id: None,
                }]),
            }
        }

        fn tool_then_text(tool: &str, args: &str, text: &str) -> Self {
            Self {
                script: std::sync::Mutex::new(vec![
                    ChatMessage {
                        role: "assistant".to_string(),
                        content: None,
                        tool_calls: Some(vec![ToolCall {
                            id: "call-1".to_string(),
                            kind: "function".to_string(),
                            function: FunctionCall {
                                name: tool.to_string(),
                                arguments: args.to_string(),
                            },
                        }]),
                        tool_call_id: None,
                    },
                    ChatMessage {
                        role: "assistant".to_string(),
                        content: Some(text.to_string()),
                        tool_calls: None,
                        tool_call_id: None,
                    },
                ]),
            }
        }
    }

    impl Provider for FakeProvider {
        fn name(&self) -> &str {
            "fake"
        }

        fn model(&self) -> &str {
            "fake-model"
        }

        fn endpoint_url(&self) -> &str {
            "http://127.0.0.1:9/v1"
        }

        fn is_paid(&self) -> bool {
            false
        }

        fn chat<'a>(
            &'a self,
            _ctx: &'a CallContext<'a>,
            _messages: &'a [ChatMessage],
            _tools: &'a [serde_json::Value],
        ) -> BoxFuture<'a, Result<ChatMessage, MimintError>> {
            let mut script = self.script.lock().unwrap();
            let message = if script.len() > 1 {
                script.remove(0)
            } else {
                script[0].clone()
            };
            Box::pin(async move { Ok(message) })
        }
    }

    fn temp_paths(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        // Unique per call: tests run in parallel in one process, so
        // the pid + tag alone would collide and the tests would
        // delete each other's database mid-turn.
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("mimint-worker-{}-{tag}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        (dir.join("notes.db"), dir.join("audit.jsonl"))
    }

    fn spawn_fake(
        provider: FakeProvider,
    ) -> (WorkerHandle, Receiver<ApprovalJob>, std::path::PathBuf) {
        let (db_path, audit_path) = temp_paths("fake");
        let dir = db_path.parent().unwrap().to_path_buf();
        let (approval_tx, approval_rx) = approval_channel();
        let deps = WorkerDeps {
            initial_providers: vec![Box::new(provider)],
            retry: RetryPolicy::default(),
            rebuild: None,
            notebook: Arc::new(Notebook::open(&db_path).unwrap()),
            audit: Arc::new(AuditLog::open(&audit_path).unwrap()),
            max_steps: 5,
            approval_tx,
            stop_slot: TokenSlot::new(),
            turn_slot: TokenSlot::new(),
            repaint: RepaintSignal::new(),
            approval_timeout: Duration::from_secs(120),
        };
        (spawn_worker(deps), approval_rx, dir)
    }

    fn wait_event(handle: &WorkerHandle) -> WorkerEvent {
        // The handle's receiver is private; poll through try_recv.
        for _ in 0..200 {
            if let Some(event) = handle.try_recv_event() {
                return event;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("no worker event within 2s");
    }

    /// Drain TryingBackend events, returning the first non-progress event.
    fn wait_terminal(handle: &WorkerHandle) -> WorkerEvent {
        loop {
            match wait_event(handle) {
                WorkerEvent::TryingBackend { .. } => continue,
                other => return other,
            }
        }
    }

    #[test]
    fn turn_with_plain_answer_completes() {
        let (handle, _approvals, dir) = spawn_fake(FakeProvider::answering("hello back"));
        let first = wait_event(&handle);
        assert!(matches!(
            first,
            WorkerEvent::ChainRebuilt { error: None, .. }
        ));
        handle.run_turn("hi".to_string());
        match wait_terminal(&handle) {
            WorkerEvent::TurnDone {
                text,
                tools_used,
                partial,
                backend,
            } => {
                assert_eq!(text, "hello back");
                assert!(tools_used.is_empty());
                assert!(!partial);
                assert_eq!(backend.as_deref(), Some("fake"));
            }
            other => panic!("unexpected event: {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn trying_backend_is_reported_before_the_answer() {
        let (handle, _approvals, dir) = spawn_fake(FakeProvider::answering("hi"));
        let _ = wait_event(&handle);
        handle.run_turn("hi".to_string());
        match wait_event(&handle) {
            WorkerEvent::TryingBackend { name } => assert_eq!(name, "fake"),
            other => panic!("expected TryingBackend first, got: {other:?}"),
        }
        assert!(matches!(
            wait_terminal(&handle),
            WorkerEvent::TurnDone { .. }
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn consequential_tool_waits_for_the_ui_decision() {
        let (handle, approvals, dir) = spawn_fake(FakeProvider::tool_then_text(
            "notebook_add",
            r#"{"title":"t","body":"b"}"#,
            "saved",
        ));
        let _ = wait_event(&handle);
        handle.run_turn("remember this".to_string());
        // The worker must ask before the tool runs.
        let job = approvals
            .recv_timeout(Duration::from_secs(5))
            .expect("approval job must arrive");
        assert_eq!(job.tool, "notebook_add");
        job.reply.send(ApprovalDecision::Allow).unwrap();
        match wait_terminal(&handle) {
            WorkerEvent::TurnDone {
                text, tools_used, ..
            } => {
                assert_eq!(text, "saved");
                assert_eq!(tools_used, vec!["notebook_add"]);
            }
            other => panic!("unexpected event: {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn denied_tool_still_finishes_the_turn() {
        let (handle, approvals, dir) = spawn_fake(FakeProvider::tool_then_text(
            "notebook_add",
            r#"{"title":"t","body":"b"}"#,
            "ok, not saved",
        ));
        let _ = wait_event(&handle);
        handle.run_turn("remember this".to_string());
        let job = approvals
            .recv_timeout(Duration::from_secs(5))
            .expect("approval job must arrive");
        job.reply.send(ApprovalDecision::Deny).unwrap();
        match wait_terminal(&handle) {
            WorkerEvent::TurnDone { text, .. } => {
                assert_eq!(text, "ok, not saved");
            }
            other => panic!("unexpected event: {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stop_before_a_turn_cancels_it_distinctly() {
        let (handle, _approvals, dir) = spawn_fake(FakeProvider::answering("never"));
        let _ = wait_event(&handle);
        handle.stop_now();
        assert!(handle.is_stopped());
        handle.run_turn("hi".to_string());
        match wait_terminal(&handle) {
            WorkerEvent::TurnCancelled => {}
            other => panic!("expected TurnCancelled, got: {other:?}"),
        }
        handle.resume();
        assert!(!handle.is_stopped());
        handle.run_turn("hi".to_string());
        match wait_terminal(&handle) {
            WorkerEvent::TurnDone { text, .. } => assert_eq!(text, "never"),
            other => panic!("expected TurnDone after resume, got: {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn classify_error_picks_the_display_kind() {
        assert_eq!(
            classify_error("request blocked: Privacy Mode is ON"),
            FailureKind::PolicyBlocked
        );
        assert_eq!(
            classify_error("provider returned 401 Unauthorized"),
            FailureKind::Auth
        );
        assert_eq!(
            classify_error("all 2 provider(s) failed: boom"),
            FailureKind::OutOfBackends
        );
        assert_eq!(classify_error("weird"), FailureKind::Other);
    }
}
