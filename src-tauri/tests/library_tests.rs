//! ZEB-442: consolidated library test harness (ZEB-440 lever 3 — test-binary
//! consolidation).
//!
//! Scope: the **library / CAS / butler** domain — library announce + directory, two-node CAS serve, and butler deposit + out-hold.
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
//! Run just this group: `cargo nextest run -E 'binary(library_tests)'`.

#[path = "common/mod.rs"]
mod common;

#[path = "library/butler_deposit_integration.rs"]
mod butler_deposit_integration;
#[path = "library/butler_outhold_integration.rs"]
mod butler_outhold_integration;
#[path = "library/cas_serve_two_node_integration.rs"]
mod cas_serve_two_node_integration;
#[path = "library/library_announce_integration.rs"]
mod library_announce_integration;
#[path = "library/library_directory_integration.rs"]
mod library_directory_integration;
