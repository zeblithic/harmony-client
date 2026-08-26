//! ZEB-442: consolidated dm test harness (ZEB-440 lever 3 — test-binary
//! consolidation).
//!
//! Scope: the **DM / messaging** domain — DM create/send/thread + IPC roundtrip, mail sync, friend-token roundtrip, and channel backfill.
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
//! Run just this group: `cargo nextest run -E 'binary(dm_tests)'`.

#[path = "dm/channel_backfill_integration.rs"]
mod channel_backfill_integration;
#[path = "dm/dm_cert_identity_integration.rs"]
mod dm_cert_identity_integration;
#[path = "dm/dm_create_integration.rs"]
mod dm_create_integration;
#[path = "dm/dm_ipc_roundtrip.rs"]
mod dm_ipc_roundtrip;
#[path = "dm/dm_revocation_cutoff_integration.rs"]
mod dm_revocation_cutoff_integration;
#[path = "dm/dm_send_integration.rs"]
mod dm_send_integration;
#[path = "dm/dm_thread_integration.rs"]
mod dm_thread_integration;
#[path = "dm/friend_token_roundtrip_integration.rs"]
mod friend_token_roundtrip_integration;
#[path = "dm/grant_ingest_seam_integration.rs"]
mod grant_ingest_seam_integration;
#[path = "dm/mail_sync_integration.rs"]
mod mail_sync_integration;
