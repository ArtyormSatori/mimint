//! Mimint body contract (Phase 3): one action API for every OS.
//!
//! Clean-room implementation written from scratch for the Mimint
//! project. The contract follows the ACI pattern from the Agent S
//! research (mechanism only, no copied code): a single action surface
//! generated from the implementation, one implementation per OS, and
//! one conformance suite every body must pass.
//!
//! - [`Body`]: the trait every OS body implements.
//! - [`conformance::run_suite`]: the suite that grades a body.
//! - [`MockBody`]: deterministic in-memory body, for tests anywhere.
//! - [`X11Body`]: Linux/X11 reference body (contract verification
//!   only; the Linux product body is Wayland-portal-first, Phase 4).
//! - [`MacOsBody`]: macOS body (macOS only; runtime verification
//!   needs the user's Mac — see `mac-verify/` in the repo root).

pub mod action;
pub mod body;
pub mod conformance;
pub mod error;
pub mod file_oracle;
#[cfg(target_os = "macos")]
pub mod macos;
pub mod mock;
#[cfg(target_os = "linux")]
pub mod x11;

pub use action::{action_specs, Action, ActionSpec, Key};
pub use body::{write_ppm, ActOutcome, Body, Framebuffer, PermissionState, Permissions};
pub use conformance::{run_suite, CheckResult, SuiteReport, TargetOracle, TYPE_STRINGS};
pub use error::BodyError;
pub use file_oracle::FileOracle;
#[cfg(target_os = "macos")]
pub use macos::MacOsBody;
pub use mock::{Fault, MockBody, MockEvent, MockOracle};
#[cfg(target_os = "linux")]
pub use x11::X11Body;
