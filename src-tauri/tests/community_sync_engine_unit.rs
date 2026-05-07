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
use harmony_app::owner_state_types::{MembershipKey, OwnerAddr, SpaceId};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

use harmony_app::community_membership::{
    sign_event_with_identity, EventPayload, MembershipEventKind,
};
use harmony_app::owner_state_types::Hlc;
use harmony_identity::PrivateIdentity;

/// Reach into `PrivateIdentity`'s ed25519 seed the same way production
/// does (see `lib.rs::start_node` and `community_open_flow_integration`):
/// the canonical 32-byte seed lives in bytes 32..64 of `to_private_bytes()`
/// (`X25519_secret(32) || Ed25519_secret(32)`). Construct an
/// `ed25519_dalek::SigningKey` from those bytes so tests sign with the
/// same key the IPC will use in production. ZEB-256 makes this load-
/// bearing: receive-side sig-verify in `handle_incoming_publish` checks
/// `payload.publisher_sig` against the publisher's `identity_pub`, so
/// the engine's `signing_key` must match.
fn signing_key_from(identity: &PrivateIdentity) -> ed25519_dalek::SigningKey {
    let private_bytes = identity.to_private_bytes();
    let mut secret = [0u8; 32];
    secret.copy_from_slice(&private_bytes[32..64]);
    ed25519_dalek::SigningKey::from_bytes(&secret)
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
    let mk = MembershipKey::new([0x42; 32]);
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
    let mk = MembershipKey::new([0x42; 32]);
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
                CasOp::PutLocal { cid, blob, reply } => {
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
    let mk = MembershipKey::new([0x42; 32]);

    let identity_a = PrivateIdentity::from_seed(&[0xa1; 32]);
    let admin = OwnerAddr(identity_a.identity.address_hash);
    let identity_a_pub = identity_a.identity.to_public_bytes();
    // ZEB-256: engine_a's signing_key must match identity_a's
    // ed25519 half so engine_b's receive-side sig-verify (against
    // resolver's identity_a_pub) succeeds.
    let signing_a = signing_key_from(&identity_a);

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
                actor_identity_pub: &identity_a_pub,
                countersigner_identity_pub: None,
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
                actor_identity_pub: &identity_a_pub,
                countersigner_identity_pub: None,
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
        sb.events.len(),
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
                CasOp::PutLocal { cid, blob, reply } => {
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
            }
        }
    });
    tokio::spawn(async move {
        while let Some(bytes) = a_out_rx.recv().await {
            let _ = b_in_tx.send(bytes).await;
        }
    });

    let community_id = SpaceId([1u8; 16]);
    let mk = MembershipKey::new([0x42; 32]);
    let identity_a = PrivateIdentity::from_seed(&[0xa1; 32]);
    let admin = OwnerAddr(identity_a.identity.address_hash);
    let identity_a_pub = identity_a.identity.to_public_bytes();
    // ZEB-256: engine_a's signing_key must match identity_a's
    // ed25519 half so engine_b's receive-side sig-verify (against
    // resolver's identity_a_pub) succeeds.
    let signing_a = signing_key_from(&identity_a);

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
                actor_identity_pub: &identity_a_pub,
                countersigner_identity_pub: None,
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
                actor_identity_pub: &identity_a_pub,
                countersigner_identity_pub: None,
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
                actor_identity_pub: &identity_a_pub,
                countersigner_identity_pub: None,
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
    use harmony_app::community_state_sync::{CommunityMembershipDelta, LocalInsertError};
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
    let mk = MembershipKey::new([0x33; 32]);
    let identity = PrivateIdentity::from_seed(&[0xc1; 32]);
    let admin = OwnerAddr(identity.identity.address_hash);
    let identity_pub = identity.identity.to_public_bytes();

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
    // With this StaticResolver the bad-actor address returns None at
    // resolve time, so the impl short-circuits with Err(UnknownActor)
    // BEFORE verify_event runs. Verify failures (when the resolver does
    // resolve the actor) surface as Ok(InsertOutcome::Rejected(_)) — the
    // engine's `insert_local_event` no longer carries a `Verify` variant
    // because the verify-rejection path always reaches the CRDT layer
    // and gets folded into the Insert outcome. Widening the matcher
    // preserves the test's intent ("a bogus actor must not insert").
    assert!(matches!(
        result,
        Err(LocalInsertError::UnknownActor(_))
            | Ok(harmony_app::community_state_crdt::InsertOutcome::Rejected(
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
            CommunitySyncError::UnknownPublisher { addr: alice },
            "publisher_unknown",
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
        membership_key: MembershipKey::new([0x42; 32]),
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
    use harmony_app::owner_state_types::{MembershipKey, OwnerAddr, SpaceId};

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
    let mk = MembershipKey::new([0x42; 32]);
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
//   3. UnknownPublisher — resolver doesn't know publisher_addr (cold
//      cache). Same envelope re-delivered after resolver populates
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
                CasOp::PutLocal { cid, blob, reply } => {
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
            }
        }
    });

    let community_id = SpaceId([7u8; 16]);
    let mk = MembershipKey::new([0xAA; 32]);

    let alice = PrivateIdentity::from_seed(&[0xa1; 32]);
    let alice_addr = OwnerAddr(alice.identity.address_hash);
    let alice_pub = alice.identity.to_public_bytes();

    let bob = PrivateIdentity::from_seed(&[0xb1; 32]);
    let bob_addr = OwnerAddr(bob.identity.address_hash);
    let bob_pub = bob.identity.to_public_bytes();
    // Bob's signing key is derived from his identity seed (production
    // path); the value of the bytes doesn't matter for spoofing intent
    // — what matters is that it doesn't match alice's identity_pub.
    let bob_signing = signing_key_from(&bob);

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
                actor_identity_pub: &alice_pub,
                countersigner_identity_pub: None,
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
    let envelope = signed.into_wire(bad_sig);
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
        signing_key: Arc::new(signing_key_from(&bob)),
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
                CasOp::PutLocal { cid, blob, reply } => {
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
            }
        }
    });

    let community_id = SpaceId([8u8; 16]);
    let mk = MembershipKey::new([0xCC; 32]);

    let admin_id = PrivateIdentity::from_seed(&[0xa0; 32]);
    let admin_addr = OwnerAddr(admin_id.identity.address_hash);
    let admin_pub = admin_id.identity.to_public_bytes();
    let admin_signing = signing_key_from(&admin_id);

    let alice = PrivateIdentity::from_seed(&[0xa1; 32]);
    let alice_addr = OwnerAddr(alice.identity.address_hash);
    let alice_pub = alice.identity.to_public_bytes();
    let alice_signing = signing_key_from(&alice);

    // Build a CommunityState where:
    //   - admin is Joined
    //   - alice was Joined then Kicked at HLC 100
    let mut state = CommunityState::new(community_id);
    let push_event = |state: &mut CommunityState,
                      actor: OwnerAddr,
                      actor_id: &PrivateIdentity,
                      actor_pub: &[u8; 64],
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
                actor_identity_pub: actor_pub,
                countersigner_identity_pub: None,
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
    let envelope = signed.into_wire(valid_sig);
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
        signing_key: Arc::new(signing_key_from(&admin_id)),
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
async fn cold_cache_publish_rejected_then_succeeds_after_propagation() {
    // The same envelope is delivered twice. First time the resolver
    // returns None for alice → UnknownPublisher rejection. Then we
    // populate the resolver (via Mutex<HashMap> so the test owns the
    // entries) and re-deliver — the engine accepts.
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
                CasOp::PutLocal { cid, blob, reply } => {
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
            }
        }
    });

    let community_id = SpaceId([9u8; 16]);
    let mk = MembershipKey::new([0xDD; 32]);

    let alice = PrivateIdentity::from_seed(&[0xa1; 32]);
    let alice_addr = OwnerAddr(alice.identity.address_hash);
    let alice_pub = alice.identity.to_public_bytes();
    let alice_signing = signing_key_from(&alice);

    // CommunityState with alice Joined.
    let mut state = CommunityState::new(community_id);
    {
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
        let ev = sign_event_with_identity(&p, &alice).expect("sign");
        let outcome = state.insert_event(
            ev,
            &harmony_app::community_membership::VerifyContext {
                expected_community_id: community_id,
                admin_addr: alice_addr,
                is_invite_only: false,
                actor_identity_pub: &alice_pub,
                countersigner_identity_pub: None,
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
    let envelope = signed.into_wire(sig);
    let envelope_bytes = canonical_cbor_encode(&envelope).expect("encode env");
    let wire = encrypt_root_publish(&mk, &envelope_bytes).expect("encrypt root");

    let resolver = Arc::new(MutableResolver {
        inner: tokio::sync::Mutex::new(std::collections::HashMap::new()),
    });
    let resolver_for_engine: Arc<dyn harmony_app::community_state_sync::IdentityResolver> =
        Arc::clone(&resolver) as _;

    // B's CRDT pre-seeds alice as Joined so the membership-at-HLC gate
    // passes; the failure mode under test is the resolver-cold-cache
    // gate, which sits BETWEEN membership and sig-verify.
    let state_b = Arc::new(Mutex::new(state));
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
        self_owner: alice_addr,
        signing_key: Arc::new(signing_key_from(&alice)),
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
    });

    // 1. Cold cache: resolver empty → first delivery rejected.
    in_tx.send(wire.clone()).await.expect("inject 1");
    let report = tokio::time::timeout(std::time::Duration::from_secs(2), degraded_rx.recv())
        .await
        .expect("degraded report within 2s")
        .expect("degraded channel open");
    assert_eq!(report.reason_tag, "publisher_unknown");
    {
        let t = tracker_b.lock().await;
        assert!(
            !t.per_device
                .contains_key(&(alice_addr, "alice-dev".to_string())),
            "tracker MUST NOT have advanced on UnknownPublisher"
        );
    }

    // 2. Insert alice into resolver — simulating cache propagation.
    resolver.inner.lock().await.insert(alice_addr, alice_pub);

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
    .expect("tracker should advance within 2s after resolver populated");

    engine_b.shutdown().await.expect("shutdown");
}
