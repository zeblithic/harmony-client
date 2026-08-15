//! ZEB-918: after a membership epoch rotation the publisher's NEXT refresh
//! must publish the community beacon under the NEW epoch key, while the OLD
//! key's last record remains resolvable until it ages out — the natural
//! overlap window that (together with the gateway ladder's previous-epoch
//! candidate) replaces the pre-fix behavior, where an un-restarted process
//! pinned the spawn-time key forever and a restarted one hard-cut discovery
//! for every stale resolver.
//!
//! Full stack, mirroring `zeb880_record_size.rs`: real
//! `CommunityRendezvousPublisher` → real `PkarrPublisher` → strict
//! `MockPkarrRelay` (payloads validated as real pkarr relay payloads),
//! resolved back via `PkarrResolver`.

use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use harmony_app::community_rendezvous::{decode_rendezvous_blob, rendezvous_slot_verifying_key};
use harmony_app::community_rendezvous_publisher::CommunityRendezvousPublisher;
use harmony_app::owner_state_types::{EpochKey, OwnerAddr, SpaceId};
use harmony_app::reachability_record::ReachabilityAnnouncePayload;
use harmony_pkarr::{
    current_epoch_id, testing::MockPkarrRelay, PkarrPublisher, PkarrResolver, RelayClient,
    RelayPool,
};

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Small single-address host blob — record size is ZEB-880's concern, not
/// this test's; keep the payload comfortably under the cap.
fn small_blob() -> Vec<u8> {
    let payload = ReachabilityAnnouncePayload {
        iroh_node_id: [0x03; 32],
        home_relay_url: "https://r.example/".to_string(),
        direct_addresses: vec!["192.168.1.59:63933".parse().unwrap()],
        announced_at_ms: now_ms(),
        identity_signature: [0u8; 64],
        butler_set: vec![],
        bs_at: now_ms(),
    };
    let mut buf = Vec::new();
    ciborium::into_writer(&payload, &mut buf).expect("encode routing blob");
    buf
}

/// Poll until the slot-0 record for `epoch_key` is resolvable, trying both
/// the current and previous WEEKLY time epoch per attempt (epoch-boundary
/// tolerance, as in `zeb880_record_size.rs`). Fresh resolver per attempt to
/// sidestep the 60 s negative cache. Returns `None` on deadline.
async fn poll_slot0(
    client: &Arc<RelayClient>,
    epoch_key: &EpochKey,
    deadline: tokio::time::Instant,
) -> Option<harmony_pkarr::PkarrRoutingRecord> {
    loop {
        let epoch_now = current_epoch_id(now_ms());
        for epoch_id in [epoch_now, epoch_now.saturating_sub(1)] {
            let vk = rendezvous_slot_verifying_key(epoch_key, 0, epoch_id);
            let resolver = PkarrResolver::new(Arc::clone(client));
            if let Ok(Some(rec)) = resolver.resolve(&vk).await {
                return Some(rec);
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn rotation_rekeys_beacon_on_next_refresh_and_old_record_stays_until_it_ages_out() {
    let result = tokio::time::timeout(Duration::from_secs(60), async {
        let relay = MockPkarrRelay::start_strict().await;
        let pool = RelayPool::new(vec![relay.base_url.clone()]);
        let client = Arc::new(RelayClient::new(pool));
        let publisher = Arc::new(PkarrPublisher::new(Arc::clone(&client)));
        let _ph = Arc::clone(&publisher).spawn();

        let id_sk = SigningKey::from_bytes(&[0x55; 32]);
        let mut id_pub = [0u8; 64];
        id_pub[32..].copy_from_slice(&id_sk.verifying_key().to_bytes());
        let device_sk = Arc::new(SigningKey::from_bytes(&[0x66; 32]));

        let rdv = CommunityRendezvousPublisher::new(
            Arc::clone(&publisher),
            id_sk,
            id_pub,
            device_sk,
            Arc::new(small_blob),
        );

        let community = SpaceId([0x6f; 16]);
        let k1 = EpochKey::new([0x41; 32]);
        let k2 = EpochKey::new([0x42; 32]);
        let me = OwnerAddr([0x01; 16]);

        // Pre-rotation publish under K1.
        rdv.refresh_slot(community, k1.clone(), vec![(me, String::new())], me)
            .await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        let rec_k1 = poll_slot0(&client, &k1, deadline)
            .await
            .expect("pre-rotation record must publish under K1");
        let (payload_k1, vouch_k1) =
            decode_rendezvous_blob(&rec_k1.routing_blob).expect("K1 blob decodes");
        assert!(vouch_k1.is_some(), "vouch must ride the K1 beacon");
        assert!(!payload_k1.direct_addresses.is_empty());

        // Rotation: the caller (the live-key read in the slot-refresh arm)
        // now supplies K2 on the next refresh — same slot, same handle.
        rdv.refresh_slot(community, k2.clone(), vec![(me, String::new())], me)
            .await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        let rec_k2 = poll_slot0(&client, &k2, deadline).await.expect(
            "post-rotation refresh must publish under K2 — a pinned \
             spawn-time key would keep publishing under K1 forever (ZEB-918)",
        );
        let (payload_k2, vouch_k2) =
            decode_rendezvous_blob(&rec_k2.routing_blob).expect("K2 blob decodes");
        assert!(
            vouch_k2.is_some(),
            "publisher invariants must survive the rekey: vouch present"
        );
        assert!(!payload_k2.direct_addresses.is_empty());

        // Natural overlap window: the K1 record is not deleted by the rekey —
        // it stays on the relay (until relay expiry / freshness ageout), so
        // stale resolvers (an un-rotated member, a rotation-crossing invite)
        // don't hard-cut at the instant of rotation.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let rec_k1_after = poll_slot0(&client, &k1, deadline)
            .await
            .expect("old-key record must remain resolvable after the rekey");
        assert!(
            decode_rendezvous_blob(&rec_k1_after.routing_blob).is_some(),
            "old-key record still decodes during the natural window"
        );
    })
    .await;
    result.expect("test timed out");
}
