//! ZEB-341 cross-peer: owner A's card verifies + caches under A's owner_id for
//! a subscriber; a card signed by B but claiming A's owner_id is rejected by the
//! cert-model `verify_card` gate (the same gate the event-loop subscriber pool
//! applies before `insert_verified`).
//!
//! Spinning a real Zenoh session is heavy and flaky in CI, so this exercises the
//! FULL verify + attribution + cache pipeline directly (the exact sequence the
//! pool task runs per received sample): decode -> verify_card -> attribution
//! (verified owner_id == subscribed owner_id) -> insert_verified.

use harmony_app::community_membership::mint_test_owner;
use harmony_app::owner_state_types::Hlc;
use harmony_app::profile_card_broadcast::{sign_card, verify_card, ProfileCardCache};

fn hlc(wall_ms: u64, device_id: &str) -> Hlc {
    Hlc {
        wall_ms,
        logical: 0,
        device_id: device_id.to_string(),
    }
}

/// Mirrors the event-loop pool's per-sample gate: a card is cached only when it
/// verifies AND its bound owner_id matches the subscribed owner_id.
async fn pool_ingest(
    cache: &ProfileCardCache,
    sub: u64,
    subscribed_owner: [u8; 16],
    card: &harmony_app::profile_card_broadcast::ProfileCardBroadcast,
) {
    match verify_card(card, 0) {
        Ok(verified_owner) if verified_owner == subscribed_owner => {
            cache.insert_verified(sub, card).await;
        }
        _ => { /* dropped: invalid cert/sig or attribution mismatch */ }
    }
}

#[tokio::test]
async fn owner_b_resolves_owner_a_card_and_rejects_spoof() {
    let a = mint_test_owner(0x0A);
    let b = mint_test_owner(0x0B);

    // A publishes a valid card; it verifies and binds to A's owner_id.
    let a_card = sign_card(
        &a.device_key,
        a.owner.0,
        "Alice".into(),
        "hi".into(),
        None,
        None,
        a.cert.clone(),
        hlc(1, "a"),
    )
    .unwrap();
    assert_eq!(verify_card(&a_card, 0).unwrap(), a.owner.0);

    // B subscribes to A's owner_id; A's card caches through the pool gate.
    let cache = ProfileCardCache::default();
    cache.register(1, a.owner.0).await;
    pool_ingest(&cache, 1, a.owner.0, &a_card).await;
    let got = cache.get_cached(1).await.expect("A's card cached");
    assert_eq!(got.display_name, "Alice");
    assert_eq!(got.status_text, "hi");
    assert_eq!(got.owner_id_hex, hex::encode(a.owner.0));

    // Spoof: B signs a card but forges A's owner_id into the wire field. The
    // embedded cert is B's (cert.owner_id == B), so verify_card rejects it with
    // an owner-mismatch — it never reaches the cache.
    let mut spoof = sign_card(
        &b.device_key,
        b.owner.0,
        "NotAlice".into(),
        "".into(),
        None,
        None,
        b.cert.clone(),
        hlc(2, "b"), // NEWER HLC than A's card — would win newer-wins if it ever inserted.
    )
    .unwrap();
    spoof.owner_id = a.owner.0; // forge attribution to A.
    assert!(
        verify_card(&spoof, 0).is_err(),
        "spoof claiming A's owner_id with B's cert must fail verify_card"
    );

    // Drive the forged card through the SAME pool gate the production subscriber
    // uses. Despite its newer HLC and matching forged owner_id, the verify gate
    // drops it, so the cache still holds Alice's card unchanged.
    pool_ingest(&cache, 1, a.owner.0, &spoof).await;
    let still = cache.get_cached(1).await.expect("still cached");
    assert_eq!(
        still.display_name, "Alice",
        "spoof must not overwrite A's verified card"
    );
}
