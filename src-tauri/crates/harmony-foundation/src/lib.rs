//! harmony-foundation — ZEB-548 Stage 1 (foundation-first).
//!
//! The project's rarely-changing, broadly-depended-on time/causality
//! primitives, extracted from `harmony-app` as a pure leaf crate:
//!
//! - [`clock_trust`] — the one auditable home for the bounded-time trust
//!   policy (ZEB-831).
//! - [`hlc_adopt_floor`] — the bounded causal adoption floor for HLC minting
//!   (ZEB-790).
//! - [`wall_clock_ms`] — the wall-clock source (ms since the Unix epoch).
//!
//! No I/O, no Tauri, no `harmony-*` deps, no back-reference to `harmony-app`.
//! `harmony-app` re-exports these modules so existing `crate::clock_trust::*` /
//! `crate::hlc_adopt_floor::*` / `crate::wall_clock_ms()` call sites resolve
//! unchanged.

pub mod clock_trust;
pub mod hlc_adopt_floor;

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
