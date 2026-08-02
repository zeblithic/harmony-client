//! Sub-D Phase 4 (ZEB-281) integration tests at the
//! `ProfileBroadcastCache` layer.
//!
//! These tests model end-to-end protocol behavior at the cache boundary:
//! the publisher's payload bytes (built via `sign_broadcast` + canonical
//! CBOR — identical to what the production publisher emits) are handed
//! directly to a peer-side `ProfileBroadcastCache::on_sample` as if
//! Zenoh had transported them. Full Zenoh end-to-end is too heavy for
//! nextest (each test would need a multi-second session bootstrap); the
//! transport layer is covered by the Phase 2 announce integration
//! tests + the Task 4 wire-format pinning tests.
//!
//! Spec §11.3.

use crate::common;

use common::profile_fixtures::{
    build_test_owner_identity, fixture_hlc, fixture_space_id, mock_profile_broadcast,
};

use harmony_app::profile_broadcast::{
    CacheOnSampleError, CacheOnSampleOutcome, ProfileBroadcastCache,
};

/// Cache-level: register a subscription for peer P, deliver P's
/// broadcast via on_sample, get_cached returns the expected snapshot.
/// Spec §11.3 row 1.
#[tokio::test]
async fn peer_subscribe_receives_broadcast() {
    let cache = ProfileBroadcastCache::default();
    let (_signer, _identity_pub, peer_addr) = build_test_owner_identity([1u8; 32]);
    cache.register(1, peer_addr).await;

    let (_bytes, _addr, broadcast) = mock_profile_broadcast(
        [1u8; 32],
        vec![fixture_space_id(10), fixture_space_id(20)],
        fixture_hlc(1000, 0),
    );
    let outcome = cache
        .on_sample(1, broadcast, 0)
        .await
        .expect("on_sample should succeed for valid broadcast");
    assert_eq!(outcome, CacheOnSampleOutcome::InsertedFirst);

    let snap = cache.get_cached(1).await.expect("cached entry present");
    assert_eq!(snap.owner_addr, hex::encode(peer_addr.0));
    assert_eq!(snap.community_ids.len(), 2);
    // shared_at is `wall_ms.to_string()` — display-only.
    assert_eq!(snap.shared_at, "1000");
}

/// Adversary publishes on peer X's topic with peer Y's identity bundle —
/// `on_sample` returns `AttributionMismatch` because the broadcaster's
/// derived OwnerAddr does not equal the subscription's registered peer
/// addr. Cache stays empty. Spec §11.3 row 2.
#[tokio::test]
async fn attribution_mismatch_rejected() {
    let cache = ProfileBroadcastCache::default();
    let (_signer_x, _identity_pub_x, peer_x_addr) = build_test_owner_identity([1u8; 32]);
    cache.register(1, peer_x_addr).await;

    // Broadcast signed by seed [2u8; 32] → derives a different OwnerAddr.
    let (_bytes, _y_addr, broadcast_from_y) =
        mock_profile_broadcast([2u8; 32], vec![fixture_space_id(7)], fixture_hlc(1000, 0));
    let err = cache
        .on_sample(1, broadcast_from_y, 0)
        .await
        .expect_err("attribution mismatch must surface as Err");
    assert!(
        matches!(err, CacheOnSampleError::AttributionMismatch { .. }),
        "expected AttributionMismatch, got {err:?}"
    );
    // Cache MUST remain empty — a rejected broadcast must not poison
    // the cached state.
    assert!(cache.get_cached(1).await.is_none());
}

/// Subscribe → land a broadcast → drop_subscription → cache cleared,
/// re-registering and querying returns None (no broadcast yet), and
/// `on_sample` for an unknown id returns `SubscriptionNotFound`.
/// Spec §11.3 row 3.
#[tokio::test]
async fn subscribe_unsubscribe_lifecycle() {
    let cache = ProfileBroadcastCache::default();
    let (_signer, _identity_pub, peer_addr) = build_test_owner_identity([3u8; 32]);
    cache.register(7, peer_addr).await;

    let (_bytes, _addr, broadcast) =
        mock_profile_broadcast([3u8; 32], vec![fixture_space_id(1)], fixture_hlc(2000, 0));
    cache
        .on_sample(7, broadcast, 0)
        .await
        .expect("on_sample for registered subscription");
    assert!(cache.get_cached(7).await.is_some());

    cache.drop_subscription(7).await;
    assert!(
        cache.get_cached(7).await.is_none(),
        "after drop_subscription, cache must be empty for this id"
    );

    // Re-registering the same id: cache empty (new entry, no broadcast yet).
    cache.register(7, peer_addr).await;
    assert!(cache.get_cached(7).await.is_none());

    // After a second drop_subscription, on_sample for the (now-unknown)
    // id returns SubscriptionNotFound — proves drop is permanent.
    cache.drop_subscription(7).await;
    let (_bytes2, _addr2, b2) =
        mock_profile_broadcast([3u8; 32], vec![fixture_space_id(1)], fixture_hlc(3000, 0));
    let err = cache
        .on_sample(7, b2, 0)
        .await
        .expect_err("unknown subscription must surface as Err");
    assert!(
        matches!(err, CacheOnSampleError::SubscriptionNotFound(7)),
        "expected SubscriptionNotFound(7), got {err:?}"
    );
}

/// dev2's subscription to dev1's profile-broadcast topic accepts the
/// `set_shared_in_profile`-triggered publish from dev1: model the bytes
/// the publisher would emit + deliver them to a fresh cache as if Zenoh
/// transported them. Spec §11.3 row 4.
///
/// Cache-level only — full Zenoh end-to-end is too heavy for nextest;
/// the transport layer is covered by Phase 2's announce integration
/// test + Task 4's wire-format pinning tests.
#[tokio::test]
async fn self_publish_on_opt_in_change() {
    // Model the broadcast dev1's publisher would emit after the user
    // toggles `shared_in_profile = true` on community_id = fixture_space_id(42).
    let (_bytes, owner_addr, broadcast) =
        mock_profile_broadcast([99u8; 32], vec![fixture_space_id(42)], fixture_hlc(5000, 0));
    // dev2 (or any peer) subscribes to the owner's topic — for this
    // test we model the cache only; the Zenoh transport is mocked by
    // direct delivery to on_sample.
    let cache = ProfileBroadcastCache::default();
    cache.register(11, owner_addr).await;
    cache
        .on_sample(11, broadcast, 0)
        .await
        .expect("dev2 should receive + verify");
    let snap = cache.get_cached(11).await.expect("cached after on_sample");
    assert_eq!(snap.community_ids, vec![hex::encode([42u8; 16])]);
    assert_eq!(snap.shared_at, "5000");
}

/// Rotation: dev1 publisher's N→0 rotation publish (empty community_ids,
/// strictly-newer HLC) supersedes the prior non-empty broadcast at peer
/// caches. Spec §11.3 row 5.
#[tokio::test]
async fn self_publish_rotation_to_empty() {
    let cache = ProfileBroadcastCache::default();
    let (_signer, _identity_pub, owner_addr) = build_test_owner_identity([55u8; 32]);
    cache.register(13, owner_addr).await;

    // First publish: non-empty.
    let (_bytes1, _addr1, b1) = mock_profile_broadcast(
        [55u8; 32],
        vec![fixture_space_id(1), fixture_space_id(2)],
        fixture_hlc(1000, 0),
    );
    cache
        .on_sample(13, b1, 0)
        .await
        .expect("first publish accepted");
    assert_eq!(
        cache
            .get_cached(13)
            .await
            .expect("cached after first publish")
            .community_ids
            .len(),
        2
    );

    // Rotation: empty community_ids, strictly newer HLC.
    let (_bytes2, _addr2, b2) = mock_profile_broadcast([55u8; 32], vec![], fixture_hlc(2000, 0));
    let outcome = cache
        .on_sample(13, b2, 0)
        .await
        .expect("rotation publish accepted");
    assert_eq!(
        outcome,
        CacheOnSampleOutcome::Replaced,
        "rotation must replace, not insert-first"
    );
    let snap = cache
        .get_cached(13)
        .await
        .expect("cached after rotation publish");
    assert_eq!(
        snap.community_ids.len(),
        0,
        "rotation must overwrite non-empty with empty"
    );
    assert_eq!(snap.shared_at, "2000");
}
