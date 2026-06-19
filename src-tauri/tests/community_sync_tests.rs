//! ZEB-442: consolidated community state-sync harness (PR 2 of the test-binary
//! consolidation — ZEB-440 lever 3).
//!
//! The 10 former `tests/community_{sync,state,root_hlc,hlc_race,fork,open_flow}_*.rs`
//! top-level files each compiled as a separate integration-test binary, every one
//! statically re-linking the whole `harmony-app` lib (the link-time multiplier this
//! ticket targets). They are now `#[path]`-included submodules of this single harness
//! binary. Files under a `tests/` subdirectory are NOT auto-compiled as separate
//! binaries, so each only builds via its `mod` declaration below.
//!
//! Scope of this group: the community **state-synchronization** domain — the CRDT
//! state types, the sync engine/registry, HLC ordering, fork handling, and the
//! open-community catch-up flow. Voting, channel, and the remaining community tests
//! land in their own sibling harnesses in later PRs.
//!
//! Filenames keep their original `community_*` basenames (the subdir was moved, not
//! renamed) so the many cross-references to these tests from other test files and
//! from production `src/` doc-comments stay resolvable by name.
//!
//! nextest still runs every `#[test]` in its own process, so the per-test isolation
//! these tests assume is unchanged. None of them mutate process-global state (no
//! env-var writes, no `set_current_dir`, no real keychain) — isolation comes from
//! per-test tempdirs and in-process engine instances — so none use `#[serial]` and
//! merging them into one binary changes nothing observable.
//!
//! Run just this group: `cargo nextest run -E 'binary(community_sync_tests)'`.

#[path = "community_sync/community_fork_integration.rs"]
mod community_fork_integration;
#[path = "community_sync/community_hlc_race_integration.rs"]
mod community_hlc_race_integration;
#[path = "community_sync/community_open_flow_integration.rs"]
mod community_open_flow_integration;
#[path = "community_sync/community_root_hlc_tracker_unit.rs"]
mod community_root_hlc_tracker_unit;
#[path = "community_sync/community_state_crdt_unit.rs"]
mod community_state_crdt_unit;
#[path = "community_sync/community_state_persist_unit.rs"]
mod community_state_persist_unit;
#[path = "community_sync/community_state_sync_crypto_unit.rs"]
mod community_state_sync_crypto_unit;
#[path = "community_sync/community_sync_engine_unit.rs"]
mod community_sync_engine_unit;
#[path = "community_sync/community_sync_integration.rs"]
mod community_sync_integration;
#[path = "community_sync/community_sync_registry_unit.rs"]
mod community_sync_registry_unit;
