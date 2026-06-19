//! ZEB-442: consolidated community channel harness (PR 4a of the test-binary
//! consolidation — ZEB-440 lever 3).
//!
//! The 2 former `tests/community_channel_*.rs` top-level files each compiled as a
//! separate integration-test binary, every one statically re-linking the whole
//! `harmony-app` lib. They are now `#[path]`-included submodules of this single
//! harness binary. Files under a `tests/` subdirectory are NOT auto-compiled as
//! separate binaries, so each only builds via its `mod` declaration below.
//!
//! Scope: the community **channel** domain — channel-config CRDT and channel
//! messages. Full `community_*` basenames are preserved (the subdir was moved,
//! not renamed) so cross-references stay resolvable by name.
//!
//! nextest runs every `#[test]` in its own process, so per-test isolation is
//! unchanged. Neither file mutates process-global state, so no `#[serial]`.
//!
//! Run just this group: `cargo nextest run -E 'binary(community_channel_tests)'`.

#[path = "community_channel/community_channel_config_integration.rs"]
mod community_channel_config_integration;
#[path = "community_channel/community_channel_messages_integration.rs"]
mod community_channel_messages_integration;
