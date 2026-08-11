//! ZEB-442: consolidated community "misc" harness (PR 4b of the test-binary
//! consolidation — ZEB-440 lever 3). Completes the 4-way community split
//! (sync / voting / channel / misc).
//!
//! The 10 former top-level `tests/community_*.rs` files for the remaining
//! community surface each compiled as a separate integration-test binary, every
//! one statically re-linking the whole `harmony-app` lib. They are now
//! `#[path]`-included submodules of this single harness binary. Files under a
//! `tests/` subdirectory are NOT auto-compiled as separate binaries, so each only
//! builds via its `mod` declaration below.
//!
//! Scope: the remaining community domain — admin quorum, backward secrecy,
//! invite/join (invite unit, invite-only flow, inviter enrollment, ZEB-911
//! witness acceptance), membership, pending join, reachability, relay, and
//! serve allowlist.
//!
//! Full `community_*` basenames are preserved (the subdir was moved, not renamed)
//! so the cross-references to these tests from other tests and from production
//! `src/` doc-comments stay resolvable by name.
//!
//! nextest runs every `#[test]` in its own process, so per-test isolation is
//! unchanged. Only `community_invite_only_integration` mutates a process-global
//! env var (`HARMONY_REDEEM_INVITE_TIMEOUT_MS`), and it self-serializes via its
//! own static `Mutex` + RAII restore guard (no other module touches that var),
//! so merging is race-free under both nextest and plain `cargo test` and no
//! `#[serial]` is needed. `community_backward_secrecy_integration` carries
//! `#![cfg(feature = "test-fixtures")]`, which gates cleanly as a module inner
//! attribute.
//!
//! Run just this group: `cargo nextest run -E 'binary(community_misc_tests)'`.

#[path = "community_misc/community_admin_quorum_integration.rs"]
mod community_admin_quorum_integration;
#[path = "community_misc/community_backward_secrecy_integration.rs"]
mod community_backward_secrecy_integration;
#[path = "community_misc/community_invite_inviter_enrollment.rs"]
mod community_invite_inviter_enrollment;
#[path = "community_misc/community_invite_only_integration.rs"]
mod community_invite_only_integration;
#[path = "community_misc/community_invite_unit.rs"]
mod community_invite_unit;
#[path = "community_misc/community_membership_unit.rs"]
mod community_membership_unit;
#[path = "community_misc/community_pending_join_integration.rs"]
mod community_pending_join_integration;
#[path = "community_misc/community_reachability_two_engine_integration.rs"]
mod community_reachability_two_engine_integration;
#[path = "community_misc/community_relay_integration.rs"]
mod community_relay_integration;
#[path = "community_misc/community_serve_allowlist_integration.rs"]
mod community_serve_allowlist_integration;
#[path = "community_misc/zeb911_witness_acceptance.rs"]
mod zeb911_witness_acceptance;
