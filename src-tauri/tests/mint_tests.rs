//! ZEB-442: consolidated mint test harness (ZEB-440 lever 3 — test-binary
//! consolidation).
//!
//! Scope: the **mint** domain — mint integration, owner lifecycle, and sync.
//!
//! Each former `tests/*.rs` here compiled as its own integration-test binary,
//! statically re-linking the whole `harmony-app` lib (the link-time multiplier
//! this ticket targets). They are now `#[path]`-included submodules of this one
//! harness binary. Files under a `tests/` subdirectory are NOT auto-compiled as
//! separate binaries, so each builds only via its `mod` declaration below.
//!
//! Pure move: original basenames are preserved (subdir moved, not renamed) so
//! cross-references from other tests and from production `src/` doc-comments stay
//! resolvable by name. nextest still runs every `#[test]` in its own process, so
//! per-test isolation (and any `#[serial]`/per-test stack-size) is unchanged.
//!
//! Run just this group: `cargo nextest run -E 'binary(mint_tests)'`.

#[path = "mint/mint_integration.rs"]
mod mint_integration;
#[path = "mint/mint_owner_lifecycle.rs"]
mod mint_owner_lifecycle;
#[path = "mint/mint_sync_integration.rs"]
mod mint_sync_integration;
