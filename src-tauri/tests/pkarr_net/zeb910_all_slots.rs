//! ZEB-910: the all-slots rendezvous resolve returns EVERY publisher's
//! verified beacon — the bridge-both-islands shape the escalating single-hit
//! resolve cannot produce (it stops at the first found slot, which on a
//! community split is coin-flip likely to be an already-reachable member).
//!
//! Full stack, mirroring `zeb918_epoch_rotation.rs`: two real
//! `CommunityRendezvousPublisher`s (distinct identities/devices/slots, one
//! epoch key) → real `PkarrPublisher` → strict `MockPkarrRelay`, resolved
//! back via `resolve_rendezvous_all_slots`.

use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use harmony_app::community_rendezvous::resolve_rendezvous_all_slots;
use harmony_app::community_rendezvous_publisher::CommunityRendezvousPublisher;
use harmony_app::owner_state_types::{EpochKey, OwnerAddr, SpaceId};
use harmony_app::reachability_record::ReachabilityAnnouncePayload;
use harmony_pkarr::{
    testing::MockPkarrRelay, PkarrPublisher, PkarrResolver, RelayClient, RelayPool,
};

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Single-address host blob for `node_id` (record size is ZEB-880's concern,
/// not this test's).
fn blob_for(node_id: [u8; 32]) -> Vec<u8> {
    let payload = ReachabilityAnnouncePayload {
        iroh_node_id: node_id,
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

/// One publisher: identity seed, device seed, and the node id its blob
/// advertises. Returns (publisher, device verifying key bytes).
fn make_publisher(
    pkarr_publisher: &Arc<PkarrPublisher>,
    id_seed: u8,
    device_seed: u8,
    node_id: [u8; 32],
) -> (CommunityRendezvousPublisher, [u8; 32]) {
    let id_sk = SigningKey::from_bytes(&[id_seed; 32]);
    let mut id_pub = [0u8; 64];
    id_pub[32..].copy_from_slice(&id_sk.verifying_key().to_bytes());
    let device_sk = Arc::new(SigningKey::from_bytes(&[device_seed; 32]));
    let device_vk = device_sk.verifying_key().to_bytes();
    let rdv = CommunityRendezvousPublisher::new(
        Arc::clone(pkarr_publisher),
        id_sk,
        id_pub,
        device_sk,
        Arc::new(move || blob_for(node_id)),
    );
    (rdv, device_vk)
}

#[tokio::test]
async fn all_slots_resolve_returns_every_publishers_beacon() {
    let result = tokio::time::timeout(Duration::from_secs(60), async {
        let relay = MockPkarrRelay::start_strict().await;
        let pool = RelayPool::new(vec![relay.base_url.clone()]);
        let client = Arc::new(RelayClient::new(pool));
        let publisher = Arc::new(PkarrPublisher::new(Arc::clone(&client)));
        let _ph = Arc::clone(&publisher).spawn();

        let community = SpaceId([0x6a; 16]);
        let epoch_key = EpochKey::new([0x51; 32]);
        let (node_a, node_b) = ([0x0A; 32], [0x0B; 32]);
        // Addresses chosen so A ranks slot 0 and B slot 1 (ascending sort).
        let (owner_a, owner_b) = (OwnerAddr([0x01; 16]), OwnerAddr([0x02; 16]));
        let advertisers = vec![owner_a, owner_b];

        let (rdv_a, vk_a) = make_publisher(&publisher, 0x55, 0x66, node_a);
        let (rdv_b, vk_b) = make_publisher(&publisher, 0x57, 0x68, node_b);
        rdv_a
            .refresh_slot(community, epoch_key.clone(), advertisers.clone(), owner_a)
            .await;
        rdv_b
            .refresh_slot(community, epoch_key.clone(), advertisers.clone(), owner_b)
            .await;

        let enrolled: Arc<std::collections::HashSet<[u8; 32]>> =
            Arc::new([vk_a, vk_b].into_iter().collect());
        // PR #659 review: fixed test-local config — a repo-wide
        // HARMONY_OPEN_JOIN_RESOLVE_DEADLINE_MS override must not be able to
        // time out every probe and fail this test around a correct impl.
        let cfg = harmony_pkarr::rendezvous::RendezvousResolveConfig {
            batch_curve: vec![1, 2, 8],
            per_batch_deadline: Duration::from_millis(2_500),
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        // Fresh resolver per attempt to sidestep the 60 s negative cache;
        // self endpoint id matches neither publisher.
        loop {
            let resolver = Arc::new(PkarrResolver::new(Arc::clone(&client)));
            let res = resolve_rendezvous_all_slots(
                &resolver,
                &epoch_key,
                [0xEE; 32],
                community,
                Arc::clone(&enrolled),
                now_ms(),
                &cfg,
            )
            .await;
            let mut nodes: Vec<[u8; 32]> =
                res.hits.iter().map(|h| h.payload.iroh_node_id).collect();
            nodes.sort();
            if nodes == vec![node_a, node_b] {
                assert_eq!(
                    res.membership_rejects, 0,
                    "both vouches must verify against the enrolled set"
                );
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "all-slots resolve must eventually return BOTH publishers' \
                 beacons; last attempt saw {} hit(s)",
                res.hits.len()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
    result.expect("test timed out");
}
