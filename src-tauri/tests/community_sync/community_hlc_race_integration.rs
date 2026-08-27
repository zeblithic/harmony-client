//! ZEB-267: concurrent-IPC HLC race regression test.
//!
//! Stands up a single community on one engine with one admin device
//! (power 100). Spawns two `tokio::spawn` tasks on a multi-thread
//! runtime, gated through a `tokio::sync::Barrier`, each reserving an
//! HLC via `reserve_next_hlc_for_device` then minting + inserting a
//! Kick event for a DIFFERENT target. The barrier forces both tasks
//! to be at the tracker `lock().await` boundary simultaneously, so
//! the tracker mutex is the actual contention point — a non-atomic
//! snapshot-then-release implementation would deterministically
//! collide under this shape rather than hide behind sequential
//! polling on a single task. Asserts:
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

use harmony_app::community_state_crdt::{CommunityState, InsertOutcome};
use harmony_app::community_state_sync::{
    CommunityMembershipDelta, CommunityReplayTracker, CommunitySyncEngine,
    CommunitySyncEngineConfig, IdentityResolver, PersistPaths, DEFAULT_DEBOUNCE_MS,
};
use harmony_app::content_store::{CasOp, ContentStore, RuntimeContentStore};
use harmony_app::dm_outbox::reserve_next_hlc_for_device;
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};
use harmony_app::{mint_community_creation, mint_kick_event, mint_redemption};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Barrier, Mutex};

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_kicks_from_same_device_yield_distinct_hlcs() {
    // ── Setup: admin Alice (power 100), two kick targets Bob/Carol ──
    let alice = harmony_app::community_membership::mint_test_owner(0xA1);
    let bob = harmony_app::community_membership::mint_test_owner(0xB0);
    let carol = harmony_app::community_membership::mint_test_owner(0xC0);

    let alice_addr = alice.owner;
    let bob_addr = bob.owner;
    let carol_addr = carol.owner;

    let alice_signing = Arc::new(alice.device_key.clone());
    let bob_signing = Arc::new(bob.device_key.clone());
    let carol_signing = Arc::new(carol.device_key.clone());

    // ZEB-339: signer resolution uses the EnrollmentCert / materialized enrolled
    // keys, not the resolver — so the resolver's identity_pubs are unused here.
    let resolver: Arc<dyn IdentityResolver> = Arc::new(ThreeWayResolver {
        entries: vec![
            (alice_addr, [0u8; 64]),
            (bob_addr, [0u8; 64]),
            (carol_addr, [0u8; 64]),
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
                CasOp::PutLocal {
                    cid, blob, reply, ..
                } => {
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
                CasOp::GetLocal { cid, reply } => {
                    let v = cas_for_servicer.lock().await.get(&cid).cloned();
                    let _ = reply.send(v);
                }
                CasOp::AllowServeSubtree { reply, .. } => {
                    let _ = reply.send(Ok(0));
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
    // the SAME shape the real IPC uses — `Arc<Mutex<harmony_crdt_sync::ReplayTracker<String, Hlc>>>`.
    let device_id = "alice-dev".to_string();
    let hlc_tracker: Arc<Mutex<harmony_crdt_sync::ReplayTracker<String, Hlc>>> = Arc::new(
        Mutex::new(harmony_crdt_sync::ReplayTracker::new(device_id.clone())),
    );

    // Mint Alice's community + bootstrap Join. Reserve via the helper
    // so the tracker has a valid starting state.
    let bootstrap_hlc = reserve_next_hlc_for_device(
        &hlc_tracker,
        &harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
        &device_id,
        100_000,
    )
    .await;
    let minted = mint_community_creation(
        "TestCommunity",
        false, // open
        alice_addr,
        &alice_signing,
        &alice.cert,
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
    let tracker = Arc::new(Mutex::new(CommunityReplayTracker::new((
        alice_addr,
        device_id.clone(),
    ))));
    let tmp = tempfile::tempdir().expect("tmp");
    let engine = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        device_cipher: harmony_app::device_dataset_file::test_cipher(),
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
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
        crdt_state: None,
        inviter_identity_pub: None,
        nav_emitter: None,
        membership_updated_emitter: None,
        root_serve_rx: None,
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
        inviter_signer_certs: Vec::new(),
        community_id,
        epoch_snapshot: harmony_app::community_invite::InviteEpochSnapshot {
            epoch: 0,
            sealed_epoch_key: minted.membership_key.as_bytes().to_vec(),
            sealed_epoch_keys: Vec::new(),
            state_snapshot: harmony_app::community_invite::MaterializedCommunityState::default(),
        },
        admin_addr: alice_addr,
        community_name: "TestCommunity".into(),
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
    for (target_addr, target_signing, target_cert) in [
        (bob_addr, &bob_signing, &bob.cert),
        (carol_addr, &carol_signing, &carol.cert),
    ] {
        // Reserve against the TARGET's own device id, not Alice's
        // device id — the resulting Join is self-authored by the
        // target, so the tracker entry that should advance is
        // target_dev_id. Reserving against alice-dev would bump
        // Alice's tracker for events she didn't author, leaving
        // it misaligned with her actual event history (Greptile
        // PR #94 review).
        // Full owner bytes (not just the 4-byte prefix) so the
        // per-target tracker key is collision-free (CodeRabbit review).
        let target_dev_id = format!("{}-dev", hex::encode(target_addr.0));
        // The target is a DIFFERENT device, so it mints from its OWN tracker —
        // which is also what keeps Alice's lane untouched (the point of the
        // comment above). Reserving a foreign lane inside Alice's tracker was
        // the old way to express that; a real device never shares one.
        let target_tracker = Arc::new(Mutex::new(harmony_crdt_sync::ReplayTracker::new(
            target_dev_id.clone(),
        )));
        let target_join_hlc = reserve_next_hlc_for_device(
            &target_tracker,
            &harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
            &target_dev_id,
            100_000,
        )
        .await;
        let minted_join = mint_redemption(
            &invite_payload,
            target_addr,
            target_signing,
            target_cert,
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
    //
    // Spawning each path on its OWN task (via `tokio::spawn` on the
    // multi-thread runtime configured at the test attribute above)
    // and gating both on a `Barrier::new(2)` makes the tracker mutex
    // the actual contention point. The barrier wait completes only
    // when both tasks reach the rendezvous — they then race into
    // `tracker.lock().await` simultaneously. A non-atomic helper
    // (snapshot, drop guard, return; bump elsewhere) would
    // deterministically collide under this shape; the atomic helper
    // serializes the two reservations into strictly-monotone HLCs.
    let wall_now_ms = 200_000u64;
    let barrier = Arc::new(Barrier::new(2));

    let task_bob = {
        let tracker = Arc::clone(&hlc_tracker);
        let device = device_id.clone();
        let signing = Arc::clone(&alice_signing);
        let barrier = Arc::clone(&barrier);
        tokio::spawn(async move {
            barrier.wait().await;
            let hlc = reserve_next_hlc_for_device(
                &tracker,
                &harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
                &device,
                wall_now_ms,
            )
            .await;
            mint_kick_event(
                community_id,
                alice_addr,
                bob_addr,
                Some("race-test bob".into()),
                &signing,
                hlc,
            )
        })
    };

    let task_carol = {
        let tracker = Arc::clone(&hlc_tracker);
        let device = device_id.clone();
        let signing = Arc::clone(&alice_signing);
        let barrier = Arc::clone(&barrier);
        tokio::spawn(async move {
            barrier.wait().await;
            let hlc = reserve_next_hlc_for_device(
                &tracker,
                &harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
                &device,
                wall_now_ms,
            )
            .await;
            mint_kick_event(
                community_id,
                alice_addr,
                carol_addr,
                Some("race-test carol".into()),
                &signing,
                hlc,
            )
        })
    };

    let kick_bob = task_bob
        .await
        .expect("kick(bob) task panicked")
        .expect("mint kick(bob)");
    let kick_carol = task_carol
        .await
        .expect("kick(carol) task panicked")
        .expect("mint kick(carol)");

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
        .accepted()
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
