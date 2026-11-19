//! The production driver backend: hub driver requests go
//! through the [`BodyActor`], never straight to the body.
//!
//! Why the actor matters: the actor owns the single body
//! thread, enforces the per-action timeout, and latches
//! `broken` on [`BodyError::UnknownOutcome`] so a half-done
//! action is never silently retried. A driver request that
//! bypassed the actor would lose all three guarantees.
//!
//! Scope (Phase 7): the ACTION methods (§5.3) are mapped to
//! [`Action`]s and run on the actor. The three READ methods
//! (`display_size`, `focused_window`, `permissions`) and
//! `stop` are NOT wired here — reads need a probe path, not
//! an action, and `stop` has no [`Action`] (the hub's kill
//! button uses [`Hub::kill_all_drivers`]). They fail with a
//! clear error rather than a faked value.
//!
//! [`Hub::kill_all_drivers`]: mimint_core::friends::hub::Hub::kill_all_drivers

use crate::actor::BodyActor;
use mimint_body::{ActOutcome, Action, Body, Key, StopFlag};
use mimint_core::friends::hub::DriverBackend;
use mimint_core::MimintError;
use std::sync::Arc;

/// A [`DriverBackend`] that drives the body through the actor.
pub struct ActorDriverBackend {
    actor: BodyActor,
}

impl ActorDriverBackend {
    /// Spawn the actor thread owning `body` and wrap it.
    pub fn spawn(body: Box<dyn Body>, stop: Arc<StopFlag>) -> Self {
        Self {
            actor: BodyActor::spawn(body, stop),
        }
    }

    /// Wrap an EXISTING actor (the app's body actor): the hub
    /// drives the same single body thread the worker uses, so
    /// the timeout + broken-latch guarantees hold for driver
    /// requests too. `BodyActor` is cheaply clonable.
    pub fn from_actor(actor: BodyActor) -> Self {
        Self { actor }
    }
}

fn param_i(params: &serde_json::Value, key: &str) -> Result<i32, MimintError> {
    params
        .get(key)
        .and_then(|v| v.as_i64())
        .and_then(|n| i32::try_from(n).ok())
        .ok_or_else(|| {
            MimintError::Config(format!("driver param '{key}' missing or not an integer"))
        })
}

fn param_u(params: &serde_json::Value, key: &str) -> Result<u32, MimintError> {
    params
        .get(key)
        .and_then(|v| v.as_u64())
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| {
            MimintError::Config(format!("driver param '{key}' missing or not an integer"))
        })
}

fn param_s(params: &serde_json::Value, key: &str) -> Result<String, MimintError> {
    params
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| MimintError::Config(format!("driver param '{key}' missing or not a string")))
}

fn param_key(params: &serde_json::Value, key: &str) -> Result<Key, MimintError> {
    let name = param_s(params, key)?;
    Key::parse(&name).ok_or_else(|| MimintError::Config(format!("unknown key name '{name}'")))
}

impl DriverBackend for ActorDriverBackend {
    fn execute(
        &self,
        method: &str,
        params: &serde_json::Value,
    ) -> Result<serde_json::Value, MimintError> {
        let action = match method {
            "screenshot" => Action::Screenshot,
            "click" => Action::Click {
                x: param_i(params, "x")?,
                y: param_i(params, "y")?,
            },
            "move" => Action::Move {
                x: param_i(params, "x")?,
                y: param_i(params, "y")?,
            },
            "type_text" => Action::TypeText {
                text: param_s(params, "text")?,
            },
            "key" => Action::Key {
                key: param_key(params, "key")?,
            },
            "scroll" => Action::Scroll {
                x: param_i(params, "x")?,
                y: param_i(params, "y")?,
                dx: param_i(params, "dx")?,
                dy: param_i(params, "dy")?,
            },
            "drag" => Action::Drag {
                from_x: param_i(params, "from_x")?,
                from_y: param_i(params, "from_y")?,
                to_x: param_i(params, "to_x")?,
                to_y: param_i(params, "to_y")?,
            },
            "double_click" => Action::DoubleClick {
                x: param_i(params, "x")?,
                y: param_i(params, "y")?,
            },
            "right_click" => Action::RightClick {
                x: param_i(params, "x")?,
                y: param_i(params, "y")?,
            },
            "hold_key" => Action::HoldKey {
                key: param_key(params, "key")?,
                millis: param_u(params, "millis")?,
            },
            "display_size" | "focused_window" | "permissions" => {
                return Err(MimintError::Config(format!(
                    "driver method '{method}' is a read, not an action: not wired through the actor in Phase 7"
                )));
            }
            "stop" => {
                return Err(MimintError::Config(
                    "driver method 'stop' has no action: use the hub kill button instead"
                        .to_string(),
                ));
            }
            other => {
                return Err(MimintError::Config(format!(
                    "unknown driver method '{other}'"
                )));
            }
        };
        match self.actor.act(action) {
            Ok(ActOutcome::Frame(frame)) => Ok(serde_json::json!({
                "ok": true,
                "width": frame.width,
                "height": frame.height,
                "bytes": frame.data.len(),
            })),
            Ok(ActOutcome::Done) => Ok(serde_json::json!({ "ok": true })),
            Err(err) => Err(MimintError::Config(format!(
                "driver action failed: {err:?}"
            ))),
        }
    }

    fn name(&self) -> &str {
        "actor"
    }
}
