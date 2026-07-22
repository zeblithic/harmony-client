//! Tests for `CommunitySyncRegistry` — the multi-community engine
//! lifecycle manager. Covers the spawn → has → stop → shutdown_all
//! happy path, idempotent re-spawn, and `known_ids()` snapshot
//! ordering invariant.

use harmony_app::community_state_sync::{
    CommunityRegistryConfig, CommunitySyncRegistry, IdentityResolver, DEFAULT_DEBOUNCE_MS,
};
use harmony_app::content_store::{ContentStore, RuntimeContentStore};
use harmony_app::owner_state_types::{EpochKey, OwnerAddr, SpaceId};
use std::sync::Arc;
use tokio::sync::mpsc;

struct NopResolver;
#[async_trait::async_trait]
impl IdentityResolver for NopResolver {
    async fn resolve(&self, _: &OwnerAddr) -> Option<[u8; 64]> {
        None
    }
}

#[tokio::test]
async fn registry_spawns_and_tears_down_per_community() {
    let (cas_op_tx, _cas_op_rx) = mpsc::channel(8);
    let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx,
        std::time::Duration::from_millis(1000),
    ));

    let dir = tempfile::tempdir().expect("tempdir");

    let registry = CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_id: "dev".into(),
        content_store: cs,
        identity_resolver: Arc::new(NopResolver),
        identity_dir: dir.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: None,
        // ZEB-256 Task 6: registry holds local identity. This test
        // exercises spawn/teardown lifecycle only — no publishes
        // flow, so the OwnerAddr/SigningKey values just need to
        // satisfy the type bound.
        self_owner: OwnerAddr([0x01; 16]),
        signing_key: Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0x42; 32])),
        crdt_state: None,
        nav_emitter: None,
        presence_resync_rx: None,
    });

    let cid_a = SpaceId([1u8; 16]);
    let mk_a = EpochKey::new([0xa1; 32]);
    let admin_a = OwnerAddr([0xb1; 16]);

    let (a_pub_tx, _a_pub_rx) = mpsc::channel(8);
    let (_a_sub_tx, a_sub_rx) = mpsc::channel(8);
    registry
        .spawn_engine_inner_now(
            cid_a,
            mk_a,
            admin_a,
            /* is_invite_only */ false,
            a_pub_tx,
            a_sub_rx,
            harmony_app::community_state_sync::CatchUpChannels::none(),
        )
        .await
        .expect("spawn a");

    assert!(registry.has_engine(&cid_a).await);

    registry.stop_engine(&cid_a).await.expect("stop");
    assert!(!registry.has_engine(&cid_a).await);

    registry.shutdown_all().await.expect("shutdown_all");
}

#[tokio::test]
async fn registry_spawn_is_idempotent_and_known_ids_is_sorted() {
    let (cas_op_tx, _cas_op_rx) = mpsc::channel(8);
    let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx,
        std::time::Duration::from_millis(1000),
    ));
    let dir = tempfile::tempdir().expect("tempdir");

    let registry = CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_id: "dev".into(),
        content_store: cs,
        identity_resolver: Arc::new(NopResolver),
        identity_dir: dir.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: None,
        // ZEB-256 Task 6: lifecycle/idempotency test — values are
        // arbitrary, no publishes go through the wire.
        self_owner: OwnerAddr([0x01; 16]),
        signing_key: Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0x42; 32])),
        crdt_state: None,
        nav_emitter: None,
        presence_resync_rx: None,
    });

    // Spawn two distinct communities. Use unsorted ID order to verify
    // BTreeMap-backed known_ids() returns them sorted.
    let cid_b = SpaceId([0x02; 16]);
    let cid_a = SpaceId([0x01; 16]);

    let (a_pub_tx, _a_pub_rx) = mpsc::channel(8);
    let (_a_sub_tx, a_sub_rx) = mpsc::channel(8);
    let (b_pub_tx, _b_pub_rx) = mpsc::channel(8);
    let (_b_sub_tx, b_sub_rx) = mpsc::channel(8);

    registry
        .spawn_engine_inner_now(
            cid_b,
            EpochKey::new([0xb1; 32]),
            OwnerAddr([0xb1; 16]),
            false,
            b_pub_tx,
            b_sub_rx,
            harmony_app::community_state_sync::CatchUpChannels::none(),
        )
        .await
        .expect("spawn b");
    registry
        .spawn_engine_inner_now(
            cid_a,
            EpochKey::new([0xa1; 32]),
            OwnerAddr([0xa1; 16]),
            false,
            a_pub_tx,
            a_sub_rx,
            harmony_app::community_state_sync::CatchUpChannels::none(),
        )
        .await
        .expect("spawn a");

    // Idempotent re-spawn: returns Ok, doesn't double-insert.
    let (a2_pub_tx, _a2_pub_rx) = mpsc::channel(8);
    let (_a2_sub_tx, a2_sub_rx) = mpsc::channel(8);
    registry
        .spawn_engine_inner_now(
            cid_a,
            EpochKey::new([0xa1; 32]),
            OwnerAddr([0xa1; 16]),
            false,
            a2_pub_tx,
            a2_sub_rx,
            harmony_app::community_state_sync::CatchUpChannels::none(),
        )
        .await
        .expect("re-spawn idempotent");

    // BTreeMap → known_ids() returns SpaceId-Ord order regardless of
    // insertion order.
    let ids = registry.known_ids().await;
    assert_eq!(ids, vec![cid_a, cid_b], "known_ids must be sorted");

    // shutdown_all drains the map: known_ids must be empty afterwards.
    registry.shutdown_all().await.expect("shutdown_all");
    assert!(
        registry.known_ids().await.is_empty(),
        "shutdown_all must empty the map"
    );
}

// ── ZEB-262 Phase 4 Task 7: shutdown_engine_and_cleanup_persistence ──
// + pending_redemptions map ─────────────────────────────────────────

#[tokio::test]
async fn shutdown_engine_and_cleanup_persistence_idempotent_on_unknown_id() {
    let (cas_op_tx, _cas_op_rx) = mpsc::channel(8);
    let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx,
        std::time::Duration::from_millis(1000),
    ));

    let dir = tempfile::tempdir().expect("tempdir");
    let registry = CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_id: "dev".into(),
        content_store: cs,
        identity_resolver: Arc::new(NopResolver),
        identity_dir: dir.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: None,
        self_owner: OwnerAddr([0x01; 16]),
        signing_key: Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0x42; 32])),
        crdt_state: None,
        nav_emitter: None,
        presence_resync_rx: None,
    });

    let unknown_id = SpaceId([0xff; 16]);
    registry
        .shutdown_engine_and_cleanup_persistence(&unknown_id)
        .await
        .expect("idempotent on unknown id must return Ok");
}

#[tokio::test]
async fn shutdown_engine_and_cleanup_persistence_removes_dir_after_engine_stops() {
    let (cas_op_tx, mut cas_op_rx) = mpsc::channel(8);
    // RuntimeContentStore expects someone to service CasOps. Without
    // this, `flush_now`'s `content_store.put` hangs forever waiting
    // for the PutLocal reply. Spawn a no-op servicer that just acks
    // every PutLocal — the persist+publish cycle is what we're
    // exercising, not the CAS layer itself.
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
    let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx,
        std::time::Duration::from_millis(1000),
    ));

    let dir = tempfile::tempdir().expect("tempdir");
    let registry = CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_id: "dev".into(),
        content_store: cs,
        identity_resolver: Arc::new(NopResolver),
        identity_dir: dir.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: None,
        self_owner: OwnerAddr([0x01; 16]),
        signing_key: Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0x42; 32])),
        crdt_state: None,
        nav_emitter: None,
        presence_resync_rx: None,
    });

    let cid = SpaceId([1u8; 16]);
    let mk = EpochKey::new([0xaa; 32]);
    let admin = OwnerAddr([0xbb; 16]);

    let (pub_tx, mut pub_rx) = mpsc::channel(8);
    let (_sub_tx, sub_rx) = mpsc::channel(8);
    registry
        .spawn_engine_inner_now(
            cid,
            mk,
            admin,
            false,
            pub_tx,
            sub_rx,
            harmony_app::community_state_sync::CatchUpChannels::none(),
        )
        .await
        .expect("spawn");

    // The per-community dir is created lazily on the first persist
    // write. Drive a flush_now so the engine emits one publish + one
    // persist cycle, materialising the directory before we tear it
    // down. Drain the publish channel so flush_now isn't backpressured
    // on a full mpsc.
    tokio::spawn(async move { while pub_rx.recv().await.is_some() {} });
    registry.flush_now(&cid).await.expect("flush_now");

    let community_dir = dir.path().join("communities").join(hex::encode(cid.0));
    assert!(
        community_dir.exists(),
        "community dir should exist after flush_now: {:?}",
        community_dir
    );

    registry
        .shutdown_engine_and_cleanup_persistence(&cid)
        .await
        .expect("teardown");

    // Engine no longer in registry.
    assert!(
        !registry.has_engine(&cid).await,
        "engine map still holds {} after shutdown",
        hex::encode(cid.0)
    );

    // Persistence directory removed.
    assert!(
        !community_dir.exists(),
        "per-community persist dir not removed: {:?}",
        community_dir
    );
}

/// ZEB-501 regression guard: a non-countersign event (here a Join) whose own
/// id matches a registered pending-redemption key must NOT fire the oneshot.
/// Only a `JoinCountersign` targeting that id wakes the redeem — the joiner's
/// own PendingJoin self-insert used to wake it (the bug). The positive
/// direction (a JoinCountersign fires the oneshot → redeem returns Joined) is
/// covered by `pkarr_iroh_redeem_full_integration`.
#[tokio::test]
async fn pending_redemption_oneshot_not_fired_by_own_event_insert() {
    use harmony_app::community_membership::{sign_event, EventPayload, MembershipEventKind};
    use harmony_app::owner_state_types::Hlc;

    struct AdminResolver {
        addr: OwnerAddr,
        pubkey: [u8; 64],
    }
    #[async_trait::async_trait]
    impl IdentityResolver for AdminResolver {
        async fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
            if *addr == self.addr {
                Some(self.pubkey)
            } else {
                None
            }
        }
    }

    // ZEB-339: enrolled-device owner — actor = owner_id, the self-Join is signed
    // by the device key and carries the owner's EnrollmentCert so verify_event
    // resolves the signer from the cert (not via AdminResolver) and the
    // pending-redemption oneshot fires on insert.
    // `admin_pub` is zeroed ([0u8;64]) to assert that the AdminResolver is
    // intentionally NOT consulted during cert-bearing Join verification — the
    // EnrollmentCert binding takes precedence.
    let owner = harmony_app::community_membership::mint_test_owner(0x55);
    let admin_addr = owner.owner;
    let admin_pub = [0u8; 64]; // zeroed: AdminResolver NOT consulted for cert-bearing Joins
    let admin_sk = Arc::new(owner.device_key.clone());

    let (cas_op_tx, _cas_op_rx) = mpsc::channel(8);
    let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx,
        std::time::Duration::from_millis(1000),
    ));
    let dir = tempfile::tempdir().expect("tempdir");
    let registry = Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_id: "admin-dev".into(),
        content_store: cs,
        identity_resolver: Arc::new(AdminResolver {
            addr: admin_addr,
            pubkey: admin_pub,
        }),
        identity_dir: dir.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: None,
        self_owner: admin_addr,
        signing_key: Arc::clone(&admin_sk),
        crdt_state: None,
        nav_emitter: None,
        presence_resync_rx: None,
    }));

    let cid = SpaceId([0x10; 16]);
    let mk = EpochKey::new([0xaa; 32]);
    let (pub_tx, _pub_rx) = mpsc::channel(8);
    let (_sub_tx, sub_rx) = mpsc::channel(8);
    registry
        .spawn_engine_inner_now(
            cid,
            mk,
            admin_addr,
            /* is_invite_only */ false,
            pub_tx,
            sub_rx,
            harmony_app::community_state_sync::CatchUpChannels::none(),
        )
        .await
        .expect("spawn");

    // Mint a self-Join event and register a oneshot keyed on its EventId.
    let event_id = [0x77u8; 16];
    let join = harmony_app::community_membership::SignedMembershipEvent {
        enrollment: Some(owner.cert.clone()),
        ..sign_event(
            &EventPayload {
                id: event_id,
                community_id: cid,
                kind: MembershipEventKind::Join,
                actor: admin_addr,
                at: Hlc {
                    wall_ms: 1000,
                    logical: 0,
                    device_id: "admin-dev".into(),
                },
            },
            admin_sk.as_ref(),
        )
        .unwrap()
    };

    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    registry.register_pending_redemption(event_id, tx).await;

    let engine = registry.engine_arc(&cid).await.expect("engine present");
    engine
        .insert_local_event(join.clone())
        .await
        .expect("insert");

    // ZEB-501: a Join whose own id == the registered key must NOT fire the
    // oneshot — only a JoinCountersign with target_event_id == the key does.
    // (Pre-fix, this Insert fired it via the legacy notify-on-event.id.)
    // 200ms is comfortably longer than the in-process local-insert + hook path.
    tokio::time::timeout(std::time::Duration::from_millis(200), rx)
        .await
        .expect_err(
            "ZEB-501: a non-countersign (Join) insert must NOT wake the \
             pending-redemption oneshot, even when its id matches the key",
        );
}

#[tokio::test]
async fn pending_redemption_unregistered_when_no_match() {
    use harmony_app::community_membership::{sign_event, EventPayload, MembershipEventKind};
    use harmony_app::owner_state_types::Hlc;

    struct AdminResolver {
        addr: OwnerAddr,
        pubkey: [u8; 64],
    }
    #[async_trait::async_trait]
    impl IdentityResolver for AdminResolver {
        async fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
            if *addr == self.addr {
                Some(self.pubkey)
            } else {
                None
            }
        }
    }

    // ZEB-339: enrolled-device owner — actor = owner_id, the self-Join is signed
    // by the device key and carries the owner's EnrollmentCert so verify_event
    // resolves the signer from the cert (not via AdminResolver) and the
    // pending-redemption oneshot fires on insert.
    // `admin_pub` is zeroed ([0u8;64]) to assert that the AdminResolver is
    // intentionally NOT consulted during cert-bearing Join verification — the
    // EnrollmentCert binding takes precedence.
    let owner = harmony_app::community_membership::mint_test_owner(0x55);
    let admin_addr = owner.owner;
    let admin_pub = [0u8; 64]; // zeroed: AdminResolver NOT consulted for cert-bearing Joins
    let admin_sk = Arc::new(owner.device_key.clone());

    let (cas_op_tx, _cas_op_rx) = mpsc::channel(8);
    let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx,
        std::time::Duration::from_millis(1000),
    ));
    let dir = tempfile::tempdir().expect("tempdir");
    let registry = Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_id: "admin-dev".into(),
        content_store: cs,
        identity_resolver: Arc::new(AdminResolver {
            addr: admin_addr,
            pubkey: admin_pub,
        }),
        identity_dir: dir.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: None,
        self_owner: admin_addr,
        signing_key: Arc::clone(&admin_sk),
        crdt_state: None,
        nav_emitter: None,
        presence_resync_rx: None,
    }));

    let cid = SpaceId([0x10; 16]);
    let mk = EpochKey::new([0xaa; 32]);
    let (pub_tx, _pub_rx) = mpsc::channel(8);
    let (_sub_tx, sub_rx) = mpsc::channel(8);
    registry
        .spawn_engine_inner_now(
            cid,
            mk,
            admin_addr,
            /* is_invite_only */ false,
            pub_tx,
            sub_rx,
            harmony_app::community_state_sync::CatchUpChannels::none(),
        )
        .await
        .expect("spawn");

    // Register a oneshot keyed on EventId X but insert an event with
    // EventId Y. The notify hook on Inserted is keyed off the event's
    // OWN id, so the X-keyed oneshot must NOT fire.
    let registered_id = [0xeeu8; 16];
    let inserted_id = [0x11u8; 16];
    assert_ne!(registered_id, inserted_id);

    let join = harmony_app::community_membership::SignedMembershipEvent {
        enrollment: Some(owner.cert.clone()),
        ..sign_event(
            &EventPayload {
                id: inserted_id,
                community_id: cid,
                kind: MembershipEventKind::Join,
                actor: admin_addr,
                at: Hlc {
                    wall_ms: 1000,
                    logical: 0,
                    device_id: "admin-dev".into(),
                },
            },
            admin_sk.as_ref(),
        )
        .unwrap()
    };

    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    registry
        .register_pending_redemption(registered_id, tx)
        .await;

    let engine = registry.engine_arc(&cid).await.expect("engine present");
    engine
        .insert_local_event(join.clone())
        .await
        .expect("insert");

    // Short timeout — we want to verify the oneshot did NOT fire.
    // 200ms is comfortably longer than the engine's local-insert path
    // takes (entirely in-process; no I/O between insert and the hook
    // firing or not firing).
    tokio::time::timeout(std::time::Duration::from_millis(200), rx)
        .await
        .expect_err(
            "oneshot must NOT fire when the inserted event id doesn't match the registered key",
        );
}
