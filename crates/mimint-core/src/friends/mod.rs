//! Friends: pairing, E2E sync, hub, cross-device approval,
//! handoff, and ESP32 bodies (Phase 7).
//!
//! The design is `docs/phase7-design.md` (signed off). This
//! module tree is the implementation: one cipher suite, no new
//! dependencies, deny-by-default everywhere.

pub mod address;
pub mod approval;
pub mod bench;
pub mod conflict;
pub mod context;
pub mod crypto;
pub mod esp32;
pub mod handoff;
pub mod hub;
pub mod identity;
pub mod ota;
pub mod pairing;
pub mod records;
pub mod registry;
pub mod servo;
pub mod session;
pub mod sync;
pub mod texts;
pub mod todos;
pub mod util;
pub mod wire;
pub mod ws;
