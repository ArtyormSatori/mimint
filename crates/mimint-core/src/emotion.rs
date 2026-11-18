//! Emotion codes (Phase 5, P5.6): the model sends one of 9 codes
//! over a STRUCTURED channel — a tool call to `set_emotion` — and
//! the UI face follows it. Codes are never parsed out of free
//! text with regex (Agent S lesson: regex parsing of model output
//! is brittle; the brain reads the parsed JSON arguments of the
//! tool call, nothing else).
//!
//! The 9 codes map 1:1 onto the 9 UI face states (design spec §2,
//! core looks 1–9). An unknown, missing, or non-string code falls
//! back safely: the previous face state is kept (or Neutral when
//! there is none), the fallback is audited, and nothing crashes.
//! The mapping from code to face lives in `mimint-ui`; this module
//! owns the wire format only, so the core stays headless-testable.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The 9 emotion codes, in design order (core looks 1–9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmotionCode {
    Neutral,
    Happy,
    Listening,
    Thinking,
    Speaking,
    Surprised,
    Confused,
    Sleepy,
    Celebrating,
}

impl EmotionCode {
    /// All 9 codes, in design order.
    pub const ALL: [EmotionCode; 9] = [
        EmotionCode::Neutral,
        EmotionCode::Happy,
        EmotionCode::Listening,
        EmotionCode::Thinking,
        EmotionCode::Speaking,
        EmotionCode::Surprised,
        EmotionCode::Confused,
        EmotionCode::Sleepy,
        EmotionCode::Celebrating,
    ];

    /// Wire name: the exact string the model must send in the
    /// `code` argument of the `set_emotion` tool call.
    pub fn wire_name(self) -> &'static str {
        match self {
            EmotionCode::Neutral => "neutral",
            EmotionCode::Happy => "happy",
            EmotionCode::Listening => "listening",
            EmotionCode::Thinking => "thinking",
            EmotionCode::Speaking => "speaking",
            EmotionCode::Surprised => "surprised",
            EmotionCode::Confused => "confused",
            EmotionCode::Sleepy => "sleepy",
            EmotionCode::Celebrating => "celebrating",
        }
    }

    /// Short Vietnamese label (matches the UI face labels).
    pub fn label_vi(self) -> &'static str {
        match self {
            EmotionCode::Neutral => "Sẵn sàng",
            EmotionCode::Happy => "Vui",
            EmotionCode::Listening => "Đang nghe",
            EmotionCode::Thinking => "Đang nghĩ",
            EmotionCode::Speaking => "Đang nói",
            EmotionCode::Surprised => "Ngạc nhiên",
            EmotionCode::Confused => "Chưa hiểu",
            EmotionCode::Sleepy => "Buồn ngủ",
            EmotionCode::Celebrating => "Ăn mừng",
        }
    }

    /// Parse a wire name. Exact match, case-sensitive: the schema
    /// constrains the model to these 9 strings, and anything else
    /// is an unknown code that falls back, not a near-miss to
    /// rescue by guessing.
    pub fn parse(wire: &str) -> Option<Self> {
        match wire {
            "neutral" => Some(EmotionCode::Neutral),
            "happy" => Some(EmotionCode::Happy),
            "listening" => Some(EmotionCode::Listening),
            "thinking" => Some(EmotionCode::Thinking),
            "speaking" => Some(EmotionCode::Speaking),
            "surprised" => Some(EmotionCode::Surprised),
            "confused" => Some(EmotionCode::Confused),
            "sleepy" => Some(EmotionCode::Sleepy),
            "celebrating" => Some(EmotionCode::Celebrating),
            _ => None,
        }
    }
}

/// Outcome of handling one `set_emotion` tool call's arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmotionOutcome {
    /// The accepted code, when the arguments carried a known one.
    pub code: Option<EmotionCode>,
    /// True when the arguments did not carry a known code and the
    /// caller must fall back (keep the previous state).
    pub fallback: bool,
    /// The raw `code` value as received (for the audit record),
    /// truncated: the raw value is model output and must not be
    /// trusted to be short.
    pub raw: String,
}

/// Handle the parsed JSON arguments of one `set_emotion` tool
/// call. Reads ONLY `args["code"]` as a JSON string: a missing
/// field, a non-string value, or an unknown code all fall back.
/// No text is scanned, no regex runs, and the reply text is never
/// consulted — the structured channel is the whole protocol.
pub fn handle_emotion_args(args: &Value) -> EmotionOutcome {
    const MAX_RAW: usize = 80;
    let raw_value = args.get("code");
    let raw = match raw_value {
        Some(Value::String(text)) => text.chars().take(MAX_RAW).collect::<String>(),
        Some(other) => {
            let rendered = other.to_string();
            rendered.chars().take(MAX_RAW).collect::<String>()
        }
        None => String::new(),
    };
    match raw_value.and_then(Value::as_str) {
        Some(wire) => match EmotionCode::parse(wire) {
            Some(code) => EmotionOutcome {
                code: Some(code),
                fallback: false,
                raw,
            },
            None => EmotionOutcome {
                code: None,
                fallback: true,
                raw,
            },
        },
        None => EmotionOutcome {
            code: None,
            fallback: true,
            raw,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn all_nine_codes_round_trip_through_wire_names() {
        assert_eq!(EmotionCode::ALL.len(), 9);
        for code in EmotionCode::ALL {
            assert_eq!(EmotionCode::parse(code.wire_name()), Some(code));
            assert!(!code.label_vi().is_empty());
        }
        // Wire names are unique.
        let mut names: Vec<&str> = EmotionCode::ALL.iter().map(|c| c.wire_name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 9);
    }

    #[test]
    fn unknown_and_malformed_codes_fall_back() {
        // Unknown code.
        let outcome = handle_emotion_args(&json!({"code": "ecstatic"}));
        assert!(outcome.fallback);
        assert_eq!(outcome.code, None);
        assert_eq!(outcome.raw, "ecstatic");
        // Case-sensitive: near-misses do not rescue.
        let outcome = handle_emotion_args(&json!({"code": "Happy"}));
        assert!(outcome.fallback);
        // Missing field.
        let outcome = handle_emotion_args(&json!({}));
        assert!(outcome.fallback);
        assert_eq!(outcome.raw, "");
        // Non-string value.
        let outcome = handle_emotion_args(&json!({"code": 3}));
        assert!(outcome.fallback);
        assert_eq!(outcome.raw, "3");
        // Null.
        let outcome = handle_emotion_args(&json!({"code": null}));
        assert!(outcome.fallback);
        // Extra fields are ignored; only `code` is read.
        let outcome = handle_emotion_args(&json!({"code": "happy", "text": "surprised"}));
        assert!(!outcome.fallback);
        assert_eq!(outcome.code, Some(EmotionCode::Happy));
    }

    #[test]
    fn every_code_is_accepted_from_structured_args() {
        for code in EmotionCode::ALL {
            let outcome = handle_emotion_args(&json!({"code": code.wire_name()}));
            assert!(!outcome.fallback, "{code:?} must be accepted");
            assert_eq!(outcome.code, Some(code));
        }
    }

    #[test]
    fn raw_value_is_truncated_for_audit() {
        let long = "x".repeat(500);
        let outcome = handle_emotion_args(&json!({"code": long}));
        assert!(outcome.fallback);
        assert_eq!(outcome.raw.chars().count(), 80);
    }
}
