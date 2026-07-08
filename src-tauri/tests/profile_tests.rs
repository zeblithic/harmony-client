//! ZEB-442: consolidated profile test harness (ZEB-440 lever 3 — test-binary
//! consolidation).
//!
//! Scope: the **profile** domain — profile broadcast, cross-peer profile cards (avatar + page), and profile isolation.
//!
//! Each former `tests/*.rs` here compiled as its own integration-test binary,
//! statically re-linking the whole `harmony-app` lib (the link-time multiplier
//! this ticket targets). They are now `#[path]`-included submodules of this one
//! harness binary. Files under a `tests/` subdirectory are NOT auto-compiled as
//! separate binaries, so each builds only via its `mod` declaration below.
//!
//! The shared `tests/common/` helper is declared **once** here as `mod common`;
//! the former per-file `mod common;` becomes `use crate::common;` (loading the
//! same file as a module more than once per binary is a rustc error). Original
//! basenames are preserved so cross-references stay resolvable by name. nextest
//! runs every `#[test]` in its own process, so `#[serial]` isolation is unchanged.
//!
//! Run just this group: `cargo nextest run -E 'binary(profile_tests)'`.

#[path = "common/mod.rs"]
mod common;

#[path = "profile/profile_broadcast_integration.rs"]
mod profile_broadcast_integration;
#[path = "profile/profile_card_avatar_cross_peer_integration.rs"]
mod profile_card_avatar_cross_peer_integration;
#[path = "profile/profile_card_cross_peer_integration.rs"]
mod profile_card_cross_peer_integration;
#[path = "profile/profile_isolation.rs"]
mod profile_isolation;
#[path = "profile/profile_page_cross_peer_integration.rs"]
mod profile_page_cross_peer_integration;
