//! ZEB-442: consolidated wire-format fixture harness (PR 1 of the test-binary
//! consolidation — ZEB-440 lever 3).
//!
//! The 27 former `tests/wire_format_*.rs` top-level files each compiled as a
//! separate integration-test binary, every one statically re-linking the whole
//! `harmony-app` lib (the link-time multiplier this ticket targets). They are now
//! `#[path]`-included submodules of this single harness binary. Files under a
//! `tests/` subdirectory are NOT auto-compiled as separate binaries, so each only
//! builds via its `mod` declaration below.
//!
//! nextest still runs every `#[test]` in its own process, so the per-test
//! isolation these fixtures assume is unchanged. These are all pure canonical-CBOR
//! / wire-format pinning tests — no shared mutable state, no env-var touching,
//! no `#[serial]`.
//!
//! Run just this group: `cargo nextest run -E 'binary(wire_format_tests)'`.

#[path = "wire_format/channel_log_fixtures.rs"]
mod channel_log_fixtures;
#[path = "wire_format/community_fixtures.rs"]
mod community_fixtures;
#[path = "wire_format/community_relay_fixtures.rs"]
mod community_relay_fixtures;
#[path = "wire_format/community_sync_fixtures.rs"]
mod community_sync_fixtures;
#[path = "wire_format/community_voting_policy_fixtures.rs"]
mod community_voting_policy_fixtures;
#[path = "wire_format/fixture.rs"]
mod fixture;
#[path = "wire_format/library_announce_fixtures.rs"]
mod library_announce_fixtures;
#[path = "wire_format/library_directory_fixtures.rs"]
mod library_directory_fixtures;
#[path = "wire_format/pkarr_routing_record_fixtures.rs"]
mod pkarr_routing_record_fixtures;
#[path = "wire_format/presence_fixtures.rs"]
mod presence_fixtures;
#[path = "wire_format/profile_broadcast_fixtures.rs"]
mod profile_broadcast_fixtures;
#[path = "wire_format/profile_card_avatar_fixtures.rs"]
mod profile_card_avatar_fixtures;
#[path = "wire_format/profile_card_fixtures.rs"]
mod profile_card_fixtures;
#[path = "wire_format/profile_page_doc_fixtures.rs"]
mod profile_page_doc_fixtures;
#[path = "wire_format/reachability_announce_fixtures.rs"]
mod reachability_announce_fixtures;
#[path = "wire_format/tier3_poll_export_fixtures.rs"]
mod tier3_poll_export_fixtures;
#[path = "wire_format/voice_fixtures.rs"]
mod voice_fixtures;
#[path = "wire_format/voting_tier3_fixtures.rs"]
mod voting_tier3_fixtures;
#[path = "wire_format/voting_tier3_secret_fixtures.rs"]
mod voting_tier3_secret_fixtures;
#[path = "wire_format/zeb1031_reset_fixtures.rs"]
mod zeb1031_reset_fixtures;
#[path = "wire_format/zeb213_fixtures.rs"]
mod zeb213_fixtures;
#[path = "wire_format/zeb250_fixtures.rs"]
mod zeb250_fixtures;
#[path = "wire_format/zeb254_fixtures.rs"]
mod zeb254_fixtures;
#[path = "wire_format/zeb285_fixtures.rs"]
mod zeb285_fixtures;
#[path = "wire_format/zeb290_fixtures.rs"]
mod zeb290_fixtures;
#[path = "wire_format/zeb291_fixtures.rs"]
mod zeb291_fixtures;
#[path = "wire_format/zeb303_dfrost_fixtures.rs"]
mod zeb303_dfrost_fixtures;
#[path = "wire_format/zeb370_fixtures.rs"]
mod zeb370_fixtures;
#[path = "wire_format/zeb375_pex_fixtures.rs"]
mod zeb375_pex_fixtures;
#[path = "wire_format/zeb376_intro_fixtures.rs"]
mod zeb376_intro_fixtures;
