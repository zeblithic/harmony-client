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

    let tmp_a = tempfile::tempdir().expect("tempdir a");
    let tmp_b = tempfile::tempdir().expect("tempdir b");

    let engine_a = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        community_id,
        membership_key: mk.clone(),
        admin_addr: admin,
        is_invite_only: false,
        device_id: "a-dev".into(),
        self_owner: admin,
        signing_key: Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0x42; 32])),
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
    engine_a.flush_now().await.expect("flush_now");

    // Wait deterministically for B to merge. A bounded poll keeps the
    // test from flaking on slow CI runners.
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if state_b.lock().await.events.len() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("B should have merged A's event within 2s");

    let sb = state_b.lock().await;
    assert_eq!(sb.events.len(), 1, "B should have merged A's event");
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
        assert!(
            matches!(
                outcome,
                harmony_app::community_state_crdt::InsertOutcome::Inserted
            ),
            "fixture insert must succeed; got {outcome:?}"
        );
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
        signing_key: Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0x42; 32])),
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

    let delta = tokio::time::timeout(Duration::from_secs(2), delta_rx.recv())
        .await
        .expect("delta should arrive within 2s")
        .expect("delta channel should be open");
    assert_eq!(delta.community_id, community_id);
    assert_eq!(delta.event.actor, admin);
    assert!(matches!(delta.event.kind, MembershipEventKind::Join));

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
