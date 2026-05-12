//! ZEB-279 Sub-D Phase 2 — integration tests for the library-announce
//! consumer (`harmony/discovery/library/announce`).
//!
//! ## Strategy
//!
//! Drives `LibraryDirectory::process_announce` directly (mirroring
//! `library_directory_integration.rs`'s `process_sample` pattern). The
//! announce-topic Zenoh subscriber in `event_loop.rs` is plumbing
//! exercised end-to-end by the cross-peer harness — these tests cover
//! the ingest path that the subscriber feeds: CBOR decode →
//! `verify_announce` → `Announces::on_announce` → outcome.
//!
//! Six scenarios:
//!   1. Insert-and-snapshot: an announce becomes visible in `snapshot()`.
//!   2. Dedupe (latest-listed_at-wins): newer HLC replaces older.
//!   3. Older-listed_at-dropped silently: returns `Idempotent`.
//!   4. Invalid-sig rejected: bit-flipped payload fails verify.
//!   5. Name-too-long rejected: >200 bytes returns `NameTooLong`.
//!   6. Cap eviction: 1001st announce evicts oldest-by-listed_at.

#![cfg(feature = "test-fixtures")]

mod common;

use harmony_app::library_directory::{
    AnnounceOutcome, AnnounceVerifyError, LibraryDirectory, MAX_DISCOVERED_LIBRARIES,
};

#[tokio::test]
async fn announce_ingests_and_appears_in_snapshot() {
    let (dir, _rx) = LibraryDirectory::new();
    let (bytes, addr) = common::library_fixtures::mock_library_announce(
        [1u8; 32],
        "Indie Games",
        "Curated indie games",
        100,
    );
    let result = dir
        .process_announce(bytes)
        .await
        .expect("process_announce ok");
    assert_eq!(result.outcome, AnnounceOutcome::Inserted(addr));
    assert_eq!(result.evicted, None);

    let announces = dir.announces.lock().await;
    let snap = announces.snapshot();
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].name, "Indie Games");
    assert_eq!(snap[0].description, "Curated indie games");
}

#[tokio::test]
async fn announce_dedupes_latest_listed_at_wins() {
    let (dir, _rx) = LibraryDirectory::new();
    // Use the SAME seed so both fixtures derive the same library_addr.
    let (bytes_old, addr) =
        common::library_fixtures::mock_library_announce([2u8; 32], "Old name", "Old desc", 100);
    let (bytes_new, addr_new) =
        common::library_fixtures::mock_library_announce([2u8; 32], "New name", "New desc", 200);
    assert_eq!(addr, addr_new, "same seed → same library_addr");

    dir.process_announce(bytes_old).await.expect("first ok");
    let result = dir.process_announce(bytes_new).await.expect("second ok");
    assert_eq!(result.outcome, AnnounceOutcome::Updated(addr));

    let snap = dir.announces.lock().await.snapshot();
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].name, "New name");
}

#[tokio::test]
async fn announce_older_listed_at_dropped_silently() {
    let (dir, _rx) = LibraryDirectory::new();
    let (bytes_new, _addr) =
        common::library_fixtures::mock_library_announce([3u8; 32], "New", "", 200);
    let (bytes_old, _) =
        common::library_fixtures::mock_library_announce([3u8; 32], "Older", "", 100);
    dir.process_announce(bytes_new).await.expect("new ok");
    let result = dir
        .process_announce(bytes_old)
        .await
        .expect("older ok (no sig fail)");
    assert_eq!(result.outcome, AnnounceOutcome::Idempotent);
    assert_eq!(result.evicted, None);

    let snap = dir.announces.lock().await.snapshot();
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].name, "New");
}

#[tokio::test]
async fn announce_invalid_sig_rejected() {
    let (dir, _rx) = LibraryDirectory::new();
    let (mut bytes, _addr) =
        common::library_fixtures::mock_library_announce([4u8; 32], "Tampered", "", 100);
    // Flip a bit in the middle of the payload to corrupt the signed bytes.
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0x01;

    let err = dir.process_announce(bytes).await.unwrap_err();
    assert!(
        matches!(
            err,
            AnnounceVerifyError::SignatureInvalid | AnnounceVerifyError::Encode(_)
        ),
        "expected SignatureInvalid (or Encode if the bit flip broke CBOR structure), got {:?}",
        err,
    );

    // Map remains empty.
    assert_eq!(dir.announces.lock().await.snapshot().len(), 0);
}

#[tokio::test]
async fn announce_name_too_long_rejected() {
    let (dir, _rx) = LibraryDirectory::new();
    // 201-byte name exceeds MAX_NAME_LEN=200.
    let huge_name = "x".repeat(201);
    let (bytes, _addr) =
        common::library_fixtures::mock_library_announce([5u8; 32], &huge_name, "", 100);
    let err = dir.process_announce(bytes).await.unwrap_err();
    assert!(
        matches!(err, AnnounceVerifyError::NameTooLong),
        "expected NameTooLong, got {:?}",
        err,
    );
    assert_eq!(dir.announces.lock().await.snapshot().len(), 0);
}

#[tokio::test]
async fn announce_cap_eviction_drops_oldest() {
    let (dir, _rx) = LibraryDirectory::new();

    // Fill exactly to cap with distinct seeds + ascending listed_at.
    // Each call does Ed25519 sig + verify so this is real CPU work,
    // but for cap=1_000 it stays comfortably under the integration
    // test budget on dev hardware.
    for i in 0..MAX_DISCOVERED_LIBRARIES {
        let mut seed = [0u8; 32];
        seed[0] = (i & 0xFF) as u8;
        seed[1] = ((i >> 8) & 0xFF) as u8;
        let (bytes, _addr) =
            common::library_fixtures::mock_library_announce(seed, "filler", "", 1_000 + i as u64);
        dir.process_announce(bytes).await.expect("fill ok");
    }
    assert_eq!(
        dir.announces.lock().await.snapshot().len(),
        MAX_DISCOVERED_LIBRARIES
    );

    // Insert one more with the highest listed_at. The earliest filler
    // (i=0, listed_at=1_000) should be evicted.
    let new_seed = [0xFEu8; 32];
    let (bytes_new, new_addr) =
        common::library_fixtures::mock_library_announce(new_seed, "newest", "", 99_999);
    let result = dir.process_announce(bytes_new).await.expect("over-cap ok");
    assert_eq!(result.outcome, AnnounceOutcome::Inserted(new_addr));
    assert!(result.evicted.is_some(), "must have evicted oldest");

    let snap = dir.announces.lock().await.snapshot();
    assert_eq!(snap.len(), MAX_DISCOVERED_LIBRARIES);
    // Newest is at index 0 (snapshot sorts by listed_at desc).
    assert_eq!(snap[0].name, "newest");
}
