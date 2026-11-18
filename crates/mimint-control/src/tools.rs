//! Body actions as brain tools (P4.1, ACI).
//!
//! One [`Tool`] per body action, named `body_<action>`, with
//! the JSON Schema generated from the implementation: the
//! schemas come from [`mimint_body::action_specs`], the same
//! source the conformance suite pins. There is no second,
//! hand-written copy of the schemas — if the action set
//! changes, the tool surface changes with it, and the
//! spec-pin test in mimint-body fails loudly.
//!
//! Risk levels:
//! - `body_screenshot` is [`Risk::ReadOnly`]: it reads the
//!   screen and returns METADATA ONLY (frame id, dimensions,
//!   scale, hash). Pixels stay in the [`FrameStore`], never
//!   in the tool-result JSON. No approval is required, but
//!   the call is still audited and still cancel-checked.
//! - Every input action is [`Risk::Consequential`]: the guard
//!   executor requires approval (the UiApprover card in the
//!   desktop app) before the actor runs it, and the approval
//!   binds the exact arguments shown to the user.
//!
//! Tools are thin: argument parsing + actor dispatch + output
//! shaping. All policy (exclusion zones, rate limits, stop
//! latch, per-action timeout) lives in the [`GuardedBody`]
//! inside the actor and in the actor itself. Tools never
//! touch a body directly.
//!
//! Two pre-execution checks live here, because only this layer
//! sees both the observation store and the approval snapshot:
//! - **Observation TTL (F4-08):** an input action planned from
//!   a frame older than [`DEFAULT_MAX_OBSERVATION_AGE`] is
//!   refused with `stale_observation` until a fresh screenshot
//!   is taken. The default is 300s: local 3B model calls were
//!   measured at 175–500s under load, so a shorter default
//!   would refuse nearly every slow-model turn. The TTL is a
//!   backstop, layered with the re-verify check and the
//!   approval card's age warning — not the only freshness
//!   signal. No stored frame at all means no refusal (explicit
//!   coordinates, first action).
//! - **Re-verify (F4-07):** when the approver stored a target
//!   snapshot for this action's point, the tool takes a fresh
//!   snapshot through the actor and compares: a changed window
//!   or changed pixels refuse with `target_changed`, and the
//!   user must re-approve against the current screen.
//! - **Mutation generation (F6-03):** the re-verify check
//!   first compares the approval snapshot's generation
//!   against the actor's counter: any action by any agent
//!   since the approval refuses with
//!   `target_changed_since_approval`, even when the pixels
//!   under the point look unchanged. The surviving
//!   generation is then carried into the actor's dequeue
//!   check, closing the gap between re-verification and
//!   execution. Coordinate actions only — type/key actions
//!   carry no snapshot and get no generation check.

use crate::actor::BodyActor;
use crate::frames::{FrameMeta, FrameStore};
use crate::slots::AgentSnapshotSlots;
use mimint_body::{
    action_specs, ActOutcome, Action, ActionSpec, BodyError, Key, TargetSnapshotSlot,
};
use mimint_core::{MimintError, Risk, Tool, AGENT_DEFAULT};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Default maximum age of the observation an input action may
/// be planned from (F4-08). See the module docs for why 300s.
pub const DEFAULT_MAX_OBSERVATION_AGE: Duration = Duration::from_secs(300);

/// Shared Privacy Mode state: the screenshot tool reads it at
/// capture time to taint the stored frame. The worker owns the
/// flag and flips it from the UI toggle.
#[derive(Debug, Clone, Default)]
pub struct PrivacyFlag {
    on: Arc<Mutex<bool>>,
}

impl PrivacyFlag {
    pub fn new(on: bool) -> PrivacyFlag {
        PrivacyFlag {
            on: Arc::new(Mutex::new(on)),
        }
    }

    pub fn is_on(&self) -> bool {
        *self.on.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn set(&self, on: bool) {
        *self.on.lock().unwrap_or_else(|p| p.into_inner()) = on;
    }
}

/// One body action as a tool. The action name selects the
/// spec; arguments are parsed per action.
pub struct BodyTool {
    spec: &'static ActionSpec,
    actor: BodyActor,
    frames: Arc<FrameStore>,
    privacy: PrivacyFlag,
    /// Approval-time snapshot slot, shared with the approver.
    snapshot_slot: Arc<TargetSnapshotSlot>,
    /// Maximum observation age for input actions (F4-08).
    max_observation_age: Duration,
    /// The agent this tool belongs to (Phase 6, §3): its
    /// actions join its own actor queue, its snapshots live
    /// in its own slot, and its frames carry its id.
    agent_id: String,
    /// Queue wait of the last `execute`, read from the actor
    /// after every call (Ok and Err alike) for the
    /// `queued_ms` audit field (§9).
    queue_wait_ms: AtomicU64,
}

impl BodyTool {
    fn with_config(
        spec: &'static ActionSpec,
        actor: BodyActor,
        frames: Arc<FrameStore>,
        privacy: PrivacyFlag,
        snapshot_slot: Arc<TargetSnapshotSlot>,
        max_observation_age: Duration,
        agent_id: &str,
    ) -> BodyTool {
        BodyTool {
            spec,
            actor,
            frames,
            privacy,
            snapshot_slot,
            max_observation_age,
            agent_id: agent_id.to_string(),
            queue_wait_ms: AtomicU64::new(0),
        }
    }

    /// The tool name: `body_<action>`.
    pub fn tool_name(action: &str) -> String {
        format!("body_{action}")
    }

    /// The agent this tool acts for.
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// Queue wait of the last `execute` call, in
    /// milliseconds (0 when it ran at once).
    pub fn last_queue_wait_ms(&self) -> u64 {
        self.queue_wait_ms.load(Ordering::SeqCst)
    }
}

impl BodyTool {
    fn description(&self) -> &'static str {
        self.spec.description
    }

    fn parameters_schema(&self) -> Value {
        self.spec.params_schema.clone()
    }

    fn risk(&self) -> Risk {
        if self.spec.name == "screenshot" {
            Risk::ReadOnly
        } else {
            Risk::Consequential
        }
    }

    fn execute(&self, args: &Value) -> Result<Value, MimintError> {
        let action = parse_action(self.spec.name, args)?;
        let expected_seq = if !matches!(action, Action::Screenshot) {
            self.check_observation_fresh()?;
            self.reverify_target(&action)?
        } else {
            None
        };
        let outcome = match expected_seq {
            // A consumed snapshot binds the dispatch to its
            // generation: the actor refuses at dequeue time
            // when another agent acted in between.
            Some(seq) => self.actor.act_for_checked(&self.agent_id, action, seq),
            None => self.actor.act_for(&self.agent_id, action),
        };
        // Record the queue wait on BOTH paths: the guard
        // executor reads it for the execution record even
        // when the call failed (§9).
        self.queue_wait_ms.store(
            self.actor.last_queue_wait_ms_for(&self.agent_id),
            Ordering::SeqCst,
        );
        match outcome {
            Ok(ActOutcome::Frame(frame)) => {
                let meta = self
                    .frames
                    .store_for(&self.agent_id, frame, self.privacy.is_on());
                Ok(frame_meta_json(&meta))
            }
            Ok(ActOutcome::Done) => Ok(json!({"ok": true})),
            Err(err) => Err(body_err_to_tool(err)),
        }
    }

    /// Refuse input actions planned from a stale observation
    /// (F4-08) or from before a body rebuild (Phase 6, §4.1).
    /// Freshness is PER-AGENT (§3 row 8): the check reads the
    /// acting agent's own newest frame — another agent's
    /// fresh screenshot does not refresh this agent's
    /// observation, and another agent's stale frame does not
    /// block this agent. Screenshots are exempt (they refresh
    /// the observation); an agent with no stored frame is
    /// exempt (nothing stale) UNLESS a rebuild raised the
    /// floor — after a rebuild the first input action MUST
    /// follow a fresh screenshot by that same agent.
    fn check_observation_fresh(&self) -> Result<(), MimintError> {
        let floor = self.frames.floor();
        if floor > 0 {
            let fresh_enough = self
                .frames
                .latest_meta_for(&self.agent_id)
                .is_some_and(|meta| meta.generation >= floor);
            if !fresh_enough {
                return Err(MimintError::Tool(
                    "stale_observation: the body was rebuilt; take a fresh screenshot before acting"
                        .to_string(),
                ));
            }
        }
        if self
            .frames
            .observation_stale_for(&self.agent_id, self.max_observation_age)
        {
            let age_secs = self
                .frames
                .latest_age_for(&self.agent_id)
                .map(|age| age.as_secs())
                .unwrap_or(0);
            return Err(body_err_to_tool(BodyError::StaleObservation {
                age_secs,
                max_secs: self.max_observation_age.as_secs(),
            }));
        }
        Ok(())
    }

    /// Re-verify the target against the approval-time snapshot
    /// (F4-07 + F6-03). Only coordinate actions carry a
    /// snapshot, and only when the approver stored one for
    /// this exact point: no snapshot means no re-verification
    /// (gates, bodies without snapshot support), never a
    /// refusal. A fresh snapshot the body cannot produce
    /// (`None`) also proceeds. Returns the consumed
    /// snapshot's generation when one was consumed, so the
    /// dispatch can carry it into the actor's dequeue check;
    /// `None` when no snapshot applied.
    ///
    /// The generation is checked FIRST: any body mutation
    /// since the approval refuses with
    /// `target_changed_since_approval`, even when the pixels
    /// under the point look unchanged. Only a surviving
    /// generation reaches the pixel comparison, which refuses
    /// with `target_changed` on mismatch. The action never
    /// runs on either refusal.
    fn reverify_target(&self, action: &Action) -> Result<Option<u64>, MimintError> {
        let Some((x, y)) = action_point(action) else {
            return Ok(None);
        };
        let Some(expected) = self.snapshot_slot.take_for(x, y) else {
            return Ok(None);
        };
        let current = self.actor.mutation_seq();
        if current != expected.mutation_seq {
            return Err(body_err_to_tool(BodyError::TargetChangedSinceApproval {
                snapshot_seq: expected.mutation_seq,
                current_seq: current,
            }));
        }
        let fresh = match self.actor.probe_target_for(&self.agent_id, x, y) {
            Ok(probe) => probe.snapshot,
            Err(err) => return Err(body_err_to_tool(err)),
        };
        if let Some(fresh) = fresh {
            if let Some(reason) = expected.mismatch_reason(&fresh) {
                return Err(body_err_to_tool(BodyError::TargetChanged { reason }));
            }
        }
        Ok(Some(expected.mutation_seq))
    }
}

/// The approval point of a coordinate action: the point the
/// approver snapshots, and the point re-verified at execution.
/// Drags snapshot their FROM point (where the press lands).
/// Non-coordinate actions have no point.
fn action_point(action: &Action) -> Option<(i32, i32)> {
    match action {
        Action::Click { x, y }
        | Action::Move { x, y }
        | Action::Scroll { x, y, .. }
        | Action::DoubleClick { x, y }
        | Action::RightClick { x, y } => Some((*x, *y)),
        Action::Drag { from_x, from_y, .. } => Some((*from_x, *from_y)),
        _ => None,
    }
}

/// A [`BodyTool`] with its `body_<action>` name. The `Tool`
/// trait requires a `&'static str` name, so the prefixed names
/// live in a static table built once.
pub struct NamedBodyTool {
    name: &'static str,
    inner: BodyTool,
}

impl NamedBodyTool {
    fn new(name: &'static str, inner: BodyTool) -> NamedBodyTool {
        NamedBodyTool { name, inner }
    }
}

impl Tool for NamedBodyTool {
    fn name(&self) -> &'static str {
        self.name
    }

    fn description(&self) -> &'static str {
        self.inner.description()
    }

    fn parameters_schema(&self) -> Value {
        self.inner.parameters_schema()
    }

    fn risk(&self) -> Risk {
        self.inner.risk()
    }

    fn execute(&self, args: &Value) -> Result<Value, MimintError> {
        self.inner.execute(args)
    }

    fn last_queue_wait_ms(&self) -> Option<u64> {
        Some(self.inner.last_queue_wait_ms())
    }
}

/// Static `body_<action>` names, in spec order. Built once
/// from the action specs; the leak is one small table for the
/// process lifetime, and it keeps tool names `&'static`.
fn tool_names() -> &'static [&'static str] {
    use std::sync::OnceLock;
    static NAMES: OnceLock<Vec<&'static str>> = OnceLock::new();
    NAMES.get_or_init(|| {
        action_specs()
            .iter()
            .map(|spec| {
                let name: &'static str = Box::leak(format!("body_{}", spec.name).into_boxed_str());
                name
            })
            .collect()
    })
}

/// Register all body tools into a registry, in spec order.
/// The specs are leaked once (see [`action_specs_static`]) so
/// tools can hold `&'static ActionSpec`. Uses a fresh snapshot
/// slot and the default observation age: gates and tests that
/// do not wire an approver get no re-verification and the
/// default TTL.
pub fn register_body_tools(
    registry: &mut mimint_core::Registry,
    actor: BodyActor,
    frames: Arc<FrameStore>,
    privacy: PrivacyFlag,
) {
    register_body_tools_configured(
        registry,
        actor,
        frames,
        privacy,
        Arc::new(TargetSnapshotSlot::new()),
        DEFAULT_MAX_OBSERVATION_AGE,
    );
}

/// Register all body tools with the approver-shared snapshot
/// slot and a configured observation age (the worker path).
/// Single-agent entry: the tools act for the default agent.
pub fn register_body_tools_configured(
    registry: &mut mimint_core::Registry,
    actor: BodyActor,
    frames: Arc<FrameStore>,
    privacy: PrivacyFlag,
    snapshot_slot: Arc<TargetSnapshotSlot>,
    max_observation_age: Duration,
) {
    let specs = action_specs_static();
    let names = tool_names();
    for (spec, name) in specs.iter().zip(names.iter()) {
        registry.register(Box::new(NamedBodyTool::new(
            name,
            BodyTool::with_config(
                spec,
                actor.clone(),
                Arc::clone(&frames),
                privacy.clone(),
                Arc::clone(&snapshot_slot),
                max_observation_age,
                AGENT_DEFAULT,
            ),
        )));
    }
}

/// Register all body tools for one agent (Phase 6, §3): the
/// tools join the agent's actor queue, share the agent's
/// snapshot slot from `slots`, and attribute their frames to
/// the agent. Each agent gets its own registry built by this
/// function; the actor, store, and privacy flag stay shared.
pub fn register_body_tools_for_agent(
    registry: &mut mimint_core::Registry,
    actor: BodyActor,
    frames: Arc<FrameStore>,
    privacy: PrivacyFlag,
    slots: &AgentSnapshotSlots,
    agent_id: &str,
    max_observation_age: Duration,
) {
    let specs = action_specs_static();
    let names = tool_names();
    let slot = slots.slot_for(agent_id);
    for (spec, name) in specs.iter().zip(names.iter()) {
        registry.register(Box::new(NamedBodyTool::new(
            name,
            BodyTool::with_config(
                spec,
                actor.clone(),
                Arc::clone(&frames),
                privacy.clone(),
                Arc::clone(&slot),
                max_observation_age,
                agent_id,
            ),
        )));
    }
}

/// The action specs as `&'static` references, leaked once.
/// The specs are data, not code: leaking one small table for
/// the process lifetime is the honest price of the `Tool`
/// trait's `&'static str` names.
fn action_specs_static() -> &'static [ActionSpec] {
    use std::sync::OnceLock;
    static SPECS: OnceLock<Vec<ActionSpec>> = OnceLock::new();
    SPECS.get_or_init(action_specs)
}

/// Frame metadata as tool output: id, dimensions, scale, hash.
/// Pixels are never here.
fn frame_meta_json(meta: &FrameMeta) -> Value {
    json!({
        "frame_id": meta.id,
        "width": meta.width,
        "height": meta.height,
        "scale_factor": meta.scale_factor,
        "sha256": meta.sha256,
        "privacy_on_at_capture": meta.privacy_on_at_capture,
    })
}

fn body_err_to_tool(err: BodyError) -> MimintError {
    // A cancelled body request is a cancellation, not a
    // tool failure: the brain ends the turn Cancelled (§10).
    if let BodyError::Cancelled { .. } = err {
        return MimintError::Cancelled(err.message());
    }
    MimintError::Tool(format!("{}: {}", err.code(), err.message()))
}

/// Parse tool arguments into an [`Action`], per the action's
/// spec. Unknown actions and malformed arguments are tool
/// errors, never panics. Coordinate arguments must be JSON
/// integers in range; the body's own bounds check runs at
/// execution time.
fn parse_action(name: &str, args: &Value) -> Result<Action, MimintError> {
    let arg_i32 = |key: &str| -> Result<i32, MimintError> {
        args.get(key)
            .and_then(Value::as_i64)
            .and_then(|v| i32::try_from(v).ok())
            .ok_or_else(|| MimintError::Tool(format!("missing or invalid integer argument: {key}")))
    };
    let arg_u32 = |key: &str| -> Result<u32, MimintError> {
        args.get(key)
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
            .ok_or_else(|| MimintError::Tool(format!("missing or invalid integer argument: {key}")))
    };
    let arg_string = |key: &str| -> Result<String, MimintError> {
        args.get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| MimintError::Tool(format!("missing or invalid string argument: {key}")))
    };
    match name {
        "screenshot" => Ok(Action::Screenshot),
        "click" => Ok(Action::Click {
            x: arg_i32("x")?,
            y: arg_i32("y")?,
        }),
        "move" => Ok(Action::Move {
            x: arg_i32("x")?,
            y: arg_i32("y")?,
        }),
        "type_text" => Ok(Action::TypeText {
            text: arg_string("text")?,
        }),
        "key" => {
            let raw = arg_string("key")?;
            let key = Key::parse(&raw)
                .ok_or_else(|| MimintError::Tool(format!("unknown key name: {raw:?}")))?;
            Ok(Action::Key { key })
        }
        "scroll" => Ok(Action::Scroll {
            x: arg_i32("x")?,
            y: arg_i32("y")?,
            dx: arg_i32("dx")?,
            dy: arg_i32("dy")?,
        }),
        "drag" => Ok(Action::Drag {
            from_x: arg_i32("from_x")?,
            from_y: arg_i32("from_y")?,
            to_x: arg_i32("to_x")?,
            to_y: arg_i32("to_y")?,
        }),
        "double_click" => Ok(Action::DoubleClick {
            x: arg_i32("x")?,
            y: arg_i32("y")?,
        }),
        "right_click" => Ok(Action::RightClick {
            x: arg_i32("x")?,
            y: arg_i32("y")?,
        }),
        "hold_key" => {
            let raw = arg_string("key")?;
            let key = Key::parse(&raw)
                .ok_or_else(|| MimintError::Tool(format!("unknown key name: {raw:?}")))?;
            Ok(Action::HoldKey {
                key,
                millis: arg_u32("millis")?,
            })
        }
        other => Err(MimintError::Tool(format!("unknown body action: {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mimint_body::{MockBody, StopFlag};

    fn harness() -> (mimint_core::Registry, Arc<FrameStore>) {
        let actor = BodyActor::spawn(Box::new(MockBody::new()), StopFlag::new());
        let frames = Arc::new(FrameStore::new());
        let mut registry = mimint_core::Registry::new();
        register_body_tools(
            &mut registry,
            actor,
            Arc::clone(&frames),
            PrivacyFlag::new(false),
        );
        (registry, frames)
    }

    #[test]
    fn all_ten_tools_registered_in_spec_order() {
        let (registry, _) = harness();
        assert_eq!(
            registry.names(),
            vec![
                "body_screenshot",
                "body_click",
                "body_move",
                "body_type_text",
                "body_key",
                "body_scroll",
                "body_drag",
                "body_double_click",
                "body_right_click",
                "body_hold_key",
            ]
        );
    }

    #[test]
    fn schemas_come_from_the_specs() {
        let (registry, _) = harness();
        let specs = action_specs();
        for spec in &specs {
            let tool = registry
                .get(&format!("body_{}", spec.name))
                .unwrap_or_else(|| panic!("missing tool for {}", spec.name));
            assert_eq!(tool.parameters_schema(), spec.params_schema);
            assert_eq!(tool.description(), spec.description);
        }
    }

    #[test]
    fn screenshot_is_read_only_and_returns_metadata_only() {
        let (registry, frames) = harness();
        let tool = registry.get("body_screenshot").unwrap();
        assert!(matches!(tool.risk(), Risk::ReadOnly));
        let out = tool.execute(&json!({})).unwrap();
        assert_eq!(out["width"], 800);
        assert_eq!(out["height"], 600);
        assert!(out.get("frame_id").and_then(Value::as_u64).is_some());
        assert_eq!(out["sha256"].as_str().unwrap().len(), 64);
        // No pixel data anywhere in the output.
        let text = out.to_string();
        assert!(!text.contains("data"));
        assert_eq!(frames.len(), 1);
    }

    #[test]
    fn input_tools_are_consequential_and_execute() {
        let (registry, _) = harness();
        let tool = registry.get("body_click").unwrap();
        assert!(matches!(tool.risk(), Risk::Consequential));
        let out = tool.execute(&json!({"x": 10, "y": 10})).unwrap();
        assert_eq!(out, json!({"ok": true}));
    }

    #[test]
    fn malformed_arguments_are_tool_errors() {
        let (registry, _) = harness();
        let tool = registry.get("body_click").unwrap();
        assert!(tool.execute(&json!({"x": "ten"})).is_err());
        assert!(tool.execute(&json!({})).is_err());
        let key_tool = registry.get("body_key").unwrap();
        assert!(key_tool.execute(&json!({"key": "hyper"})).is_err());
        let hold = registry.get("body_hold_key").unwrap();
        assert!(hold.execute(&json!({"key": "f5", "millis": 0})).is_err());
    }

    #[test]
    fn privacy_flag_taints_stored_frames() {
        let body = MockBody::new();
        let actor = BodyActor::spawn(Box::new(body), StopFlag::new());
        let frames = Arc::new(FrameStore::new());
        let privacy = PrivacyFlag::new(true);
        let mut registry = mimint_core::Registry::new();
        register_body_tools(&mut registry, actor, Arc::clone(&frames), privacy);
        let out = registry
            .get("body_screenshot")
            .unwrap()
            .execute(&json!({}))
            .unwrap();
        assert_eq!(out["privacy_on_at_capture"], true);
        assert_eq!(frames.purge_tainted(), 1);
    }

    #[test]
    fn stale_observation_refuses_input_but_not_screenshots() {
        use mimint_body::Framebuffer;
        use std::time::Instant;
        let actor = BodyActor::spawn(Box::new(MockBody::new()), StopFlag::new());
        let frames = Arc::new(FrameStore::new());
        // A frame captured 400s ago, with a 300s TTL.
        let old = Framebuffer::new(4, 4, vec![0u8; 64])
            .unwrap()
            .with_captured_at(Instant::now() - Duration::from_secs(400));
        frames.store(old, false);
        let mut registry = mimint_core::Registry::new();
        register_body_tools(
            &mut registry,
            actor,
            Arc::clone(&frames),
            PrivacyFlag::new(false),
        );
        let err = registry
            .get("body_click")
            .unwrap()
            .execute(&json!({"x": 10, "y": 10}))
            .unwrap_err();
        assert!(err.to_string().contains("stale_observation"), "{err}");
        // A fresh screenshot clears the staleness: the
        // screenshot tool itself is never refused.
        let out = registry
            .get("body_screenshot")
            .unwrap()
            .execute(&json!({}))
            .unwrap();
        assert!(out.get("frame_id").is_some());
        assert!(registry
            .get("body_click")
            .unwrap()
            .execute(&json!({"x": 10, "y": 10}))
            .is_ok());
    }

    #[test]
    fn reverify_refuses_a_changed_target_and_allows_an_unchanged_one() {
        use mimint_body::{MockWindow, ScreenRect};
        let body = MockBody::new();
        body.set_windows(vec![MockWindow::new(
            1,
            ScreenRect::new(100, 100, 300, 300),
        )
        .with_class("editor")
        .with_fill((200, 40, 40))]);
        // The oracle shares the body's state: it can swap the
        // window underneath the actor-owned body (TOCTOU).
        let oracle = body.oracle();
        let actor = BodyActor::spawn(Box::new(body), StopFlag::new());
        let frames = Arc::new(FrameStore::new());
        let slot = Arc::new(TargetSnapshotSlot::new());
        let mut registry = mimint_core::Registry::new();
        register_body_tools_configured(
            &mut registry,
            actor.clone(),
            Arc::clone(&frames),
            PrivacyFlag::new(false),
            Arc::clone(&slot),
            DEFAULT_MAX_OBSERVATION_AGE,
        );
        // Approval-time snapshot, as the approver would take it.
        let probe = actor.probe_target(200, 200).unwrap();
        slot.set(200, 200, probe.snapshot.unwrap());
        // Unchanged target: the click runs.
        assert!(registry
            .get("body_click")
            .unwrap()
            .execute(&json!({"x": 200, "y": 200}))
            .is_ok());
        // A new snapshot, then the world changes underneath:
        // a different window with different content moves in.
        let probe = actor.probe_target(200, 200).unwrap();
        slot.set(200, 200, probe.snapshot.unwrap());
        oracle.set_windows(vec![MockWindow::new(
            2,
            ScreenRect::new(100, 100, 300, 300),
        )
        .with_class("browser")
        .with_fill((40, 40, 200))]);
        let err = registry
            .get("body_click")
            .unwrap()
            .execute(&json!({"x": 200, "y": 200}))
            .unwrap_err();
        assert!(err.to_string().contains("target_changed"), "{err}");
        // A snapshot for another point never applies.
        oracle.set_windows(vec![MockWindow::new(
            2,
            ScreenRect::new(100, 100, 300, 300),
        )
        .with_class("browser")
        .with_fill((40, 40, 200))]);
        let probe = actor.probe_target(200, 200).unwrap();
        slot.set(201, 201, probe.snapshot.unwrap());
        assert!(registry
            .get("body_click")
            .unwrap()
            .execute(&json!({"x": 200, "y": 200}))
            .is_ok());
    }

    #[test]
    fn generation_check_refuses_after_another_agent_acts() {
        use mimint_body::TargetOracle;
        // F6-03: A is approved against a snapshot; B clicks
        // elsewhere (the pixels under A's point do not
        // change); A's execution must be refused on the
        // generation alone, and A's click must never reach
        // the body. A fresh approval then runs fine.
        let body = MockBody::new();
        let mut oracle = body.oracle();
        let actor = BodyActor::spawn(Box::new(body), StopFlag::new());
        let frames = Arc::new(FrameStore::new());
        let slots = AgentSnapshotSlots::new();
        let mut reg_a = mimint_core::Registry::new();
        register_body_tools_for_agent(
            &mut reg_a,
            actor.clone(),
            Arc::clone(&frames),
            PrivacyFlag::new(false),
            &slots,
            "agent-001",
            DEFAULT_MAX_OBSERVATION_AGE,
        );
        let mut reg_b = mimint_core::Registry::new();
        register_body_tools_for_agent(
            &mut reg_b,
            actor.clone(),
            Arc::clone(&frames),
            PrivacyFlag::new(false),
            &slots,
            "agent-002",
            DEFAULT_MAX_OBSERVATION_AGE,
        );
        // A's approval-time snapshot, as the approver would
        // take it through the actor.
        let probe = actor.probe_target_for("agent-001", 200, 200).unwrap();
        slots
            .slot_for("agent-001")
            .set(200, 200, probe.snapshot.unwrap());
        // B acts: the click runs, the generation moves.
        assert!(reg_b
            .get("body_click")
            .unwrap()
            .execute(&json!({"x": 10, "y": 10}))
            .is_ok());
        // A's execution is refused on the generation — the
        // pixels under (200, 200) never changed.
        let err = reg_a
            .get("body_click")
            .unwrap()
            .execute(&json!({"x": 200, "y": 200}))
            .unwrap_err();
        let err_text = err.to_string();
        assert!(
            err_text.contains("target_changed_since_approval"),
            "{err_text}"
        );
        // A recoverable refusal, not a body fault: it must
        // never trip the fail-all path.
        assert!(!mimint_core::is_body_fault_message(&err_text));
        assert_eq!(oracle.received_clicks(), vec![(10, 10)]);
        // A fresh approval against the current generation
        // runs.
        let probe = actor.probe_target_for("agent-001", 200, 200).unwrap();
        slots
            .slot_for("agent-001")
            .set(200, 200, probe.snapshot.unwrap());
        assert!(reg_a
            .get("body_click")
            .unwrap()
            .execute(&json!({"x": 200, "y": 200}))
            .is_ok());
        assert_eq!(oracle.received_clicks(), vec![(10, 10), (200, 200)]);
    }

    #[test]
    fn cancelled_agent_maps_to_cancelled_not_a_tool_error() {
        let actor = BodyActor::spawn(Box::new(MockBody::new()), StopFlag::new());
        actor.cancel_agent("agent-001");
        let frames = Arc::new(FrameStore::new());
        let mut registry = mimint_core::Registry::new();
        register_body_tools(
            &mut registry,
            actor,
            Arc::clone(&frames),
            PrivacyFlag::new(false),
        );
        let err = registry
            .get("body_click")
            .unwrap()
            .execute(&json!({"x": 10, "y": 10}))
            .unwrap_err();
        assert!(
            matches!(err, mimint_core::MimintError::Cancelled(_)),
            "expected Cancelled, got {err:?}"
        );
    }

    #[test]
    fn queue_wait_is_reported_after_execute() {
        let (registry, _) = harness();
        let tool = registry.get("body_click").unwrap();
        assert!(tool.execute(&json!({"x": 10, "y": 10})).is_ok());
        // Ran at once: a queue wait is reported (usually 0,
        // occasionally 1ms under load), never None — body
        // tools have a queue, and the executor reads this
        // after every call for the execution record.
        let wait = tool.last_queue_wait_ms();
        assert!(wait.is_some(), "queue wait must be reported");
        assert!(wait.unwrap() < 1000, "wait = {wait:?}");
    }

    #[test]
    fn floor_check_refuses_input_after_rebuild_until_a_fresh_screenshot() {
        let actor = BodyActor::spawn(Box::new(MockBody::new()), StopFlag::new());
        let frames = Arc::new(FrameStore::new());
        let mut registry = mimint_core::Registry::new();
        register_body_tools(
            &mut registry,
            actor,
            Arc::clone(&frames),
            PrivacyFlag::new(false),
        );
        // Before any rebuild the floor is 0: input runs.
        assert!(registry
            .get("body_click")
            .unwrap()
            .execute(&json!({"x": 10, "y": 10}))
            .is_ok());
        // A rebuild purges the store and raises the floor:
        // input is refused until a fresh screenshot lands.
        frames.rebuild_barrier();
        let err = registry
            .get("body_click")
            .unwrap()
            .execute(&json!({"x": 10, "y": 10}))
            .unwrap_err();
        assert!(err.to_string().contains("stale_observation"), "{err}");
        assert!(registry
            .get("body_screenshot")
            .unwrap()
            .execute(&json!({}))
            .is_ok());
        assert!(registry
            .get("body_click")
            .unwrap()
            .execute(&json!({"x": 10, "y": 10}))
            .is_ok());
    }

    #[test]
    fn per_agent_slots_partition_reverification() {
        use mimint_body::{MockWindow, ScreenRect};
        let body = MockBody::new();
        body.set_windows(vec![MockWindow::new(
            1,
            ScreenRect::new(100, 100, 300, 300),
        )
        .with_class("editor")
        .with_fill((200, 40, 40))]);
        let oracle = body.oracle();
        let actor = BodyActor::spawn(Box::new(body), StopFlag::new());
        let frames = Arc::new(FrameStore::new());
        let slots = AgentSnapshotSlots::new();
        let mut reg1 = mimint_core::Registry::new();
        register_body_tools_for_agent(
            &mut reg1,
            actor.clone(),
            Arc::clone(&frames),
            PrivacyFlag::new(false),
            &slots,
            "agent-001",
            DEFAULT_MAX_OBSERVATION_AGE,
        );
        let mut reg2 = mimint_core::Registry::new();
        register_body_tools_for_agent(
            &mut reg2,
            actor.clone(),
            Arc::clone(&frames),
            PrivacyFlag::new(false),
            &slots,
            "agent-002",
            DEFAULT_MAX_OBSERVATION_AGE,
        );
        // A snapshot in agent-001's slot only, then the
        // world changes underneath.
        let probe = actor.probe_target(200, 200).unwrap();
        slots
            .slot_for("agent-001")
            .set(200, 200, probe.snapshot.unwrap());
        oracle.set_windows(vec![MockWindow::new(
            2,
            ScreenRect::new(100, 100, 300, 300),
        )
        .with_class("browser")
        .with_fill((40, 40, 200))]);
        // agent-001 re-verifies against its snapshot: refused.
        let err = reg1
            .get("body_click")
            .unwrap()
            .execute(&json!({"x": 200, "y": 200}))
            .unwrap_err();
        assert!(err.to_string().contains("target_changed"), "{err}");
        // agent-002 has no snapshot in its own slot: the
        // same point proceeds (no re-verification).
        assert!(reg2
            .get("body_click")
            .unwrap()
            .execute(&json!({"x": 200, "y": 200}))
            .is_ok());
    }

    #[test]
    fn g6_7_cross_agent_snapshot_refused() {
        use mimint_body::{Framebuffer, MockWindow, ScreenRect, TargetSnapshot};
        use std::time::Instant;
        // One window under the whole screen: the probe reports
        // window 2 at every point.
        let body = MockBody::new();
        body.set_windows(vec![MockWindow::new(2, ScreenRect::new(0, 0, 800, 600))]);
        let actor = BodyActor::spawn(Box::new(body), StopFlag::new());
        let frames = Arc::new(FrameStore::new());
        let slots = AgentSnapshotSlots::new();
        let mut reg_a = mimint_core::Registry::new();
        register_body_tools_for_agent(
            &mut reg_a,
            actor.clone(),
            Arc::clone(&frames),
            PrivacyFlag::new(false),
            &slots,
            "agent-001",
            Duration::from_secs(300),
        );
        let mut reg_b = mimint_core::Registry::new();
        register_body_tools_for_agent(
            &mut reg_b,
            actor.clone(),
            Arc::clone(&frames),
            PrivacyFlag::new(false),
            &slots,
            "agent-002",
            Duration::from_secs(300),
        );

        // (1) Snapshot partition: A holds a stale snapshot for
        // (10,10) — approval-time window 1, since replaced by
        // window 2. B holds no snapshot at all.
        slots.slot_for("agent-001").set(
            10,
            10,
            TargetSnapshot {
                window_id: Some(1),
                grid: [0u8; 64],
                mutation_seq: 0,
            },
        );
        // B clicks first: B never consults A's snapshot, so
        // the click runs even though A's snapshot is stale.
        assert!(reg_b
            .get("body_click")
            .unwrap()
            .execute(&json!({"x": 10, "y": 10}))
            .is_ok());
        // A's own click consults A's stale snapshot: the
        // target changed, and the action never runs.
        let err = reg_a
            .get("body_click")
            .unwrap()
            .execute(&json!({"x": 10, "y": 10}))
            .unwrap_err();
        assert!(err.to_string().contains("target_changed"), "{err}");

        // (2) TTL is per-agent: A's frame is 400s old, B's is
        // fresh. The frame store attributes each frame.
        let old = Framebuffer::new(4, 4, vec![0u8; 64])
            .unwrap()
            .with_captured_at(Instant::now() - Duration::from_secs(400));
        assert_eq!(
            frames.store_for("agent-001", old, false).agent_id,
            "agent-001"
        );
        let fresh = Framebuffer::new(4, 4, vec![0u8; 64]).unwrap();
        assert_eq!(
            frames.store_for("agent-002", fresh, false).agent_id,
            "agent-002"
        );
        assert!(reg_b
            .get("body_click")
            .unwrap()
            .execute(&json!({"x": 20, "y": 20}))
            .is_ok());
        let err = reg_a
            .get("body_click")
            .unwrap()
            .execute(&json!({"x": 20, "y": 20}))
            .unwrap_err();
        assert!(err.to_string().contains("stale_observation"), "{err}");

        // (3) The rebuild floor is per-agent too: after a
        // rebuild, only the agent that screenshots anew may act.
        frames.rebuild_barrier();
        assert!(reg_b
            .get("body_screenshot")
            .unwrap()
            .execute(&json!({}))
            .is_ok());
        assert!(reg_b
            .get("body_click")
            .unwrap()
            .execute(&json!({"x": 20, "y": 20}))
            .is_ok());
        let err = reg_a
            .get("body_click")
            .unwrap()
            .execute(&json!({"x": 20, "y": 20}))
            .unwrap_err();
        assert!(err.to_string().contains("stale_observation"), "{err}");
    }
}
