//! ZEB-442: consolidated voice test harness (ZEB-440 lever 3 — test-binary
//! consolidation).
//!
//! Scope: the **voice** domain — 1:1 DM calls, group-DM voice, media auth, moderation, and the presence / mute / scale two-engine flows.
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
//! Run just this group: `cargo nextest run -E 'binary(voice_tests)'`.

#[path = "voice/group_dm_voice_three_engine_integration.rs"]
mod group_dm_voice_three_engine_integration;
#[path = "voice/voice_dm_two_engine_integration.rs"]
mod voice_dm_two_engine_integration;
#[path = "voice/voice_media_auth_integration.rs"]
mod voice_media_auth_integration;
#[path = "voice/voice_moderation_integration.rs"]
mod voice_moderation_integration;
#[path = "voice/voice_presence_mute_integration.rs"]
mod voice_presence_mute_integration;
#[path = "voice/voice_presence_scale_integration.rs"]
mod voice_presence_scale_integration;
#[path = "voice/voice_presence_two_engine_integration.rs"]
mod voice_presence_two_engine_integration;
