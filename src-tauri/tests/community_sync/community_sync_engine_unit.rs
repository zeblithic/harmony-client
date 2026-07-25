//! CommunitySyncEngine unit tests — covers (1) construction + clean
//! shutdown, (2) the flush_now → publish_root_now → wire-bytes
//! path, and (3) the two-engine round-trip exercising
//! handle_incoming_publish with verify-on-receive. The multi-engine
//! registry lands in Task 11; the start_node wire-up in Task 13.

use harmony_app::community_state_crdt::CommunityState;
use harmony_app::community_state_sync::{
    CommunityRootHlcTracker, CommunitySyncEngine, CommunitySyncEngineConfig, DEFAULT_DEBOUNCE_MS,
};
use harmony_app::content_store::{ContentStore, RuntimeContentStore};
use harmony_app::owner_state_types::{EpochKey, OwnerAddr, SpaceId};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

use harmony_app::community_membership::{
    mint_test_owner, sign_event, EventPayload, MembershipEventKind, SignedMembershipEvent,
    TestOwner,
};
use harmony_app::owner_state_types::Hlc;
use harmony_identity::PrivateIdentity;

/// ZEB-339: sign a membership event with the owner's enrolled device key (#2),
/// attaching the owner's Master cert on identity-introducing events (Join /
/// PendingJoin) so `materialize`/`insert` populates `enrolled_device_keys` and
/// the engine's publisher-sig + verify_event paths resolve the signer.
fn sign_event_with_identity(
    payload: &EventPayload,
    owner: &TestOwner,
) -> Result<SignedMembershipEvent, harmony_app::owner_state_crypto::CryptoError> {
    let ev = sign_event(payload, &owner.device_key)?;
    Ok(match ev.kind {
        MembershipEventKind::Join | MembershipEventKind::PendingJoin { .. } => {
            SignedMembershipEvent {
                enrollment: Some(owner.cert.clone()),
                ..ev
            }
        }
        _ => ev,
    })
}

/// `IdentityResolver` backed by an in-memory `HashMap`. Used across the
/// receive-side rejection tests below; maps a fixed `OwnerAddr` →
/// `identity_pub` set, returns `None` for any other addr. More general
/// than the per-test `SingleIdentityResolver` stubs because the
/// spoofing scenarios need both alice + bob in the same resolver to
/// hit the sig-verify gate (resolver hands back alice's pub, but the
/// envelope was signed with bob's key).
struct MapResolver {
    entries: std::collections::HashMap<OwnerAddr, [u8; 64]>,
}

#[async_trait::async_trait]
impl harmony_app::community_state_sync::IdentityResolver for MapResolver {
    async fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
        self.entries.get(addr).copied()
    }
}

#[tokio::test]
async fn engine_constructs_and_shuts_down_cleanly() {
    let (out_tx, _out_rx) = mpsc::channel::<Vec<u8>>(8);
    let (_in_tx, in_rx) = mpsc::channel::<Vec<u8>>(8);
    let (cas_op_tx, _cas_op_rx) = mpsc::channel(8);

    let community_id = SpaceId([1u8; 16]);
    let mk = EpochKey::new([0x42; 32]);
    let admin = OwnerAddr([2u8; 16]);

    let state = Arc::new(Mutex::new(CommunityState::new(community_id)));
    let tracker = Arc::new(Mutex::new(CommunityRootHlcTracker::default()));
    let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx,
        std::time::Duration::from_millis(1000),
    ));

    let tmp = tempfile::tempdir().expect("tempdir");

    let engine = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        community_id,
        membership_key: mk,
        admin_addr: admin,
        is_invite_only: false,
        device_id: "test-device".into(),
        self_owner: admin,
        signing_key: Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0x42; 32])),
        state: Arc::clone(&state),
        tracker: Arc::clone(&tracker),
        content_store: cs,
        publisher_tx: out_tx,
        subscriber_rx: in_rx,
        paths: harmony_app::community_state_sync::PersistPaths {
            crdt: tmp.path().join("crdt.cbor"),
            replay: tmp.path().join("replay.cbor"),
        },
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        identity_resolver: None,
        error_tx: None,
        delta_tx: None,
        pending_redemptions: None,
        crdt_state: None,
        admin_identity_pub: None,
        nav_emitter: None,
        root_serve_rx: None,
    });

    // Shutdown without ever sending dirty — clean path.
    engine.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn flush_now_publishes_one_root_publish() {
    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(8);
    let (_in_tx, in_rx) = mpsc::channel::<Vec<u8>>(8);
    let (cas_op_tx, mut cas_op_rx) = mpsc::channel(8);

    // Drain CasOps in a background task — RuntimeContentStore expects
    // someone to service them. For this test we just ack with empty
    // PutLocal responses so the engine's content_store.put doesn't
    // hang.
    tokio::spawn(async move {
        use harmony_app::content_store::CasOp;
        while let Some(op) = cas_op_rx.recv().await {
            if let CasOp::PutLocal {
                reply: Some(reply), ..
            } = op
            {
                let _ = reply.send(Ok(()));
            }
        }
    });

    let community_id = SpaceId([1u8; 16]);
    let mk = EpochKey::new([0x42; 32]);
    let admin = OwnerAddr([2u8; 16]);

    let state = Arc::new(Mutex::new(CommunityState::new(community_id)));
    let tracker = Arc::new(Mutex::new(CommunityRootHlcTracker::default()));
    let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx,
        std::time::Duration::from_millis(1000),
    ));

    let tmp = tempfile::tempdir().expect("tempdir");

    let engine = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        community_id,
        membership_key: mk,
        admin_addr: admin,
        is_invite_only: false,
        device_id: "test-device".into(),
        self_owner: admin,
        signing_key: Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0x42; 32])),
        state: Arc::clone(&state),
        tracker: Arc::clone(&tracker),
        content_store: cs,
        publisher_tx: out_tx,
        subscriber_rx: in_rx,
        paths: harmony_app::community_state_sync::PersistPaths {
            crdt: tmp.path().join("crdt.cbor"),
            replay: tmp.path().join("replay.cbor"),
        },
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        identity_resolver: None,
        error_tx: None,
        delta_tx: None,
        pending_redemptions: None,
        crdt_state: None,
        admin_identity_pub: None,
        nav_emitter: None,
        root_serve_rx: None,
    });

    engine.flush_now().await.expect("flush_now");

    // The engine should have written one wire packet to out_rx.
    let bytes = out_rx
        .recv()
        .await
        .expect("publisher_tx dropped or never sent");
    assert!(!bytes.is_empty(), "wire packet should be non-empty");

    engine.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn engine_receives_remote_publish_and_merges_event() {
    // Two-engine setup: A publishes a Join event; B receives and
    // merges. Wire the engines together via mpsc — A's out_rx is
    // forwarded to B's in_tx. ContentStore is shared between A and B
    // so B can fetch the blob A wrote.
    use std::time::Duration;

    let (a_out_tx, mut a_out_rx) = mpsc::channel::<Vec<u8>>(8);
    let (_a_in_tx, a_in_rx) = mpsc::channel::<Vec<u8>>(8);
    let (b_out_tx, _b_out_rx) = mpsc::channel::<Vec<u8>>(8);
    let (b_in_tx, b_in_rx) = mpsc::channel::<Vec<u8>>(8);
    let (cas_op_tx, mut cas_op_rx) = mpsc::channel(64);

    // Shared in-memory CAS for both engines. Spawn a CasOp servicer.
    let cas: Arc<
        tokio::sync::Mutex<std::collections::HashMap<harmony_content::cid::ContentId, Vec<u8>>>,
    > = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
    let cas_for_servicer = Arc::clone(&cas);
    tokio::spawn(async move {
        use harmony_app::content_store::CasOp;
        while let Some(op) = cas_op_rx.recv().await {
            match op {
                CasOp::PutLocal {
                    cid, blob, reply, ..
                } => {
                    cas_for_servicer.lock().await.insert(cid, blob);
                    if let Some(reply) = reply {
                        let _ = reply.send(Ok(()));
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

    // Forwarder: drain A's out_rx into B's in_tx.
    tokio::spawn(async move {
        while let Some(bytes) = a_out_rx.recv().await {
            let _ = b_in_tx.send(bytes).await;
        }
    });

    let community_id = SpaceId([1u8; 16]);
    let mk = EpochKey::new([0x42; 32]);

    let identity_a = mint_test_owner(0xA1);
    let admin = identity_a.owner;
    // ZEB-339: the publisher sig + verify_event resolve admin's signer from the
    // materialized enrolled device key (learned from admin's cert-bearing Join),
    // not the resolver's identity_pub — so it can be a placeholder.
    let identity_a_pub = [0u8; 64];
    let signing_a = identity_a.device_key.clone();

    let state_a = Arc::new(Mutex::new(CommunityState::new(community_id)));
    let state_b = Arc::new(Mutex::new(CommunityState::new(community_id)));
    let tracker_a = Arc::new(Mutex::new(CommunityRootHlcTracker::default()));
    let tracker_b = Arc::new(Mutex::new(CommunityRootHlcTracker::default()));

    let cs_a: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx.clone(),
        Duration::from_millis(2000),
    ));
    let cs_b: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx,
        Duration::from_millis(2000),
    ));

    // Pre-populate state A with one Join event by the admin so the
    // publish carries non-empty state.
    {
        let mut sa = state_a.lock().await;
        let payload = EventPayload {
            id: [9u8; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: admin,
            at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "a-dev".into(),
            },
        };
        let event = sign_event_with_identity(&payload, &identity_a).expect("sign");
        let outcome = sa.insert_event(
            event,
            &harmony_app::community_membership::VerifyContext {
                expected_community_id: community_id,
                admin_addr: admin,
                is_invite_only: false,
            },
        );
        assert!(matches!(
            outcome,
            harmony_app::community_state_crdt::InsertOutcome::Inserted
        ));
    }

    // ZEB-256: pre-seed B's CRDT with admin's Join so B's receive-side
    // membership-at-HLC gate sees admin as `Joined` when the publish
    // arrives. Without this seed, materialize() over B's empty log
    // would return None for admin, and the gate would reject with
    // PublisherNotJoined before reaching the merge path. (Production
    // bootstraps via redemption-Join; this test bypasses Phase 3 IPC.)
    {
        let mut sb = state_b.lock().await;
        let payload = EventPayload {
            id: [9u8; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: admin,
            at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "a-dev".into(),
            },
        };
        let event = sign_event_with_identity(&payload, &identity_a).expect("sign");
        let outcome = sb.insert_event(
            event,
            &harmony_app::community_membership::VerifyContext {
                expected_community_id: community_id,
                admin_addr: admin,
                is_invite_only: false,
            },
        );
        assert!(matches!(
            outcome,
            harmony_app::community_state_crdt::InsertOutcome::Inserted
        ));
    }

    let tmp_a = tempfile::tempdir().expect("tempdir a");
    let tmp_b = tempfile::tempdir().expect("tempdir b");

    let engine_a = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        community_id,
        membership_key: mk.clone(),
        admin_addr: admin,
        is_invite_only: false,
        device_id: "a-dev".into(),
        self_owner: admin,
        signing_key: Arc::new(signing_a),
        state: Arc::clone(&state_a),
        tracker: Arc::clone(&tracker_a),
        content_store: cs_a,
        publisher_tx: a_out_tx,
        subscriber_rx: a_in_rx,
        paths: harmony_app::community_state_sync::PersistPaths {
            crdt: tmp_a.path().join("crdt.cbor"),
            replay: tmp_a.path().join("replay.cbor"),
        },
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        identity_resolver: None,
        error_tx: None,
        delta_tx: None,
        pending_redemptions: None,
        crdt_state: None,
        admin_identity_pub: None,
        nav_emitter: None,
        root_serve_rx: None,
    });

    // B needs an OwnerDeviceCache-style lookup that returns
    // identity_a_pub for `admin`. Production wires Task 13's
    // `OwnerDeviceCacheResolver`; this test uses a static stub.
    let identity_resolver: Arc<dyn harmony_app::community_state_sync::IdentityResolver> =
        Arc::new(SingleIdentityResolver {
            addr: admin,
            identity_pub: identity_a_pub,
        });

    let engine_b = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        community_id,
        membership_key: mk,
        admin_addr: admin,
        is_invite_only: false,
        device_id: "b-dev".into(),
        // Engine B is a distinct local member from the admin — pick a
        // fresh OwnerAddr. B doesn't publish in this test, so the dummy
        // signing_key just needs to compile.
        self_owner: OwnerAddr([0xb1; 16]),
        signing_key: Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0x42; 32])),
        state: Arc::clone(&state_b),
        tracker: Arc::clone(&tracker_b),
        content_store: cs_b,
        publisher_tx: b_out_tx,
        subscriber_rx: b_in_rx,
        paths: harmony_app::community_state_sync::PersistPaths {
            crdt: tmp_b.path().join("crdt.cbor"),
            replay: tmp_b.path().join("replay.cbor"),
        },
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        identity_resolver: Some(identity_resolver),
        error_tx: None,
        delta_tx: None,
        pending_redemptions: None,
        crdt_state: None,
        admin_identity_pub: None,
        nav_emitter: None,
        root_serve_rx: None,
    });

    // Trigger A's publish. B's subscriber arm should fire and merge.
    // The pre-seeded admin Join already lives in B's state, so the
    // publish is admit-as-no-op (tracker advances, no new event); we
    // assert exactly one event remains in B's state.
    engine_a.flush_now().await.expect("flush_now");

    // Wait deterministically for B's tracker to advance — confirms the
    // publish made it through receive. A bounded poll keeps the test
    // from flaking on slow CI runners.
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let t = tracker_b.lock().await;
            if t.per_device.contains_key(&(admin, "a-dev".to_string())) {
                break;
            }
            drop(t);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("B should have advanced its tracker for A's publish within 2s");

    let sb = state_b.lock().await;
    assert_eq!(
        sb.event_count(),
        1,
        "B should still hold exactly one event (the pre-seeded Join, also present in A's blob)"
    );
    drop(sb);

    engine_a.shutdown().await.expect("shutdown a");
    engine_b.shutdown().await.expect("shutdown b");
}

struct SingleIdentityResolver {
    addr: OwnerAddr,
    identity_pub: [u8; 64],
}

#[async_trait::async_trait]
impl harmony_app::community_state_sync::IdentityResolver for SingleIdentityResolver {
    async fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
        if *addr == self.addr {
            Some(self.identity_pub)
        } else {
            None
        }
    }
}

#[tokio::test]
async fn engine_emits_membership_delta_on_remote_insert() {
    use harmony_app::community_state_sync::CommunityMembershipDelta;
    use std::time::Duration;

    let (a_out_tx, mut a_out_rx) = mpsc::channel::<Vec<u8>>(8);
    let (_a_in_tx, a_in_rx) = mpsc::channel::<Vec<u8>>(8);
    let (b_out_tx, _b_out_rx) = mpsc::channel::<Vec<u8>>(8);
    let (b_in_tx, b_in_rx) = mpsc::channel::<Vec<u8>>(8);
    let (cas_op_tx, mut cas_op_rx) = mpsc::channel(64);
    let (delta_tx, mut delta_rx) = mpsc::channel::<CommunityMembershipDelta>(8);

    let cas: Arc<
        tokio::sync::Mutex<std::collections::HashMap<harmony_content::cid::ContentId, Vec<u8>>>,
    > = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
    let cas_for_servicer = Arc::clone(&cas);
    tokio::spawn(async move {
        use harmony_app::content_store::CasOp;
        while let Some(op) = cas_op_rx.recv().await {
            match op {
                CasOp::PutLocal {
                    cid, blob, reply, ..
                } => {
                    cas_for_servicer.lock().await.insert(cid, blob);
                    if let Some(reply) = reply {
                        let _ = reply.send(Ok(()));
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
    tokio::spawn(async move {
        while let Some(bytes) = a_out_rx.recv().await {
            let _ = b_in_tx.send(bytes).await;
        }
    });

    let community_id = SpaceId([1u8; 16]);
    let mk = EpochKey::new([0x42; 32]);
    let identity_a = mint_test_owner(0xA1);
    let admin = identity_a.owner;
    // ZEB-339: the publisher sig + verify_event resolve admin's signer from the
    // materialized enrolled device key (learned from admin's cert-bearing Join),
    // not the resolver's identity_pub — so it can be a placeholder.
    let identity_a_pub = [0u8; 64];
    let signing_a = identity_a.device_key.clone();

    let state_a = Arc::new(Mutex::new(CommunityState::new(community_id)));
    let state_b = Arc::new(Mutex::new(CommunityState::new(community_id)));
    let tracker_a = Arc::new(Mutex::new(CommunityRootHlcTracker::default()));
    let tracker_b = Arc::new(Mutex::new(CommunityRootHlcTracker::default()));

    let cs_a: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx.clone(),
        Duration::from_millis(2000),
    ));
    let cs_b: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx,
        Duration::from_millis(2000),
    ));

    let admin_join_event = {
        let payload = EventPayload {
            id: [9u8; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: admin,
            at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "a-dev".into(),
            },
        };
        sign_event_with_identity(&payload, &identity_a).expect("sign")
    };
    // A's state holds admin's Join + a SetPower so the publish
    // carries a strictly-larger event log than B's pre-seed. Without
    // a NEW event for B to merge, no delta would fire.
    let setpower_event = {
        let payload = EventPayload {
            id: [10u8; 16],
            community_id,
            kind: MembershipEventKind::SetPower {
                target: admin,
                level: 50,
            },
            actor: admin,
            at: Hlc {
                wall_ms: 200,
                logical: 0,
                device_id: "a-dev".into(),
            },
        };
        sign_event_with_identity(&payload, &identity_a).expect("sign")
    };
    {
        let mut sa = state_a.lock().await;
        let outcome1 = sa.insert_event(
            admin_join_event.clone(),
            &harmony_app::community_membership::VerifyContext {
                expected_community_id: community_id,
                admin_addr: admin,
                is_invite_only: false,
            },
        );
        assert!(
            matches!(
                outcome1,
                harmony_app::community_state_crdt::InsertOutcome::Inserted
            ),
            "fixture admin Join must succeed; got {outcome1:?}"
        );
        let outcome2 = sa.insert_event(
            setpower_event.clone(),
            &harmony_app::community_membership::VerifyContext {
                expected_community_id: community_id,
                admin_addr: admin,
                is_invite_only: false,
            },
        );
        assert!(
            matches!(
                outcome2,
                harmony_app::community_state_crdt::InsertOutcome::Inserted
            ),
            "fixture SetPower must succeed; got {outcome2:?}"
        );
    }

    // ZEB-256: pre-seed B's CRDT with admin's Join so B's receive-side
    // membership-at-HLC gate sees admin as `Joined` when the publish
    // arrives. The SetPower event will arrive new, triggering the delta.
    {
        let mut sb = state_b.lock().await;
        let outcome = sb.insert_event(
            admin_join_event,
            &harmony_app::community_membership::VerifyContext {
                expected_community_id: community_id,
                admin_addr: admin,
                is_invite_only: false,
            },
        );
        assert!(matches!(
            outcome,
            harmony_app::community_state_crdt::InsertOutcome::Inserted
        ));
    }

    let tmp_a = tempfile::tempdir().expect("tempdir a");
    let tmp_b = tempfile::tempdir().expect("tempdir b");

    let engine_a = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        community_id,
        membership_key: mk.clone(),
        admin_addr: admin,
        is_invite_only: false,
        device_id: "a-dev".into(),
        self_owner: admin,
        signing_key: Arc::new(signing_a),
        state: Arc::clone(&state_a),
        tracker: Arc::clone(&tracker_a),
        content_store: cs_a,
        publisher_tx: a_out_tx,
        subscriber_rx: a_in_rx,
        paths: harmony_app::community_state_sync::PersistPaths {
            crdt: tmp_a.path().join("crdt.cbor"),
            replay: tmp_a.path().join("replay.cbor"),
        },
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        identity_resolver: None,
        error_tx: None,
        delta_tx: None,
        pending_redemptions: None,
        crdt_state: None,
        admin_identity_pub: None,
        nav_emitter: None,
        root_serve_rx: None,
    });

    struct SingleIdentityResolver {
        addr: OwnerAddr,
        identity_pub: [u8; 64],
    }
    #[async_trait::async_trait]
    impl harmony_app::community_state_sync::IdentityResolver for SingleIdentityResolver {
        async fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
            if *addr == self.addr {
                Some(self.identity_pub)
            } else {
                None
            }
        }
    }
    let resolver: Arc<dyn harmony_app::community_state_sync::IdentityResolver> =
        Arc::new(SingleIdentityResolver {
            addr: admin,
            identity_pub: identity_a_pub,
        });

    let engine_b = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        community_id,
        membership_key: mk,
        admin_addr: admin,
        is_invite_only: false,
        device_id: "b-dev".into(),
        // Engine B is a distinct local member from the admin — pick a
        // fresh OwnerAddr. B doesn't publish in this test, so the dummy
        // signing_key just needs to compile.
        self_owner: OwnerAddr([0xb1; 16]),
        signing_key: Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0x42; 32])),
        state: Arc::clone(&state_b),
        tracker: Arc::clone(&tracker_b),
        content_store: cs_b,
        publisher_tx: b_out_tx,
        subscriber_rx: b_in_rx,
        paths: harmony_app::community_state_sync::PersistPaths {
            crdt: tmp_b.path().join("crdt.cbor"),
            replay: tmp_b.path().join("replay.cbor"),
        },
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        identity_resolver: Some(resolver),
        error_tx: None,
        delta_tx: Some(delta_tx),
        pending_redemptions: None,
        crdt_state: None,
        admin_identity_pub: None,
        nav_emitter: None,
        root_serve_rx: None,
    });

    engine_a.flush_now().await.expect("flush_now");

    // B already holds admin's Join (pre-seed); the SetPower is the
    // novel event that should drive a delta.
    let delta = tokio::time::timeout(Duration::from_secs(2), delta_rx.recv())
        .await
        .expect("delta should arrive within 2s")
        .expect("delta channel should be open");
    assert_eq!(delta.community_id, community_id);
    assert_eq!(delta.event.actor, admin);
    assert!(matches!(
        delta.event.kind,
        MembershipEventKind::SetPower { .. }
    ));

    engine_a.shutdown().await.expect("shutdown a");
    engine_b.shutdown().await.expect("shutdown b");
}

#[tokio::test]
async fn engine_insert_local_event_emits_delta_and_notifies_publish() {
    use harmony_app::community_state_sync::CommunityMembershipDelta;
    use std::time::Duration;

    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(8);
    let (_in_tx, in_rx) = mpsc::channel::<Vec<u8>>(8);
    let (cas_op_tx, mut cas_op_rx) = mpsc::channel(8);
    let (delta_tx, mut delta_rx) = mpsc::channel::<CommunityMembershipDelta>(8);

    tokio::spawn(async move {
        use harmony_app::content_store::CasOp;
        while let Some(op) = cas_op_rx.recv().await {
            if let CasOp::PutLocal {
                reply: Some(reply), ..
            } = op
            {
                let _ = reply.send(Ok(()));
            }
        }
    });

    let community_id = SpaceId([2u8; 16]);
    let mk = EpochKey::new([0x33; 32]);
    let identity = mint_test_owner(0xC1);
    let admin = identity.owner;
    // ZEB-339: signer resolution uses the cert / materialized enrolled key, not
    // the resolver — identity_pub is a placeholder.
    let identity_pub = [0u8; 64];

    let state = Arc::new(Mutex::new(CommunityState::new(community_id)));
    let tracker = Arc::new(Mutex::new(CommunityRootHlcTracker::default()));
    let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx,
        Duration::from_millis(1000),
    ));
    let tmp = tempfile::tempdir().expect("tempdir");

    struct StaticResolver {
        addr: OwnerAddr,
        identity_pub: [u8; 64],
    }
    #[async_trait::async_trait]
    impl harmony_app::community_state_sync::IdentityResolver for StaticResolver {
        async fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
            if *addr == self.addr {
                Some(self.identity_pub)
            } else {
                None
            }
        }
    }
    let resolver: Arc<dyn harmony_app::community_state_sync::IdentityResolver> =
        Arc::new(StaticResolver {
            addr: admin,
            identity_pub,
        });

    let engine = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        community_id,
        membership_key: mk,
        admin_addr: admin,
        is_invite_only: false,
        device_id: "local-dev".into(),
        self_owner: admin,
        signing_key: Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0x42; 32])),
        state: Arc::clone(&state),
        tracker: Arc::clone(&tracker),
        content_store: cs,
        publisher_tx: out_tx,
        subscriber_rx: in_rx,
        paths: harmony_app::community_state_sync::PersistPaths {
            crdt: tmp.path().join("crdt.cbor"),
            replay: tmp.path().join("replay.cbor"),
        },
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        identity_resolver: Some(resolver),
        error_tx: None,
        delta_tx: Some(delta_tx),
        pending_redemptions: None,
        crdt_state: None,
        admin_identity_pub: None,
        nav_emitter: None,
        root_serve_rx: None,
    });

    let payload = EventPayload {
        id: [7u8; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: admin,
        at: Hlc {
            wall_ms: 1000,
            logical: 0,
            device_id: "local-dev".into(),
        },
    };
    let event = sign_event_with_identity(&payload, &identity).expect("sign");

    let outcome = engine
        .insert_local_event(event.clone())
        .await
        .expect("insert_local_event should succeed");
    assert_eq!(
        outcome,
        harmony_app::community_state_crdt::InsertOutcome::Inserted
    );

    let delta = tokio::time::timeout(Duration::from_secs(1), delta_rx.recv())
        .await
        .expect("delta within 1s")
        .expect("delta channel open");
    assert_eq!(delta.event.id, event.id);

    let _bytes = tokio::time::timeout(Duration::from_secs(2), out_rx.recv())
        .await
        .expect("publish within 2s")
        .expect("publisher channel open");

    let outcome2 = engine.insert_local_event(event).await.expect("idempotent");
    assert_eq!(
        outcome2,
        harmony_app::community_state_crdt::InsertOutcome::AlreadyKnown
    );
    let none_delta = tokio::time::timeout(Duration::from_millis(200), delta_rx.recv()).await;
    assert!(none_delta.is_err(), "AlreadyKnown must not emit a delta");

    let bad_payload = EventPayload {
        id: [8u8; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: OwnerAddr([0xff; 16]),
        at: Hlc {
            wall_ms: 2000,
            logical: 0,
            device_id: "local-dev".into(),
        },
    };
    let bad_event = sign_event_with_identity(&bad_payload, &identity).expect("sign");
    let result = engine.insert_local_event(bad_event).await;
    // `insert_local_event` routes straight to the CRDT layer's
    // `insert_event`, so a bogus actor (one that can't be verified
    // against the materialized membership) surfaces as
    // `Ok(InsertOutcome::Rejected(_))` rather than a pre-insert `Err`.
    // The matcher preserves the test's intent ("a bogus actor must not
    // insert").
    assert!(matches!(
        result,
        Ok(harmony_app::community_state_crdt::InsertOutcome::Rejected(
            _
        ))
    ));

    engine.shutdown().await.expect("shutdown");
}

#[test]
fn classify_incoming_error_covers_publisher_auth_variants() {
    use harmony_app::community_membership::MemberStatus;
    use harmony_app::community_state_sync::CommunitySyncError;
    use harmony_app::owner_state_types::OwnerAddr;

    // Each variant has a distinct, stable reason_tag — these strings
    // are the contract with the frontend banner copy.
    let alice = OwnerAddr([0xA1; 16]);
    let cases = [
        (
            CommunitySyncError::PublisherNotJoined {
                addr: alice,
                status: MemberStatus::Banned,
                left_at: None,
            },
            "publisher_not_joined",
        ),
        (
            CommunitySyncError::PublisherSigInvalid { addr: alice },
            "publisher_sig_invalid",
        ),
    ];
    for (err, expected_tag) in cases {
        let actual_tag = harmony_app::community_state_sync::classify_incoming_error_for_test(&err);
        assert_eq!(
            actual_tag, expected_tag,
            "reason_tag for {err:?} must be {expected_tag}"
        );
    }
}

#[tokio::test]
async fn engine_accepts_self_owner_and_signing_key_in_config() {
    let (out_tx, _out_rx) = mpsc::channel::<Vec<u8>>(8);
    let (_in_tx, in_rx) = mpsc::channel::<Vec<u8>>(8);
    let (cas_op_tx, _cas_op_rx) = mpsc::channel(8);

    let community_id = SpaceId([1u8; 16]);
    let identity = PrivateIdentity::from_seed(&[0xa1; 32]);
    let self_owner = OwnerAddr(identity.identity.address_hash);
    let signing_key = Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0x42; 32]));

    let state = Arc::new(Mutex::new(CommunityState::new(community_id)));
    let tracker = Arc::new(Mutex::new(CommunityRootHlcTracker::default()));
    let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx,
        std::time::Duration::from_millis(1000),
    ));
    let tmp = tempfile::tempdir().expect("tempdir");

    let engine = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        community_id,
        membership_key: EpochKey::new([0x42; 32]),
        admin_addr: self_owner,
        is_invite_only: false,
        device_id: "test-device".into(),
        self_owner,
        signing_key,
        state,
        tracker,
        content_store: cs,
        publisher_tx: out_tx,
        subscriber_rx: in_rx,
        paths: harmony_app::community_state_sync::PersistPaths {
            crdt: tmp.path().join("crdt.cbor"),
            replay: tmp.path().join("replay.cbor"),
        },
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        identity_resolver: None,
        error_tx: None,
        delta_tx: None,
        pending_redemptions: None,
        crdt_state: None,
        admin_identity_pub: None,
        nav_emitter: None,
        root_serve_rx: None,
    });
    engine.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn publish_carries_valid_publisher_sig() {
    use ed25519_dalek::Verifier;
    use harmony_app::community_state_sync::{
        decrypt_root_publish, CommunityRootHlcTracker, CommunityRootPublishPayload,
        CommunityRootSignedPayload, CommunitySyncEngine, CommunitySyncEngineConfig,
        DEFAULT_DEBOUNCE_MS,
    };
    use harmony_app::content_store::{ContentStore, RuntimeContentStore};
    use harmony_app::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
    use harmony_app::owner_state_types::{EpochKey, OwnerAddr, SpaceId};

    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let (_in_tx, in_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let (cas_op_tx, mut cas_op_rx) = tokio::sync::mpsc::channel(8);

    tokio::spawn(async move {
        use harmony_app::content_store::CasOp;
        while let Some(op) = cas_op_rx.recv().await {
            if let CasOp::PutLocal {
                reply: Some(reply), ..
            } = op
            {
                let _ = reply.send(Ok(()));
            }
        }
    });

    let community_id = SpaceId([1u8; 16]);
    let mk = EpochKey::new([0x42; 32]);
    let signing_key = std::sync::Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0xAB; 32]));
    let verifying_key = signing_key.verifying_key();
    // self_owner is just an opaque tag here; the test verifies sig
    // against verifying_key directly without going through resolver.
    let self_owner = OwnerAddr([0x12; 16]);
    let admin = self_owner;

    let state = std::sync::Arc::new(tokio::sync::Mutex::new(
        harmony_app::community_state_crdt::CommunityState::new(community_id),
    ));
    let tracker = std::sync::Arc::new(tokio::sync::Mutex::new(CommunityRootHlcTracker::default()));
    let cs: std::sync::Arc<dyn ContentStore> = std::sync::Arc::new(RuntimeContentStore::new(
        cas_op_tx,
        std::time::Duration::from_millis(1000),
    ));
    let tmp = tempfile::tempdir().expect("tempdir");

    let engine = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        community_id,
        membership_key: mk.clone(),
        admin_addr: admin,
        is_invite_only: false,
        device_id: "pub-dev".into(),
        self_owner,
        signing_key,
        state,
        tracker,
        content_store: cs,
        publisher_tx: out_tx,
        subscriber_rx: in_rx,
        paths: harmony_app::community_state_sync::PersistPaths {
            crdt: tmp.path().join("crdt.cbor"),
            replay: tmp.path().join("replay.cbor"),
        },
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        identity_resolver: None,
        error_tx: None,
        delta_tx: None,
        pending_redemptions: None,
        crdt_state: None,
        admin_identity_pub: None,
        nav_emitter: None,
        root_serve_rx: None,
    });

    engine.flush_now().await.expect("flush_now");

    let wire = out_rx
        .recv()
        .await
        .expect("publisher_tx must have received one wire packet");
    let payload_bytes = decrypt_root_publish(&mk, &wire).expect("decrypt");
    let payload: CommunityRootPublishPayload =
        canonical_cbor_decode(&payload_bytes).expect("decode envelope");

    // The wire envelope's publisher_addr matches self_owner.
    assert_eq!(payload.publisher_addr, self_owner);

    // The publisher_sig validates against the verifying_key for the
    // canonical CBOR of CommunityRootSignedPayload::from(&payload).
    let signed = CommunityRootSignedPayload::from(&payload);
    let signed_bytes = canonical_cbor_encode(&signed).expect("encode signed");
    let sig = ed25519_dalek::Signature::from_bytes(&payload.publisher_sig);
    verifying_key
        .verify(&signed_bytes, &sig)
        .expect("publisher_sig must verify against signing_key.verifying_key()");

    engine.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------
// ZEB-256 Task 6: receive-side verify gates
//
// Three rejection paths in `handle_incoming_publish`, each a load-
// bearing defense:
//
//   1. PublisherSigInvalid — Bob signs an envelope claiming Alice as
//      publisher_addr. Resolver returns Alice's identity_pub; sig
//      doesn't validate. Tracker NOT advanced.
//   2. PublisherNotJoined — Alice was Joined then Kicked. Her own
//      validly-signed publish at HLC > kick HLC fails the membership-
//      at-HLC gate. Tracker NOT advanced.
//   3. Cold cache — publisher not yet materialized as a member. Same
//      envelope re-delivered after the publisher's Join propagates
//      → admit + tracker advances.
//
// Each test asserts BOTH the reason_tag AND that the tracker did not
// advance — the censorship-defense invariant. A regression that
// silently advanced the tracker would let a kicked-but-still-keyed
// member squat HLC slots even with the per-addr namespacing.
// ---------------------------------------------------------------------

#[tokio::test]
async fn spoofed_publisher_addr_rejected_with_publisher_sig_invalid() {
    use ed25519_dalek::Signer;
    use harmony_app::community_state_sync::{encrypt_blob, encrypt_root_publish};
    use harmony_app::community_state_sync::{CommunityDegradedReport, CommunityRootSignedPayload};
    use harmony_app::owner_state_crypto::canonical_cbor_encode;

    let (out_tx, _out_rx) = mpsc::channel::<Vec<u8>>(8);
    let (in_tx, in_rx) = mpsc::channel::<Vec<u8>>(8);
    let (cas_op_tx, mut cas_op_rx) = mpsc::channel(64);
    let (degraded_tx, mut degraded_rx) = mpsc::channel::<CommunityDegradedReport>(8);

    let cas: Arc<
        tokio::sync::Mutex<std::collections::HashMap<harmony_content::cid::ContentId, Vec<u8>>>,
    > = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
    let cas_for_servicer = Arc::clone(&cas);
    tokio::spawn(async move {
        use harmony_app::content_store::CasOp;
        while let Some(op) = cas_op_rx.recv().await {
            match op {
                CasOp::PutLocal {
                    cid, blob, reply, ..
                } => {
                    cas_for_servicer.lock().await.insert(cid, blob);
                    if let Some(reply) = reply {
                        let _ = reply.send(Ok(()));
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

    let community_id = SpaceId([7u8; 16]);
    let mk = EpochKey::new([0xAA; 32]);

    let alice = mint_test_owner(0xA1);
    let alice_addr = alice.owner;
    let alice_pub = [0u8; 64];

    let bob = mint_test_owner(0xB1);
    let bob_addr = bob.owner;
    let bob_pub = [0u8; 64];
    // ZEB-339: Bob signs the forged envelope with HIS enrolled device key. The
    // receiver resolves alice's enrolled device key (from her materialized
    // cert-bearing Join) and bob's sig fails against it → PublisherSigInvalid.
    let bob_signing = bob.device_key.clone();

    // Build a CommunityState where Alice is Joined (admin self-Join).
    let mut alice_state = CommunityState::new(community_id);
    {
        let payload = EventPayload {
            id: [1u8; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: alice_addr,
            at: Hlc {
                wall_ms: 1000,
                logical: 0,
                device_id: "alice-dev".into(),
            },
        };
        let event = sign_event_with_identity(&payload, &alice).expect("sign");
        let outcome = alice_state.insert_event(
            event,
            &harmony_app::community_membership::VerifyContext {
                expected_community_id: community_id,
                admin_addr: alice_addr,
                is_invite_only: false,
            },
        );
        assert_eq!(
            outcome,
            harmony_app::community_state_crdt::InsertOutcome::Inserted
        );
    }

    // Encrypt Alice's state into the CAS so the receiver can fetch.
    let blob_cleartext = canonical_cbor_encode(&alice_state).expect("encode state");
    let blob_ciphertext = encrypt_blob(&mk, &blob_cleartext).expect("encrypt blob");
    let root_cid = harmony_content::cid::ContentId::for_book(
        &blob_ciphertext,
        harmony_content::cid::ContentFlags {
            encrypted: true,
            ..Default::default()
        },
    )
    .expect("cid");
    cas.lock().await.insert(root_cid, blob_ciphertext);

    // Build a forged envelope: publisher_addr = alice (so resolver
    // hands back alice_pub), but signed with Bob's key (so the sig
    // doesn't validate against alice_pub).
    let signed = CommunityRootSignedPayload {
        root_cid,
        publisher_addr: alice_addr,
        at: Hlc {
            wall_ms: 2000,
            logical: 0,
            device_id: "alice-dev".into(),
        },
    };
    let signed_bytes = canonical_cbor_encode(&signed).expect("encode signed");
    let bad_sig = bob_signing.sign(&signed_bytes).to_bytes();
    let envelope = signed.into_wire(bad_sig, None);
    let envelope_bytes = canonical_cbor_encode(&envelope).expect("encode envelope");
    let wire = encrypt_root_publish(&mk, &envelope_bytes).expect("encrypt root");

    // Engine_b — receiver. Uses MapResolver that knows both alice
    // and bob, so resolve(alice) returns alice_pub (used to verify
    // the sig, which fails because Bob signed it).
    let mut entries = std::collections::HashMap::new();
    entries.insert(alice_addr, alice_pub);
    entries.insert(bob_addr, bob_pub);
    let resolver: Arc<dyn harmony_app::community_state_sync::IdentityResolver> =
        Arc::new(MapResolver { entries });

    // B's CRDT must already see alice as Joined for the membership-
    // at-HLC gate to admit the publish into the sig-verify check; we
    // pre-seed it with the same Join event Alice's state holds. (The
    // gate runs BEFORE sig-verify per cheapest-first ordering.)
    let state_b = Arc::new(Mutex::new(alice_state.clone()));
    let tracker_b = Arc::new(Mutex::new(CommunityRootHlcTracker::default()));
    let cs_b: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx,
        std::time::Duration::from_millis(2000),
    ));
    let tmp_b = tempfile::tempdir().expect("tempdir b");

    let engine_b = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        community_id,
        membership_key: mk,
        admin_addr: alice_addr,
        is_invite_only: false,
        device_id: "b-dev".into(),
        self_owner: bob_addr,
        signing_key: Arc::new(bob.device_key.clone()),
        state: Arc::clone(&state_b),
        tracker: Arc::clone(&tracker_b),
        content_store: cs_b,
        publisher_tx: out_tx,
        subscriber_rx: in_rx,
        paths: harmony_app::community_state_sync::PersistPaths {
            crdt: tmp_b.path().join("crdt.cbor"),
            replay: tmp_b.path().join("replay.cbor"),
        },
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        identity_resolver: Some(resolver),
        error_tx: Some(degraded_tx),
        delta_tx: None,
        pending_redemptions: None,
        crdt_state: None,
        admin_identity_pub: None,
        nav_emitter: None,
        root_serve_rx: None,
    });

    in_tx.send(wire).await.expect("inject wire");

    // Wait for the rejection report.
    let report = tokio::time::timeout(std::time::Duration::from_secs(2), degraded_rx.recv())
        .await
        .expect("degraded report within 2s")
        .expect("degraded channel still open");
    assert_eq!(report.reason_tag, "publisher_sig_invalid");

    // Tracker NOT advanced for alice's slot.
    let t = tracker_b.lock().await;
    assert!(
        !t.per_device
            .contains_key(&(alice_addr, "alice-dev".to_string())),
        "tracker MUST NOT have advanced on sig-invalid rejection"
    );
    drop(t);

    engine_b.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn kicked_member_publish_rejected_with_publisher_not_joined() {
    use ed25519_dalek::Signer;
    use harmony_app::community_state_sync::{encrypt_blob, encrypt_root_publish};
    use harmony_app::community_state_sync::{CommunityDegradedReport, CommunityRootSignedPayload};
    use harmony_app::owner_state_crypto::canonical_cbor_encode;

    let (out_tx, _out_rx) = mpsc::channel::<Vec<u8>>(8);
    let (in_tx, in_rx) = mpsc::channel::<Vec<u8>>(8);
    let (cas_op_tx, mut cas_op_rx) = mpsc::channel(64);
    let (degraded_tx, mut degraded_rx) = mpsc::channel::<CommunityDegradedReport>(8);
    let cas: Arc<
        tokio::sync::Mutex<std::collections::HashMap<harmony_content::cid::ContentId, Vec<u8>>>,
    > = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
    let cas_for_servicer = Arc::clone(&cas);
    tokio::spawn(async move {
        use harmony_app::content_store::CasOp;
        while let Some(op) = cas_op_rx.recv().await {
            match op {
                CasOp::PutLocal {
                    cid, blob, reply, ..
                } => {
                    cas_for_servicer.lock().await.insert(cid, blob);
                    if let Some(reply) = reply {
                        let _ = reply.send(Ok(()));
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

    let community_id = SpaceId([8u8; 16]);
    let mk = EpochKey::new([0xCC; 32]);

    let admin_id = mint_test_owner(0xA0);
    let admin_addr = admin_id.owner;
    let admin_pub = [0u8; 64];
    let admin_signing = admin_id.device_key.clone();

    let alice = mint_test_owner(0xA1);
    let alice_addr = alice.owner;
    let alice_pub = [0u8; 64];
    let alice_signing = alice.device_key.clone();

    // Build a CommunityState where:
    //   - admin is Joined
    //   - alice was Joined then Kicked at HLC 100
    let mut state = CommunityState::new(community_id);
    let push_event = |state: &mut CommunityState,
                      actor: OwnerAddr,
                      actor_id: &TestOwner,
                      _actor_pub: &[u8; 64],
                      kind: MembershipEventKind,
                      dev: &str,
                      wall: u64,
                      eid: [u8; 16]| {
        let p = EventPayload {
            id: eid,
            community_id,
            kind,
            actor,
            at: Hlc {
                wall_ms: wall,
                logical: 0,
                device_id: dev.into(),
            },
        };
        let ev = sign_event_with_identity(&p, actor_id).expect("sign");
        let outcome = state.insert_event(
            ev,
            &harmony_app::community_membership::VerifyContext {
                expected_community_id: community_id,
                admin_addr,
                is_invite_only: false,
            },
        );
        assert_eq!(
            outcome,
            harmony_app::community_state_crdt::InsertOutcome::Inserted,
            "fixture insert must succeed"
        );
    };
    push_event(
        &mut state,
        admin_addr,
        &admin_id,
        &admin_pub,
        MembershipEventKind::Join,
        "admin-dev",
        10,
        [1u8; 16],
    );
    push_event(
        &mut state,
        alice_addr,
        &alice,
        &alice_pub,
        MembershipEventKind::Join,
        "alice-dev",
        20,
        [2u8; 16],
    );
    // Admin kicks alice at HLC 100.
    push_event(
        &mut state,
        admin_addr,
        &admin_id,
        &admin_pub,
        MembershipEventKind::Kick {
            target: alice_addr,
            reason: None,
        },
        "admin-dev",
        100,
        [3u8; 16],
    );

    // Encrypt + put. The blob's actual contents don't reach the merge
    // path because the membership-at-HLC gate rejects on `publisher_addr`
    // before sig-verify or blob fetch.
    let blob_cleartext = canonical_cbor_encode(&state).expect("encode state");
    let blob_ciphertext = encrypt_blob(&mk, &blob_cleartext).expect("encrypt blob");
    let root_cid = harmony_content::cid::ContentId::for_book(
        &blob_ciphertext,
        harmony_content::cid::ContentFlags {
            encrypted: true,
            ..Default::default()
        },
    )
    .expect("cid");
    cas.lock().await.insert(root_cid, blob_ciphertext);
    // Suppress unused-variable warning for `admin_signing`; the kick
    // attack only needs alice's signing key.
    let _ = admin_signing;

    // Alice publishes at HLC 150 — AFTER her kick. Her sig is valid
    // (she still has her signing key); the membership-at-HLC gate
    // rejects.
    let signed = CommunityRootSignedPayload {
        root_cid,
        publisher_addr: alice_addr,
        at: Hlc {
            wall_ms: 150,
            logical: 0,
            device_id: "alice-dev".into(),
        },
    };
    let signed_bytes = canonical_cbor_encode(&signed).expect("encode signed");
    let valid_sig = alice_signing.sign(&signed_bytes).to_bytes();
    let envelope = signed.into_wire(valid_sig, None);
    let envelope_bytes = canonical_cbor_encode(&envelope).expect("encode env");
    let wire = encrypt_root_publish(&mk, &envelope_bytes).expect("encrypt root");

    let mut entries = std::collections::HashMap::new();
    entries.insert(admin_addr, admin_pub);
    entries.insert(alice_addr, alice_pub);
    let resolver: Arc<dyn harmony_app::community_state_sync::IdentityResolver> =
        Arc::new(MapResolver { entries });

    let state_b = Arc::new(Mutex::new(state.clone()));
    let tracker_b = Arc::new(Mutex::new(CommunityRootHlcTracker::default()));
    let cs_b: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx,
        std::time::Duration::from_millis(2000),
    ));
    let tmp_b = tempfile::tempdir().expect("tempdir b");

    let engine_b = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        community_id,
        membership_key: mk,
        admin_addr,
        is_invite_only: false,
        device_id: "b-dev".into(),
        self_owner: admin_addr,
        signing_key: Arc::new(admin_id.device_key.clone()),
        state: Arc::clone(&state_b),
        tracker: Arc::clone(&tracker_b),
        content_store: cs_b,
        publisher_tx: out_tx,
        subscriber_rx: in_rx,
        paths: harmony_app::community_state_sync::PersistPaths {
            crdt: tmp_b.path().join("crdt.cbor"),
            replay: tmp_b.path().join("replay.cbor"),
        },
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        identity_resolver: Some(resolver),
        error_tx: Some(degraded_tx),
        delta_tx: None,
        pending_redemptions: None,
        crdt_state: None,
        admin_identity_pub: None,
        nav_emitter: None,
        root_serve_rx: None,
    });

    in_tx.send(wire).await.expect("inject wire");

    let report = tokio::time::timeout(std::time::Duration::from_secs(2), degraded_rx.recv())
        .await
        .expect("degraded report within 2s")
        .expect("degraded channel still open");
    assert_eq!(report.reason_tag, "publisher_not_joined");

    let t = tracker_b.lock().await;
    assert!(
        !t.per_device
            .contains_key(&(alice_addr, "alice-dev".to_string())),
        "tracker MUST NOT have advanced on PublisherNotJoined"
    );
    drop(t);

    engine_b.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn invite_only_cold_cache_publish_rejected_then_succeeds_after_propagation() {
    // ZEB-558: this exercises the INVITE-ONLY cold-cache reject→propagate→
    // succeed path, which the open-community gate relaxation deliberately
    // leaves unchanged. (For OPEN communities a cold-cache publish carrying the
    // publisher's self-Join now self-admits on first delivery — covered by
    // `open_community_two_node_wire_convergence_no_preseed` + the
    // `bootstrap_admit_open_publisher` unit tests. Keeping this test OPEN would
    // make alice's in-blob self-Join self-admit and there would be no degraded
    // report to assert on.)
    //
    // The same envelope is delivered twice. First time alice is not yet
    // materialized as a member → PublisherNotJoined rejection. Then her
    // cert-bearing Join propagates into the receiver's CRDT and we
    // re-deliver — the engine accepts.
    use ed25519_dalek::Signer;
    use harmony_app::community_state_sync::{encrypt_blob, encrypt_root_publish};
    use harmony_app::community_state_sync::{CommunityDegradedReport, CommunityRootSignedPayload};
    use harmony_app::owner_state_crypto::canonical_cbor_encode;

    // Mutable resolver — entries can be inserted at runtime.
    struct MutableResolver {
        inner: tokio::sync::Mutex<std::collections::HashMap<OwnerAddr, [u8; 64]>>,
    }
    #[async_trait::async_trait]
    impl harmony_app::community_state_sync::IdentityResolver for MutableResolver {
        async fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
            self.inner.lock().await.get(addr).copied()
        }
    }

    let (out_tx, _out_rx) = mpsc::channel::<Vec<u8>>(8);
    let (in_tx, in_rx) = mpsc::channel::<Vec<u8>>(8);
    let (cas_op_tx, mut cas_op_rx) = mpsc::channel(64);
    let (degraded_tx, mut degraded_rx) = mpsc::channel::<CommunityDegradedReport>(8);
    let cas: Arc<
        tokio::sync::Mutex<std::collections::HashMap<harmony_content::cid::ContentId, Vec<u8>>>,
    > = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
    let cas_for_servicer = Arc::clone(&cas);
    tokio::spawn(async move {
        use harmony_app::content_store::CasOp;
        while let Some(op) = cas_op_rx.recv().await {
            match op {
                CasOp::PutLocal {
                    cid, blob, reply, ..
                } => {
                    cas_for_servicer.lock().await.insert(cid, blob);
                    if let Some(reply) = reply {
                        let _ = reply.send(Ok(()));
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

    let community_id = SpaceId([9u8; 16]);
    let mk = EpochKey::new([0xDD; 32]);

    let alice = mint_test_owner(0xA1);
    let alice_addr = alice.owner;
    let alice_pub = [0u8; 64];
    let alice_signing = alice.device_key.clone();

    // ZEB-339: the publisher is authenticated against their MATERIALIZED
    // enrolled device key (from their cert-bearing Join), NOT a resolver lookup.
    // So the "cold cache" under the new model is: alice's cert-bearing Join has
    // not yet propagated into the RECEIVER's CRDT — she isn't materialized, so
    // her publish is rejected with PublisherNotJoined. Once her Join lands in
    // the receiver's state (propagation), the same wire packet admits.
    //
    // Build alice's cert-bearing Join (also embedded in the published blob).
    let alice_join = {
        let p = EventPayload {
            id: [1u8; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: alice_addr,
            at: Hlc {
                wall_ms: 10,
                logical: 0,
                device_id: "alice-dev".into(),
            },
        };
        sign_event_with_identity(&p, &alice).expect("sign")
    };
    let mut state = CommunityState::new(community_id);
    {
        let outcome = state.insert_event(
            alice_join.clone(),
            &harmony_app::community_membership::VerifyContext {
                expected_community_id: community_id,
                admin_addr: alice_addr,
                is_invite_only: true,
            },
        );
        assert_eq!(
            outcome,
            harmony_app::community_state_crdt::InsertOutcome::Inserted
        );
    }

    let blob_cleartext = canonical_cbor_encode(&state).expect("encode");
    let blob_ciphertext = encrypt_blob(&mk, &blob_cleartext).expect("encrypt blob");
    let root_cid = harmony_content::cid::ContentId::for_book(
        &blob_ciphertext,
        harmony_content::cid::ContentFlags {
            encrypted: true,
            ..Default::default()
        },
    )
    .expect("cid");
    cas.lock().await.insert(root_cid, blob_ciphertext);

    let signed = CommunityRootSignedPayload {
        root_cid,
        publisher_addr: alice_addr,
        at: Hlc {
            wall_ms: 200,
            logical: 0,
            device_id: "alice-dev".into(),
        },
    };
    let signed_bytes = canonical_cbor_encode(&signed).expect("encode signed");
    let sig = alice_signing.sign(&signed_bytes).to_bytes();
    let envelope = signed.into_wire(sig, None);
    let envelope_bytes = canonical_cbor_encode(&envelope).expect("encode env");
    let wire = encrypt_root_publish(&mk, &envelope_bytes).expect("encrypt root");

    // ZEB-339: the resolver is no longer consulted on the publish path; supply
    // an always-empty one to prove the gate doesn't depend on it.
    let resolver = Arc::new(MutableResolver {
        inner: tokio::sync::Mutex::new(std::collections::HashMap::new()),
    });
    let resolver_for_engine: Arc<dyn harmony_app::community_state_sync::IdentityResolver> =
        Arc::clone(&resolver) as _;

    // B's CRDT starts EMPTY — alice's cert-bearing Join hasn't propagated yet,
    // so she is not a materialized member and her publish is rejected with
    // PublisherNotJoined (the enrolled-device cold-cache shape).
    let state_b = Arc::new(Mutex::new(CommunityState::new(community_id)));
    let tracker_b = Arc::new(Mutex::new(CommunityRootHlcTracker::default()));
    let cs_b: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx,
        std::time::Duration::from_millis(2000),
    ));
    let tmp_b = tempfile::tempdir().expect("tempdir b");

    let engine_b = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        community_id,
        membership_key: mk,
        admin_addr: alice_addr,
        is_invite_only: true,
        device_id: "b-dev".into(),
        self_owner: alice_addr,
        signing_key: Arc::new(alice.device_key.clone()),
        state: Arc::clone(&state_b),
        tracker: Arc::clone(&tracker_b),
        content_store: cs_b,
        publisher_tx: out_tx,
        subscriber_rx: in_rx,
        paths: harmony_app::community_state_sync::PersistPaths {
            crdt: tmp_b.path().join("crdt.cbor"),
            replay: tmp_b.path().join("replay.cbor"),
        },
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        identity_resolver: Some(resolver_for_engine),
        error_tx: Some(degraded_tx),
        delta_tx: None,
        pending_redemptions: None,
        crdt_state: None,
        admin_identity_pub: None,
        nav_emitter: None,
        root_serve_rx: None,
    });

    // 1. Cold cache: alice not yet materialized in B → first delivery rejected
    //    with PublisherNotJoined.
    in_tx.send(wire.clone()).await.expect("inject 1");
    let report = tokio::time::timeout(std::time::Duration::from_secs(2), degraded_rx.recv())
        .await
        .expect("degraded report within 2s")
        .expect("degraded channel open");
    assert_eq!(report.reason_tag, "publisher_not_joined");
    {
        let t = tracker_b.lock().await;
        assert!(
            !t.per_device
                .contains_key(&(alice_addr, "alice-dev".to_string())),
            "tracker MUST NOT have advanced on PublisherNotJoined"
        );
    }
    // The empty resolver was never consulted on the publish path.
    assert!(
        resolver.inner.lock().await.is_empty(),
        "resolver must remain unconsulted on the ZEB-339 publish path"
    );
    let _ = alice_pub;

    // 2. Propagate alice's cert-bearing Join into B's CRDT — now she is a
    //    materialized member with her enrolled device key.
    {
        let mut sb = state_b.lock().await;
        let outcome = sb.insert_event(
            alice_join.clone(),
            &harmony_app::community_membership::VerifyContext {
                expected_community_id: community_id,
                admin_addr: alice_addr,
                is_invite_only: true,
            },
        );
        assert_eq!(
            outcome,
            harmony_app::community_state_crdt::InsertOutcome::Inserted
        );
    }

    // 3. Re-deliver the SAME wire packet — should now admit.
    in_tx.send(wire).await.expect("inject 2");
    // Poll for tracker advance with a deterministic 2s bound.
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let t = tracker_b.lock().await;
            if t.per_device
                .contains_key(&(alice_addr, "alice-dev".to_string()))
            {
                break;
            }
            drop(t);
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("tracker should advance within 2s after Join propagated");

    engine_b.shutdown().await.expect("shutdown");
}

/// ZEB-761: a failed community state-root publish must retry itself.
///
/// The community engine had the identical gap `fleet_sync` did: a failed
/// publish restored the dirty bit but cleared `next_wakeup`, arming nothing.
/// Its own in-code comment conceded it — *"the next publish OPPORTUNITY
/// retries it — a later mutation's debounce, flush_now, or shutdown"*. On a
/// quiescent community that means an unreplicated membership mutation, so
/// other members never see the roster change until the app is restarted.
///
/// Failure is injected at the CAS: the first `PutLocal` is answered with an
/// error, every later one succeeds. Time is paused, so the 30 s backoff costs
/// no wall-clock.
#[tokio::test(start_paused = true)]
async fn a_failed_community_publish_retries_itself_on_a_quiescent_community_zeb761() {
    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(8);
    let (_in_tx, in_rx) = mpsc::channel::<Vec<u8>>(8);
    let (cas_op_tx, mut cas_op_rx) = mpsc::channel(8);

    let put_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let put_attempts_task = Arc::clone(&put_attempts);
    tokio::spawn(async move {
        use harmony_app::content_store::CasOp;
        while let Some(op) = cas_op_rx.recv().await {
            if let CasOp::PutLocal {
                reply: Some(reply), ..
            } = op
            {
                let n = put_attempts_task.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                // Fail only the FIRST put; the retry must then succeed.
                let _ = reply.send(if n == 0 {
                    Err(harmony_app::content_store::ContentStoreError::Io(
                        "forced publish failure (ZEB-761)".into(),
                    ))
                } else {
                    Ok(())
                });
            }
        }
    });

    let community_id = SpaceId([9u8; 16]);
    let mk = EpochKey::new([0x77; 32]);
    let admin = OwnerAddr([8u8; 16]);

    let state = Arc::new(Mutex::new(CommunityState::new(community_id)));
    let tracker = Arc::new(Mutex::new(CommunityRootHlcTracker::default()));
    let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx,
        std::time::Duration::from_millis(1000),
    ));
    let tmp = tempfile::tempdir().expect("tempdir");

    let engine = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        community_id,
        membership_key: mk,
        admin_addr: admin,
        is_invite_only: false,
        device_id: "retry-device".into(),
        self_owner: admin,
        signing_key: Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0x77; 32])),
        state: Arc::clone(&state),
        tracker: Arc::clone(&tracker),
        content_store: cs,
        publisher_tx: out_tx,
        subscriber_rx: in_rx,
        paths: harmony_app::community_state_sync::PersistPaths {
            crdt: tmp.path().join("crdt.cbor"),
            replay: tmp.path().join("replay.cbor"),
        },
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        identity_resolver: None,
        error_tx: None,
        delta_tx: None,
        pending_redemptions: None,
        crdt_state: None,
        admin_identity_pub: None,
        nav_emitter: None,
        root_serve_rx: None,
    });

    // One dirty signal, then the community goes quiet — nothing else will
    // ever re-arm the debounce window.
    engine.notify_dirty();

    // Only the retry schedule can deliver this. The idle sleep is 3600 s,
    // far beyond this budget, so the pre-fix behaviour fails the timeout.
    let bytes = tokio::time::timeout(std::time::Duration::from_secs(120), out_rx.recv())
        .await
        .expect("a failed community publish must retry itself on a quiescent community (ZEB-761)")
        .expect("publisher_tx dropped or never sent");
    assert!(!bytes.is_empty(), "retry must carry a real wire packet");

    // It genuinely took a retry rather than succeeding first time.
    assert!(
        put_attempts.load(std::sync::atomic::Ordering::SeqCst) >= 2,
        "expected a failed publish followed by a retry"
    );

    engine.shutdown().await.ok();
}

/// ZEB-761: a persistently failing community publish must PACE its retries.
///
/// The community analog of `fleet_sync`'s
/// `a_persistently_failing_publish_paces_its_retries_zeb761`. Recovery and
/// pacing are two different properties, and pacing is the one that pins why
/// the naive fix was rejected: re-arming the deadline that just fired targets
/// an instant already in the PAST (its firing is what put us here), so the
/// sleep is zero-length and it re-fires immediately — `fire → fail → re-arm →
/// fire → …`, hammering the transport precisely while it is unhealthy. Both
/// engines compose debounce + retry the same way, so both need the assertion.
///
/// Every `PutLocal` fails, and `encode_root_packet` propagates the first CAS
/// error with `?`, so exactly one recorded attempt corresponds to one publish
/// attempt — which is what makes the gaps below readable as the retry cadence.
#[tokio::test(start_paused = true)]
async fn a_persistently_failing_community_publish_paces_its_retries_zeb761() {
    let (out_tx, _out_rx) = mpsc::channel::<Vec<u8>>(8);
    let (_in_tx, in_rx) = mpsc::channel::<Vec<u8>>(8);
    let (cas_op_tx, mut cas_op_rx) = mpsc::channel(8);

    let attempts = Arc::new(std::sync::Mutex::new(Vec::<tokio::time::Instant>::new()));
    let attempts_task = Arc::clone(&attempts);
    tokio::spawn(async move {
        use harmony_app::content_store::CasOp;
        while let Some(op) = cas_op_rx.recv().await {
            if let CasOp::PutLocal {
                reply: Some(reply), ..
            } = op
            {
                attempts_task
                    .lock()
                    .expect("attempts mutex")
                    .push(tokio::time::Instant::now());
                // Never recovers: the CAS stays down for the whole window.
                let _ = reply.send(Err(harmony_app::content_store::ContentStoreError::Io(
                    "forced persistent publish failure (ZEB-761)".into(),
                )));
            }
        }
    });

    let community_id = SpaceId([10u8; 16]);
    let mk = EpochKey::new([0x78; 32]);
    let admin = OwnerAddr([7u8; 16]);

    let state = Arc::new(Mutex::new(CommunityState::new(community_id)));
    let tracker = Arc::new(Mutex::new(CommunityRootHlcTracker::default()));
    let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx,
        std::time::Duration::from_millis(1000),
    ));
    let tmp = tempfile::tempdir().expect("tempdir");

    let engine = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        community_id,
        membership_key: mk,
        admin_addr: admin,
        is_invite_only: false,
        device_id: "spin-device".into(),
        self_owner: admin,
        signing_key: Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0x78; 32])),
        state: Arc::clone(&state),
        tracker: Arc::clone(&tracker),
        content_store: cs,
        publisher_tx: out_tx,
        subscriber_rx: in_rx,
        paths: harmony_app::community_state_sync::PersistPaths {
            crdt: tmp.path().join("crdt.cbor"),
            replay: tmp.path().join("replay.cbor"),
        },
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        identity_resolver: None,
        error_tx: None,
        delta_tx: None,
        pending_redemptions: None,
        crdt_state: None,
        admin_identity_pub: None,
        nav_emitter: None,
        root_serve_rx: None,
    });

    engine.notify_dirty();

    // Eight minutes of LOGICAL time (the clock is paused, so this costs no
    // wall-clock). The schedule is 30 s → 60 s → 120 s → 240 s, so this
    // window admits roughly four retries after the first failure.
    tokio::time::sleep(std::time::Duration::from_secs(480)).await;

    let stamps = attempts.lock().expect("attempts mutex").clone();

    // The anti-spin assertion. Unpaced, this window would have produced
    // attempts without bound rather than a handful.
    assert!(
        stamps.len() <= 6,
        "community retries must be paced, not spun: {} attempts in 480 s",
        stamps.len()
    );
    // ...but it must genuinely keep trying, not give up after one.
    assert!(
        stamps.len() >= 4,
        "the schedule must keep retrying a persistent failure: only {} attempts",
        stamps.len()
    );

    let gaps: Vec<u64> = stamps
        .windows(2)
        .map(|w| w[1].duration_since(w[0]).as_secs())
        .collect();
    assert!(
        gaps[0] >= 29,
        "first retry should wait ~30 s, waited {} s (gaps: {gaps:?})",
        gaps[0]
    );
    for pair in gaps.windows(2) {
        assert!(pair[1] >= pair[0], "retry gaps must never shrink: {gaps:?}");
    }
    assert!(
        *gaps.last().expect("at least one gap") > gaps[0],
        "retry gaps must ESCALATE, not merely be non-zero: {gaps:?}"
    );

    engine.shutdown().await.ok();
}
