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
//! - [`conformance::run_guarded_suite`]: the guarded checks
//!   (self-exclusion zone, focus verification) for wrapped bodies.
//! - [`MockBody`]: deterministic in-memory body, for tests anywhere.
//! - [`X11Body`]: Linux/X11 reference body (contract verification
//!   only; the Linux product body is Wayland-portal-first, Phase 4).
//! - [`MacOsBody`]: macOS body (macOS only; runtime verification
//!   needs the user's Mac — see `mac-verify/` in the repo root).
//! - [`GuardedBody`]: the body-level safety wrapper (exclusion zone,
//!   rate limits, stop latch, focus delegation).

pub mod action;
pub mod android;
pub mod body;
pub mod captures;
pub mod conformance;
pub mod error;
pub mod file_oracle;
pub mod forbidden;
pub mod guarded;
pub mod hotkey;
#[cfg(target_os = "macos")]
pub mod macos;
pub mod mock;
pub mod wayland;
#[cfg(target_os = "windows")]
pub mod windows;
#[cfg(target_os = "linux")]
pub mod x11;

pub use action::{action_specs, Action, ActionSpec, Key, MAX_HOLD_MILLIS};
pub use android::{
    fingerprint_of_cert_der, AndroidBody, AndroidConfig, FakeAndroidOracle, FakeAndroidServer,
    FakeServerConfig, ProtocolClient, Role, TlsMode, PROTOCOL_VERSION,
};
pub use body::{
    write_ppm, ActOutcome, Body, Framebuffer, FramebufferPool, PermissionState, Permissions,
    PoolStats, ScreenRect, TargetProbe, TargetSnapshot, TargetSnapshotSlot, WindowIdentity,
    REVERIFY_GRID_MAX_MEAN_DIFF,
};
pub use captures::{
    append_audit_line, bodyctl_audit_line, captures_dir, ensure_captures_dir, shot_path_allowed,
};
pub use conformance::{
    check_permission_denied, run_guarded_suite, run_suite, CheckResult, SuiteReport, TargetOracle,
    PATTERN_VARIANT, TYPE_STRINGS, UNICODE_STRINGS,
};
pub use error::BodyError;
pub use file_oracle::FileOracle;
pub use forbidden::{
    check_forbidden_signals, check_forbidden_window, is_forbidden_signals, is_forbidden_window,
    WindowSignals,
};
pub use guarded::{GuardedBody, RateLimits, StopFlag, StopFlagCell, TripInfo};
pub use hotkey::{chord_matches, grab_modifier_combinations, CHORD_MODIFIERS};
#[cfg(target_os = "linux")]
pub use hotkey::{HotkeyHealth, HotkeyListener};
#[cfg(target_os = "macos")]
pub use macos::MacOsBody;
pub use mock::{Fault, MockBody, MockEvent, MockOracle, MockWindow};
pub use wayland::{
    MockPortal, PortalClient, PortalSession, SessionKinds, SessionTokenStore, WaylandBody,
    WaylandOracle,
};
#[cfg(target_os = "windows")]
pub use windows::WindowsBody;
#[cfg(target_os = "linux")]
pub use x11::X11Body;
