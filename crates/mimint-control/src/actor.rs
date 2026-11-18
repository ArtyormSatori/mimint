//! The body actor: one dedicated thread owns the body.
//!
//! The `Body` trait is synchronous and `!Sync` in spirit (bodies
//! own OS connections and threads of their own); the brain is
//! async and its tools are `Send + Sync`. The actor bridges
//! the two: tools send [`Action`]s over a channel, the actor
//! thread executes them against the body, and replies come
//! back over a per-request channel.
//!
//! Per-action timeout (P4.6.3): every request waits at most
//! `timeout` for its reply. On timeout the actor is marked
//! broken and fails fast afterwards: the body thread may still
//! be stuck inside a platform call, and sending it more work
//! would pile up behind the stuck call. The timeout bounds
//! the WAIT, not the platform call — a stuck body thread is
//! detached, never killed (there is no safe way to kill a
//! thread stuck in a foreign OS call). The broken state is
//! terminal for the actor: the caller builds a fresh actor
//! for a fresh body.
//!
//! Unknown outcomes (F4-14): a timeout, a dead thread, or a
//! panicking body reports [`BodyError::UnknownOutcome`], never
//! a generic backend error — the action may or may not have
//! executed, and the caller must not retry automatically. A
//! panicking body ALSO breaks the actor and ends the loop: the
//! body's state after a panic is unknown, and serving more
//! requests against it would be guessing.
//!
//! Stop integration: the actor checks the shared [`StopFlag`]
//! before executing each action. A tripped flag refuses the
//! action with a typed error and never touches the body.
//!
//! Target probes (F4-07): [`BodyActor::probe_target`] asks the
//! body, through the same serialized channel, for the window
//! identity and re-verify snapshot under a point — so the
//! approver's snapshot and the action's execution can never
//! interleave with another action.

//! Multi-agent queues (Phase 6, §9): one FIFO queue per
//! agent, served round-robin — exactly one action per agent
//! per round, no priorities. Caps (64 total / 16 per agent
//! by default) and the queue SLA (60s) are enforced inside
//! the loop: over-cap and expired requests are denied
//! without executing and never run late, and neither denial
//! breaks the actor. Replies arrive in two phases —
//! `Started` (with the queue wait) then the outcome — so the
//! caller can tell "never started" (queue expiry) from "may
//! have executed" (execution timeout). Push-cancel (§10):
//! [`BodyActor::cancel_agent`] makes waiting callers return
//! `Cancelled` within milliseconds and drops queued work.

use mimint_body::{ActOutcome, Action, Body, BodyError, StopFlag, TargetProbe};
use mimint_core::{AuditLog, AGENT_DEFAULT};
use serde_json::json;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Default per-action timeout: generous for a local body, but
/// bounded. A body that cannot answer in 30s is wedged.
pub const DEFAULT_ACTION_TIMEOUT: Duration = Duration::from_secs(30);

/// What the actor thread can be asked to do. Actions and probes
/// share one channel, so a probe can never run concurrently
/// with an action against the same body.
/// Configuration for the actor's queue and timeouts
/// (Phase 6, §9). The loop enforces the caps and the SLA;
/// callers enforce their own wait bounds.
#[derive(Debug, Clone, Copy)]
pub struct ActorConfig {
    /// Bound on the EXECUTION wait: after `Started`, the
    /// caller waits at most this long for the outcome.
    /// Unchanged from Phase 4 (30s default).
    pub action_timeout: Duration,
    /// Bound on the QUEUE wait: a request that has not
    /// started within this long of enqueueing is denied
    /// without executing — never runs late.
    pub queue_sla: Duration,
    /// Maximum requests queued across all agents.
    pub max_queued_total: usize,
    /// Maximum requests queued for one agent.
    pub max_queued_per_agent: usize,
}

impl Default for ActorConfig {
    fn default() -> Self {
        ActorConfig {
            action_timeout: DEFAULT_ACTION_TIMEOUT,
            queue_sla: Duration::from_secs(60),
            max_queued_total: 64,
            max_queued_per_agent: 16,
        }
    }
}

/// Queue SLA for target probes (Phase 6, §9): a probe that
/// has not started within 5s of enqueueing is denied. The
/// probe is a read, so the denial never breaks the actor.
pub const PROBE_QUEUE_SLA: Duration = Duration::from_secs(5);

/// What the actor thread can be asked to do. Actions and probes
/// share one channel, so a probe can never run concurrently
/// with an action against the same body.
enum RequestKind {
    Act(Action),
    ProbeTarget { x: i32, y: i32 },
}

/// A reply from the actor thread, in two phases (§9).
/// `Started` arrives just before execution, carrying the
/// queue wait; the outcome follows. A caller that saw
/// `Started` and then times out reports `UnknownOutcome`
/// (the action may have executed); a caller that never saw
/// `Started` reports a queue expiry (nothing executed).
enum RequestReply {
    Started { queued_ms: u64 },
    Action(Result<ActOutcome, BodyError>),
    ProbeTarget(Result<TargetProbe, BodyError>),
}

struct Request {
    agent_id: String,
    kind: RequestKind,
    /// Approval-time mutation generation (F6-03): when set,
    /// the loop refuses the action at dequeue time unless
    /// the generation still matches — another agent's action
    /// in the meantime invalidates the approval. Only
    /// `act_for_checked` sets it; probes never carry one.
    expected_seq: Option<u64>,
    enqueued_at: Instant,
    /// Enqueue + queue SLA (actions) or probe SLA (probes).
    /// The loop denies the request past this instant.
    deadline: Instant,
    reply: Sender<RequestReply>,
}

impl Request {
    fn sla_secs(&self) -> u64 {
        self.deadline
            .duration_since(self.enqueued_at)
            .as_secs()
            .max(1)
    }

    fn queued_ms(&self) -> u64 {
        self.enqueued_at.elapsed().as_millis() as u64
    }
}

/// Build a denial reply matching the request kind.
fn deny_reply(kind: &RequestKind, err: BodyError) -> RequestReply {
    match kind {
        RequestKind::Act(_) => RequestReply::Action(Err(err)),
        RequestKind::ProbeTarget { .. } => RequestReply::ProbeTarget(Err(err)),
    }
}

struct ActorInner {
    tx: Sender<Request>,
    /// Set once a request times out, the thread dies, or the
    /// body panics: all later requests fail fast without
    /// touching the channel. Shared with the loop thread so a
    /// panic inside the loop breaks the actor too.
    broken: Arc<Mutex<bool>>,
    /// Agents whose in-flight work must stop now (§10):
    /// callers poll this while waiting and return
    /// `Cancelled` within milliseconds; the loop drops
    /// queued requests of cancelled agents without running
    /// them.
    cancelled: Arc<Mutex<HashSet<String>>>,
    /// Last queue wait per agent, recorded from `Started`
    /// (§9): body tools read it for the `queued_ms` audit
    /// field. Written by the waiting caller, never by the
    /// loop, so no cross-agent race is possible.
    last_queued: Arc<Mutex<HashMap<String, u64>>>,
    /// Body mutation generation (F6-03): the count of
    /// non-screenshot actions the loop has dispatched.
    /// Incremented by the loop thread only, after every
    /// dispatched non-screenshot `body.act()` returns (Ok or
    /// Err — a refused action may have partially executed,
    /// so it still invalidates approvals). Probes and
    /// denied requests never bump it. Shared with callers:
    /// re-verification compares the approval snapshot's
    /// generation against it, and probe replies are stamped
    /// with it.
    mutation_seq: Arc<AtomicU64>,
}

/// A body behind a dedicated thread, with a per-action timeout.
pub struct BodyActor {
    inner: Arc<ActorInner>,
    stop: Arc<StopFlag>,
    timeout: Duration,
    config: ActorConfig,
}

/// Outcome of waiting for a reply: a real reply, or the
/// reason the wait ended without one. The caller maps the
/// reason to the error its path requires (an action timeout
/// breaks the actor; a probe timeout does not).
enum WaitResult {
    Reply(RequestReply),
    /// The wait bound passed. `started` says whether a
    /// `Started` arrived first (the action may have
    /// executed) or not (nothing executed).
    TimedOut {
        started: bool,
    },
    /// The actor thread is gone.
    ThreadDead,
    /// The agent was cancelled while waiting.
    Cancelled,
}

impl BodyActor {
    /// Spawn the actor thread owning `body`. The thread runs
    /// until the last `BodyActor` handle is dropped (the
    /// channel closes) or a request times out (the thread is
    /// detached and the actor fails fast).
    pub fn spawn(body: Box<dyn Body>, stop: Arc<StopFlag>) -> BodyActor {
        Self::spawn_with_timeout(body, stop, DEFAULT_ACTION_TIMEOUT)
    }

    pub fn spawn_with_timeout(
        body: Box<dyn Body>,
        stop: Arc<StopFlag>,
        timeout: Duration,
    ) -> BodyActor {
        Self::spawn_with_config(
            body,
            stop,
            ActorConfig {
                action_timeout: timeout,
                ..ActorConfig::default()
            },
        )
    }

    /// Spawn with an explicit queue/timeout configuration
    /// (Phase 6, §9). No audit sink: queue denials are
    /// replied but not audited.
    pub fn spawn_with_config(
        body: Box<dyn Body>,
        stop: Arc<StopFlag>,
        config: ActorConfig,
    ) -> BodyActor {
        Self::spawn_with_config_and_audit(body, stop, config, None)
    }

    /// Spawn with a queue configuration and an audit sink:
    /// queue-full and queue-expired denials are audited
    /// (`kind=decision`, `decision=deny`, turn `ui-queue`)
    /// through the sink, attributed to the denied agent.
    pub fn spawn_with_config_and_audit(
        body: Box<dyn Body>,
        stop: Arc<StopFlag>,
        config: ActorConfig,
        audit: Option<Arc<AuditLog>>,
    ) -> BodyActor {
        let (tx, rx) = channel::<Request>();
        let broken = Arc::new(Mutex::new(false));
        let cancelled = Arc::new(Mutex::new(HashSet::new()));
        let last_queued = Arc::new(Mutex::new(HashMap::new()));
        let mutation_seq = Arc::new(AtomicU64::new(0));
        let thread_stop = Arc::clone(&stop);
        let thread_broken = Arc::clone(&broken);
        let thread_cancelled = Arc::clone(&cancelled);
        let thread_mutation_seq = Arc::clone(&mutation_seq);
        std::thread::spawn(move || {
            actor_loop(
                body,
                rx,
                thread_stop,
                thread_broken,
                config,
                thread_cancelled,
                audit,
                thread_mutation_seq,
            )
        });
        BodyActor {
            inner: Arc::new(ActorInner {
                tx,
                broken,
                cancelled,
                last_queued,
                mutation_seq,
            }),
            stop,
            timeout: config.action_timeout,
            config,
        }
    }

    /// Push-cancel for one agent (§10): requests waiting for
    /// a reply return `Cancelled` within milliseconds, and
    /// queued requests are dropped without executing. The
    /// actor itself is NOT broken, and other agents are
    /// unaffected. An action already executing cannot be
    /// interrupted — its outcome is discarded.
    pub fn cancel_agent(&self, agent_id: &str) {
        self.inner
            .cancelled
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(agent_id.to_string());
    }

    /// True when `cancel_agent` was called for this agent.
    pub fn is_agent_cancelled(&self, agent_id: &str) -> bool {
        self.inner
            .cancelled
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains(agent_id)
    }

    /// Milliseconds the agent's last request waited in the
    /// queue before starting (0 when it ran at once, or when
    /// the agent never waited). Read by body tools for the
    /// `queued_ms` audit field (§9).
    pub fn last_queue_wait_ms_for(&self, agent_id: &str) -> u64 {
        self.inner
            .last_queued
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(agent_id)
            .copied()
            .unwrap_or(0)
    }

    /// The queue/timeout configuration this actor runs with.
    pub fn queue_config(&self) -> ActorConfig {
        self.config
    }

    /// Current body mutation generation (F6-03): the count
    /// of non-screenshot actions dispatched so far.
    pub fn mutation_seq(&self) -> u64 {
        self.inner.mutation_seq.load(Ordering::SeqCst)
    }

    fn record_queued(&self, agent_id: &str, queued_ms: u64) {
        self.inner
            .last_queued
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(agent_id.to_string(), queued_ms);
    }

    /// Execute one action, bounded by the per-action timeout.
    /// A tripped stop flag refuses before the body is touched.
    /// A timeout or a dead thread reports `UnknownOutcome`:
    /// the action may have executed, and must not be retried.
    /// Single-agent entry: attributed to the default agent.
    pub fn act(&self, action: Action) -> Result<ActOutcome, BodyError> {
        self.act_inner(AGENT_DEFAULT, action, false, None)
    }

    /// Execute one action for `agent_id` (Phase 6, §9): the
    /// request joins the agent's FIFO queue, waits at most
    /// the queue SLA for `Started`, then at most the action
    /// timeout for the outcome. A queue expiry reports
    /// `QueueExpired` and does NOT break the actor; an
    /// execution timeout reports `UnknownOutcome` and does.
    pub fn act_for(&self, agent_id: &str, action: Action) -> Result<ActOutcome, BodyError> {
        self.act_inner(agent_id, action, true, None)
    }

    /// Like [`BodyActor::act_for`], but the action carries
    /// the approval-time mutation generation (F6-03): the
    /// loop refuses it at dequeue time when the generation
    /// has moved, before `Started` — nothing executed. Body
    /// tools use this when a snapshot was consumed by
    /// re-verification.
    pub fn act_for_checked(
        &self,
        agent_id: &str,
        action: Action,
        expected_seq: u64,
    ) -> Result<ActOutcome, BodyError> {
        self.act_inner(agent_id, action, true, Some(expected_seq))
    }

    fn act_inner(
        &self,
        agent_id: &str,
        action: Action,
        phased: bool,
        expected_seq: Option<u64>,
    ) -> Result<ActOutcome, BodyError> {
        self.check_entry(agent_id)?;
        let (reply_tx, reply_rx) = channel::<RequestReply>();
        let enqueued_at = Instant::now();
        let queue_bound = if phased {
            self.config.queue_sla
        } else {
            self.timeout
        };
        if self
            .inner
            .tx
            .send(Request {
                agent_id: agent_id.to_string(),
                kind: RequestKind::Act(action),
                expected_seq,
                enqueued_at,
                deadline: enqueued_at + queue_bound,
                reply: reply_tx,
            })
            .is_err()
        {
            // The thread is gone BEFORE receiving: the action
            // never started, so this is a known non-execution,
            // not an unknown outcome.
            self.mark_broken();
            return Err(BodyError::Backend("body actor thread is gone".to_string()));
        }
        match self.wait_reply(&reply_rx, agent_id, enqueued_at + queue_bound, phased) {
            WaitResult::Reply(RequestReply::Action(outcome)) => outcome,
            WaitResult::Reply(RequestReply::ProbeTarget(_))
            | WaitResult::Reply(RequestReply::Started { .. }) => {
                self.mark_broken();
                Err(BodyError::Backend(
                    "body actor replied with the wrong kind; actor marked broken".to_string(),
                ))
            }
            WaitResult::TimedOut { started } => {
                if phased && !started {
                    // Never started: nothing executed, the
                    // actor is healthy, no UnknownOutcome.
                    Err(BodyError::QueueExpired {
                        agent_id: agent_id.to_string(),
                        queued_ms: enqueued_at.elapsed().as_millis() as u64,
                        sla_secs: self.config.queue_sla.as_secs(),
                    })
                } else {
                    self.mark_broken();
                    Err(BodyError::UnknownOutcome {
                        reason: format!(
                            "body action timed out after {:?}; it may or may not have executed, and the actor is marked broken",
                            self.timeout
                        ),
                    })
                }
            }
            WaitResult::ThreadDead => {
                self.mark_broken();
                Err(BodyError::UnknownOutcome {
                    reason: "body actor thread died while executing; the action may or may not have executed".to_string(),
                })
            }
            WaitResult::Cancelled => Err(BodyError::Cancelled {
                agent_id: agent_id.to_string(),
            }),
        }
    }

    /// Probe the target at (`x`, `y`): the window identity and
    /// re-verify snapshot, taken in the actor's serialized
    /// order. A tripped stop or a broken actor fails with a
    /// backend error (a probe is a read: nothing may have
    /// executed, so no `UnknownOutcome`).
    ///
    /// A probe TIMEOUT does NOT break the actor, unlike an
    /// action timeout: a probe changes no state, so the late
    /// reply is dropped harmlessly and the loop keeps serving
    /// (the next action gets its own verdict). Only a dead
    /// thread breaks the actor on the probe path.
    pub fn probe_target(&self, x: i32, y: i32) -> Result<TargetProbe, BodyError> {
        self.probe_target_with_timeout(x, y, self.timeout)
    }

    /// Like [`BodyActor::probe_target`], with an explicit wait
    /// bound. The approval card uses a short bound (a snapshot
    /// must never delay the card): on timeout the caller simply
    /// proceeds without a snapshot.
    pub fn probe_target_with_timeout(
        &self,
        x: i32,
        y: i32,
        timeout: Duration,
    ) -> Result<TargetProbe, BodyError> {
        self.probe_inner(AGENT_DEFAULT, x, y, timeout)
    }

    /// Probe the target for `agent_id` (Phase 6, §9): the
    /// probe joins the agent's queue with the 5s probe SLA.
    pub fn probe_target_for(
        &self,
        agent_id: &str,
        x: i32,
        y: i32,
    ) -> Result<TargetProbe, BodyError> {
        self.probe_inner(agent_id, x, y, self.timeout)
    }

    /// Like [`BodyActor::probe_target_for`], with an explicit
    /// wait bound.
    pub fn probe_target_with_timeout_for(
        &self,
        agent_id: &str,
        x: i32,
        y: i32,
        timeout: Duration,
    ) -> Result<TargetProbe, BodyError> {
        self.probe_inner(agent_id, x, y, timeout)
    }

    fn probe_inner(
        &self,
        agent_id: &str,
        x: i32,
        y: i32,
        timeout: Duration,
    ) -> Result<TargetProbe, BodyError> {
        self.check_entry(agent_id)?;
        let (reply_tx, reply_rx) = channel::<RequestReply>();
        let enqueued_at = Instant::now();
        if self
            .inner
            .tx
            .send(Request {
                agent_id: agent_id.to_string(),
                kind: RequestKind::ProbeTarget { x, y },
                expected_seq: None,
                enqueued_at,
                deadline: enqueued_at + PROBE_QUEUE_SLA,
                reply: reply_tx,
            })
            .is_err()
        {
            self.mark_broken();
            return Err(BodyError::Backend("body actor thread is gone".to_string()));
        }
        match self.wait_reply(&reply_rx, agent_id, enqueued_at + timeout, false) {
            WaitResult::Reply(RequestReply::ProbeTarget(result)) => result,
            WaitResult::Reply(RequestReply::Action(_))
            | WaitResult::Reply(RequestReply::Started { .. }) => {
                self.mark_broken();
                Err(BodyError::Backend(
                    "body actor replied with the wrong kind; actor marked broken".to_string(),
                ))
            }
            WaitResult::TimedOut { .. } => {
                // No mark_broken: the probe is a read, the
                // late reply is dropped, the loop keeps
                // serving. See `probe_target`.
                Err(BodyError::Backend(format!(
                    "body target probe timed out after {timeout:?}; no snapshot taken"
                )))
            }
            WaitResult::ThreadDead => {
                self.mark_broken();
                Err(BodyError::Backend(
                    "body actor thread died while probing".to_string(),
                ))
            }
            WaitResult::Cancelled => Err(BodyError::Cancelled {
                agent_id: agent_id.to_string(),
            }),
        }
    }

    /// Entry checks shared by actions and probes: stop latch,
    /// broken actor, and an already-cancelled agent (fail fast
    /// without queueing work for a dead agent).
    fn check_entry(&self, agent_id: &str) -> Result<(), BodyError> {
        if self.stop.is_tripped() {
            return Err(BodyError::Backend(
                "emergency stop is latched; the body refuses all actions".to_string(),
            ));
        }
        if self.is_broken() {
            return Err(BodyError::Backend(
                "body actor is broken (a previous action timed out); rebuild the actor".to_string(),
            ));
        }
        if self.is_agent_cancelled(agent_id) {
            return Err(BodyError::Cancelled {
                agent_id: agent_id.to_string(),
            });
        }
        Ok(())
    }

    /// Wait for a reply, consuming `Started` messages and
    /// recording their queue wait. Polls every 10ms so a
    /// pushed cancel lands within milliseconds (§10). When
    /// `phased`, a `Started` restarts the deadline with the
    /// action timeout (queue wait and execution wait are
    /// bounded separately, totalling at most queue SLA +
    /// action timeout); otherwise the single `deadline`
    /// bounds the whole wait. On expiry, one final `try_recv`
    /// closes the race with a `Started` already in flight.
    fn wait_reply(
        &self,
        rx: &Receiver<RequestReply>,
        agent_id: &str,
        mut deadline: Instant,
        phased: bool,
    ) -> WaitResult {
        let mut started = false;
        loop {
            if Instant::now() >= deadline {
                match rx.try_recv() {
                    Ok(RequestReply::Started { queued_ms }) => {
                        self.record_queued(agent_id, queued_ms);
                        if phased {
                            started = true;
                            deadline = Instant::now() + self.config.action_timeout;
                            continue;
                        }
                        return WaitResult::TimedOut { started: true };
                    }
                    Ok(reply) => return WaitResult::Reply(reply),
                    Err(_) => return WaitResult::TimedOut { started },
                }
            }
            match rx.recv_timeout(Duration::from_millis(10)) {
                Ok(RequestReply::Started { queued_ms }) => {
                    self.record_queued(agent_id, queued_ms);
                    started = true;
                    if phased {
                        deadline = Instant::now() + self.config.action_timeout;
                    }
                }
                Ok(reply) => return WaitResult::Reply(reply),
                Err(RecvTimeoutError::Timeout) => {
                    if self.is_agent_cancelled(agent_id) {
                        return WaitResult::Cancelled;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return WaitResult::ThreadDead,
            }
        }
    }

    /// True once the actor is broken (timeout or dead thread).
    pub fn is_broken(&self) -> bool {
        *self.inner.broken.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The shared stop flag this actor consults.
    pub fn stop_flag(&self) -> &Arc<StopFlag> {
        &self.stop
    }

    fn mark_broken(&self) {
        *self.inner.broken.lock().unwrap_or_else(|p| p.into_inner()) = true;
    }
}

impl Clone for BodyActor {
    fn clone(&self) -> BodyActor {
        BodyActor {
            inner: Arc::clone(&self.inner),
            stop: Arc::clone(&self.stop),
            timeout: self.timeout,
            config: self.config,
        }
    }
}

/// Audit one queue denial through the sink, best-effort: the
/// denial itself is the safety property, and a failing audit
/// write must not kill the actor thread. Turn `ui-queue`,
/// attributed to the denied agent (§9).
fn audit_queue_deny(
    audit: &Option<Arc<AuditLog>>,
    agent_id: &str,
    reason: &str,
    queued_ms: u64,
    sla_secs: u64,
) {
    if let Some(audit) = audit {
        let _ = audit.log_for(
            agent_id,
            "ui-queue",
            "decision",
            None,
            Some("deny"),
            json!({
                "reason": reason,
                "queued_ms": queued_ms,
                "sla_secs": sla_secs,
            }),
        );
    }
}

/// Queue one request, or deny it at once when a cap binds
/// (§9). Caps are enforced HERE, inside the loop — never by
/// the sender — so the counts are exact.
fn push_or_deny(
    request: Request,
    queues: &mut HashMap<String, VecDeque<Request>>,
    order: &mut Vec<String>,
    config: &ActorConfig,
    audit: &Option<Arc<AuditLog>>,
) {
    let total: usize = queues.values().map(VecDeque::len).sum();
    let per_agent = queues.get(&request.agent_id).map_or(0, VecDeque::len);
    let over = if total >= config.max_queued_total {
        Some((total, config.max_queued_total))
    } else if per_agent >= config.max_queued_per_agent {
        Some((per_agent, config.max_queued_per_agent))
    } else {
        None
    };
    if let Some((queued, max)) = over {
        let agent_id = request.agent_id.clone();
        let queued_ms = request.queued_ms();
        let sla_secs = request.sla_secs();
        let reply = deny_reply(
            &request.kind,
            BodyError::QueueFull {
                agent_id: agent_id.clone(),
                queued,
                max,
            },
        );
        let _ = request.reply.send(reply);
        audit_queue_deny(audit, &agent_id, "body_queue_full", queued_ms, sla_secs);
        return;
    }
    if !queues.contains_key(&request.agent_id) {
        order.push(request.agent_id.clone());
    }
    queues
        .entry(request.agent_id.clone())
        .or_default()
        .push_back(request);
}

/// Drop expired and cancelled-agent requests from every
/// queue (§9): expiry is checked here (reap) and again at
/// dequeue, so nothing runs late even if it expires between
/// the two. Expiry is audited; cancellation is not (the
/// coordinator audits the cancel itself, once).
fn reap(
    queues: &mut HashMap<String, VecDeque<Request>>,
    cancelled: &Mutex<HashSet<String>>,
    audit: &Option<Arc<AuditLog>>,
) {
    let cancelled = cancelled.lock().unwrap_or_else(|p| p.into_inner());
    let now = Instant::now();
    for queue in queues.values_mut() {
        let mut kept = VecDeque::with_capacity(queue.len());
        while let Some(request) = queue.pop_front() {
            if cancelled.contains(&request.agent_id) {
                let agent_id = request.agent_id.clone();
                let reply = deny_reply(&request.kind, BodyError::Cancelled { agent_id });
                let _ = request.reply.send(reply);
            } else if now >= request.deadline {
                let queued_ms = request.queued_ms();
                let sla_secs = request.sla_secs();
                let agent_id = request.agent_id.clone();
                let reply = deny_reply(
                    &request.kind,
                    BodyError::QueueExpired {
                        agent_id: agent_id.clone(),
                        queued_ms,
                        sla_secs,
                    },
                );
                let _ = request.reply.send(reply);
                audit_queue_deny(audit, &agent_id, "queue_sla_expired", queued_ms, sla_secs);
            } else {
                kept.push_back(request);
            }
        }
        *queue = kept;
    }
}

/// Round-robin pick (§2): one request from the next agent
/// with a non-empty queue, starting after the agent served
/// last. No priorities; one action per agent per round.
/// The last-served agent is remembered BY NAME, not by
/// index: the agent order grows as new agents appear, and
/// an index cursor would misfire after a join.
fn pick_next(
    queues: &mut HashMap<String, VecDeque<Request>>,
    order: &[String],
    last_served: &mut Option<String>,
) -> Option<Request> {
    if order.is_empty() {
        return None;
    }
    let start = last_served
        .as_ref()
        .and_then(|agent| order.iter().position(|other| other == agent))
        .map(|idx| idx + 1)
        .unwrap_or(0);
    for step in 0..order.len() {
        let idx = (start + step) % order.len();
        let agent = &order[idx];
        if let Some(queue) = queues.get_mut(agent) {
            if let Some(request) = queue.pop_front() {
                *last_served = Some(agent.clone());
                return Some(request);
            }
        }
    }
    None
}

// 8 args: the F6-03 shared mutation generation counter is the
// 8th. Same allow as fallback.rs/mcp.rs/skills.rs/brain.rs.
#[allow(clippy::too_many_arguments)]
fn actor_loop(
    mut body: Box<dyn Body>,
    rx: Receiver<Request>,
    stop: Arc<StopFlag>,
    broken: Arc<Mutex<bool>>,
    config: ActorConfig,
    cancelled: Arc<Mutex<HashSet<String>>>,
    audit: Option<Arc<AuditLog>>,
    mutation_seq: Arc<AtomicU64>,
) {
    let mut queues: HashMap<String, VecDeque<Request>> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut last_served: Option<String> = None;
    loop {
        // Block for work only when every queue is empty;
        // otherwise drain without blocking, so a steady
        // stream from one agent can never starve the rest.
        if queues.values().all(|q| q.is_empty()) {
            match rx.recv() {
                Ok(request) => push_or_deny(request, &mut queues, &mut order, &config, &audit),
                Err(_) => break,
            }
        }
        while let Ok(request) = rx.try_recv() {
            push_or_deny(request, &mut queues, &mut order, &config, &audit);
        }
        reap(&mut queues, &cancelled, &audit);
        let Some(request) = pick_next(&mut queues, &order, &mut last_served) else {
            continue;
        };
        // Dequeue-time checks: the second expiry checkpoint,
        // the stop latch, and the pushed cancel.
        if stop.is_tripped() {
            let reply = deny_reply(
                &request.kind,
                BodyError::Backend(
                    "emergency stop is latched; the body refuses all actions".to_string(),
                ),
            );
            let _ = request.reply.send(reply);
            continue;
        }
        if cancelled
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains(&request.agent_id)
        {
            let agent_id = request.agent_id.clone();
            let reply = deny_reply(&request.kind, BodyError::Cancelled { agent_id });
            let _ = request.reply.send(reply);
            continue;
        }
        if Instant::now() >= request.deadline {
            let queued_ms = request.queued_ms();
            let sla_secs = request.sla_secs();
            let agent_id = request.agent_id.clone();
            let reply = deny_reply(
                &request.kind,
                BodyError::QueueExpired {
                    agent_id: agent_id.clone(),
                    queued_ms,
                    sla_secs,
                },
            );
            let _ = request.reply.send(reply);
            audit_queue_deny(&audit, &agent_id, "queue_sla_expired", queued_ms, sla_secs);
            continue;
        }
        // Mutation-generation check (F6-03): an action
        // carrying the approval-time generation is refused
        // when any non-screenshot action ran since the
        // approval snapshot was taken — another agent may
        // have moved the world. Refused BEFORE Started:
        // nothing executed, and the caller sees a
        // recoverable refusal, not an unknown outcome.
        if let (RequestKind::Act(_), Some(expected)) = (&request.kind, request.expected_seq) {
            let current = mutation_seq.load(Ordering::SeqCst);
            if current != expected {
                let reply = deny_reply(
                    &request.kind,
                    BodyError::TargetChangedSinceApproval {
                        snapshot_seq: expected,
                        current_seq: current,
                    },
                );
                let _ = request.reply.send(reply);
                continue;
            }
        }
        // Started BEFORE execution: a failed send means the
        // caller went away — never execute work nobody waits
        // for (§9, anti-late-run).
        let queued_ms = request.queued_ms();
        if request
            .reply
            .send(RequestReply::Started { queued_ms })
            .is_err()
        {
            continue;
        }
        // A panicking body must not kill the loop silently — and
        // must not keep serving either: the body's state after a
        // panic is unknown. Catch the panic, report it as
        // `UnknownOutcome` (the action may have partially
        // executed), mark the actor broken, and end the loop.
        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match request.kind {
                RequestKind::Act(ref action) => RequestReply::Action(body.act(action)),
                RequestKind::ProbeTarget { x, y } => {
                    let identity = body.window_at(x, y);
                    let mut snapshot = body.snapshot_target(x, y);
                    // Stamp the generation the probe observed:
                    // the loop is serialized, so no action can
                    // run between the snapshot and this read.
                    if let Some(snap) = snapshot.as_mut() {
                        snap.mutation_seq = mutation_seq.load(Ordering::SeqCst);
                    }
                    RequestReply::ProbeTarget(Ok(TargetProbe { identity, snapshot }))
                }
            }));
        match outcome {
            Ok(reply) => {
                // The generation moves on every dispatched
                // non-screenshot action, Ok or Err: a refused
                // action may have partially executed, so it
                // still invalidates approvals taken before it.
                // Screenshots and probes never bump it.
                if let RequestKind::Act(action) = &request.kind {
                    if !matches!(action, Action::Screenshot) {
                        mutation_seq.fetch_add(1, Ordering::SeqCst);
                    }
                }
                if request.reply.send(reply).is_err() {
                    // The requester went away (timed out): the
                    // loop keeps running — the NEXT request will
                    // still be served if the caller builds a
                    // fresh actor... but this actor is marked
                    // broken on the caller side, so in practice
                    // the loop idles until the channel closes.
                }
            }
            Err(_) => {
                *broken.lock().unwrap_or_else(|p| p.into_inner()) = true;
                let reply = match request.kind {
                    RequestKind::Act(_) => RequestReply::Action(Err(BodyError::UnknownOutcome {
                        reason: "body panicked while executing the action; the action may have partially executed".to_string(),
                    })),
                    RequestKind::ProbeTarget { .. } => {
                        RequestReply::ProbeTarget(Err(BodyError::Backend(
                            "body panicked while probing the target".to_string(),
                        )))
                    }
                };
                let _ = request.reply.send(reply);
                break;
            }
        }
    }
}

/// Bridge a [`StopFlag`] to a [`CancelToken`]: a background
/// thread polls the flag and cancels the token when the flag
/// trips, so a body-level stop (global hotkey, self-exclusion
/// trip) aborts the brain turn that is driving the body.
///
/// The bridge polls every 50ms — cheap, and it keeps the two
/// crates decoupled (mimint-body knows nothing about
/// cancellation tokens). The thread exits when the flag trips
/// (its job is done — it cancels the token first) or when the
/// returned handle is dropped (the handle requests exit and
/// joins the thread).
pub mod bridge {
    use mimint_body::StopFlag;
    use mimint_core::CancelToken;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    /// A running stop bridge. Dropping it requests the bridge
    /// thread to exit (it notices within one poll interval).
    pub struct StopBridge {
        exit: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Drop for StopBridge {
        fn drop(&mut self) {
            self.exit.store(true, Ordering::SeqCst);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    /// Start bridging `flag` → `token`. When the flag trips,
    /// the token is cancelled. Returns the bridge handle: keep
    /// it alive for as long as the turn runs.
    pub fn stop_bridge(flag: Arc<StopFlag>, token: CancelToken) -> StopBridge {
        let exit = Arc::new(AtomicBool::new(false));
        let thread_exit = Arc::clone(&exit);
        let thread = std::thread::spawn(move || {
            while !thread_exit.load(Ordering::SeqCst) {
                if flag.is_tripped() {
                    token.cancel();
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        StopBridge {
            exit,
            thread: Some(thread),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mimint_body::conformance::TargetOracle;
    use mimint_body::{GuardedBody, MockBody, RateLimits};

    fn actor_with_mock() -> BodyActor {
        BodyActor::spawn(Box::new(MockBody::new()), StopFlag::new())
    }

    #[test]
    fn actor_executes_actions_and_returns_frames() {
        let actor = actor_with_mock();
        let outcome = actor.act(Action::Screenshot).unwrap();
        assert!(matches!(outcome, ActOutcome::Frame(_)));
        let outcome = actor.act(Action::Click { x: 10, y: 10 }).unwrap();
        assert!(matches!(outcome, ActOutcome::Done));
    }

    #[test]
    fn actor_reports_body_errors() {
        let actor = actor_with_mock();
        let err = actor.act(Action::Click { x: 9999, y: 9999 }).unwrap_err();
        assert_eq!(err.code(), "out_of_bounds");
    }

    #[test]
    fn tripped_stop_refuses_before_the_body() {
        let stop = StopFlag::new();
        let body = MockBody::new();
        let mut oracle = body.oracle();
        let actor = BodyActor::spawn(Box::new(body), Arc::clone(&stop));
        stop.trip();
        let err = actor.act(Action::Click { x: 10, y: 10 }).unwrap_err();
        assert!(err.message().contains("emergency stop"));
        assert!(oracle.received_clicks().is_empty());
    }

    #[test]
    fn mutation_seq_counts_mutations_and_stamps_probes() {
        let body = MockBody::new();
        let actor = BodyActor::spawn(Box::new(body), StopFlag::new());
        assert_eq!(actor.mutation_seq(), 0);
        let probe = actor.probe_target(10, 10).unwrap();
        assert_eq!(probe.snapshot.unwrap().mutation_seq, 0);
        // Screenshots and probes never bump the generation.
        assert!(actor.act(Action::Screenshot).is_ok());
        assert_eq!(actor.mutation_seq(), 0);
        let _ = actor.probe_target(10, 10).unwrap();
        assert_eq!(actor.mutation_seq(), 0);
        // A dispatched click bumps it; the next probe is
        // stamped with the new generation.
        assert!(actor.act(Action::Click { x: 10, y: 10 }).is_ok());
        assert_eq!(actor.mutation_seq(), 1);
        let probe = actor.probe_target(10, 10).unwrap();
        assert_eq!(probe.snapshot.unwrap().mutation_seq, 1);
        // Even a refused action bumps it (fail-closed: it
        // may have partially executed).
        let _ = actor.act(Action::Click { x: 9999, y: 9999 });
        assert_eq!(actor.mutation_seq(), 2);
    }

    #[test]
    fn act_for_checked_refuses_a_stale_generation_before_starting() {
        let body = MockBody::new();
        let mut oracle = body.oracle();
        let actor = BodyActor::spawn(Box::new(body), StopFlag::new());
        let probe = actor.probe_target(10, 10).unwrap();
        let seq = probe.snapshot.unwrap().mutation_seq;
        assert!(actor.act(Action::Click { x: 20, y: 20 }).is_ok());
        let err = actor
            .act_for_checked("agent-001", Action::Click { x: 10, y: 10 }, seq)
            .unwrap_err();
        assert_eq!(err.code(), "target_changed_since_approval");
        assert!(
            err.message().contains("mutation 0 -> 1"),
            "{}",
            err.message()
        );
        // The refused click never reached the body.
        assert_eq!(oracle.received_clicks(), vec![(20, 20)]);
        // The current generation dispatches fine.
        assert!(actor
            .act_for_checked("agent-001", Action::Click { x: 10, y: 10 }, 1)
            .is_ok());
    }

    #[test]
    fn guarded_body_inside_the_actor_enforces_its_limits() {
        let inner = MockBody::new();
        let mut guarded = GuardedBody::new(inner, RateLimits::default());
        guarded.set_excluded_rects(vec![mimint_body::ScreenRect {
            x: 0,
            y: 0,
            width: 40,
            height: 40,
        }]);
        let actor = BodyActor::spawn(Box::new(guarded), StopFlag::new());
        let err = actor.act(Action::Click { x: 10, y: 10 }).unwrap_err();
        assert_eq!(err.code(), "forbidden_target");
        assert!(actor.act(Action::Click { x: 100, y: 100 }).is_ok());
    }

    #[test]
    fn timeout_marks_the_actor_broken_and_fails_fast() {
        struct SleepingBody;
        impl Body for SleepingBody {
            fn name(&self) -> &'static str {
                "sleeping"
            }
            fn display_size(&mut self) -> Result<(u32, u32), BodyError> {
                Ok((800, 600))
            }
            fn screenshot(&mut self) -> Result<mimint_body::Framebuffer, BodyError> {
                std::thread::sleep(Duration::from_secs(30));
                Ok(mimint_body::Framebuffer::new(800, 600, vec![0u8; 800 * 600 * 4]).unwrap())
            }
            fn click(&mut self, _x: i32, _y: i32) -> Result<(), BodyError> {
                Ok(())
            }
            fn mouse_move(&mut self, _x: i32, _y: i32) -> Result<(), BodyError> {
                Ok(())
            }
            fn type_text(&mut self, _text: &str) -> Result<(), BodyError> {
                Ok(())
            }
            fn key(&mut self, _key: mimint_body::Key) -> Result<(), BodyError> {
                Ok(())
            }
            fn permissions(&mut self) -> mimint_body::Permissions {
                mimint_body::Permissions {
                    screen_capture: mimint_body::PermissionState::Granted,
                    input_control: mimint_body::PermissionState::Granted,
                }
            }
        }
        let actor = BodyActor::spawn_with_timeout(
            Box::new(SleepingBody),
            StopFlag::new(),
            Duration::from_millis(100),
        );
        let err = actor.act(Action::Screenshot).unwrap_err();
        assert_eq!(err.code(), "unknown_outcome");
        assert!(err.message().contains("timed out"), "{}", err.message());
        assert!(actor.is_broken());
        // Fail-fast: the second call returns immediately.
        let start = std::time::Instant::now();
        let err = actor.act(Action::Click { x: 1, y: 1 }).unwrap_err();
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(err.message().contains("broken"));
    }

    #[test]
    fn hanging_body_reports_unknown_outcome_and_breaks_the_actor() {
        use mimint_body::Fault;
        let body = MockBody::with_fault(Fault::Hang);
        let actor = BodyActor::spawn_with_timeout(
            Box::new(body),
            StopFlag::new(),
            Duration::from_millis(100),
        );
        let err = actor.act(Action::Click { x: 10, y: 10 }).unwrap_err();
        assert_eq!(err.code(), "unknown_outcome");
        assert!(actor.is_broken());
    }

    #[test]
    fn panicking_body_reports_unknown_outcome_and_breaks_the_actor() {
        struct PanicBody;
        impl Body for PanicBody {
            fn name(&self) -> &'static str {
                "panic"
            }
            fn display_size(&mut self) -> Result<(u32, u32), BodyError> {
                Ok((800, 600))
            }
            fn screenshot(&mut self) -> Result<mimint_body::Framebuffer, BodyError> {
                Ok(mimint_body::Framebuffer::new(2, 2, vec![0u8; 16]).unwrap())
            }
            fn click(&mut self, _x: i32, _y: i32) -> Result<(), BodyError> {
                panic!("injected body panic");
            }
            fn mouse_move(&mut self, _x: i32, _y: i32) -> Result<(), BodyError> {
                Ok(())
            }
            fn type_text(&mut self, _text: &str) -> Result<(), BodyError> {
                Ok(())
            }
            fn key(&mut self, _key: mimint_body::Key) -> Result<(), BodyError> {
                Ok(())
            }
            fn permissions(&mut self) -> mimint_body::Permissions {
                mimint_body::Permissions {
                    screen_capture: mimint_body::PermissionState::Granted,
                    input_control: mimint_body::PermissionState::Granted,
                }
            }
        }
        let actor = BodyActor::spawn(Box::new(PanicBody), StopFlag::new());
        let err = actor.act(Action::Click { x: 10, y: 10 }).unwrap_err();
        assert_eq!(err.code(), "unknown_outcome");
        assert!(err.message().contains("panicked"), "{}", err.message());
        assert!(actor.is_broken(), "a panic breaks the actor");
        // Fail-fast afterwards: the loop ended, no more work.
        let err = actor.act(Action::Click { x: 10, y: 10 }).unwrap_err();
        assert!(err.message().contains("broken"));
    }

    #[test]
    fn probe_target_returns_identity_and_snapshot_from_the_mock() {
        use mimint_body::{MockWindow, ScreenRect};
        let body = MockBody::new();
        body.set_windows(vec![MockWindow::new(
            7,
            ScreenRect::new(100, 100, 200, 200),
        )
        .with_class("editor")
        .with_fill((200, 40, 40))]);
        let actor = BodyActor::spawn(Box::new(body), StopFlag::new());
        let probe = actor.probe_target(150, 150).unwrap();
        let identity = probe.identity.unwrap();
        assert_eq!(identity.window_id, Some(7));
        assert_eq!(identity.class.as_deref(), Some("editor"));
        let snapshot = probe.snapshot.unwrap();
        assert_eq!(snapshot.window_id, Some(7));
        // Outside every window: no identity, but the snapshot
        // still describes the pixels under the point.
        let probe = actor.probe_target(10, 10).unwrap();
        assert!(probe.identity.is_none());
        assert!(probe.snapshot.is_some());
    }

    #[test]
    fn probe_target_on_a_body_without_windows_reports_none() {
        let actor = actor_with_mock();
        let probe = actor.probe_target(10, 10).unwrap();
        assert!(probe.identity.is_none(), "no windows scripted");
        assert!(probe.snapshot.is_some(), "the mock always snapshots");
    }

    #[test]
    fn probe_timeout_does_not_break_the_actor() {
        use mimint_body::{Framebuffer, Key, Permissions};
        use std::sync::atomic::{AtomicBool, Ordering};
        /// Body whose first `window_at` sleeps, then answers.
        struct SlowProbeBody {
            slept: AtomicBool,
        }
        impl mimint_body::Body for SlowProbeBody {
            fn name(&self) -> &'static str {
                "slow-probe"
            }
            fn display_size(&mut self) -> Result<(u32, u32), mimint_body::BodyError> {
                Ok((800, 600))
            }
            fn screenshot(&mut self) -> Result<Framebuffer, mimint_body::BodyError> {
                Framebuffer::new(800, 600, vec![0u8; 800 * 600 * 4])
            }
            fn click(&mut self, _x: i32, _y: i32) -> Result<(), mimint_body::BodyError> {
                Ok(())
            }
            fn mouse_move(&mut self, _x: i32, _y: i32) -> Result<(), mimint_body::BodyError> {
                Ok(())
            }
            fn type_text(&mut self, _text: &str) -> Result<(), mimint_body::BodyError> {
                Ok(())
            }
            fn key(&mut self, _key: Key) -> Result<(), mimint_body::BodyError> {
                Ok(())
            }
            fn permissions(&mut self) -> Permissions {
                Permissions {
                    screen_capture: mimint_body::PermissionState::Granted,
                    input_control: mimint_body::PermissionState::Granted,
                }
            }
            fn window_at(&mut self, _x: i32, _y: i32) -> Option<mimint_body::WindowIdentity> {
                if !self.slept.swap(true, Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(300));
                }
                None
            }
        }
        let actor = BodyActor::spawn(
            Box::new(SlowProbeBody {
                slept: AtomicBool::new(false),
            }),
            StopFlag::new(),
        );
        // The first probe times out on the short bound...
        let err = actor
            .probe_target_with_timeout(10, 10, Duration::from_millis(50))
            .unwrap_err();
        assert_eq!(err.code(), "backend");
        assert!(err.message().contains("timed out"), "{}", err.message());
        assert!(
            !actor.is_broken(),
            "a probe timeout must not break the actor"
        );
        // ...and the actor keeps serving: the next probe,
        // with a patient bound, succeeds.
        let probe = actor
            .probe_target_with_timeout(10, 10, Duration::from_secs(5))
            .unwrap();
        assert!(probe.identity.is_none());
    }

    #[test]
    fn stop_bridge_cancels_the_token_when_the_flag_trips() {
        let stop = StopFlag::new();
        let token = mimint_core::CancelToken::new();
        let _bridge = bridge::stop_bridge(Arc::clone(&stop), token.clone());
        assert!(!token.is_cancelled());
        stop.trip();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !token.is_cancelled() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(token.is_cancelled());
    }

    // ---- Phase 6 queue / SLA / cancel tests (§9, §10) ----

    /// A body that blocks its FIRST action on a gate and
    /// records every click's coordinates in order. Lets the
    /// test queue requests behind a stuck action, then
    /// release them all at once.
    struct GateBody {
        gate: std::sync::mpsc::Receiver<()>,
        entered: std::sync::mpsc::Sender<()>,
        clicks: Arc<Mutex<Vec<(i32, i32)>>>,
        gated: std::sync::atomic::AtomicBool,
    }

    impl GateBody {
        fn pair() -> (
            Self,
            std::sync::mpsc::Sender<()>,
            std::sync::mpsc::Receiver<()>,
        ) {
            let (release_tx, gate) = std::sync::mpsc::channel();
            let (entered_tx, entered_rx) = std::sync::mpsc::channel();
            (
                GateBody {
                    gate,
                    entered: entered_tx,
                    clicks: Arc::new(Mutex::new(Vec::new())),
                    gated: std::sync::atomic::AtomicBool::new(false),
                },
                release_tx,
                entered_rx,
            )
        }
    }

    impl mimint_body::Body for GateBody {
        fn name(&self) -> &'static str {
            "gate"
        }
        fn display_size(&mut self) -> Result<(u32, u32), BodyError> {
            Ok((800, 600))
        }
        fn screenshot(&mut self) -> Result<mimint_body::Framebuffer, BodyError> {
            mimint_body::Framebuffer::new(800, 600, vec![0u8; 800 * 600 * 4])
        }
        fn click(&mut self, x: i32, y: i32) -> Result<(), BodyError> {
            if !self.gated.swap(true, std::sync::atomic::Ordering::SeqCst) {
                let _ = self.entered.send(());
                let _ = self.gate.recv();
            }
            self.clicks.lock().unwrap().push((x, y));
            Ok(())
        }
        fn mouse_move(&mut self, _x: i32, _y: i32) -> Result<(), BodyError> {
            Ok(())
        }
        fn type_text(&mut self, _text: &str) -> Result<(), BodyError> {
            Ok(())
        }
        fn key(&mut self, _key: mimint_body::Key) -> Result<(), BodyError> {
            Ok(())
        }
        fn permissions(&mut self) -> mimint_body::Permissions {
            mimint_body::Permissions {
                screen_capture: mimint_body::PermissionState::Granted,
                input_control: mimint_body::PermissionState::Granted,
            }
        }
    }

    /// Poll the audit file until `needle` appears at least
    /// `count` times (the actor loop audits asynchronously:
    /// a denied caller can return before the loop's audit
    /// write lands). Returns the file contents.
    fn wait_for_audit(dir: &std::path::Path, needle: &str, count: usize) -> String {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let text = std::fs::read_to_string(dir.join("audit.jsonl")).unwrap_or_default();
            if text.matches(needle).count() >= count {
                return text;
            }
            assert!(
                Instant::now() < deadline,
                "audit never contained {needle:?}: {text}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Wait until `n` spawned request threads have signalled
    /// that they are about to issue their request, then allow
    /// a grace period for the sends themselves to land. A
    /// bare sleep is not enough: under load a spawned thread
    /// may not be scheduled for hundreds of milliseconds,
    /// and a request issued after the gate releases would
    /// execute instead of queueing.
    fn wait_started(rx: &std::sync::mpsc::Receiver<()>, n: usize) {
        for _ in 0..n {
            assert!(
                rx.recv_timeout(Duration::from_secs(5)).is_ok(),
                "request thread did not start in time"
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    #[test]
    fn round_robin_serves_one_action_per_agent_per_round() {
        let (body, release, entered) = GateBody::pair();
        let clicks = Arc::clone(&body.clicks);
        let actor = BodyActor::spawn(Box::new(body), StopFlag::new());
        // Four requests, one thread each: A1 blocks on the
        // gate; A2/B1/B2 queue behind it (channel order is
        // racy, but per-agent queues + round-robin make the
        // execution order deterministic: B1, A2, B2).
        let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
        let spawn_click =
            |actor: BodyActor, agent: &str, x: i32, started: std::sync::mpsc::Sender<()>| {
                let agent = agent.to_string();
                std::thread::spawn(move || {
                    let _ = started.send(());
                    actor.act_for(&agent, Action::Click { x, y: 0 })
                })
            };
        let t1 = {
            let (tx, _) = std::sync::mpsc::channel::<()>();
            spawn_click(actor.clone(), "agent-001", 1, tx)
        };
        assert!(entered.recv_timeout(Duration::from_secs(5)).is_ok());
        let t2 = spawn_click(actor.clone(), "agent-001", 2, started_tx.clone());
        let t3 = spawn_click(actor.clone(), "agent-002", 3, started_tx.clone());
        let t4 = spawn_click(actor.clone(), "agent-002", 4, started_tx.clone());
        // Prove all three queued requests were issued
        // before releasing the gate.
        wait_started(&started_rx, 3);
        let _ = release.send(());
        assert!(t1.join().unwrap().is_ok());
        assert!(t2.join().unwrap().is_ok());
        assert!(t3.join().unwrap().is_ok());
        assert!(t4.join().unwrap().is_ok());
        // A1 first (it held the gate), then strict
        // alternation between agents: B, A, B. Pure FIFO
        // would give A1, A2, B1, B2. Which of B's two
        // clicks lands first depends on thread scheduling
        // (channel arrival order), so the assertion is on
        // agent alternation, not on B's internal order.
        let order: Vec<i32> = clicks.lock().unwrap().iter().map(|c| c.0).collect();
        assert_eq!(order[0], 1);
        assert_eq!(order[2], 2);
        assert!(order[1] == 3 || order[1] == 4, "order: {order:?}");
        assert!(order[3] == 3 || order[3] == 4, "order: {order:?}");
        assert_ne!(order[1], order[3]);
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, vec![1, 2, 3, 4]);
    }

    #[test]
    fn queue_capacity_denies_overflow() {
        let dir = std::env::temp_dir().join(format!("mimint-actor-cap-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let audit = Arc::new(mimint_core::AuditLog::open(dir.join("audit.jsonl")).unwrap());
        let (body, release, entered) = GateBody::pair();
        let config = ActorConfig {
            max_queued_total: 64,
            max_queued_per_agent: 1,
            ..ActorConfig::default()
        };
        let actor = BodyActor::spawn_with_config_and_audit(
            Box::new(body),
            StopFlag::new(),
            config,
            Some(Arc::clone(&audit)),
        );
        let t1 = {
            let actor = actor.clone();
            std::thread::spawn(move || actor.act_for("agent-001", Action::Click { x: 1, y: 0 }))
        };
        assert!(entered.recv_timeout(Duration::from_secs(5)).is_ok());
        // Three more from the same agent: the per-agent cap
        // (1) admits one and denies two at drain time.
        let mut denied = 0;
        let mut handles = Vec::new();
        let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
        for x in [2, 3, 4] {
            let actor = actor.clone();
            let started = started_tx.clone();
            handles.push(std::thread::spawn(move || {
                let _ = started.send(());
                actor.act_for("agent-001", Action::Click { x, y: 0 })
            }));
        }
        wait_started(&started_rx, 3);
        let _ = release.send(());
        assert!(t1.join().unwrap().is_ok());
        for handle in handles {
            match handle.join().unwrap() {
                Ok(_) => {}
                Err(err) => {
                    assert_eq!(err.code(), "body_queue_full");
                    denied += 1;
                }
            }
        }
        assert_eq!(denied, 2, "two of three queued requests denied");
        assert!(!actor.is_broken());
        // The actor keeps serving after cap denials.
        assert!(actor
            .act_for("agent-001", Action::Click { x: 9, y: 0 })
            .is_ok());
        // Both denials were audited.
        let text = wait_for_audit(&dir, "body_queue_full", 2);
        assert_eq!(text.matches("body_queue_full").count(), 2, "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn queue_sla_expiry_denies_safely() {
        let dir = std::env::temp_dir().join(format!("mimint-actor-sla-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let audit = Arc::new(mimint_core::AuditLog::open(dir.join("audit.jsonl")).unwrap());
        let (body, release, entered) = GateBody::pair();
        let clicks = Arc::clone(&body.clicks);
        let config = ActorConfig {
            queue_sla: Duration::from_millis(100),
            ..ActorConfig::default()
        };
        let actor = BodyActor::spawn_with_config_and_audit(
            Box::new(body),
            StopFlag::new(),
            config,
            Some(Arc::clone(&audit)),
        );
        let t1 = {
            let actor = actor.clone();
            std::thread::spawn(move || actor.act_for("agent-001", Action::Click { x: 1, y: 0 }))
        };
        assert!(entered.recv_timeout(Duration::from_secs(5)).is_ok());
        let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
        let t2 = {
            let actor = actor.clone();
            std::thread::spawn(move || {
                let _ = started_tx.send(());
                actor.act_for("agent-002", Action::Click { x: 2, y: 0 })
            })
        };
        wait_started(&started_rx, 1);
        // Hold the gate past the SLA, then release: the
        // queued request must be denied, never executed.
        std::thread::sleep(Duration::from_millis(300));
        let _ = release.send(());
        assert!(t1.join().unwrap().is_ok());
        let err = t2.join().unwrap().unwrap_err();
        assert_eq!(err.code(), "queue_sla_expired");
        assert_eq!(clicks.lock().unwrap().len(), 1, "no late run");
        assert!(!actor.is_broken());
        let text = wait_for_audit(&dir, "queue_sla_expired", 1);
        assert!(text.contains("agent-002"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn level3_push_cancel_releases_blocked_caller() {
        let (body, release, entered) = GateBody::pair();
        let actor = BodyActor::spawn(Box::new(body), StopFlag::new());
        let handle = {
            let actor = actor.clone();
            std::thread::spawn(move || {
                let started = Instant::now();
                let result = actor.act_for("agent-002", Action::Click { x: 1, y: 0 });
                (started.elapsed(), result)
            })
        };
        assert!(entered.recv_timeout(Duration::from_secs(5)).is_ok());
        // The action is stuck in the gate; the cancel must
        // still land within milliseconds (poll, not wait).
        actor.cancel_agent("agent-002");
        let (elapsed, result) = handle.join().unwrap();
        assert!(elapsed < Duration::from_secs(1), "cancel took {elapsed:?}");
        let err = result.unwrap_err();
        assert_eq!(err.code(), "cancelled");
        // A queued request from the same agent is dropped
        // without executing; another agent is unaffected.
        let _ = release.send(());
        let err = actor
            .act_for("agent-002", Action::Click { x: 2, y: 0 })
            .unwrap_err();
        assert_eq!(err.code(), "cancelled");
        assert!(actor
            .act_for("agent-001", Action::Click { x: 3, y: 0 })
            .is_ok());
        assert!(!actor.is_broken());
    }

    #[test]
    fn queued_ms_recorded() {
        let (body, release, entered) = GateBody::pair();
        let actor = BodyActor::spawn(Box::new(body), StopFlag::new());
        let t1 = {
            let actor = actor.clone();
            std::thread::spawn(move || actor.act_for("agent-001", Action::Click { x: 1, y: 0 }))
        };
        assert!(entered.recv_timeout(Duration::from_secs(5)).is_ok());
        let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
        let t2 = {
            let actor = actor.clone();
            std::thread::spawn(move || {
                let _ = started_tx.send(());
                actor.act_for("agent-002", Action::Click { x: 2, y: 0 })
            })
        };
        wait_started(&started_rx, 1);
        std::thread::sleep(Duration::from_millis(300));
        let _ = release.send(());
        assert!(t1.join().unwrap().is_ok());
        assert!(t2.join().unwrap().is_ok());
        // agent-002 waited ~300ms in the queue; agent-001
        // ran at once.
        assert!(
            actor.last_queue_wait_ms_for("agent-002") >= 200,
            "queued_ms = {}",
            actor.last_queue_wait_ms_for("agent-002")
        );
        assert!(actor.last_queue_wait_ms_for("agent-001") < 200);
    }

    #[test]
    fn probe_sla_short() {
        let (body, release, entered) = GateBody::pair();
        let actor = BodyActor::spawn(Box::new(body), StopFlag::new());
        let t1 = {
            let actor = actor.clone();
            std::thread::spawn(move || actor.act_for("agent-001", Action::Click { x: 1, y: 0 }))
        };
        assert!(entered.recv_timeout(Duration::from_secs(5)).is_ok());
        let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
        let probe = {
            let actor = actor.clone();
            std::thread::spawn(move || {
                let _ = started_tx.send(());
                actor.probe_target_for("agent-002", 10, 10)
            })
        };
        wait_started(&started_rx, 1);
        // Hold the gate past the 5s probe SLA.
        std::thread::sleep(Duration::from_millis(5500));
        let _ = release.send(());
        assert!(t1.join().unwrap().is_ok());
        let err = probe.join().unwrap().unwrap_err();
        assert_eq!(err.code(), "queue_sla_expired");
        assert!(!actor.is_broken());
        assert!(actor.probe_target(10, 10).is_ok());
    }

    #[test]
    fn act_for_execution_timeout_is_unknown_outcome_and_breaks() {
        // Two-phase deadline: Started arrives at once, then
        // the action timeout (not queue SLA + action
        // timeout) bounds the execution wait.
        use mimint_body::Fault;
        let body = MockBody::with_fault(Fault::Hang);
        let config = ActorConfig {
            action_timeout: Duration::from_millis(100),
            queue_sla: Duration::from_secs(60),
            ..ActorConfig::default()
        };
        let actor = BodyActor::spawn_with_config(Box::new(body), StopFlag::new(), config);
        let started = Instant::now();
        let err = actor
            .act_for("agent-002", Action::Click { x: 10, y: 10 })
            .unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "execution timeout must not wait out the queue SLA: {:?}",
            started.elapsed()
        );
        assert_eq!(err.code(), "unknown_outcome");
        assert!(actor.is_broken());
    }

    #[test]
    fn actor_thread_death_reports_unknown_outcome_to_queued_callers() {
        // The body sleeps 200ms, then panics: the first
        // agent's action reports the panic, the second
        // agent's queued action observes the dead thread —
        // both as UnknownOutcome, never as success, and the
        // actor refuses everything afterwards.
        struct SleepThenPanicBody;
        impl Body for SleepThenPanicBody {
            fn name(&self) -> &'static str {
                "sleep-panic"
            }
            fn display_size(&mut self) -> Result<(u32, u32), BodyError> {
                Ok((800, 600))
            }
            fn screenshot(&mut self) -> Result<mimint_body::Framebuffer, BodyError> {
                Ok(mimint_body::Framebuffer::new(2, 2, vec![0u8; 16]).unwrap())
            }
            fn click(&mut self, _x: i32, _y: i32) -> Result<(), BodyError> {
                std::thread::sleep(Duration::from_millis(200));
                panic!("injected body panic after a delay");
            }
            fn mouse_move(&mut self, _x: i32, _y: i32) -> Result<(), BodyError> {
                Ok(())
            }
            fn type_text(&mut self, _text: &str) -> Result<(), BodyError> {
                Ok(())
            }
            fn key(&mut self, _key: mimint_body::Key) -> Result<(), BodyError> {
                Ok(())
            }
            fn permissions(&mut self) -> mimint_body::Permissions {
                mimint_body::Permissions {
                    screen_capture: mimint_body::PermissionState::Granted,
                    input_control: mimint_body::PermissionState::Granted,
                }
            }
        }
        let actor = BodyActor::spawn(Box::new(SleepThenPanicBody), StopFlag::new());
        let actor = Arc::new(actor);
        let first = {
            let actor = Arc::clone(&actor);
            std::thread::spawn(move || actor.act_for("agent-001", Action::Click { x: 10, y: 10 }))
        };
        // The second action queues while the first executes.
        std::thread::sleep(Duration::from_millis(10));
        let second = {
            let actor = Arc::clone(&actor);
            std::thread::spawn(move || actor.act_for("agent-002", Action::Click { x: 20, y: 20 }))
        };
        let first_err = first.join().unwrap().unwrap_err();
        assert_eq!(first_err.code(), "unknown_outcome");
        assert!(
            first_err.message().contains("panicked"),
            "{}",
            first_err.message()
        );
        let second_err = second.join().unwrap().unwrap_err();
        assert_eq!(second_err.code(), "unknown_outcome");
        assert!(
            second_err.message().contains("thread died"),
            "the queued caller must observe the dead thread: {}",
            second_err.message()
        );
        assert!(actor.is_broken());
        let err = actor
            .act_for("agent-001", Action::Click { x: 1, y: 1 })
            .unwrap_err();
        assert!(err.message().contains("broken"));
    }

    #[test]
    fn g6_1_round_robin_no_interleaving() {
        use mimint_body::{MockBody, MockEvent};
        // Two agents drive text input into the oracle
        // concurrently. The oracle stream must consist of
        // whole actions only (each agent's 3 chars contiguous,
        // never interleaved), served in strict alternation
        // while both agents have work pending.
        let body = MockBody::new();
        body.set_latency(7, 30, 30);
        let oracle = body.oracle();
        let actor = Arc::new(BodyActor::spawn(Box::new(body), StopFlag::new()));
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let drive =
            |actor: Arc<BodyActor>, barrier: Arc<std::sync::Barrier>, agent: &str, text: &str| {
                let agent = agent.to_string();
                let text = text.to_string();
                std::thread::spawn(move || {
                    barrier.wait();
                    for _ in 0..3 {
                        actor
                            .act_for(&agent, Action::TypeText { text: text.clone() })
                            .unwrap();
                    }
                })
            };
        let a = drive(Arc::clone(&actor), Arc::clone(&barrier), "agent-001", "AAA");
        let b = drive(Arc::clone(&actor), Arc::clone(&barrier), "agent-002", "BBB");
        a.join().unwrap();
        b.join().unwrap();
        let chars: Vec<char> = oracle
            .events()
            .iter()
            .map(|e| match e {
                MockEvent::Char(ch) => *ch,
                other => panic!("unexpected oracle event: {other:?}"),
            })
            .collect();
        assert_eq!(chars.len(), 18, "6 actions x 3 chars");
        // Whole actions: the stream is 6 groups of 3 identical
        // chars, with no interleaving inside a group.
        let groups: Vec<String> = chars.chunks(3).map(|c| c.iter().collect()).collect();
        assert_eq!(groups.len(), 6);
        for group in &groups {
            assert!(
                group == "AAA" || group == "BBB",
                "each group must be one whole action: {groups:?}"
            );
        }
        // Strict alternation while both agents have work: no
        // two consecutive groups from the same agent.
        for pair in groups.windows(2) {
            assert_ne!(pair[0], pair[1], "round-robin must alternate: {groups:?}");
        }
    }

    #[test]
    fn scheduler_starvation_bound() {
        // Agent A floods 50 actions; agent B sends 1 after
        // all of A's requests are issued. With N=2 agents,
        // B must be served after at most (N-1) = 1 of A's
        // actions: B's click lands at index 0 or 1 in the
        // execution order.
        let (body, release, entered) = GateBody::pair();
        let clicks = Arc::clone(&body.clicks);
        // Caps raised past the flood: this test measures
        // scheduling order, not capacity (caps are covered by
        // queue_capacity_denies_overflow).
        let config = ActorConfig {
            max_queued_total: 64,
            max_queued_per_agent: 64,
            ..ActorConfig::default()
        };
        let actor = Arc::new(BodyActor::spawn_with_config(
            Box::new(body),
            StopFlag::new(),
            config,
        ));
        let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
        let mut handles = Vec::new();
        for i in 0..50 {
            let actor = Arc::clone(&actor);
            let started = started_tx.clone();
            handles.push(std::thread::spawn(move || {
                let _ = started.send(());
                actor.act_for("agent-001", Action::Click { x: 100 + i, y: 0 })
            }));
        }
        wait_started(&started_rx, 50);
        // B's single action, issued after A's flood.
        let b_handle = {
            let actor = Arc::clone(&actor);
            let started = started_tx.clone();
            std::thread::spawn(move || {
                let _ = started.send(());
                actor.act_for("agent-002", Action::Click { x: 999, y: 0 })
            })
        };
        wait_started(&started_rx, 1);
        // Let every request land in the channel before the
        // gate opens; the first action may already hold the
        // gate (entered) or arrive right after.
        std::thread::sleep(Duration::from_millis(100));
        let _ = entered.try_recv();
        let _ = release.send(());
        for handle in handles {
            assert!(handle.join().unwrap().is_ok());
        }
        assert!(b_handle.join().unwrap().is_ok());
        let order: Vec<i32> = clicks.lock().unwrap().iter().map(|c| c.0).collect();
        assert_eq!(order.len(), 51);
        let b_index = order.iter().position(|x| *x == 999).expect("B's click ran");
        assert!(
            b_index <= 1,
            "B must be served after at most 1 A action, landed at index {b_index}: {order:?}"
        );
    }

    #[test]
    fn no_priority_bypass() {
        // Three agents queue actions while the first holds
        // the gate: the service order must rotate through all
        // three agents with no agent served twice in a row
        // while the others wait — no priority path exists.
        let (body, release, entered) = GateBody::pair();
        let clicks = Arc::clone(&body.clicks);
        let actor = Arc::new(BodyActor::spawn(Box::new(body), StopFlag::new()));
        let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
        let mut handles = Vec::new();
        for (agent, base) in [("agent-001", 100), ("agent-002", 200), ("agent-003", 300)] {
            for i in 0..3 {
                let actor = Arc::clone(&actor);
                let started = started_tx.clone();
                let agent = agent.to_string();
                handles.push(std::thread::spawn(move || {
                    let _ = started.send(());
                    actor.act_for(&agent, Action::Click { x: base + i, y: 0 })
                }));
            }
        }
        wait_started(&started_rx, 9);
        std::thread::sleep(Duration::from_millis(100));
        let _ = entered.try_recv();
        let _ = release.send(());
        for handle in handles {
            assert!(handle.join().unwrap().is_ok());
        }
        let order: Vec<i32> = clicks.lock().unwrap().iter().map(|c| c.0).collect();
        assert_eq!(order.len(), 9);
        let agent_of = |x: i32| x / 100;
        for pair in order.windows(2) {
            assert_ne!(
                agent_of(pair[0]),
                agent_of(pair[1]),
                "no agent may be served twice in a row: {order:?}"
            );
        }
        for agent in [1, 2, 3] {
            assert_eq!(
                order.iter().filter(|x| agent_of(**x) == agent).count(),
                3,
                "each agent served exactly 3 times: {order:?}"
            );
        }
    }

    #[test]
    fn queue_depth_backstop_reality() {
        use mimint_body::MockBody;
        // A sequential brain (one outstanding request per
        // agent: an action, then a probe, then the next
        // action) never needs queue depth: with the caps cut
        // to 1 per agent / 2 total, two sequential drivers
        // still complete every request. The 16/64 caps are
        // only reachable by flooding a blocked actor (see
        // queue_capacity_denies_overflow).
        let body = MockBody::new();
        body.set_latency(3, 20, 20);
        let config = ActorConfig {
            action_timeout: Duration::from_secs(5),
            queue_sla: Duration::from_secs(60),
            max_queued_total: 2,
            max_queued_per_agent: 1,
        };
        let actor = Arc::new(BodyActor::spawn_with_config(
            Box::new(body),
            StopFlag::new(),
            config,
        ));
        let drive = |actor: Arc<BodyActor>, agent: &str| {
            let agent = agent.to_string();
            std::thread::spawn(move || {
                for i in 0..5 {
                    actor
                        .act_for(&agent, Action::Click { x: 10 + i, y: 10 })
                        .unwrap();
                    actor.probe_target_for(&agent, 10, 10).unwrap();
                }
            })
        };
        let a = drive(Arc::clone(&actor), "agent-001");
        let b = drive(Arc::clone(&actor), "agent-002");
        a.join().unwrap();
        b.join().unwrap();
        assert!(!actor.is_broken());
    }
}
