//! Worker thread: owns the tokio runtime, the provider chain, the
//! notebook, the audit log, and the agent coordinator. The UI
//! thread never touches a provider or a brain directly; it sends
//! requests and receives events over channels.
//!
//! Phase 6: the worker runs agents, not one turn at a time. Each
//! agent runs as one real tokio task on a multi-thread runtime
//! (`max_agents + 2` threads); the single [`BodyActor`] serializes
//! body actions round-robin. The loop itself stays single-threaded:
//! every agent table mutation happens here, driven by the merged
//! request channel — agent completions arrive as
//! [`WorkerRequest::AgentFinished`] messages on the same channel
//! as UI requests, so no lock guards the table.
//!
//! Rebuilds (Privacy toggle, backend switch) re-run [`build_chain`]
//! with the new policy/order and swap the whole chain: the egress
//! policy is fixed at client construction, so a rebuild is the only
//! way to change it, and the old chain is kept when the new one
//! fails to build. A rebuild never disturbs a running turn: the
//! turn holds the old chain Arc and its own [`TurnRuntime`]
//! snapshot.
//!
//! Cancellation runs on shared memory AND on push-cancel: the UI
//! trips the per-turn or stop token slots directly, so a cancel
//! lands even while the worker is blocked; the loop additionally
//! pushes the cancel into the actor and the approver, aborts the
//! task, and joins it within 5 seconds (§10). A cancelled turn
//! reports [`WorkerEvent::TurnCancelled`], distinct from a failure.
//!
//! Durability: a panic inside a turn is caught by the task's
//! [`CatchUnwind`] wrapper, reported as a single-agent fault, and
//! the loop keeps serving — the UI must never stick on "thinking"
//! because a task died silently. A panic in the loop itself is
//! caught per request and reported as [`WorkerEvent::WorkerFault`].
//! If the UI disconnects, the loop exits.
//!
//! [`Brain`]: mimint_core::Brain
//! [`build_chain`]: mimint_core::build_chain
//! [`CatchUnwind`]: crate::coordinator::CatchUnwind
//! [`TurnRuntime`]: crate::coordinator::TurnRuntime

use crate::approver::{ApprovalJob, RepaintSignal, TokenSlot, UiApprover};
use crate::coordinator::{
    audit_failure_scope, is_body_fault, panic_message, AgentCoordinator, AgentEntry, AgentState,
    CancelOutcome, CatchUnwind, ChildOutcome, ChildResult, ChildSpec, EndVerification, TaskOutcome,
    TurnRuntime,
};
use crate::delegate::DelegateTool;
use mimint_body::{Body, StopFlag, StopFlagCell};
use mimint_control::{
    register_body_tools_for_agent, ActorConfig, AgentSnapshotSlots, BodyActor, BodyStepVerifier,
    FrameStore, PrivacyFlag,
};
use mimint_core::audit::now_ms;
use mimint_core::{
    build_system_prompt, default_greeting, default_registry_for, default_soul_text, default_vault,
    find_trigger_conflicts, soul_content_hash, soul_may_be_sent, url_is_loopback, AgentsConfig,
    Approver, AuditLog, BackendSpec, Brain, BrainProgress, CancelReport, CancelToken,
    ChildProcessRegistry, CostCap, CostLedger, EgressPolicy, ExecuteCodeTool, ExperienceStore,
    FallbackChain, HierarchyConfig, KillLedger, McpManager, McpServerConfig, MimintError, Notebook,
    Profile, ProfileSet, Provider, RetryPolicy, RoutingConfig, RunDirTracker, Sandbox,
    SandboxConfig, SkillDef, SkillStore, SoulDoc, SoulStore, VerificationKind, VerificationStatus,
    AGENT_DEFAULT, DEFAULT_MAX_TOKENS, SOUL_FORMAT_VERSION,
};
use serde_json::json;
use std::collections::{HashMap, VecDeque};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Requests from the UI thread (and from agent tasks) to the worker.
#[derive(Debug)]
pub enum WorkerRequest {
    /// Run one brain turn for this prompt, on the default agent
    /// when idle, or on a fresh agent when parallelism is on and
    /// a slot is free; otherwise queued until an agent frees up.
    RunTurn { prompt: String },
    /// Run one brain turn on a FRESH agent (parallelism on and a
    /// slot free); otherwise queued like [`WorkerRequest::RunTurn`].
    SpawnAgent { prompt: String },
    /// Cancel one agent: push-cancel, abort, join within the
    /// bound (§10). `source` is a §10.2 source string.
    CancelAgent { agent_id: String, source: String },
    /// Stop everything: trip the stop latch, cancel every agent,
    /// deny every queued action (§15.3). The body stack must be
    /// rebuilt before body actions run again.
    StopAll,
    /// Rebuild the body stack with a fresh actor and stop flag
    /// (§4.1). Used after a fail-all and after DỪNG TẤT CẢ.
    RebuildBody,
    /// One agent's task finished (sent by the task itself, on
    /// this same channel, so the loop handles completions and
    /// UI requests in one order). `backend` is the provider
    /// that answered the task's last call, when known.
    AgentFinished {
        agent_id: String,
        outcome: TaskOutcome,
        backend: Option<String>,
    },
    /// Spawn one hierarchy child (§8), sent by the `delegate`
    /// tool. The loop replies on `reply` when the child ends
    /// (or refuses at once when the limits forbid it).
    SpawnChild {
        // P8.5: boxed like TaskOutcome::Done — ChildSpec carries
        // the parent TurnRuntime snapshot (~384 bytes) and trips
        // `clippy::large_enum_variant` unboxed.
        spec: Box<ChildSpec>,
        reply: Sender<ChildResult>,
    },
    /// Rebuild the provider chain with a new Privacy Mode and order.
    /// The active profile stays: only the Privacy Mode and the
    /// chain order change (the header toggle / backend picker).
    Rebuild {
        privacy_on: bool,
        order: Vec<String>,
    },
    /// Switch to a named profile: rebuild the chain from the
    /// profile's chain, Privacy Mode, and cost cap, reload SOUL
    /// and the skill filter, and audit the switch. An unknown
    /// name falls closed to the default profile with a warning.
    SwitchProfile { name: String },
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

/// The scope of a failure (§4, G6-9): one agent, or all of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureScope {
    /// Only the agent that hit the failure stops.
    Single,
    /// Every agent stops; the body stack must be rebuilt.
    All,
}

impl FailureScope {
    pub fn as_str(&self) -> &'static str {
        match self {
            FailureScope::Single => "single",
            FailureScope::All => "all",
        }
    }
}

/// One MCP server's status for the UI's continuous indicator
/// (§6.3, §15.2): unsandboxed servers stay visible the whole run.
#[derive(Debug, Clone)]
pub struct McpServerStatus {
    pub name: String,
    pub unsandboxed: bool,
    pub connected: bool,
}

/// Events from the worker to the UI thread.
#[derive(Debug)]
pub enum WorkerEvent {
    /// A turn finished cleanly. `backend` is the provider that
    /// answered the last call, when known. `verified` is the
    /// aggregate verification status (R4); `delegate_failures`
    /// names the children that failed while the parent went on
    /// (§8.5, partial results). Children never emit this event:
    /// their result reaches the UI through the parent.
    TurnDone {
        agent_id: String,
        text: String,
        tools_used: Vec<String>,
        partial: bool,
        backend: Option<String>,
        verified: Option<VerificationStatus>,
        /// What kind of verification the verified steps carry
        /// (F6-05): travels with `verified` so a mixed turn is
        /// never presented as bare GUI verification.
        verification_kind: VerificationKind,
        /// Paths an `execute_code` step failed to access inside
        /// the sandbox (F6-17), detected from the tool result.
        sandbox_denied_paths: Vec<String>,
        delegate_failures: Vec<String>,
    },
    /// A turn failed (provider error, guard error, audit failure).
    /// `kind` is the display classification; `error` is the raw text.
    TurnFailed {
        agent_id: String,
        error: String,
        kind: FailureKind,
    },
    /// The turn was cancelled (turn cancel or STOP), before or
    /// during execution. Not a failure: nothing ran that should
    /// not have, and the prompt stays available for retry.
    TurnCancelled { agent_id: String },
    /// The chain is trying a backend for one agent (one event per
    /// attempt, in chain order, including retries). The UI shows
    /// which backend is being tried, instead of a generic spinner.
    TryingBackend { agent_id: String, name: String },
    /// The brain entered a new phase (thinking, acting,
    /// verifying, tool failed). The UI shows the phase plus
    /// elapsed time, instead of a generic spinner. The progress
    /// itself carries the agent id.
    TurnProgress { progress: BrainProgress },
    /// The chain was (re)built, or the attempt failed. `names` is
    /// empty when the build failed; the previous chain (if any) is
    /// kept on a failed rebuild.
    ChainRebuilt {
        names: Vec<String>,
        error: Option<String>,
    },
    /// A profile switch completed: the chain was rebuilt from the
    /// new profile. The UI updates its header, sidebar, greeting,
    /// Privacy Mode, and backend list from this event — never
    /// optimistically. `warning` is set when the requested name
    /// was unknown and the switch fell closed to the default.
    ProfileSwitched {
        name: String,
        greeting: String,
        soul_sent: bool,
        privacy_on: bool,
        order: Vec<String>,
        warning: Option<String>,
        /// Bare SOUL file name of the new profile, so the UI
        /// Settings section shows the active profile's file
        /// (fix-pass demo catch: the startup file went stale
        /// after a switch).
        soul_file: String,
    },
    /// The worker thread caught a panic in the loop itself. The
    /// loop keeps serving; the UI shows the control error state.
    /// (Panics inside agent tasks arrive as single-agent
    /// [`WorkerEvent::AgentFault`] instead.)
    WorkerFault { error: String },
    /// The body's emergency-stop flag tripped (global hotkey, or
    /// an input attempt while an approval card waited). The stop
    /// slot is already cancelled; the UI shows the stopped state
    /// and the reason. The flag latches: only a body rebuild
    /// clears the body stop.
    StopTripped { reason: String },
    /// The active profile's SOUL file changed between two
    /// rebuilds without going through the code path (the app
    /// never writes SOUL): it was edited by hand or by another
    /// program. The new text is already in use for the next
    /// turn; the UI warns persistently. A profile switch is
    /// NOT this event: switching legitimately loads a
    /// different SOUL file.
    SoulChangedExternally { profile: String, soul_file: String },
    /// One agent failed in a way the UI must show distinctly
    /// from a plain turn failure: a body fault (scope `All`,
    /// every agent stops) or a task panic (scope `Single`).
    /// `cause` is the raw error text.
    AgentFault {
        agent_id: String,
        scope: FailureScope,
        cause: String,
    },
    /// The coordinator degraded: a cancelled task did not end
    /// within the join bound, so its end is unverified (§10).
    /// No new agent spawns until restart.
    Degraded { agent_id: String, reason: String },
    /// The body stack was rebuilt with a fresh actor and stop
    /// flag (§4.1). Body actions may run again (after a fresh
    /// screenshot: the frame floor rejects stale observations).
    BodyRebuilt,
    /// A body rebuild failed. The body stays unavailable.
    BodyRebuildFailed { error: String },
    /// A new agent was spawned (a UI-spawned agent or a
    /// hierarchy child). The default agent is reported once at
    /// startup. `parent` is set for hierarchy children.
    AgentSpawned {
        agent_id: String,
        label: String,
        parent: Option<String>,
    },
    /// A spawned agent was reaped (its id retired, its snapshot
    /// slot dropped). The default agent never emits this: it
    /// stays, idle, between turns.
    AgentEnded { agent_id: String },
    /// MCP server status at startup, for the continuous
    /// indicator (§6.3). Sent once, after the first connect.
    McpStatus { servers: Vec<McpServerStatus> },
}

/// What a rebuild needs: the specs to build from and the retry
/// policy. The cost cap comes from the active profile at rebuild
/// time, not from here. Absent in tests, where rebuilds are
/// rejected.
pub struct RebuildConfig {
    pub specs: Vec<BackendSpec>,
    pub retry: RetryPolicy,
}

/// Everything the worker thread owns.
/// The body stack, built by the caller when `--body` is on
/// (Phase 4). The worker registers the body tools, verifies each
/// step through the body, purges privacy-tainted frames on
/// rebuild and before each turn, and bridges the stop flag into
/// the stop slot. On a body rebuild (§4.1) the actor and the stop
/// flag are swapped for fresh ones; the frame store and the
/// privacy flag survive (the frame floor rejects stale frames).
pub struct BodyStack {
    pub actor: BodyActor,
    pub frames: Arc<FrameStore>,
    pub privacy: PrivacyFlag,
    pub stop_flag: Arc<StopFlag>,
}

/// Build a fresh body plus its stop flag, for body rebuilds
/// (§4.1). The caller supplies it when `--body` is on; tests
/// supply a mock body. `None` means no body stack exists and no
/// rebuild is possible.
pub type BodyFactory =
    Arc<dyn Fn() -> Result<(Box<dyn Body>, Arc<StopFlag>), MimintError> + Send + Sync>;

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
    /// Per-turn cancel slot for the DEFAULT agent, shared with
    /// the UI (Hủy lượt). Spawned agents get their own slots.
    pub turn_slot: Arc<TokenSlot>,
    /// Wake-up flag the approver pokes when a job arrives.
    pub repaint: Arc<RepaintSignal>,
    /// How long an approval card may wait before timing out.
    pub approval_timeout: Duration,
    /// Maximum age of the observation a body action may run
    /// on (F4-08). The same value the card warns against and
    /// the body tools enforce at execution time.
    pub max_observation_age: Duration,
    /// Body stack for `--body` mode. `None` means no body tools:
    /// the brain cannot touch the screen.
    pub body: Option<BodyStack>,
    /// Shared approval-pending flag: the SAME `Arc<AtomicBool>`
    /// the body guard consults. Set while a card waits.
    pub approval_pending: Option<Arc<AtomicBool>>,
    /// Effective Privacy Mode for the initial chain (the active
    /// profile's value, as the caller built the chain).
    pub privacy_on: bool,
    /// All resolved profiles. The worker resolves switches
    /// against this set and falls closed to the default.
    pub profiles: ProfileSet,
    /// Active profile name at startup (already resolved by the
    /// caller; an unknown name falls closed to the default).
    pub active_profile: String,
    /// SOUL store for the active profile's persona. `None` in
    /// tests: turns run with the built-in default prompt.
    pub soul_store: Option<SoulStore>,
    /// Skill store for the promoted-skill catalog. `None` in
    /// tests: no skills are retrieved.
    pub skill_store: Option<SkillStore>,
    /// Experience store for retrieval before planning. Only set
    /// on the brain when the egress rule allows user data to
    /// reach the backends. `None` in tests.
    pub experience: Option<Arc<ExperienceStore>>,
    /// Phase 6 agent configuration (§12): parallelism, caps,
    /// queue SLA, action timeout.
    pub agents: AgentsConfig,
    /// Phase 6 hierarchy configuration (§12). Disabled by
    /// default; enabling it without parallelism is a config
    /// error (caught at validation time).
    pub hierarchy: HierarchyConfig,
    /// Phase 6 sandbox configuration (§12): execute_code
    /// timeout, output cap, rlimits.
    pub sandbox: SandboxConfig,
    /// Phase 6 routing configuration (§12): R6 threshold.
    pub routing: RoutingConfig,
    /// MCP server entries from the config file (§12). Empty
    /// means no MCP. Never honored on Android.
    pub mcp_servers: Vec<McpServerConfig>,
    /// Factory for fresh bodies on rebuild (§4.1). `None` when
    /// there is no body stack.
    pub body_factory: Option<BodyFactory>,
    /// The current-flag cell shared with the hotkey listener
    /// (§4.1). `None` without a body stack.
    pub stop_flag_cell: Option<Arc<StopFlagCell>>,
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

    /// Run a turn on a fresh agent (parallelism on, slot free),
    /// or queued like [`WorkerHandle::run_turn`] otherwise.
    pub fn spawn_agent(&self, prompt: String) {
        let _ = self.request_tx.send(WorkerRequest::SpawnAgent { prompt });
    }

    pub fn rebuild(&self, privacy_on: bool, order: Vec<String>) {
        let _ = self
            .request_tx
            .send(WorkerRequest::Rebuild { privacy_on, order });
    }

    /// Switch to a named profile (sidebar). The worker resolves
    /// the name, rebuilds, and reports
    /// [`WorkerEvent::ProfileSwitched`].
    pub fn switch_profile(&self, name: String) {
        let _ = self.request_tx.send(WorkerRequest::SwitchProfile { name });
    }

    /// Cancel the default agent's running turn. Trips the turn
    /// slot directly (lands even while the loop is busy) AND
    /// asks the loop for the full push-cancel/abort/join path.
    /// Safe to call when idle.
    pub fn cancel_turn(&self) {
        self.turn_slot.cancel_current();
        let _ = self.request_tx.send(WorkerRequest::CancelAgent {
            agent_id: AGENT_DEFAULT.to_string(),
            source: "turn_cancel".to_string(),
        });
    }

    /// Cancel one agent (its children go with it). The loop
    /// performs the full push-cancel/abort/join path.
    pub fn cancel_agent(&self, agent_id: String) {
        let _ = self.request_tx.send(WorkerRequest::CancelAgent {
            agent_id,
            source: "turn_cancel".to_string(),
        });
    }

    /// Trip the latching emergency stop (the DỪNG NGAY button:
    /// no warning, immediate). Trips the slots directly AND
    /// asks the loop for the full stop-all path. Stays tripped
    /// until [`WorkerHandle::resume`] replaces the token; the
    /// body stop additionally needs a body rebuild.
    pub fn stop_now(&self) {
        self.stop_slot.cancel_current();
        self.turn_slot.cancel_current();
        let _ = self.request_tx.send(WorkerRequest::StopAll);
    }

    /// Stop everything, via the loop only (the DỪNG TẤT CẢ
    /// button, after its scope warning). The loop trips the
    /// latch, cancels every agent, and denies queued actions.
    pub fn stop_all(&self) {
        let _ = self.request_tx.send(WorkerRequest::StopAll);
    }

    /// Rebuild the body stack with a fresh actor and stop flag
    /// (after a fail-all or DỪNG TẤT CẢ).
    pub fn rebuild_body(&self) {
        let _ = self.request_tx.send(WorkerRequest::RebuildBody);
    }

    /// Clear the stop latch (the user actively re-enables). Turns
    /// already cancelled stay cancelled; new turns start fresh.
    /// The body stop still needs a rebuild (see
    /// [`WorkerHandle::rebuild_body`]).
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

/// Everything a turn needs from the active profile, recomputed
/// on every successful (re)build: the chain's endpoints decide
/// the egress rule, so SOUL/experience availability is re-derived
/// whenever the chain changes. A failed rebuild keeps the old
/// runtime, like it keeps the old chain.
struct ProfileRuntime {
    profile: Profile,
    privacy_on: bool,
    order: Vec<String>,
    /// The system prompt for turns, or `None` when SOUL must be
    /// withheld (Privacy Mode + a remote backend in the chain).
    /// Withholding is audited at rebuild time, not per turn.
    soul_prompt: Option<String>,
    /// True when user data (SOUL, experience) may reach the
    /// backends. Gates both the SOUL prompt and the experience
    /// store on the brain.
    may_send_user_data: bool,
    greeting: String,
    /// Promoted skills, filtered by the profile's selection
    /// (`None` means all promoted skills).
    skills: Vec<SkillDef>,
    /// FNV-1a hash of the loaded SOUL body, when SOUL came from
    /// a store. `None` for the built-in default (no file) or a
    /// failed load. The worker loop compares it across rebuilds
    /// to detect hand edits (F5-11).
    soul_hash: Option<String>,
}

/// Build the runtime for one profile against a built chain.
/// SOUL load/skill catalog failures fall back to the built-in
/// default / no skills and are audited; they never fail the
/// rebuild, because the chain itself is usable without them.
#[allow(clippy::too_many_arguments)]
fn compute_runtime(
    profile: &Profile,
    privacy_on: bool,
    order: Vec<String>,
    endpoints: &[&str],
    soul_store: Option<&SoulStore>,
    skill_store: Option<&SkillStore>,
    audit: &AuditLog,
) -> ProfileRuntime {
    let all_loopback = endpoints
        .iter()
        .all(|url| url_is_loopback(url).unwrap_or(false));
    let may_send = soul_may_be_sent(privacy_on, all_loopback);
    // SOUL: load (creating the default file on first use) when a
    // store exists; otherwise the built-in default text, held in
    // memory only and never written anywhere.
    let fallback_doc = || SoulDoc {
        version: SOUL_FORMAT_VERSION,
        body: default_soul_text().to_string(),
        path: None,
    };
    let (doc, greeting) = match soul_store {
        Some(store) => match store.load_or_create(&profile.soul_file) {
            Ok(doc) => {
                let greeting = doc.greeting().to_string();
                (doc, greeting)
            }
            Err(err) => {
                let _ = audit.log(
                    "ui-profile",
                    "soul_load_failed",
                    None,
                    Some("fallback"),
                    json!({"profile": profile.name, "error": err.to_string()}),
                );
                (fallback_doc(), default_greeting().to_string())
            }
        },
        None => (fallback_doc(), default_greeting().to_string()),
    };
    // The hash names the text the model receives. It is only
    // meaningful when SOUL came from a file: the built-in
    // default has no file to change behind our back.
    let soul_hash = if soul_store.is_some() && doc.path.is_some() {
        Some(soul_content_hash(&doc))
    } else {
        None
    };
    let soul_prompt = if may_send {
        Some(build_system_prompt(&doc, &[]))
    } else {
        let _ = audit.log(
            "ui-profile",
            "soul_withheld",
            None,
            Some("withheld"),
            json!({
                "profile": profile.name,
                "privacy_on": privacy_on,
                "all_loopback": all_loopback,
            }),
        );
        None
    };
    let skills = match skill_store {
        Some(store) => match store.catalog() {
            Ok(catalog) => match &profile.skills {
                None => catalog,
                Some(selected) => catalog
                    .into_iter()
                    .filter(|def| selected.iter().any(|name| name == &def.name))
                    .collect(),
            },
            Err(err) => {
                let _ = audit.log(
                    "ui-profile",
                    "skill_catalog_failed",
                    None,
                    Some("empty"),
                    json!({"profile": profile.name, "error": err.to_string()}),
                );
                Vec::new()
            }
        },
        None => Vec::new(),
    };
    // Trigger conflicts (F5-14): identical triggers do not
    // override each other — retrieval order decides — but the
    // overlap is worth a warning in the audit log at load time.
    for (trigger, names) in find_trigger_conflicts(&skills) {
        let _ = audit.log(
            "ui-profile",
            "skill_trigger_conflict",
            None,
            Some("warning"),
            json!({
                "profile": profile.name,
                "trigger": trigger,
                "skills": names,
            }),
        );
    }
    ProfileRuntime {
        profile: profile.clone(),
        privacy_on,
        order,
        soul_prompt,
        may_send_user_data: may_send,
        greeting,
        skills,
        soul_hash,
    }
}

/// Compare a freshly computed runtime's SOUL hash against the
/// baseline (F5-11). A different hash for the SAME file means
/// the file changed outside the code path — the app never
/// writes SOUL — so the change is audited and the UI is told
/// to warn persistently. A different file (profile switch) or
/// a missing hash (built-in default / failed load) resets the
/// baseline silently: neither is evidence of an external edit.
fn check_soul_change(
    baseline: &mut (String, Option<String>),
    runtime: &ProfileRuntime,
    audit: &AuditLog,
    event_tx: &Sender<WorkerEvent>,
) {
    let same_file = baseline.0 == runtime.profile.soul_file;
    if same_file {
        if let (Some(old), Some(new)) = (&baseline.1, &runtime.soul_hash) {
            if old != new {
                let _ = audit.log(
                    "ui-profile",
                    "soul_changed_externally",
                    None,
                    Some("warning"),
                    json!({
                        "profile": runtime.profile.name,
                        "soul_file": runtime.profile.soul_file,
                        "old_hash": old,
                        "new_hash": new,
                    }),
                );
                let _ = event_tx.send(WorkerEvent::SoulChangedExternally {
                    profile: runtime.profile.name.clone(),
                    soul_file: runtime.profile.soul_file.clone(),
                });
            }
        }
    }
    baseline.0 = runtime.profile.soul_file.clone();
    baseline.1 = runtime.soul_hash.clone();
}

/// Spawn the worker thread. The initial chain state is reported as
/// the first [`WorkerEvent::ChainRebuilt`].
pub fn spawn_worker(deps: WorkerDeps) -> WorkerHandle {
    let (request_tx, request_rx) = std::sync::mpsc::channel();
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let stop_slot = Arc::clone(&deps.stop_slot);
    let turn_slot = Arc::clone(&deps.turn_slot);
    let request_tx_for_loop = request_tx.clone();
    std::thread::spawn(move || worker_loop(deps, request_rx, request_tx_for_loop, event_tx));
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
    chain.set_progress_hook(Arc::new(move |name, agent_id| {
        let _ = tx.send(WorkerEvent::TryingBackend {
            agent_id: agent_id.to_string(),
            name: name.to_string(),
        });
    }));
}

/// Everything the loop owns. All mutation happens on the worker
/// thread, driven by the merged request channel.
struct LoopState {
    chain: Option<Arc<FallbackChain>>,
    profile_runtime: ProfileRuntime,
    soul_baseline: (String, Option<String>),
    body: Option<BodyStack>,
    actor_cell: Option<Arc<Mutex<BodyActor>>>,
    stop_cell: Option<Arc<StopFlagCell>>,
    body_factory: Option<BodyFactory>,
    actor_config: ActorConfig,
    coordinator: AgentCoordinator,
    approver: Arc<UiApprover>,
    notebook: Arc<Notebook>,
    audit: Arc<AuditLog>,
    max_steps: usize,
    stop_slot: Arc<TokenSlot>,
    turn_slot: Arc<TokenSlot>,
    max_observation_age: Duration,
    rebuild: Option<RebuildConfig>,
    profiles: ProfileSet,
    soul_store: Option<SoulStore>,
    skill_store: Option<SkillStore>,
    experience: Option<Arc<ExperienceStore>>,
    agents_config: AgentsConfig,
    hierarchy: HierarchyConfig,
    routing: RoutingConfig,
    pending_turns: VecDeque<String>,
    pending_children: HashMap<String, Sender<ChildResult>>,
    request_tx: Sender<WorkerRequest>,
    event_tx: Sender<WorkerEvent>,
}

impl LoopState {
    fn send(&self, event: WorkerEvent) {
        let _ = self.event_tx.send(event);
    }

    /// The current profile's turn snapshot (§10.1): cloned at
    /// every spawn, so a profile switch mid-turn never changes
    /// what a running turn sees.
    fn snapshot_runtime(&self) -> TurnRuntime {
        TurnRuntime {
            profile_name: self.profile_runtime.profile.name.clone(),
            soul_prompt: self.profile_runtime.soul_prompt.clone(),
            skills: self.profile_runtime.skills.clone(),
            may_send_user_data: self.profile_runtime.may_send_user_data,
            experience: self.experience.clone(),
            max_tokens: DEFAULT_MAX_TOKENS,
            gui_batch_threshold: self.routing.gui_batch_threshold,
            max_steps: self.max_steps,
            profile: self.profile_runtime.profile.clone(),
        }
    }

    /// Create a fresh entry for a new agent id (spawned agents
    /// and hierarchy children). The default agent's entry is
    /// created by the coordinator at startup instead.
    fn create_entry(&mut self, agent_id: String, label: String, parent: Option<String>, depth: u8) {
        let entry = AgentEntry {
            agent_id: agent_id.clone(),
            label,
            parent,
            depth,
            state: AgentState::Idle,
            join_handle: None,
            cancel_token: CancelToken::new(),
            turn_slot: TokenSlot::new(),
            cancel_report: Arc::new(CancelReport::new()),
            kill_ledger: KillLedger::new(),
            spawned_at: Instant::now(),
            turn_id: String::new(),
            delegate_failures: Arc::new(Mutex::new(Vec::new())),
            budget: None,
            run_dirs: Arc::new(RunDirTracker::new()),
        };
        self.coordinator.register_running(entry);
    }

    /// Reset the default agent's entry to idle after a normal
    /// completion (its entry is persistent, never reaped).
    fn reset_default_entry(&mut self) {
        if let Some(entry) = self.coordinator.get_mut(AGENT_DEFAULT) {
            entry.state = AgentState::Idle;
            entry.join_handle = None;
            entry.cancel_token = CancelToken::new();
            entry.cancel_report = Arc::new(CancelReport::new());
            entry.kill_ledger = KillLedger::new();
            entry.run_dirs = Arc::new(RunDirTracker::new());
            entry.turn_id = String::new();
            entry.budget = None;
            if let Ok(mut failures) = entry.delegate_failures.lock() {
                failures.clear();
            }
        }
    }

    /// Start a turn on an agent whose entry already exists,
    /// building its registry and brain and spawning its task.
    /// On a build failure the turn fails immediately and the
    /// entry returns to idle (default) or is reaped (spawned).
    #[allow(clippy::too_many_arguments)]
    fn spawn_for(
        &mut self,
        runtime: &tokio::runtime::Runtime,
        agent_id: &str,
        prompt: String,
        turn_runtime: TurnRuntime,
        max_steps: usize,
        budget: Option<u64>,
        time_limit: Option<Duration>,
    ) {
        let Some(chain) = self.chain.clone() else {
            self.send(WorkerEvent::TurnFailed {
                agent_id: agent_id.to_string(),
                error: "no backend is configured: add a backend to the config file or start Ollama/LM Studio, then restart the app".to_string(),
                kind: FailureKind::OutOfBackends,
            });
            self.drop_idle_entry(agent_id);
            return;
        };
        let turn_id = self.coordinator.next_turn_id(agent_id);
        // Refresh the entry's per-turn state.
        let (turn_token, cancel_report, kill_ledger, run_dirs, delegate_failures) = {
            let Some(entry) = self.coordinator.get_mut(agent_id) else {
                return;
            };
            let turn_token = CancelToken::new();
            entry.turn_slot.replace(turn_token.clone());
            entry.cancel_token = turn_token.clone();
            entry.cancel_report = Arc::new(CancelReport::new());
            entry.kill_ledger = KillLedger::new();
            entry.run_dirs = Arc::new(RunDirTracker::new());
            entry.delegate_failures = Arc::new(Mutex::new(Vec::new()));
            entry.turn_id = turn_id.clone();
            entry.state = AgentState::Running;
            entry.spawned_at = Instant::now();
            entry.budget = budget;
            (
                turn_token,
                Arc::clone(&entry.cancel_report),
                entry.kill_ledger.clone(),
                Arc::clone(&entry.run_dirs),
                Arc::clone(&entry.delegate_failures),
            )
        };
        let turn_slot = self
            .coordinator
            .get(agent_id)
            .map(|e| Arc::clone(&e.turn_slot));
        if let Some(slot) = turn_slot {
            self.approver.register_turn_slot(agent_id, slot);
        }
        if let Some(mcp) = &self.coordinator.mcp {
            mcp.begin_turn(&turn_id);
        }
        if let Some(tokens) = budget {
            self.coordinator.ledger.set_agent_budget(agent_id, tokens);
        }
        let depth = self.coordinator.get(agent_id).map(|e| e.depth).unwrap_or(0);
        // One registry per turn (§10.1): notebook tools bound to
        // this agent, body tools bound to this agent and the
        // current actor, execute_code with this turn's tokens,
        // MCP tools for this agent, and `delegate` only for the
        // depth-0 default agent when hierarchy is on.
        let mut registry = default_registry_for(Arc::clone(&self.notebook), agent_id);
        if let Some(stack) = &self.body {
            register_body_tools_for_agent(
                &mut registry,
                stack.actor.clone(),
                Arc::clone(&stack.frames),
                stack.privacy.clone(),
                &self.coordinator.snapshot_slots,
                agent_id,
                self.max_observation_age,
            );
        }
        if !cfg!(target_os = "android") {
            let code_tool = ExecuteCodeTool::new(
                agent_id,
                Arc::clone(&self.coordinator.sandbox),
                Arc::clone(&run_dirs),
                self.coordinator.children.clone(),
            );
            code_tool.set_kill_ledger(kill_ledger.clone());
            code_tool.set_cancel_tokens(vec![turn_token.clone(), self.stop_slot.get()]);
            registry.register(Box::new(code_tool));
        }
        if let Some(mcp) = &self.coordinator.mcp {
            for tool in mcp.tools_for(agent_id) {
                registry.register(Box::new(tool));
            }
        }
        if self.hierarchy.enabled && depth == 0 && agent_id == AGENT_DEFAULT {
            registry.register(Box::new(DelegateTool::new(
                self.request_tx.clone(),
                agent_id.to_string(),
                turn_runtime.clone(),
                self.hierarchy.clone(),
                Arc::clone(&delegate_failures),
            )));
        }
        let brain_result = Brain::new(
            Arc::clone(&chain) as Arc<dyn Provider>,
            Arc::new(registry),
            Some(Arc::clone(&self.approver) as Arc<dyn Approver>),
            Arc::clone(&self.audit),
            Arc::clone(&self.notebook),
            turn_id.clone(),
            agent_id.to_string(),
            max_steps,
        );
        let mut brain = match brain_result {
            Ok(brain) => brain,
            Err(err) => {
                let text = err.to_string();
                self.send(WorkerEvent::TurnFailed {
                    agent_id: agent_id.to_string(),
                    kind: classify_error(&text),
                    error: text,
                });
                self.drop_idle_entry(agent_id);
                return;
            }
        };
        brain.add_cancel_token(turn_token.clone());
        brain.add_cancel_token(self.stop_slot.get());
        brain.set_profile(turn_runtime.profile_name.clone());
        if let Some(prompt) = &turn_runtime.soul_prompt {
            brain.set_system_prompt(prompt.clone());
        }
        if turn_runtime.may_send_user_data {
            brain.set_skills(turn_runtime.skills.clone());
            if let Some(store) = &turn_runtime.experience {
                brain.set_experience_store(Arc::clone(store));
            }
        }
        let progress_tx = self.event_tx.clone();
        brain.set_progress_hook(Arc::new(move |progress| {
            let _ = progress_tx.send(WorkerEvent::TurnProgress { progress });
        }));
        if let Some(stack) = &self.body {
            // Privacy discipline (P4.6.6): tainted frames from
            // earlier turns never survive into a new turn.
            stack.frames.purge_tainted();
            brain.set_step_verifier(Arc::new(BodyStepVerifier::new_for(
                agent_id,
                stack.actor.clone(),
                Arc::clone(&stack.frames),
            )));
        }
        brain.set_ledger(Arc::clone(&self.coordinator.ledger));
        brain.set_max_tokens(turn_runtime.max_tokens);
        brain.set_gui_batch_threshold(turn_runtime.gui_batch_threshold);
        brain.set_cancel_report(Arc::clone(&cancel_report));
        brain.set_kill_ledger(kill_ledger);
        // The task reports through the merged request channel,
        // and always reports: panics are caught by the wrapper.
        let request_tx = self.request_tx.clone();
        let agent_id_owned = agent_id.to_string();
        let chain_for_backend = Arc::clone(&chain);
        let handle = runtime.spawn(async move {
            let outcome = CatchUnwind::new(brain.run(&prompt)).await;
            let outcome = map_task_outcome(outcome);
            let backend = chain_for_backend.last_used_provider();
            let _ = request_tx.send(WorkerRequest::AgentFinished {
                agent_id: agent_id_owned,
                outcome,
                backend,
            });
        });
        if let Some(entry) = self.coordinator.get_mut(agent_id) {
            entry.join_handle = Some(handle);
        }
        // Hierarchy children get a wall-clock sleeper (§8.2):
        // at the limit the token trips with source "timeout"
        // and the delegate tool reports `subagent_timeout`.
        // The report is marked too: the completion handler
        // reads the cancel SOURCE from the report to tell a
        // timeout (TimedOut) from a plain cancel, and a bare
        // token trip would leave the report blank — the child
        // would end as a generic cancellation instead.
        if let Some(limit) = time_limit {
            let token = turn_token.clone();
            let report = Arc::clone(&cancel_report);
            runtime.spawn(async move {
                tokio::time::sleep(limit).await;
                token.cancel_with_source("timeout");
                report.mark_requested("timeout");
            });
        }
    }

    /// Drop an entry that never started (no chain, brain build
    /// failure): the default agent returns to idle; a spawned
    /// agent is reaped and reported ended.
    fn drop_idle_entry(&mut self, agent_id: &str) {
        if agent_id == AGENT_DEFAULT {
            self.reset_default_entry();
        } else {
            self.coordinator.reap(agent_id);
            self.send(WorkerEvent::AgentEnded {
                agent_id: agent_id.to_string(),
            });
        }
    }

    /// Start a UI turn: the default agent when idle, a fresh
    /// agent when parallelism is on and a slot is free, the
    /// default agent when it is idle but a fresh one cannot
    /// spawn, otherwise queued. `prefer_new` (the "new agent"
    /// action) skips the default-first branch.
    fn start_turn(&mut self, runtime: &tokio::runtime::Runtime, prompt: String, prefer_new: bool) {
        let default_idle = self
            .coordinator
            .get(AGENT_DEFAULT)
            .is_some_and(|e| e.state == AgentState::Idle);
        let turn_runtime = self.snapshot_runtime();
        if !prefer_new && default_idle {
            self.spawn_for(
                runtime,
                AGENT_DEFAULT,
                prompt,
                turn_runtime,
                self.max_steps,
                None,
                None,
            );
            return;
        }
        if self.agents_config.parallelism_enabled && self.coordinator.can_spawn() {
            let agent_id = self.coordinator.next_agent_id();
            let label = label_for(&prompt);
            self.create_entry(agent_id.clone(), label.clone(), None, 0);
            self.send(WorkerEvent::AgentSpawned {
                agent_id: agent_id.clone(),
                label,
                parent: None,
            });
            self.spawn_for(
                runtime,
                &agent_id,
                prompt,
                turn_runtime,
                self.max_steps,
                None,
                None,
            );
            return;
        }
        if default_idle {
            self.spawn_for(
                runtime,
                AGENT_DEFAULT,
                prompt,
                turn_runtime,
                self.max_steps,
                None,
                None,
            );
            return;
        }
        self.pending_turns.push_back(prompt);
    }

    /// Start queued turns while an agent is free. Called after
    /// every completion and every cancel.
    fn start_next_queued(&mut self, runtime: &tokio::runtime::Runtime) {
        while let Some(prompt) = self.pending_turns.front().cloned() {
            let default_idle = self
                .coordinator
                .get(AGENT_DEFAULT)
                .is_some_and(|e| e.state == AgentState::Idle);
            let can_spawn = self.agents_config.parallelism_enabled && self.coordinator.can_spawn();
            if !default_idle && !can_spawn {
                return;
            }
            self.pending_turns.pop_front();
            self.start_turn(runtime, prompt, false);
        }
    }

    /// Cancel one agent through the coordinator (§10). Returns
    /// the outcome; the caller decides events and reaping.
    fn cancel_one_agent(
        &mut self,
        runtime: &tokio::runtime::Runtime,
        agent_id: &str,
        source: &str,
    ) -> CancelOutcome {
        let actor = self.body.as_ref().map(|stack| &stack.actor);
        self.coordinator.cancel_agent_sync(
            agent_id,
            source,
            runtime,
            actor,
            &self.approver,
            &self.audit,
            &self.event_tx,
        )
    }

    /// Cancel one agent and report it: child replies, terminal
    /// events, re-registration of the default entry. Children
    /// of the agent are cancelled first by the caller.
    fn cancel_and_report(
        &mut self,
        runtime: &tokio::runtime::Runtime,
        agent_id: &str,
        source: &str,
    ) {
        let is_child = self
            .coordinator
            .get(agent_id)
            .is_some_and(|e| e.parent.is_some());
        let is_default = agent_id == AGENT_DEFAULT;
        let outcome = self.cancel_one_agent(runtime, agent_id, source);
        match outcome {
            CancelOutcome::EndedVerified => {
                self.reply_child_cancelled(agent_id, source);
                if !is_child {
                    self.send(WorkerEvent::TurnCancelled {
                        agent_id: agent_id.to_string(),
                    });
                }
                if is_default {
                    self.coordinator
                        .reregister_default_agent(Arc::clone(&self.turn_slot));
                } else {
                    self.send(WorkerEvent::AgentEnded {
                        agent_id: agent_id.to_string(),
                    });
                }
            }
            CancelOutcome::EndUnverified => {
                // No terminal event: the end is unverified, and
                // the Degraded event already went out. The entry
                // stays for a late completion message.
            }
            CancelOutcome::AlreadyFinished | CancelOutcome::NotRunning => {}
        }
    }

    /// Deliver a cancellation to a waiting parent: the child's
    /// `delegate` call must not block until its backstop.
    fn reply_child_cancelled(&mut self, agent_id: &str, source: &str) {
        if let Some(reply) = self.pending_children.remove(agent_id) {
            let outcome = if source == "timeout" {
                ChildOutcome::TimedOut
            } else {
                ChildOutcome::Failed {
                    text: String::new(),
                    error: format!("cancelled: {source}"),
                }
            };
            let _ = reply.send(ChildResult {
                agent_id: agent_id.to_string(),
                outcome,
            });
        }
    }

    /// Handle a `CancelAgent` request: the agent's children go
    /// first (source "parent"), then the agent itself.
    fn handle_cancel(&mut self, runtime: &tokio::runtime::Runtime, agent_id: &str, source: &str) {
        let children: Vec<String> = self
            .coordinator
            .agents()
            .filter(|e| e.state == AgentState::Running && e.parent.as_deref() == Some(agent_id))
            .map(|e| e.agent_id.clone())
            .collect();
        for child in &children {
            self.cancel_and_report(runtime, child, "parent");
        }
        self.cancel_and_report(runtime, agent_id, source);
        self.start_next_queued(runtime);
    }

    /// Handle `StopAll` (§15.3): trip the latch, cancel every
    /// agent, drop the queued turns. The body stack must be
    /// rebuilt before body actions run again.
    fn handle_stop_all(&mut self, runtime: &tokio::runtime::Runtime) {
        self.stop_slot.cancel_current();
        self.turn_slot.cancel_current();
        if let Some(cell) = &self.stop_cell {
            cell.get().trip_with(
                "ui_stop",
                "DỪNG TẤT CẢ: the user stopped everything; the body stack must be rebuilt",
            );
        } else if let Some(stack) = &self.body {
            stack.stop_flag.trip_with(
                "ui_stop",
                "DỪNG TẤT CẢ: the user stopped everything; the body stack must be rebuilt",
            );
        }
        if self.body.is_none() {
            // No bridge thread without a body: record the stop
            // and tell the UI directly.
            let _ = self.audit.log(
                "ui-control",
                "emergency_stop",
                None,
                Some("stopped"),
                json!({
                    "source": "ui_stop",
                    "reason": "DỪNG TẤT CẢ: the user stopped everything",
                    "trip_ts_ms": now_ms(),
                }),
            );
            self.send(WorkerEvent::StopTripped {
                reason: "ui_stop: the user stopped everything".to_string(),
            });
        }
        // Children first (depth 1 before depth 0), so parents
        // get their child replies before they are cancelled.
        let mut ids: Vec<(u8, String)> = self
            .coordinator
            .agents()
            .filter(|e| e.state == AgentState::Running)
            .map(|e| (e.depth, e.agent_id.clone()))
            .collect();
        ids.sort_by_key(|a| std::cmp::Reverse(a.0));
        for (_, id) in ids {
            self.cancel_and_report(runtime, &id, "ui_stop");
        }
        self.pending_turns.clear();
    }

    /// Handle a hierarchy spawn request (§8): enforce the
    /// limits, clamp the budget, spawn the child — or refuse
    /// at once with the reason the tool reports.
    fn handle_spawn_child(
        &mut self,
        runtime: &tokio::runtime::Runtime,
        spec: ChildSpec,
        reply: Sender<ChildResult>,
    ) {
        let refuse = |reason: String| {
            let _ = reply.send(ChildResult {
                agent_id: AGENT_DEFAULT.to_string(),
                outcome: ChildOutcome::Refused { reason },
            });
        };
        if !self.hierarchy.enabled {
            refuse("subagent_unavailable: hierarchy is disabled".to_string());
            return;
        }
        if self.coordinator.degraded().is_some() {
            refuse(
                "subagent_unavailable: the coordinator is degraded; restart to spawn again"
                    .to_string(),
            );
            return;
        }
        if self.coordinator.live_children_of(&spec.parent_id) >= self.hierarchy.max_children {
            refuse(format!(
                "subagent_limit: {} already has {} live children (max {})",
                spec.parent_id, self.hierarchy.max_children, self.hierarchy.max_children
            ));
            return;
        }
        if !self.coordinator.can_spawn() {
            refuse("subagent_unavailable: no free agent slot".to_string());
            return;
        }
        // Budget (§8.2): the requested budget, else a quarter
        // of the parent's remaining budget (unlimited when the
        // parent is unlimited), clamped to what is left of the
        // parent's budget and the global cap. Clamping is
        // audited; the child never silently gets more.
        let parent_remaining = self.coordinator.ledger.agent_remaining(&spec.parent_id);
        let global_remaining = self.coordinator.ledger.global_remaining();
        let requested = spec.token_budget;
        let mut budget = requested.or_else(|| parent_remaining.map(|left| left / 4));
        if let Some(tokens) = budget {
            let mut clamped = tokens;
            if let Some(left) = parent_remaining {
                clamped = clamped.min(left);
            }
            if let Some(left) = global_remaining {
                clamped = clamped.min(left);
            }
            if clamped != tokens {
                let parent_turn = self
                    .coordinator
                    .get(&spec.parent_id)
                    .map(|e| e.turn_id.clone())
                    .unwrap_or_default();
                let _ = self.audit.log_for(
                    &spec.parent_id,
                    &parent_turn,
                    "delegate_clamped",
                    Some("delegate"),
                    Some("clamped"),
                    json!({
                        "requested_tokens": tokens,
                        "clamped_tokens": clamped,
                        "parent_remaining": parent_remaining,
                        "global_remaining": global_remaining,
                    }),
                );
            }
            budget = Some(clamped);
        }
        let agent_id = self.coordinator.next_agent_id();
        let label = label_for(&spec.task);
        self.create_entry(
            agent_id.clone(),
            label.clone(),
            Some(spec.parent_id.clone()),
            1,
        );
        self.send(WorkerEvent::AgentSpawned {
            agent_id: agent_id.clone(),
            label,
            parent: Some(spec.parent_id.clone()),
        });
        self.pending_children.insert(agent_id.clone(), reply);
        let time_limit = Duration::from_secs(spec.time_limit_secs);
        self.spawn_for(
            runtime,
            &agent_id,
            spec.task.clone(),
            spec.runtime.clone(),
            spec.max_steps,
            budget,
            Some(time_limit),
        );
        // A spawn that failed immediately (no chain, brain
        // build error) leaves no task behind: the waiting
        // parent must hear it at once, not at the backstop.
        let running = self
            .coordinator
            .get(&agent_id)
            .is_some_and(|e| e.state == AgentState::Running);
        if !running {
            self.reply_child_cancelled(&agent_id, "coordinator");
        }
    }

    /// Handle one agent's completion (§4, §8.5). Fail-all
    /// errors divert to [`LoopState::fail_all`]; everything
    /// else completes single-agent.
    fn handle_finished(
        &mut self,
        runtime: &tokio::runtime::Runtime,
        agent_id: &str,
        outcome: TaskOutcome,
        backend: Option<String>,
    ) {
        let Some(entry) = self.coordinator.get(agent_id) else {
            return; // Already reaped by a cancel.
        };
        if entry.state != AgentState::Running && entry.state != AgentState::EndedUnverified {
            return;
        }
        match &outcome {
            TaskOutcome::Failed {
                error,
                body_fault: true,
            } => {
                let actor_broken = self
                    .body
                    .as_ref()
                    .is_some_and(|stack| stack.actor.is_broken());
                self.fail_all(
                    runtime,
                    agent_id,
                    error.clone(),
                    actor_broken,
                    outcome,
                    backend,
                );
                return;
            }
            TaskOutcome::BudgetExhausted {
                scope: mimint_core::BudgetScope::Global,
                error,
            } => {
                self.fail_all(runtime, agent_id, error.clone(), false, outcome, backend);
                return;
            }
            _ => {}
        }
        self.complete_single(agent_id, outcome, backend);
        self.start_next_queued(runtime);
    }

    /// Complete one agent without fail-all: run-dir cleanup,
    /// child reply, turn events, reap/reset.
    fn complete_single(&mut self, agent_id: &str, outcome: TaskOutcome, backend: Option<String>) {
        let (is_child, is_default, delegate_failures, cancel_source) = {
            let Some(entry) = self.coordinator.get(agent_id) else {
                return;
            };
            let failures = entry
                .delegate_failures
                .lock()
                .map(|guard| guard.clone())
                .unwrap_or_default();
            (
                entry.parent.is_some(),
                agent_id == AGENT_DEFAULT,
                failures,
                entry.cancel_report.source(),
            )
        };
        if let Some(entry) = self.coordinator.get(agent_id) {
            entry.run_dirs.cleanup();
        }
        // The waiting parent hears first: the child's result
        // unblocks the `delegate` call before any UI event.
        if let Some(reply) = self.pending_children.remove(agent_id) {
            let child_outcome = match &outcome {
                TaskOutcome::Done(turn) => ChildOutcome::Completed {
                    text: turn.text.clone(),
                    partial: turn.partial,
                },
                TaskOutcome::Failed { error, .. } => ChildOutcome::Failed {
                    text: String::new(),
                    error: error.clone(),
                },
                TaskOutcome::BudgetExhausted { error, .. } => ChildOutcome::Failed {
                    text: String::new(),
                    error: error.clone(),
                },
                TaskOutcome::Cancelled => {
                    if cancel_source.as_deref() == Some("timeout") {
                        ChildOutcome::TimedOut
                    } else {
                        ChildOutcome::Failed {
                            text: String::new(),
                            error: format!("cancelled: {}", cancel_source.unwrap_or_default()),
                        }
                    }
                }
                TaskOutcome::Panicked(message) => ChildOutcome::Failed {
                    text: String::new(),
                    error: format!("sub-agent panicked: {message}"),
                },
            };
            let _ = reply.send(ChildResult {
                agent_id: agent_id.to_string(),
                outcome: child_outcome,
            });
        }
        // Children never emit turn events: their result reaches
        // the UI through the parent (§8).
        if !is_child {
            match &outcome {
                TaskOutcome::Done(turn) => self.send(WorkerEvent::TurnDone {
                    agent_id: agent_id.to_string(),
                    text: turn.text.clone(),
                    tools_used: turn.tools_used.clone(),
                    partial: turn.partial,
                    backend,
                    verified: turn.verification,
                    verification_kind: turn.verification_kind,
                    sandbox_denied_paths: turn.sandbox_denied_paths.clone(),
                    delegate_failures,
                }),
                TaskOutcome::Failed { error, .. } => self.send(WorkerEvent::TurnFailed {
                    agent_id: agent_id.to_string(),
                    kind: classify_error(error),
                    error: error.clone(),
                }),
                TaskOutcome::BudgetExhausted { error, .. } => self.send(WorkerEvent::TurnFailed {
                    agent_id: agent_id.to_string(),
                    kind: FailureKind::Other,
                    error: error.clone(),
                }),
                TaskOutcome::Cancelled => self.send(WorkerEvent::TurnCancelled {
                    agent_id: agent_id.to_string(),
                }),
                TaskOutcome::Panicked(message) => {
                    audit_failure_scope(
                        &self.audit,
                        FailureScope::Single,
                        &format!("agent task panicked: {message}"),
                        agent_id,
                        false,
                        false,
                        &[(agent_id.to_string(), true, EndVerification::Joined)],
                    );
                    self.send(WorkerEvent::AgentFault {
                        agent_id: agent_id.to_string(),
                        scope: FailureScope::Single,
                        cause: format!("agent task panicked: {message}"),
                    });
                    self.send(WorkerEvent::TurnFailed {
                        agent_id: agent_id.to_string(),
                        kind: FailureKind::Other,
                        error: format!("agent task panicked: {message}"),
                    });
                }
            }
        } else if let TaskOutcome::Panicked(message) = &outcome {
            audit_failure_scope(
                &self.audit,
                FailureScope::Single,
                &format!("agent task panicked: {message}"),
                agent_id,
                false,
                false,
                &[(agent_id.to_string(), true, EndVerification::Joined)],
            );
        }
        if is_default {
            self.reset_default_entry();
        } else {
            self.coordinator.reap(agent_id);
            self.send(WorkerEvent::AgentEnded {
                agent_id: agent_id.to_string(),
            });
        }
    }

    /// Fail-all (§4, G6-9): the causing agent already finished;
    /// every other running agent is cancelled, one scope record
    /// is written, the fault event goes out, and the body stack
    /// is rebuilt at once (a rebuild failure leaves the body
    /// unavailable, reported, never silent).
    fn fail_all(
        &mut self,
        runtime: &tokio::runtime::Runtime,
        cause_agent: &str,
        cause: String,
        actor_broken: bool,
        cause_outcome: TaskOutcome,
        backend: Option<String>,
    ) {
        let others: Vec<String> = self
            .coordinator
            .agents()
            .filter(|e| e.state == AgentState::Running && e.agent_id != cause_agent)
            .map(|e| e.agent_id.clone())
            .collect();
        let mut scope_agents: Vec<(String, bool, EndVerification)> =
            vec![(cause_agent.to_string(), true, EndVerification::Joined)];
        for id in &others {
            let is_child = self.coordinator.get(id).is_some_and(|e| e.parent.is_some());
            let is_default = id == AGENT_DEFAULT;
            let outcome = self.cancel_one_agent(runtime, id, "coordinator");
            match outcome {
                CancelOutcome::EndedVerified => {
                    scope_agents.push((id.clone(), true, EndVerification::Joined));
                    self.reply_child_cancelled(id, "coordinator");
                    if !is_child {
                        self.send(WorkerEvent::TurnCancelled {
                            agent_id: id.clone(),
                        });
                    }
                    if is_default {
                        self.coordinator
                            .reregister_default_agent(Arc::clone(&self.turn_slot));
                    } else {
                        self.send(WorkerEvent::AgentEnded {
                            agent_id: id.clone(),
                        });
                    }
                }
                CancelOutcome::EndUnverified => {
                    scope_agents.push((id.clone(), false, EndVerification::TimeoutUnverified));
                }
                CancelOutcome::AlreadyFinished => {
                    scope_agents.push((id.clone(), true, EndVerification::Joined));
                }
                CancelOutcome::NotRunning => {
                    scope_agents.push((id.clone(), false, EndVerification::NotApplicable));
                }
            }
        }
        audit_failure_scope(
            &self.audit,
            FailureScope::All,
            &cause,
            cause_agent,
            false,
            actor_broken,
            &scope_agents,
        );
        self.send(WorkerEvent::AgentFault {
            agent_id: cause_agent.to_string(),
            scope: FailureScope::All,
            cause: cause.clone(),
        });
        // The causing agent completes as a failure (unless it
        // is a child: its parent hears through the reply).
        self.complete_single(cause_agent, cause_outcome, backend);
        // Rebuild the body stack at once (§4): the old actor is
        // broken, and no new body action may run on it. A
        // rebuild failure is reported, never silent, and the
        // body stays unavailable until the user retries.
        if self.body.is_some() {
            match self.rebuild_body_stack("fail-all: body fault") {
                Ok(()) => self.send(WorkerEvent::BodyRebuilt),
                Err(error) => self.send(WorkerEvent::BodyRebuildFailed { error }),
            }
        }
        self.start_next_queued(runtime);
    }

    /// Rebuild the body stack (§4.1): a fresh body and stop flag
    /// from the factory, a fresh actor, the frame floor raised
    /// (only post-rebuild screenshots authorize input), the
    /// shared cells swapped, and one audit record. The old
    /// actor is dropped: its thread exits when its channel
    /// closes, unless it is stuck inside a body call — that
    /// thread can never be killed (§10), and the limitation is
    /// documented, not hidden.
    /// Rebuild wrapper (F6-15): a failed rebuild is audited
    /// (`body_rebuild_failed`, with the raw error), so the
    /// user-facing message can stay friendly without losing
    /// the cause. Success already audits `body_rebuilt`.
    fn rebuild_body_stack(&mut self, reason: &str) -> Result<(), String> {
        let result = self.rebuild_body_stack_inner(reason);
        if let Err(error) = &result {
            let _ = self.audit.log(
                "ui-control",
                "body_rebuild_failed",
                None,
                Some("error"),
                json!({
                    "reason": reason,
                    "error": error,
                }),
            );
        }
        result
    }

    fn rebuild_body_stack_inner(&mut self, reason: &str) -> Result<(), String> {
        let Some(factory) = self.body_factory.clone() else {
            return Err("no body factory: cannot rebuild the body stack".to_string());
        };
        let Some(stack) = self.body.as_mut() else {
            return Err("no body stack: nothing to rebuild".to_string());
        };
        let (body, flag) = factory().map_err(|err| err.to_string())?;
        let actor = BodyActor::spawn_with_config_and_audit(
            body,
            Arc::clone(&flag),
            self.actor_config,
            Some(Arc::clone(&self.audit)),
        );
        let generation = stack.frames.rebuild_barrier();
        stack.actor = actor.clone();
        stack.stop_flag = Arc::clone(&flag);
        if let Some(cell) = &self.actor_cell {
            *cell.lock().unwrap_or_else(|p| p.into_inner()) = actor;
        }
        if let Some(cell) = &self.stop_cell {
            cell.replace(flag);
        }
        let _ = self.audit.log(
            "ui-control",
            "body_rebuilt",
            None,
            Some("rebuilt"),
            json!({
                "reason": reason,
                "generation": generation,
            }),
        );
        Ok(())
    }

    /// Handle one request. Returns `false` when the loop must
    /// exit (shutdown was already handled by the caller).
    fn handle_request(&mut self, runtime: &tokio::runtime::Runtime, request: WorkerRequest) {
        match request {
            WorkerRequest::Shutdown => {}
            WorkerRequest::RunTurn { prompt } => {
                self.start_turn(runtime, prompt, false);
            }
            WorkerRequest::SpawnAgent { prompt } => {
                self.start_turn(runtime, prompt, true);
            }
            WorkerRequest::CancelAgent { agent_id, source } => {
                self.handle_cancel(runtime, &agent_id, &source);
            }
            WorkerRequest::StopAll => {
                self.handle_stop_all(runtime);
            }
            WorkerRequest::RebuildBody => match self.rebuild_body_stack("user request") {
                Ok(()) => {
                    // A user-requested rebuild is the active
                    // re-enable: clear the shared stop latch
                    // the user tripped, or every new turn is
                    // born cancelled (the UI clears its flags
                    // on BodyRebuilt and never offers
                    // "Tiếp tục" afterwards). Only this
                    // user-requested path clears the latch;
                    // the fail-all automatic rebuild leaves
                    // it alone.
                    self.stop_slot.replace(CancelToken::new());
                    self.send(WorkerEvent::BodyRebuilt)
                }
                Err(error) => self.send(WorkerEvent::BodyRebuildFailed { error }),
            },
            WorkerRequest::AgentFinished {
                agent_id,
                outcome,
                backend,
            } => {
                self.handle_finished(runtime, &agent_id, outcome, backend);
            }
            WorkerRequest::SpawnChild { spec, reply } => {
                self.handle_spawn_child(runtime, *spec, reply);
            }
            WorkerRequest::Rebuild { privacy_on, order } => {
                // Privacy discipline (P4.6.6): the new mode takes
                // effect for the body tools immediately, and
                // tainted frames are purged even if the chain
                // rebuild fails — a failed rebuild must not leave
                // privacy-ON screenshots lying around.
                if let Some(stack) = &self.body {
                    stack.privacy.set(privacy_on);
                    stack.frames.purge_tainted();
                }
                let profile = self.profile_runtime.profile.clone();
                let event = rebuild_chain(
                    &mut self.chain,
                    &self.rebuild,
                    &self.audit,
                    &self.event_tx,
                    privacy_on,
                    &order,
                    profile.cost_cap,
                    &profile.name,
                );
                if matches!(&event, WorkerEvent::ChainRebuilt { error: None, .. }) {
                    let endpoints: Vec<&str> = self
                        .chain
                        .as_ref()
                        .map(|built| built.provider_endpoints())
                        .unwrap_or_default();
                    self.profile_runtime = compute_runtime(
                        &profile,
                        privacy_on,
                        order,
                        &endpoints,
                        self.soul_store.as_ref(),
                        self.skill_store.as_ref(),
                        &self.audit,
                    );
                    check_soul_change(
                        &mut self.soul_baseline,
                        &self.profile_runtime,
                        &self.audit,
                        &self.event_tx,
                    );
                }
                self.send(event);
            }
            WorkerRequest::SwitchProfile { name } => {
                let (profile, warning) = self.profiles.resolve_or_default(&name);
                let profile = profile.clone();
                if let Some(stack) = &self.body {
                    stack.privacy.set(profile.privacy);
                    stack.frames.purge_tainted();
                }
                let from = self.profile_runtime.profile.name.clone();
                let event = rebuild_chain(
                    &mut self.chain,
                    &self.rebuild,
                    &self.audit,
                    &self.event_tx,
                    profile.privacy,
                    &profile.chain,
                    profile.cost_cap,
                    &profile.name,
                );
                let switched = matches!(&event, WorkerEvent::ChainRebuilt { error: None, .. });
                self.send(event);
                if switched {
                    let _ = self.audit.log(
                        "ui-profile",
                        "profile_switch",
                        None,
                        Some("switched"),
                        json!({
                            "from": from,
                            "to": profile.name,
                            "warning": warning,
                            "profile": profile.name,
                        }),
                    );
                    let endpoints: Vec<&str> = self
                        .chain
                        .as_ref()
                        .map(|built| built.provider_endpoints())
                        .unwrap_or_default();
                    self.profile_runtime = compute_runtime(
                        &profile,
                        profile.privacy,
                        profile.chain.clone(),
                        &endpoints,
                        self.soul_store.as_ref(),
                        self.skill_store.as_ref(),
                        &self.audit,
                    );
                    check_soul_change(
                        &mut self.soul_baseline,
                        &self.profile_runtime,
                        &self.audit,
                        &self.event_tx,
                    );
                    self.send(WorkerEvent::ProfileSwitched {
                        name: self.profile_runtime.profile.name.clone(),
                        greeting: self.profile_runtime.greeting.clone(),
                        soul_sent: self.profile_runtime.may_send_user_data,
                        privacy_on: self.profile_runtime.privacy_on,
                        order: self.profile_runtime.order.clone(),
                        warning,
                        soul_file: self.profile_runtime.profile.soul_file.clone(),
                    });
                }
            }
        }
    }
}

/// Display label for a spawned agent: the task text, cut to 40
/// characters (§1). Display only; the full prompt is the turn's.
fn label_for(prompt: &str) -> String {
    let trimmed = prompt.trim();
    let mut label: String = trimmed.chars().take(40).collect();
    if trimmed.chars().count() > 40 {
        label.push('…');
    }
    if label.is_empty() {
        label.push_str("Agent");
    }
    label
}

/// Map a guarded brain run to the task outcome the loop acts on.
/// Classification happens here, where the error type is still
/// available: cancellations, budget scope, and body faults.
fn map_task_outcome(
    result: Result<Result<mimint_core::TurnOutcome, MimintError>, Box<dyn std::any::Any + Send>>,
) -> TaskOutcome {
    match result {
        Ok(Ok(outcome)) => TaskOutcome::Done(Box::new(outcome)),
        Ok(Err(err)) if err.is_cancelled() => TaskOutcome::Cancelled,
        Ok(Err(err)) => {
            if let Some(scope) = err.budget_scope() {
                TaskOutcome::BudgetExhausted {
                    scope,
                    error: format!("budget_exhausted: {}", err.message()),
                }
            } else {
                let message = err.message().to_string();
                TaskOutcome::Failed {
                    body_fault: is_body_fault(&message),
                    error: message,
                }
            }
        }
        Err(payload) => TaskOutcome::Panicked(panic_message(&payload)),
    }
}

fn worker_loop(
    deps: WorkerDeps,
    request_rx: Receiver<WorkerRequest>,
    request_tx: Sender<WorkerRequest>,
    event_tx: Sender<WorkerEvent>,
) {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(deps.agents.max_agents + 2)
        .enable_all()
        .build()
    {
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
        max_observation_age,
        body,
        approval_pending,
        privacy_on: initial_privacy_on,
        profiles,
        active_profile,
        soul_store,
        skill_store,
        experience,
        agents: agents_config,
        hierarchy,
        sandbox: sandbox_config,
        routing,
        mcp_servers,
        body_factory,
        stop_flag_cell,
    } = deps;
    let actor_config = ActorConfig {
        action_timeout: Duration::from_secs(agents_config.action_timeout_secs),
        queue_sla: Duration::from_secs(agents_config.queue_sla_secs),
        max_queued_total: agents_config.max_queued_total,
        max_queued_per_agent: agents_config.max_queued_per_agent,
    };
    // Shared per-turn machinery (§3): one ledger, one slot map,
    // one sandbox, one child registry for all agents.
    let ledger = Arc::new(CostLedger::unbounded());
    let snapshot_slots = Arc::new(AgentSnapshotSlots::new());
    let sandbox = Arc::new(Sandbox::from_config(&sandbox_config));
    let children = ChildProcessRegistry::new();
    // Sandbox preflight at startup (§5): the verdict is cached,
    // so a broken sandbox denies every run from the first turn.
    // A failure is recorded, never silent — and never fatal:
    // the app still runs, execute_code just denies.
    match sandbox.preflight() {
        Ok(()) => {
            let _ = audit.log(
                "ui-startup",
                "sandbox_preflight",
                None,
                Some("ok"),
                json!({}),
            );
        }
        Err(reason) => {
            eprintln!("mimint: sandbox preflight failed ({reason}); execute_code will deny runs");
            let _ = audit.log(
                "ui-startup",
                "sandbox_preflight",
                None,
                Some("failed"),
                json!({"reason": reason}),
            );
        }
    }
    // MCP (§6): no manager on Android, none without servers.
    let mcp = if !mcp_servers.is_empty() && !cfg!(target_os = "android") {
        match McpManager::new(
            mcp_servers,
            Arc::clone(&audit),
            Arc::clone(&sandbox),
            children.clone(),
        ) {
            Ok(manager) => Some(Arc::new(manager)),
            Err(err) => {
                eprintln!("mimint: cannot start the MCP manager: {err}");
                let _ = audit.log(
                    "ui-startup",
                    "mcp_manager_failed",
                    None,
                    Some("failed"),
                    json!({"error": err.to_string()}),
                );
                None
            }
        }
    } else {
        None
    };
    if let Some(manager) = &mcp {
        let _ = manager.connect_all();
        let servers: Vec<McpServerStatus> = manager
            .server_names()
            .into_iter()
            .map(|name| {
                let handle = manager.handle(&name);
                McpServerStatus {
                    unsandboxed: handle.as_ref().is_some_and(|h| h.config().unsandboxed),
                    connected: handle
                        .as_ref()
                        .is_some_and(|h| h.protocol_version().is_some()),
                    name,
                }
            })
            .collect();
        if initial_privacy_on && !servers.is_empty() {
            // Privacy Mode ON does not limit MCP servers (§6.3):
            // say so at startup, listing every server, on
            // stderr and in the audit log.
            let names: Vec<&str> = servers.iter().map(|s| s.name.as_str()).collect();
            eprintln!(
                "mimint: Privacy Mode is ON, but MCP servers run as separate processes and may send data outside this machine: {}. Privacy Mode does not limit MCP server egress.",
                names.join(", ")
            );
            let _ = audit.log(
                "ui-startup",
                "mcp_privacy_warning",
                None,
                Some("warning"),
                json!({
                    "servers": names,
                    "privacy_on": true,
                    "note": "MCP servers run as separate processes; Privacy Mode does not limit their egress.",
                }),
            );
        }
        let _ = event_tx.send(WorkerEvent::McpStatus { servers });
    }
    let mut coordinator = AgentCoordinator::new(
        agents_config.max_agents,
        Arc::clone(&ledger),
        Arc::clone(&snapshot_slots),
        mcp,
        children,
        Arc::clone(&sandbox),
    );
    coordinator.register_default_agent(Arc::clone(&turn_slot));
    let _ = event_tx.send(WorkerEvent::AgentSpawned {
        agent_id: AGENT_DEFAULT.to_string(),
        label: "Agent 1".to_string(),
        parent: None,
    });
    // The approver is shared: brains hold it as `Arc<dyn
    // Approver>`, the loop holds the concrete Arc for
    // withdrawal and slot registration.
    let mut approver = UiApprover::new(approval_tx)
        .with_timeout(approval_timeout)
        .with_slots(Arc::clone(&stop_slot), Arc::clone(&turn_slot))
        .with_repaint(repaint)
        .with_max_observation_age(max_observation_age)
        .with_ledger(Arc::clone(&ledger));
    if let Some(flag) = approval_pending {
        approver = approver.with_pending_flag(flag);
    }
    // The actor cell: the snapshotter probes through the
    // CURRENT actor, which a body rebuild swaps (§4.1).
    let actor_cell = body
        .as_ref()
        .map(|stack| Arc::new(Mutex::new(stack.actor.clone())));
    if let Some(stack) = &body {
        let cell = Arc::clone(actor_cell.as_ref().unwrap());
        approver = approver.with_target_probe(
            Arc::new(move |agent_id, x, y| {
                cell.lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .probe_target_with_timeout_for(agent_id, x, y, Duration::from_secs(5))
                    .ok()
            }),
            Arc::clone(&snapshot_slots),
        );
        let frames = Arc::clone(&stack.frames);
        approver =
            approver.with_frame_age(Arc::new(move |agent_id| frames.latest_age_for(agent_id)));
    }
    let approver = Arc::new(approver);
    // The stop-flag cell: the bridge and the hotkey listener
    // read the CURRENT flag, which a body rebuild swaps. When
    // the caller supplied none (tests), build one from the
    // stack's flag so the bridge still follows rebuilds.
    let stop_cell = stop_flag_cell.or_else(|| {
        body.as_ref()
            .map(|stack| StopFlagCell::new(Arc::clone(&stack.stop_flag)))
    });
    // Stop bridge: when the body's stop flag trips (hotkey or
    // approval-pending trip), cancel the stop slot so the
    // running turns abort, record the latch with its context,
    // and tell the UI. It follows the cell across rebuilds and
    // reports each distinct flag's trip once.
    let bridge_exit = Arc::new(AtomicBool::new(false));
    let bridge = stop_cell.as_ref().map(|cell| {
        let cell = Arc::clone(cell);
        let slot = Arc::clone(&stop_slot);
        let tx = event_tx.clone();
        let exit = Arc::clone(&bridge_exit);
        let audit = Arc::clone(&audit);
        std::thread::spawn(move || {
            let mut reported: Option<Arc<StopFlag>> = None;
            while !exit.load(Ordering::SeqCst) {
                let current = cell.get();
                let already = reported
                    .as_ref()
                    .is_some_and(|flag| Arc::ptr_eq(flag, &current));
                if current.is_tripped() && !already {
                    slot.cancel_current();
                    // The latch is audited with its context:
                    // source, reason, trip time, and the acting
                    // agent when known (F4-16, §3).
                    let (source, reason, ts_ms, agent_id) = match current.trip_info() {
                        Some(info) => (info.source, info.reason, info.ts_ms, info.agent_id),
                        None => (
                            "unknown",
                            "emergency stop tripped".to_string(),
                            now_ms(),
                            None,
                        ),
                    };
                    let _ = audit.log(
                        "ui-control",
                        "emergency_stop",
                        None,
                        Some("stopped"),
                        json!({
                            "source": source,
                            "reason": reason,
                            "trip_ts_ms": ts_ms,
                            "agent_id": agent_id,
                        }),
                    );
                    let _ = tx.send(WorkerEvent::StopTripped {
                        reason: format!("{source}: {reason}"),
                    });
                    reported = Some(current);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        })
    });
    let chain = match FallbackChain::new(initial_providers, retry) {
        Ok(mut chain) => {
            attach_progress_hook(&mut chain, &event_tx);
            let _ = event_tx.send(WorkerEvent::ChainRebuilt {
                names: vec![chain.name().to_string()],
                error: None,
            });
            Some(Arc::new(chain))
        }
        Err(err) => {
            let _ = event_tx.send(WorkerEvent::ChainRebuilt {
                names: Vec::new(),
                error: Some(err.to_string()),
            });
            None
        }
    };
    // Initial profile runtime, from the chain the caller built.
    // An unknown startup name falls closed to the default, audited.
    let (initial_profile, initial_warning) = profiles.resolve_or_default(&active_profile);
    if let Some(warning) = &initial_warning {
        let _ = audit.log(
            "ui-profile",
            "profile_fallback",
            None,
            Some("default"),
            json!({
                "requested": active_profile,
                "warning": warning,
                "profile": initial_profile.name,
            }),
        );
    }
    // Forced-safe fallback (F5-03): every [[profile]] table was
    // dropped, so the app runs on the forced-safe profile. The
    // record names the warnings that explain why.
    if profiles.fallback_active {
        let _ = audit.log(
            "ui-profile",
            "profile_fallback",
            None,
            Some("forced_safe"),
            json!({
                "profile": initial_profile.name,
                "warnings": profiles.warnings,
                "privacy_on": true,
                "paid_locked": true,
            }),
        );
    }
    let initial_profile = initial_profile.clone();
    let endpoints: Vec<&str> = chain
        .as_ref()
        .map(|built| built.provider_endpoints())
        .unwrap_or_default();
    let profile_runtime = compute_runtime(
        &initial_profile,
        initial_privacy_on,
        initial_profile.chain.clone(),
        &endpoints,
        soul_store.as_ref(),
        skill_store.as_ref(),
        &audit,
    );
    // SOUL hash baseline (F5-11): the first load sets the
    // reference; later rebuilds of the SAME file compare
    // against it. A switch to another file resets the
    // baseline without a warning (see `check_soul_change`).
    let soul_baseline = (
        initial_profile.soul_file.clone(),
        profile_runtime.soul_hash.clone(),
    );

    let mut state = LoopState {
        chain,
        profile_runtime,
        soul_baseline,
        body,
        actor_cell,
        stop_cell,
        body_factory,
        actor_config,
        coordinator,
        approver,
        notebook,
        audit: Arc::clone(&audit),
        max_steps,
        stop_slot,
        turn_slot,
        max_observation_age,
        rebuild,
        profiles,
        soul_store,
        skill_store,
        experience,
        agents_config,
        hierarchy,
        routing,
        pending_turns: VecDeque::new(),
        pending_children: HashMap::new(),
        request_tx,
        event_tx: event_tx.clone(),
    };

    while let Ok(request) = request_rx.recv() {
        if matches!(request, WorkerRequest::Shutdown) {
            break;
        }
        // A panic in the loop must not kill the worker or
        // leave the UI stuck: catch it, report it, keep
        // serving. (Agent-task panics never reach here: the
        // task wrapper catches them first.)
        let result = catch_unwind(AssertUnwindSafe(|| {
            state.handle_request(&runtime, request);
        }));
        if let Err(payload) = result {
            let message = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_string());
            if event_tx
                .send(WorkerEvent::WorkerFault {
                    error: format!("worker thread panicked: {message}"),
                })
                .is_err()
            {
                break;
            }
        }
    }
    // Shutdown: cancel every running agent without verifying
    // ends (the process is exiting; no degraded flag is
    // needed), disconnect the MCP servers, stop the bridge.
    let running: Vec<String> = state
        .coordinator
        .agents()
        .filter(|e| e.state == AgentState::Running)
        .map(|e| e.agent_id.clone())
        .collect();
    for id in running {
        if let Some(entry) = state.coordinator.get_mut(&id) {
            entry.cancel_token.cancel();
            if let Some(handle) = entry.join_handle.take() {
                handle.abort();
            }
        }
        if let Some(stack) = &state.body {
            stack.actor.cancel_agent(&id);
        }
        state.approver.withdraw_agent(&id);
    }
    if let Some(mcp) = &state.coordinator.mcp {
        mcp.disconnect_all();
    }
    bridge_exit.store(true, Ordering::SeqCst);
    if let Some(thread) = bridge {
        let _ = thread.join();
    }
}

#[allow(clippy::too_many_arguments)]
fn rebuild_chain(
    chain: &mut Option<Arc<FallbackChain>>,
    rebuild: &Option<RebuildConfig>,
    audit: &Arc<AuditLog>,
    event_tx: &Sender<WorkerEvent>,
    privacy_on: bool,
    order: &[String],
    cost_cap: CostCap,
    profile_name: &str,
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
            "profile": profile_name,
        }),
    ) {
        return WorkerEvent::ChainRebuilt {
            names: Vec::new(),
            error: Some(format!("cannot record the Privacy Mode change: {err}")),
        };
    }
    let policy = EgressPolicy::new(privacy_on);
    let vault = default_vault();
    match mimint_core::build_chain(&rebuild.specs, order, &vault, policy, cost_cap) {
        Ok(providers) => match FallbackChain::new(providers, rebuild.retry) {
            Ok(mut new_chain) => {
                attach_progress_hook(&mut new_chain, event_tx);
                let names = vec![new_chain.name().to_string()];
                *chain = Some(Arc::new(new_chain));
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
        ApprovalDecision, BackendKind, BoxFuture, BudgetScope, CallContext, ChatMessage,
        FunctionCall, MimintError, ProfileEntry, TokenUsage, ToolCall,
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
                    usage: None,
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
                        usage: None,
                    },
                    ChatMessage {
                        role: "assistant".to_string(),
                        content: Some(text.to_string()),
                        tool_calls: None,
                        tool_call_id: None,
                        usage: None,
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
        spawn_fake_with(
            provider,
            AgentsConfig::default(),
            HierarchyConfig::default(),
        )
    }

    fn spawn_fake_with(
        provider: impl Provider + 'static,
        agents: AgentsConfig,
        hierarchy: HierarchyConfig,
    ) -> (WorkerHandle, Receiver<ApprovalJob>, std::path::PathBuf) {
        let (db_path, audit_path) = temp_paths("fake");
        let dir = db_path.parent().unwrap().to_path_buf();
        let (approval_tx, approval_rx) = approval_channel();
        let profiles = ProfileSet::resolve(&[], None, &[], &[], true, CostCap { cap_usd: None });
        let active_profile = profiles.default_profile().name.clone();
        let deps = WorkerDeps {
            initial_providers: vec![Box::new(provider) as Box<dyn Provider>],
            retry: RetryPolicy::default(),
            rebuild: None,
            notebook: Arc::new(Notebook::open(&db_path).unwrap()),
            audit: Arc::new(AuditLog::open(&audit_path).unwrap()),
            max_steps: 5,
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
        (spawn_worker(deps), approval_rx, dir)
    }

    fn wait_event(handle: &WorkerHandle) -> WorkerEvent {
        // The handle's receiver is private; poll through try_recv.
        // 10s bound: under full-suite parallel load on 2 cores the
        // old 2s bound starved (flake, 2026-11-18/19); the event
        // itself is still required, only the liveness bound grows.
        for _ in 0..1000 {
            if let Some(event) = handle.try_recv_event() {
                return event;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("no worker event within 10s");
    }

    /// Drain progress events (backend attempts + brain
    /// phases), returning the first terminal event.
    fn wait_terminal(handle: &WorkerHandle) -> WorkerEvent {
        loop {
            match wait_event(handle) {
                WorkerEvent::TryingBackend { .. } | WorkerEvent::TurnProgress { .. } => continue,
                other => return other,
            }
        }
    }

    #[test]
    fn turn_with_plain_answer_completes() {
        let (handle, _approvals, dir) = spawn_fake(FakeProvider::answering("hello back"));
        wait_startup(&handle);
        handle.run_turn("hi".to_string());
        match wait_terminal(&handle) {
            WorkerEvent::TurnDone {
                text,
                tools_used,
                partial,
                backend,
                ..
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
        wait_startup(&handle);
        handle.run_turn("hi".to_string());
        // The brain reports Analyzing before the chain reports
        // its attempt; both are progress, in that order.
        match wait_event(&handle) {
            WorkerEvent::TurnProgress {
                progress: BrainProgress::Analyzing { .. },
            } => {}
            other => panic!("expected TurnProgress Analyzing first, got: {other:?}"),
        }
        match wait_event(&handle) {
            WorkerEvent::TryingBackend { name, .. } => assert_eq!(name, "fake"),
            other => panic!("expected TryingBackend, got: {other:?}"),
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
        wait_startup(&handle);
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
        wait_startup(&handle);
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
        wait_startup(&handle);
        handle.stop_now();
        assert!(handle.is_stopped());
        // The loop reports the latch trip (no body, so no
        // bridge thread: the loop records it directly).
        match wait_event(&handle) {
            WorkerEvent::StopTripped { .. } => {}
            other => panic!("expected StopTripped, got: {other:?}"),
        }
        handle.run_turn("hi".to_string());
        match wait_terminal(&handle) {
            WorkerEvent::TurnCancelled { .. } => {}
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

    /// Provider that records the messages it receives and reports
    /// a configurable endpoint URL, so the egress-gate test can
    /// place it on either side of the loopback boundary without
    /// any real HTTP.
    struct CaptureProvider {
        endpoint: String,
        seen: Arc<std::sync::Mutex<Vec<ChatMessage>>>,
    }

    impl Provider for CaptureProvider {
        fn name(&self) -> &str {
            "capture"
        }

        fn model(&self) -> &str {
            "capture-model"
        }

        fn endpoint_url(&self) -> &str {
            &self.endpoint
        }

        fn is_paid(&self) -> bool {
            false
        }

        fn chat<'a>(
            &'a self,
            _ctx: &'a CallContext<'a>,
            messages: &'a [ChatMessage],
            _tools: &'a [serde_json::Value],
        ) -> BoxFuture<'a, Result<ChatMessage, MimintError>> {
            *self.seen.lock().unwrap() = messages.to_vec();
            Box::pin(async move {
                Ok(ChatMessage {
                    role: "assistant".to_string(),
                    content: Some("done".to_string()),
                    tool_calls: None,
                    tool_call_id: None,
                    usage: None,
                })
            })
        }
    }

    /// Spawn a worker whose data dir holds a SOUL file and one
    /// promoted skill with marker content, behind a capturing
    /// provider at `endpoint`. Privacy Mode is ON.
    fn spawn_capture(
        endpoint: &str,
    ) -> (
        WorkerHandle,
        Arc<std::sync::Mutex<Vec<ChatMessage>>>,
        std::path::PathBuf,
    ) {
        use mimint_core::{
            Checker, SkillDef, SkillStatus, SkillStore, ValidationRun, ValidationTask,
        };
        let (db_path, audit_path) = temp_paths("capture");
        let dir = db_path.parent().unwrap().to_path_buf();
        std::fs::write(dir.join("SOUL.md"), "SOUL-BODY-MARKER chào bạn\n").unwrap();
        let store = SkillStore::new(dir.join("skills"));
        let mut def = SkillDef {
            name: "ghi-chu".to_string(),
            description: "Lưu ghi chú.".to_string(),
            triggers: vec!["lưu ghi chú".to_string()],
            content: "SKILL-CONTENT-MARKER: lưu ghi chú ngay.".to_string(),
            version: 1,
            author: "test".to_string(),
            validation_tasks: vec![ValidationTask {
                id: "t1".to_string(),
                prompt: "Lưu ghi chú: x".to_string(),
                checker: Checker::ResponseNonEmpty,
            }],
            status: SkillStatus::Promoted,
            validation_history: Vec::new(),
            imported: false,
            import_approved: false,
        };
        // A genuinely usable promotion: a passing run bound to
        // the current content hash (the F5-06 predicate requires
        // it, so the fixture carries it from the start).
        let hash = def.content_hash();
        def.validation_history = vec![ValidationRun {
            ts_ms: now_ms(),
            content_hash: hash,
            passed: 1,
            total: 1,
            results: Vec::new(),
        }];
        store.save(&def).unwrap();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = CaptureProvider {
            endpoint: endpoint.to_string(),
            seen: Arc::clone(&seen),
        };
        let (approval_tx, _approval_rx) = approval_channel();
        let profiles = ProfileSet::resolve(&[], None, &[], &[], true, CostCap { cap_usd: None });
        let active_profile = profiles.default_profile().name.clone();
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
            approval_timeout: Duration::from_secs(60),
            max_observation_age: Duration::from_secs(300),
            body: None,
            approval_pending: None,
            privacy_on: true,
            profiles,
            active_profile,
            soul_store: Some(SoulStore::new(dir.clone())),
            skill_store: Some(SkillStore::new(dir.join("skills"))),
            experience: None,
            agents: AgentsConfig::default(),
            hierarchy: HierarchyConfig::default(),
            sandbox: SandboxConfig::default(),
            routing: RoutingConfig::default(),
            mcp_servers: Vec::new(),
            body_factory: None,
            stop_flag_cell: None,
        };
        (spawn_worker(deps), seen, dir)
    }

    fn joined_messages(seen: &Arc<std::sync::Mutex<Vec<ChatMessage>>>) -> String {
        seen.lock()
            .unwrap()
            .iter()
            .filter_map(|message| message.content.clone())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn skills_follow_the_soul_egress_gate() {
        // Non-loopback endpoint + Privacy ON: neither SOUL nor
        // skill content may reach the provider (F5-01).
        let (handle, seen, dir) = spawn_capture("https://example.com/v1");
        wait_startup(&handle);
        handle.run_turn("lưu ghi chú mua sữa".to_string());
        assert!(matches!(
            wait_terminal(&handle),
            WorkerEvent::TurnDone { .. }
        ));
        let joined = joined_messages(&seen);
        assert!(
            !joined.contains("SKILL-CONTENT-MARKER"),
            "skill content must be withheld: {joined}"
        );
        assert!(
            !joined.contains("SOUL-BODY-MARKER"),
            "SOUL must be withheld: {joined}"
        );
        drop(handle);
        let _ = std::fs::remove_dir_all(&dir);

        // Loopback endpoint + Privacy ON: both reach the provider.
        let (handle, seen, dir) = spawn_capture("http://127.0.0.1:9/v1");
        wait_startup(&handle);
        handle.run_turn("lưu ghi chú mua sữa".to_string());
        assert!(matches!(
            wait_terminal(&handle),
            WorkerEvent::TurnDone { .. }
        ));
        let joined = joined_messages(&seen);
        assert!(
            joined.contains("SKILL-CONTENT-MARKER"),
            "skill content must be sent to loopback: {joined}"
        );
        assert!(
            joined.contains("SOUL-BODY-MARKER"),
            "SOUL must be sent to loopback: {joined}"
        );
        drop(handle);
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

    #[test]
    fn profile_switch_rebuilds_and_tags_turn_audit() {
        let (db_path, audit_path) = temp_paths("switch");
        let dir = db_path.parent().unwrap().to_path_buf();
        std::fs::write(dir.join("SOUL.md"), "Chào mặc định\n").unwrap();
        std::fs::write(dir.join("work.md"), "Chào làm việc\n").unwrap();
        // A dead loopback port: the rebuilt chain constructs
        // offline, and a turn through it fails fast without
        // touching any real backend.
        let specs = vec![BackendSpec {
            name: "local".to_string(),
            kind: BackendKind::Ollama,
            model: "dead-model".to_string(),
            base_url: Some("http://127.0.0.1:9".to_string()),
            key_env: None,
            key_file: None,
            key_keychain: None,
            timeout: Duration::from_secs(5),
            max_tokens: None,
            allow_plaintext_auth: false,
        }];
        let entry = |name: &str, soul: Option<&str>| ProfileEntry {
            name: name.to_string(),
            soul: soul.map(str::to_string),
            chain: Some(vec!["local".to_string()]),
            voice: None,
            skills: None,
            proactive: None,
            budget_usd: None,
            privacy: None,
            cost_cap_usd: None,
            avatar: None,
        };
        let profiles = ProfileSet::resolve(
            &[entry("Mặc định", None), entry("Làm việc", Some("work.md"))],
            None,
            &specs,
            &["local".to_string()],
            true,
            CostCap { cap_usd: None },
        );
        let (approval_tx, _approval_rx) = approval_channel();
        let deps = WorkerDeps {
            initial_providers: vec![Box::new(FakeProvider::answering("hi"))],
            retry: RetryPolicy::default(),
            rebuild: Some(RebuildConfig {
                specs: specs.clone(),
                retry: RetryPolicy::default(),
            }),
            notebook: Arc::new(Notebook::open(&db_path).unwrap()),
            audit: Arc::new(AuditLog::open(&audit_path).unwrap()),
            max_steps: 5,
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
            active_profile: "Mặc định".to_string(),
            soul_store: Some(SoulStore::new(dir.clone())),
            skill_store: None,
            experience: None,
            agents: AgentsConfig::default(),
            hierarchy: HierarchyConfig::default(),
            sandbox: SandboxConfig::default(),
            routing: RoutingConfig::default(),
            mcp_servers: Vec::new(),
            body_factory: None,
            stop_flag_cell: None,
        };
        let handle = spawn_worker(deps);
        wait_startup(&handle);
        handle.switch_profile("Làm việc".to_string());
        match wait_event(&handle) {
            WorkerEvent::ChainRebuilt { error: None, .. } => {}
            other => panic!("expected ChainRebuilt ok, got: {other:?}"),
        }
        match wait_event(&handle) {
            WorkerEvent::ProfileSwitched {
                name,
                greeting,
                soul_sent,
                privacy_on,
                order,
                warning,
                soul_file,
            } => {
                assert_eq!(name, "Làm việc");
                assert_eq!(greeting, "Chào làm việc");
                assert!(soul_sent);
                assert!(privacy_on);
                assert_eq!(soul_file, "work.md");
                assert_eq!(order, vec!["local".to_string()]);
                assert_eq!(warning, None);
            }
            other => panic!("expected ProfileSwitched, got: {other:?}"),
        }
        // A turn through the dead backend fails fast; its
        // provider records must carry the new profile's tag.
        handle.run_turn("hi".to_string());
        assert!(matches!(
            wait_terminal(&handle),
            WorkerEvent::TurnFailed { .. }
        ));
        drop(handle);
        let audit_text = std::fs::read_to_string(&audit_path).unwrap();
        let tagged = audit_text
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .any(|record| {
                (record["kind"] == "provider_attempt" || record["kind"] == "provider_failure")
                    && record["detail"]["profile"] == "Làm việc"
            });
        assert!(
            tagged,
            "no provider record tagged with the switched profile:\n{audit_text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Drain startup events until the initial chain report,
    /// asserting the default agent was announced first.
    fn wait_startup(handle: &WorkerHandle) {
        let mut saw_default = false;
        for _ in 0..500 {
            match wait_event(handle) {
                WorkerEvent::AgentSpawned { agent_id, .. } if agent_id == "agent-001" => {
                    saw_default = true;
                }
                WorkerEvent::ChainRebuilt { error: None, .. } => {
                    assert!(
                        saw_default,
                        "the default agent must be announced at startup"
                    );
                    return;
                }
                WorkerEvent::ChainRebuilt {
                    error: Some(error), ..
                } => {
                    panic!("initial chain build failed: {error}")
                }
                WorkerEvent::McpStatus { .. } => {}
                other => panic!("unexpected startup event: {other:?}"),
            }
        }
        panic!("startup never completed");
    }

    /// Provider whose calls rendezvous: both turns must be
    /// inside `chat` at the same time, or the call fails. A
    /// serialized scheduler can never pass this test.
    struct BarrierProvider {
        barrier: Arc<tokio::sync::Barrier>,
    }

    impl Provider for BarrierProvider {
        fn name(&self) -> &str {
            "barrier"
        }

        fn model(&self) -> &str {
            "barrier-model"
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
            let barrier = Arc::clone(&self.barrier);
            Box::pin(async move {
                tokio::time::timeout(Duration::from_secs(10), barrier.wait())
                    .await
                    .map_err(|_| {
                        MimintError::Tool(
                            "barrier timeout: the two turns never overlapped".to_string(),
                        )
                    })?;
                Ok(ChatMessage {
                    role: "assistant".to_string(),
                    content: Some("barrier done".to_string()),
                    tool_calls: None,
                    tool_call_id: None,
                    usage: None,
                })
            })
        }
    }

    #[test]
    fn two_parallel_turns_run_concurrently() {
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let (handle, _approvals, dir) = spawn_fake_with(
            BarrierProvider {
                barrier: Arc::clone(&barrier),
            },
            AgentsConfig {
                parallelism_enabled: true,
                ..Default::default()
            },
            HierarchyConfig::default(),
        );
        wait_startup(&handle);
        handle.run_turn("one".to_string());
        handle.run_turn("two".to_string());
        let mut done: Vec<String> = Vec::new();
        let mut spawned: Vec<String> = Vec::new();
        for _ in 0..500 {
            match wait_event(&handle) {
                WorkerEvent::TurnDone { agent_id, .. } => {
                    done.push(agent_id);
                    if done.len() == 2 {
                        break;
                    }
                }
                WorkerEvent::AgentSpawned { agent_id, .. } => spawned.push(agent_id),
                WorkerEvent::TryingBackend { .. }
                | WorkerEvent::TurnProgress { .. }
                | WorkerEvent::AgentEnded { .. } => {}
                other => panic!("unexpected event: {other:?}"),
            }
        }
        assert_eq!(done.len(), 2, "both turns must complete");
        done.sort();
        assert_eq!(done, vec!["agent-001".to_string(), "agent-002".to_string()]);
        assert!(spawned.contains(&"agent-002".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Provider that sleeps per agent: the default agent
    /// answers after 1.5s, every other agent after 30s — long
    /// enough that only a cancel ends it within the test.
    struct SleeperProvider;

    impl Provider for SleeperProvider {
        fn name(&self) -> &str {
            "sleeper"
        }

        fn model(&self) -> &str {
            "sleeper-model"
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
            let delay = if ctx.agent_id == "agent-001" {
                Duration::from_millis(1500)
            } else {
                Duration::from_secs(30)
            };
            Box::pin(async move {
                tokio::time::sleep(delay).await;
                Ok(ChatMessage {
                    role: "assistant".to_string(),
                    content: Some("slept".to_string()),
                    tool_calls: None,
                    tool_call_id: None,
                    usage: None,
                })
            })
        }
    }

    #[test]
    fn cancel_agent_stops_only_that_agent() {
        let (handle, _approvals, dir) = spawn_fake_with(
            SleeperProvider,
            AgentsConfig {
                parallelism_enabled: true,
                ..Default::default()
            },
            HierarchyConfig::default(),
        );
        wait_startup(&handle);
        handle.run_turn("one".to_string());
        handle.run_turn("two".to_string());
        // Wait until the second agent exists, then cancel it.
        for _ in 0..500 {
            match wait_event(&handle) {
                WorkerEvent::AgentSpawned { agent_id, .. } if agent_id == "agent-002" => break,
                WorkerEvent::TryingBackend { .. } | WorkerEvent::TurnProgress { .. } => {}
                other => panic!("unexpected event before cancel: {other:?}"),
            }
        }
        handle.cancel_agent("agent-002".to_string());
        let mut cancelled: Vec<String> = Vec::new();
        let mut done: Vec<String> = Vec::new();
        for _ in 0..500 {
            match wait_event(&handle) {
                WorkerEvent::TurnCancelled { agent_id } => cancelled.push(agent_id),
                WorkerEvent::TurnDone { agent_id, .. } => done.push(agent_id),
                WorkerEvent::TryingBackend { .. }
                | WorkerEvent::TurnProgress { .. }
                | WorkerEvent::AgentEnded { .. } => {}
                other => panic!("unexpected event: {other:?}"),
            }
            if !cancelled.is_empty() && !done.is_empty() {
                break;
            }
        }
        assert_eq!(cancelled, vec!["agent-002".to_string()]);
        assert_eq!(done, vec!["agent-001".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hierarchy_child_completes_through_delegate() {
        let (handle, approvals, dir) = spawn_fake_with(
            FakeProvider::tool_then_text("delegate", r#"{"task":"child task"}"#, "parent done"),
            AgentsConfig {
                parallelism_enabled: true,
                ..Default::default()
            },
            HierarchyConfig {
                enabled: true,
                max_children: 2,
                default_child_steps: 10,
                default_child_time_secs: 300,
            },
        );
        wait_startup(&handle);
        handle.run_turn("do it via a child".to_string());
        let job = approvals
            .recv_timeout(Duration::from_secs(5))
            .expect("delegate approval job must arrive");
        assert_eq!(job.tool, "delegate");
        assert_eq!(job.agent_id, "agent-001");
        job.reply.send(ApprovalDecision::Allow).unwrap();
        let mut child_spawned = false;
        for _ in 0..500 {
            match wait_event(&handle) {
                WorkerEvent::TurnDone {
                    agent_id,
                    text,
                    tools_used,
                    ..
                } => {
                    assert_eq!(agent_id, "agent-001");
                    assert_eq!(text, "parent done");
                    assert_eq!(tools_used, vec!["delegate".to_string()]);
                    break;
                }
                WorkerEvent::AgentSpawned {
                    parent: Some(parent),
                    ..
                } => {
                    assert_eq!(parent, "agent-001");
                    child_spawned = true;
                }
                WorkerEvent::TryingBackend { .. }
                | WorkerEvent::TurnProgress { .. }
                | WorkerEvent::AgentSpawned { .. }
                | WorkerEvent::AgentEnded { .. } => {}
                other => panic!("unexpected event: {other:?}"),
            }
        }
        assert!(child_spawned, "a child agent must have spawned");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- Phase 6 gate tests: failure isolation (§4.4, §11) ----

    use mimint_body::{Fault, MockBody};

    /// Spawn a worker with a real body stack (actor + frames +
    /// stop flag) built from `body`, and a factory that builds
    /// a fresh healthy [`MockBody`] on every rebuild. Returns
    /// the handle, the approval receiver, the temp dir, and the
    /// initial stop flag (for trip tests).
    fn spawn_fake_with_body(
        provider: impl Provider + 'static,
        agents: AgentsConfig,
        hierarchy: HierarchyConfig,
        body: Box<dyn Body>,
    ) -> (
        WorkerHandle,
        Receiver<ApprovalJob>,
        std::path::PathBuf,
        Arc<StopFlag>,
    ) {
        let (db_path, audit_path) = temp_paths("body");
        let dir = db_path.parent().unwrap().to_path_buf();
        let (approval_tx, approval_rx) = approval_channel();
        let profiles = ProfileSet::resolve(&[], None, &[], &[], true, CostCap { cap_usd: None });
        let active_profile = profiles.default_profile().name.clone();
        let audit = Arc::new(AuditLog::open(&audit_path).unwrap());
        let stop_flag = StopFlag::new();
        let actor_config = ActorConfig {
            action_timeout: Duration::from_secs(agents.action_timeout_secs),
            queue_sla: Duration::from_secs(agents.queue_sla_secs),
            max_queued_total: agents.max_queued_total,
            max_queued_per_agent: agents.max_queued_per_agent,
        };
        let actor = BodyActor::spawn_with_config_and_audit(
            body,
            Arc::clone(&stop_flag),
            actor_config,
            Some(Arc::clone(&audit)),
        );
        let stack = BodyStack {
            actor,
            frames: Arc::new(FrameStore::new()),
            privacy: PrivacyFlag::new(true),
            stop_flag: Arc::clone(&stop_flag),
        };
        let factory: BodyFactory =
            Arc::new(|| Ok((Box::new(MockBody::new()) as Box<dyn Body>, StopFlag::new())));
        let deps = WorkerDeps {
            initial_providers: vec![Box::new(provider) as Box<dyn Provider>],
            retry: RetryPolicy::default(),
            rebuild: None,
            notebook: Arc::new(Notebook::open(&db_path).unwrap()),
            audit,
            max_steps: 5,
            approval_tx,
            stop_slot: TokenSlot::new(),
            turn_slot: TokenSlot::new(),
            repaint: RepaintSignal::new(),
            approval_timeout: Duration::from_secs(60),
            max_observation_age: Duration::from_secs(300),
            body: Some(stack),
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
            body_factory: Some(factory),
            stop_flag_cell: None,
        };
        (spawn_worker(deps), approval_rx, dir, stop_flag)
    }

    /// Auto-allow every approval card on a detached thread.
    fn auto_allow(approvals: Receiver<ApprovalJob>) {
        std::thread::spawn(move || {
            while let Ok(job) = approvals.recv() {
                let _ = job.reply.send(ApprovalDecision::Allow);
            }
        });
    }

    /// Parse the audit file, failing on any malformed line:
    /// the trail must stay intact JSONL under every fault.
    fn audit_records(path: &std::path::Path) -> Vec<serde_json::Value> {
        let text = std::fs::read_to_string(path).unwrap();
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("audit line must be valid JSON"))
            .collect()
    }

    fn text_msg(text: &str) -> ChatMessage {
        ChatMessage {
            role: "assistant".to_string(),
            content: Some(text.to_string()),
            tool_calls: None,
            tool_call_id: None,
            usage: None,
        }
    }

    fn tool_msg(tool: &str, args: &str) -> ChatMessage {
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
            usage: None,
        }
    }

    /// agent-001 clicks once, then answers; every other agent
    /// sleeps 30s inside the provider call (only a cancel ends
    /// it within the test).
    struct ClickerAndSleeper {
        calls: std::sync::Mutex<std::collections::HashMap<String, usize>>,
    }

    impl Provider for ClickerAndSleeper {
        fn name(&self) -> &str {
            "clicker-sleeper"
        }

        fn model(&self) -> &str {
            "clicker-model"
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
            let mut calls = self.calls.lock().unwrap();
            let n = calls.entry(ctx.agent_id.to_string()).or_insert(0);
            *n += 1;
            let n = *n;
            let agent = ctx.agent_id.to_string();
            Box::pin(async move {
                if agent == "agent-001" {
                    if n == 1 {
                        Ok(tool_msg("body_click", r#"{"x": 10, "y": 10}"#))
                    } else {
                        Ok(text_msg("alive"))
                    }
                } else {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    Ok(text_msg("slept"))
                }
            })
        }
    }

    /// Every agent clicks once, then answers.
    struct AllClickOnce {
        calls: std::sync::Mutex<std::collections::HashMap<String, usize>>,
    }

    impl Provider for AllClickOnce {
        fn name(&self) -> &str {
            "all-click"
        }

        fn model(&self) -> &str {
            "all-click-model"
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
            let mut calls = self.calls.lock().unwrap();
            let n = calls.entry(ctx.agent_id.to_string()).or_insert(0);
            *n += 1;
            let first = *n == 1;
            Box::pin(async move {
                if first {
                    Ok(tool_msg("body_click", r#"{"x": 10, "y": 10}"#))
                } else {
                    Ok(text_msg("done"))
                }
            })
        }
    }

    /// Every agent sleeps 30s inside the provider call.
    struct AllSleep;

    impl Provider for AllSleep {
        fn name(&self) -> &str {
            "all-sleep"
        }

        fn model(&self) -> &str {
            "all-sleep-model"
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
            Box::pin(async move {
                tokio::time::sleep(Duration::from_secs(30)).await;
                Ok(text_msg("slept"))
            })
        }
    }

    /// Body that panics on click, counting clicks for the
    /// rebuild test (the old actor must receive no new action
    /// after a rebuild).
    struct PanicBody {
        clicks: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Body for PanicBody {
        fn name(&self) -> &'static str {
            "panic"
        }

        fn display_size(&mut self) -> Result<(u32, u32), mimint_body::BodyError> {
            Ok((800, 600))
        }

        fn screenshot(&mut self) -> Result<mimint_body::Framebuffer, mimint_body::BodyError> {
            Ok(mimint_body::Framebuffer::new(2, 2, vec![0u8; 16]).unwrap())
        }

        fn click(&mut self, _x: i32, _y: i32) -> Result<(), mimint_body::BodyError> {
            self.clicks
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            panic!("injected body panic");
        }

        fn mouse_move(&mut self, _x: i32, _y: i32) -> Result<(), mimint_body::BodyError> {
            Ok(())
        }

        fn type_text(&mut self, _text: &str) -> Result<(), mimint_body::BodyError> {
            Ok(())
        }

        fn key(&mut self, _key: mimint_body::Key) -> Result<(), mimint_body::BodyError> {
            Ok(())
        }

        fn permissions(&mut self) -> mimint_body::Permissions {
            mimint_body::Permissions {
                screen_capture: mimint_body::PermissionState::Granted,
                input_control: mimint_body::PermissionState::Granted,
            }
        }
    }

    /// Body that sleeps 200ms, then panics on click: the
    /// second agent's action is queued when the actor thread
    /// dies, so it observes the disconnect.
    struct SleepThenPanicBody;

    impl Body for SleepThenPanicBody {
        fn name(&self) -> &'static str {
            "sleep-panic"
        }

        fn display_size(&mut self) -> Result<(u32, u32), mimint_body::BodyError> {
            Ok((800, 600))
        }

        fn screenshot(&mut self) -> Result<mimint_body::Framebuffer, mimint_body::BodyError> {
            Ok(mimint_body::Framebuffer::new(2, 2, vec![0u8; 16]).unwrap())
        }

        fn click(&mut self, _x: i32, _y: i32) -> Result<(), mimint_body::BodyError> {
            std::thread::sleep(Duration::from_millis(200));
            panic!("injected body panic after a delay");
        }

        fn mouse_move(&mut self, _x: i32, _y: i32) -> Result<(), mimint_body::BodyError> {
            Ok(())
        }

        fn type_text(&mut self, _text: &str) -> Result<(), mimint_body::BodyError> {
            Ok(())
        }

        fn key(&mut self, _key: mimint_body::Key) -> Result<(), mimint_body::BodyError> {
            Ok(())
        }

        fn permissions(&mut self) -> mimint_body::Permissions {
            mimint_body::Permissions {
                screen_capture: mimint_body::PermissionState::Granted,
                input_control: mimint_body::PermissionState::Granted,
            }
        }
    }

    /// Collect non-progress events until `done` returns true
    /// for the collected set, or `max` events pass.
    fn collect_until(
        handle: &WorkerHandle,
        max: usize,
        done: impl Fn(&[WorkerEvent]) -> bool,
    ) -> Vec<WorkerEvent> {
        let mut seen = Vec::new();
        for _ in 0..max {
            match wait_event(handle) {
                WorkerEvent::TryingBackend { .. } | WorkerEvent::TurnProgress { .. } => {}
                other => {
                    seen.push(other);
                    if done(&seen) {
                        break;
                    }
                }
            }
        }
        seen
    }

    fn find_scope_record(records: &[serde_json::Value]) -> &serde_json::Value {
        records
            .iter()
            .find(|r| r["kind"] == "agent_failure_scope")
            .expect("agent_failure_scope record")
    }

    #[test]
    fn g6_9_body_hang_fails_all() {
        let (handle, approvals, dir, _flag) = spawn_fake_with_body(
            ClickerAndSleeper {
                calls: std::sync::Mutex::new(std::collections::HashMap::new()),
            },
            AgentsConfig {
                parallelism_enabled: true,
                action_timeout_secs: 1,
                ..Default::default()
            },
            HierarchyConfig::default(),
            Box::new(MockBody::with_fault(Fault::Hang)),
        );
        auto_allow(approvals);
        wait_startup(&handle);
        handle.run_turn("click".to_string());
        handle.spawn_agent("sleep".to_string());
        let seen = collect_until(&handle, 500, |seen| {
            seen.iter()
                .any(|e| matches!(e, WorkerEvent::AgentFault { .. }))
                && seen
                    .iter()
                    .any(|e| matches!(e, WorkerEvent::TurnFailed { .. }))
                && seen
                    .iter()
                    .any(|e| matches!(e, WorkerEvent::TurnCancelled { .. }))
        });
        // The clicking agent failed with the body fault; the
        // sleeping agent was cancelled by fail-all.
        let failed = seen.iter().find_map(|e| match e {
            WorkerEvent::TurnFailed {
                agent_id, error, ..
            } => Some((agent_id.clone(), error.clone())),
            _ => None,
        });
        let (failed_id, failed_error) = failed.expect("TurnFailed");
        assert_eq!(failed_id, "agent-001");
        assert!(
            failed_error.contains("unknown_outcome"),
            "body fault must surface: {failed_error}"
        );
        let cancelled: Vec<&str> = seen
            .iter()
            .filter_map(|e| match e {
                WorkerEvent::TurnCancelled { agent_id } => Some(agent_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(cancelled, vec!["agent-002"]);
        assert!(
            !seen
                .iter()
                .any(|e| matches!(e, WorkerEvent::TurnDone { .. })),
            "no agent may complete after a body fault"
        );
        let fault = seen.iter().find_map(|e| match e {
            WorkerEvent::AgentFault {
                agent_id,
                scope,
                cause,
            } => Some((agent_id.clone(), *scope, cause.clone())),
            _ => None,
        });
        let (fault_id, scope, cause) = fault.expect("AgentFault");
        assert_eq!(fault_id, "agent-001");
        assert_eq!(scope, FailureScope::All);
        assert!(cause.contains("unknown_outcome"), "{cause}");
        // The audit trail stays intact JSONL, the scope record
        // is unambiguous, and no body action executed after
        // the fault (the hanging click only ever errored).
        let records = audit_records(&dir.join("audit.jsonl"));
        let scope_record = find_scope_record(&records);
        assert_eq!(scope_record["detail"]["scope"], "all");
        assert_eq!(scope_record["detail"]["failed_agent"], "agent-001");
        assert_eq!(scope_record["detail"]["actor_broken"], true);
        assert!(scope_record["detail"]["stop_latched"].is_boolean());
        assert!(scope_record["detail"]["end_verified"].is_boolean());
        assert!(scope_record["detail"]["end_verification"].is_string());
        assert!(
            !records.iter().any(|r| r["kind"] == "execution"
                && r["tool"] == "body_click"
                && r["decision"] == "executed"),
            "no body action may execute after the fault"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn g6_9_body_panic_fails_all() {
        let clicks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (handle, approvals, dir, _flag) = spawn_fake_with_body(
            ClickerAndSleeper {
                calls: std::sync::Mutex::new(std::collections::HashMap::new()),
            },
            AgentsConfig {
                parallelism_enabled: true,
                ..Default::default()
            },
            HierarchyConfig::default(),
            Box::new(PanicBody {
                clicks: Arc::clone(&clicks),
            }),
        );
        auto_allow(approvals);
        wait_startup(&handle);
        handle.run_turn("click".to_string());
        handle.spawn_agent("sleep".to_string());
        let seen = collect_until(&handle, 500, |seen| {
            seen.iter()
                .any(|e| matches!(e, WorkerEvent::AgentFault { .. }))
                && seen
                    .iter()
                    .any(|e| matches!(e, WorkerEvent::TurnFailed { .. }))
                && seen
                    .iter()
                    .any(|e| matches!(e, WorkerEvent::TurnCancelled { .. }))
        });
        let failed_error = seen.iter().find_map(|e| match e {
            WorkerEvent::TurnFailed { error, .. } => Some(error.clone()),
            _ => None,
        });
        let failed_error = failed_error.expect("TurnFailed");
        assert!(
            failed_error.contains("unknown_outcome"),
            "panic must surface as unknown_outcome: {failed_error}"
        );
        assert!(seen.iter().any(|e| matches!(
            e,
            WorkerEvent::AgentFault {
                scope: FailureScope::All,
                ..
            }
        )));
        let records = audit_records(&dir.join("audit.jsonl"));
        let scope_record = find_scope_record(&records);
        assert_eq!(scope_record["detail"]["scope"], "all");
        assert_eq!(scope_record["detail"]["actor_broken"], true);
        // The app did not crash: the worker still serves a
        // plain turn after the panic + automatic rebuild.
        handle.run_turn("alive check".to_string());
        let seen2 = collect_until(&handle, 500, |seen| {
            seen.iter()
                .any(|e| matches!(e, WorkerEvent::TurnDone { .. }))
        });
        assert!(seen2.iter().any(|e| matches!(
            e,
            WorkerEvent::TurnDone {
                text, ..
            } if text == "alive"
        )));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn g6_9_body_disconnect_fails_all() {
        let (handle, approvals, dir, _flag) = spawn_fake_with_body(
            AllClickOnce {
                calls: std::sync::Mutex::new(std::collections::HashMap::new()),
            },
            AgentsConfig {
                parallelism_enabled: true,
                ..Default::default()
            },
            HierarchyConfig::default(),
            Box::new(SleepThenPanicBody),
        );
        auto_allow(approvals);
        wait_startup(&handle);
        handle.run_turn("click one".to_string());
        handle.spawn_agent("click two".to_string());
        let seen = collect_until(&handle, 500, |seen| {
            seen.iter()
                .any(|e| matches!(e, WorkerEvent::AgentFault { .. }))
                && seen
                    .iter()
                    .filter(|e| {
                        matches!(
                            e,
                            WorkerEvent::TurnFailed { .. } | WorkerEvent::TurnCancelled { .. }
                        )
                    })
                    .count()
                    >= 2
        });
        // Both agents ended failed or cancelled; neither
        // completed. At least one failure names the body
        // fault (the panic or the dead thread behind it).
        assert!(
            !seen
                .iter()
                .any(|e| matches!(e, WorkerEvent::TurnDone { .. })),
            "no agent may complete after the actor thread died"
        );
        let failures: Vec<String> = seen
            .iter()
            .filter_map(|e| match e {
                WorkerEvent::TurnFailed { error, .. } => Some(error.clone()),
                _ => None,
            })
            .collect();
        assert!(!failures.is_empty(), "at least one agent must fail");
        assert!(
            failures.iter().any(|e| e.contains("unknown_outcome")),
            "a body fault must surface: {failures:?}"
        );
        assert!(seen.iter().any(|e| matches!(
            e,
            WorkerEvent::AgentFault {
                scope: FailureScope::All,
                ..
            }
        )));
        let records = audit_records(&dir.join("audit.jsonl"));
        let scope_record = find_scope_record(&records);
        assert_eq!(scope_record["detail"]["scope"], "all");
        assert!(scope_record["detail"]["end_verified"].is_boolean());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn g6_9_provider_failure_isolates_single() {
        struct FailingForOne;
        impl Provider for FailingForOne {
            fn name(&self) -> &str {
                "failing-one"
            }

            fn model(&self) -> &str {
                "failing-model"
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
                Box::pin(async move {
                    if agent == "agent-001" {
                        Err(MimintError::Provider(
                            "permanent provider failure for agent-001".to_string(),
                        ))
                    } else {
                        Ok(text_msg("B done"))
                    }
                })
            }
        }
        let (handle, _approvals, dir) = spawn_fake_with(
            FailingForOne,
            AgentsConfig {
                parallelism_enabled: true,
                ..Default::default()
            },
            HierarchyConfig::default(),
        );
        wait_startup(&handle);
        handle.run_turn("fail".to_string());
        handle.spawn_agent("succeed".to_string());
        let seen = collect_until(&handle, 500, |seen| {
            seen.iter()
                .any(|e| matches!(e, WorkerEvent::TurnFailed { .. }))
                && seen
                    .iter()
                    .any(|e| matches!(e, WorkerEvent::TurnDone { .. }))
        });
        let failed_id = seen.iter().find_map(|e| match e {
            WorkerEvent::TurnFailed { agent_id, .. } => Some(agent_id.clone()),
            _ => None,
        });
        assert_eq!(failed_id.as_deref(), Some("agent-001"));
        let done = seen.iter().find_map(|e| match e {
            WorkerEvent::TurnDone { agent_id, text, .. } => Some((agent_id.clone(), text.clone())),
            _ => None,
        });
        assert_eq!(done, Some(("agent-002".to_string(), "B done".to_string())));
        assert!(
            !seen
                .iter()
                .any(|e| matches!(e, WorkerEvent::AgentFault { .. })),
            "a provider failure is not a fault event"
        );
        let records = audit_records(&dir.join("audit.jsonl"));
        assert!(
            !records.iter().any(|r| r["kind"] == "agent_failure_scope"),
            "no scope record for a single-agent failure"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn g6_9_agent_task_panic_isolates_single() {
        struct PanickingForOne {
            panicked: std::sync::atomic::AtomicBool,
        }
        impl Provider for PanickingForOne {
            fn name(&self) -> &str {
                "panicking-one"
            }

            fn model(&self) -> &str {
                "panicking-model"
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
                // One-shot for agent-001 only: the flag must be
                // consumed by agent-001's own first call. The
                // previous form swapped on EVERY call, so a
                // concurrent agent-002 call racing ahead would
                // consume the flag and agent-001 would never
                // panic (flake: no AgentFault, then the 10s
                // wait_event timeout). Other agents never
                // touch the flag; short-circuit keeps the swap
                // agent-001-only.
                let first = agent == "agent-001"
                    && !self
                        .panicked
                        .swap(true, std::sync::atomic::Ordering::SeqCst);
                Box::pin(async move {
                    if agent == "agent-001" && first {
                        panic!("injected agent task panic");
                    }
                    Ok(text_msg("B done"))
                })
            }
        }
        let (handle, _approvals, dir) = spawn_fake_with(
            PanickingForOne {
                panicked: std::sync::atomic::AtomicBool::new(false),
            },
            AgentsConfig {
                parallelism_enabled: true,
                ..Default::default()
            },
            HierarchyConfig::default(),
        );
        wait_startup(&handle);
        handle.run_turn("panic".to_string());
        handle.spawn_agent("succeed".to_string());
        let seen = collect_until(&handle, 500, |seen| {
            seen.iter()
                .any(|e| matches!(e, WorkerEvent::AgentFault { .. }))
                && seen
                    .iter()
                    .any(|e| matches!(e, WorkerEvent::TurnDone { .. }))
        });
        let fault = seen.iter().find_map(|e| match e {
            WorkerEvent::AgentFault {
                agent_id,
                scope,
                cause,
            } => Some((agent_id.clone(), *scope, cause.clone())),
            _ => None,
        });
        let (fault_id, scope, cause) = fault.expect("AgentFault");
        assert_eq!(fault_id, "agent-001");
        assert_eq!(scope, FailureScope::Single);
        assert!(cause.contains("injected agent task panic"), "{cause}");
        let done_id = seen.iter().find_map(|e| match e {
            WorkerEvent::TurnDone { agent_id, .. } => Some(agent_id.clone()),
            _ => None,
        });
        assert_eq!(done_id.as_deref(), Some("agent-002"));
        // A task panic is not a stop: no stop event, no
        // latched stop in the scope record, and the worker
        // keeps serving afterwards.
        assert!(
            !seen
                .iter()
                .any(|e| matches!(e, WorkerEvent::StopTripped { .. })),
            "a task panic must not trip the stop latch"
        );
        let records = audit_records(&dir.join("audit.jsonl"));
        let scope_record = find_scope_record(&records);
        assert_eq!(scope_record["detail"]["scope"], "single");
        assert_eq!(scope_record["detail"]["stop_latched"], false);
        assert_eq!(scope_record["detail"]["actor_broken"], false);
        assert_eq!(scope_record["detail"]["end_verified"], true);
        assert_eq!(scope_record["detail"]["end_verification"], "joined");
        handle.run_turn("still alive".to_string());
        let seen2 = collect_until(&handle, 500, |seen| {
            seen.iter()
                .any(|e| matches!(e, WorkerEvent::TurnDone { .. }))
        });
        assert!(seen2.iter().any(|e| matches!(
            e,
            WorkerEvent::TurnDone {
                agent_id, ..
            } if agent_id == "agent-001"
        )));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn g6_3_stop_halts_all() {
        let (handle, _approvals, dir, flag) = spawn_fake_with_body(
            AllSleep,
            AgentsConfig {
                parallelism_enabled: true,
                ..Default::default()
            },
            HierarchyConfig::default(),
            Box::new(MockBody::new()),
        );
        wait_startup(&handle);
        handle.run_turn("sleep one".to_string());
        handle.spawn_agent("sleep two".to_string());
        // Wait until both agents are running, then trip the
        // flag as agent-001 (the acting agent is recorded).
        let spawned = collect_until(&handle, 500, |seen| {
            seen.iter().any(|e| {
                matches!(e, WorkerEvent::AgentSpawned { agent_id, .. } if agent_id == "agent-002")
            })
        });
        assert!(spawned.iter().any(|e| matches!(
            e,
            WorkerEvent::AgentSpawned { agent_id, .. } if agent_id == "agent-002"
        )));
        flag.trip_with_agent("test", "emergency stop from the test", Some("agent-001"));
        let seen = collect_until(&handle, 500, |seen| {
            seen.iter()
                .filter(|e| matches!(e, WorkerEvent::TurnCancelled { .. }))
                .count()
                >= 2
                && seen
                    .iter()
                    .any(|e| matches!(e, WorkerEvent::StopTripped { .. }))
        });
        let cancelled: Vec<String> = seen
            .iter()
            .filter_map(|e| match e {
                WorkerEvent::TurnCancelled { agent_id } => Some(agent_id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(cancelled.len(), 2, "both agents must stop");
        assert!(cancelled.contains(&"agent-001".to_string()));
        assert!(cancelled.contains(&"agent-002".to_string()));
        assert!(
            !seen
                .iter()
                .any(|e| matches!(e, WorkerEvent::TurnDone { .. })),
            "no agent may complete after the trip"
        );
        // The trip is attributed to the acting agent, in the
        // flag itself and in the audit record; both turns
        // ended cancelled in the audit trail.
        let info = flag.trip_info().expect("trip info recorded");
        assert_eq!(info.agent_id.as_deref(), Some("agent-001"));
        let records = audit_records(&dir.join("audit.jsonl"));
        let stop = records
            .iter()
            .find(|r| r["kind"] == "emergency_stop")
            .expect("emergency_stop record");
        assert_eq!(stop["detail"]["agent_id"], "agent-001");
        let cancelled_turns: Vec<&serde_json::Value> = records
            .iter()
            .filter(|r| r["kind"] == "turn_end" && r["decision"] == "cancelled")
            .collect();
        assert_eq!(cancelled_turns.len(), 2, "both turns audited cancelled");
        assert!(
            !records.iter().any(|r| r["kind"] == "execution"),
            "no tool may run after the trip"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rebuild_after_user_stop_clears_the_stop_latch() {
        // Demo-found bug (fix pass, F6-16 area): DỪNG KHẨN
        // CẤP cancels the shared stop latch; a
        // user-requested rebuild is the active re-enable,
        // so the latch must not outlive it — otherwise
        // every new turn is born cancelled while the UI
        // promises recovery.
        let (handle, approvals, _dir, _flag) = spawn_fake_with_body(
            FakeProvider::answering("fresh turn"),
            AgentsConfig::default(),
            HierarchyConfig::default(),
            Box::new(MockBody::new()),
        );
        auto_allow(approvals);
        wait_startup(&handle);
        handle.stop_now();
        assert!(handle.is_stopped());
        handle.rebuild_body();
        let seen = collect_until(&handle, 500, |seen| {
            seen.iter().any(|e| matches!(e, WorkerEvent::BodyRebuilt))
        });
        assert!(seen.iter().any(|e| matches!(e, WorkerEvent::BodyRebuilt)));
        assert!(!handle.is_stopped(), "rebuild must clear the stop latch");
        handle.run_turn("hi".to_string());
        match wait_terminal(&handle) {
            WorkerEvent::TurnDone { text, .. } => assert_eq!(text, "fresh turn"),
            other => panic!("turn must complete after rebuild, got: {other:?}"),
        }
    }

    #[test]
    fn g6_9_rebuild_does_not_rewrite_unknown() {
        // Call sequence across both turns: turn 1 clicks
        // (panic, fail-all, automatic rebuild); turn 2
        // screenshots first, then clicks on the NEW actor,
        // then answers.
        struct RebuildProvider {
            calls: std::sync::Mutex<usize>,
        }
        impl Provider for RebuildProvider {
            fn name(&self) -> &str {
                "rebuild-seq"
            }

            fn model(&self) -> &str {
                "rebuild-model"
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
                let mut calls = self.calls.lock().unwrap();
                *calls += 1;
                let n = *calls;
                Box::pin(async move {
                    match n {
                        1 => Ok(tool_msg("body_click", r#"{"x": 10, "y": 10}"#)),
                        2 => Ok(tool_msg("body_screenshot", "{}")),
                        3 => Ok(tool_msg("body_click", r#"{"x": 10, "y": 10}"#)),
                        _ => Ok(text_msg("rebuilt turn done")),
                    }
                })
            }
        }
        let clicks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (handle, approvals, dir, _flag) = spawn_fake_with_body(
            RebuildProvider {
                calls: std::sync::Mutex::new(0),
            },
            AgentsConfig {
                parallelism_enabled: true,
                ..Default::default()
            },
            HierarchyConfig::default(),
            Box::new(PanicBody {
                clicks: Arc::clone(&clicks),
            }),
        );
        auto_allow(approvals);
        wait_startup(&handle);
        handle.run_turn("click".to_string());
        let seen = collect_until(&handle, 500, |seen| {
            seen.iter()
                .any(|e| matches!(e, WorkerEvent::AgentFault { .. }))
                && seen.iter().any(|e| matches!(e, WorkerEvent::BodyRebuilt))
        });
        assert!(seen.iter().any(|e| matches!(
            e,
            WorkerEvent::AgentFault {
                scope: FailureScope::All,
                ..
            }
        )));
        assert_eq!(clicks.load(std::sync::atomic::Ordering::SeqCst), 1);
        // The failed action is still UnknownOutcome in the
        // audit trail: history is not rewritten by the
        // rebuild, and no record claims it succeeded.
        let records = audit_records(&dir.join("audit.jsonl"));
        let failed_execution = records
            .iter()
            .find(|r| {
                r["kind"] == "execution" && r["tool"] == "body_click" && r["decision"] == "error"
            })
            .expect("failed execution record");
        assert!(
            failed_execution["detail"]["error"]
                .as_str()
                .unwrap()
                .contains("unknown_outcome"),
            "the failed action stays unknown_outcome"
        );
        let request_id = failed_execution["detail"]["request_id"].clone();
        assert!(
            !records.iter().any(|r| r["kind"] == "execution"
                && r["decision"] == "executed"
                && r["detail"]["request_id"] == request_id),
            "no record may rewrite the failed action as executed"
        );
        // A new turn after the rebuild screenshots first,
        // then clicks on the NEW actor: the old body sees no
        // further click, and the new turn completes.
        handle.run_turn("after rebuild".to_string());
        let seen2 = collect_until(&handle, 500, |seen| {
            seen.iter()
                .any(|e| matches!(e, WorkerEvent::TurnDone { .. }))
        });
        assert!(seen2.iter().any(|e| matches!(
            e,
            WorkerEvent::TurnDone { agent_id, .. } if agent_id == "agent-001"
        )));
        assert_eq!(
            clicks.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the old actor must receive no new action after the rebuild"
        );
        let records = audit_records(&dir.join("audit.jsonl"));
        assert!(
            records.iter().any(|r| r["kind"] == "body_rebuilt"),
            "the rebuild itself is audited"
        );
        // The post-rebuild turn took a fresh screenshot
        // before acting: its screenshot execution precedes
        // its click execution in the trail.
        let positions: Vec<(usize, &str)> = records
            .iter()
            .enumerate()
            .filter_map(|(i, r)| {
                if r["kind"] == "execution" && r["decision"] == "executed" {
                    Some((i, r["tool"].as_str().unwrap_or("")))
                } else {
                    None
                }
            })
            .collect();
        let screenshot_pos = positions
            .iter()
            .find(|(_, tool)| *tool == "body_screenshot")
            .map(|(i, _)| *i)
            .expect("screenshot execution after rebuild");
        let click_pos = positions
            .iter()
            .find(|(_, tool)| *tool == "body_click")
            .map(|(i, _)| *i)
            .expect("click execution after rebuild");
        assert!(
            screenshot_pos < click_pos,
            "the post-rebuild turn must screenshot before acting"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn level3_thread_end_verified() {
        let (handle, _approvals, dir) = spawn_fake_with(
            SleeperProvider,
            AgentsConfig {
                parallelism_enabled: true,
                ..Default::default()
            },
            HierarchyConfig::default(),
        );
        wait_startup(&handle);
        handle.run_turn("one".to_string());
        handle.run_turn("two".to_string());
        for _ in 0..500 {
            match wait_event(&handle) {
                WorkerEvent::AgentSpawned { agent_id, .. } if agent_id == "agent-002" => break,
                WorkerEvent::TryingBackend { .. } | WorkerEvent::TurnProgress { .. } => {}
                other => panic!("unexpected event before cancel: {other:?}"),
            }
        }
        let started = Instant::now();
        handle.cancel_agent("agent-002".to_string());
        let seen = collect_until(&handle, 500, |seen| {
            seen.iter().any(|e| {
                matches!(
                    e,
                    WorkerEvent::TurnCancelled { agent_id } if agent_id == "agent-002"
                )
            }) && seen.iter().any(|e| {
                matches!(
                    e,
                    WorkerEvent::AgentEnded { agent_id } if agent_id == "agent-002"
                )
            }) && seen.iter().any(|e| {
                matches!(
                    e,
                    WorkerEvent::TurnDone { agent_id, .. } if agent_id == "agent-001"
                )
            })
        });
        // The end is verified promptly (well inside the 5s
        // join bound): the cancelled agent's task joined, the
        // agent was reaped, and no leaked task survives — the
        // channel stays quiet afterwards (no late completion
        // from a zombie task).
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "cancel must join within the bound, took {:?}",
            started.elapsed()
        );
        assert!(seen.iter().any(|e| matches!(
            e,
            WorkerEvent::TurnCancelled { agent_id } if agent_id == "agent-002"
        )));
        assert!(seen.iter().any(|e| matches!(
            e,
            WorkerEvent::AgentEnded { agent_id } if agent_id == "agent-002"
        )));
        assert!(seen.iter().any(|e| matches!(
            e,
            WorkerEvent::TurnDone { agent_id, .. } if agent_id == "agent-001"
        )));
        let quiet: Vec<WorkerEvent> = {
            let mut out = Vec::new();
            let deadline = Instant::now() + Duration::from_millis(500);
            while Instant::now() < deadline {
                if let Some(event) = handle.try_recv_event() {
                    out.push(event);
                } else {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            out
        };
        assert!(
            quiet.is_empty(),
            "no event may arrive from a leaked task: {quiet:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- Phase 6 gate tests: budget scope, audit attribution,
    // flag-off, hierarchy (§8.5, §11) ----

    /// Shared recording state for [`ScriptedByAgent`]: the tool
    /// schemas and message histories each agent was sent, in
    /// call order.
    type ToolLog = std::sync::Arc<std::sync::Mutex<Vec<(String, Vec<String>)>>>;
    type MessageLog = std::sync::Arc<std::sync::Mutex<Vec<(String, Vec<ChatMessage>)>>>;
    #[derive(Default, Clone)]
    struct Recorder {
        tools: ToolLog,
        messages: MessageLog,
    }

    impl Recorder {
        fn tools_for(&self, agent_id: &str) -> Vec<Vec<String>> {
            self.tools
                .lock()
                .unwrap()
                .iter()
                .filter(|(agent, _)| agent == agent_id)
                .map(|(_, tools)| tools.clone())
                .collect()
        }

        fn messages_for(&self, agent_id: &str) -> Vec<Vec<ChatMessage>> {
            self.messages
                .lock()
                .unwrap()
                .iter()
                .filter(|(agent, _)| agent == agent_id)
                .map(|(_, messages)| messages.clone())
                .collect()
        }
    }

    /// A provider with a per-agent script of replies. Each call
    /// records the tool schema and the messages it was sent, so
    /// tests can assert what each agent saw. When an agent's
    /// script runs out, the last reply repeats. Agents in
    /// `sleepers` sleep 30s instead of answering (only a cancel
    /// ends them within a test).
    struct ScriptedByAgent {
        scripts: std::sync::Mutex<
            std::collections::HashMap<String, std::collections::VecDeque<ChatMessage>>,
        >,
        repeat: std::sync::Mutex<std::collections::HashMap<String, ChatMessage>>,
        recorder: Recorder,
        sleepers: std::collections::HashSet<String>,
    }

    impl ScriptedByAgent {
        fn new(scripts: Vec<(&str, Vec<ChatMessage>)>, recorder: Recorder) -> Self {
            Self {
                scripts: std::sync::Mutex::new(
                    scripts
                        .into_iter()
                        .map(|(agent, script)| (agent.to_string(), script.into_iter().collect()))
                        .collect(),
                ),
                repeat: std::sync::Mutex::new(std::collections::HashMap::new()),
                recorder,
                sleepers: std::collections::HashSet::new(),
            }
        }

        fn sleeping(mut self, agent: &str) -> Self {
            self.sleepers.insert(agent.to_string());
            self
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
            messages: &'a [ChatMessage],
            tools: &'a [serde_json::Value],
        ) -> BoxFuture<'a, Result<ChatMessage, MimintError>> {
            let agent = ctx.agent_id.to_string();
            let tool_names: Vec<String> = tools
                .iter()
                .filter_map(|t| t.get("function")?.get("name")?.as_str().map(str::to_string))
                .collect();
            self.recorder
                .tools
                .lock()
                .unwrap()
                .push((agent.clone(), tool_names));
            self.recorder
                .messages
                .lock()
                .unwrap()
                .push((agent.clone(), messages.to_vec()));
            let sleep = self.sleepers.contains(&agent);
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
            Box::pin(async move {
                if sleep {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    Ok(text_msg("slept"))
                } else {
                    Ok(reply)
                }
            })
        }
    }

    fn usage_msg(text: &str, prompt: u64, completion: u64) -> ChatMessage {
        ChatMessage {
            role: "assistant".to_string(),
            content: Some(text.to_string()),
            tool_calls: None,
            tool_call_id: None,
            usage: Some(TokenUsage {
                prompt_tokens: prompt,
                completion_tokens: completion,
            }),
        }
    }

    /// agent-001's first call fails with a budget error of the
    /// given scope; every other agent answers "B done".
    struct BudgetForOne {
        scope: BudgetScope,
        calls: std::sync::Mutex<std::collections::HashMap<String, usize>>,
        sleep_others: bool,
    }

    impl Provider for BudgetForOne {
        fn name(&self) -> &str {
            "budget-one"
        }

        fn model(&self) -> &str {
            "budget-model"
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
            let mut calls = self.calls.lock().unwrap();
            let n = calls.entry(ctx.agent_id.to_string()).or_insert(0);
            *n += 1;
            let (n, agent, scope, sleep_others) =
                (*n, ctx.agent_id.to_string(), self.scope, self.sleep_others);
            Box::pin(async move {
                if agent == "agent-001" && n == 1 {
                    Err(MimintError::BudgetExhausted(scope, "cap spent".to_string()))
                } else if sleep_others && agent != "agent-001" {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    Ok(text_msg("slept"))
                } else {
                    Ok(text_msg("B done"))
                }
            })
        }
    }

    #[test]
    fn g6_6_global_cap_exhaustion_fails_all() {
        let (handle, _approvals, dir) = spawn_fake_with(
            BudgetForOne {
                scope: BudgetScope::Global,
                calls: std::sync::Mutex::new(std::collections::HashMap::new()),
                sleep_others: true,
            },
            AgentsConfig {
                parallelism_enabled: true,
                ..Default::default()
            },
            HierarchyConfig::default(),
        );
        wait_startup(&handle);
        handle.run_turn("spend".to_string());
        handle.spawn_agent("sleep".to_string());
        let seen = collect_until(&handle, 500, |seen| {
            seen.iter()
                .any(|e| matches!(e, WorkerEvent::AgentFault { .. }))
                && seen
                    .iter()
                    .any(|e| matches!(e, WorkerEvent::TurnFailed { .. }))
                && seen
                    .iter()
                    .any(|e| matches!(e, WorkerEvent::TurnCancelled { .. }))
        });
        let failed = seen.iter().find_map(|e| match e {
            WorkerEvent::TurnFailed {
                agent_id, error, ..
            } => Some((agent_id.clone(), error.clone())),
            _ => None,
        });
        let (failed_id, failed_error) = failed.expect("TurnFailed");
        assert_eq!(failed_id, "agent-001");
        assert!(
            failed_error.contains("budget_exhausted"),
            "global exhaustion must surface: {failed_error}"
        );
        let cancelled: Vec<&str> = seen
            .iter()
            .filter_map(|e| match e {
                WorkerEvent::TurnCancelled { agent_id } => Some(agent_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(cancelled, vec!["agent-002"]);
        let fault = seen.iter().find_map(|e| match e {
            WorkerEvent::AgentFault {
                agent_id,
                scope,
                cause,
            } => Some((agent_id.clone(), *scope, cause.clone())),
            _ => None,
        });
        let (fault_id, scope, cause) = fault.expect("AgentFault");
        assert_eq!(fault_id, "agent-001");
        assert_eq!(scope, FailureScope::All);
        assert!(cause.contains("budget_exhausted"), "{cause}");
        let records = audit_records(&dir.join("audit.jsonl"));
        let scope_record = find_scope_record(&records);
        assert_eq!(scope_record["detail"]["scope"], "all");
        assert_eq!(scope_record["detail"]["failed_agent"], "agent-001");
        assert_eq!(scope_record["detail"]["actor_broken"], false);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn agent_budget_exhaustion_isolates_single() {
        let (handle, _approvals, dir) = spawn_fake_with(
            BudgetForOne {
                scope: BudgetScope::Agent,
                calls: std::sync::Mutex::new(std::collections::HashMap::new()),
                sleep_others: false,
            },
            AgentsConfig {
                parallelism_enabled: true,
                ..Default::default()
            },
            HierarchyConfig::default(),
        );
        wait_startup(&handle);
        handle.run_turn("spend".to_string());
        handle.spawn_agent("succeed".to_string());
        let seen = collect_until(&handle, 500, |seen| {
            seen.iter()
                .any(|e| matches!(e, WorkerEvent::TurnFailed { .. }))
                && seen
                    .iter()
                    .any(|e| matches!(e, WorkerEvent::TurnDone { .. }))
        });
        let failed_id = seen.iter().find_map(|e| match e {
            WorkerEvent::TurnFailed { agent_id, .. } => Some(agent_id.clone()),
            _ => None,
        });
        assert_eq!(failed_id.as_deref(), Some("agent-001"));
        let done = seen.iter().find_map(|e| match e {
            WorkerEvent::TurnDone { agent_id, text, .. } => Some((agent_id.clone(), text.clone())),
            _ => None,
        });
        assert_eq!(done, Some(("agent-002".to_string(), "B done".to_string())));
        assert!(
            !seen
                .iter()
                .any(|e| matches!(e, WorkerEvent::AgentFault { .. })),
            "an agent-scope budget stop is not a fault event"
        );
        let records = audit_records(&dir.join("audit.jsonl"));
        assert!(
            !records.iter().any(|r| r["kind"] == "agent_failure_scope"),
            "no scope record for a single-agent budget stop"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn g6_4_audit_attribution_under_load() {
        use crate::audit_view::{distinct_agents, read_audit_tail};
        let (handle, approvals, dir, _flag) = spawn_fake_with_body(
            AllClickOnce {
                calls: std::sync::Mutex::new(std::collections::HashMap::new()),
            },
            AgentsConfig {
                parallelism_enabled: true,
                ..Default::default()
            },
            HierarchyConfig::default(),
            Box::new(MockBody::new()),
        );
        auto_allow(approvals);
        wait_startup(&handle);
        handle.run_turn("click one".to_string());
        handle.spawn_agent("click two".to_string());
        handle.spawn_agent("click three".to_string());
        let seen = collect_until(&handle, 500, |seen| {
            seen.iter()
                .filter(|e| matches!(e, WorkerEvent::TurnDone { .. }))
                .count()
                == 3
        });
        let done_agents: Vec<String> = seen
            .iter()
            .filter_map(|e| match e {
                WorkerEvent::TurnDone { agent_id, .. } => Some(agent_id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(done_agents.len(), 3);
        // The whole file must parse as JSONL (audit_records
        // fails on any malformed line), and every record must
        // carry a non-empty agent id.
        let path = dir.join("audit.jsonl");
        let records = audit_records(&path);
        assert!(!records.is_empty());
        for record in &records {
            let agent = record
                .get("agent_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            assert!(!agent.is_empty(), "every record needs an agent: {record}");
            assert!(
                ["agent-000", "agent-001", "agent-002", "agent-003"].contains(&agent),
                "unexpected agent id: {agent}"
            );
        }
        // Attribution is stable per turn: all records sharing a
        // turn id carry the same agent, and each agent's
        // subsequence is in timestamp order.
        let mut turn_agents: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        let mut last_ts: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        for record in &records {
            let agent = record["agent_id"].as_str().unwrap().to_string();
            let turn = record["turn_id"].as_str().unwrap_or("").to_string();
            if !turn.is_empty() {
                let entry = turn_agents.entry(turn).or_insert_with(|| agent.clone());
                assert_eq!(*entry, agent, "turn attribution must not change");
            }
            if let Some(ts) = record["ts_ms"].as_u64() {
                let entry = last_ts.entry(agent).or_insert(0);
                assert!(ts >= *entry, "per-agent order must hold");
                *entry = ts;
            }
        }
        // Each agent attempted its click under its own id. F6-03:
        // a click whose snapshot predates another agent's
        // dispatched action is refused with
        // target_changed_since_approval instead of executing, so
        // under load an agent may legitimately have no executed
        // click — but the attempt must still be attributed to
        // that agent, executed or refused. At least one click
        // executes: the first to reach re-verification always
        // does, since no dispatch precedes it.
        let mut executed_agents = 0;
        for agent in ["agent-001", "agent-002", "agent-003"] {
            let attempts: Vec<_> = records
                .iter()
                .filter(|r| {
                    r["kind"] == "execution" && r["tool"] == "body_click" && r["agent_id"] == agent
                })
                .collect();
            assert!(
                !attempts.is_empty(),
                "{agent} must have a click attempt under its own id"
            );
            for record in &attempts {
                assert!(
                    record["decision"] == "executed"
                        || record["detail"]["error"]
                            .as_str()
                            .unwrap_or("")
                            .contains("target_changed_since_approval"),
                    "{agent} click must be executed or F6-03-refused: {record}"
                );
            }
            if attempts.iter().any(|r| r["decision"] == "executed") {
                executed_agents += 1;
            }
        }
        assert!(
            executed_agents >= 1,
            "at least one agent's click must execute"
        );
        // The viewer half: the tail parses, the agent list is
        // exact, and filtering by agent shows only that agent.
        let rows = read_audit_tail(&path, 10_000);
        assert_eq!(rows.len(), records.len());
        let agents = distinct_agents(&rows);
        for agent in ["agent-001", "agent-002", "agent-003"] {
            assert!(
                agents.iter().any(|a| a == agent),
                "viewer must list {agent}: {agents:?}"
            );
        }
        assert!(
            agents
                .iter()
                .all(|a| ["agent-000", "agent-001", "agent-002", "agent-003"].contains(&a.as_str())),
            "no unexpected agent in the viewer list: {agents:?}"
        );
        for agent in ["agent-001", "agent-002", "agent-003"] {
            let filtered: Vec<_> = rows.iter().filter(|r| r.agent_id == agent).collect();
            assert!(!filtered.is_empty());
            assert!(filtered.iter().all(|r| r.agent_id == agent));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn flag_off_single_task() {
        // Answers from the prompt text: "one" sleeps 300ms so
        // the second turn is issued while the first still runs.
        struct EchoByPrompt;
        impl Provider for EchoByPrompt {
            fn name(&self) -> &str {
                "echo-prompt"
            }

            fn model(&self) -> &str {
                "echo-model"
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
                messages: &'a [ChatMessage],
                _tools: &'a [serde_json::Value],
            ) -> BoxFuture<'a, Result<ChatMessage, MimintError>> {
                let prompt = messages
                    .iter()
                    .rev()
                    .find(|m| m.role == "user")
                    .and_then(|m| m.content.clone())
                    .unwrap_or_default();
                Box::pin(async move {
                    if prompt == "one" {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        Ok(text_msg("done-one"))
                    } else {
                        Ok(text_msg("done-two"))
                    }
                })
            }
        }
        let (handle, _approvals, dir) = spawn_fake_with(
            EchoByPrompt,
            AgentsConfig {
                parallelism_enabled: false,
                ..Default::default()
            },
            HierarchyConfig::default(),
        );
        wait_startup(&handle);
        handle.run_turn("one".to_string());
        handle.run_turn("two".to_string());
        let seen = collect_until(&handle, 500, |seen| {
            seen.iter()
                .filter(|e| matches!(e, WorkerEvent::TurnDone { .. }))
                .count()
                == 2
        });
        let done: Vec<(String, String)> = seen
            .iter()
            .filter_map(|e| match e {
                WorkerEvent::TurnDone { agent_id, text, .. } => {
                    Some((agent_id.clone(), text.clone()))
                }
                _ => None,
            })
            .collect();
        // Strictly sequential, both on the default agent, in
        // issue order — and no second agent ever spawned.
        assert_eq!(
            done,
            vec![
                ("agent-001".to_string(), "done-one".to_string()),
                ("agent-001".to_string(), "done-two".to_string()),
            ]
        );
        assert!(
            !seen.iter().any(|e| matches!(
                e,
                WorkerEvent::AgentSpawned { agent_id, .. } if agent_id != "agent-001"
            )),
            "flag off must never spawn another agent: {seen:?}"
        );
        assert!(
            !seen
                .iter()
                .any(|e| matches!(e, WorkerEvent::TurnFailed { .. })),
            "no turn may fail: {seen:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hierarchy_default_off() {
        let recorder = Recorder::default();
        let provider = ScriptedByAgent::new(
            vec![(
                "agent-001",
                vec![
                    tool_msg("delegate", r#"{"task":"child task"}"#),
                    text_msg("parent done"),
                ],
            )],
            recorder.clone(),
        );
        let (handle, _approvals, dir) = spawn_fake_with(
            provider,
            AgentsConfig {
                parallelism_enabled: true,
                ..Default::default()
            },
            HierarchyConfig::default(),
        );
        wait_startup(&handle);
        handle.run_turn("do it via a child".to_string());
        let seen = collect_until(&handle, 500, |seen| {
            seen.iter()
                .any(|e| matches!(e, WorkerEvent::TurnDone { .. }))
        });
        assert!(
            seen.iter().any(|e| matches!(
                e,
                WorkerEvent::TurnDone { text, .. } if text == "parent done"
            )),
            "parent completes after the denied delegate call: {seen:?}"
        );
        // The schema never offered `delegate`...
        for tools in recorder.tools_for("agent-001") {
            assert!(
                !tools.iter().any(|t| t == "delegate"),
                "delegate must not be offered when hierarchy is off: {tools:?}"
            );
        }
        // ...and calling it anyway is denied as an unknown tool.
        let records = audit_records(&dir.join("audit.jsonl"));
        assert!(
            records.iter().any(|r| r["kind"] == "decision"
                && r["tool"] == "delegate"
                && r["decision"] == "deny"
                && r["detail"]["reason"] == "unknown_tool"
                && r["agent_id"] == "agent-001"),
            "delegate call must be denied unknown_tool"
        );
        assert!(
            !seen.iter().any(|e| matches!(
                e,
                WorkerEvent::AgentSpawned {
                    parent: Some(_),
                    ..
                }
            )),
            "no child may spawn when hierarchy is off"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hierarchy_depth_one() {
        let recorder = Recorder::default();
        let provider = ScriptedByAgent::new(
            vec![
                (
                    "agent-001",
                    vec![
                        tool_msg("delegate", r#"{"task":"child task"}"#),
                        text_msg("parent done"),
                    ],
                ),
                (
                    "agent-002",
                    vec![
                        tool_msg("delegate", r#"{"task":"grandchild task"}"#),
                        text_msg("child done"),
                    ],
                ),
            ],
            recorder.clone(),
        );
        let (handle, approvals, dir) = spawn_fake_with(
            provider,
            AgentsConfig {
                parallelism_enabled: true,
                ..Default::default()
            },
            HierarchyConfig {
                enabled: true,
                max_children: 2,
                default_child_steps: 10,
                default_child_time_secs: 300,
            },
        );
        auto_allow(approvals);
        wait_startup(&handle);
        handle.run_turn("do it via a child".to_string());
        let seen = collect_until(&handle, 500, |seen| {
            seen.iter()
                .any(|e| matches!(e, WorkerEvent::TurnDone { .. }))
        });
        assert!(
            seen.iter().any(|e| matches!(
                e,
                WorkerEvent::TurnDone { text, .. } if text == "parent done"
            )),
            "parent completes: {seen:?}"
        );
        // The parent is offered `delegate`; the child is not.
        let parent_tools = recorder.tools_for("agent-001");
        assert!(!parent_tools.is_empty());
        assert!(
            parent_tools[0].iter().any(|t| t == "delegate"),
            "parent must be offered delegate: {:?}",
            parent_tools[0]
        );
        let child_tools = recorder.tools_for("agent-002");
        assert!(!child_tools.is_empty(), "the child must run");
        for tools in &child_tools {
            assert!(
                !tools.iter().any(|t| t == "delegate"),
                "child must not be offered delegate: {tools:?}"
            );
        }
        // The child's delegate call is denied as unknown_tool,
        // and no grandchild ever spawns.
        let records = audit_records(&dir.join("audit.jsonl"));
        assert!(
            records.iter().any(|r| r["kind"] == "decision"
                && r["tool"] == "delegate"
                && r["decision"] == "deny"
                && r["detail"]["reason"] == "unknown_tool"
                && r["agent_id"] == "agent-002"),
            "child delegate call must be denied unknown_tool"
        );
        assert!(
            !records.iter().any(|r| r["agent_id"] == "agent-003"),
            "no grandchild may exist"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn subagent_limits_enforced() {
        // (a) Step limit: the child is given max_steps=2 and a
        // 3-action script — it ends partial, and the parent
        // receives the partial flag.
        let recorder = Recorder::default();
        let provider = ScriptedByAgent::new(
            vec![
                (
                    "agent-001",
                    vec![
                        tool_msg("delegate", r#"{"task":"take notes","max_steps":2}"#),
                        text_msg("parent done"),
                    ],
                ),
                (
                    "agent-002",
                    vec![
                        tool_msg("notebook_add", r#"{"title":"n1","body":"b1"}"#),
                        tool_msg("notebook_add", r#"{"title":"n2","body":"b2"}"#),
                        tool_msg("notebook_add", r#"{"title":"n3","body":"b3"}"#),
                        text_msg("child done"),
                    ],
                ),
            ],
            recorder.clone(),
        );
        let (handle, approvals, dir) = spawn_fake_with(
            provider,
            AgentsConfig {
                parallelism_enabled: true,
                ..Default::default()
            },
            HierarchyConfig {
                enabled: true,
                max_children: 2,
                default_child_steps: 10,
                default_child_time_secs: 300,
            },
        );
        auto_allow(approvals);
        wait_startup(&handle);
        handle.run_turn("delegate notes".to_string());
        let seen = collect_until(&handle, 500, |seen| {
            seen.iter()
                .any(|e| matches!(e, WorkerEvent::TurnDone { .. }))
        });
        assert!(
            seen.iter().any(|e| matches!(
                e,
                WorkerEvent::TurnDone { text, .. } if text == "parent done"
            )),
            "parent completes: {seen:?}"
        );
        let records = audit_records(&dir.join("audit.jsonl"));
        assert!(
            records.iter().any(|r| r["kind"] == "turn_end"
                && r["decision"] == "partial"
                && r["detail"]["reason"] == "step_budget_exhausted"
                && r["agent_id"] == "agent-002"),
            "child must end partial on its step budget"
        );
        let parent_calls = recorder.messages_for("agent-001");
        let tool_results: Vec<&str> = parent_calls
            .iter()
            .flat_map(|messages| messages.iter())
            .filter(|m| m.role == "tool")
            .filter_map(|m| m.content.as_deref())
            .collect();
        assert!(
            tool_results.iter().any(|c| c.contains("\"partial\":true")),
            "parent must receive partial=true: {tool_results:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);

        // (b) Token limit: a 100-token child budget cannot cover
        // the 1000-token reserve floor — the child fails on
        // budget, the parent gets the failure framed and the
        // delegate_failures list names the child.
        let recorder = Recorder::default();
        let provider = ScriptedByAgent::new(
            vec![(
                "agent-001",
                vec![
                    tool_msg("delegate", r#"{"task":"cheap task","token_budget":100}"#),
                    text_msg("parent done"),
                ],
            )],
            recorder.clone(),
        );
        let (handle, approvals, dir) = spawn_fake_with(
            provider,
            AgentsConfig {
                parallelism_enabled: true,
                ..Default::default()
            },
            HierarchyConfig {
                enabled: true,
                max_children: 2,
                default_child_steps: 10,
                default_child_time_secs: 300,
            },
        );
        auto_allow(approvals);
        wait_startup(&handle);
        handle.run_turn("delegate cheap".to_string());
        let seen = collect_until(&handle, 500, |seen| {
            seen.iter()
                .any(|e| matches!(e, WorkerEvent::TurnDone { .. }))
        });
        let failures = seen.iter().find_map(|e| match e {
            WorkerEvent::TurnDone {
                delegate_failures, ..
            } => Some(delegate_failures.clone()),
            _ => None,
        });
        let failures = failures.expect("TurnDone");
        assert_eq!(failures.len(), 1, "one child failure: {failures:?}");
        assert!(
            failures[0].starts_with("agent-002: "),
            "failure names the child: {:?}",
            failures[0]
        );
        assert!(
            failures[0].contains("budget"),
            "failure names the budget: {:?}",
            failures[0]
        );
        let parent_calls = recorder.messages_for("agent-001");
        let tool_results: Vec<&str> = parent_calls
            .iter()
            .flat_map(|messages| messages.iter())
            .filter(|m| m.role == "tool")
            .filter_map(|m| m.content.as_deref())
            .collect();
        assert!(
            tool_results
                .iter()
                .any(|c| c.contains("[child failed:") && c.contains("\"partial\":true")),
            "parent must receive the framed failure: {tool_results:?}"
        );
        let records = audit_records(&dir.join("audit.jsonl"));
        assert!(
            records.iter().any(|r| r["kind"] == "provider_attempt"
                && r["decision"] == "budget_exhausted"
                && r["agent_id"] == "agent-002"),
            "child budget exhaustion must be audited"
        );
        let _ = std::fs::remove_dir_all(&dir);

        // (c) Time limit: the default 2s child limit (no
        // time_limit_secs arg) times out a sleeping child — the
        // delegate call errors with subagent_timeout.
        let recorder = Recorder::default();
        let provider = ScriptedByAgent::new(
            vec![(
                "agent-001",
                vec![
                    tool_msg("delegate", r#"{"task":"slow task"}"#),
                    text_msg("parent done"),
                ],
            )],
            recorder.clone(),
        )
        .sleeping("agent-002");
        let (handle, approvals, dir) = spawn_fake_with(
            provider,
            AgentsConfig {
                parallelism_enabled: true,
                ..Default::default()
            },
            HierarchyConfig {
                enabled: true,
                max_children: 2,
                default_child_steps: 10,
                default_child_time_secs: 2,
            },
        );
        auto_allow(approvals);
        wait_startup(&handle);
        let started = std::time::Instant::now();
        handle.run_turn("delegate slow".to_string());
        let seen = collect_until(&handle, 500, |seen| {
            seen.iter()
                .any(|e| matches!(e, WorkerEvent::TurnDone { .. }))
        });
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the timeout must fire, not the 30s sleep: {:?}",
            started.elapsed()
        );
        assert!(
            seen.iter().any(|e| matches!(
                e,
                WorkerEvent::TurnDone { text, .. } if text == "parent done"
            )),
            "parent completes after the timeout: {seen:?}"
        );
        let records = audit_records(&dir.join("audit.jsonl"));
        assert!(
            records.iter().any(|r| r["kind"] == "execution"
                && r["tool"] == "delegate"
                && r["agent_id"] == "agent-001"
                && r.to_string().contains("subagent_timeout")),
            "delegate timeout must be audited as subagent_timeout"
        );
        let parent_calls = recorder.messages_for("agent-001");
        let tool_results: Vec<&str> = parent_calls
            .iter()
            .flat_map(|messages| messages.iter())
            .filter(|m| m.role == "tool")
            .filter_map(|m| m.content.as_deref())
            .collect();
        assert!(
            tool_results.iter().any(|c| c.contains("subagent_timeout")),
            "parent must see the timeout as the tool error: {tool_results:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parent_cancel_cancels_children() {
        let provider = ScriptedByAgent::new(
            vec![(
                "agent-001",
                vec![
                    tool_msg("delegate", r#"{"task":"slow task"}"#),
                    text_msg("parent done"),
                ],
            )],
            Recorder::default(),
        )
        .sleeping("agent-002");
        let (handle, approvals, dir) = spawn_fake_with(
            provider,
            AgentsConfig {
                parallelism_enabled: true,
                ..Default::default()
            },
            HierarchyConfig {
                enabled: true,
                max_children: 2,
                default_child_steps: 10,
                default_child_time_secs: 300,
            },
        );
        auto_allow(approvals);
        wait_startup(&handle);
        handle.run_turn("delegate slow".to_string());
        // Wait for the child to spawn, then cancel the parent.
        for _ in 0..500 {
            match wait_event(&handle) {
                WorkerEvent::AgentSpawned {
                    parent: Some(parent),
                    ..
                } if parent == "agent-001" => break,
                WorkerEvent::TryingBackend { .. }
                | WorkerEvent::TurnProgress { .. }
                | WorkerEvent::AgentSpawned { .. }
                | WorkerEvent::AgentEnded { .. } => {}
                other => panic!("unexpected event before cancel: {other:?}"),
            }
        }
        handle.cancel_turn();
        let seen = collect_until(&handle, 500, |seen| {
            seen.iter()
                .any(|e| matches!(e, WorkerEvent::TurnCancelled { .. }))
        });
        assert!(
            seen.iter().any(|e| matches!(
                e,
                WorkerEvent::TurnCancelled { agent_id } if agent_id == "agent-001"
            )),
            "parent must be cancelled: {seen:?}"
        );
        // The child ends too: its turn_end is cancelled, and it
        // is reaped.
        let records = audit_records(&dir.join("audit.jsonl"));
        assert!(
            records.iter().any(|r| r["kind"] == "turn_end"
                && r["decision"] == "cancelled"
                && r["agent_id"] == "agent-002"),
            "child turn_end must be cancelled"
        );
        let mut child_ended = seen.iter().any(|e| {
            matches!(
                e,
                WorkerEvent::AgentEnded { agent_id, .. } if agent_id == "agent-002"
            )
        });
        if !child_ended {
            // AgentEnded may arrive after TurnCancelled; drain
            // briefly for it (10s: same 2-core load starvation
            // as wait_event; the event is still required).
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while std::time::Instant::now() < deadline {
                if let Some(event) = handle.try_recv_event() {
                    if matches!(
                        event,
                        WorkerEvent::AgentEnded { ref agent_id, .. }
                            if agent_id == "agent-002"
                    ) {
                        child_ended = true;
                        break;
                    }
                } else {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
        assert!(child_ended, "child must be reaped after parent cancel");
        // No child task survives: a fresh turn runs cleanly.
        handle.run_turn("after".to_string());
        let seen = collect_until(&handle, 500, |seen| {
            seen.iter()
                .any(|e| matches!(e, WorkerEvent::TurnDone { .. }))
        });
        assert!(
            seen.iter().any(|e| matches!(
                e,
                WorkerEvent::TurnDone { agent_id, .. } if agent_id == "agent-001"
            )),
            "a fresh turn must complete: {seen:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delegate_output_framed() {
        let recorder = Recorder::default();
        let provider = ScriptedByAgent::new(
            vec![
                (
                    "agent-001",
                    vec![
                        tool_msg("delegate", r#"{"task":"report back"}"#),
                        text_msg("parent done"),
                    ],
                ),
                ("agent-002", vec![text_msg("CHILD SAYS HELLO")]),
            ],
            recorder.clone(),
        );
        let (handle, approvals, dir) = spawn_fake_with(
            provider,
            AgentsConfig {
                parallelism_enabled: true,
                ..Default::default()
            },
            HierarchyConfig {
                enabled: true,
                max_children: 2,
                default_child_steps: 10,
                default_child_time_secs: 300,
            },
        );
        auto_allow(approvals);
        wait_startup(&handle);
        handle.run_turn("delegate report".to_string());
        let seen = collect_until(&handle, 500, |seen| {
            seen.iter()
                .any(|e| matches!(e, WorkerEvent::TurnDone { .. }))
        });
        assert!(
            seen.iter().any(|e| matches!(
                e,
                WorkerEvent::TurnDone { text, .. } if text == "parent done"
            )),
            "parent completes: {seen:?}"
        );
        // The parent's second call carries the framed output as
        // a tool result plus the system reminder — and nowhere
        // else.
        let parent_calls = recorder.messages_for("agent-001");
        assert!(parent_calls.len() >= 2, "parent must call twice");
        let second = &parent_calls[1];
        let frame = "[UNTRUSTED_SUBAGENT_OUTPUT agent_id=\"agent-002\"]";
        // The tool result is the delegate tool's JSON return
        // value, stringified: parse it and inspect the framed
        // `output` field (the raw string carries escaped quotes).
        let tool_messages: Vec<String> = second
            .iter()
            .filter(|m| m.role == "tool")
            .filter_map(|m| m.content.as_deref())
            .filter_map(|c| serde_json::from_str::<serde_json::Value>(c).ok())
            .filter_map(|v| {
                v.get("output")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
            .filter(|output| output.contains(frame))
            .collect();
        assert_eq!(tool_messages.len(), 1, "exactly one framed tool result");
        let framed = &tool_messages[0];
        assert!(framed.contains("CHILD SAYS HELLO"));
        assert!(framed.contains("[/UNTRUSTED_SUBAGENT_OUTPUT]"));
        assert!(
            second.iter().any(|m| m.role == "system"
                && m.content.as_deref() == Some(mimint_core::DELEGATE_OUTPUT_REMINDER)),
            "the untrusted-output reminder must follow the delegate result"
        );
        for message in second {
            if message.role != "tool" && message.role != "system" {
                assert!(
                    !message
                        .content
                        .as_deref()
                        .unwrap_or("")
                        .contains("CHILD SAYS HELLO"),
                    "child output must not appear as an instruction (role {}): {:?}",
                    message.role,
                    message.content
                );
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hierarchy_latency_measured() {
        // Both parent and child report measured usage: the
        // audit must carry a provider_attempt with agent id,
        // tokens, and kind for each of them, so hierarchy
        // overhead is measurable per agent.
        let provider = ScriptedByAgent::new(
            vec![
                (
                    "agent-001",
                    vec![
                        ChatMessage {
                            role: "assistant".to_string(),
                            content: None,
                            tool_calls: Some(vec![ToolCall {
                                id: "call-1".to_string(),
                                kind: "function".to_string(),
                                function: FunctionCall {
                                    name: "delegate".to_string(),
                                    arguments: r#"{"task":"child task"}"#.to_string(),
                                },
                            }]),
                            tool_call_id: None,
                            usage: Some(TokenUsage {
                                prompt_tokens: 100,
                                completion_tokens: 50,
                            }),
                        },
                        usage_msg("parent done", 120, 30),
                    ],
                ),
                ("agent-002", vec![usage_msg("child done", 80, 20)]),
            ],
            Recorder::default(),
        );
        let (handle, approvals, dir) = spawn_fake_with(
            provider,
            AgentsConfig {
                parallelism_enabled: true,
                ..Default::default()
            },
            HierarchyConfig {
                enabled: true,
                max_children: 2,
                default_child_steps: 10,
                default_child_time_secs: 300,
            },
        );
        auto_allow(approvals);
        wait_startup(&handle);
        handle.run_turn("delegate measured".to_string());
        let seen = collect_until(&handle, 500, |seen| {
            seen.iter()
                .any(|e| matches!(e, WorkerEvent::TurnDone { .. }))
        });
        assert!(
            seen.iter().any(|e| matches!(
                e,
                WorkerEvent::TurnDone { text, .. } if text == "parent done"
            )),
            "parent completes: {seen:?}"
        );
        let records = audit_records(&dir.join("audit.jsonl"));
        let attempts: Vec<&serde_json::Value> = records
            .iter()
            .filter(|r| r["kind"] == "provider_attempt")
            .collect();
        assert!(!attempts.is_empty());
        for (agent, expected) in [("agent-001", 150u64), ("agent-002", 100u64)] {
            let mine: Vec<&&serde_json::Value> =
                attempts.iter().filter(|r| r["agent_id"] == agent).collect();
            assert!(!mine.is_empty(), "{agent} must have provider attempts");
            assert!(
                mine.iter()
                    .any(|r| r["detail"]["charged_tokens"] == expected
                        && r["detail"]["charge_kind"] == "measured"),
                "{agent} must have a measured attempt of {expected} tokens: {mine:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn click_msg(x: i32, y: i32) -> ChatMessage {
        ChatMessage {
            role: "assistant".to_string(),
            content: None,
            tool_calls: Some(vec![ToolCall {
                id: format!("call-click-{x}-{y}"),
                kind: "function".to_string(),
                function: FunctionCall {
                    name: "body_click".to_string(),
                    arguments: format!(r#"{{"x": {x}, "y": {y}}}"#),
                },
            }]),
            tool_call_id: None,
            usage: Some(TokenUsage {
                prompt_tokens: 80,
                completion_tokens: 40,
            }),
        }
    }

    fn delegate_msg() -> ChatMessage {
        ChatMessage {
            role: "assistant".to_string(),
            content: None,
            tool_calls: Some(vec![ToolCall {
                id: "call-delegate".to_string(),
                kind: "function".to_string(),
                function: FunctionCall {
                    name: "delegate".to_string(),
                    arguments: r#"{"task":"click twice"}"#.to_string(),
                },
            }]),
            tool_call_id: None,
            usage: Some(TokenUsage {
                prompt_tokens: 80,
                completion_tokens: 40,
            }),
        }
    }

    /// One measured run: a fresh worker, a scripted provider
    /// (120 tokens per reply), and a mock body. Flat runs do
    /// two clicks directly (3 provider attempts); delegated
    /// runs delegate to a child that clicks twice (5
    /// attempts). Identical agent config except
    /// `hierarchy.enabled`. Wall time runs from `run_turn`
    /// to `TurnDone`; time-to-first-action is the receipt of
    /// the first `Acting` progress event; tokens, attempts,
    /// and queue waits come from the audit file.
    fn measure_one(run: usize, delegated: bool) -> serde_json::Value {
        let provider = if delegated {
            ScriptedByAgent::new(
                vec![
                    ("agent-001", vec![delegate_msg(), usage_msg("done", 80, 40)]),
                    (
                        "agent-002",
                        vec![
                            click_msg(10, 10),
                            click_msg(20, 20),
                            usage_msg("done", 80, 40),
                        ],
                    ),
                ],
                Recorder::default(),
            )
        } else {
            ScriptedByAgent::new(
                vec![(
                    "agent-001",
                    vec![
                        click_msg(10, 10),
                        click_msg(20, 20),
                        usage_msg("done", 80, 40),
                    ],
                )],
                Recorder::default(),
            )
        };
        let (handle, approvals, dir, _flag) = spawn_fake_with_body(
            provider,
            AgentsConfig {
                parallelism_enabled: true,
                ..Default::default()
            },
            HierarchyConfig {
                enabled: delegated,
                max_children: 2,
                default_child_steps: 10,
                default_child_time_secs: 300,
            },
            Box::new(MockBody::new()),
        );
        auto_allow(approvals);
        wait_startup(&handle);
        let started = std::time::Instant::now();
        handle.run_turn("measure".to_string());
        let mut first_action_ms = None;
        let mut text = None;
        for _ in 0..500 {
            match wait_event(&handle) {
                WorkerEvent::TurnProgress { progress } => {
                    if first_action_ms.is_none() && matches!(progress, BrainProgress::Acting { .. })
                    {
                        first_action_ms = Some(started.elapsed().as_millis() as u64);
                    }
                }
                WorkerEvent::TryingBackend { .. } => {}
                // Delegated runs emit AgentSpawned and the
                // child's TurnDone before the parent
                // finishes: only agent-001's TurnDone ends
                // the measured turn. Anything else is
                // ignored; the 500-event cap plus the
                // expect below catch a stuck run.
                WorkerEvent::TurnDone {
                    agent_id,
                    text: done,
                    ..
                } if agent_id == "agent-001" => {
                    text = Some(done);
                    break;
                }
                WorkerEvent::TurnDone { .. } => {}
                _ => {}
            }
        }
        let wall_ms = started.elapsed().as_millis() as u64;
        let text = text.expect("the measured turn must complete");
        let records = audit_records(&dir.join("audit.jsonl"));
        let attempts: Vec<&serde_json::Value> = records
            .iter()
            .filter(|r| r["kind"] == "provider_attempt")
            .collect();
        let tokens: u64 = attempts
            .iter()
            .filter_map(|r| r["detail"]["charged_tokens"].as_u64())
            .sum();
        let mut agents_seen: Vec<String> = attempts
            .iter()
            .filter_map(|r| r["agent_id"].as_str().map(str::to_string))
            .collect();
        agents_seen.sort();
        agents_seen.dedup();
        let queued: Vec<u64> = records
            .iter()
            .filter(|r| r["kind"] == "execution")
            .filter_map(|r| r["detail"]["queued_ms"].as_u64())
            .collect();
        let _ = std::fs::remove_dir_all(&dir);
        json!({
            "measurement": "hierarchy",
            "harness": "p6_hierarchy_stub_measurement",
            "provider": "stub",
            "mode": if delegated { "delegated" } else { "flat" },
            "delegated": delegated,
            "run": run,
            "max_steps": 5,
            "child_steps": 10,
            "wall_ms": wall_ms,
            "time_to_first_action_ms": first_action_ms.expect("an action must run"),
            "provider_attempts": attempts.len(),
            "tokens_charged": tokens,
            "queued_ms": queued,
            "agents_seen": agents_seen,
            "outcome": "done",
            "text": text,
        })
    }

    /// Hierarchy stub measurement (F6-10): the Phase 6
    /// measurement harness, restored into the tree. Ignored
    /// by default — run explicitly:
    /// `cargo test -p mimint-ui p6_hierarchy_stub_measurement -- --ignored`
    /// with `MIMINT_P6_MEASURE_OUT` set to a JSONL path
    /// (default: a temp file, path printed) and
    /// `MIMINT_P6_MEASURE_RUNS` (default 3). No threshold
    /// gating: every run must complete, and the numbers are
    /// recorded as-is.
    #[test]
    #[ignore]
    fn p6_hierarchy_stub_measurement() {
        let runs: usize = std::env::var("MIMINT_P6_MEASURE_RUNS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3);
        let out_path = std::env::var("MIMINT_P6_MEASURE_OUT")
            .ok()
            .unwrap_or_else(|| {
                std::env::temp_dir()
                    .join(format!("mimint-p6-measure-{}.jsonl", std::process::id()))
                    .to_string_lossy()
                    .into_owned()
            });
        let mut out = std::fs::File::create(&out_path).expect("create measurement output");
        eprintln!("p6 measurement output: {out_path}");
        for run in 1..=runs {
            for delegated in [false, true] {
                let row = measure_one(run, delegated);
                use std::io::Write;
                writeln!(out, "{}", serde_json::to_string(&row).unwrap()).unwrap();
            }
        }
        eprintln!("p6 measurement: {runs} runs x flat/delegated written to {out_path}");
    }
}
