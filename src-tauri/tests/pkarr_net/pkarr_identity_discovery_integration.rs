//! Case B integration test: pkarr identity-keyed publish → resolve.
//!
//! Alice enables identity discoverability via `PkarrIdentityPublisher::enable()`.
//! Bob derives the same HKDF(owner_identity_pub, epoch) key and resolves via
//! `PkarrResolver`. Test uses `MockPkarrRelay` as the relay.

use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use harmony_app::pkarr_identity_publisher::PkarrIdentityPublisher;
use harmony_app::reachability_record::ReachabilityAnnouncePayload;
use harmony_pkarr::{
    current_epoch_id, derive_ephemeral_key, testing::MockPkarrRelay, PkarrCase, PkarrPublisher,
    PkarrResolver, RelayClient, RelayPool,
};

fn build_identity_pub(sk: &SigningKey) -> [u8; 64] {
    let mut out = [0u8; 64];
    out[32..].copy_from_slice(&sk.verifying_key().to_bytes());
    out
}

fn fixture_routing_blob(iroh_node_id: [u8; 32]) -> Vec<u8> {
    let payload = ReachabilityAnnouncePayload {
        iroh_node_id,
        home_relay_url: "https://identity-relay.test/".into(),
        direct_addresses: vec![],
        announced_at_ms: 1_700_000_000_000,
        identity_signature: [0xEE; 64],
        butler_set: Vec::new(),
        bs_at: 0,
    };
    let mut buf = Vec::new();
    ciborium::into_writer(&payload, &mut buf).expect("encode routing_blob");
    buf
}

#[tokio::test]
async fn case_b_identity_publish_then_resolve_round_trip() {
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        // --- Setup ---
        let relay = MockPkarrRelay::start().await;
        let pool = RelayPool::new(vec![relay.base_url.clone()]);
        let client = Arc::new(RelayClient::new(pool));

        let publisher = Arc::new(PkarrPublisher::new(Arc::clone(&client)));
        let _ph = Arc::clone(&publisher).spawn();

        // Alice's identity key (deterministic for test reproducibility).
        let alice_sk = SigningKey::from_bytes(&[0x55u8; 32]);
        let alice_identity_pub = build_identity_pub(&alice_sk);
        let alice_iroh_node_id = [0xBBu8; 32];

        let routing_blob_builder = {
            let iroh_id = alice_iroh_node_id;
            Arc::new(move || fixture_routing_blob(iroh_id))
        };

        let id_pub = PkarrIdentityPublisher::new(
            Arc::clone(&publisher),
            alice_sk.clone(),
            alice_identity_pub,
            routing_blob_builder,
        );

        // --- Case B: Alice enables identity discoverability ---
        id_pub.enable().await;
        assert!(
            publisher
                .active_handles()
                .await
                .contains(&"identity".to_string()),
            "identity handle must be registered after enable()"
        );

        // --- Bob's side: derive the same key from alice_identity_pub + current epoch ---
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time")
            .as_millis() as u64;
        let epoch_id = current_epoch_id(now_ms);
        let bob_signing = derive_ephemeral_key(
            PkarrCase::Identity,
            &alice_identity_pub,
            &epoch_id.to_be_bytes(),
        );
        let bob_verifying = bob_signing.verifying_key();

        // --- Poll the relay until the record appears (up to 5s) ---
        let resolver = PkarrResolver::new(Arc::clone(&client));
        let mut record = None;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if let Ok(Some(rec)) = resolver.resolve(&bob_verifying).await {
                record = Some(rec);
                break;
            }
        }
        let record = record.expect("record should appear within 5s");

        // --- Verify inner signature (RPK2) ---
        record
            .verify_inner_sig()
            .expect("inner sig must be valid (RPK2)");

        // --- Verify identity match (RPK3): record must carry alice's identity_pub ---
        record
            .verify_identity_match(&alice_identity_pub)
            .expect("identity_pub must match alice's (RPK3)");

        assert_eq!(
            record.harmony_identity_pub, alice_identity_pub,
            "record must carry alice's identity_pub"
        );

        // --- Decode routing_blob and verify iroh_node_id ---
        let decoded_payload: ReachabilityAnnouncePayload =
            ciborium::from_reader(record.routing_blob.as_slice())
                .expect("routing_blob must decode as ReachabilityAnnouncePayload");
        assert_eq!(
            decoded_payload.iroh_node_id, alice_iroh_node_id,
            "decoded routing must carry alice's iroh node id"
        );
        assert_eq!(
            decoded_payload.home_relay_url, "https://identity-relay.test/",
            "relay URL must match"
        );
    })
    .await;

    result.expect("case B integration test timed out");
}
