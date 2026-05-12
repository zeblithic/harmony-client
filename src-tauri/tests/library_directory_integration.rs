//! ZEB-218 Sub-D Phase 1 — integration tests for the library directory
//! consumer.
//!
//! ## Strategy
//!
//! These tests exercise the `LibraryDirectory::process_sample` path —
//! the same `(decode → verify → aggregate)` pipeline the event-loop
//! Zenoh subscriber consumes in production (see
//! `event_loop.rs::ZEB-218 Sub-D Phase 1: library-directory subscription
//! consumer`). The tests are end-to-end at the consumer layer: they
//! encode `LibraryDirectoryEntry` to CBOR, feed the bytes through
//! `process_sample`, and assert on the `LibraryDirectory`'s
//! `snapshot_all` / `snapshot_filtered_by_library` / aggregation state.
//!
//! Tests do NOT spin up the full Tauri runtime + a sibling Zenoh peer
//! session. That's what `dm_send_integration` calls out as "not
//! strictly required to validate the orchestrator + state-machine
//! integration" — the inner pipeline is the actual interesting
//! component; the Zenoh bridge is plumbing exercised by Sub-A/B/C
//! integration tests already.
//!
//! Tauri IPC parameter binding (the production add_library /
//! remove_library / browse_library / list_libraries entry points) is
//! covered separately by the `dm_ipc_roundtrip` family (ZEB-247). The
//! 4 library-directory IPCs land in `lib.rs` alongside this test file.

#![cfg(feature = "test-fixtures")]

mod common;

use harmony_app::community_invite::{
    encode_invite_url, CommunityInvitePayload, InviteEpochSnapshot, MaterializedCommunityState,
};
use harmony_app::library_directory::{
    DirectoryEntryDTO, LibraryDirectory, MAX_ENTRIES_PER_LIBRARY,
};
use harmony_app::owner_state_crypto::canonical_cbor_encode;
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};

use common::library_fixtures::mock_directory_entry;

/// Build a real open-community invite URL (mirrors the helper in
/// `library_directory::tests`).
fn open_invite_url() -> String {
    let payload = CommunityInvitePayload {
        community_id: SpaceId([0u8; 16]),
        epoch_snapshot: InviteEpochSnapshot {
            epoch: 0,
            sealed_epoch_key: vec![0u8; 32],
            state_snapshot: MaterializedCommunityState::default(),
        },
        admin_addr: OwnerAddr([0u8; 16]),
        community_name: "test".to_string(),
        is_invite_only: false,
        expires_at: None,
        invite_token: None,
        admin_bootstrap: None,
        admin_identity_pub: None,
    };
    encode_invite_url(&payload).expect("encode open invite url")
}

/// Build an invite-only invite URL (must be rejected by verify_entry).
fn invite_only_url() -> String {
    use harmony_app::community_invite::InviteToken;
    use harmony_app::community_membership::{MembershipEventKind, SignedMembershipEvent};
    let admin_addr = OwnerAddr([0u8; 16]);
    let community_id = SpaceId([0u8; 16]);
    let admin_bootstrap = SignedMembershipEvent {
        id: [0u8; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: admin_addr,
        at: Hlc {
            wall_ms: 1_000,
            logical: 0,
            device_id: "test".to_string(),
        },
        sig: [0u8; 64],
        countersig: None,
    };
    let payload = CommunityInvitePayload {
        community_id,
        epoch_snapshot: InviteEpochSnapshot {
            epoch: 0,
            sealed_epoch_key: vec![0u8; 92],
            state_snapshot: MaterializedCommunityState::default(),
        },
        admin_addr,
        community_name: "test-invite-only".to_string(),
        is_invite_only: true,
        expires_at: None,
        invite_token: Some(InviteToken {
            inviter: admin_addr,
            invitee_hint: None,
            minted_at: Hlc {
                wall_ms: 1_000,
                logical: 0,
                device_id: "test".to_string(),
            },
            expires_at: None,
            sig: [0u8; 64],
        }),
        admin_bootstrap: Some(admin_bootstrap),
        admin_identity_pub: Some([0u8; 64]),
    };
    encode_invite_url(&payload).expect("encode invite-only url")
}

fn hlc(wall_ms: u64) -> Hlc {
    Hlc {
        wall_ms,
        logical: 0,
        device_id: "test-dev".to_string(),
    }
}

/// CBOR-encode an entry (raw wire bytes). `process_sample` accepts
/// these as if they had arrived through the Zenoh subscriber.
fn encode_entry(entry: &harmony_app::library_directory::LibraryDirectoryEntry) -> Vec<u8> {
    canonical_cbor_encode(entry).expect("canonical cbor encode")
}

/// Build a fresh `LibraryDirectory` instance for one test. The matching
/// `request_rx` is dropped — production binds it to the event-loop's
/// per-Subscribe Zenoh declarer; here we only exercise `process_sample`
/// and `drop_library`, neither of which consume the request channel.
fn build_directory() -> std::sync::Arc<LibraryDirectory> {
    let (dir, _rx) = LibraryDirectory::new();
    dir
}

// ── Test 1: subscribe-to-library happy path ──────────────────────────

/// Publish 3 distinct entries from one library; `snapshot_all` returns 3.
#[tokio::test]
async fn subscribe_to_library_receives_published_entries() {
    let dir = build_directory();
    let library = OwnerAddr([0xAA; 16]);
    let invite_url = open_invite_url();

    for i in 0u8..3 {
        let entry = mock_directory_entry(
            SpaceId([i; 16]),
            [7u8 + i; 32],
            library,
            hlc(1_000 + i as u64),
            invite_url.clone(),
            &format!("c{i}"),
            "",
            vec![],
        );
        let bytes = encode_entry(&entry);
        let outcome = dir.process_sample(bytes).await.expect("process_sample");
        assert!(
            matches!(
                outcome,
                harmony_app::library_directory::OnEntryOutcome::Inserted(_)
            ),
            "first arrival from a library is Inserted, got {outcome:?}"
        );
    }

    let snap = dir.snapshot_all().await;
    assert_eq!(snap.len(), 3, "expected 3 aggregated entries");
    let mut names: Vec<_> = snap.iter().map(|e| e.entry.name.clone()).collect();
    names.sort();
    assert_eq!(names, vec!["c0", "c1", "c2"]);

    // browse_library DTO mapping: community_addr is derived from
    // admin_identity_pub; raw signature/identity_pub fields are dropped.
    let dtos: Vec<DirectoryEntryDTO> = snap
        .iter()
        .map(DirectoryEntryDTO::from_aggregated)
        .collect();
    for dto in &dtos {
        assert_eq!(dto.community_id.len(), 32, "32-hex-char community_id");
        assert_eq!(dto.community_addr.len(), 32, "32-hex-char community_addr");
        assert_eq!(dto.listed_by_count, 1);
    }
}

// ── Test 2: dedupe by community_id across libraries ──────────────────

/// Two libraries publish entries for the same community_id;
/// snapshot_all returns ONE entry with listed_by_count == 2.
#[tokio::test]
async fn aggregation_dedupes_same_community_from_two_libraries() {
    let dir = build_directory();
    let library_a = OwnerAddr([0xAA; 16]);
    let library_b = OwnerAddr([0xBB; 16]);
    let community = SpaceId([1; 16]);
    let invite_url = open_invite_url();

    let entry_a = mock_directory_entry(
        community,
        [7; 32],
        library_a,
        hlc(1_000),
        invite_url.clone(),
        "Same",
        "",
        vec![],
    );
    let entry_b = mock_directory_entry(
        community,
        [7; 32],
        library_b,
        hlc(1_000),
        invite_url,
        "Same",
        "",
        vec![],
    );

    dir.process_sample(encode_entry(&entry_a)).await.expect("a");
    dir.process_sample(encode_entry(&entry_b)).await.expect("b");

    let snap = dir.snapshot_all().await;
    assert_eq!(snap.len(), 1, "dedupe by community_id");
    assert_eq!(snap[0].listed_by.len(), 2);
    assert!(snap[0].listed_by.contains(&library_a));
    assert!(snap[0].listed_by.contains(&library_b));

    let dto = DirectoryEntryDTO::from_aggregated(&snap[0]);
    assert_eq!(dto.listed_by_count, 2);
}

// ── Test 3: latest-HLC-wins on conflict ──────────────────────────────

/// One library publishes two entries for the same community_id with
/// different names; the newer-HLC entry wins.
#[tokio::test]
async fn latest_hlc_wins_on_conflict() {
    let dir = build_directory();
    let library = OwnerAddr([0xAA; 16]);
    let community = SpaceId([1; 16]);
    let invite_url = open_invite_url();

    let old = mock_directory_entry(
        community,
        [7; 32],
        library,
        hlc(1_000),
        invite_url.clone(),
        "OldName",
        "old",
        vec!["old".into()],
    );
    let new = mock_directory_entry(
        community,
        [7; 32],
        library,
        hlc(2_000),
        invite_url,
        "NewName",
        "new",
        vec!["new".into()],
    );

    dir.process_sample(encode_entry(&old)).await.expect("old");
    dir.process_sample(encode_entry(&new)).await.expect("new");

    let snap = dir.snapshot_all().await;
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].entry.name, "NewName");
    assert_eq!(snap[0].entry.description, "new");
    assert_eq!(snap[0].entry.topics, vec!["new".to_string()]);
}

// ── Test 4: tampered signature rejected ──────────────────────────────

/// Tamper an entry's name after signing; `process_sample` returns a
/// `Verify(SignatureInvalid)` error and the aggregation stays empty.
#[tokio::test]
async fn invalid_community_signature_rejected() {
    let dir = build_directory();
    let library = OwnerAddr([0xAA; 16]);
    let community = SpaceId([1; 16]);

    let mut entry = mock_directory_entry(
        community,
        [7; 32],
        library,
        hlc(1_000),
        open_invite_url(),
        "Original",
        "",
        vec![],
    );
    // Tamper AFTER signing — wire bytes will fail verify_entry.
    entry.name = "Tampered".to_string();

    let res = dir.process_sample(encode_entry(&entry)).await;
    assert!(
        matches!(
            res,
            Err(harmony_app::library_directory::ProcessSampleError::Verify(
                harmony_app::library_directory::EntryVerifyError::SignatureInvalid
            ))
        ),
        "expected SignatureInvalid, got {res:?}"
    );
    assert!(dir.snapshot_all().await.is_empty());
}

// ── Test 5: invite-only URLs rejected at receive ─────────────────────

/// An entry carrying an invite-only invite URL must be rejected
/// (spec §4.1, §9 — directory carries open-community URLs only).
#[tokio::test]
async fn invite_only_invite_url_rejected_at_receive() {
    let dir = build_directory();
    let library = OwnerAddr([0xAA; 16]);
    let community = SpaceId([1; 16]);

    let entry = mock_directory_entry(
        community,
        [7; 32],
        library,
        hlc(1_000),
        invite_only_url(),
        "Closed",
        "",
        vec![],
    );

    let res = dir.process_sample(encode_entry(&entry)).await;
    assert!(
        matches!(
            res,
            Err(harmony_app::library_directory::ProcessSampleError::Verify(
                harmony_app::library_directory::EntryVerifyError::InviteOnlyUrl
            ))
        ),
        "expected InviteOnlyUrl, got {res:?}"
    );
    assert!(dir.snapshot_all().await.is_empty());
}

// ── Test 6: drop_library evicts entries (the remove_library path) ────

/// `drop_library` is the inner method the `LibraryDirectoryRequest::
/// Unsubscribe` arm of the event-loop calls when an IPC `remove_library`
/// fires. After dropping, snapshots see no contributions from the
/// removed library; solo listings are evicted; shared listings stay.
#[tokio::test]
async fn remove_library_evicts_entries_and_drops_subscription() {
    let dir = build_directory();
    let library_a = OwnerAddr([0xAA; 16]);
    let library_b = OwnerAddr([0xBB; 16]);
    let solo = SpaceId([1; 16]);
    let shared = SpaceId([2; 16]);
    let invite_url = open_invite_url();

    // library_a publishes 2 (one solo + one shared with library_b).
    dir.process_sample(encode_entry(&mock_directory_entry(
        solo,
        [7; 32],
        library_a,
        hlc(1_000),
        invite_url.clone(),
        "Solo",
        "",
        vec![],
    )))
    .await
    .expect("solo");
    dir.process_sample(encode_entry(&mock_directory_entry(
        shared,
        [7; 32],
        library_a,
        hlc(1_000),
        invite_url.clone(),
        "Shared",
        "",
        vec![],
    )))
    .await
    .expect("shared from a");
    dir.process_sample(encode_entry(&mock_directory_entry(
        shared,
        [7; 32],
        library_b,
        hlc(1_000),
        invite_url,
        "Shared",
        "",
        vec![],
    )))
    .await
    .expect("shared from b");

    assert_eq!(dir.snapshot_all().await.len(), 2);

    // Drop library_a — solo evicts; shared stays with library_b only.
    let evicted = dir.drop_library(&library_a).await;
    assert_eq!(evicted, vec![solo]);

    let snap = dir.snapshot_all().await;
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].entry.community_id, shared);
    assert_eq!(snap[0].listed_by.len(), 1);
    assert!(snap[0].listed_by.contains(&library_b));

    // browse_library filtered to library_a is now empty.
    let filtered_a = dir.snapshot_filtered_by_library(&library_a).await;
    assert!(filtered_a.is_empty());
}

// ── Test 7: per-library cap evicts oldest on overflow ────────────────

/// Publish `MAX_ENTRIES_PER_LIBRARY + 1` entries from one library;
/// the overflow insert evicts the oldest. Aggregation never exceeds
/// the cap for a single library.
///
/// Performance: 10_001 Ed25519 verifies × ~50 µs ≈ 500 ms wall time.
/// Stays well under nextest's 60 s SLOW threshold; keep as a regular
/// test (no `#[ignore]`). If CI ever flags it as slow, the
/// `MAX_ENTRIES_PER_LIBRARY` const is the only knob to revisit.
#[tokio::test]
async fn per_library_cap_evicts_oldest_on_overflow() {
    let dir = build_directory();
    let library = OwnerAddr([0xAA; 16]);
    let invite_url = open_invite_url();

    for i in 0..(MAX_ENTRIES_PER_LIBRARY as u32 + 1) {
        let mut cid = [0u8; 16];
        cid[..4].copy_from_slice(&i.to_be_bytes());
        let entry = mock_directory_entry(
            SpaceId(cid),
            [7; 32],
            library,
            // Strictly-increasing wall_ms — guarantees the find_oldest
            // sort picks i=0 as the eviction candidate at overflow.
            hlc(1_000 + i as u64),
            invite_url.clone(),
            "c",
            "",
            vec![],
        );
        let outcome = dir
            .process_sample(encode_entry(&entry))
            .await
            .expect("process");
        if i < MAX_ENTRIES_PER_LIBRARY as u32 {
            assert!(
                matches!(
                    outcome,
                    harmony_app::library_directory::OnEntryOutcome::Inserted(_)
                ),
                "i={i}: expected Inserted, got {outcome:?}"
            );
        } else {
            let mut oldest_cid = [0u8; 16];
            oldest_cid[..4].copy_from_slice(&0u32.to_be_bytes());
            match outcome {
                harmony_app::library_directory::OnEntryOutcome::EvictedThenInserted {
                    evicted,
                    ..
                } => {
                    assert_eq!(
                        evicted,
                        SpaceId(oldest_cid),
                        "overflow must evict the i=0 (oldest-HLC) entry"
                    );
                }
                other => panic!("expected EvictedThenInserted, got {other:?}"),
            }
        }
    }

    // Aggregation map size == cap (the overflow's eviction-then-insert
    // is net-zero on cardinality).
    let snap = dir.snapshot_all().await;
    assert_eq!(snap.len(), MAX_ENTRIES_PER_LIBRARY);
}
