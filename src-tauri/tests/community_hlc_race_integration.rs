//! ZEB-267: concurrent-IPC HLC race regression test.
//!
//! Stands up a single community on one engine with one admin device
//! (power 100). Spawns two parallel `tokio::join!` futures, each
//! reserving an HLC via `reserve_next_hlc_for_device` then minting +
//! inserting a Kick event for a DIFFERENT target. Asserts:
//!
//!   1. Both engine inserts succeed (`InsertOutcome::Inserted`).
//!   2. The two events' HLCs are distinct under `event_sort_key`
//!      ordering — i.e., the per-device monotone-HLC invariant holds
//!      under concurrent reservation.
//!
//! The pre-ZEB-267 snapshot-then-release pattern would (probabilistically)
//! produce two events with identical HLC tuples, violating the
//! invariant the receive side depends on. With the atomic
//! `reserve_next_hlc_for_device` helper, the race is closed.
//!
//! This test exercises the HELPER + MINT + INSERT path, not the full
//! Tauri IPC boundary. The IPC boundary itself is just a thin wrapper:
//! - hex decode + handle snapshot
//! - the reserve+mint+insert flow this test drives
//! - generation/registry fences that don't interact with HLC ordering
//!
//! Driving the IPC layer directly would require a `tauri::test::mock_app`
//! runtime; the helper-level test is a tighter, faster regression
//! gate that covers the same race surface.

use harmony_app::community_membership::SignedMembershipEvent;
use harmony_app::community_state_crdt::{CommunityState, InsertOutcome};
use harmony_app::community_state_sync::{
    CommunityMembershipDelta, CommunityRootHlcTracker, CommunitySyncEngine,
    CommunitySyncEngineConfig, IdentityResolver, PersistPaths, DEFAULT_DEBOUNCE_MS,
};
use harmony_app::content_store::{CasOp, ContentStore, RuntimeContentStore};
use harmony_app::dm_outbox::reserve_next_hlc_for_device;
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};
use harmony_app::{mint_community_creation, mint_kick_event, mint_redemption};
use harmony_identity::PrivateIdentity;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};

// Implements the in-test IdentityResolver: maps owner addresses to
// identity public keys for the three participants (admin + two targets).
struct ThreeWayResolver {
    entries: Vec<(OwnerAddr, [u8; 64])>,
}

#[async_trait::async_trait]
impl IdentityResolver for ThreeWayResolver {
    async fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
        self.entries
            .iter()
            .find(|(a, _)| a == addr)
            .map(|(_, pub_bytes)| *pub_bytes)
    }
}

fn signing_key_from(identity: &PrivateIdentity) -> Arc<ed25519_dalek::SigningKey> {
    let bytes = identity.to_private_bytes();
    let mut seed = [0u8; 32];
    seed.copy_from_slice(&bytes[32..64]);
    Arc::new(ed25519_dalek::SigningKey::from_bytes(&seed))
}

#[tokio::test]
async fn concurrent_kicks_from_same_device_yield_distinct_hlcs() {
    // ── Setup: admin Alice (power 100), two kick targets Bob/Carol ──
    let alice = PrivateIdentity::from_seed(&[0xa1; 32]);
    let bob = PrivateIdentity::from_seed(&[0xb0; 32]);
    let carol = PrivateIdentity::from_seed(&[0xc0; 32]);

    let alice_addr = OwnerAddr(alice.identity.address_hash);
    let bob_addr = OwnerAddr(bob.identity.address_hash);
    let carol_addr = OwnerAddr(carol.identity.address_hash);

    let alice_pub = alice.identity.to_public_bytes();
    let bob_pub = bob.identity.to_public_bytes();
    let carol_pub = carol.identity.to_public_bytes();

    let alice_signing = signing_key_from(&alice);
    let bob_signing = signing_key_from(&bob);
    let carol_signing = signing_key_from(&carol);

    let resolver: Arc<dyn IdentityResolver> = Arc::new(ThreeWayResolver {
        entries: vec![
            (alice_addr, alice_pub),
            (bob_addr, bob_pub),
            (carol_addr, carol_pub),
        ],
    });

    // CAS servicer (in-memory).
    let cas_map: Arc<Mutex<HashMap<harmony_content::cid::ContentId, Vec<u8>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let (cas_op_tx, mut cas_op_rx) = mpsc::channel::<CasOp>(64);
    let cas_for_servicer = Arc::clone(&cas_map);
    tokio::spawn(async move {
        while let Some(op) = cas_op_rx.recv().await {
            match op {
                CasOp::PutLocal { cid, blob, reply } => {
                    cas_for_servicer.lock().await.insert(cid, blob);
                    if let Some(r) = reply {
                        let _ = r.send(Ok(()));
                    }
                }
                CasOp::GetOrFetch {
                    cid,
                    timeout: _,
                    reply,
                } => {
                    let v = cas_for_servicer.lock().await.get(&cid).cloned();
                    let _ = reply.send(Ok(v));
                }
            }
        }
    });

    // Engine pub/sub: not networked — we never expect any publishes
    // to land on `pub_rx` because the test doesn't drive convergence.
    let (pub_tx, _pub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (_sub_tx, sub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (delta_tx, _delta_rx) = mpsc::channel::<CommunityMembershipDelta>(32);

    // The IPC-level HLC tracker (per-device monotone HLCs). This is
    // the SAME shape the real IPC uses — `Arc<Mutex<BTreeMap<String, Hlc>>>`.
    let device_id = "alice-dev".to_string();
    let hlc_tracker: Arc<Mutex<BTreeMap<String, Hlc>>> = Arc::new(Mutex::new(BTreeMap::new()));

    // Mint Alice's community + bootstrap Join. Reserve via the helper
    // so the tracker has a valid starting state.
    let bootstrap_hlc = reserve_next_hlc_for_device(&hlc_tracker, &device_id, 100_000).await;
    let minted = mint_community_creation(
        "TestCommunity",
        false, // open
        alice_addr,
        &alice_signing,
        bootstrap_hlc,
    )
    .expect("mint_community_creation");
    let community_id: SpaceId = minted.community_id;

    // Stand up the engine.
    let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx.clone(),
        Duration::from_secs(2),
    ));
    let state = Arc::new(Mutex::new(CommunityState::new(community_id)));
    let tracker = Arc::new(Mutex::new(CommunityRootHlcTracker::default()));
    let tmp = tempfile::tempdir().expect("tmp");
    let engine = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        community_id,
        membership_key: minted.membership_key.clone(),
        admin_addr: alice_addr,
        is_invite_only: false,
        device_id: device_id.clone(),
        self_owner: alice_addr,
        signing_key: Arc::clone(&alice_signing),
        state: Arc::clone(&state),
        tracker: Arc::clone(&tracker),
        content_store: cs,
        publisher_tx: pub_tx,
        subscriber_rx: sub_rx,
        paths: PersistPaths {
            crdt: tmp.path().join("crdt.cbor"),
            replay: tmp.path().join("replay.cbor"),
        },
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        identity_resolver: Some(Arc::clone(&resolver)),
        error_tx: None,
        delta_tx: Some(delta_tx),
        pending_redemptions: None,
    });

    // Insert the bootstrap Join (Alice's self-Join, which gives her
    // power 100 as the admin).
    let outcome = engine
        .insert_local_event(minted.bootstrap_join.clone())
        .await
        .expect("insert bootstrap_join");
    assert_eq!(
        outcome,
        InsertOutcome::Inserted,
        "bootstrap Join must insert"
    );

    // Pre-seed Bob and Carol as members via mint_redemption so they
    // appear in prior_state.members (required for kick validation).
    let invite_payload = harmony_app::community_invite::CommunityInvitePayload {
        community_id,
        membership_key: minted.membership_key.clone(),
        admin_addr: alice_addr,
        community_name: "TestCommunity".into(),
        is_invite_only: false,
        expires_at: None,
        invite_token: None,
        admin_bootstrap: None,
        admin_identity_pub: None,
    };
    for (target_addr, target_signing) in [(bob_addr, &bob_signing), (carol_addr, &carol_signing)] {
        let join_hlc = reserve_next_hlc_for_device(&hlc_tracker, &device_id, 100_000).await;
        // Use target's own HLC device so the join is self-authored.
        let target_join_hlc = Hlc {
            wall_ms: join_hlc.wall_ms,
            logical: join_hlc.logical,
            device_id: format!("{}-dev", hex::encode(&target_addr.0[..4])),
        };
        let minted_join = mint_redemption(
            &invite_payload,
            target_addr,
            target_signing,
            target_join_hlc,
        )
        .expect("mint_redemption");
        let outcome = engine
            .insert_local_event(minted_join.bootstrap_join.clone())
            .await
            .expect("insert member Join");
        assert_eq!(
            outcome,
            InsertOutcome::Inserted,
            "Join must insert for {:?}",
            target_addr
        );
    }

    // ── The actual race test: two concurrent kick reservations ──
    //
    // Both calls happen on the SAME device with the SAME wall_now_ms.
    // Without the atomic helper, the snapshot-then-release pattern
    // would let both observe the same prev_hlc and produce events
    // with identical (wall_ms, logical, device_id) tuples. With
    // reserve_next_hlc_for_device, the read-bump-write is atomic
    // and the two reservations are guaranteed strictly-monotone.
    let wall_now_ms = 200_000u64;
    let tracker_a = Arc::clone(&hlc_tracker);
    let tracker_b = Arc::clone(&hlc_tracker);
    let device_a = device_id.clone();
    let device_b = device_id.clone();
    let signing_a = Arc::clone(&alice_signing);
    let signing_b = Arc::clone(&alice_signing);

    // tokio::join!: both futures run concurrently on the same task,
    // contending for the tracker mutex. The helper serializes them
    // under one read-bump-write critical section each.
    let (kick_bob, kick_carol): (
        Result<SignedMembershipEvent, String>,
        Result<SignedMembershipEvent, String>,
    ) = tokio::join!(
        async {
            let hlc = reserve_next_hlc_for_device(&tracker_a, &device_a, wall_now_ms).await;
            mint_kick_event(
                community_id,
                alice_addr,
                bob_addr,
                Some("race-test bob".into()),
                &signing_a,
                hlc,
            )
        },
        async {
            let hlc = reserve_next_hlc_for_device(&tracker_b, &device_b, wall_now_ms).await;
            mint_kick_event(
                community_id,
                alice_addr,
                carol_addr,
                Some("race-test carol".into()),
                &signing_b,
                hlc,
            )
        },
    );

    let kick_bob = kick_bob.expect("mint kick(bob)");
    let kick_carol = kick_carol.expect("mint kick(carol)");

    // ── Assertion 1: HLCs distinct under sort-key ordering ─────────
    let bob_key = (
        kick_bob.at.wall_ms,
        kick_bob.at.logical,
        &kick_bob.at.device_id,
    );
    let carol_key = (
        kick_carol.at.wall_ms,
        kick_carol.at.logical,
        &kick_carol.at.device_id,
    );
    assert_ne!(
        bob_key, carol_key,
        "concurrent reservations must produce distinct sort keys; \
         got bob={:?} carol={:?}",
        kick_bob.at, kick_carol.at
    );

    // ── Assertion 2: both engine inserts succeed ────────────────────
    let outcome_bob = engine
        .insert_local_event(kick_bob.clone())
        .await
        .expect("insert kick(bob)");
    let outcome_carol = engine
        .insert_local_event(kick_carol.clone())
        .await
        .expect("insert kick(carol)");
    assert_eq!(
        outcome_bob,
        InsertOutcome::Inserted,
        "kick(bob) must insert"
    );
    assert_eq!(
        outcome_carol,
        InsertOutcome::Inserted,
        "kick(carol) must insert"
    );

    // ── Assertion 3: tracker holds the LATER of the two HLCs ───────
    let stored = hlc_tracker
        .lock()
        .await
        .get(&device_id)
        .cloned()
        .expect("tracker entry");
    let max_kick_at = if (
        kick_bob.at.wall_ms,
        kick_bob.at.logical,
        &kick_bob.at.device_id,
    ) > (
        kick_carol.at.wall_ms,
        kick_carol.at.logical,
        &kick_carol.at.device_id,
    ) {
        kick_bob.at
    } else {
        kick_carol.at
    };
    assert_eq!(
        stored, max_kick_at,
        "tracker must hold the max-by-sort-key HLC of the two reservations"
    );

    engine.shutdown().await.expect("shutdown");
}
