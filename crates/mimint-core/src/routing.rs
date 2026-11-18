//! GUI/code routing rules (Phase 6, §7, R1–R8).
//!
//! This module owns the enforceable parts of the routing rules:
//!
//! - **R6** (§7.3): the batch-loop guard. More than `N` identical
//!   body actions in a row in one turn ends the turn with the
//!   routing error `gui_batch_loop`. The fingerprint is
//!   `(tool_name, canonicalized_json_args)`; numbers compare by
//!   value (`1 == 1.0`); click/move coordinates are quantized to
//!   a 2px grid. Scroll actions are the exception: they only
//!   count as repeats when the frame hash after the scroll is
//!   unchanged. There is deliberately NO in-turn override path:
//!   the guard has no disable method, and the threshold can only
//!   change through configuration between turns.
//! - **R4** (§7.2): the verification status lattice and its
//!   weakest-wins aggregate.
//! - **R8** (§7.1): routing audit rule names + the body/code
//!   family classifier.
//!
//! R1/R2/R3 live in prompts, tool order, and guards (see
//! `docs/gui-code-routing.md`); R5 is enforced by the sandbox
//! network namespace, not by scanning code; R7 is a prompt rule.
//! None of those have a classifier here, and none is claimed to
//! be enforced by one.

use serde_json::Value;

/// The R8 rule for the first body/code tool of a turn.
pub const ROUTING_RULE_FIRST: &str = "r8_first";
/// The R8 rule for a body↔code family switch within a turn.
pub const ROUTING_RULE_SWITCH: &str = "r8_switch";

/// Which family a tool belongs to, for R8 routing audit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolFamily {
    /// `body_*` tools: act through the GUI.
    Body,
    /// `execute_code`: act through sandboxed code.
    Code,
    /// Everything else: does not change the tracked family.
    Other,
}

/// Classify a tool for R8. Only `body_*` and `execute_code`
/// participate; other tools leave the tracked family unchanged.
pub fn tool_family(tool: &str) -> ToolFamily {
    if tool == "execute_code" {
        ToolFamily::Code
    } else if tool.starts_with("body_") {
        ToolFamily::Body
    } else {
        ToolFamily::Other
    }
}

/// Canonical JSON: object keys sorted recursively, numbers
/// compared by value (`1 == 1.0` == `1e2`), strings escaped as
/// JSON. Two argument objects with the same meaning produce the
/// same string.
pub fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&canonical_number(n)),
        Value::String(s) => {
            // serde_json escaping is deterministic.
            out.push_str(&serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string()));
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            // serde_json::Map preserves insertion order only with
            // the `preserve_order` feature; sort keys explicitly
            // so canonical form never depends on that feature.
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(key).unwrap_or_else(|_| "\"\"".to_string()));
                out.push(':');
                write_canonical(&map[*key], out);
            }
            out.push('}');
        }
    }
}

/// Numbers compare by value: integral floats print as integers,
/// so `1`, `1.0`, and `1e2`/`100` canonicalize identically.
fn canonical_number(n: &serde_json::Number) -> String {
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    if let Some(u) = n.as_u64() {
        return u.to_string();
    }
    if let Some(f) = n.as_f64() {
        if f.is_finite() && f.fract() == 0.0 && f >= i64::MIN as f64 && f <= i64::MAX as f64 {
            return (f as i64).to_string();
        }
        // Shortest round-trip representation (ryu via serde_json).
        return n.to_string();
    }
    n.to_string()
}

/// Coordinate argument keys quantized to the 2px grid (§7.3).
/// Only top-level keys of a body tool's arguments are quantized.
const COORDINATE_KEYS: [&str; 6] = ["x", "y", "x2", "y2", "from_x", "from_y"];

/// Quantize a coordinate to the 2px grid: values in the same
/// 2px bucket share a fingerprint. (Boundary pairs such as
/// 101/102 land in adjacent buckets; the grid is the mechanism
/// the design names, and the 2px tolerance is approximate at
/// bucket edges.)
fn quantize_coordinate(value: f64) -> i64 {
    (value / 2.0).floor() as i64
}

/// The R6 fingerprint of one body action: the tool name plus the
/// canonicalized arguments, with coordinates quantized. Two
/// actions with the same fingerprint are "the same action" for
/// the batch-loop guard.
pub fn action_fingerprint(tool: &str, args: &Value) -> String {
    let mut canonical_args = args.clone();
    if tool.starts_with("body_") {
        if let Value::Object(map) = &mut canonical_args {
            for key in COORDINATE_KEYS {
                if let Some(Value::Number(n)) = map.get(key) {
                    if let Some(f) = n.as_f64() {
                        map.insert(
                            key.to_string(),
                            Value::Number(quantize_coordinate(f).into()),
                        );
                    }
                }
            }
        }
    }
    format!("{tool}\n{}", canonical_json(&canonical_args))
}

/// True when this action belongs to the scroll family (§7.3):
/// `body_scroll`, or `body_key` with `PageDown`, `Down`, or `Up`.
/// Note `PageUp` is deliberately NOT in the list: the design
/// names exactly these three keys.
fn is_scroll_family(tool: &str, args: &Value) -> bool {
    if tool == "body_scroll" {
        return true;
    }
    if tool == "body_key" {
        if let Some(key) = args.get("key").and_then(|k| k.as_str()) {
            return matches!(key, "PageDown" | "Down" | "Up");
        }
    }
    false
}

/// What the guard reports when it trips, for the audit record:
/// fingerprint + count + threshold + whether a frame-hash
/// comparison informed the streak (scroll case, §7.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchLoopTrip {
    pub tool: String,
    pub fingerprint: String,
    pub count: usize,
    pub threshold: usize,
    pub frame_hash_compared: bool,
}

#[derive(Debug, Clone)]
struct Streak {
    fingerprint: String,
    count: usize,
    scroll_family: bool,
    frame_hash_compared: bool,
}

/// The R6 batch-loop guard (§7.3). One guard per turn.
///
/// Counting discipline: `check` runs BEFORE an action executes;
/// `note_executed` runs AFTER it executes (both Ok and tool-error
/// outcomes count). A guard denial resets the streak, as does any
/// non-body tool. The threshold `N` means `N` actions run; the
/// (`N`+1)-th identical action trips the guard.
///
/// Scroll exception: counting is optimistic, and
/// `note_frame_hash` resets a scroll-family streak when the frame
/// hash after verification changed — scrolling through new
/// content is progress, not a loop. No new frame means normal
/// counting.
#[derive(Debug)]
pub struct BatchLoopGuard {
    threshold: usize,
    streak: Option<Streak>,
    last_frame_hash: Option<String>,
}

impl BatchLoopGuard {
    /// A guard with the given threshold (configured
    /// `gui_batch_threshold`, 2..=50; values below 2 are treated
    /// as 2 — a threshold of 1 would trip on the first repeat,
    /// which no configuration may express).
    pub fn new(threshold: usize) -> Self {
        Self {
            threshold: threshold.max(2),
            streak: None,
            last_frame_hash: None,
        }
    }

    /// The configured threshold.
    pub fn threshold(&self) -> usize {
        self.threshold
    }

    /// Length of the current identical-action streak (0 when no
    /// streak is open).
    pub fn streak_count(&self) -> usize {
        self.streak.as_ref().map(|s| s.count).unwrap_or(0)
    }

    /// Check before executing a body action. Trips when this
    /// would be the (`threshold`+1)-th identical action in a row.
    /// Non-body tools never trip (they reset via
    /// `note_other_tool` instead).
    pub fn check(&self, tool: &str, args: &Value) -> Result<(), BatchLoopTrip> {
        if !tool.starts_with("body_") {
            return Ok(());
        }
        if let Some(streak) = &self.streak {
            let fingerprint = action_fingerprint(tool, args);
            if streak.fingerprint == fingerprint && streak.count >= self.threshold {
                return Err(BatchLoopTrip {
                    tool: tool.to_string(),
                    fingerprint,
                    count: streak.count,
                    threshold: self.threshold,
                    frame_hash_compared: streak.frame_hash_compared,
                });
            }
        }
        Ok(())
    }

    /// Record that a body action executed (success or tool
    /// error — both count toward a loop).
    pub fn note_executed(&mut self, tool: &str, args: &Value) {
        if !tool.starts_with("body_") {
            self.streak = None;
            return;
        }
        let fingerprint = action_fingerprint(tool, args);
        match &mut self.streak {
            Some(streak) if streak.fingerprint == fingerprint => {
                streak.count += 1;
            }
            _ => {
                self.streak = Some(Streak {
                    fingerprint,
                    count: 1,
                    scroll_family: is_scroll_family(tool, args),
                    frame_hash_compared: false,
                });
            }
        }
    }

    /// A guard denial breaks the streak: a denied action is not
    /// a loop iteration.
    pub fn note_denied(&mut self) {
        self.streak = None;
    }

    /// Any non-body tool breaks the streak.
    pub fn note_other_tool(&mut self) {
        self.streak = None;
    }

    /// Feed the frame hash observed after verification. When the
    /// current streak is scroll-family and the hash CHANGED since
    /// the last observation, the streak resets: the scrolls are
    /// moving through new content. An unchanged hash (or no new
    /// frame, `None`) keeps counting normally.
    pub fn note_frame_hash(&mut self, hash: Option<&str>) {
        let Some(hash) = hash else {
            return;
        };
        if let Some(streak) = &mut self.streak {
            if streak.scroll_family {
                if let Some(previous) = &self.last_frame_hash {
                    streak.frame_hash_compared = true;
                    if previous != hash {
                        self.streak = None;
                    }
                }
            }
        }
        self.last_frame_hash = Some(hash.to_string());
    }
}

/// Verification status of one step (R4, §7.2), and the aggregate
/// status of a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum VerificationStatus {
    /// Verified by looking at the GUI after the step.
    VerifiedGui,
    /// Verified by re-reading the code step's output artifact.
    /// This is NOT "verified by GUI" and must never be labelled
    /// as such (§7.2).
    VerifiedArtifact,
    /// No verification was performed or none was possible to
    /// confirm.
    Unverified,
    /// Verification ran and the step did not hold.
    Failed,
    /// No body stack and no artifact: verification unavailable.
    Unavailable,
}

impl VerificationStatus {
    /// True for both verified variants.
    pub fn is_verified(&self) -> bool {
        matches!(self, Self::VerifiedGui | Self::VerifiedArtifact)
    }

    /// The §15.5 badge label. The artifact label deliberately
    /// differs from the GUI label.
    pub fn badge_label(&self) -> &'static str {
        match self {
            Self::VerifiedGui => "Đã xác minh bằng giao diện",
            Self::VerifiedArtifact => "Đã đọc lại artifact",
            Self::Unverified => "Chưa xác minh",
            Self::Failed => "Xác minh thất bại",
            Self::Unavailable => "Xác minh không khả dụng",
        }
    }

    /// Aggregate a turn's step statuses (§7.2). `None` when no
    /// step needed verification (no badge). Weakest wins:
    /// Failed > Unverified > Unavailable > Verified; among
    /// verified steps, Gui wins over Artifact when any GUI
    /// verification happened.
    pub fn aggregate(statuses: &[VerificationStatus]) -> Option<VerificationStatus> {
        if statuses.is_empty() {
            return None;
        }
        if statuses.contains(&Self::Failed) {
            return Some(Self::Failed);
        }
        if statuses.contains(&Self::Unverified) {
            return Some(Self::Unverified);
        }
        if statuses.contains(&Self::Unavailable) {
            return Some(Self::Unavailable);
        }
        if statuses.contains(&Self::VerifiedGui) {
            return Some(Self::VerifiedGui);
        }
        Some(Self::VerifiedArtifact)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn canonical_json_sorts_keys_and_unifies_numbers() {
        let a = json!({"b": 1, "a": [1.0, 2]});
        let b = json!({"a": [1, 2.0], "b": 1.0});
        assert_eq!(canonical_json(&a), canonical_json(&b));
        assert_eq!(canonical_json(&json!(1e2)), "100");
        assert_eq!(canonical_json(&json!({"x": 1.5})), "{\"x\":1.5}");
    }

    #[test]
    fn fingerprint_quantizes_coordinates_to_the_2px_grid() {
        let a = action_fingerprint("body_click", &json!({"x": 100, "y": 200}));
        let b = action_fingerprint("body_click", &json!({"x": 101, "y": 201}));
        assert_eq!(a, b, "within one 2px bucket");
        let c = action_fingerprint("body_click", &json!({"x": 104, "y": 200}));
        assert_ne!(a, c);
        // Non-body tools do not quantize.
        let d = action_fingerprint("other", &json!({"x": 100}));
        let e = action_fingerprint("other", &json!({"x": 101}));
        assert_ne!(d, e);
    }

    #[test]
    fn guard_trips_on_the_n_plus_first_identical_action() {
        let mut guard = BatchLoopGuard::new(5);
        let args = json!({"x": 10, "y": 20});
        for _ in 0..5 {
            assert!(guard.check("body_click", &args).is_ok());
            guard.note_executed("body_click", &args);
        }
        let trip = guard
            .check("body_click", &args)
            .expect_err("6th identical action must trip");
        assert_eq!(trip.count, 5);
        assert_eq!(trip.threshold, 5);
        assert_eq!(trip.tool, "body_click");
    }

    #[test]
    fn guard_resets_on_different_action_denial_and_other_tool() {
        let mut guard = BatchLoopGuard::new(2);
        let args = json!({"x": 10, "y": 20});
        guard.note_executed("body_click", &args);
        guard.note_executed("body_click", &json!({"x": 30, "y": 20}));
        assert_eq!(guard.streak_count(), 1);
        guard.note_executed("body_click", &json!({"x": 30, "y": 20}));
        guard.note_denied();
        assert_eq!(guard.streak_count(), 0);
        guard.note_executed("body_click", &args);
        guard.note_other_tool();
        assert_eq!(guard.streak_count(), 0);
    }

    #[test]
    fn scroll_streak_resets_when_the_frame_hash_changes() {
        let mut guard = BatchLoopGuard::new(2);
        let args = json!({"direction": "down"});
        guard.note_executed("body_scroll", &args);
        guard.note_frame_hash(Some("hash-a"));
        guard.note_executed("body_scroll", &args);
        // New content: the streak resets instead of tripping.
        guard.note_frame_hash(Some("hash-b"));
        assert_eq!(guard.streak_count(), 0);
        assert!(guard.check("body_scroll", &args).is_ok());
    }

    #[test]
    fn scroll_streak_trips_when_the_frame_hash_is_unchanged() {
        let mut guard = BatchLoopGuard::new(2);
        let args = json!({"direction": "down"});
        guard.note_executed("body_scroll", &args);
        guard.note_frame_hash(Some("hash-a"));
        guard.note_executed("body_scroll", &args);
        guard.note_frame_hash(Some("hash-a"));
        let trip = guard
            .check("body_scroll", &args)
            .expect_err("unchanged frame means the scroll loops");
        assert!(trip.frame_hash_compared);
    }

    #[test]
    fn scroll_exception_does_not_apply_to_clicks() {
        let mut guard = BatchLoopGuard::new(2);
        let args = json!({"x": 10, "y": 20});
        guard.note_executed("body_click", &args);
        guard.note_frame_hash(Some("hash-a"));
        guard.note_executed("body_click", &args);
        guard.note_frame_hash(Some("hash-b"));
        assert_eq!(guard.streak_count(), 2, "clicks keep counting");
        assert!(guard.check("body_click", &args).is_err());
    }

    #[test]
    fn page_up_is_not_in_the_scroll_family() {
        assert!(is_scroll_family("body_key", &json!({"key": "PageDown"})));
        assert!(is_scroll_family("body_key", &json!({"key": "Down"})));
        assert!(is_scroll_family("body_key", &json!({"key": "Up"})));
        assert!(!is_scroll_family("body_key", &json!({"key": "PageUp"})));
        assert!(!is_scroll_family("body_key", &json!({"key": "Enter"})));
    }

    #[test]
    fn verification_aggregate_is_weakest_wins() {
        use VerificationStatus::*;
        assert_eq!(VerificationStatus::aggregate(&[]), None);
        assert_eq!(
            VerificationStatus::aggregate(&[VerifiedArtifact, VerifiedGui]),
            Some(VerifiedGui)
        );
        assert_eq!(
            VerificationStatus::aggregate(&[VerifiedGui, Unavailable]),
            Some(Unavailable)
        );
        assert_eq!(
            VerificationStatus::aggregate(&[VerifiedGui, Unverified]),
            Some(Unverified)
        );
        assert_eq!(
            VerificationStatus::aggregate(&[Unverified, Failed]),
            Some(Failed)
        );
        assert_eq!(
            VerificationStatus::aggregate(&[VerifiedArtifact]),
            Some(VerifiedArtifact)
        );
    }

    /// §15.6 `ui_verification_badges` (F6D-14): the five badge
    /// values are pairwise distinct, the GUI and artifact
    /// labels differ (an artifact re-read is never labelled a
    /// GUI verification), and only the two verified values
    /// count as verified — the "Completed" display state is
    /// built on `is_verified`, never on the absence of a badge.
    #[test]
    fn ui_verification_badges() {
        use VerificationStatus::*;
        let labels = [
            VerifiedGui.badge_label(),
            VerifiedArtifact.badge_label(),
            Unverified.badge_label(),
            Failed.badge_label(),
            Unavailable.badge_label(),
        ];
        assert_eq!(labels[0], "Đã xác minh bằng giao diện");
        assert_eq!(labels[1], "Đã đọc lại artifact");
        assert_eq!(labels[2], "Chưa xác minh");
        assert_eq!(labels[3], "Xác minh thất bại");
        assert_eq!(labels[4], "Xác minh không khả dụng");
        let mut sorted = labels.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 5, "badge labels must be pairwise distinct");
        assert!(VerifiedGui.is_verified());
        assert!(VerifiedArtifact.is_verified());
        assert!(!Unverified.is_verified());
        assert!(!Failed.is_verified());
        assert!(!Unavailable.is_verified());
        // No badge at all when nothing needed verification:
        // the UI shows no badge rather than "unverified".
        assert_eq!(VerificationStatus::aggregate(&[]), None);
    }

    #[test]
    fn tool_family_classification() {
        assert_eq!(tool_family("body_click"), ToolFamily::Body);
        assert_eq!(tool_family("body_screenshot"), ToolFamily::Body);
        assert_eq!(tool_family("execute_code"), ToolFamily::Code);
        assert_eq!(tool_family("notebook_add"), ToolFamily::Other);
        assert_eq!(tool_family("delegate"), ToolFamily::Other);
    }

    /// Gate (§7.4): there is no in-turn override path for the R6
    /// guard. The guard type exposes no disable/override/bypass
    /// method — this test reads the module source and fails if
    /// such a method ever appears.
    #[test]
    fn r6_no_in_turn_override() {
        let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let source = std::fs::read_to_string(manifest.join("src").join("routing.rs"))
            .expect("read routing.rs");
        // Only the production code counts: the test module below
        // legitimately names the forbidden words.
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("test module marker");
        for line in production.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("pub fn") || trimmed.starts_with("fn ") {
                for forbidden in ["override", "bypass", "disable", "skip_guard"] {
                    assert!(
                        !trimmed.contains(forbidden),
                        "R6 guard must not expose an override method: {trimmed}"
                    );
                }
            }
        }
        // And the guard struct itself carries no enable/disable flag.
        assert!(!production.contains("enabled: bool"));
    }
}
