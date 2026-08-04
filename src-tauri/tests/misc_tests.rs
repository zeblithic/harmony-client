//! ZEB-442: consolidated misc test harness (ZEB-440 lever 3 — test-binary
//! consolidation).
//!
//! Scope: cross-cutting singletons that don't form their own domain — error-phrasing regression, IPC arg casing, and the heavy community open-join-cross-WAN + two-engine-presence integration tests.
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
//! Run just this group: `cargo nextest run -E 'binary(misc_tests)'`.

#[path = "misc/community_open_join_cross_wan_integration.rs"]
mod community_open_join_cross_wan_integration;
#[path = "misc/community_presence_two_engine_integration.rs"]
mod community_presence_two_engine_integration;
#[path = "misc/error_phrasing_regression.rs"]
mod error_phrasing_regression;
#[path = "misc/ipc_arg_casing.rs"]
mod ipc_arg_casing;
#[path = "misc/open_join_shed_acceptor_harness_integration.rs"]
mod open_join_shed_acceptor_harness_integration;
