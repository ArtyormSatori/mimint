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
//! [`Brain`]: mimint_core::Brain
//! [`build_chain`]: mimint_core::build_chain

use crate::approver::{ApprovalJob, UiApprover};
use mimint_core::audit::now_ms;
use mimint_core::{
    default_registry, default_vault, AuditLog, BackendSpec, Brain, CostCap, EgressPolicy,
    FallbackChain, Notebook, Provider, Registry, RetryPolicy,
};
use serde_json::json;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;

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

/// Events from the worker to the UI thread.
#[derive(Debug)]
pub enum WorkerEvent {
    /// A turn finished cleanly.
    TurnDone {
        text: String,
        tools_used: Vec<String>,
        partial: bool,
    },
    /// A turn failed (provider error, guard error, audit failure).
    TurnFailed { error: String },
    /// The chain was (re)built, or the attempt failed. `names` is
    /// empty when the build failed; the previous chain (if any) is
    /// kept on a failed rebuild.
    ChainRebuilt {
        names: Vec<String>,
        error: Option<String>,
    },
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
}

/// Handle for the UI thread: send requests, poll events.
pub struct WorkerHandle {
    request_tx: Sender<WorkerRequest>,
    event_rx: Receiver<WorkerEvent>,
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
    std::thread::spawn(move || worker_loop(deps, request_rx, event_tx));
    WorkerHandle {
        request_tx,
        event_rx,
    }
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
    } = deps;
    let registry = default_registry(Arc::clone(&notebook));
    let approver = UiApprover::new(approval_tx);
    let mut chain = match FallbackChain::new(initial_providers, retry) {
        Ok(chain) => {
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
                };
                let event = run_one_turn(&runtime, chain.as_ref(), &ctx, &prompt);
                if event_tx.send(event).is_err() {
                    break;
                }
            }
            WorkerRequest::Rebuild { privacy_on, order } => {
                let event = rebuild_chain(&mut chain, &rebuild, &audit, privacy_on, &order);
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
        };
    };
    let turn_id = format!("ui-turn-{}-{}", std::process::id(), now_ms());
    let brain = match Brain::new(
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
            }
        }
    };
    match runtime.block_on(brain.run(prompt)) {
        Ok(outcome) => WorkerEvent::TurnDone {
            text: outcome.text,
            tools_used: outcome.tools_used,
            partial: outcome.partial,
        },
        Err(err) => WorkerEvent::TurnFailed {
            error: err.to_string(),
        },
    }
}

fn rebuild_chain(
    chain: &mut Option<FallbackChain>,
    rebuild: &Option<RebuildConfig>,
    audit: &Arc<AuditLog>,
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
            Ok(new_chain) => {
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
    use mimint_core::{BoxFuture, CallContext, ChatMessage, FunctionCall, MimintError, ToolCall};
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

    #[test]
    fn turn_with_plain_answer_completes() {
        let (handle, _approvals, dir) = spawn_fake(FakeProvider::answering("hello back"));
        let first = wait_event(&handle);
        assert!(matches!(
            first,
            WorkerEvent::ChainRebuilt { error: None, .. }
        ));
        handle.run_turn("hi".to_string());
        match wait_event(&handle) {
            WorkerEvent::TurnDone {
                text,
                tools_used,
                partial,
            } => {
                assert_eq!(text, "hello back");
                assert!(tools_used.is_empty());
                assert!(!partial);
            }
            other => panic!("unexpected event: {other:?}"),
        }
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
        job.reply.send(true).unwrap();
        match wait_event(&handle) {
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
        job.reply.send(false).unwrap();
        match wait_event(&handle) {
            WorkerEvent::TurnDone { text, .. } => {
                assert_eq!(text, "ok, not saved");
            }
            other => panic!("unexpected event: {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
