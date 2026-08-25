//! harmony-foundation — ZEB-548 Stage 1 (foundation-first).
//!
//! The project's rarely-changing, broadly-depended-on primitives, extracted
//! from `harmony-app` as a leaf crate so every higher tier depends on them
//! downward instead of reaching sideways into a peer module:
//!
//! - [`clock_trust`] — the one auditable home for the bounded-time trust
//!   policy (ZEB-831).
//! - [`hlc_adopt_floor`] — the bounded causal adoption floor for HLC minting
//!   (ZEB-790).
//! - [`wall_clock_ms`] — the wall-clock source (ms since the Unix epoch).
//! - [`persist::save_atomically`] — durable atomic file replacement, shared by
//!   9 call sites across the owner-fleet, community, identity-crypto, and app
//!   tiers (ZEB-548 Stage 1).
//! - [`profile`] — process-global named-profile selection (ZEB-446).
//!
//! No Tauri, no `harmony-*` deps, no back-reference to `harmony-app`. The
//! durable-write primitive makes this not strictly I/O-free, but it stays a
//! pure leaf at the bottom of the crate DAG, alongside `harmony-core-types`.
//! `harmony-app` re-exports these items so existing `crate::clock_trust::*` /
//! `crate::wall_clock_ms()` / `crate::profile::*` /
//! `crate::owner_state_persist::save_atomically` call sites resolve unchanged.

pub mod clock_trust;
pub mod hlc_adopt_floor;
pub mod persist;
pub mod profile;

pub use persist::save_atomically;

/// Wall-clock milliseconds since the Unix epoch.
///
/// A monotonic clock this is not — it reflects the host wall clock and can move
/// backward across NTP steps. Causal ordering must go through the HLC/`clock_trust`
/// machinery; this is only the raw wall-time reading those policies gate.
pub fn wall_clock_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
