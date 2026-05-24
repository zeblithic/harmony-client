//! Case C integration test: in-community pkarr fallback via `PkarrResolverAdapter`.
//!
//! Alice publishes her iroh routing per-community via `PkarrCommunityPublisher`.
//! A `PkarrResolverAdapter` with a deterministic `contexts_fn` resolves alice's
//! record when queried via `ReachabilityResolver::resolve_async()`. Also verifies
//! warm-cache: a subsequent sync `resolve()` returns the same record without a
//! second pkarr round-trip.
//!
//! This test constructs the adapter directly and does NOT rely on Task 8's
//! stubbed `contexts_fn` closure in lib.rs.

use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use harmony_app::owner_state_types::{OwnerAddr, SpaceId};
use harmony_app::pkarr_community_publisher::PkarrCommunityPublisher;
use harmony_app::pkarr_resolver_adapter::{PkarrCommunityContext, PkarrResolverAdapter};
use harmony_app::reachability_record::ReachabilityAnnouncePayload;
use harmony_app::reachability_resolver::ReachabilityResolver;
use harmony_pkarr::{
    testing::MockPkarrRelay, PkarrPublisher, PkarrResolver, RelayClient, RelayPool,
};

fn build_identity_pub(sk: &SigningKey) -> [u8; 64] {
    let mut out = [0u8; 64];
    out[32..].copy_from_slice(&sk.verifying_key().to_bytes());
    out
}

fn fixture_routing_blob(iroh_node_id: [u8; 32]) -> Vec<u8> {
    let payload = ReachabilityAnnouncePayload {
        iroh_node_id,
        home_relay_url: "https://community-relay.test/".into(),
        direct_addresses: vec![],
        announced_at_ms: 1_700_000_000_000,
        identity_signature: [0xFFu8; 64],
    };
    let mut buf = Vec::new();
    ciborium::into_writer(&payload, &mut buf).expect("encode routing_blob");
    buf
}

#[tokio::test]
async fn case_c_community_fallback_resolve_and_warm_cache() {
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        // --- Setup ---
        let relay = MockPkarrRelay::start().await;
        let pool = RelayPool::new(vec![relay.base_url.clone()]);
        let client = Arc::new(RelayClient::new(pool));

        let publisher = Arc::new(PkarrPublisher::new(Arc::clone(&client)));
        let _ph = Arc::clone(&publisher).spawn();

        // Deterministic test fixtures.
        let alice_sk = SigningKey::from_bytes(&[0x77u8; 32]);
        let alice_identity_pub = build_identity_pub(&alice_sk);
        let alice_iroh_node_id = [0xCCu8; 32];
        let alice_owner_addr = OwnerAddr([0x22u8; 16]);

        // Community + epoch key shared between alice (publisher) and the resolver.
        let community_id = SpaceId([0x33u8; 16]);
        let epoch_key = [0xBBu8; 32];

        // Alice publishes routing via PkarrCommunityPublisher.
        let routing_blob_builder = {
            let iroh_id = alice_iroh_node_id;
            Arc::new(move || fixture_routing_blob(iroh_id))
        };
        let com_pub = PkarrCommunityPublisher::new(
            Arc::clone(&publisher),
            alice_sk.clone(),
            alice_identity_pub,
            routing_blob_builder,
        );
        com_pub.on_community_joined(community_id, epoch_key).await;

        // --- Build PkarrResolverAdapter with a deterministic contexts_fn ---
        // The contexts_fn returns one community context whenever the target
        // addr matches alice's owner_addr. In real code this would be driven
        // by NodeState; here we hardcode a closure that tests the code path.
        let pkarr_resolver = Arc::new(PkarrResolver::new(Arc::clone(&client)));
        let contexts_fn: harmony_app::pkarr_resolver_adapter::ContextsFn =
            Arc::new(move |addr: &OwnerAddr| {
                if *addr == alice_owner_addr {
                    vec![PkarrCommunityContext {
                        community_id,
                        epoch_key,
                        target_member_identity_pub: alice_identity_pub,
                    }]
                } else {
                    vec![]
                }
            });
        let adapter = Arc::new(PkarrResolverAdapter::new(
            Arc::clone(&pkarr_resolver),
            contexts_fn,
        ));

        // --- Wire adapter into Phase 1's ReachabilityResolver ---
        let rr = ReachabilityResolver::new();
        rr.set_fallback_source(adapter);

        // Poll the relay until alice's publish lands (up to 5s),
        // then call resolve_async — the adapter should find the record.
        let mut resolved = Vec::new();
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let payloads = rr.resolve_async(&alice_owner_addr).await;
            if !payloads.is_empty() {
                resolved = payloads;
                break;
            }
        }

        assert!(
            !resolved.is_empty(),
            "adapter must return at least one payload within 5s"
        );
        let payload = &resolved[0];
        assert_eq!(
            payload.iroh_node_id, alice_iroh_node_id,
            "decoded routing must carry alice's iroh node id"
        );
        assert_eq!(
            payload.home_relay_url, "https://community-relay.test/",
            "relay URL must match"
        );

        // --- Warm-cache check: subsequent sync resolve() must return the same record ---
        let warm = rr.resolve(&alice_owner_addr);
        assert_eq!(
            warm.len(),
            1,
            "warm-cache sync resolve must return the fallback-populated record"
        );
        assert_eq!(
            warm[0].iroh_node_id, alice_iroh_node_id,
            "warm-cache entry must carry alice's iroh node id"
        );
    })
    .await;

    result.expect("case C integration test timed out");
}
