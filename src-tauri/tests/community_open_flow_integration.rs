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
    CommunityMembershipDelta, CommunityRootHlcTracker, CommunitySyncEngine,
    CommunitySyncEngineConfig, IdentityResolver, PersistPaths, DEFAULT_DEBOUNCE_MS,
};
use harmony_app::content_store::{CasOp, ContentStore, RuntimeContentStore};
use harmony_app::owner_state_types::OwnerAddr;
use harmony_app::{
    delta_to_change, member_info_for, mint_community_creation, mint_leave_event, mint_redemption,
    MemberStatusDto, MembershipChangeType,
};
use harmony_identity::PrivateIdentity;
use std::collections::HashMap;
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

/// Reach into `PrivateIdentity`'s ed25519 seed the same way production
/// does (see `lib.rs::start_node` and Task 9's unit test): the canonical
/// 32-byte seed lives in bytes 32..64 of `to_private_bytes()`
/// (`X25519_secret(32) || Ed25519_secret(32)`). Construct an
/// `ed25519_dalek::SigningKey` from those bytes so the test signs with
/// the same key the IPC will use in production.
fn signing_key_from(identity: &PrivateIdentity) -> ed25519_dalek::SigningKey {
    let private_bytes = identity.to_private_bytes();
    let mut secret = [0u8; 32];
    secret.copy_from_slice(&private_bytes[32..64]);
    ed25519_dalek::SigningKey::from_bytes(&secret)
}

#[tokio::test]
async fn open_community_create_redeem_leave_round_trip() {
    let identity_a = PrivateIdentity::from_seed(&[0xa1; 32]);
    let identity_b = PrivateIdentity::from_seed(&[0xb2; 32]);
    let owner_a = OwnerAddr(identity_a.identity.address_hash);
    let owner_b = OwnerAddr(identity_b.identity.address_hash);
    let pub_a = identity_a.identity.to_public_bytes();
    let pub_b = identity_b.identity.to_public_bytes();
    let signing_a = signing_key_from(&identity_a);
    let signing_b = signing_key_from(&identity_b);

    let resolver: Arc<dyn IdentityResolver> = Arc::new(TwoIdentityResolver {
        a: (owner_a, pub_a),
        b: (owner_b, pub_b),
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
        "a-dev",
        100_000,
        None,
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
    let tracker_a = Arc::new(Mutex::new(CommunityRootHlcTracker::default()));
    let tracker_b = Arc::new(Mutex::new(CommunityRootHlcTracker::default()));

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
    });
    let engine_b = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        community_id,
        membership_key: minted_a.membership_key.clone(),
        admin_addr: owner_a,
        is_invite_only: false,
        device_id: "b-dev".into(),
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

    // B should converge on A's bootstrap Join via the bridge.
    assert!(
        wait_until(
            || async { state_b.lock().await.events.len() == 1 },
            Duration::from_secs(10),
        )
        .await,
        "B should receive A's bootstrap Join"
    );
    let _delta_b_remote = tokio::time::timeout(Duration::from_secs(2), delta_b_rx.recv())
        .await
        .expect("B remote delta")
        .expect("channel open");

    // ── Step 2: B redeems an invite for the same community. ────────────
    let invite_payload = harmony_app::community_invite::CommunityInvitePayload {
        community_id,
        membership_key: minted_a.membership_key.clone(),
        admin_addr: owner_a,
        community_name: "TestCommunity".into(),
        is_invite_only: false,
        expires_at: None,
        invite_token: None,
    };

    let minted_b = mint_redemption(&invite_payload, owner_b, &signing_b, "b-dev", 200_000, None)
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

    // A should converge on B's redemption Join.
    assert!(
        wait_until(
            || async { state_a.lock().await.events.len() == 2 },
            Duration::from_secs(10),
        )
        .await,
        "A should receive B's redemption Join"
    );
    let _delta_a_remote = tokio::time::timeout(Duration::from_secs(2), delta_a_rx.recv())
        .await
        .expect("A remote delta")
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
    let leave_b = mint_leave_event(
        community_id,
        owner_b,
        &signing_b,
        "b-dev",
        300_000,
        Some(&minted_b.bootstrap_join.at),
    )
    .expect("mint leave");
    let leave_outcome = engine_b
        .insert_local_event(leave_b)
        .await
        .expect("B leave insert");
    assert_eq!(leave_outcome, InsertOutcome::Inserted);

    assert!(
        wait_until(
            || async {
                let s = state_a.lock().await;
                s.events.len() == 3
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
    let identity_a = PrivateIdentity::from_seed(&[0xa3; 32]);
    let identity_b = PrivateIdentity::from_seed(&[0xb4; 32]);
    let owner_a = OwnerAddr(identity_a.identity.address_hash);
    let owner_b = OwnerAddr(identity_b.identity.address_hash);
    let pub_a = identity_a.identity.to_public_bytes();
    let pub_b = identity_b.identity.to_public_bytes();
    let signing_a = signing_key_from(&identity_a);
    let signing_b = signing_key_from(&identity_b);

    let resolver: Arc<dyn IdentityResolver> = Arc::new(TwoIdentityResolver {
        a: (owner_a, pub_a),
        b: (owner_b, pub_b),
    });

    // Same shared in-memory CAS shape as the round-trip test.
    let cas: Arc<Mutex<HashMap<harmony_content::cid::ContentId, Vec<u8>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let (cas_op_tx, mut cas_op_rx) = mpsc::channel(64);
    let cas_for_servicer = Arc::clone(&cas);
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
        "a-dev",
        100_000,
        None,
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
    let tracker_a = Arc::new(Mutex::new(CommunityRootHlcTracker::default()));
    let tracker_b = Arc::new(Mutex::new(CommunityRootHlcTracker::default()));

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
    });
    let engine_b = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        community_id,
        membership_key: minted_a.membership_key.clone(),
        admin_addr: owner_a,
        is_invite_only: false,
        device_id: "b-dev".into(),
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
    assert!(
        wait_until(
            || async { state_b.lock().await.events.len() == 1 },
            Duration::from_secs(10),
        )
        .await,
        "B should receive A's bootstrap Join"
    );
    tokio::time::timeout(Duration::from_secs(2), delta_b_rx.recv())
        .await
        .expect("B remote delta (A's bootstrap) did not arrive within 2s")
        .expect("B delta channel closed before A's bootstrap delta arrived");

    // ── First redemption: B mints + inserts ───────────────────────────
    let invite_payload = harmony_app::community_invite::CommunityInvitePayload {
        community_id,
        membership_key: minted_a.membership_key.clone(),
        admin_addr: owner_a,
        community_name: "TestCommunity".into(),
        is_invite_only: false,
        expires_at: None,
        invite_token: None,
    };

    let minted_b1 = mint_redemption(&invite_payload, owner_b, &signing_b, "b-dev", 200_000, None)
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
    assert!(
        wait_until(
            || async { state_a.lock().await.events.len() == 2 },
            Duration::from_secs(10),
        )
        .await,
        "A should receive B's first redemption Join"
    );
    tokio::time::timeout(Duration::from_secs(2), delta_a_rx.recv())
        .await
        .expect("A remote delta (B's first redemption) did not arrive within 2s")
        .expect("A delta channel closed before B's redemption delta arrived");

    // ── Second redemption: B mints + inserts AGAIN with the same URL.
    //     Distinct event_id (random) and HLC tick advance produce a
    //     CRDT-distinct event, so InsertOutcome::Inserted again. ──────
    let minted_b2 = mint_redemption(
        &invite_payload,
        owner_b,
        &signing_b,
        "b-dev",
        300_000,
        Some(&minted_b1.bootstrap_join.at),
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
            s.events.len(),
            3,
            "event log should have A's bootstrap + B's two redemption Joins"
        );
    }

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
            || async { state_a.lock().await.events.len() == 3 },
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
