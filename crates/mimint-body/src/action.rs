//! The action surface, generated from the implementation (ACI pattern).
//!
//! Every action a body can perform is one variant of [`Action`], and
//! [`action_specs`] produces the machine-readable description of each
//! one (name, description, JSON parameter schema) from the same code.
//! There is no hand-written prompt text describing actions anywhere:
//! anything that needs to tell a model what a body can do reads these
//! specs. A test pins the two sides together: every variant has exactly
//! one spec, and every spec names a variant.

use serde_json::{json, Value};

/// Special keys a body can press. Only keys with a mapping on every
/// supported platform belong here; platform bodies map each variant to
/// their native key code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    Enter,
    Tab,
    Escape,
    Backspace,
    Delete,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    Space,
    /// Function key, used by test targets to change their content.
    F5,
}

impl Key {
    /// Stable wire name, shared by specs, logs, and oracles.
    pub fn name(self) -> &'static str {
        match self {
            Key::Enter => "enter",
            Key::Tab => "tab",
            Key::Escape => "escape",
            Key::Backspace => "backspace",
            Key::Delete => "delete",
            Key::ArrowUp => "arrow_up",
            Key::ArrowDown => "arrow_down",
            Key::ArrowLeft => "arrow_left",
            Key::ArrowRight => "arrow_right",
            Key::Space => "space",
            Key::F5 => "f5",
        }
    }

    /// Parse a wire name back. Unknown names are an error, never a guess.
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "enter" => Key::Enter,
            "tab" => Key::Tab,
            "escape" => Key::Escape,
            "backspace" => Key::Backspace,
            "delete" => Key::Delete,
            "arrow_up" => Key::ArrowUp,
            "arrow_down" => Key::ArrowDown,
            "arrow_left" => Key::ArrowLeft,
            "arrow_right" => Key::ArrowRight,
            "space" => Key::Space,
            "f5" => Key::F5,
            _ => return None,
        })
    }

    /// Every key, in wire-name order. Used by the spec pin test.
    pub fn all() -> &'static [Key] {
        &[
            Key::Enter,
            Key::Tab,
            Key::Escape,
            Key::Backspace,
            Key::Delete,
            Key::ArrowUp,
            Key::ArrowDown,
            Key::ArrowLeft,
            Key::ArrowRight,
            Key::Space,
            Key::F5,
        ]
    }
}

/// One action a body can perform. Coordinates are framebuffer
/// pixels (see the contract in `crate::body`): the single coordinate
/// space, origin top-left of the frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Capture the full primary display.
    Screenshot,
    /// Move the pointer to (x, y) and click the primary button.
    Click { x: i32, y: i32 },
    /// Move the pointer to (x, y) without clicking.
    Move { x: i32, y: i32 },
    /// Type Unicode text as characters. `\n` means Enter, `\t`
    /// means Tab. This is the TEXT path: it types characters,
    /// independent of the keyboard layout where the platform allows
    /// it. It is NOT the physical-key path — arrows, function keys,
    /// and modifiers belong to [`Action::Key`].
    TypeText { text: String },
    /// Press and release one special (physical) key.
    Key { key: Key },
    /// Scroll the wheel at (x, y). `dx`/`dy` are wheel notches
    /// (detents): `dy > 0` scrolls down, `dy < 0` scrolls up,
    /// `dx > 0` scrolls right, `dx < 0` scrolls left. (0, 0) is a
    /// no-op that still validates the point. Notches — not pixels
    /// or lines — are the portable unit: every platform reports
    /// wheel input in detents, and the target decides how far one
    /// detent moves.
    Scroll { x: i32, y: i32, dx: i32, dy: i32 },
    /// Press the primary button at (from_x, from_y), move to
    /// (to_x, to_y) with the button held, release. Both endpoints
    /// are validated like a click.
    Drag {
        from_x: i32,
        from_y: i32,
        to_x: i32,
        to_y: i32,
    },
    /// Two primary-button clicks at (x, y) in quick succession, as
    /// one action (the target must observe a double-click, not two
    /// unrelated clicks).
    DoubleClick { x: i32, y: i32 },
    /// One secondary-button (right) click at (x, y).
    RightClick { x: i32, y: i32 },
    /// Hold one special (physical) key down for `millis`
    /// milliseconds, then release. `millis` must be 1..=5000: a
    /// longer hold is refused before anything happens (a stuck
    /// modifier must be a caller error, not a body behavior).
    HoldKey { key: Key, millis: u32 },
}

impl Action {
    /// Stable wire name of this action.
    pub fn name(&self) -> &'static str {
        match self {
            Action::Screenshot => "screenshot",
            Action::Click { .. } => "click",
            Action::Move { .. } => "move",
            Action::TypeText { .. } => "type_text",
            Action::Key { .. } => "key",
            Action::Scroll { .. } => "scroll",
            Action::Drag { .. } => "drag",
            Action::DoubleClick { .. } => "double_click",
            Action::RightClick { .. } => "right_click",
            Action::HoldKey { .. } => "hold_key",
        }
    }
}

/// Maximum key-hold duration in milliseconds. Holds longer than
/// this are refused with [`crate::BodyError::InvalidArgument`].
pub const MAX_HOLD_MILLIS: u32 = 5000;

/// Machine-readable description of one action, generated from code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionSpec {
    /// Wire name, matching [`Action::name`].
    pub name: &'static str,
    /// One sentence a model or a user can read.
    pub description: &'static str,
    /// JSON Schema (draft 2020-12 subset) for the action's parameters.
    /// Always an object schema; `screenshot` takes no parameters.
    pub params_schema: Value,
}

/// The full action surface, generated from the implementation. This is
/// the single source of truth: the [`Action`] enum and this list are
/// pinned together by test.
pub fn action_specs() -> Vec<ActionSpec> {
    vec![
        ActionSpec {
            name: "screenshot",
            description: "Capture the full primary display as an RGBA image.",
            params_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false,
            }),
        },
        ActionSpec {
            name: "click",
            description: "Move the pointer to framebuffer-pixel coordinates (x, y) and click the primary button. Origin is the top-left of the screenshot frame; the body converts to OS-native coordinates internally. Validated against the display size at execution time.",
            params_schema: json!({
                "type": "object",
                "properties": {
                    "x": { "type": "integer", "description": "Framebuffer X pixel, 0-based from the left edge of the screenshot." },
                    "y": { "type": "integer", "description": "Framebuffer Y pixel, 0-based from the top edge of the screenshot." },
                },
                "required": ["x", "y"],
                "additionalProperties": false,
            }),
        },
        ActionSpec {
            name: "move",
            description: "Move the pointer to framebuffer-pixel coordinates (x, y) without clicking.",
            params_schema: json!({
                "type": "object",
                "properties": {
                    "x": { "type": "integer" },
                    "y": { "type": "integer" },
                },
                "required": ["x", "y"],
                "additionalProperties": false,
            }),
        },
        ActionSpec {
            name: "type_text",
            description: "Type Unicode text as characters into the focused target. Newline means Enter, tab means Tab. Only characters the platform can deliver can be typed; others fail the whole call before anything is typed. This types TEXT (characters); physical keys (arrows, function keys) use the 'key' action instead. IME composition is not supported: precomposed characters are injected as single characters.",
            params_schema: json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string" },
                },
                "required": ["text"],
                "additionalProperties": false,
            }),
        },
        ActionSpec {
            name: "key",
            description: "Press and release one special (physical) key. This presses a KEY, it does not type a character; use 'type_text' for text.",
            params_schema: json!({
                "type": "object",
                "properties": {
                    "key": {
                        "type": "string",
                        "enum": Key::all().iter().map(|k| k.name()).collect::<Vec<_>>(),
                    },
                },
                "required": ["key"],
                "additionalProperties": false,
            }),
        },
        ActionSpec {
            name: "scroll",
            description: "Scroll the wheel at framebuffer-pixel coordinates (x, y). dx/dy are wheel notches: dy > 0 scrolls down, dy < 0 up, dx > 0 right, dx < 0 left.",
            params_schema: json!({
                "type": "object",
                "properties": {
                    "x": { "type": "integer" },
                    "y": { "type": "integer" },
                    "dx": { "type": "integer", "description": "Horizontal wheel notches (positive = right)." },
                    "dy": { "type": "integer", "description": "Vertical wheel notches (positive = down)." },
                },
                "required": ["x", "y", "dx", "dy"],
                "additionalProperties": false,
            }),
        },
        ActionSpec {
            name: "drag",
            description: "Press the primary button at (from_x, from_y), move to (to_x, to_y) with the button held, then release. Coordinates are framebuffer pixels.",
            params_schema: json!({
                "type": "object",
                "properties": {
                    "from_x": { "type": "integer" },
                    "from_y": { "type": "integer" },
                    "to_x": { "type": "integer" },
                    "to_y": { "type": "integer" },
                },
                "required": ["from_x", "from_y", "to_x", "to_y"],
                "additionalProperties": false,
            }),
        },
        ActionSpec {
            name: "double_click",
            description: "Double-click the primary button at framebuffer-pixel coordinates (x, y): two clicks in quick succession, as one action.",
            params_schema: json!({
                "type": "object",
                "properties": {
                    "x": { "type": "integer" },
                    "y": { "type": "integer" },
                },
                "required": ["x", "y"],
                "additionalProperties": false,
            }),
        },
        ActionSpec {
            name: "right_click",
            description: "Click the secondary (right) button at framebuffer-pixel coordinates (x, y).",
            params_schema: json!({
                "type": "object",
                "properties": {
                    "x": { "type": "integer" },
                    "y": { "type": "integer" },
                },
                "required": ["x", "y"],
                "additionalProperties": false,
            }),
        },
        ActionSpec {
            name: "hold_key",
            description: "Hold one special (physical) key down for millis milliseconds, then release. millis must be between 1 and 5000.",
            params_schema: json!({
                "type": "object",
                "properties": {
                    "key": {
                        "type": "string",
                        "enum": Key::all().iter().map(|k| k.name()).collect::<Vec<_>>(),
                    },
                    "millis": { "type": "integer", "minimum": 1, "maximum": 5000 },
                },
                "required": ["key", "millis"],
                "additionalProperties": false,
            }),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_cover_every_action_variant_exactly_once() {
        let specs = action_specs();
        let variants = [
            Action::Screenshot,
            Action::Click { x: 0, y: 0 },
            Action::Move { x: 0, y: 0 },
            Action::TypeText {
                text: String::new(),
            },
            Action::Key { key: Key::Enter },
            Action::Scroll {
                x: 0,
                y: 0,
                dx: 0,
                dy: 0,
            },
            Action::Drag {
                from_x: 0,
                from_y: 0,
                to_x: 0,
                to_y: 0,
            },
            Action::DoubleClick { x: 0, y: 0 },
            Action::RightClick { x: 0, y: 0 },
            Action::HoldKey {
                key: Key::Enter,
                millis: 100,
            },
        ];
        assert_eq!(
            specs.len(),
            variants.len(),
            "spec count must match variants"
        );
        for variant in &variants {
            let matches: Vec<_> = specs.iter().filter(|s| s.name == variant.name()).collect();
            assert_eq!(matches.len(), 1, "exactly one spec for {}", variant.name());
        }
        // Every spec is a valid object schema with the required shape.
        for spec in &specs {
            assert_eq!(spec.params_schema["type"], "object");
            assert!(spec.params_schema["properties"].is_object());
            assert_eq!(spec.params_schema["additionalProperties"], false);
        }
        // The key enum in the schema matches Key::all() exactly.
        let key_spec = specs.iter().find(|s| s.name == "key").unwrap();
        let schema_keys: Vec<String> = key_spec.params_schema["properties"]["key"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        let all_keys: Vec<String> = Key::all().iter().map(|k| k.name().to_string()).collect();
        assert_eq!(schema_keys, all_keys);
    }

    #[test]
    fn key_names_round_trip() {
        for key in Key::all() {
            assert_eq!(Key::parse(key.name()), Some(*key));
        }
        assert_eq!(Key::parse("nope"), None);
        assert_eq!(Key::parse(""), None);
    }
}
