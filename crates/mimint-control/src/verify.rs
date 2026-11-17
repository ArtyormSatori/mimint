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
//! - Only `body_*` tools are verified here; every other tool
//!   gets `None` (no opinion → status verification).
//! - `body_screenshot` itself is NOT re-screenshotted: its
//!   output already is a fresh frame's metadata. Verifying it
//!   means confirming the frame id in its output resolves in
//!   the store.
//! - A failed re-read (actor error, timeout) yields
//!   `(false, detail)` — recorded, never fatal, never
//!   retried. The turn continues; the model sees the failed
//!   verification in the reflection record.
//! - The detail carries metadata only: frame id, dimensions,
//!   hash. No pixels, ever.

use crate::actor::BodyActor;
use crate::frames::FrameStore;
use mimint_body::{ActOutcome, Action};
use mimint_core::{MimintError, StepVerifier};
use serde_json::{json, Value};
use std::sync::Arc;

/// Re-reads the screen after body tools.
pub struct BodyStepVerifier {
    actor: BodyActor,
    frames: Arc<FrameStore>,
}

impl BodyStepVerifier {
    pub fn new(actor: BodyActor, frames: Arc<FrameStore>) -> BodyStepVerifier {
        BodyStepVerifier { actor, frames }
    }
}

impl StepVerifier for BodyStepVerifier {
    fn verify(
        &self,
        tool: &str,
        _args: &Value,
        output: &Value,
    ) -> Result<Option<(bool, Value)>, MimintError> {
        let Some(action) = tool.strip_prefix("body_") else {
            return Ok(None);
        };
        if action == "screenshot" {
            // The output already names a fresh frame: verify
            // the frame resolves in the store.
            let id = output.get("frame_id").and_then(Value::as_u64);
            let resolves = id.is_some_and(|id| self.frames.meta(id).is_some());
            return Ok(Some((
                resolves,
                json!({
                    "re_read": "frame_resolves_in_store",
                    "frame_id": id,
                    "resolves": resolves,
                }),
            )));
        }
        // Any other body tool: re-read the screen now.
        match self.actor.act(Action::Screenshot) {
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
                let meta = self.frames.store(frame, true);
                Ok(Some((
                    true,
                    json!({
                        "re_read": "screenshot_after_action",
                        "action": action,
                        "frame_id": meta.id,
                        "width": meta.width,
                        "height": meta.height,
                        "sha256": meta.sha256,
                    }),
                )))
            }
            Ok(ActOutcome::Done) => Ok(Some((
                false,
                json!({"re_read": "screenshot_returned_no_frame", "action": action}),
            ))),
            Err(err) => Ok(Some((
                false,
                json!({
                    "re_read": "screenshot_failed",
                    "action": action,
                    "error": format!("{}: {}", err.code(), err.message()),
                }),
            ))),
        }
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
        assert_eq!(frames.len(), 1);
        // Verification frames are privacy-tainted.
        assert_eq!(frames.purge_tainted(), 1);
    }
}
