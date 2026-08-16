//! ZEB-442: consolidated content test harness (ZEB-440 lever 3 — test-binary
//! consolidation).
//!
//! Scope: the **content / folder / vine-media** domain — content index, folder walker + primitive, move/rename, and vine content roundtrip + feed cache/persistence. Some set an 8 MB per-test thread stack internally (a per-test concern, not binary-level), so merging is safe.
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
//! Run just this group: `cargo nextest run -E 'binary(content_tests)'`.

// ZEB-183: shared harness (spawn_test_runtime, TestHarness, ingest_*, make_leaf,
// make_entry, insert_top_level, fresh_index) — one copy for the whole binary.
#[path = "content/harness.rs"]
mod harness;

#[path = "content/content_index_integration.rs"]
mod content_index_integration;
#[path = "content/folder_ingest_walker_integration.rs"]
mod folder_ingest_walker_integration;
#[path = "content/folder_primitive_integration.rs"]
mod folder_primitive_integration;
#[path = "content/move_content_integration.rs"]
mod move_content_integration;
#[path = "content/rename_content_integration.rs"]
mod rename_content_integration;
#[path = "content/vine_content_roundtrip_integration.rs"]
mod vine_content_roundtrip_integration;
#[path = "content/vine_feed_cache_integration.rs"]
mod vine_feed_cache_integration;
#[path = "content/vine_feed_persistence_integration.rs"]
mod vine_feed_persistence_integration;
#[path = "content/vine_signing_testutil.rs"]
mod vine_signing_testutil;
