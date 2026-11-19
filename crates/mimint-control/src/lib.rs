//! Body actions as brain tools (Phase 4, P4.1/P4.6).
//!
//! This crate sits between mimint-core (the brain, body-free)
//! and mimint-body (the bodies, dependency-light):
//!
//! - [`actor::BodyActor`]: one dedicated thread owns the body;
//!   per-action timeout with fail-fast on expiry; stop-latch
//!   checks before every action.
//! - [`frames::FrameStore`]: pixels live here, never in tool
//!   output; Privacy-Mode taint + purge.
//! - [`tools`]: one `body_<action>` tool per action, schemas
//!   generated from [`mimint_body::action_specs`] (ACI).
//! - [`verify::BodyStepVerifier`]: the brain's verify-previous
//!   hook, re-reading the screen after body tools.
//! - [`actor::bridge`]: StopFlag → CancelToken bridging, so a
//!   body-level stop aborts the driving turn.

pub mod actor;
pub mod driver;
pub mod frames;
pub mod slots;
pub mod tools;
pub mod verify;

pub use actor::{bridge, ActorConfig, BodyActor, DEFAULT_ACTION_TIMEOUT, PROBE_QUEUE_SLA};
pub use driver::ActorDriverBackend;
pub use frames::{encode_png, hex_encode, sha256_hex, FrameMeta, FrameStore};
pub use slots::AgentSnapshotSlots;
pub use tools::{
    register_body_tools, register_body_tools_configured, register_body_tools_for_agent, BodyTool,
    NamedBodyTool, PrivacyFlag, DEFAULT_MAX_OBSERVATION_AGE,
};
pub use verify::BodyStepVerifier;
