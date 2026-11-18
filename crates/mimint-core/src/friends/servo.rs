//! Device MCP + servo control through the hub (Phase 7, §8.4).
//!
//! Every device command passes the same allow-list, whoever
//! asks for it — including the local user over MCP
//! (`mcp_user_only_not_trusted`): MCP calls are untrusted
//! input, not a privilege escalation path. Out-of-range
//! numeric values are CLAMPED (and the clamping is reported),
//! never silently accepted and never rejected for being
//! slightly out of range.
//!
//! `reboot` and `upgrade_firmware` are NEVER callable by the
//! hub — not by a peer, not by MCP, not by the user through
//! the hub. There is no code path that invokes them.

use crate::error::MimintError;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;

/// Who asked for the command. All callers get the same
/// validation; none bypasses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpCaller {
    User,
    Peer,
    Device,
}

/// One device command as received (method + params).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServoCommand {
    pub method: String,
    pub params: serde_json::Value,
}

/// A validated command, ready to dispatch to the driver.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedServoCommand {
    pub method: String,
    pub params: serde_json::Value,
    /// True when a value was clamped into range.
    pub clamped: bool,
}

/// The allow-list (§8.4): method → (param, min, max) for
/// numeric params. Methods outside this list are refused.
pub const SERVO_ALLOW_LIST: &[&str] = &[
    "servo.move",
    "servo.stop",
    "device.led.set",
    "device.status",
];

/// Methods that are NEVER callable by the hub, from any
/// caller, through any path.
pub const NEVER_CALLABLE: &[&str] = &["reboot", "upgrade_firmware"];

/// Validate one command against the allow-list. Unknown
/// methods are refused; `reboot`/`upgrade_firmware` are
/// refused with an explicit reason; numeric params are
/// clamped into range.
pub fn validate_servo_command(
    caller: McpCaller,
    command: &ServoCommand,
) -> Result<ValidatedServoCommand, MimintError> {
    let _ = caller; // No caller bypasses validation.
    if NEVER_CALLABLE.contains(&command.method.as_str()) {
        return Err(MimintError::Config(format!(
            "method '{}' is never callable by the hub",
            command.method
        )));
    }
    if !SERVO_ALLOW_LIST.contains(&command.method.as_str()) {
        return Err(MimintError::Config(format!(
            "method '{}' is not on the device allow-list",
            command.method
        )));
    }
    let mut clamped = false;
    let mut params = command.params.clone();
    let clamp = |params: &mut serde_json::Value, key: &str, min: f64, max: f64| {
        if let Some(value) = params.get_mut(key) {
            if let Some(number) = value.as_f64() {
                let clamped_value = number.clamp(min, max);
                if (clamped_value - number).abs() > f64::EPSILON {
                    *value = serde_json::json!(clamped_value);
                    return true;
                }
            }
        }
        false
    };
    match command.method.as_str() {
        "servo.move" => {
            clamped |= clamp(&mut params, "angle", 0.0, 180.0);
            clamped |= clamp(&mut params, "speed", 0.0, 100.0);
            if params
                .get("angle")
                .and_then(serde_json::Value::as_f64)
                .is_none()
            {
                return Err(MimintError::Config(
                    "servo.move requires a numeric angle".to_string(),
                ));
            }
        }
        "device.led.set" => {
            clamped |= clamp(&mut params, "brightness", 0.0, 100.0);
            let state = params
                .get("state")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("on");
            if !matches!(state, "on" | "off") {
                return Err(MimintError::Config(
                    "device.led.set state must be 'on' or 'off'".to_string(),
                ));
            }
        }
        "servo.stop" | "device.status" => {}
        _ => unreachable!(),
    }
    Ok(ValidatedServoCommand {
        method: command.method.clone(),
        params,
        clamped,
    })
}

/// Approved multi-step sequences: named scripts the user
/// approved once; the hub runs them without re-asking per
/// step. Unapproved sequences are refused.
pub struct ApprovedSequences {
    sequences: Mutex<HashMap<String, Vec<ValidatedServoCommand>>>,
}

impl ApprovedSequences {
    pub fn new() -> Self {
        Self {
            sequences: Mutex::new(HashMap::new()),
        }
    }

    /// Approve a sequence (user action). Every command is
    /// validated at approval time.
    pub fn approve(&self, name: &str, commands: &[ServoCommand]) -> Result<(), MimintError> {
        let mut validated = Vec::new();
        for command in commands {
            validated.push(validate_servo_command(McpCaller::User, command)?);
        }
        let mut sequences = self.sequences.lock().unwrap_or_else(|p| p.into_inner());
        sequences.insert(name.to_string(), validated);
        Ok(())
    }

    /// The validated commands of an approved sequence.
    pub fn get(&self, name: &str) -> Result<Vec<ValidatedServoCommand>, MimintError> {
        let sequences = self.sequences.lock().unwrap_or_else(|p| p.into_inner());
        sequences
            .get(name)
            .cloned()
            .ok_or_else(|| MimintError::Config(format!("device sequence '{name}' is not approved")))
    }

    pub fn list(&self) -> Vec<String> {
        let sequences = self.sequences.lock().unwrap_or_else(|p| p.into_inner());
        let mut names: Vec<String> = sequences.keys().cloned().collect();
        names.sort();
        names
    }
}

impl Default for ApprovedSequences {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(method: &str, params: serde_json::Value) -> ServoCommand {
        ServoCommand {
            method: method.to_string(),
            params,
        }
    }

    #[test]
    fn mcp_servo_allowlist_enforced() {
        // In-range passes untouched.
        let ok = validate_servo_command(
            McpCaller::Peer,
            &command("servo.move", serde_json::json!({"angle": 90, "speed": 50})),
        )
        .unwrap();
        assert!(!ok.clamped);
        // Out-of-range clamps.
        let clamped = validate_servo_command(
            McpCaller::Peer,
            &command("servo.move", serde_json::json!({"angle": 270, "speed": -5})),
        )
        .unwrap();
        assert!(clamped.clamped);
        assert_eq!(clamped.params["angle"], serde_json::json!(180.0));
        assert_eq!(clamped.params["speed"], serde_json::json!(0.0));
        // Unknown method refused.
        assert!(validate_servo_command(
            McpCaller::User,
            &command("servo.explode", serde_json::json!({})),
        )
        .is_err());
        // servo.stop / device.status need no params.
        assert!(validate_servo_command(
            McpCaller::Device,
            &command("servo.stop", serde_json::json!({})),
        )
        .is_ok());
    }

    #[test]
    fn mcp_user_only_not_trusted() {
        // Even the USER caller cannot invoke the forbidden
        // methods through the hub, and cannot bypass the
        // allow-list.
        for method in ["reboot", "upgrade_firmware"] {
            assert!(
                validate_servo_command(McpCaller::User, &command(method, serde_json::json!({})))
                    .is_err(),
                "{method} must be refused even for the user"
            );
        }
        assert!(validate_servo_command(
            McpCaller::User,
            &command("unknown.method", serde_json::json!({})),
        )
        .is_err());
    }

    #[test]
    fn approved_sequences_run_only_when_approved() {
        let sequences = ApprovedSequences::new();
        assert!(sequences.get("wave").is_err());
        sequences
            .approve(
                "wave",
                &[
                    command("servo.move", serde_json::json!({"angle": 45})),
                    command("servo.move", serde_json::json!({"angle": 135})),
                    command("servo.stop", serde_json::json!({})),
                ],
            )
            .unwrap();
        assert_eq!(sequences.get("wave").unwrap().len(), 3);
        // Approval validates every step: a bad step fails the
        // whole sequence.
        assert!(sequences
            .approve("bad", &[command("reboot", serde_json::json!({}))])
            .is_err());
    }
}
