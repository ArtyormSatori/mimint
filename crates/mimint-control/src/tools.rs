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

use crate::actor::BodyActor;
use crate::frames::{FrameMeta, FrameStore};
use mimint_body::{action_specs, ActOutcome, Action, ActionSpec, BodyError, Key};
use mimint_core::{MimintError, Risk, Tool};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

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
}

impl BodyTool {
    fn new(
        spec: &'static ActionSpec,
        actor: BodyActor,
        frames: Arc<FrameStore>,
        privacy: PrivacyFlag,
    ) -> BodyTool {
        BodyTool {
            spec,
            actor,
            frames,
            privacy,
        }
    }

    /// The tool name: `body_<action>`.
    pub fn tool_name(action: &str) -> String {
        format!("body_{action}")
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
        match self.actor.act(action) {
            Ok(ActOutcome::Frame(frame)) => {
                let meta = self.frames.store(frame, self.privacy.is_on());
                Ok(frame_meta_json(&meta))
            }
            Ok(ActOutcome::Done) => Ok(json!({"ok": true})),
            Err(err) => Err(body_err_to_tool(err)),
        }
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
/// tools can hold `&'static ActionSpec`.
pub fn register_body_tools(
    registry: &mut mimint_core::Registry,
    actor: BodyActor,
    frames: Arc<FrameStore>,
    privacy: PrivacyFlag,
) {
    let specs = action_specs_static();
    let names = tool_names();
    for (spec, name) in specs.iter().zip(names.iter()) {
        registry.register(Box::new(NamedBodyTool::new(
            name,
            BodyTool::new(spec, actor.clone(), Arc::clone(&frames), privacy.clone()),
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
}
