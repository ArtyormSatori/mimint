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
use mimint_body::{StopFlag, TargetSnapshotSlot};
use mimint_control::{
    register_body_tools_configured, BodyActor, BodyStepVerifier, FrameStore, PrivacyFlag,
};
use mimint_core::audit::now_ms;
use mimint_core::{
    build_system_prompt, default_greeting, default_registry, default_soul_text, default_vault,
    soul_may_be_sent, url_is_loopback, AuditLog, BackendSpec, Brain, BrainProgress, CancelToken,
    CostCap, EgressPolicy, ExperienceStore, FallbackChain, Notebook, Profile, ProfileSet, Provider,
    Registry, RetryPolicy, SkillDef, SkillStore, SoulDoc, SoulStore, SOUL_FORMAT_VERSION,
};
use serde_json::json;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

/// Requests from the UI thread to the worker.
#[derive(Debug)]
pub enum WorkerRequest {
    /// Run one brain turn for this prompt.
    RunTurn { prompt: String },
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
    /// The brain entered a new phase (thinking, acting,
    /// verifying, tool failed). The UI shows the phase plus
    /// elapsed time, instead of a generic spinner.
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
    },
    /// The worker thread caught a panic inside a turn. The loop
    /// keeps serving; the UI shows the control error state.
    WorkerFault { error: String },
    /// The body's emergency-stop flag tripped (global hotkey, or
    /// an input attempt while an approval card waited). The stop
    /// slot is already cancelled; the UI shows the stopped state
    /// and the reason. The flag latches: only a restart clears
    /// the body stop.
    StopTripped { reason: String },
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
/// the stop slot.
pub struct BodyStack {
    pub actor: BodyActor,
    pub frames: Arc<FrameStore>,
    pub privacy: PrivacyFlag,
    pub stop_flag: Arc<StopFlag>,
}

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

    /// Switch to a named profile (sidebar). The worker resolves
    /// the name, rebuilds, and reports
    /// [`WorkerEvent::ProfileSwitched`].
    pub fn switch_profile(&self, name: String) {
        let _ = self.request_tx.send(WorkerRequest::SwitchProfile { name });
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
    ProfileRuntime {
        profile: profile.clone(),
        privacy_on,
        order,
        soul_prompt,
        may_send_user_data: may_send,
        greeting,
        skills,
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
        max_observation_age,
        body,
        approval_pending,
        privacy_on: initial_privacy_on,
        profiles,
        active_profile,
        soul_store,
        skill_store,
        experience,
    } = deps;
    // One snapshot slot shared by the approver (fills it at
    // request time) and the body tools (consume it at execution
    // time for the re-verify). Created unconditionally; without
    // a body it is never filled.
    let snapshot_slot = Arc::new(TargetSnapshotSlot::new());
    let mut registry = default_registry(Arc::clone(&notebook));
    if let Some(stack) = &body {
        register_body_tools_configured(
            &mut registry,
            stack.actor.clone(),
            Arc::clone(&stack.frames),
            stack.privacy.clone(),
            Arc::clone(&snapshot_slot),
            max_observation_age,
        );
    }
    let mut approver = UiApprover::new(approval_tx)
        .with_timeout(approval_timeout)
        .with_slots(Arc::clone(&stop_slot), Arc::clone(&turn_slot))
        .with_repaint(repaint)
        .with_max_observation_age(max_observation_age);
    if let Some(flag) = approval_pending {
        approver = approver.with_pending_flag(flag);
    }
    if let Some(stack) = &body {
        // Request-time target probe (F4-07): a short bound, so
        // a slow body never delays the card — a failed probe
        // just means no snapshot.
        let actor = stack.actor.clone();
        approver = approver.with_target_probe(
            Arc::new(move |x, y| {
                actor
                    .probe_target_with_timeout(x, y, Duration::from_secs(5))
                    .ok()
            }),
            Arc::clone(&snapshot_slot),
        );
        let frames = Arc::clone(&stack.frames);
        approver = approver.with_frame_age(Arc::new(move || frames.latest_age()));
    }
    // Stop bridge: when the body's stop flag trips (hotkey or
    // approval-pending trip), cancel the stop slot so the running
    // turn aborts, record the latch with its context, and tell
    // the UI. Fires once; the flag latches.
    let bridge_exit = Arc::new(AtomicBool::new(false));
    let bridge = body.as_ref().map(|stack| {
        let flag = Arc::clone(&stack.stop_flag);
        let slot = Arc::clone(&stop_slot);
        let tx = event_tx.clone();
        let exit = Arc::clone(&bridge_exit);
        let audit = Arc::clone(&audit);
        std::thread::spawn(move || {
            while !exit.load(Ordering::SeqCst) {
                if flag.is_tripped() {
                    slot.cancel_current();
                    // The latch is audited with its context:
                    // source, reason, and trip time (F4-16).
                    // Best-effort: a failed audit write must
                    // not block the stop itself.
                    let (source, reason, ts_ms) = match flag.trip_info() {
                        Some(info) => (info.source, info.reason, info.ts_ms),
                        None => ("unknown", "emergency stop tripped".to_string(), now_ms()),
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
                        }),
                    );
                    let _ = tx.send(WorkerEvent::StopTripped {
                        reason: format!("{source}: {reason}"),
                    });
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        })
    });
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
    let initial_profile = initial_profile.clone();
    let endpoints: Vec<&str> = chain
        .as_ref()
        .map(|built| built.provider_endpoints())
        .unwrap_or_default();
    let mut profile_runtime = compute_runtime(
        &initial_profile,
        initial_privacy_on,
        initial_profile.chain.clone(),
        &endpoints,
        soul_store.as_ref(),
        skill_store.as_ref(),
        &audit,
    );

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
                    body: body.as_ref(),
                    runtime: &profile_runtime,
                    experience: experience.as_ref(),
                };
                // A panic in a turn must not kill the worker or
                // leave the UI stuck on "thinking": catch it,
                // report it, keep serving.
                let event = match catch_unwind(AssertUnwindSafe(|| {
                    run_one_turn(&runtime, chain.as_ref(), &ctx, &prompt, &event_tx)
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
                // Privacy discipline (P4.6.6): the new mode takes
                // effect for the body tools immediately, and
                // tainted frames are purged even if the chain
                // rebuild fails — a failed rebuild must not leave
                // privacy-ON screenshots lying around.
                if let Some(stack) = &body {
                    stack.privacy.set(privacy_on);
                    stack.frames.purge_tainted();
                }
                let profile = profile_runtime.profile.clone();
                let event = rebuild_chain(
                    &mut chain,
                    &rebuild,
                    &audit,
                    &event_tx,
                    privacy_on,
                    &order,
                    profile.cost_cap,
                    &profile.name,
                );
                if matches!(&event, WorkerEvent::ChainRebuilt { error: None, .. }) {
                    let endpoints: Vec<&str> = chain
                        .as_ref()
                        .map(|built| built.provider_endpoints())
                        .unwrap_or_default();
                    profile_runtime = compute_runtime(
                        &profile,
                        privacy_on,
                        order,
                        &endpoints,
                        soul_store.as_ref(),
                        skill_store.as_ref(),
                        &audit,
                    );
                }
                if event_tx.send(event).is_err() {
                    break;
                }
            }
            WorkerRequest::SwitchProfile { name } => {
                let (profile, warning) = profiles.resolve_or_default(&name);
                let profile = profile.clone();
                if let Some(stack) = &body {
                    stack.privacy.set(profile.privacy);
                    stack.frames.purge_tainted();
                }
                let from = profile_runtime.profile.name.clone();
                let event = rebuild_chain(
                    &mut chain,
                    &rebuild,
                    &audit,
                    &event_tx,
                    profile.privacy,
                    &profile.chain,
                    profile.cost_cap,
                    &profile.name,
                );
                let switched = matches!(&event, WorkerEvent::ChainRebuilt { error: None, .. });
                if event_tx.send(event).is_err() {
                    break;
                }
                if switched {
                    let _ = audit.log(
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
                    let endpoints: Vec<&str> = chain
                        .as_ref()
                        .map(|built| built.provider_endpoints())
                        .unwrap_or_default();
                    profile_runtime = compute_runtime(
                        &profile,
                        profile.privacy,
                        profile.chain.clone(),
                        &endpoints,
                        soul_store.as_ref(),
                        skill_store.as_ref(),
                        &audit,
                    );
                    let event = WorkerEvent::ProfileSwitched {
                        name: profile_runtime.profile.name.clone(),
                        greeting: profile_runtime.greeting.clone(),
                        soul_sent: profile_runtime.may_send_user_data,
                        privacy_on: profile_runtime.privacy_on,
                        order: profile_runtime.order.clone(),
                        warning,
                    };
                    if event_tx.send(event).is_err() {
                        break;
                    }
                }
            }
        }
    }
    bridge_exit.store(true, Ordering::SeqCst);
    if let Some(thread) = bridge {
        let _ = thread.join();
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
    body: Option<&'a BodyStack>,
    runtime: &'a ProfileRuntime,
    experience: Option<&'a Arc<ExperienceStore>>,
}

fn run_one_turn(
    runtime: &tokio::runtime::Runtime,
    chain: Option<&FallbackChain>,
    ctx: &TurnCtx,
    prompt: &str,
    event_tx: &Sender<WorkerEvent>,
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
    // Phase 5 profile wiring: the profile name tags every audit
    // record; the SOUL prompt (when egress allows) replaces the
    // built-in prompt; promoted skills and experience retrieval
    // follow the same egress rule as SOUL.
    brain.set_profile(ctx.runtime.profile.name.clone());
    if let Some(prompt) = &ctx.runtime.soul_prompt {
        brain.set_system_prompt(prompt.clone());
    }
    brain.set_skills(ctx.runtime.skills.clone());
    if ctx.runtime.may_send_user_data {
        if let Some(store) = ctx.experience {
            brain.set_experience_store(Arc::clone(store));
        }
    }
    // Phase reports for the status line (F4-17): the hook only
    // sends on a channel, so it cannot block the turn.
    let progress_tx = event_tx.clone();
    brain.set_progress_hook(Arc::new(move |progress| {
        let _ = progress_tx.send(WorkerEvent::TurnProgress { progress });
    }));
    if let Some(stack) = ctx.body {
        // Privacy discipline (P4.6.6): tainted frames from
        // earlier turns never survive into a new turn.
        stack.frames.purge_tainted();
        brain.set_step_verifier(Arc::new(BodyStepVerifier::new(
            stack.actor.clone(),
            Arc::clone(&stack.frames),
        )));
    }
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

#[allow(clippy::too_many_arguments)]
fn rebuild_chain(
    chain: &mut Option<FallbackChain>,
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
        ApprovalDecision, BackendKind, BoxFuture, CallContext, ChatMessage, FunctionCall,
        MimintError, ProfileEntry, ToolCall,
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
            soul_store: None,
            skill_store: None,
            experience: None,
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
        // The brain reports Analyzing before the chain reports
        // its attempt; both are progress, in that order.
        match wait_event(&handle) {
            WorkerEvent::TurnProgress {
                progress: BrainProgress::Analyzing,
            } => {}
            other => panic!("expected TurnProgress Analyzing first, got: {other:?}"),
        }
        match wait_event(&handle) {
            WorkerEvent::TryingBackend { name } => assert_eq!(name, "fake"),
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
        };
        let handle = spawn_worker(deps);
        let _ = wait_event(&handle);
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
            } => {
                assert_eq!(name, "Làm việc");
                assert_eq!(greeting, "Chào làm việc");
                assert!(soul_sent);
                assert!(privacy_on);
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
}
