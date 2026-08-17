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

use crate::common;

use harmony_app::community_invite::{
    encode_invite_url, CommunityInvitePayload, InviteEpochSnapshot, MaterializedCommunityState,
};
use harmony_app::library_directory::{
    DirectoryEntryDTO, LibraryDirectory, MAX_ENTRIES_PER_LIBRARY,
};
use harmony_app::owner_state_crypto::canonical_cbor_encode;
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};

use common::library_fixtures::mock_directory_entry;

/// Build a real open-community invite URL whose `community_id` and
/// `admin_addr` bind to the directory entry the test will sign — so
/// `verify_entry`'s R2 F1 payload-consistency check passes.
///
/// Use `open_invite_url_for(community, admin_seed)` when the test
/// publishes a specific community_id/admin pair. `open_invite_url()`
/// preserves the legacy zero-arg signature for the common
/// (SpaceId([1; 16]), [7; 32]) fixture used throughout this file.
fn open_invite_url_for(community_id: SpaceId, admin_seed: [u8; 32]) -> String {
    let (_signing_key, identity_pub) =
        common::library_fixtures::build_test_admin_identity(admin_seed);
    let admin_addr = OwnerAddr(
        harmony_identity::Identity::from_public_bytes(&identity_pub)
            .expect("identity from pub")
            .address_hash,
    );
    let payload = CommunityInvitePayload {
        inviter_signer_certs: Vec::new(),
        community_id,
        epoch_snapshot: InviteEpochSnapshot {
            epoch: 0,
            sealed_epoch_key: vec![0u8; 32],
            sealed_epoch_keys: Vec::new(),
            state_snapshot: MaterializedCommunityState::default(),
        },
        admin_addr,
        community_name: "test".to_string(),
        is_invite_only: false,
        expires_at: None,
        invite_token: None,
        admin_bootstrap: None,
        inviter_identity_pub: None,
        forked_from: None,
        pre_fork_snapshot: None,
        inviter_enrollment: None,
        untargeted_decrypt_key: None,
    };
    encode_invite_url(&payload).expect("encode open invite url")
}

fn open_invite_url() -> String {
    open_invite_url_for(SpaceId([1; 16]), [7; 32])
}

/// Build an invite-only invite URL (must be rejected by verify_entry).
fn invite_only_url() -> String {
    use harmony_app::community_invite::InviteToken;
    use harmony_app::community_membership::{MembershipEventKind, SignedMembershipEvent};
    let admin_addr = OwnerAddr([0u8; 16]);
    let community_id = SpaceId([0u8; 16]);
    let admin_bootstrap = SignedMembershipEvent {
        signer_certs: Vec::new(),
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
        // ZEB-339: bootstrap-Join must embed the admin's EnrollmentCert.
        enrollment: Some(harmony_app::community_membership::mint_test_owner(0xC2).cert),
    };
    let payload = CommunityInvitePayload {
        inviter_signer_certs: Vec::new(),
        community_id,
        epoch_snapshot: InviteEpochSnapshot {
            epoch: 0,
            sealed_epoch_key: vec![0u8; 92],
            sealed_epoch_keys: Vec::new(),
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
        inviter_identity_pub: Some([0u8; 64]),
        forked_from: None,
        pre_fork_snapshot: None,
        // ZEB-339: encode_invite_url requires invite-only payloads to carry the
        // inviter's EnrollmentCert. Its content is irrelevant — this URL must be
        // rejected at receive (verify_entry rejects invite-only directory entries).
        inviter_enrollment: Some(harmony_app::community_membership::mint_test_owner(0xC1).cert),
        // ZEB-367: untargeted invite-only payload requires the URL decrypt key for
        // encode_invite_url to accept it (content irrelevant — rejected at verify).
        untargeted_decrypt_key: Some([0u8; 32]),
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

    for i in 0u8..3 {
        // R2 F1: invite URL must bind to the same (community_id,
        // admin_seed) the entry will be signed under.
        let invite_url = open_invite_url_for(SpaceId([i; 16]), [7u8 + i; 32]);
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
        let result = dir
            .process_sample(library, bytes)
            .await
            .expect("process_sample");
        assert!(
            matches!(
                result.outcome,
                harmony_app::library_directory::OnEntryOutcome::Inserted(_)
            ),
            "first arrival from a library is Inserted, got {result:?}"
        );
        assert!(result.evicted.is_none(), "no cap-eviction expected");
    }

    let snap = dir.snapshot_all().await;
    assert_eq!(snap.len(), 3, "expected 3 aggregated entries");
    let mut names: Vec<_> = snap.iter().map(|e| e.entry.name.clone()).collect();
    names.sort();
    assert_eq!(names, vec!["c0", "c1", "c2"]);

    // browse_library DTO mapping: community_addr is derived from
    // inviter_identity_pub; raw signature/identity_pub fields are dropped.
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

    dir.process_sample(library_a, encode_entry(&entry_a))
        .await
        .expect("a");
    dir.process_sample(library_b, encode_entry(&entry_b))
        .await
        .expect("b");

    let snap = dir.snapshot_all().await;
    assert_eq!(snap.len(), 1, "dedupe by community_id");
    assert_eq!(snap[0].attested_by.len(), 2);
    assert!(snap[0].attested_by.contains(&library_a));
    assert!(snap[0].attested_by.contains(&library_b));

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

    dir.process_sample(library, encode_entry(&old))
        .await
        .expect("old");
    dir.process_sample(library, encode_entry(&new))
        .await
        .expect("new");

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

    let res = dir.process_sample(library, encode_entry(&entry)).await;
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

    let res = dir.process_sample(library, encode_entry(&entry)).await;
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

// ── F2 regression: spoofed listed_by rejected ────────────────────────

/// Security regression: an entry whose payload `listed_by` disagrees
/// with the subscribed library's topic owner must be rejected as
/// `AttributionMismatch`, preventing a malicious library from
/// publishing entries attributing themselves to OTHER libraries (which
/// would bypass per-library caps and prevent `remove_library` from
/// evicting them).
#[tokio::test]
async fn attribution_spoof_rejected() {
    let dir = build_directory();
    let real_library = OwnerAddr([0xAA; 16]);
    let spoofed_library = OwnerAddr([0xBB; 16]);

    // Entry's payload `listed_by` claims library B but arrives on the
    // topic subscribed under library A — must reject.
    let entry = mock_directory_entry(
        SpaceId([1; 16]),
        [7; 32],
        spoofed_library, // payload claims B
        hlc(1_000),
        open_invite_url(),
        "spoofed",
        "",
        vec![],
    );
    let res = dir
        .process_sample(real_library, encode_entry(&entry)) // arrived on A's topic
        .await;
    match res {
        Err(harmony_app::library_directory::ProcessSampleError::AttributionMismatch {
            expected,
            actual,
        }) => {
            assert_eq!(expected, real_library);
            assert_eq!(actual, spoofed_library);
        }
        other => panic!("expected AttributionMismatch, got {other:?}"),
    }
    assert!(
        dir.snapshot_all().await.is_empty(),
        "spoofed entry must not aggregate"
    );
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
    // R2 F1: each entry needs an invite URL bound to its own community_id
    // (admin_seed [7; 32] is shared across all three).
    let invite_solo = open_invite_url_for(solo, [7; 32]);
    let invite_shared = open_invite_url_for(shared, [7; 32]);

    // library_a publishes 2 (one solo + one shared with library_b).
    dir.process_sample(
        library_a,
        encode_entry(&mock_directory_entry(
            solo,
            [7; 32],
            library_a,
            hlc(1_000),
            invite_solo,
            "Solo",
            "",
            vec![],
        )),
    )
    .await
    .expect("solo");
    dir.process_sample(
        library_a,
        encode_entry(&mock_directory_entry(
            shared,
            [7; 32],
            library_a,
            hlc(1_000),
            invite_shared.clone(),
            "Shared",
            "",
            vec![],
        )),
    )
    .await
    .expect("shared from a");
    dir.process_sample(
        library_b,
        encode_entry(&mock_directory_entry(
            shared,
            [7; 32],
            library_b,
            hlc(1_000),
            invite_shared,
            "Shared",
            "",
            vec![],
        )),
    )
    .await
    .expect("shared from b");

    assert_eq!(dir.snapshot_all().await.len(), 2);

    // Drop library_a — both communities evict.
    //
    // `solo` evicts because library_a was its only contributor.
    //
    // `shared` evicts because library_a was published FIRST (and the
    // library_b publish at the same HLC is NOT strictly newer, so the
    // stored entry's `listed_by` field remained library_a). Per the F3
    // correctness rule (see `Aggregation::drop_library` doc): when the
    // stored entry's `listed_by` matches the dropped library, the
    // community is fully evicted to avoid surfacing stale metadata
    // sourced from a removed library.
    let mut evicted = dir.drop_library(&library_a).await;
    evicted.sort();
    let mut expected = vec![solo, shared];
    expected.sort();
    assert_eq!(evicted, expected);

    let snap = dir.snapshot_all().await;
    assert!(
        snap.is_empty(),
        "F3 correctness: communities whose stored entry was sourced from the dropped library must evict entirely"
    );

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

    for i in 0..(MAX_ENTRIES_PER_LIBRARY as u32 + 1) {
        let mut cid = [0u8; 16];
        cid[..4].copy_from_slice(&i.to_be_bytes());
        let community_id = SpaceId(cid);
        // R2 F1: invite URL must bind to this community_id.
        let invite_url = open_invite_url_for(community_id, [7; 32]);
        let entry = mock_directory_entry(
            community_id,
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
        let result = dir
            .process_sample(library, encode_entry(&entry))
            .await
            .expect("process");
        if i < MAX_ENTRIES_PER_LIBRARY as u32 {
            assert!(
                matches!(
                    result.outcome,
                    harmony_app::library_directory::OnEntryOutcome::Inserted(_)
                ),
                "i={i}: expected Inserted, got {result:?}"
            );
            assert!(result.evicted.is_none(), "i={i}: no eviction under cap");
        } else {
            let mut oldest_cid = [0u8; 16];
            oldest_cid[..4].copy_from_slice(&0u32.to_be_bytes());
            assert!(
                matches!(
                    result.outcome,
                    harmony_app::library_directory::OnEntryOutcome::Inserted(_)
                ),
                "overflow path: new arrival is Inserted, got {result:?}"
            );
            assert_eq!(
                result.evicted,
                Some(SpaceId(oldest_cid)),
                "overflow must evict the i=0 (oldest-HLC) entry"
            );
        }
    }

    // Aggregation map size == cap (the overflow's eviction-then-insert
    // is net-zero on cardinality).
    let snap = dir.snapshot_all().await;
    assert_eq!(snap.len(), MAX_ENTRIES_PER_LIBRARY);
}

// ── Test 8: click-to-join end-to-end smoke (spec §8) ─────────────────

/// End-to-end smoke test for the ZEB-218 Sub-D Phase 1 spec §8
/// click-to-join flow:
///
/// 1. A founder mints a real open-community invite URL (via
///    `build_open_invite_url`, the same path the production
///    `generate_invite` IPC uses).
/// 2. A mock library publishes a `LibraryDirectoryEntry` carrying that
///    URL — the joiner's `LibraryDirectory::process_sample` accepts
///    and aggregates it.
/// 3. The joiner reads the aggregated entry's `invite_url` (mirroring
///    how the UI would pull it from `browse_library` and feed it to
///    `redeem_invite`).
/// 4. The joiner calls `redeem_invite_inner` (the inner of the
///    `redeem_invite` IPC — same call path the click-to-join button
///    in spec §8 invokes).
/// 5. Assert: the joiner's `OwnerState.spaces` now contains a
///    Community Space whose id matches the founder's community.
///
/// Validates that the entire reuse-existing-redeem_invite architecture
/// holds end-to-end at the module-API level. No new join protocol
/// surface needed; ZEB-249's open-community invite shape (unsealed
/// 32-byte EpochKey) handles the actual join correctly.
///
/// Harness scope: follows the Task-4 `dm_send_integration.rs`-style
/// direct-module-API pattern rather than the heavier two-engine
/// in-memory Zenoh bridge of `community_open_flow_integration.rs`.
/// The library-publishes step is driven via `process_sample` (the
/// same in-process pipeline Tests 1-7 above use); the redeem step is
/// driven via `redeem_invite_inner` (the same inner helper the
/// `redeem_invite_inner_tests::happy_path_no_pending_transaction_after_success`
/// unit test in `lib.rs` uses).
#[tokio::test]
async fn click_to_join_redeem_invite_smoke() {
    use harmony_app::community_channel_log_engine::{
        ChannelLogEngineConfig, ChannelLogRegistry, ChannelLogRegistryConfig,
    };
    use harmony_app::community_state_sync::{
        CommunityRegistryConfig, CommunitySyncRegistry, IdentityResolver, DEFAULT_DEBOUNCE_MS,
    };
    use harmony_app::content_store::{ContentStore, RuntimeContentStore};
    use harmony_app::dm_outbox::DmOutbox;
    use harmony_app::owner_state_crdt::OwnerState;
    use harmony_app::owner_state_types::{DeviceIdentityHash, EpochKey, SpaceKind};
    use harmony_identity::PrivateIdentity;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::{mpsc, Mutex};

    // ── Founder side: mint a real OPEN-community invite URL. ─────────
    //
    // The founder is just an identity + a fixed community_id +
    // EpochKey. We don't need to spin up the founder's community-sync
    // engine; the invite payload is self-describing per spec §5.2 —
    // joiner's `redeem_invite_inner` cold-bootstraps from it.
    //
    // Importantly: the founder's `inviter_identity_pub` must match the
    // `community_admin_identity_pub` in the directory entry, because
    // the joiner's `verify_entry` checks both the entry's signature
    // (Ed25519 over canonical CBOR) AND that the URL's `admin_addr`
    // resolves consistently with the entry's identity_pub. The
    // `mock_directory_entry` fixture's `admin_seed` parameter drives
    // both — we use the same seed for the URL's admin_addr derivation.
    let founder_seed = [0x55u8; 32];
    let founder_signing = ed25519_dalek::SigningKey::from_bytes(&founder_seed);
    let founder_ed_pub = founder_signing.verifying_key().to_bytes();
    let mut founder_identity_pub = [0u8; 64];
    founder_identity_pub[..32].copy_from_slice(&[0x11; 32]);
    founder_identity_pub[32..].copy_from_slice(&founder_ed_pub);
    // R2 F1: the directory entry's `community_admin_identity_pub`
    // (derived from `founder_seed` via `mock_directory_entry`) must
    // resolve to the same `OwnerAddr` the invite payload's `admin_addr`
    // carries — otherwise `verify_entry`'s payload-consistency check
    // rejects the entry as `PayloadAdminIdentityMismatch`. Derive both
    // sides from the same identity_pub.
    let founder_owner_addr = OwnerAddr(
        harmony_identity::Identity::from_public_bytes(&founder_identity_pub)
            .expect("identity from pub")
            .address_hash,
    );

    let community_id = SpaceId([0xC0; 16]);
    let membership_key = EpochKey::new([0xEC; 32]);

    let invite_payload = CommunityInvitePayload {
        inviter_signer_certs: Vec::new(),
        community_id,
        epoch_snapshot: InviteEpochSnapshot {
            epoch: 0,
            sealed_epoch_key: membership_key.as_bytes().to_vec(),
            sealed_epoch_keys: Vec::new(),
            state_snapshot: MaterializedCommunityState::default(),
        },
        admin_addr: founder_owner_addr,
        community_name: "Founder Community".into(),
        is_invite_only: false,
        expires_at: None,
        invite_token: None,
        admin_bootstrap: None,
        inviter_identity_pub: None,
        forked_from: None,
        pre_fork_snapshot: None,
        inviter_enrollment: None,
        untargeted_decrypt_key: None,
    };
    let founder_invite_url =
        harmony_app::build_open_invite_url(&invite_payload).expect("build open invite url");

    // ── Library side: publish a directory entry carrying the URL. ────
    //
    // Reuses the same `process_sample` pipeline as Tests 1-7 — exact
    // same code path the production Zenoh subscriber drives in
    // `event_loop.rs`. The `mock_directory_entry` fixture signs the
    // entry with the founder's identity seed so `verify_entry`
    // accepts it.
    let dir = build_directory();
    let library_addr = OwnerAddr([0xBB; 16]);
    let entry = mock_directory_entry(
        community_id,
        founder_seed,
        library_addr,
        hlc(1_000),
        founder_invite_url.clone(),
        "Founder Community",
        "smoke-test target",
        vec!["smoke".into()],
    );
    let result = dir
        .process_sample(library_addr, encode_entry(&entry))
        .await
        .expect("process_sample");
    assert!(
        matches!(
            result.outcome,
            harmony_app::library_directory::OnEntryOutcome::Inserted(_)
        ),
        "directory entry must aggregate, got {result:?}"
    );

    // The joiner's UI would call browse_library() and read entry
    // DTOs. Here we go through `snapshot_all` (the underlying state
    // browse_library projects from) and extract the URL exactly as
    // the click-to-join button would.
    let snap = dir.snapshot_all().await;
    assert_eq!(snap.len(), 1, "exactly the founder's entry aggregated");
    let click_to_join_url = snap[0].entry.invite_url.clone();
    assert_eq!(
        click_to_join_url, founder_invite_url,
        "directory URL must round-trip unchanged"
    );

    // ── Joiner side: build a minimal redeem_invite_inner fixture. ────
    //
    // Mirrors `redeem_invite_inner_tests::build_redeem_invite_test_fixture`
    // (in `lib.rs`) but inlined here since fixtures are private to
    // the crate. Same shape: a CommunitySyncRegistry + ChannelLogRegistry
    // + DmOutbox + crdt_state + hlc_tracker, plus adapter/unicast
    // channels whose receivers are kept alive so try_send doesn't
    // observe Closed.
    let tmp = tempfile::TempDir::new().expect("tempdir");
    // ZEB-339: the joiner is an enrolled-device owner — actor = owner_id, its
    // redemption Join is signed by the device key (#2) and carries the joiner's
    // Master enrollment cert (passed into redeem_invite_inner). A separate
    // PrivateIdentity backs the DM-layer DmOutbox plumbing below (unrelated to
    // community membership verification).
    let joiner = harmony_app::community_membership::mint_test_owner(0xBB);
    let joiner_owner = joiner.owner;
    let joiner_pub_64 = [0u8; 64];
    let joiner_signing_key = Arc::new(joiner.device_key.clone());
    let joiner_identity = PrivateIdentity::from_seed(&[0xbb; 32]);

    // Identity resolver — admits the joiner's own signature on the
    // bootstrap Join. Founder isn't reached on the OPEN path (no
    // counter-sign dance), so we only need the joiner here.
    struct JoinerResolver {
        joiner: OwnerAddr,
        joiner_pub: [u8; 64],
    }
    #[async_trait::async_trait]
    impl IdentityResolver for JoinerResolver {
        async fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
            if *addr == self.joiner {
                Some(self.joiner_pub)
            } else {
                None
            }
        }
    }

    // CAS servicer stub (drains the channel so `put_local` doesn't
    // block forever on a oneshot reply). Mirrors the pattern from
    // `community_sync_integration::build_unreachable_invite_only_redeem_fixture`.
    let (cas_op_tx, mut cas_op_rx) = mpsc::channel::<harmony_app::content_store::CasOp>(8);
    tokio::spawn(async move {
        while let Some(op) = cas_op_rx.recv().await {
            match op {
                harmony_app::content_store::CasOp::PutLocal { reply, .. } => {
                    if let Some(r) = reply {
                        let _ = r.send(Ok(()));
                    }
                }
                harmony_app::content_store::CasOp::GetOrFetch { reply, .. } => {
                    let _ = reply.send(Ok(None));
                }
                harmony_app::content_store::CasOp::GetLocal { reply, .. } => {
                    let _ = reply.send(None);
                }
                harmony_app::content_store::CasOp::AllowServeSubtree { reply, .. } => {
                    let _ = reply.send(Ok(0));
                }
            }
        }
    });
    let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx,
        Duration::from_millis(1000),
    ));

    let community_registry = Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
        device_id: "joiner-dev".into(),
        content_store: cs,
        identity_resolver: Arc::new(JoinerResolver {
            joiner: joiner_owner,
            joiner_pub: joiner_pub_64,
        }),
        identity_dir: tmp.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: None,
        self_owner: joiner_owner,
        signing_key: Arc::clone(&joiner_signing_key),
        crdt_state: None,
        nav_emitter: None,
        presence_resync_rx: None,
    }));

    let (community_adapter_tx, _community_adapter_rx) =
        mpsc::channel::<harmony_app::event_loop::CommunityAdapterRequest>(16);

    // ZEB-339: use joiner's real owner material (seed 0xBB) for community
    // signing so DmOutbox::new's debug_assert passes (cert.owner_id == joiner_owner).
    let joiner_community_sk_lib = Arc::new(ed25519_dalek::SigningKey::from_bytes(
        &joiner.device_key.to_bytes(),
    ));
    let joiner_enrollment_lib = joiner.cert.clone();
    let dm_outbox = Arc::new(Mutex::new(DmOutbox::new(
        "joiner-dev".into(),
        joiner_owner,
        DeviceIdentityHash(joiner_identity.identity.address_hash),
        Arc::clone(&joiner_signing_key),
        Arc::new(joiner_identity),
        joiner_community_sk_lib,
        joiner_enrollment_lib,
    )));

    let (channel_log_adapter_tx, _channel_log_adapter_rx) =
        mpsc::unbounded_channel::<harmony_app::event_loop::ChannelLogAdapterRequest>();
    let channel_log_registry = Arc::new(ChannelLogRegistry::new(ChannelLogRegistryConfig {
        adapter_request_tx: channel_log_adapter_tx,
        sink: Arc::new(harmony_app::node_event_sink::FanoutSink(vec![])),
        identity_dir: tmp.path().to_path_buf(),
        self_owner: joiner_owner,
        self_device_id: "joiner-dev".into(),
        signing_key: Arc::clone(&joiner_signing_key),
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
        engine_config: ChannelLogEngineConfig::default(),
        transport_epoch_rx: None,
        // ZEB-599 Direction 1: no presence watch in this integration harness.
        presence_resync_rx: None,
    }));

    let crdt_state = Arc::new(Mutex::new(OwnerState::default()));
    let hlc_tracker = Arc::new(Mutex::new(harmony_crdt_sync::ReplayTracker::new(
        "joiner-dev".to_string(),
    )));

    // Pre-call snapshot: joiner has no Spaces.
    {
        let g = crdt_state.lock().await;
        assert!(
            g.spaces.is_empty(),
            "joiner owner-state must start empty (no Spaces)"
        );
    }

    // ── Click: drive redeem_invite_inner with the URL from the entry. ─
    let result = harmony_app::redeem_invite_inner(
        click_to_join_url,
        Arc::clone(&crdt_state),
        Arc::clone(&hlc_tracker),
        harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
        "joiner-dev".into(),
        joiner_owner,
        Arc::clone(&joiner_signing_key),
        joiner.cert.clone(),
        Arc::clone(&community_registry),
        community_adapter_tx,
        None, // ZEB-434: no transport-epoch watch in this test
        Arc::clone(&dm_outbox),
        Arc::clone(&channel_log_registry),
        || Ok(()),
        None,
    )
    .await;

    let dto = result.expect("click-to-join redeem must succeed for OPEN invite");
    assert_eq!(
        dto.community_id,
        hex::encode(community_id.0),
        "DTO must echo the founder's community_id"
    );
    assert!(!dto.is_invite_only, "URL was OPEN");
    assert_eq!(dto.community_name, "Founder Community");

    // ── Assert: founder's community Space landed in joiner's owner-state. ─
    let g = crdt_state.lock().await;
    let space = g
        .spaces
        .get(&community_id)
        .expect("joiner OwnerState.spaces must contain the founder's community after redeem");
    assert_eq!(space.kind, SpaceKind::Community);
    assert_eq!(space.id, community_id);
    assert_eq!(space.name, "Founder Community");
    assert_eq!(
        space.admin_addr,
        Some(founder_owner_addr),
        "Space.admin_addr must carry the URL's admin_addr"
    );
    assert_eq!(space.is_invite_only, Some(false));

    drop(g);
    community_registry
        .shutdown_all()
        .await
        .expect("registry shutdown");
}

// ── ZEB-280 Sub-D Phase 3: federation integration tests ──────────────

/// ZEB-280 Phase 3: library A broadcasts a wrapped entry; library B
/// re-syndicates the SAME admin-signed bytes verbatim, replacing only
/// the wrapping sig with its own. This is the verbatim re-syndication
/// primitive — the admin sig is portable across libraries, so B can
/// rebroadcast A's admin-signed entry without re-signing the inner
/// payload. Aggregation should treat them as 2 distinct broadcasting
/// attestations of the same community. DTO surfaces
/// `listed_by_count = 2`, `unattested = false`.
///
/// Re-syndication preserves the entry's `listed_at` HLC verbatim, so
/// the aggregation's `incoming_newer` check on B's `process_sample`
/// sees an equal-or-not-newer HLC and keeps `existing.entry = entry_a`.
/// `attested_by` still gains `library_b` (set insert) — the assertions
/// below match this expected behavior.
#[tokio::test]
async fn federation_two_libraries_broadcast_same_community_aggregates() {
    use common::library_fixtures::{
        build_test_library_addr, mock_library_entry_republished_by, mock_library_entry_wrapped,
    };

    let community_id = SpaceId([0x88; 16]);
    let admin_seed = [42u8; 32];

    let (lib_a_signer, lib_a_bundle, library_a) = build_test_library_addr([1u8; 32]);
    let (lib_b_signer, lib_b_bundle, library_b) = build_test_library_addr([2u8; 32]);

    // Library A: produce the original admin-signed + A-wrapped entry.
    let entry_a = mock_library_entry_wrapped(
        community_id,
        admin_seed,
        library_a,
        Hlc {
            wall_ms: 1_700_000_000_000,
            logical: 0,
            device_id: "test-a".to_string(),
        },
        open_invite_url_for(community_id, admin_seed),
        "Federated Community",
        "Same community, two libraries.",
        vec!["federation".to_string()],
        Some((&lib_a_signer, lib_a_bundle)),
    );
    // Library B: verbatim re-syndication — clone A's entry and replace
    // ONLY the wrapping sig with B's own. listed_by and listed_at HLC
    // stay as A's (this is what "verbatim" means: the admin-signed
    // bytes are byte-identical to what A published).
    let entry_b = mock_library_entry_republished_by(&entry_a, &lib_b_signer, lib_b_bundle);

    let dir = build_directory();
    let bytes_a = canonical_cbor_encode(&entry_a).expect("encode a");
    let bytes_b = canonical_cbor_encode(&entry_b).expect("encode b");
    dir.process_sample(library_a, bytes_a)
        .await
        .expect("process a");
    dir.process_sample(library_b, bytes_b)
        .await
        .expect("process b");

    let snap = dir.snapshot_all().await;
    assert_eq!(
        snap.len(),
        1,
        "single community aggregated across libraries"
    );
    assert_eq!(
        snap[0].attested_by.len(),
        2,
        "both broadcasting libraries attested"
    );
    assert!(snap[0].attested_by.contains(&library_a));
    assert!(snap[0].attested_by.contains(&library_b));
    assert!(snap[0].unattested_by.is_empty(), "no unattested broadcasts");

    let dto = DirectoryEntryDTO::from_aggregated(&snap[0]);
    assert_eq!(dto.listed_by_count, 2);
    assert!(!dto.unattested);
}

/// ZEB-280 Phase 3: library A broadcasts a valid wrapped entry;
/// library B broadcasts the same community but with a TAMPERED
/// wrapping sig. Aggregation: attested_by = {A}, unattested_by = {B},
/// DTO: listed_by_count = 1, unattested = true (badge surfaces).
#[tokio::test]
async fn federation_one_library_tampered_wrapping_shows_unattested() {
    use common::library_fixtures::{build_test_library_addr, mock_library_entry_wrapped};

    let community_id = SpaceId([0x99; 16]);
    let admin_seed = [43u8; 32];

    let (lib_a_signer, lib_a_bundle, library_a) = build_test_library_addr([3u8; 32]);
    let (lib_b_signer, lib_b_bundle, library_b) = build_test_library_addr([4u8; 32]);

    // Library A: valid wrapping.
    let entry_a = mock_library_entry_wrapped(
        community_id,
        admin_seed,
        library_a,
        Hlc {
            wall_ms: 1_700_000_000_000,
            logical: 0,
            device_id: "test-a".to_string(),
        },
        open_invite_url_for(community_id, admin_seed),
        "Tampered Test",
        "One good wrap, one bad wrap.",
        vec![],
        Some((&lib_a_signer, lib_a_bundle)),
    );

    // Library B: produce a valid wrap first, then TAMPER the wrapping sig.
    let mut entry_b = mock_library_entry_wrapped(
        community_id,
        admin_seed,
        library_b,
        Hlc {
            wall_ms: 1_700_000_000_001,
            logical: 0,
            device_id: "test-b".to_string(),
        },
        open_invite_url_for(community_id, admin_seed),
        "Tampered Test",
        "One good wrap, one bad wrap.",
        vec![],
        Some((&lib_b_signer, lib_b_bundle)),
    );
    let mut tampered_sig = entry_b.library_signature.expect("sig present");
    tampered_sig[0] ^= 0xFF;
    entry_b.library_signature = Some(tampered_sig);

    let dir = build_directory();
    let bytes_a = canonical_cbor_encode(&entry_a).expect("encode a");
    let bytes_b = canonical_cbor_encode(&entry_b).expect("encode b");
    dir.process_sample(library_a, bytes_a)
        .await
        .expect("process a");
    dir.process_sample(library_b, bytes_b)
        .await
        .expect("process b (unattested but NOT dropped)");

    let snap = dir.snapshot_all().await;
    assert_eq!(snap.len(), 1, "tampered entry still surfaced");
    assert!(
        snap[0].attested_by.contains(&library_a),
        "library_a attested via valid wrap"
    );
    assert!(
        snap[0].unattested_by.contains(&library_b),
        "library_b in unattested_by (bad wrap)"
    );
    assert!(!snap[0].attested_by.contains(&library_b));

    let dto = DirectoryEntryDTO::from_aggregated(&snap[0]);
    assert_eq!(
        dto.listed_by_count, 1,
        "only library_a counted in listed_by_count"
    );
    assert!(dto.unattested, "DTO unattested = true triggers UI badge");
}

/// ZEB-280 Phase 3: a Phase 1-style entry (no wrapping sig) and a
/// Phase 3 wrapped entry from different libraries aggregate to the
/// same community. Both contribute to attested_by. DTO unattested = false.
/// Tests cross-version wire compat.
#[tokio::test]
async fn federation_phase1_entry_aggregates_alongside_phase3_wrapped() {
    use common::library_fixtures::{
        build_test_library_addr, mock_directory_entry, mock_library_entry_wrapped,
    };

    let community_id = SpaceId([0xAA; 16]);
    let admin_seed = [44u8; 32];

    let library_a = OwnerAddr([0xA1; 16]); // Phase 1 library (no key pair needed)
    let (lib_b_signer, lib_b_bundle, library_b) = build_test_library_addr([5u8; 32]);

    // Library A: Phase 1 unwrapped entry (no wrapping sig).
    let entry_a = mock_directory_entry(
        community_id,
        admin_seed,
        library_a,
        Hlc {
            wall_ms: 1_700_000_000_000,
            logical: 0,
            device_id: "test-a".to_string(),
        },
        open_invite_url_for(community_id, admin_seed),
        "Mixed Mode",
        "Phase 1 + Phase 3 in the same aggregation.",
        vec![],
    );

    // Library B: Phase 3 wrapped entry, same community.
    let entry_b = mock_library_entry_wrapped(
        community_id,
        admin_seed,
        library_b,
        Hlc {
            wall_ms: 1_700_000_000_001,
            logical: 0,
            device_id: "test-b".to_string(),
        },
        open_invite_url_for(community_id, admin_seed),
        "Mixed Mode",
        "Phase 1 + Phase 3 in the same aggregation.",
        vec![],
        Some((&lib_b_signer, lib_b_bundle)),
    );

    let dir = build_directory();
    let bytes_a = canonical_cbor_encode(&entry_a).expect("encode a");
    let bytes_b = canonical_cbor_encode(&entry_b).expect("encode b");
    dir.process_sample(library_a, bytes_a)
        .await
        .expect("process Phase 1 entry");
    dir.process_sample(library_b, bytes_b)
        .await
        .expect("process Phase 3 wrapped entry");

    let snap = dir.snapshot_all().await;
    assert_eq!(snap.len(), 1);
    assert!(
        snap[0].attested_by.contains(&library_a),
        "Phase 1 entry: Unwrapped status falls back to entry.listed_by = library_a"
    );
    assert!(
        snap[0].attested_by.contains(&library_b),
        "Phase 3 entry: Attested(library_b)"
    );
    assert!(snap[0].unattested_by.is_empty());

    let dto = DirectoryEntryDTO::from_aggregated(&snap[0]);
    assert_eq!(dto.listed_by_count, 2);
    assert!(!dto.unattested, "no unattested contributions");
}

/// ZEB-280 Phase 3: library_directory::drop_library walks BOTH the
/// attested_by and unattested_by sets and sweeps the dropped library
/// from each. A library that was only in unattested_by (no valid
/// attestation, only bad-sig attempts) is still cleanly removed.
#[tokio::test]
async fn federation_remove_library_evicts_attested_and_unattested_contributions() {
    use common::library_fixtures::{build_test_library_addr, mock_library_entry_wrapped};

    let community_id = SpaceId([0xBB; 16]);
    let admin_seed = [45u8; 32];

    let (lib_a_signer, lib_a_bundle, library_a) = build_test_library_addr([6u8; 32]);
    let (lib_b_signer, lib_b_bundle, library_b) = build_test_library_addr([7u8; 32]);

    // Library A: valid wrapping. HLC is NEWER than B's so the stored
    // entry's `listed_by` stays `library_a` (matters for drop_library:
    // it evicts when stored `entry.listed_by == dropped_library`,
    // independent of attested_by membership — see spec §5.3).
    let entry_a = mock_library_entry_wrapped(
        community_id,
        admin_seed,
        library_a,
        Hlc {
            wall_ms: 1_700_000_000_001,
            logical: 0,
            device_id: "test-a".to_string(),
        },
        open_invite_url_for(community_id, admin_seed),
        "Remove Test",
        "A is attested, B is unattested (tampered).",
        vec![],
        Some((&lib_a_signer, lib_a_bundle)),
    );

    // Library B: tampered wrapping. Older HLC than A so it never
    // overwrites the stored entry — only its unattested broadcast is
    // recorded in `unattested_by`.
    let mut entry_b = mock_library_entry_wrapped(
        community_id,
        admin_seed,
        library_b,
        Hlc {
            wall_ms: 1_700_000_000_000,
            logical: 0,
            device_id: "test-b".to_string(),
        },
        open_invite_url_for(community_id, admin_seed),
        "Remove Test",
        "A is attested, B is unattested (tampered).",
        vec![],
        Some((&lib_b_signer, lib_b_bundle)),
    );
    let mut tampered = entry_b.library_signature.expect("sig present");
    tampered[0] ^= 0xFF;
    entry_b.library_signature = Some(tampered);

    let dir = build_directory();
    dir.process_sample(
        library_a,
        canonical_cbor_encode(&entry_a).expect("encode a"),
    )
    .await
    .expect("process a");
    dir.process_sample(
        library_b,
        canonical_cbor_encode(&entry_b).expect("encode b"),
    )
    .await
    .expect("process b unattested");

    // Confirm initial state.
    let snap = dir.snapshot_all().await;
    assert_eq!(snap.len(), 1);
    assert!(snap[0].attested_by.contains(&library_a));
    assert!(snap[0].unattested_by.contains(&library_b));

    // Drop library_b — should sweep it from unattested_by but NOT
    // evict the community (library_a still attests).
    let evicted = dir.drop_library(&library_b).await;
    assert!(
        evicted.is_empty(),
        "library_b drop should NOT evict (library_a still attests)"
    );
    let snap_after_b = dir.snapshot_all().await;
    assert_eq!(snap_after_b.len(), 1);
    assert!(snap_after_b[0].attested_by.contains(&library_a));
    assert!(snap_after_b[0].unattested_by.is_empty());

    // Drop library_a — should evict (no more attestations).
    let evicted = dir.drop_library(&library_a).await;
    assert_eq!(
        evicted,
        vec![community_id],
        "library_a drop evicts community"
    );
    let snap_final = dir.snapshot_all().await;
    assert!(snap_final.is_empty());
}
