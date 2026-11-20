//! The agent coordinator (Phase 6, §2): the single authority
//! that issues agent ids, owns the table of live agents, and
//! carries out cancels and failure scoping. It runs no brain, it
//! makes no guard decision, and it never touches the body
//! directly: the worker loop drives it, and push-cancel reaches
//! the actor and the approver through the collaborators the loop
//! passes in.
//!
//! Ids are `agent-NNN`, issued centrally from a counter that
//! starts at 1 and never reuses an id within the process
//! (`agent-000` is the system, `agent-001` the default agent).
//! The coordinator also owns the shared per-turn machinery the
//! design partitions per agent: the cost ledger, the snapshot
//! slots, the MCP manager, the child-process registry, and the
//! sandbox. Run directories are tracked per-turn (in the agent
//! entry), not shared: one turn's cleanup must never delete
//! another running agent's workdir.
//!
//! Cancellation (§10) is synchronous from the loop's point of
//! view: cancel the token, push the cancel into the actor and
//! the approver, abort the task, and join it within a 5-second
//! bound. A join past the bound leaves the coordinator
//! DEGRADED — no new spawns until restart — and the agent's end
//! unverified. A task that already finished when the cancel
//! lands is left to its queued completion message: the cancel
//! path never rewrites a finished turn into a cancelled one.
//!
//! Failure scoping (§4, G6-9): a body fault (timeout, panic,
//! dead thread, broken actor, unknown outcome) or a GLOBAL
//! budget exhaustion fails ALL agents; anything else fails only
//! the agent that hit it. The scope record is written once, by
//! the loop, through [`audit_failure_scope`].

use crate::approver::{TokenSlot, UiApprover};
use crate::worker::{FailureScope, WorkerEvent};
use mimint_control::{AgentSnapshotSlots, BodyActor};
use mimint_core::audit::now_ms;
use mimint_core::{
    format_agent_id, AuditLog, BudgetScope, CancelReport, CancelToken, ChildProcessRegistry,
    CostLedger, ExperienceStore, KillLedger, McpManager, Profile, RunDirTracker, Sandbox, SkillDef,
};
use serde_json::json;
use std::collections::HashMap;
use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;

/// How long the coordinator waits for a cancelled task to end
/// before declaring its end unverified (§10, path 3).
pub const JOIN_BOUND: Duration = Duration::from_secs(5);

/// How the end of one agent was verified, for the
/// `agent_failure_scope` record (§4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndVerification {
    /// The task joined within the bound.
    Joined,
    /// The join timed out: the task may still be running.
    TimeoutUnverified,
    /// The agent had no running task (idle, or already ended).
    NotApplicable,
}

impl EndVerification {
    pub fn as_str(&self) -> &'static str {
        match self {
            EndVerification::Joined => "joined",
            EndVerification::TimeoutUnverified => "timeout_unverified",
            EndVerification::NotApplicable => "not_applicable",
        }
    }
}

/// Lifecycle state of one agent entry, as the coordinator sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    /// Registered, no task running.
    Idle,
    /// A task is running.
    Running,
    /// Ended, join verified.
    EndedVerified,
    /// Ended (or still running) without join verification.
    EndedUnverified,
}

/// One live (or recently ended) agent.
pub struct AgentEntry {
    pub agent_id: String,
    /// Display label: the task text, cut to 40 characters.
    pub label: String,
    /// The parent agent, for hierarchy children.
    pub parent: Option<String>,
    /// 0 for top-level agents, 1 for children (§8: depth ≤ 1).
    pub depth: u8,
    pub state: AgentState,
    pub join_handle: Option<JoinHandle<()>>,
    /// The token handed to the brain (the slot's current token
    /// at spawn time; cancelling it cancels the turn).
    pub cancel_token: CancelToken,
    /// The turn slot the approver watches for this agent.
    pub turn_slot: Arc<TokenSlot>,
    /// §10.2 cancel context, shared with the brain.
    pub cancel_report: Arc<CancelReport>,
    /// Child processes killed during this turn (§10.2).
    pub kill_ledger: KillLedger,
    pub spawned_at: Instant,
    pub turn_id: String,
    /// Failures of this agent's children, collected for the
    /// parent's turn-done event (§8.5).
    pub delegate_failures: Arc<Mutex<Vec<String>>>,
    /// Token budget for a hierarchy child (`None` = unlimited).
    pub budget: Option<u64>,
    /// This turn's run directories (§5): cleaned when the turn
    /// ends. Per-turn, not shared: a global tracker would let
    /// one turn's cleanup delete another running agent's
    /// workdir.
    pub run_dirs: Arc<RunDirTracker>,
}

/// Everything a turn needs from the active profile, snapshotted
/// at spawn time (Phase 5 wiring + Phase 6 knobs). Children
/// inherit their parent's snapshot: a profile switch mid-turn
/// must not change what a running turn sees.
#[derive(Clone)]
pub struct TurnRuntime {
    pub profile_name: String,
    /// The system prompt for turns, or `None` when SOUL must be
    /// withheld (Privacy Mode + a remote backend in the chain).
    pub soul_prompt: Option<String>,
    /// Promoted skills, filtered by the profile's selection.
    pub skills: Vec<SkillDef>,
    /// True when user data (SOUL, experience) may reach the
    /// backends.
    pub may_send_user_data: bool,
    pub experience: Option<Arc<ExperienceStore>>,
    /// Reserve basis for provider calls (§3.3).
    pub max_tokens: u32,
    /// R6 batch-loop threshold (§7.1).
    pub gui_batch_threshold: usize,
    /// Step budget for the turn.
    pub max_steps: usize,
    /// The profile, for audit tags.
    pub profile: Profile,
}

impl std::fmt::Debug for TurnRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnRuntime")
            .field("profile_name", &self.profile_name)
            .field("soul_prompt", &self.soul_prompt.as_ref().map(|p| p.len()))
            .field(
                "skills",
                &self.skills.iter().map(|s| &s.name).collect::<Vec<_>>(),
            )
            .field("may_send_user_data", &self.may_send_user_data)
            .field("experience", &self.experience.is_some())
            .field("max_tokens", &self.max_tokens)
            .field("gui_batch_threshold", &self.gui_batch_threshold)
            .field("max_steps", &self.max_steps)
            .finish()
    }
}

/// A request to spawn one hierarchy child (§8), sent by the
/// `delegate` tool to the worker loop.
#[derive(Debug)]
pub struct ChildSpec {
    pub parent_id: String,
    pub task: String,
    pub max_steps: usize,
    /// Requested token budget (`None` = default: a quarter of
    /// the parent's remaining budget, or unlimited when the
    /// parent is unlimited).
    pub token_budget: Option<u64>,
    pub time_limit_secs: u64,
    /// The parent's turn snapshot, inherited by the child.
    pub runtime: TurnRuntime,
}

/// How one child ended, as the `delegate` tool sees it.
#[derive(Debug)]
pub enum ChildOutcome {
    /// The child finished (possibly partially: `partial`).
    Completed { text: String, partial: bool },
    /// The child failed; `text` is what it produced, if any.
    Failed { text: String, error: String },
    /// The child hit its wall-clock limit.
    TimedOut,
    /// The loop refused to spawn the child (limit, capacity,
    /// degraded coordinator).
    Refused { reason: String },
}

/// The reply to a [`ChildSpec`]: which agent ran, and how it ended.
#[derive(Debug)]
pub struct ChildResult {
    pub agent_id: String,
    pub outcome: ChildOutcome,
}

/// The outcome of one agent's task, as the spawned task reports
/// it to the loop. Classified by the task itself, where the
/// error type is still available.
#[derive(Debug)]
pub enum TaskOutcome {
    /// The brain finished (possibly partially). Boxed: `TurnOutcome`
    /// is a large struct (text + several Vecs), and an unboxed
    /// variant trips `clippy::large_enum_variant` (P8.5).
    Done(Box<mimint_core::TurnOutcome>),
    /// The brain failed. `body_fault` marks G6-9 fail-all errors.
    Failed { error: String, body_fault: bool },
    /// The ledger refused a reserve. A GLOBAL scope fails all
    /// agents; an agent scope fails only this one (§3.3).
    BudgetExhausted { scope: BudgetScope, error: String },
    /// The turn was cancelled.
    Cancelled,
    /// The task panicked. A single-agent fault (§4): it must
    /// never trip the stop latch.
    Panicked(String),
}

/// True when an error message names a body fault (G6-9): an
/// unknown outcome, a broken actor, or a dead actor thread.
/// The substrings are the exact texts the actor and the body
/// tools produce (`unknown_outcome` is the error code prefix
/// `body_err_to_tool` adds; the other two are the actor's
/// messages verbatim).
pub fn is_body_fault(error: &str) -> bool {
    // The predicate lives in mimint-core, shared with the
    // brain's fatal-body-fault arm (§4.1); this wrapper keeps
    // the coordinator's call sites unchanged.
    mimint_core::is_body_fault_message(error)
}

/// A future wrapper that catches panics during polling and
/// reports them as values instead of unwinding the task: the
/// spawned task must ALWAYS report its outcome to the loop,
/// even when the brain panics. Without this, a panic would
/// leave the agent entry stranded with no completion message.
pub struct CatchUnwind<F> {
    inner: F,
}

impl<F> CatchUnwind<F> {
    pub fn new(inner: F) -> Self {
        Self { inner }
    }
}

impl<F: Future> Future for CatchUnwind<F> {
    type Output = Result<F::Output, Box<dyn std::any::Any + Send + 'static>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: the projection only polls the inner future;
        // it is never moved out, and the outer pin guarantees
        // the inner future stays pinned for the poll.
        let inner = unsafe { self.map_unchecked_mut(|s| &mut s.inner) };
        match catch_unwind(AssertUnwindSafe(|| inner.poll(cx))) {
            Ok(Poll::Ready(value)) => Poll::Ready(Ok(value)),
            Ok(Poll::Pending) => Poll::Pending,
            Err(payload) => Poll::Ready(Err(payload)),
        }
    }
}

/// The panic message from a caught payload, best-effort.
///
/// Takes the box by reference on purpose: passing `&payload`
/// (a `&Box<dyn Any>`) to a `&(dyn Any)` parameter coerces to
/// the BOX itself as the `Any` value — `Box<dyn Any>` also
/// implements `Any` — so every downcast misses and every panic
/// reads as "unknown panic". `as_ref()` below names the inner
/// value unambiguously.
pub fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    let inner: &(dyn std::any::Any + Send) = payload.as_ref();
    inner
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| inner.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_string())
}

/// What a synchronous cancel achieved, for the loop's event and
/// audit decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOutcome {
    /// The task was already finished: its queued completion
    /// message drives the outcome; the cancel changed nothing.
    AlreadyFinished,
    /// The task ended (joined) within the bound.
    EndedVerified,
    /// The join timed out: the end is unverified, the
    /// coordinator is degraded.
    EndUnverified,
    /// The agent had no running task.
    NotRunning,
}

/// The coordinator: id authority, agent table, shared per-turn
/// machinery, and the cancel/fail-all machinery. All mutation
/// happens on the worker thread (the loop), so the table needs
/// no lock.
pub struct AgentCoordinator {
    next_id: AtomicU64,
    agents: HashMap<String, AgentEntry>,
    turn_seqs: HashMap<String, u64>,
    max_agents: usize,
    degraded: Option<String>,
    /// The cost ledger (§3.3): one ledger for all agents.
    pub ledger: Arc<CostLedger>,
    /// Per-agent approval snapshot slots (§3).
    pub snapshot_slots: Arc<AgentSnapshotSlots>,
    /// The MCP manager, when MCP is configured (never on Android).
    pub mcp: Option<Arc<McpManager>>,
    /// Live child processes by agent (§10, path 2 backstop).
    pub children: ChildProcessRegistry,
    /// The sandbox (§5), shared by execute_code and MCP servers.
    pub sandbox: Arc<Sandbox>,
}

impl AgentCoordinator {
    pub fn new(
        max_agents: usize,
        ledger: Arc<CostLedger>,
        snapshot_slots: Arc<AgentSnapshotSlots>,
        mcp: Option<Arc<McpManager>>,
        children: ChildProcessRegistry,
        sandbox: Arc<Sandbox>,
    ) -> Self {
        Self {
            next_id: AtomicU64::new(1),
            agents: HashMap::new(),
            turn_seqs: HashMap::new(),
            max_agents,
            degraded: None,
            ledger,
            snapshot_slots,
            mcp,
            children,
            sandbox,
        }
    }

    /// The next fresh id. Ids are never reused in a process.
    pub fn next_agent_id(&self) -> String {
        format_agent_id(self.next_id.fetch_add(1, Ordering::SeqCst))
    }

    /// The default agent's entry, idle, registered at startup.
    /// Its turn slot is the shared UI slot: "Hủy lượt" trips it.
    pub fn register_default_agent(&mut self, turn_slot: Arc<TokenSlot>) {
        let agent_id = self.next_agent_id();
        debug_assert_eq!(agent_id, "agent-001");
        self.insert_idle_default(agent_id, turn_slot);
    }

    /// Restore the default agent's idle entry after a cancel
    /// reaped it. The id is FIXED (`agent-001`): this is not id
    /// reuse — the default id never leaves the default role —
    /// and the counter is untouched.
    pub fn reregister_default_agent(&mut self, turn_slot: Arc<TokenSlot>) {
        self.insert_idle_default("agent-001".to_string(), turn_slot);
    }

    fn insert_idle_default(&mut self, agent_id: String, turn_slot: Arc<TokenSlot>) {
        self.agents.insert(
            agent_id.clone(),
            AgentEntry {
                agent_id,
                label: "Agent 1".to_string(),
                parent: None,
                depth: 0,
                state: AgentState::Idle,
                join_handle: None,
                cancel_token: CancelToken::new(),
                turn_slot,
                cancel_report: Arc::new(CancelReport::new()),
                kill_ledger: KillLedger::new(),
                spawned_at: Instant::now(),
                turn_id: String::new(),
                delegate_failures: Arc::new(Mutex::new(Vec::new())),
                budget: None,
                run_dirs: Arc::new(RunDirTracker::new()),
            },
        );
    }

    /// The next turn id for one agent: `ui-turn-{agent}-{pid}-{ms}-{seq}`,
    /// with a per-agent monotonic sequence (§1).
    pub fn next_turn_id(&mut self, agent_id: &str) -> String {
        let seq = self.turn_seqs.entry(agent_id.to_string()).or_insert(0);
        *seq += 1;
        format!(
            "ui-turn-{}-{}-{}-{}",
            agent_id,
            std::process::id(),
            now_ms(),
            seq
        )
    }

    pub fn get(&self, agent_id: &str) -> Option<&AgentEntry> {
        self.agents.get(agent_id)
    }

    pub fn get_mut(&mut self, agent_id: &str) -> Option<&mut AgentEntry> {
        self.agents.get_mut(agent_id)
    }

    pub fn agents(&self) -> impl Iterator<Item = &AgentEntry> {
        self.agents.values()
    }

    /// Agents with a running task.
    pub fn live_count(&self) -> usize {
        self.agents
            .values()
            .filter(|e| e.state == AgentState::Running)
            .count()
    }

    /// Live children of one parent (for the §8.2 child limit).
    pub fn live_children_of(&self, parent_id: &str) -> usize {
        self.agents
            .values()
            .filter(|e| e.state == AgentState::Running && e.parent.as_deref() == Some(parent_id))
            .count()
    }

    /// True when a new agent may spawn: not degraded, and a
    /// free slot. The caller audits/denies on `false`.
    pub fn can_spawn(&self) -> bool {
        self.degraded.is_none() && self.live_count() < self.max_agents
    }

    pub fn max_agents(&self) -> usize {
        self.max_agents
    }

    /// The degrade reason, when degraded. Degraded lasts until
    /// restart: there is deliberately no clear API (§10).
    pub fn degraded(&self) -> Option<&str> {
        self.degraded.as_deref()
    }

    pub fn mark_degraded(&mut self, reason: String) {
        self.degraded = Some(reason);
    }

    /// Register a freshly spawned task. The entry must already
    /// exist (idle) or be created by the caller.
    pub fn register_running(&mut self, entry: AgentEntry) {
        self.agents.insert(entry.agent_id.clone(), entry);
    }

    /// Reap an agent: drop its snapshot slot and retire its id.
    /// Idempotent: reaping an unknown id is a no-op.
    pub fn reap(&mut self, agent_id: &str) {
        self.snapshot_slots.remove(agent_id);
        self.agents.remove(agent_id);
    }

    /// Cancel one agent synchronously (§10): cancel the token,
    /// push the cancel into the actor and the approver, kill
    /// the agent's child processes (backstop), abort the task,
    /// and join within [`JOIN_BOUND`].
    ///
    /// On a verified end the entry is reaped and a `turn_end`
    /// record is written (the aborted brain could not write
    /// its own). On an unverified end the entry is kept in
    /// `EndedUnverified` state, the coordinator degrades, and
    /// a `task_end_unverified` record is written; a late
    /// completion message still reaps the entry, but the
    /// degraded flag never clears on its own.
    #[allow(clippy::too_many_arguments)]
    pub fn cancel_agent_sync(
        &mut self,
        agent_id: &str,
        source: &str,
        runtime: &tokio::runtime::Runtime,
        actor: Option<&BodyActor>,
        approver: &UiApprover,
        audit: &AuditLog,
        event_tx: &Sender<WorkerEvent>,
    ) -> CancelOutcome {
        let Some(entry) = self.agents.get_mut(agent_id) else {
            return CancelOutcome::NotRunning;
        };
        let Some(handle) = entry.join_handle.take() else {
            entry.state = AgentState::EndedVerified;
            return CancelOutcome::NotRunning;
        };
        if handle.is_finished() {
            // The task completed (its message is queued): do
            // not rewrite a finished turn into a cancelled one.
            entry.join_handle = Some(handle);
            return CancelOutcome::AlreadyFinished;
        }
        let turn_id = entry.turn_id.clone();
        let report = Arc::clone(&entry.cancel_report);
        let kill_ledger = entry.kill_ledger.clone();
        report.mark_requested(source);
        entry.cancel_token.cancel_with_source(source);
        entry.turn_slot.cancel_current();
        // Push-cancel (F6D-02): in-flight body waits and
        // approval cards resolve as Cancelled at once.
        if let Some(actor) = actor {
            actor.cancel_agent(agent_id);
        }
        approver.withdraw_agent(agent_id);
        report.mark_push_cancelled();
        // Backstop: kill the agent's child processes. The
        // tools' own wait loops kill first; this catches any
        // child whose tool never saw the token trip.
        let _killed = self.children.kill_all(agent_id);
        handle.abort();
        let joined = runtime.block_on(async { tokio::time::timeout(JOIN_BOUND, handle).await });
        match joined {
            Ok(Ok(_outcome)) => {
                // The task finished between the check and the
                // abort: its queued message drives the outcome.
                CancelOutcome::AlreadyFinished
            }
            Ok(Err(_join_err)) => {
                // Aborted (or panicked past the guard): the
                // brain could not write its own turn_end, so
                // the coordinator writes it with the §10.2
                // context it owns. `in_flight` is omitted: the
                // coordinator cannot know the brain's phase.
                let killed = kill_ledger.entries();
                let killed_desc: Vec<String> = killed
                    .iter()
                    .map(|e| format!("child_process:{}", e.pid))
                    .collect();
                let _ = audit.log_for(
                    agent_id,
                    &turn_id,
                    "turn_end",
                    None,
                    Some("cancelled"),
                    json!({
                        "reason": "cancelled",
                        "source": source,
                        "killed": if killed_desc.is_empty() { "none".to_string() } else { killed_desc.join(",") },
                        "cancel_latency_ms": report.latency_ms().map(|v| v as u64),
                        "push_cancelled": report.push_cancelled(),
                    }),
                );
                self.reap(agent_id);
                CancelOutcome::EndedVerified
            }
            Err(_elapsed) => {
                // The task did not end within the bound: its
                // end is unverified, and the coordinator is
                // degraded until restart (§10, path 3).
                let reason = format!(
                    "task for {agent_id} did not end within {}s of cancel; its end is unverified",
                    JOIN_BOUND.as_secs()
                );
                let _ = audit.log_for(
                    agent_id,
                    &turn_id,
                    "task_end_unverified",
                    None,
                    Some("unverified"),
                    json!({
                        "reason": "task_end_unverified",
                        "source": source,
                        "join_timeout_secs": JOIN_BOUND.as_secs(),
                        "push_cancelled": report.push_cancelled(),
                    }),
                );
                self.mark_degraded(reason.clone());
                if let Some(entry) = self.agents.get_mut(agent_id) {
                    entry.state = AgentState::EndedUnverified;
                }
                let _ = event_tx.send(WorkerEvent::Degraded {
                    agent_id: agent_id.to_string(),
                    reason,
                });
                CancelOutcome::EndUnverified
            }
        }
    }
}

/// Write the single `agent_failure_scope` record (§4) for a
/// failure that may span agents. `agents` carries one
/// (agent_id, end_verified, end_verification) triple per
/// affected agent; the totals are the AND of the flags and the
/// common verification value, or "mixed" when they differ.
#[allow(clippy::too_many_arguments)]
pub fn audit_failure_scope(
    audit: &AuditLog,
    scope: FailureScope,
    cause: &str,
    failed_agent: &str,
    stop_latched: bool,
    actor_broken: bool,
    agents: &[(String, bool, EndVerification)],
) {
    let end_verified = agents.iter().all(|(_, verified, _)| *verified);
    let mut verifications = agents
        .iter()
        .map(|(_, _, v)| v.as_str())
        .collect::<Vec<_>>();
    verifications.dedup();
    let end_verification = if verifications.len() == 1 {
        verifications[0].to_string()
    } else {
        "mixed".to_string()
    };
    let agent_list: Vec<serde_json::Value> = agents
        .iter()
        .map(|(id, verified, verification)| {
            json!({
                "agent_id": id,
                "end_verified": verified,
                "end_verification": verification.as_str(),
            })
        })
        .collect();
    let _ = audit.log(
        "ui-fault",
        "agent_failure_scope",
        None,
        Some(scope.as_str()),
        json!({
            "scope": scope.as_str(),
            "cause": cause,
            "failed_agent": failed_agent,
            "stop_latched": stop_latched,
            "actor_broken": actor_broken,
            "end_verified": end_verified,
            "end_verification": end_verification,
            "agents": agent_list,
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use mimint_core::{format_agent_id, AGENT_SYSTEM};

    #[test]
    fn ids_are_sequential_and_never_reused() {
        let ledger = Arc::new(CostLedger::unbounded());
        let slots = Arc::new(AgentSnapshotSlots::new());
        let children = ChildProcessRegistry::new();
        let sandbox = Arc::new(Sandbox::from_config(&mimint_core::SandboxConfig::default()));
        let mut coord = AgentCoordinator::new(3, ledger, slots, None, children, sandbox);
        assert_eq!(coord.next_agent_id(), "agent-001");
        assert_eq!(coord.next_agent_id(), "agent-002");
        coord.reap("agent-001");
        assert_eq!(coord.next_agent_id(), "agent-003");
        assert_eq!(format_agent_id(0), AGENT_SYSTEM);
    }

    #[test]
    fn turn_ids_carry_agent_pid_and_sequence() {
        let ledger = Arc::new(CostLedger::unbounded());
        let slots = Arc::new(AgentSnapshotSlots::new());
        let children = ChildProcessRegistry::new();
        let sandbox = Arc::new(Sandbox::from_config(&mimint_core::SandboxConfig::default()));
        let mut coord = AgentCoordinator::new(3, ledger, slots, None, children, sandbox);
        let first = coord.next_turn_id("agent-002");
        let second = coord.next_turn_id("agent-002");
        let other = coord.next_turn_id("agent-001");
        assert!(first.starts_with("ui-turn-agent-002-"), "{first}");
        assert!(second.starts_with("ui-turn-agent-002-"), "{second}");
        assert_ne!(first, second);
        assert!(second.ends_with("-2"), "{second}");
        assert!(other.ends_with("-1"), "{other}");
    }

    #[test]
    fn capacity_and_degraded_gate_spawns() {
        let ledger = Arc::new(CostLedger::unbounded());
        let slots = Arc::new(AgentSnapshotSlots::new());
        let children = ChildProcessRegistry::new();
        let sandbox = Arc::new(Sandbox::from_config(&mimint_core::SandboxConfig::default()));
        let mut coord = AgentCoordinator::new(1, ledger, slots, None, children, sandbox);
        assert!(coord.can_spawn());
        coord.mark_degraded("test".to_string());
        assert!(!coord.can_spawn());
        assert_eq!(coord.degraded(), Some("test"));
    }

    #[test]
    fn body_fault_classifier_matches_actor_texts() {
        assert!(is_body_fault(
            "unknown_outcome: action outcome unknown (not auto-retried): timed out"
        ));
        assert!(is_body_fault(
            "backend: body actor is broken (a previous action timed out); rebuild the actor"
        ));
        assert!(is_body_fault("backend: body actor thread is gone"));
        assert!(!is_body_fault("provider returned 500"));
        assert!(!is_body_fault("approval denied by the user"));
        assert!(!is_body_fault("sandbox_unavailable: bwrap not found"));
    }

    #[test]
    fn catch_unwind_reports_a_panic_as_a_value() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = runtime.block_on(async { CatchUnwind::new(async { panic!("boom") }).await });
        let payload = result.expect_err("panic must be caught");
        assert_eq!(panic_message(&payload), "boom");
        let ok = runtime.block_on(async { CatchUnwind::new(async { 42 }).await });
        assert_eq!(ok.unwrap(), 42);
    }
}
