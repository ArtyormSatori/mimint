//! Hierarchy overhead measurement (P8.5).
//!
//! Restores the Phase 6 measurement method (`p6_measure.rs`, deleted from
//! the tree before Phase 6 closed; only its JSONL logs survived) as a
//! runnable binary in the existing harness, per F6-10 / D-DESIGN-5.
//!
//! Method (one JSON line per run, Phase 6 schema):
//! - Same task, same provider, same machine; only the hierarchy flag
//!   differs. Flat = hierarchy off (parent does the work). Delegated =
//!   hierarchy on (parent delegates, child does the work).
//! - Stub task: write 2 notebook notes. Flat parent script =
//!   [notebook_add, notebook_add, "done"] (3 attempts, 360 tokens).
//!   Delegated: parent [delegate, "parent done"] + child
//!   [notebook_add, notebook_add, "child done"] (5 attempts, 600
//!   tokens). Each scripted reply carries usage 100 prompt + 20
//!   completion = 120 tokens, exactly matching the Phase 6 counts.
//!   (Phase 6's exact script is lost with `p6_measure.rs`; the
//!   attempt/token counts and metric definitions match.)
//! - Ollama task (max_steps=1, child_steps=1, as in Phase 6):
//!   one short question, parent delegates when it has the tool.
//! - Metrics: wall_ms (run_turn -> terminal event), time to first
//!   provider attempt (audit turn_start -> first provider_attempt),
//!   queued_ms (execution records), tokens_charged (sum of
//!   provider_attempt charged_tokens), outcome (done/failed/
//!   cancelled/timeout). >= 3 runs per cell; report median + range.
//!
//! Usage:
//!   hierarchy_measure --provider stub|ollama --mode flat|delegated
//!     --runs 3 --out measurements.jsonl [--max-steps N]
//!     [--child-steps N] [--prompt "..."] [--timeout-secs N]

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mimint_core::audit::AuditLog;
use mimint_core::egress::{EgressPolicy, GuardedHttpClient};
use mimint_core::guards::ApprovalDecision;
use mimint_core::notebook::Notebook;
use mimint_core::provider::{
    BoxFuture, CallContext, ChatMessage, FunctionCall, Provider, ProviderClient, ToolCall,
};
use mimint_core::registry::CostCap;
use mimint_core::{
    AgentsConfig, HierarchyConfig, MimintError, ProfileSet, RetryPolicy, RoutingConfig,
    SandboxConfig, TokenUsage,
};
use mimint_ui::approver::{approval_channel, RepaintSignal, TokenSlot};
use mimint_ui::worker::{spawn_worker, WorkerDeps, WorkerEvent, WorkerHandle};

const STUB_USAGE_PROMPT: u64 = 100;
const STUB_USAGE_COMPLETION: u64 = 20;

fn text_msg(text: &str) -> ChatMessage {
    ChatMessage {
        role: "assistant".to_string(),
        content: Some(text.to_string()),
        tool_calls: None,
        tool_call_id: None,
        usage: Some(TokenUsage {
            prompt_tokens: STUB_USAGE_PROMPT,
            completion_tokens: STUB_USAGE_COMPLETION,
        }),
    }
}

fn tool_msg(tool: &str, args: &str) -> ChatMessage {
    ChatMessage {
        role: "assistant".to_string(),
        content: None,
        tool_calls: Some(vec![ToolCall {
            id: format!("call-{tool}-{}", unique_suffix()),
            kind: "function".to_string(),
            function: FunctionCall {
                name: tool.to_string(),
                arguments: args.to_string(),
            },
        }]),
        tool_call_id: None,
        usage: Some(TokenUsage {
            prompt_tokens: STUB_USAGE_PROMPT,
            completion_tokens: STUB_USAGE_COMPLETION,
        }),
    }
}

fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{nanos:09}")
}

/// Scripted provider: per-agent reply queues, instant replies.
/// Mirrors the `ScriptedByAgent` test helper in worker.rs tests.
struct ScriptedByAgent {
    scripts: Mutex<HashMap<String, VecDeque<ChatMessage>>>,
    repeat: Mutex<HashMap<String, ChatMessage>>,
}

impl ScriptedByAgent {
    fn new(scripts: Vec<(&str, Vec<ChatMessage>)>) -> Self {
        Self {
            scripts: Mutex::new(
                scripts
                    .into_iter()
                    .map(|(agent, script)| (agent.to_string(), script.into_iter().collect()))
                    .collect(),
            ),
            repeat: Mutex::new(HashMap::new()),
        }
    }
}

impl Provider for ScriptedByAgent {
    fn name(&self) -> &str {
        "scripted-by-agent"
    }

    fn model(&self) -> &str {
        "scripted-model"
    }

    fn endpoint_url(&self) -> &str {
        "http://127.0.0.1:9/v1"
    }

    fn is_paid(&self) -> bool {
        false
    }

    fn chat<'a>(
        &'a self,
        ctx: &'a CallContext<'a>,
        _messages: &'a [ChatMessage],
        _tools: &'a [serde_json::Value],
    ) -> BoxFuture<'a, Result<ChatMessage, MimintError>> {
        let agent = ctx.agent_id.to_string();
        let mut scripts = self.scripts.lock().unwrap();
        let reply = scripts
            .get_mut(&agent)
            .and_then(|script| script.pop_front())
            .or_else(|| self.repeat.lock().unwrap().get(&agent).cloned())
            .unwrap_or_else(|| text_msg("done"));
        self.repeat
            .lock()
            .unwrap()
            .insert(agent.clone(), reply.clone());
        drop(scripts);
        Box::pin(async move { Ok(reply) })
    }
}

fn stub_provider(mode: &str) -> Box<dyn Provider> {
    let add1 = tool_msg("notebook_add", r#"{"title":"n1","body":"a"}"#);
    let add2 = tool_msg("notebook_add", r#"{"title":"n2","body":"b"}"#);
    let scripts = if mode == "delegated" {
        vec![
            (
                "agent-001",
                vec![
                    tool_msg(
                        "delegate",
                        r#"{"task":"ghi 2 ghi chú vào notebook: n1/a và n2/b"}"#,
                    ),
                    text_msg("parent done"),
                ],
            ),
            ("agent-002", vec![add1, add2, text_msg("child done")]),
        ]
    } else {
        vec![("agent-001", vec![add1, add2, text_msg("done")])]
    };
    Box::new(ScriptedByAgent::new(scripts))
}

fn ollama_provider() -> Result<Box<dyn Provider>, MimintError> {
    let http = GuardedHttpClient::new(EgressPolicy::privacy_on(), Duration::from_secs(600))?;
    let client = ProviderClient::new("http://127.0.0.1:11434/v1", "qwen2.5:3b", None, http)?;
    Ok(Box::new(client))
}

struct RunMetrics {
    wall_ms: u64,
    time_to_first_action_ms: Option<u64>,
    queued_ms: Vec<u64>,
    tokens_charged: u64,
    provider_attempts: usize,
    agents_seen: Vec<String>,
    outcome: String,
    text: String,
    attempt_gaps_ms: Vec<u64>,
    turn_tail_ms: Option<u64>,
}

fn epoch_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// What the audit says about one run: (time_to_first_action_ms,
/// queued_ms, tokens, attempts, agents, attempt_gap_ms, turn_tail_ms).
///
/// A named struct (not a 7-tuple): clippy `type_complexity` fires on
/// the tuple form under `-D warnings`.
struct AuditMetrics {
    ttfa_ms: Option<u64>,
    queued_ms: Vec<u64>,
    tokens_charged: u64,
    provider_attempts: usize,
    agents_seen: Vec<String>,
    attempt_gaps_ms: Vec<u64>,
    turn_tail_ms: Option<u64>,
}

/// (time_to_first_action_ms, queued_ms, tokens, attempts, agents,
/// attempt_gap_ms, turn_tail_ms)
//
// ttfa is measured from the harness's own turn-start epoch (no
// `turn_start` record is ever written to the audit by the
// brain/worker path — Phase 6's harness did the same). attempt gaps
// are the deltas between consecutive `provider_attempt` timestamps;
// the tail is turn_end ts minus the last attempt ts.
fn audit_metrics(audit_path: &Path, turn_start_ms: u64) -> AuditMetrics {
    let text = std::fs::read_to_string(audit_path).unwrap_or_default();
    let mut first_attempt: Option<u64> = None;
    let mut attempt_ts: Vec<u64> = Vec::new();
    let mut turn_end: Option<u64> = None;
    let mut queued: Vec<u64> = Vec::new();
    let mut tokens: u64 = 0;
    let mut attempts: usize = 0;
    let mut agents: Vec<String> = Vec::new();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let kind = v.get("kind").and_then(|k| k.as_str()).unwrap_or("");
        let ts = v.get("ts_ms").and_then(|t| t.as_u64());
        match kind {
            "provider_attempt" => {
                attempts += 1;
                if first_attempt.is_none() {
                    first_attempt = ts;
                }
                if let Some(t) = ts {
                    attempt_ts.push(t);
                }
                if let Some(t) = v
                    .get("detail")
                    .and_then(|d| d.get("charged_tokens"))
                    .and_then(|t| t.as_u64())
                {
                    tokens += t;
                }
                if let Some(a) = v.get("agent_id").and_then(|a| a.as_str()) {
                    if !agents.contains(&a.to_string()) {
                        agents.push(a.to_string());
                    }
                }
            }
            "turn_end" => {
                if turn_end.is_none() {
                    turn_end = ts;
                }
            }
            "execution" => {
                if let Some(q) = v
                    .get("detail")
                    .and_then(|d| d.get("queued_ms"))
                    .and_then(|q| q.as_u64())
                {
                    queued.push(q);
                }
            }
            _ => {}
        }
    }
    let ttfa = first_attempt.map(|f| f.saturating_sub(turn_start_ms));
    let gaps: Vec<u64> = attempt_ts
        .windows(2)
        .map(|w| w[1].saturating_sub(w[0]))
        .collect();
    let tail = match (turn_end, attempt_ts.last()) {
        (Some(e), Some(l)) => Some(e.saturating_sub(*l)),
        _ => None,
    };
    AuditMetrics {
        ttfa_ms: ttfa,
        queued_ms: queued,
        tokens_charged: tokens,
        provider_attempts: attempts,
        agents_seen: agents,
        attempt_gaps_ms: gaps,
        turn_tail_ms: tail,
    }
}

fn wait_terminal(handle: &WorkerHandle, timeout: Duration) -> (String, String, Duration) {
    let started = Instant::now();
    loop {
        if started.elapsed() > timeout {
            return ("timeout".to_string(), String::new(), started.elapsed());
        }
        // Drain ALL queued events per iteration: sleeping 10ms
        // after every single event inflates wall_ms by ~10ms per
        // event (flat: 12 events -> +120ms; delegated: 20 -> +200ms)
        // and fabricates a hierarchy "overhead" that is purely
        // harness polling. Sleep only when the queue is empty.
        let mut saw_any = false;
        while let Some(event) = handle.try_recv_event() {
            saw_any = true;
            let label = match &event {
                WorkerEvent::TurnDone { .. } => "TurnDone",
                WorkerEvent::TurnFailed { .. } => "TurnFailed",
                WorkerEvent::TurnCancelled { .. } => "TurnCancelled",
                WorkerEvent::TryingBackend { .. } => "TryingBackend",
                WorkerEvent::TurnProgress { .. } => "TurnProgress",
                WorkerEvent::ChainRebuilt { .. } => "ChainRebuilt",
                WorkerEvent::AgentEnded { .. } => "AgentEnded",
                _ => "other",
            };
            println!("  [event +{}ms] {label}", started.elapsed().as_millis());
            match event {
                WorkerEvent::TurnDone { text, .. } => {
                    return ("done".to_string(), text, started.elapsed());
                }
                WorkerEvent::TurnFailed { error, .. } => {
                    return ("failed".to_string(), error, started.elapsed());
                }
                WorkerEvent::TurnCancelled { .. } => {
                    return ("cancelled".to_string(), String::new(), started.elapsed());
                }
                _ => {}
            }
        }
        if !saw_any {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

fn run_once(
    provider: &str,
    mode: &str,
    prompt: &str,
    max_steps: usize,
    child_steps: usize,
    timeout_secs: u64,
    run_dir: &Path,
) -> RunMetrics {
    let _ = std::fs::create_dir_all(run_dir);
    let db_path = run_dir.join("notes.db");
    let audit_path = run_dir.join("audit.jsonl");
    let (approval_tx, approval_rx) = approval_channel();
    // Auto-allow every approval card, as the worker tests do.
    std::thread::spawn(move || {
        while let Ok(job) = approval_rx.recv() {
            let _ = job.reply.send(ApprovalDecision::Allow);
        }
    });
    let profiles = ProfileSet::resolve(&[], None, &[], &[], true, CostCap { cap_usd: None });
    let active_profile = profiles.default_profile().name.clone();
    let initial_providers: Vec<Box<dyn Provider>> = if provider == "stub" {
        vec![stub_provider(mode)]
    } else {
        vec![ollama_provider().expect("Ollama provider must construct")]
    };
    let (agents, hierarchy) = if mode == "delegated" {
        (
            AgentsConfig {
                parallelism_enabled: true,
                ..Default::default()
            },
            HierarchyConfig {
                enabled: true,
                max_children: 2,
                default_child_steps: child_steps,
                default_child_time_secs: 300,
            },
        )
    } else {
        (AgentsConfig::default(), HierarchyConfig::default())
    };
    let deps = WorkerDeps {
        initial_providers,
        retry: RetryPolicy::default(),
        rebuild: None,
        notebook: Arc::new(Notebook::open(&db_path).expect("notebook opens")),
        audit: Arc::new(AuditLog::open(&audit_path).expect("audit opens")),
        max_steps,
        approval_tx,
        stop_slot: TokenSlot::new(),
        turn_slot: TokenSlot::new(),
        repaint: RepaintSignal::new(),
        approval_timeout: Duration::from_secs(60),
        max_observation_age: Duration::from_secs(300),
        body: None,
        approval_pending: None,
        privacy_on: true,
        profiles,
        active_profile,
        soul_store: None,
        skill_store: None,
        experience: None,
        agents,
        hierarchy,
        sandbox: SandboxConfig::default(),
        routing: RoutingConfig::default(),
        mcp_servers: Vec::new(),
        body_factory: None,
        stop_flag_cell: None,
    };
    let handle = spawn_worker(deps);
    // Wait for the worker to finish startup (chain build etc.)
    // before sending the turn: a turn sent while the worker is
    // still starting would sit queued, and that queue wait would
    // be misread as turn latency. Startup sleep is 3s — generous
    // on vega, and excluded from wall_ms either way.
    std::thread::sleep(Duration::from_millis(3000));
    let started = Instant::now();
    let turn_start_ms = epoch_ms_now();
    handle.run_turn(prompt.to_string());
    let (outcome, text, elapsed) = wait_terminal(&handle, Duration::from_secs(timeout_secs));
    let _ = started;
    let wall_ms = elapsed.as_millis() as u64;
    let m = audit_metrics(&audit_path, turn_start_ms);
    RunMetrics {
        wall_ms,
        time_to_first_action_ms: m.ttfa_ms,
        queued_ms: m.queued_ms,
        tokens_charged: m.tokens_charged,
        provider_attempts: m.provider_attempts,
        agents_seen: m.agents_seen,
        outcome,
        text: text.chars().take(200).collect(),
        attempt_gaps_ms: m.attempt_gaps_ms,
        turn_tail_ms: m.turn_tail_ms,
    }
}

fn print_usage() -> ! {
    eprintln!(
        "usage: hierarchy_measure --provider stub|ollama --mode flat|delegated \
         --runs N --out FILE [--max-steps N] [--child-steps N] [--prompt S] [--timeout-secs N]"
    );
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut provider = String::new();
    let mut mode = String::new();
    let mut runs: usize = 3;
    let mut out = String::new();
    let mut max_steps: usize = 5;
    let mut child_steps: usize = 10;
    let mut prompt: Option<String> = None;
    let mut timeout_secs: u64 = 60;
    let mut i = 1;
    while i < args.len() {
        let key = args[i].as_str();
        let value = args.get(i + 1).cloned().unwrap_or_else(|| print_usage());
        match key {
            "--provider" => provider = value,
            "--mode" => mode = value,
            "--runs" => runs = value.parse().unwrap_or_else(|_| print_usage()),
            "--out" => out = value,
            "--max-steps" => max_steps = value.parse().unwrap_or_else(|_| print_usage()),
            "--child-steps" => child_steps = value.parse().unwrap_or_else(|_| print_usage()),
            "--prompt" => prompt = Some(value),
            "--timeout-secs" => timeout_secs = value.parse().unwrap_or_else(|_| print_usage()),
            _ => print_usage(),
        }
        i += 2;
    }
    if (provider != "stub" && provider != "ollama")
        || (mode != "flat" && mode != "delegated")
        || out.is_empty()
    {
        print_usage();
    }
    if provider == "ollama" && timeout_secs < 300 {
        timeout_secs = 660;
    }
    let default_prompt = if provider == "stub" {
        "ghi 2 ghi chú vào notebook".to_string()
    } else {
        "Trả lời ngắn gọn: thủ đô của Việt Nam là gì? Nếu bạn có công cụ delegate, hãy giao cho một agent con trả lời rồi báo lại kết quả.".to_string()
    };
    let prompt = prompt.unwrap_or(default_prompt);
    let out_path = PathBuf::from(&out);
    let mut out_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&out_path)
        .expect("out file opens");
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    for run in 1..=runs {
        let run_dir =
            std::env::temp_dir().join(format!("p85-measure-{provider}-{mode}-{run}-{stamp}"));
        let m = run_once(
            &provider,
            &mode,
            &prompt,
            max_steps,
            child_steps,
            timeout_secs,
            &run_dir,
        );
        let line = serde_json::json!({
            "measurement": "hierarchy",
            "mode": mode,
            "provider": provider,
            "run": run,
            "wall_ms": m.wall_ms,
            "time_to_first_action_ms": m.time_to_first_action_ms,
            "queued_ms": m.queued_ms,
            "tokens_charged": m.tokens_charged,
            "provider_attempts": m.provider_attempts,
            "agents_seen": m.agents_seen,
            "outcome": m.outcome,
            "text": m.text,
            "attempt_gaps_ms": m.attempt_gaps_ms,
            "turn_tail_ms": m.turn_tail_ms,
            "max_steps": max_steps,
            "child_steps": child_steps,
        });
        writeln!(out_file, "{}", serde_json::to_string(&line).unwrap()).unwrap();
        out_file.flush().unwrap();
        println!(
            "run {run}/{runs} {provider}/{mode}: outcome={} wall_ms={} ttfa_ms={:?} tokens={} attempts={} gaps_ms={:?} tail_ms={:?} agents={:?} text={:?}",
            m.outcome,
            m.wall_ms,
            m.time_to_first_action_ms,
            m.tokens_charged,
            m.provider_attempts,
            m.attempt_gaps_ms,
            m.turn_tail_ms,
            m.agents_seen,
            m.text,
        );
        let _ = std::fs::remove_dir_all(&run_dir);
    }
}
