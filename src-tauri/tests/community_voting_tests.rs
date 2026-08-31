//! ZEB-442: consolidated community voting + D-FROST harness (PR 3 of the
//! test-binary consolidation — ZEB-440 lever 3).
//!
//! The 14 former `tests/community_{voting,dfrost}_*.rs` top-level files each
//! compiled as a separate integration-test binary, every one statically
//! re-linking the whole `harmony-app` lib (the link-time multiplier this ticket
//! targets). They are now `#[path]`-included submodules of this single harness
//! binary. Files under a `tests/` subdirectory are NOT auto-compiled as separate
//! binaries, so each only builds via its `mod` declaration below.
//!
//! Scope of this group: the community **voting** domain — Tier 1/2/3 polls,
//! deliberation, secret ballots, and the D-FROST threshold-signing committee
//! (FROST-Ristretto255) that countersigns/decrypts on the voting path.
//!
//! Filenames keep their original `community_*` basenames (the subdir was moved,
//! not renamed) so the dense web of cross-references between these tests — and
//! from production `src/` doc-comments — stays resolvable by name.
//!
//! NOTE — `community_voting_process_inbound_prod.rs` is deliberately NOT included
//! here and stays a standalone top-level binary: its load-bearing assertion is
//! that the production inbound-voting path compiles + passes under a vanilla
//! `cargo nextest run --test community_voting_process_inbound_prod` with NO
//! `--features test-fixtures`. Most modules below DO require `test-fixtures`
//! (10 carry `#![cfg(feature = "test-fixtures")]`), so folding the canary in
//! would force the whole harness to need the feature and break that assertion.
//!
//! nextest still runs every `#[test]` in its own process, so per-test isolation
//! is unchanged. None of these mutate process-global state (no env writes, no
//! `set_current_dir`, no real keychain) — isolation is per-test tempdirs +
//! in-process engines — so none use `#[serial]` and merging changes nothing
//! observable.
//!
//! Run just this group: `cargo nextest run -E 'binary(community_voting_tests)'`.

#[path = "community_voting/community_dfrost_integration.rs"]
mod community_dfrost_integration;
#[path = "community_voting/community_dfrost_ipc_integration.rs"]
mod community_dfrost_ipc_integration;
#[path = "community_voting/community_dfrost_transport_integration.rs"]
mod community_dfrost_transport_integration;
#[path = "community_voting/community_dfrost_zenoh_integration.rs"]
mod community_dfrost_zenoh_integration;
#[path = "community_voting/community_voting_tier2_delegate_on_behalf_integration.rs"]
mod community_voting_tier2_delegate_on_behalf_integration;
#[path = "community_voting/community_voting_tier3_deliberation_ipc_integration.rs"]
mod community_voting_tier3_deliberation_ipc_integration;
#[path = "community_voting/community_voting_tier3_deliberation_multi_engine_integration.rs"]
mod community_voting_tier3_deliberation_multi_engine_integration;
#[path = "community_voting/community_voting_tier3_get_ipc_integration.rs"]
mod community_voting_tier3_get_ipc_integration;
#[path = "community_voting/community_voting_tier3_granular_events_integration.rs"]
mod community_voting_tier3_granular_events_integration;
#[path = "community_voting/community_voting_tier3_integration.rs"]
mod community_voting_tier3_integration;
#[path = "community_voting/community_voting_tier3_ipc_integration.rs"]
mod community_voting_tier3_ipc_integration;
#[path = "community_voting/community_voting_tier3_reset_void_integration.rs"]
mod community_voting_tier3_reset_void_integration;
#[path = "community_voting/community_voting_tier3_secret_ipc_integration.rs"]
mod community_voting_tier3_secret_ipc_integration;
#[path = "community_voting/community_voting_tier3_secret_kd_ts_emission_integration.rs"]
mod community_voting_tier3_secret_kd_ts_emission_integration;
#[path = "community_voting/community_voting_tier3_secret_multi_engine_integration.rs"]
mod community_voting_tier3_secret_multi_engine_integration;
#[path = "community_voting/community_voting_zenoh_integration.rs"]
mod community_voting_zenoh_integration;
