//! The body step verifier: verify-previous-step, re-reading
//! state (P4.1, Agent S).
//!
//! After a body tool executes, the brain's mandatory
//! verify-previous phase asks this verifier to re-read the
//! screen: it takes a fresh screenshot through the actor and
//! records the new frame's metadata (id, dimensions, hash) in
//! the verification audit record. The verification is
//! OBSERVATIONAL, not judgmental: it proves the screen was
//! re-read after the action, and it gives the model (via the
//! reflection record) the fresh frame's identity to reason
//! about. It does NOT decide whether the action achieved the
//! user's goal — that judgment belongs to the model reading
//! the fresh frame, and to the golden-task checker for
//! scripted gates.
//!
//! Rules:
//! - Only `body_*` tools and `execute_code` are verified here;
//!   every other tool gets `None` (no opinion → status
//!   verification).
//! - `body_screenshot` itself is NOT re-screenshotted: its
//!   output already is a fresh frame's metadata. Verifying it
//!   means confirming the frame id in its output resolves in
//!   the store.
//! - `execute_code` (Phase 6, R4) is verified by GUI when a
//!   body stack exists: a fresh screenshot after the code
//!   ran, with `verification_method: "gui"` in the detail.
//!   Without a body stack the brain falls back to artifact
//!   re-reading; this verifier never does that itself.
//! - A failed re-read (actor error, timeout) yields
//!   `(false, detail)` — recorded, never fatal, never
//!   retried. The turn continues; the model sees the failed
//!   verification in the reflection record.
//! - The detail carries metadata only: frame id, dimensions,
//!   hash. No pixels, ever. The `frame_hash` key feeds the
//!   R6 scroll exception in the brain (a changed hash after
//!   a scroll proves the screen moved).
//! - Each verifier belongs to one agent (Phase 6, §3): its
//!   screenshots join that agent's actor queue and its
//!   frames are attributed to that agent.

use crate::actor::BodyActor;
use crate::frames::FrameStore;
use mimint_body::{ActOutcome, Action};
use mimint_core::{MimintError, StepVerifier, AGENT_DEFAULT};
use serde_json::{json, Value};
use std::sync::Arc;

/// Re-reads the screen after body tools.
pub struct BodyStepVerifier {
    actor: BodyActor,
    frames: Arc<FrameStore>,
    agent_id: String,
}

impl BodyStepVerifier {
    /// Verifier for the default agent (single-agent path).
    pub fn new(actor: BodyActor, frames: Arc<FrameStore>) -> BodyStepVerifier {
        Self::new_for(AGENT_DEFAULT, actor, frames)
    }

    /// Verifier for one agent: its screenshots join the
    /// agent's actor queue and its frames carry the agent's
    /// id.
    pub fn new_for(agent_id: &str, actor: BodyActor, frames: Arc<FrameStore>) -> BodyStepVerifier {
        BodyStepVerifier {
            actor,
            frames,
            agent_id: agent_id.to_string(),
        }
    }

    /// Take a fresh screenshot through the actor and store
    /// it (privacy-tainted, attributed to this verifier's
    /// agent). Returns the verification verdict: a fresh
    /// frame proves the re-read happened.
    fn screenshot_after(&self, action: &str, method: &str) -> Option<(bool, Value)> {
        match self.actor.act_for(&self.agent_id, Action::Screenshot) {
            Ok(ActOutcome::Frame(frame)) => {
                // The verification frame is stored WITHOUT a
                // privacy taint decision of its own: it is a
                // verification artifact, and the store's purge
                // discipline treats untainted frames as
                // purgeable only by purge_all. To stay
                // conservative, verification frames are stored
                // as privacy-tainted: they are purged with the
                // next tainted purge, and they never outlive
                // the turn's privacy context.
                let meta = self.frames.store_for(&self.agent_id, frame, true);
                Some((
                    true,
                    json!({
                        "re_read": "screenshot_after_action",
                        "verification_method": method,
                        "action": action,
                        "frame_id": meta.id,
                        "width": meta.width,
                        "height": meta.height,
                        "sha256": meta.sha256,
                        "frame_hash": meta.sha256,
                    }),
                ))
            }
            Ok(ActOutcome::Done) => Some((
                false,
                json!({
                    "re_read": "screenshot_returned_no_frame",
                    "verification_method": method,
                    "action": action,
                }),
            )),
            Err(err) => Some((
                false,
                json!({
                    "re_read": "screenshot_failed",
                    "verification_method": method,
                    "action": action,
                    "error": format!("{}: {}", err.code(), err.message()),
                }),
            )),
        }
    }
}

impl StepVerifier for BodyStepVerifier {
    fn verify(
        &self,
        tool: &str,
        _args: &Value,
        output: &Value,
    ) -> Result<Option<(bool, Value)>, MimintError> {
        if tool == "execute_code" {
            // R4 GUI verification: re-read the screen after
            // the code ran. The brain maps a `true` verdict
            // here to Verified(gui).
            return Ok(self.screenshot_after("execute_code", "gui"));
        }
        let Some(action) = tool.strip_prefix("body_") else {
            return Ok(None);
        };
        if action == "screenshot" {
            // The output already names a fresh frame: verify
            // the frame resolves in the store, and report
            // its hash for the R6 scroll exception.
            let id = output.get("frame_id").and_then(Value::as_u64);
            let meta = id.and_then(|id| self.frames.meta(id));
            let resolves = meta.is_some();
            let frame_hash = meta.map(|m| m.sha256);
            return Ok(Some((
                resolves,
                json!({
                    "re_read": "frame_resolves_in_store",
                    "frame_id": id,
                    "resolves": resolves,
                    "frame_hash": frame_hash,
                }),
            )));
        }
        // Any other body tool: re-read the screen now.
        Ok(self.screenshot_after(action, "gui"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frames::FrameStore;
    use mimint_body::{MockBody, StopFlag};

    fn verifier() -> (BodyStepVerifier, Arc<FrameStore>) {
        let actor = BodyActor::spawn(Box::new(MockBody::new()), StopFlag::new());
        let frames = Arc::new(FrameStore::new());
        (BodyStepVerifier::new(actor, Arc::clone(&frames)), frames)
    }

    #[test]
    fn non_body_tools_get_no_opinion() {
        let (verifier, _) = verifier();
        let out = verifier
            .verify("notebook_search", &json!({}), &json!({}))
            .unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn screenshot_output_is_verified_by_store_resolution() {
        let (verifier, frames) = verifier();
        let frame = mimint_body::Framebuffer::new(8, 8, vec![0u8; 8 * 8 * 4]).unwrap();
        let meta = frames.store(frame, false);
        let out = verifier
            .verify("body_screenshot", &json!({}), &json!({"frame_id": meta.id}))
            .unwrap()
            .expect("a verdict");
        assert!(out.0);
        assert_eq!(out.1["re_read"], "frame_resolves_in_store");
        assert_eq!(out.1["frame_hash"], meta.sha256);
        // An unknown frame id fails verification.
        let out = verifier
            .verify("body_screenshot", &json!({}), &json!({"frame_id": 999}))
            .unwrap()
            .expect("a verdict");
        assert!(!out.0);
    }

    #[test]
    fn input_tools_trigger_a_fresh_screenshot() {
        let (verifier, frames) = verifier();
        assert_eq!(frames.len(), 0);
        let out = verifier
            .verify("body_click", &json!({"x": 1, "y": 1}), &json!({"ok": true}))
            .unwrap()
            .expect("a verdict");
        assert!(out.0);
        assert_eq!(out.1["re_read"], "screenshot_after_action");
        assert_eq!(out.1["action"], "click");
        assert_eq!(out.1["frame_hash"].as_str().unwrap().len(), 64);
        assert_eq!(frames.len(), 1);
        // Verification frames are privacy-tainted.
        assert_eq!(frames.purge_tainted(), 1);
    }

    #[test]
    fn execute_code_is_verified_by_gui_with_method_and_hash() {
        let (verifier, frames) = verifier();
        let out = verifier
            .verify(
                "execute_code",
                &json!({"language": "sh", "code": "true"}),
                &json!({"exit_code": 0, "stdout": "", "stderr": ""}),
            )
            .unwrap()
            .expect("a verdict");
        assert!(out.0);
        assert_eq!(out.1["verification_method"], "gui");
        assert_eq!(out.1["action"], "execute_code");
        assert_eq!(out.1["frame_hash"].as_str().unwrap().len(), 64);
        assert_eq!(frames.len(), 1);
    }

    #[test]
    fn verifier_attributes_frames_to_its_agent() {
        let actor = BodyActor::spawn(Box::new(MockBody::new()), StopFlag::new());
        let frames = Arc::new(FrameStore::new());
        let verifier = BodyStepVerifier::new_for("agent-002", actor, Arc::clone(&frames));
        let out = verifier
            .verify("body_click", &json!({"x": 1, "y": 1}), &json!({"ok": true}))
            .unwrap()
            .expect("a verdict");
        assert!(out.0);
        let id = out.1["frame_id"].as_u64().unwrap();
        assert_eq!(frames.meta(id).unwrap().agent_id, "agent-002");
    }
}
