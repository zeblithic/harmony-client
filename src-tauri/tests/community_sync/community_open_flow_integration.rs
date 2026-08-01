//! Two-engine open community round-trip — Phase 3 ZEB-217 Sub-C.
//!
//! Exercises the local-mint path (`mint_community_creation`,
//! `mint_redemption`, `mint_leave_event`) through two
//! `CommunitySyncEngine` instances bridged in-memory. Verifies:
//!
//! 1. Creator's bootstrap Join → published → received on B → materialized
//! 2. B's redemption-Join → published → received on A → materialized
//! 3. Both peers' member lists agree (sorted by power desc, joined_at asc)
//! 4. B's Leave → published → received on A → materialized as Left
//!
//! Cannot exercise the `#[tauri::command]` wrappers directly (no
//! AppHandle in tests until ZEB-247). The IPC wrappers are thin
//! plumbing over the inner pure helpers tested here.

use harmony_app::community_membership::MaterializedMembership;
use harmony_app::community_state_crdt::{CommunityState, InsertOutcome};
use harmony_app::community_state_sync::{
    CommunityMembershipDelta, CommunityReplayTracker, CommunitySyncEngine,
    CommunitySyncEngineConfig, IdentityResolver, PersistPaths, DEFAULT_DEBOUNCE_MS,
};
use harmony_app::content_store::{CasOp, ContentStore, RuntimeContentStore};
use harmony_app::dm_outbox::reserve_next_hlc_for_device;
use harmony_app::owner_state_types::{Hlc, OwnerAddr};
use harmony_app::{
    delta_to_change, member_info_for, mint_community_creation, mint_leave_event, mint_redemption,
    MemberStatusDto, MembershipChangeType,
};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};

struct TwoIdentityResolver {
    a: (OwnerAddr, [u8; 64]),
    b: (OwnerAddr, [u8; 64]),
}

#[async_trait::async_trait]
impl IdentityResolver for TwoIdentityResolver {
    async fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
        if *addr == self.a.0 {
            Some(self.a.1)
        } else if *addr == self.b.0 {
            Some(self.b.1)
        } else {
            None
        }
    }
}

async fn wait_until<F, Fut>(mut cond: F, timeout: Duration) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if cond().await {
            return true;
        }
        if tokio::time::Instant::now() > deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn open_community_create_redeem_leave_round_trip() {
    let owner_a_test = harmony_app::community_membership::mint_test_owner(0xA1);
    let owner_b_test = harmony_app::community_membership::mint_test_owner(0xB2);
    let owner_a = owner_a_test.owner;
    let owner_b = owner_b_test.owner;
    let signing_a = owner_a_test.device_key.clone();
    let signing_b = owner_b_test.device_key.clone();

    // ZEB-339: signer resolution uses the EnrollmentCert / materialized enrolled
    // keys, not the resolver — resolver identity_pubs unused for membership.
    let resolver: Arc<dyn IdentityResolver> = Arc::new(TwoIdentityResolver {
        a: (owner_a, [0u8; 64]),
        b: (owner_b, [0u8; 64]),
    });

    // Shared in-memory CAS servicer — A and B route their
    // RuntimeContentStore ops through the same channel so blobs A puts
    // are visible to B's gets (and vice versa). Mirrors
    // `community_sync_integration::spawn_shared_cas`.
    let cas: Arc<Mutex<HashMap<harmony_content::cid::ContentId, Vec<u8>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let (cas_op_tx, mut cas_op_rx) = mpsc::channel(64);
    let cas_for_servicer = Arc::clone(&cas);
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

    // Wire: A's publisher → B's subscriber and B's publisher →
    // A's subscriber. Forwarders mimic the Zenoh fanout adapter.
    let (a_out_tx, mut a_out_rx) = mpsc::channel::<Vec<u8>>(64);
    let (a_in_tx, a_in_rx) = mpsc::channel::<Vec<u8>>(64);
    let (b_out_tx, mut b_out_rx) = mpsc::channel::<Vec<u8>>(64);
    let (b_in_tx, b_in_rx) = mpsc::channel::<Vec<u8>>(64);
    let a_in_for_fwd = a_in_tx.clone();
    tokio::spawn(async move {
        while let Some(bytes) = b_out_rx.recv().await {
            let _ = a_in_for_fwd.send(bytes).await;
        }
    });
    let b_in_for_fwd = b_in_tx.clone();
    tokio::spawn(async move {
        while let Some(bytes) = a_out_rx.recv().await {
            let _ = b_in_for_fwd.send(bytes).await;
        }
    });

    // A mints a fresh community + bootstrap Join.
    let minted_a = mint_community_creation(
        "TestCommunity",
        false,
        owner_a,
        &signing_a,
        &owner_a_test.cert,
        Hlc {
            wall_ms: 100_000,
            logical: 0,
            device_id: "a-dev".to_string(),
        },
    )
    .expect("mint create");
    let community_id = minted_a.community_id;

    let cs_a: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx.clone(),
        Duration::from_secs(2),
    ));
    let cs_b: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx.clone(),
        Duration::from_secs(2),
    ));

    let state_a = Arc::new(Mutex::new(CommunityState::new(community_id)));
    let state_b = Arc::new(Mutex::new(CommunityState::new(community_id)));
    let tracker_a = Arc::new(Mutex::new(CommunityReplayTracker::new((
        owner_a,
        "a-dev".to_string(),
    ))));
    let tracker_b = Arc::new(Mutex::new(CommunityReplayTracker::new((
        owner_b,
        "b-dev".to_string(),
    ))));

    let (delta_a_tx, mut delta_a_rx) = mpsc::channel::<CommunityMembershipDelta>(32);
    let (delta_b_tx, mut delta_b_rx) = mpsc::channel::<CommunityMembershipDelta>(32);

    let tmp_a = tempfile::tempdir().expect("tmp a");
    let tmp_b = tempfile::tempdir().expect("tmp b");

    let engine_a = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        community_id,
        membership_key: minted_a.membership_key.clone(),
        admin_addr: owner_a,
        is_invite_only: false,
        device_id: "a-dev".into(),
        self_owner: owner_a,
        signing_key: Arc::new(signing_a.clone()),
        state: Arc::clone(&state_a),
        tracker: Arc::clone(&tracker_a),
        content_store: cs_a,
        publisher_tx: a_out_tx,
        subscriber_rx: a_in_rx,
        paths: PersistPaths {
            crdt: tmp_a.path().join("crdt.cbor"),
            replay: tmp_a.path().join("replay.cbor"),
        },
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        identity_resolver: Some(Arc::clone(&resolver)),
        error_tx: None,
        delta_tx: Some(delta_a_tx),
        pending_redemptions: None,
        crdt_state: None,
        admin_identity_pub: None,
        nav_emitter: None,
        root_serve_rx: None,
    });
    let engine_b = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        community_id,
        membership_key: minted_a.membership_key.clone(),
        admin_addr: owner_a,
        is_invite_only: false,
        device_id: "b-dev".into(),
        self_owner: owner_b,
        signing_key: Arc::new(signing_b.clone()),
        state: Arc::clone(&state_b),
        tracker: Arc::clone(&tracker_b),
        content_store: cs_b,
        publisher_tx: b_out_tx,
        subscriber_rx: b_in_rx,
        paths: PersistPaths {
            crdt: tmp_b.path().join("crdt.cbor"),
            replay: tmp_b.path().join("replay.cbor"),
        },
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        identity_resolver: Some(Arc::clone(&resolver)),
        error_tx: None,
        delta_tx: Some(delta_b_tx),
        pending_redemptions: None,
        crdt_state: None,
        admin_identity_pub: None,
        nav_emitter: None,
        root_serve_rx: None,
    });

    // ── Step 1: A inserts its bootstrap Join. ─────────────────────────
    let outcome = engine_a
        .insert_local_event(minted_a.bootstrap_join.clone())
        .await
        .expect("A bootstrap insert");
    assert_eq!(outcome, InsertOutcome::Inserted);
    let delta_a_first = tokio::time::timeout(Duration::from_secs(1), delta_a_rx.recv())
        .await
        .expect("A own delta")
        .expect("channel open");
    let (cid_hex_a, change_a) = delta_to_change(&delta_a_first).expect("project");
    assert_eq!(cid_hex_a, hex::encode(community_id.0));
    assert_eq!(change_a.r#type, MembershipChangeType::Joined);
    assert_eq!(change_a.target, hex::encode(owner_a.0));

    // ZEB-256 Task 6: B inserts A's bootstrap Join locally before B publishes.
    // ZEB-558 note: for OPEN communities this pre-seed is no longer required for
    // correctness — the deferred open-bootstrap admission path now lets B admit
    // A's first publish from the self-Join carried in the blob (proven by
    // `open_community_two_node_wire_convergence_no_preseed`, which omits these
    // seeds). It is retained HERE so the delta-tx emission this test asserts on
    // below fires symmetrically (and the seed remains correctness-required for
    // invite-only communities). Production reconstructs admin's Join via
    // `mint_redemption` from the invite payload's cryptographic proof. Going
    // through `insert_local_event` (rather than direct CRDT mutation) preserves
    // the delta-tx emission.
    let _delta_b_a_join = engine_b
        .insert_local_event(minted_a.bootstrap_join.clone())
        .await
        .expect("B insert A's bootstrap Join (pre-seed)");

    // B should converge on A's bootstrap Join (already true via the
    // pre-seed; the wait_until is a no-op assert that the state is
    // stable).
    assert!(
        wait_until(
            || async { state_b.lock().await.event_count() == 1 },
            Duration::from_secs(10),
        )
        .await,
        "B should hold A's bootstrap Join"
    );
    let _delta_b_remote = tokio::time::timeout(Duration::from_secs(2), delta_b_rx.recv())
        .await
        .expect("B insert-local delta")
        .expect("channel open");

    // ── Step 2: B redeems an invite for the same community. ────────────
    let invite_payload = harmony_app::community_invite::CommunityInvitePayload {
        inviter_signer_certs: Vec::new(),
        community_id,
        epoch_snapshot: harmony_app::community_invite::InviteEpochSnapshot {
            epoch: 0,
            sealed_epoch_key: minted_a.membership_key.as_bytes().to_vec(),
            sealed_epoch_keys: Vec::new(),
            state_snapshot: harmony_app::community_invite::MaterializedCommunityState::default(),
        },
        admin_addr: owner_a,
        community_name: "TestCommunity".into(),
        is_invite_only: false,
        expires_at: None,
        invite_token: None,
        admin_bootstrap: None,
        admin_identity_pub: None,
        forked_from: None,
        pre_fork_snapshot: None,
        inviter_enrollment: None,
        untargeted_decrypt_key: None,
    };

    let minted_b = mint_redemption(
        &invite_payload,
        owner_b,
        &signing_b,
        &owner_b_test.cert,
        Hlc {
            wall_ms: 200_000,
            logical: 0,
            device_id: "b-dev".to_string(),
        },
    )
    .expect("mint redeem");
    let redemption_outcome = engine_b
        .insert_local_event(minted_b.bootstrap_join.clone())
        .await
        .expect("B redemption insert");
    assert_eq!(redemption_outcome, InsertOutcome::Inserted);
    let _delta_b_own = tokio::time::timeout(Duration::from_secs(1), delta_b_rx.recv())
        .await
        .expect("B own delta")
        .expect("channel open");

    // ZEB-256 Task 6: A insert-locals B's redemption Join too. The
    // wire publish from B would normally bridge it, but A's
    // membership-at-HLC gate on B's publish would reject
    // (`publisher_not_joined` — B isn't in A's CRDT until A merges
    // B's Join). Production has a richer cold-cache propagation
    // story; for this test we simulate the OOB merge that ZEB-256
    // §5 "Cold-cache transient rejection" relies on.
    let _delta_a_b_join = engine_a
        .insert_local_event(minted_b.bootstrap_join.clone())
        .await
        .expect("A insert B's redemption Join (post-Task-6 bootstrap simulation)");

    // A should now hold both events (admin Join + B's redemption Join).
    assert!(
        wait_until(
            || async { state_a.lock().await.event_count() == 2 },
            Duration::from_secs(10),
        )
        .await,
        "A should hold its own Join + B's redemption Join"
    );
    let _delta_a_remote = tokio::time::timeout(Duration::from_secs(2), delta_a_rx.recv())
        .await
        .expect("A delta from B-Join insert")
        .expect("channel open");

    // ── Step 3: both peers agree on the materialized member list. ─────
    let materialized_a: MaterializedMembership = {
        let s = state_a.lock().await;
        s.materialize_now(owner_a)
    };
    let materialized_b: MaterializedMembership = {
        let s = state_b.lock().await;
        s.materialize_now(owner_a)
    };
    let dto_a = member_info_for(&materialized_a);
    let dto_b = member_info_for(&materialized_b);
    assert_eq!(dto_a.len(), 2);
    assert_eq!(dto_b.len(), 2);
    assert_eq!(dto_a[0].addr, hex::encode(owner_a.0));
    assert_eq!(dto_a[0].power, 100);
    assert_eq!(dto_a[1].addr, hex::encode(owner_b.0));
    assert_eq!(dto_a[1].power, 0);
    assert_eq!(dto_a, dto_b);

    // ── Step 4: B leaves; A should observe B's status flip to Left. ───
    // ZEB-267: reserve HLC via the production helper against a local
    // tracker pre-seeded with B's redemption join (Greptile review:
    // avoid hand-rolling next_hlc internals at the test boundary).
    let leave_tracker = Arc::new(Mutex::new({
        let mut m = BTreeMap::<String, Hlc>::new();
        m.insert("b-dev".to_string(), minted_b.bootstrap_join.at.clone());
        m
    }));
    let leave_hlc = reserve_next_hlc_for_device(
        &leave_tracker,
        &harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
        "b-dev",
        300_000,
    )
    .await;
    let leave_b =
        mint_leave_event(community_id, owner_b, &signing_b, leave_hlc).expect("mint leave");
    let leave_outcome = engine_b
        .insert_local_event(leave_b)
        .await
        .expect("B leave insert");
    assert_eq!(leave_outcome, InsertOutcome::Inserted);

    assert!(
        wait_until(
            || async {
                let s = state_a.lock().await;
                s.event_count() == 3
            },
            Duration::from_secs(10),
        )
        .await,
        "A should receive B's Leave"
    );

    let materialized_a_after: MaterializedMembership = {
        let s = state_a.lock().await;
        s.materialize_now(owner_a)
    };
    let dto_after = member_info_for(&materialized_a_after);
    let b_row = dto_after
        .iter()
        .find(|d| d.addr == hex::encode(owner_b.0))
        .expect("B still in member list (Left, not removed)");
    assert_eq!(b_row.status, MemberStatusDto::Left);

    engine_a.shutdown().await.expect("shutdown a");
    engine_b.shutdown().await.expect("shutdown b");
}

/// Regression test for PR #87 round 3 / Cursor Bugbot: verifies that
/// redeeming the same invite twice (a) does not error, (b) does not
/// corrupt the materialized member list (CRDT LWW absorbs the duplicate
/// Join), and (c) leaves the event log with one extra event (the
/// duplicate self-Join). The redeem_invite IPC's documented behavior is
/// "non-idempotent at the event-log level, idempotent at the
/// materialized-state level"; this test pins both halves of that
/// contract.
///
/// Cannot directly exercise the zombie-adapter codepath that motivated
/// the fix (no IPC harness in tests until ZEB-247), but does cover the
/// critical state-correctness invariant the fix preserves.
#[tokio::test]
async fn redeem_invite_twice_does_not_corrupt_state() {
    let owner_a_test = harmony_app::community_membership::mint_test_owner(0xA3);
    let owner_b_test = harmony_app::community_membership::mint_test_owner(0xB4);
    let owner_a = owner_a_test.owner;
    let owner_b = owner_b_test.owner;
    let signing_a = owner_a_test.device_key.clone();
    let signing_b = owner_b_test.device_key.clone();

    // ZEB-339: signer resolution uses the EnrollmentCert / materialized enrolled
    // keys, not the resolver — resolver identity_pubs unused for membership.
    let resolver: Arc<dyn IdentityResolver> = Arc::new(TwoIdentityResolver {
        a: (owner_a, [0u8; 64]),
        b: (owner_b, [0u8; 64]),
    });

    // Same shared in-memory CAS shape as the round-trip test.
    let cas: Arc<Mutex<HashMap<harmony_content::cid::ContentId, Vec<u8>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let (cas_op_tx, mut cas_op_rx) = mpsc::channel(64);
    let cas_for_servicer = Arc::clone(&cas);
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

    // Bridge A↔B as before.
    let (a_out_tx, mut a_out_rx) = mpsc::channel::<Vec<u8>>(64);
    let (a_in_tx, a_in_rx) = mpsc::channel::<Vec<u8>>(64);
    let (b_out_tx, mut b_out_rx) = mpsc::channel::<Vec<u8>>(64);
    let (b_in_tx, b_in_rx) = mpsc::channel::<Vec<u8>>(64);
    let a_in_for_fwd = a_in_tx.clone();
    tokio::spawn(async move {
        while let Some(bytes) = b_out_rx.recv().await {
            let _ = a_in_for_fwd.send(bytes).await;
        }
    });
    let b_in_for_fwd = b_in_tx.clone();
    tokio::spawn(async move {
        while let Some(bytes) = a_out_rx.recv().await {
            let _ = b_in_for_fwd.send(bytes).await;
        }
    });

    let minted_a = mint_community_creation(
        "TestCommunity",
        false,
        owner_a,
        &signing_a,
        &owner_a_test.cert,
        Hlc {
            wall_ms: 100_000,
            logical: 0,
            device_id: "a-dev".to_string(),
        },
    )
    .expect("mint create");
    let community_id = minted_a.community_id;

    let cs_a: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx.clone(),
        Duration::from_secs(2),
    ));
    let cs_b: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx.clone(),
        Duration::from_secs(2),
    ));

    let state_a = Arc::new(Mutex::new(CommunityState::new(community_id)));
    let state_b = Arc::new(Mutex::new(CommunityState::new(community_id)));
    let tracker_a = Arc::new(Mutex::new(CommunityReplayTracker::new((
        owner_a,
        "a-dev".to_string(),
    ))));
    let tracker_b = Arc::new(Mutex::new(CommunityReplayTracker::new((
        owner_b,
        "b-dev".to_string(),
    ))));

    let (delta_a_tx, mut delta_a_rx) = mpsc::channel::<CommunityMembershipDelta>(32);
    let (delta_b_tx, mut delta_b_rx) = mpsc::channel::<CommunityMembershipDelta>(32);

    let tmp_a = tempfile::tempdir().expect("tmp a");
    let tmp_b = tempfile::tempdir().expect("tmp b");

    let engine_a = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        community_id,
        membership_key: minted_a.membership_key.clone(),
        admin_addr: owner_a,
        is_invite_only: false,
        device_id: "a-dev".into(),
        self_owner: owner_a,
        signing_key: Arc::new(signing_a.clone()),
        state: Arc::clone(&state_a),
        tracker: Arc::clone(&tracker_a),
        content_store: cs_a,
        publisher_tx: a_out_tx,
        subscriber_rx: a_in_rx,
        paths: PersistPaths {
            crdt: tmp_a.path().join("crdt.cbor"),
            replay: tmp_a.path().join("replay.cbor"),
        },
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        identity_resolver: Some(Arc::clone(&resolver)),
        error_tx: None,
        delta_tx: Some(delta_a_tx),
        pending_redemptions: None,
        crdt_state: None,
        admin_identity_pub: None,
        nav_emitter: None,
        root_serve_rx: None,
    });
    let engine_b = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        community_id,
        membership_key: minted_a.membership_key.clone(),
        admin_addr: owner_a,
        is_invite_only: false,
        device_id: "b-dev".into(),
        self_owner: owner_b,
        signing_key: Arc::new(signing_b.clone()),
        state: Arc::clone(&state_b),
        tracker: Arc::clone(&tracker_b),
        content_store: cs_b,
        publisher_tx: b_out_tx,
        subscriber_rx: b_in_rx,
        paths: PersistPaths {
            crdt: tmp_b.path().join("crdt.cbor"),
            replay: tmp_b.path().join("replay.cbor"),
        },
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        identity_resolver: Some(Arc::clone(&resolver)),
        error_tx: None,
        delta_tx: Some(delta_b_tx),
        pending_redemptions: None,
        crdt_state: None,
        admin_identity_pub: None,
        nav_emitter: None,
        root_serve_rx: None,
    });

    // ── A bootstrap → B converges ─────────────────────────────────────
    let outcome = engine_a
        .insert_local_event(minted_a.bootstrap_join.clone())
        .await
        .expect("A bootstrap insert");
    assert_eq!(outcome, InsertOutcome::Inserted);
    tokio::time::timeout(Duration::from_secs(1), delta_a_rx.recv())
        .await
        .expect("A own delta did not arrive within 1s")
        .expect("A delta channel closed before own delta arrived");

    // ZEB-256 Task 6: B insert-locals A's bootstrap Join (cf. spec §5
    // "Cold-cache transient rejection"). Without this OOB merge, A's
    // wire publish would be rejected by B's membership-at-HLC gate.
    engine_b
        .insert_local_event(minted_a.bootstrap_join.clone())
        .await
        .expect("B insert A's bootstrap Join (post-Task-6 bootstrap simulation)");

    assert!(
        wait_until(
            || async { state_b.lock().await.event_count() == 1 },
            Duration::from_secs(10),
        )
        .await,
        "B should hold A's bootstrap Join"
    );
    tokio::time::timeout(Duration::from_secs(2), delta_b_rx.recv())
        .await
        .expect("B insert-local delta did not arrive within 2s")
        .expect("B delta channel closed before bootstrap delta arrived");

    // ── First redemption: B mints + inserts ───────────────────────────
    let invite_payload = harmony_app::community_invite::CommunityInvitePayload {
        inviter_signer_certs: Vec::new(),
        community_id,
        epoch_snapshot: harmony_app::community_invite::InviteEpochSnapshot {
            epoch: 0,
            sealed_epoch_key: minted_a.membership_key.as_bytes().to_vec(),
            sealed_epoch_keys: Vec::new(),
            state_snapshot: harmony_app::community_invite::MaterializedCommunityState::default(),
        },
        admin_addr: owner_a,
        community_name: "TestCommunity".into(),
        is_invite_only: false,
        expires_at: None,
        invite_token: None,
        admin_bootstrap: None,
        admin_identity_pub: None,
        forked_from: None,
        pre_fork_snapshot: None,
        inviter_enrollment: None,
        untargeted_decrypt_key: None,
    };

    let minted_b1 = mint_redemption(
        &invite_payload,
        owner_b,
        &signing_b,
        &owner_b_test.cert,
        Hlc {
            wall_ms: 200_000,
            logical: 0,
            device_id: "b-dev".to_string(),
        },
    )
    .expect("mint redeem #1");
    let outcome1 = engine_b
        .insert_local_event(minted_b1.bootstrap_join.clone())
        .await
        .expect("B redemption #1 insert");
    assert_eq!(outcome1, InsertOutcome::Inserted);
    tokio::time::timeout(Duration::from_secs(1), delta_b_rx.recv())
        .await
        .expect("B own delta (first redemption) did not arrive within 1s")
        .expect("B delta channel closed before B's own redemption delta arrived");

    // ZEB-256 Task 6: A insert-locals B's redemption Join; without
    // this OOB merge, B's wire publish (publisher_addr=owner_b)
    // would fail A's membership-at-HLC gate.
    engine_a
        .insert_local_event(minted_b1.bootstrap_join.clone())
        .await
        .expect("A insert B's first redemption Join (post-Task-6)");

    assert!(
        wait_until(
            || async { state_a.lock().await.event_count() == 2 },
            Duration::from_secs(10),
        )
        .await,
        "A should hold its own + B's first redemption Join"
    );
    tokio::time::timeout(Duration::from_secs(2), delta_a_rx.recv())
        .await
        .expect("A insert-local delta (B's first redemption) did not arrive within 2s")
        .expect("A delta channel closed before B's redemption delta arrived");

    // ── Second redemption: B mints + inserts AGAIN with the same URL.
    //     Distinct event_id (random) and HLC tick advance produce a
    //     CRDT-distinct event, so InsertOutcome::Inserted again. ──────
    // ZEB-267: reserve HLC via the production helper against a local
    // tracker pre-seeded with B's first redemption (Greptile review).
    let redeem2_tracker = Arc::new(Mutex::new({
        let mut m = BTreeMap::<String, Hlc>::new();
        m.insert("b-dev".to_string(), minted_b1.bootstrap_join.at.clone());
        m
    }));
    let redeem2_hlc = reserve_next_hlc_for_device(
        &redeem2_tracker,
        &harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
        "b-dev",
        300_000,
    )
    .await;
    let minted_b2 = mint_redemption(
        &invite_payload,
        owner_b,
        &signing_b,
        &owner_b_test.cert,
        redeem2_hlc,
    )
    .expect("mint redeem #2");
    assert_ne!(
        minted_b2.bootstrap_join.id, minted_b1.bootstrap_join.id,
        "second redemption must mint a fresh event_id"
    );
    let outcome2 = engine_b
        .insert_local_event(minted_b2.bootstrap_join.clone())
        .await
        .expect(
            "B redemption #2 insert (regression: must not error on \
             double-redeem of same invite)",
        );
    assert_eq!(outcome2, InsertOutcome::Inserted);

    // Event log grew by one (documented non-idempotence at event-log
    // level — see redeem_invite docstring).
    {
        let s = state_b.lock().await;
        assert_eq!(
            s.event_count(),
            3,
            "event log should have A's bootstrap + B's two redemption Joins"
        );
    }

    // ZEB-256 Task 6: same OOB-merge pattern for B's second
    // redemption Join.
    engine_a
        .insert_local_event(minted_b2.bootstrap_join.clone())
        .await
        .expect("A insert B's second redemption Join (post-Task-6)");

    // Materialized member list is unchanged: still {A: Joined power=100,
    // B: Joined power=0}. CRDT LWW on MemberState absorbs the duplicate.
    let materialized_b: MaterializedMembership = {
        let s = state_b.lock().await;
        s.materialize_now(owner_a)
    };
    let dto_b = member_info_for(&materialized_b);
    assert_eq!(dto_b.len(), 2, "exactly two members after double-redeem");
    assert_eq!(dto_b[0].addr, hex::encode(owner_a.0));
    assert_eq!(dto_b[0].power, 100);
    assert_eq!(dto_b[0].status, MemberStatusDto::Joined);
    assert_eq!(dto_b[1].addr, hex::encode(owner_b.0));
    assert_eq!(dto_b[1].power, 0);
    assert_eq!(dto_b[1].status, MemberStatusDto::Joined);

    // A converges on the second Join too — B's CRDT mutation publishes
    // even though it's a materialization no-op.
    assert!(
        wait_until(
            || async { state_a.lock().await.event_count() == 3 },
            Duration::from_secs(10),
        )
        .await,
        "A should receive B's second (duplicate) redemption Join"
    );
    let materialized_a: MaterializedMembership = {
        let s = state_a.lock().await;
        s.materialize_now(owner_a)
    };
    let dto_a = member_info_for(&materialized_a);
    assert_eq!(dto_a, dto_b, "A and B agree on materialized member list");

    engine_a.shutdown().await.expect("shutdown a");
    engine_b.shutdown().await.expect("shutdown b");
}

mod bootstrap_admit_open_publisher_tests {
    use super::*;
    use harmony_app::community_membership::{bootstrap_admit_open_publisher, MemberStatus};

    // A freshly-minted open community: returns (admin_owner, community_id,
    // admin's signature-valid open bootstrap Join event).
    fn mint_open_admin_join(
        seed: u8,
    ) -> (
        OwnerAddr,
        harmony_app::owner_state_types::SpaceId,
        harmony_app::community_membership::SignedMembershipEvent,
    ) {
        let t = harmony_app::community_membership::mint_test_owner(seed);
        let minted = mint_community_creation(
            "BootstrapTest",
            false, // open
            t.owner,
            &t.device_key,
            &t.cert,
            Hlc {
                wall_ms: 100_000,
                logical: 0,
                device_id: format!("dev-{seed}"),
            },
        )
        .expect("mint create");
        (t.owner, minted.community_id, minted.bootstrap_join)
    }

    // A root publish HLC well after every Join minted above (admin@100_000,
    // joiner@200_000) — so those Joins fall strictly before the root and the
    // bootstrap window admits them.
    fn root_hlc() -> Hlc {
        Hlc {
            wall_ms: 1_000_000,
            logical: 0,
            device_id: "root-publish".into(),
        }
    }

    #[test]
    fn admits_publisher_with_valid_open_self_join() {
        let (admin, community_id, admin_join) = mint_open_admin_join(0xC1);
        // Publisher == admin (the creator-publish direction): the admin's own
        // self-Join is in the blob; admit with their enrolled key seeded.
        let got = bootstrap_admit_open_publisher(
            std::slice::from_ref(&admin_join),
            admin,
            admin,
            community_id,
            &root_hlc(),
        );
        let ms = got.expect("valid open self-Join must admit");
        assert!(matches!(ms.status, MemberStatus::Joined));
        assert!(
            !ms.enrolled_device_keys.is_empty(),
            "enrolled keys must be seeded from the cert"
        );
    }

    #[test]
    fn admits_joiner_self_join_from_redemption() {
        // The joiner direction: a redemption-minted self-Join for a DIFFERENT
        // owner against the same open community must admit.
        let (admin, community_id, _admin_join) = mint_open_admin_join(0xC2);
        let joiner = harmony_app::community_membership::mint_test_owner(0xD3);
        let invite = harmony_app::community_invite::CommunityInvitePayload {
            inviter_signer_certs: Vec::new(),
            community_id,
            epoch_snapshot: harmony_app::community_invite::InviteEpochSnapshot {
                epoch: 0,
                sealed_epoch_key: vec![0u8; 32],
                sealed_epoch_keys: Vec::new(),
                state_snapshot: harmony_app::community_invite::MaterializedCommunityState::default(
                ),
            },
            admin_addr: admin,
            community_name: "BootstrapTest".into(),
            is_invite_only: false,
            expires_at: None,
            invite_token: None,
            admin_bootstrap: None,
            admin_identity_pub: None,
            forked_from: None,
            pre_fork_snapshot: None,
            inviter_enrollment: None,
            untargeted_decrypt_key: None,
        };
        let minted = mint_redemption(
            &invite,
            joiner.owner,
            &joiner.device_key,
            &joiner.cert,
            Hlc {
                wall_ms: 200_000,
                logical: 0,
                device_id: "joiner-dev".into(),
            },
        )
        .expect("mint redeem");
        let got = bootstrap_admit_open_publisher(
            std::slice::from_ref(&minted.bootstrap_join),
            joiner.owner,
            admin,
            community_id,
            &root_hlc(),
        );
        let ms = got.expect("joiner self-Join must admit");
        assert!(matches!(ms.status, MemberStatus::Joined));
        assert!(!ms.enrolled_device_keys.is_empty());
    }

    #[test]
    fn rejects_when_no_self_join_for_publisher() {
        // Blob carries the admin's Join but the publisher is a stranger with
        // no Join present → None.
        let (admin, community_id, admin_join) = mint_open_admin_join(0xC4);
        let stranger = harmony_app::community_membership::mint_test_owner(0xE5).owner;
        let got = bootstrap_admit_open_publisher(
            std::slice::from_ref(&admin_join),
            stranger,
            admin,
            community_id,
            &root_hlc(),
        );
        assert!(got.is_none(), "no self-Join for the publisher ⇒ reject");
    }

    #[test]
    fn rejects_self_join_for_wrong_community() {
        // A valid Join, but for a different community_id than the gate expects
        // ⇒ verify_event's WrongCommunity guard fires ⇒ None.
        let (admin, _community_id, admin_join) = mint_open_admin_join(0xC6);
        let other_community = harmony_app::owner_state_types::SpaceId([0x99; 16]);
        let got = bootstrap_admit_open_publisher(
            std::slice::from_ref(&admin_join),
            admin,
            admin,
            other_community,
            &root_hlc(),
        );
        assert!(
            got.is_none(),
            "Join for a different community must not admit"
        );
    }
}

/// ZEB-558: open-community join must converge over the WIRE with no manual
/// cross-seeding. This is the faithful repro of the production deadlock that
/// `open_community_create_redeem_leave_round_trip` masks with its explicit
/// `insert_local_event` pre-seeds. Pre-fix this DEADLOCKS (each engine
/// rejects the other's publish `publisher_not_joined`); post-fix the gate's
/// deferred bootstrap-admission converges both.
#[tokio::test]
async fn open_community_two_node_wire_convergence_no_preseed() {
    let owner_a_test = harmony_app::community_membership::mint_test_owner(0x5A);
    let owner_b_test = harmony_app::community_membership::mint_test_owner(0x5B);
    let owner_a = owner_a_test.owner;
    let owner_b = owner_b_test.owner;
    let signing_a = owner_a_test.device_key.clone();
    let signing_b = owner_b_test.device_key.clone();

    let resolver: Arc<dyn IdentityResolver> = Arc::new(TwoIdentityResolver {
        a: (owner_a, [0u8; 64]),
        b: (owner_b, [0u8; 64]),
    });

    // Shared in-memory CAS (mirrors the round-trip test).
    let cas: Arc<Mutex<HashMap<harmony_content::cid::ContentId, Vec<u8>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let (cas_op_tx, mut cas_op_rx) = mpsc::channel(64);
    let cas_for_servicer = Arc::clone(&cas);
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

    // Bidirectional wire forwarders.
    let (a_out_tx, mut a_out_rx) = mpsc::channel::<Vec<u8>>(64);
    let (a_in_tx, a_in_rx) = mpsc::channel::<Vec<u8>>(64);
    let (b_out_tx, mut b_out_rx) = mpsc::channel::<Vec<u8>>(64);
    let (b_in_tx, b_in_rx) = mpsc::channel::<Vec<u8>>(64);
    let a_in_for_fwd = a_in_tx.clone();
    tokio::spawn(async move {
        while let Some(bytes) = b_out_rx.recv().await {
            let _ = a_in_for_fwd.send(bytes).await;
        }
    });
    let b_in_for_fwd = b_in_tx.clone();
    tokio::spawn(async move {
        while let Some(bytes) = a_out_rx.recv().await {
            let _ = b_in_for_fwd.send(bytes).await;
        }
    });

    let minted_a = mint_community_creation(
        "WireConvergence",
        false,
        owner_a,
        &signing_a,
        &owner_a_test.cert,
        Hlc {
            wall_ms: 100_000,
            logical: 0,
            device_id: "a-dev".to_string(),
        },
    )
    .expect("mint create");
    let community_id = minted_a.community_id;

    let cs_a: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx.clone(),
        Duration::from_secs(2),
    ));
    let cs_b: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx.clone(),
        Duration::from_secs(2),
    ));

    let state_a = Arc::new(Mutex::new(CommunityState::new(community_id)));
    let state_b = Arc::new(Mutex::new(CommunityState::new(community_id)));
    let tracker_a = Arc::new(Mutex::new(CommunityReplayTracker::new((
        owner_a,
        "a-dev".to_string(),
    ))));
    let tracker_b = Arc::new(Mutex::new(CommunityReplayTracker::new((
        owner_b,
        "b-dev".to_string(),
    ))));
    let (delta_a_tx, mut _delta_a_rx) = mpsc::channel::<CommunityMembershipDelta>(32);
    let (delta_b_tx, mut _delta_b_rx) = mpsc::channel::<CommunityMembershipDelta>(32);
    let tmp_a = tempfile::tempdir().expect("tmp a");
    let tmp_b = tempfile::tempdir().expect("tmp b");

    let engine_a = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        community_id,
        membership_key: minted_a.membership_key.clone(),
        admin_addr: owner_a,
        is_invite_only: false,
        device_id: "a-dev".into(),
        self_owner: owner_a,
        signing_key: Arc::new(signing_a.clone()),
        state: Arc::clone(&state_a),
        tracker: Arc::clone(&tracker_a),
        content_store: cs_a,
        publisher_tx: a_out_tx,
        subscriber_rx: a_in_rx,
        paths: PersistPaths {
            crdt: tmp_a.path().join("crdt.cbor"),
            replay: tmp_a.path().join("replay.cbor"),
        },
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        identity_resolver: Some(Arc::clone(&resolver)),
        error_tx: None,
        delta_tx: Some(delta_a_tx),
        pending_redemptions: None,
        crdt_state: None,
        admin_identity_pub: None,
        nav_emitter: None,
        root_serve_rx: None,
    });
    let engine_b = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        community_id,
        membership_key: minted_a.membership_key.clone(),
        admin_addr: owner_a,
        is_invite_only: false,
        device_id: "b-dev".into(),
        self_owner: owner_b,
        signing_key: Arc::new(signing_b.clone()),
        state: Arc::clone(&state_b),
        tracker: Arc::clone(&tracker_b),
        content_store: cs_b,
        publisher_tx: b_out_tx,
        subscriber_rx: b_in_rx,
        paths: PersistPaths {
            crdt: tmp_b.path().join("crdt.cbor"),
            replay: tmp_b.path().join("replay.cbor"),
        },
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        identity_resolver: Some(Arc::clone(&resolver)),
        error_tx: None,
        delta_tx: Some(delta_b_tx),
        pending_redemptions: None,
        crdt_state: None,
        admin_identity_pub: None,
        nav_emitter: None,
        root_serve_rx: None,
    });

    // A inserts its bootstrap Join → publishes over the wire.
    engine_a
        .insert_local_event(minted_a.bootstrap_join.clone())
        .await
        .expect("A bootstrap insert");

    // B redeems the open invite + inserts ONLY its own Join → publishes.
    // NO manual cross-seed of A's Join into B, and NO seed of B's Join into A.
    let invite_payload = harmony_app::community_invite::CommunityInvitePayload {
        inviter_signer_certs: Vec::new(),
        community_id,
        epoch_snapshot: harmony_app::community_invite::InviteEpochSnapshot {
            epoch: 0,
            sealed_epoch_key: minted_a.membership_key.as_bytes().to_vec(),
            sealed_epoch_keys: Vec::new(),
            state_snapshot: harmony_app::community_invite::MaterializedCommunityState::default(),
        },
        admin_addr: owner_a,
        community_name: "WireConvergence".into(),
        is_invite_only: false,
        expires_at: None,
        invite_token: None,
        admin_bootstrap: None,
        admin_identity_pub: None,
        forked_from: None,
        pre_fork_snapshot: None,
        inviter_enrollment: None,
        untargeted_decrypt_key: None,
    };
    let minted_b = mint_redemption(
        &invite_payload,
        owner_b,
        &signing_b,
        &owner_b_test.cert,
        Hlc {
            wall_ms: 200_000,
            logical: 0,
            device_id: "b-dev".to_string(),
        },
    )
    .expect("mint redeem");
    engine_b
        .insert_local_event(minted_b.bootstrap_join.clone())
        .await
        .expect("B redemption insert");

    // Convergence over the WIRE: each engine must learn the other's Join with
    // no pre-seed. Pre-fix this times out (mutual publisher_not_joined reject).
    // `>= 2` (not `== 2`): a debounced re-publish could merge and push the count
    // past 2 before both polls observe the transient `== 2`, permanently
    // bypassing an exact-equality condition. The roster-equality assertion below
    // does the real semantic check.
    let a_has_both = wait_until(
        || async { state_a.lock().await.event_count() >= 2 },
        Duration::from_secs(10),
    )
    .await;
    let b_has_both = wait_until(
        || async { state_b.lock().await.event_count() >= 2 },
        Duration::from_secs(10),
    )
    .await;
    assert!(
        a_has_both,
        "A must learn B's Join over the wire (no pre-seed)"
    );
    assert!(
        b_has_both,
        "B must learn A's Join over the wire (no pre-seed)"
    );

    // Both materialize the same 2-member roster.
    let dto_a = member_info_for(&state_a.lock().await.materialize_now(owner_a));
    let dto_b = member_info_for(&state_b.lock().await.materialize_now(owner_a));
    assert_eq!(dto_a.len(), 2, "A roster = admin + joiner");
    assert_eq!(dto_b.len(), 2, "B roster = admin + joiner");
    assert_eq!(dto_a, dto_b, "rosters must agree");

    engine_a.shutdown().await.expect("shutdown a");
    engine_b.shutdown().await.expect("shutdown b");
}

/// ZEB-526 Fix 1: invite-only self-Join admission must converge over the WIRE.
///
/// This is the peering-free integration proof of the invite-only deferred
/// self-Join admission. The co-located e2e (s10) cannot prove it because its
/// plain `redeem_invite` path is not zenoh-peered — the joiner's PendingJoin
/// reaches the admin only over the state-root publish, which is exactly the
/// path this test exercises.
///
/// Topology mirrors the open-community `open_community_two_node_wire_convergence_no_preseed`
/// test but for an INVITE-ONLY community, with the publisher/receiver roles
/// chosen to match the production fix:
///   * engine A = ADMIN + RECEIVER. It holds only its own bootstrap Join and
///     is NOT pre-seeded with B's PendingJoin (the whole point — A must admit
///     B's join straight off the wire).
///   * engine B = JOINER + PUBLISHER. It mints a self-authorizing PendingJoin
///     (admin-signed `InviteToken` + its EnrollmentCert) via `mint_redemption`
///     and publishes its state-root.
///
/// Pre-fix, A rejected B's publish outright (`publisher_not_joined`,
/// pre-decode) because B was an entirely-unknown publisher in an invite-only
/// community. Post-fix, A DEFERS, decodes the blob, salvages B's PendingJoin
/// via `bootstrap_admit_invite_only_publisher`, verifies the root publisher
/// signature against B's enrolled keys, and the authoritative merge inserts
/// the PendingJoin — firing the admin's auto-counter-sign, which materializes
/// B from PendingJoin to **Joined** in A's own CommunityState.
///
/// B IS pre-seeded with A's bootstrap Join: for invite-only communities this
/// seed remains correctness-required (the joiner legitimately learns the admin
/// from the invite payload; the `InviteToken`'s admin-signer key must resolve
/// from the admin's own Join when verifying B's PendingJoin). See the note at
/// `open_community_create_redeem_leave_round_trip` lines ~248-262.
#[tokio::test]
async fn invite_only_admin_admits_joiner_pending_join_over_wire() {
    use ed25519_dalek::Signer;
    use harmony_app::community_invite::{canonical_invite_token_bytes, InviteToken};
    use harmony_app::community_membership::MemberStatus;

    let owner_a_test = harmony_app::community_membership::mint_test_owner(0x7A);
    let owner_b_test = harmony_app::community_membership::mint_test_owner(0x7B);
    let owner_a = owner_a_test.owner;
    let owner_b = owner_b_test.owner;
    let signing_a = owner_a_test.device_key.clone();
    let signing_b = owner_b_test.device_key.clone();

    let resolver: Arc<dyn IdentityResolver> = Arc::new(TwoIdentityResolver {
        a: (owner_a, [0u8; 64]),
        b: (owner_b, [0u8; 64]),
    });

    // INVITE-ONLY-specific: a PendingJoin renders as PendingJoin only while it is
    // within the 30-day `MATERIALIZE_PENDING_EXPIRY_MS` window relative to the
    // materialization `now` (an un-countersigned, expired pending is hidden). The
    // receiver materializes B's publish at the WIRE root HLC, which the engine
    // derives from the real wall clock. So unlike the open test, these Join /
    // PendingJoin HLCs must sit near the present, not at synthetic small values —
    // otherwise B's PendingJoin is born already-expired and never admits.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_millis() as u64;
    let admin_join_ms = now_ms - 60_000;
    let token_minted_ms = now_ms - 55_000;
    let pending_join_ms = now_ms - 50_000;

    // Shared in-memory CAS (mirrors the open wire-convergence test).
    let cas: Arc<Mutex<HashMap<harmony_content::cid::ContentId, Vec<u8>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let (cas_op_tx, mut cas_op_rx) = mpsc::channel(64);
    let cas_for_servicer = Arc::clone(&cas);
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

    // Bidirectional wire forwarders: A's publisher → B's subscriber and
    // B's publisher → A's subscriber. The load-bearing direction here is
    // B → A (the joiner's state-root publish reaching the admin).
    let (a_out_tx, mut a_out_rx) = mpsc::channel::<Vec<u8>>(64);
    let (a_in_tx, a_in_rx) = mpsc::channel::<Vec<u8>>(64);
    let (b_out_tx, mut b_out_rx) = mpsc::channel::<Vec<u8>>(64);
    let (b_in_tx, b_in_rx) = mpsc::channel::<Vec<u8>>(64);
    let a_in_for_fwd = a_in_tx.clone();
    tokio::spawn(async move {
        while let Some(bytes) = b_out_rx.recv().await {
            let _ = a_in_for_fwd.send(bytes).await;
        }
    });
    let b_in_for_fwd = b_in_tx.clone();
    tokio::spawn(async move {
        while let Some(bytes) = a_out_rx.recv().await {
            let _ = b_in_for_fwd.send(bytes).await;
        }
    });

    // A mints an INVITE-ONLY community + its bootstrap Join (admin = owner_a).
    let minted_a = mint_community_creation(
        "InviteOnlyWireConvergence",
        true, // is_invite_only
        owner_a,
        &signing_a,
        &owner_a_test.cert,
        Hlc {
            wall_ms: admin_join_ms,
            logical: 0,
            device_id: "a-dev".to_string(),
        },
    )
    .expect("mint create");
    let community_id = minted_a.community_id;

    let cs_a: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx.clone(),
        Duration::from_secs(2),
    ));
    let cs_b: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx.clone(),
        Duration::from_secs(2),
    ));

    let state_a = Arc::new(Mutex::new(CommunityState::new(community_id)));
    let state_b = Arc::new(Mutex::new(CommunityState::new(community_id)));
    let tracker_a = Arc::new(Mutex::new(CommunityReplayTracker::new((
        owner_a,
        "a-dev".to_string(),
    ))));
    let tracker_b = Arc::new(Mutex::new(CommunityReplayTracker::new((
        owner_b,
        "b-dev".to_string(),
    ))));
    let (delta_a_tx, mut _delta_a_rx) = mpsc::channel::<CommunityMembershipDelta>(32);
    let (delta_b_tx, mut _delta_b_rx) = mpsc::channel::<CommunityMembershipDelta>(32);
    let tmp_a = tempfile::tempdir().expect("tmp a");
    let tmp_b = tempfile::tempdir().expect("tmp b");

    let engine_a = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        community_id,
        membership_key: minted_a.membership_key.clone(),
        admin_addr: owner_a,
        is_invite_only: true,
        device_id: "a-dev".into(),
        self_owner: owner_a,
        signing_key: Arc::new(signing_a.clone()),
        state: Arc::clone(&state_a),
        tracker: Arc::clone(&tracker_a),
        content_store: cs_a,
        publisher_tx: a_out_tx,
        subscriber_rx: a_in_rx,
        paths: PersistPaths {
            crdt: tmp_a.path().join("crdt.cbor"),
            replay: tmp_a.path().join("replay.cbor"),
        },
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        identity_resolver: Some(Arc::clone(&resolver)),
        error_tx: None,
        delta_tx: Some(delta_a_tx),
        pending_redemptions: None,
        crdt_state: None,
        admin_identity_pub: None,
        nav_emitter: None,
        root_serve_rx: None,
    });
    let engine_b = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        community_id,
        membership_key: minted_a.membership_key.clone(),
        admin_addr: owner_a,
        is_invite_only: true,
        device_id: "b-dev".into(),
        self_owner: owner_b,
        signing_key: Arc::new(signing_b.clone()),
        state: Arc::clone(&state_b),
        tracker: Arc::clone(&tracker_b),
        content_store: cs_b,
        publisher_tx: b_out_tx,
        subscriber_rx: b_in_rx,
        paths: PersistPaths {
            crdt: tmp_b.path().join("crdt.cbor"),
            replay: tmp_b.path().join("replay.cbor"),
        },
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        identity_resolver: Some(Arc::clone(&resolver)),
        error_tx: None,
        delta_tx: Some(delta_b_tx),
        pending_redemptions: None,
        crdt_state: None,
        admin_identity_pub: None,
        nav_emitter: None,
        root_serve_rx: None,
    });

    // A inserts its own bootstrap Join (community creation) → publishes.
    engine_a
        .insert_local_event(minted_a.bootstrap_join.clone())
        .await
        .expect("A bootstrap insert");

    // B is pre-seeded with A's bootstrap Join. Unlike the OPEN path, this seed
    // is correctness-required for invite-only: B's PendingJoin carries an
    // admin-signed `InviteToken` whose signer key must resolve from A's own
    // Join when verify_event runs. Production reconstructs this from the invite
    // payload's `admin_bootstrap`; here we insert it directly.
    engine_b
        .insert_local_event(minted_a.bootstrap_join.clone())
        .await
        .expect("B pre-seed A's bootstrap Join (invite-only correctness-required)");

    // Build a real admin-signed InviteToken (signed by A's enrolled device key)
    // and seal the epoch key to B's enrolled X25519 key, exactly as production's
    // targeted invite-only invite does (cf. pkarr_iroh_redeem_full_integration).
    let token_minted_at = Hlc {
        wall_ms: token_minted_ms,
        logical: 0,
        device_id: "a-dev".into(),
    };
    let invite_token_unsigned = InviteToken {
        inviter: owner_a,
        invitee_hint: Some(owner_b),
        minted_at: token_minted_at.clone(),
        expires_at: None,
        sig: [0u8; 64],
    };
    let token_bytes =
        canonical_invite_token_bytes(&invite_token_unsigned).expect("canonical token bytes");
    let token_sig: [u8; 64] = signing_a.sign(&token_bytes).to_bytes();
    let invite_token = InviteToken {
        inviter: owner_a,
        invitee_hint: Some(owner_b),
        minted_at: token_minted_at,
        expires_at: None,
        sig: token_sig,
    };

    // Seal the epoch key to B's enrolled device key's X25519 pub.
    // mint_redemption decrypts it via ed25519_priv_to_x25519(signing_b).
    let b_x25519_pub = {
        let verifying_bytes = signing_b.verifying_key().to_bytes();
        harmony_app::dm_signing::ed25519_pub_to_x25519(&verifying_bytes)
            .expect("B ed25519 → x25519")
    };
    let sealed_epoch_key =
        harmony_app::dm_signing::seal_to_owner(&b_x25519_pub, minted_a.membership_key.as_bytes())
            .expect("seal epoch key to B");

    let invite_payload = harmony_app::community_invite::CommunityInvitePayload {
        inviter_signer_certs: Vec::new(),
        community_id,
        epoch_snapshot: harmony_app::community_invite::InviteEpochSnapshot {
            epoch: 0,
            // Targeted invite shape: per-device sealed envelope in
            // sealed_epoch_keys, sealed_epoch_key empty.
            sealed_epoch_key: Vec::new(),
            sealed_epoch_keys: vec![sealed_epoch_key],
            state_snapshot: harmony_app::community_invite::MaterializedCommunityState::default(),
        },
        admin_addr: owner_a,
        community_name: "InviteOnlyWireConvergence".into(),
        is_invite_only: true,
        expires_at: None,
        invite_token: Some(invite_token),
        admin_bootstrap: Some(minted_a.bootstrap_join.clone()),
        admin_identity_pub: None,
        forked_from: None,
        pre_fork_snapshot: None,
        // Invite-only payloads require the inviter's EnrollmentCert so the
        // InviteToken signer key binds to the admin actor.
        inviter_enrollment: Some(owner_a_test.cert.clone()),
        untargeted_decrypt_key: None,
    };

    // B mints its self-authorizing PendingJoin and inserts it locally → its
    // state-root (admin Join + B's PendingJoin) publishes over the wire to A.
    let minted_b = mint_redemption(
        &invite_payload,
        owner_b,
        &signing_b,
        &owner_b_test.cert,
        Hlc {
            wall_ms: pending_join_ms,
            logical: 0,
            device_id: "b-dev".to_string(),
        },
    )
    .expect("mint redeem (invite-only PendingJoin)");
    engine_b
        .insert_local_event(minted_b.bootstrap_join.clone())
        .await
        .expect("B PendingJoin insert");

    // A must ADMIT B's PendingJoin straight off the wire (no pre-seed of B in
    // A). Pre-fix this never happens — A rejects B's publish pre-decode. We
    // first confirm B reaches A's materialized membership AT ALL (the load-
    // bearing thing the fix enables), then that A's auto-counter-sign drives B
    // all the way to Joined.
    let b_reached_a = wait_until(
        || async {
            let s = state_a.lock().await;
            let mat = harmony_app::community_membership::materialize(
                &s.events().cloned().collect::<Vec<_>>(),
                owner_a,
            );
            matches!(
                mat.members.get(&owner_b).map(|m| m.status),
                Some(MemberStatus::PendingJoin) | Some(MemberStatus::Joined)
            )
        },
        Duration::from_secs(15),
    )
    .await;
    assert!(
        b_reached_a,
        "A must admit B's PendingJoin off the wire (the ZEB-526 Fix 1 invariant; \
         pre-fix A rejected B's publish pre-decode)"
    );

    // Strongest true condition: A's auto-counter-sign fires (A is a Joined
    // admin with power ≥ the invite threshold), materializing B to Joined.
    let b_joined_on_a = wait_until(
        || async {
            let s = state_a.lock().await;
            let mat = harmony_app::community_membership::materialize(
                &s.events().cloned().collect::<Vec<_>>(),
                owner_a,
            );
            matches!(
                mat.members.get(&owner_b).map(|m| m.status),
                Some(MemberStatus::Joined)
            )
        },
        Duration::from_secs(15),
    )
    .await;
    assert!(
        b_joined_on_a,
        "A's auto-counter-sign must materialize B to Joined (PendingJoin → JoinCountersign → Joined)"
    );

    // Roster sanity: A now holds exactly {admin (Joined), joiner (Joined)}.
    let dto_a = member_info_for(&state_a.lock().await.materialize_now(owner_a));
    assert_eq!(dto_a.len(), 2, "A roster = admin + joiner");
    let a_row = dto_a
        .iter()
        .find(|d| d.addr == hex::encode(owner_a.0))
        .expect("admin in A's roster");
    assert_eq!(a_row.status, MemberStatusDto::Joined);
    let b_row = dto_a
        .iter()
        .find(|d| d.addr == hex::encode(owner_b.0))
        .expect("joiner admitted into A's roster off the wire");
    assert_eq!(
        b_row.status,
        MemberStatusDto::Joined,
        "joiner materializes to Joined on the admin after auto-counter-sign"
    );

    engine_a.shutdown().await.expect("shutdown a");
    engine_b.shutdown().await.expect("shutdown b");
}
