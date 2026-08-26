//! Case A integration tests: pkarr invite-redemption publish → resolve,
//! plus the Phase 2c (ZEB-325) end-to-end orchestration test that drives
//! `connectivity_redeem_invite_iroh_inner` through to a `"joined"` outcome.
//!
//! Alice publishes her iroh routing under HKDF(invite_token.sig, epoch) via
//! `PkarrInvitePublisher`. Bob independently derives the same key from the
//! invite token and resolves via `PkarrResolver`. Tests use `MockPkarrRelay`
//! as the relay, so no live DHT is needed.

use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use harmony_app::community_invite::{
    CommunityInvitePayload, InviteEpochSnapshot, InviteToken, MaterializedCommunityState,
};
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};
use harmony_app::pkarr_invite_publisher::PkarrInvitePublisher;
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

fn fixture_hlc() -> Hlc {
    Hlc {
        wall_ms: 1_700_000_000_000,
        logical: 0,
        device_id: "test".into(),
    }
}

fn fixture_invite_token(inviter: OwnerAddr) -> InviteToken {
    InviteToken {
        inviter,
        invitee_hint: None,
        minted_at: fixture_hlc(),
        expires_at: None,
        sig: [0xAA; 64],
    }
}

fn fixture_invite_payload(inviter: OwnerAddr) -> CommunityInvitePayload {
    CommunityInvitePayload {
        inviter_signer_certs: Vec::new(),
        community_id: SpaceId([0x11; 16]),
        epoch_snapshot: InviteEpochSnapshot {
            epoch: 0,
            sealed_epoch_key: vec![0u8; 32],
            sealed_epoch_keys: Vec::new(),
            state_snapshot: MaterializedCommunityState::default(),
        },
        admin_addr: inviter,
        community_name: "Test Community".into(),
        is_invite_only: true,
        expires_at: None,
        invite_token: Some(fixture_invite_token(inviter)),
        admin_bootstrap: None,
        inviter_identity_pub: None,
        forked_from: None,
        pre_fork_snapshot: None,
        inviter_enrollment: None,
        untargeted_decrypt_key: None,
    }
}

fn fixture_routing_blob(iroh_node_id: [u8; 32]) -> Vec<u8> {
    let payload = ReachabilityAnnouncePayload {
        iroh_node_id,
        home_relay_url: "https://relay.test/".into(),
        direct_addresses: vec![],
        announced_at_ms: 1_700_000_000_000,
        identity_signature: [0xCD; 64],
        butler_set: Vec::new(),
        bs_at: 0,
    };
    let mut buf = Vec::new();
    ciborium::into_writer(&payload, &mut buf).expect("encode routing_blob");
    buf
}

#[tokio::test]
async fn case_a_publish_then_resolve_round_trip() {
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        // --- Setup ---
        let relay = MockPkarrRelay::start().await;
        let pool = RelayPool::new(vec![relay.base_url.clone()]);
        let client = Arc::new(RelayClient::new(pool));

        let publisher = Arc::new(PkarrPublisher::new(Arc::clone(&client)));
        let _ph = Arc::clone(&publisher).spawn();

        // Alice's identity key (deterministic for test reproducibility).
        let alice_sk = SigningKey::from_bytes(&[0x42u8; 32]);
        let alice_identity_pub = build_identity_pub(&alice_sk);
        let alice_iroh_node_id = [0xABu8; 32];

        let routing_blob_builder = {
            let iroh_id = alice_iroh_node_id;
            Arc::new(move || fixture_routing_blob(iroh_id))
        };

        let inv_pub = PkarrInvitePublisher::new(
            Arc::clone(&publisher),
            alice_sk.clone(),
            alice_identity_pub,
            routing_blob_builder,
        );

        // Alice's invite (fixed invite_token.sig = [0xAA; 64]).
        let alice_owner_addr = OwnerAddr([0x11u8; 16]);
        let invite = fixture_invite_payload(alice_owner_addr);

        // --- Case A: Alice registers the invite publication ---
        inv_pub
            .register_invite(invite.invite_token.as_ref().map(|t| t.sig))
            .await;

        // --- Bob's side: derive the same key ---
        let token_sig: [u8; 64] = [0xAAu8; 64];
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time")
            .as_millis() as u64;
        let epoch_id = current_epoch_id(now_ms);
        let bob_signing =
            derive_ephemeral_key(PkarrCase::Invite, &token_sig, &epoch_id.to_be_bytes());
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

        // --- Verify inner signature binds to alice's identity_pub ---
        record
            .verify_inner_sig()
            .expect("inner sig must be valid (RPK2)");

        // The record's harmony_identity_pub should be alice's.
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
            decoded_payload.home_relay_url, "https://relay.test/",
            "relay URL must match"
        );
    })
    .await;

    result.expect("case A integration test timed out");
}

// ZEB-325 Phase 2c option A pivot: the two end-to-end orchestration
// tests that previously lived in this file
// (`connectivity_redeem_invite_iroh_completes_join_via_crdt_sync` and
// `connectivity_redeem_invite_iroh_emits_progress_events`) were
// `#[ignore]`'d after the option C → option A pivot because they
// relied on a single-engine masking quirk that the new wire
// handshake makes structurally impossible. ZEB-325 PR #159 round-1
// review (CodeRabbit NITPICK F9) noted the ignored tests still
// asserted the post-handshake `"joined"` outcome with
// `iroh_endpoint: None`, which can never succeed under option A;
// they have been deleted. End-to-end coverage now lives entirely in
// `tests/pkarr_iroh_redeem_full_integration.rs`, which exercises the
// real two-process iroh bi-stream handshake.
