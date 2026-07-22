//! Two-engine integration test for ZEB-248 Phase 1 channel-config CRDT.
//!
//! Round-trips: Alice creates a channel → state-CRDT publish → Bob's
//! engine merges the published events → Bob materializes the same
//! `ChannelInfo`. Also verifies (a) mod-tier gating end-to-end (a
//! sub-mod actor's `ChannelCreate` is locally rejected before publish);
//! (b) the default-`#general` auto-creation pattern (`ChannelCreate`
//! immediately after `bootstrap_join` is materialized on both sides).
//!
//! Test plumbing mirrors `community_invite_only_integration.rs` and
//! `community_open_flow_integration.rs` — same `TwoIdentityResolver`,
//! same `publisher_tx ↔ subscriber_rx` forwarding shape, separate
//! `CommunitySyncRegistry` per identity so each side's `delta_tx` is
//! per-engine.

use harmony_app::community_membership::{
    mint_test_owner, sign_event, ChannelId, ChannelKind, EventPayload, MembershipEventKind,
    SignedMembershipEvent, TestOwner, VerifyError,
};
use harmony_app::community_state_crdt::InsertOutcome;
use harmony_app::community_state_sync::{
    CommunityMembershipDelta, CommunityRegistryConfig, CommunitySyncRegistry, IdentityResolver,
    DEFAULT_DEBOUNCE_MS,
};
use harmony_app::content_store::{CasOp, ContentStore, RuntimeContentStore};
use harmony_app::owner_state_types::{EpochKey, Hlc, OwnerAddr, SpaceId};
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

/// ZEB-339: attach the owner's Master enrollment cert to a Join event so the
/// receiving engine's `materialize`/`verify_event` learns the owner's enrolled
/// device key. Steady-state events (ChannelCreate, etc.) carry no cert.
fn with_join_cert(event: SignedMembershipEvent, owner: &TestOwner) -> SignedMembershipEvent {
    SignedMembershipEvent {
        enrollment: Some(owner.cert.clone()),
        ..event
    }
}

/// Spawn the in-memory CAS servicer used by `RuntimeContentStore`.
/// Mirrors `community_open_flow_integration::spawn_shared_cas` so
/// blobs Alice puts are visible to Bob's gets and vice versa.
fn spawn_shared_cas() -> mpsc::Sender<CasOp> {
    let cas: Arc<Mutex<HashMap<harmony_content::cid::ContentId, Vec<u8>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let (cas_op_tx, mut cas_op_rx) = mpsc::channel::<CasOp>(64);
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
    cas_op_tx
}

/// Drain `delta_rx` until a delta whose event matches `pred` arrives, or
/// the per-recv timeout elapses with no further events. Returns the
/// matching delta. Each `recv` waits up to 5s — generous because the
/// CI box can be slow under load and a tight timeout would mask a
/// real wedge as a flake. The loop is unbounded — the per-recv timeout
/// is the safety net (an arbitrary iteration cap would be brittle
/// against harmless extra emissions e.g. a delta for an unrelated
/// pre-seed event).
async fn drain_until<F>(
    rx: &mut mpsc::Receiver<CommunityMembershipDelta>,
    mut pred: F,
    label: &str,
) -> CommunityMembershipDelta
where
    F: FnMut(&CommunityMembershipDelta) -> bool,
{
    loop {
        let next = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap_or_else(|_| panic!("{label}: timed out waiting for matching delta"))
            .unwrap_or_else(|| panic!("{label}: delta channel closed"));
        if pred(&next) {
            return next;
        }
    }
}

/// Wire alice_pub_rx → bob_sub_tx and bob_pub_rx → alice_sub_tx, the
/// two-engine forwarder used by the invite-only and open-flow tests.
/// Returns nothing — the spawned tasks live for the test process.
fn spawn_forwarder(
    mut alice_pub_rx: mpsc::Receiver<Vec<u8>>,
    bob_sub_tx: mpsc::Sender<Vec<u8>>,
    mut bob_pub_rx: mpsc::Receiver<Vec<u8>>,
    alice_sub_tx: mpsc::Sender<Vec<u8>>,
) {
    tokio::spawn(async move {
        while let Some(bytes) = alice_pub_rx.recv().await {
            if bob_sub_tx.send(bytes).await.is_err() {
                break;
            }
        }
    });
    tokio::spawn(async move {
        while let Some(bytes) = bob_pub_rx.recv().await {
            if alice_sub_tx.send(bytes).await.is_err() {
                break;
            }
        }
    });
}

/// Test 1 — happy path: Alice signs `bootstrap_join` then `ChannelCreate`,
/// each is published via the state-CRDT, Bob's engine debounces, decrypts,
/// verifies, materializes, and the channel shows up in Bob's
/// `MaterializedMembership.channels`.
#[tokio::test]
async fn alice_creates_channel_bob_materializes_via_state_sync() {
    let alice = mint_test_owner(0xAA);
    let bob = mint_test_owner(0xBB);
    let alice_addr = alice.owner;
    let bob_addr = bob.owner;
    let alice_sk = Arc::new(alice.device_key.clone());
    let bob_sk = Arc::new(bob.device_key.clone());

    // ZEB-339: signer resolution uses the carried EnrollmentCert (Join) /
    // materialized enrolled keys (steady-state), not the resolver — so the
    // resolver's identity_pubs are unused for membership verify here.
    let resolver: Arc<dyn IdentityResolver> = Arc::new(TwoIdentityResolver {
        a: (alice_addr, [0u8; 64]),
        b: (bob_addr, [0u8; 64]),
    });

    let cas_op_tx = spawn_shared_cas();
    let cs_a: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx.clone(),
        Duration::from_secs(2),
    ));
    let cs_b: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx.clone(),
        Duration::from_secs(2),
    ));

    let dir_a = tempfile::tempdir().expect("dir a");
    let dir_b = tempfile::tempdir().expect("dir b");

    let community_id = SpaceId([0x37; 16]);
    let membership_key = EpochKey::new([0x55; 32]);

    // Per-identity registry so each side has its own `delta_tx`.
    let (delta_a_tx, mut delta_a_rx) = mpsc::channel::<CommunityMembershipDelta>(32);
    let (delta_b_tx, mut delta_b_rx) = mpsc::channel::<CommunityMembershipDelta>(32);

    let registry_a = Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_id: "alice-dev".into(),
        content_store: Arc::clone(&cs_a),
        identity_resolver: Arc::clone(&resolver),
        identity_dir: dir_a.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: Some(delta_a_tx),
        self_owner: alice_addr,
        signing_key: Arc::clone(&alice_sk),
        crdt_state: None,
        nav_emitter: None,
        presence_resync_rx: None,
    }));
    let registry_b = Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_id: "bob-dev".into(),
        content_store: Arc::clone(&cs_b),
        identity_resolver: Arc::clone(&resolver),
        identity_dir: dir_b.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: Some(delta_b_tx),
        self_owner: bob_addr,
        signing_key: Arc::clone(&bob_sk),
        crdt_state: None,
        nav_emitter: None,
        presence_resync_rx: None,
    }));

    // Wire the publish/subscribe round-trip: Alice's publisher_rx →
    // Bob's subscriber_tx, and vice versa.
    let (alice_pub_tx, alice_pub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (alice_sub_tx, alice_sub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (bob_pub_tx, bob_pub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (bob_sub_tx, bob_sub_rx) = mpsc::channel::<Vec<u8>>(64);
    spawn_forwarder(alice_pub_rx, bob_sub_tx, bob_pub_rx, alice_sub_tx);

    registry_a
        .spawn_engine_inner_now(
            community_id,
            membership_key.clone(),
            alice_addr,
            false,
            alice_pub_tx,
            alice_sub_rx,
            harmony_app::community_state_sync::CatchUpChannels::none(),
        )
        .await
        .expect("spawn alice engine");
    registry_b
        .spawn_engine_inner_now(
            community_id,
            membership_key.clone(),
            alice_addr,
            false,
            bob_pub_tx,
            bob_sub_rx,
            harmony_app::community_state_sync::CatchUpChannels::none(),
        )
        .await
        .expect("spawn bob engine");

    let alice_engine = registry_a
        .engine_arc(&community_id)
        .await
        .expect("alice engine");
    let bob_engine = registry_b
        .engine_arc(&community_id)
        .await
        .expect("bob engine");

    // Step 1: Alice signs + inserts her bootstrap Join (admin power 100).
    let alice_join_at = Hlc {
        wall_ms: 100_000,
        logical: 0,
        device_id: "alice-dev".into(),
    };
    let alice_join_payload = EventPayload {
        id: [0x10; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: alice_addr,
        at: alice_join_at.clone(),
    };
    let alice_join = with_join_cert(
        sign_event(&alice_join_payload, alice_sk.as_ref()).expect("sign join"),
        &alice,
    );
    let outcome = alice_engine
        .insert_local_event(alice_join.clone())
        .await
        .expect("alice bootstrap insert");
    assert_eq!(outcome, InsertOutcome::Inserted);
    let _alice_join_delta = drain_until(
        &mut delta_a_rx,
        |d| matches!(d.event.kind, MembershipEventKind::Join),
        "alice own Join delta",
    )
    .await;

    // ZEB-256 Task 6 cold-cache simulation: insert Alice's bootstrap
    // Join into Bob's engine OOB so Bob's membership-at-HLC gate
    // admits Alice's first ChannelCreate publish. Production wires
    // this via the redemption flow (admin_bootstrap field on the
    // invite URL); here we simulate the same pre-state. Mirrors
    // `community_open_flow_integration::open_community_create_redeem_leave_round_trip`.
    bob_engine
        .insert_local_event(alice_join.clone())
        .await
        .expect("bob OOB-seeds Alice's bootstrap Join (cold-cache simulation)");
    let _bob_seeded_join = drain_until(
        &mut delta_b_rx,
        |d| matches!(d.event.kind, MembershipEventKind::Join) && d.event.actor == alice_addr,
        "bob delta from OOB-seeded Alice Join",
    )
    .await;

    // Step 2: Alice signs + inserts ChannelCreate { name: "general", wp: 0 }
    // with logical+1 HLC after the bootstrap Join — same deterministic
    // ordering Task 7's `create_community_inner` uses.
    let ch_id = ChannelId([0x42; 16]);
    let alice_create_at = Hlc {
        wall_ms: alice_join_at.wall_ms,
        logical: alice_join_at.logical + 1,
        device_id: alice_join_at.device_id.clone(),
    };
    let alice_create_payload = EventPayload {
        id: [0x11; 16],
        community_id,
        kind: MembershipEventKind::ChannelCreate {
            channel_id: ch_id,
            name: "general".into(),
            write_power: 0,
            kind: ChannelKind::Text,
        },
        actor: alice_addr,
        at: alice_create_at,
    };
    let alice_create =
        sign_event(&alice_create_payload, alice_sk.as_ref()).expect("sign channel create");
    let outcome = alice_engine
        .insert_local_event(alice_create.clone())
        .await
        .expect("alice channel-create insert");
    assert_eq!(outcome, InsertOutcome::Inserted);
    let alice_ch_delta = drain_until(
        &mut delta_a_rx,
        |d| matches!(d.event.kind, MembershipEventKind::ChannelCreate { .. }),
        "alice own ChannelCreate delta",
    )
    .await;
    assert!(matches!(
        alice_ch_delta.event.kind,
        MembershipEventKind::ChannelCreate { ref name, write_power: 0, channel_id, .. }
            if name == "general" && channel_id == ch_id
    ));

    // Step 3: Bob's engine receives Alice's ChannelCreate via the
    // forwarder, debounces, decrypts, verifies, materializes, and fires
    // a delta. (Alice's bootstrap Join was OOB-seeded into Bob's engine
    // earlier — see the cold-cache simulation above.)
    let bob_ch_delta = drain_until(
        &mut delta_b_rx,
        |d| matches!(d.event.kind, MembershipEventKind::ChannelCreate { .. }),
        "bob received Alice's ChannelCreate",
    )
    .await;
    match &bob_ch_delta.event.kind {
        MembershipEventKind::ChannelCreate {
            channel_id,
            name,
            write_power,
            kind,
        } => {
            assert_eq!(*channel_id, ch_id);
            assert_eq!(name, "general");
            assert_eq!(*write_power, 0);
            assert_eq!(*kind, ChannelKind::Text);
        }
        other => panic!("expected ChannelCreate, got {other:?}"),
    }

    // Step 4: Bob's materialized state has the channel under the right
    // ChannelInfo with deleted_at == None.
    let bob_state = registry_b
        .state_for(&community_id)
        .await
        .expect("bob state");
    let bob_materialized = {
        let g = bob_state.lock().await;
        g.materialize_now(alice_addr)
    };
    let info = bob_materialized
        .channels
        .get(&ch_id)
        .expect("bob has channel");
    assert_eq!(info.name, "general");
    assert_eq!(info.write_power, 0);
    assert!(info.deleted_at.is_none());
}

/// Test 2 — sub-mod power gating: Bob is a Joined member with default
/// power 0 (well below `POWER_THRESHOLDS.kick = 50`). He attempts a
/// `ChannelCreate`. `verify_event`'s membership check (step 4) passes
/// because Bob IS Joined; the per-kind power gate (step 5) fires and
/// returns `ChannelAdminInsufficientPower`. This is the actual semantics
/// the test name implies — distinct from the membership-gate rejection
/// (`ActorNotJoined`) that an unjoined actor would hit.
///
/// Single-engine test — no forwarder. Bob's engine is OOB-pre-seeded
/// with Alice's bootstrap Join + Bob's own Join so `prior_state` has
/// Bob as Joined with power 0.
#[tokio::test]
async fn joined_sub_mod_member_channel_create_rejected_with_channel_admin_insufficient_power() {
    let alice = mint_test_owner(0xAA);
    let bob = mint_test_owner(0xBB);
    let alice_addr = alice.owner;
    let bob_addr = bob.owner;
    let alice_sk = Arc::new(alice.device_key.clone());
    let bob_sk = Arc::new(bob.device_key.clone());

    // ZEB-339: signer resolution uses the carried EnrollmentCert (Join) /
    // materialized enrolled keys (steady-state), not the resolver — so the
    // resolver's identity_pubs are unused for membership verify here.
    let resolver: Arc<dyn IdentityResolver> = Arc::new(TwoIdentityResolver {
        a: (alice_addr, [0u8; 64]),
        b: (bob_addr, [0u8; 64]),
    });

    let cas_op_tx = spawn_shared_cas();
    let cs_b: Arc<dyn ContentStore> =
        Arc::new(RuntimeContentStore::new(cas_op_tx, Duration::from_secs(2)));
    let dir_b = tempfile::tempdir().expect("dir b");

    let community_id = SpaceId([0x38; 16]);
    let membership_key = EpochKey::new([0x66; 32]);

    let registry_b = Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_id: "bob-dev".into(),
        content_store: Arc::clone(&cs_b),
        identity_resolver: Arc::clone(&resolver),
        identity_dir: dir_b.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: None,
        self_owner: bob_addr,
        signing_key: Arc::clone(&bob_sk),
        crdt_state: None,
        nav_emitter: None,
        presence_resync_rx: None,
    }));

    // Bob's wire channels are unwired — no peers in this test. The
    // `subscriber_rx` is held by the engine; the `publisher_rx` is held
    // here so Bob's engine doesn't latch off `transport_closed`. Neither
    // matters because the local-reject path returns BEFORE any publish
    // happens.
    let (bob_pub_tx, _bob_pub_rx_held) = mpsc::channel::<Vec<u8>>(64);
    let (_bob_sub_tx_held, bob_sub_rx) = mpsc::channel::<Vec<u8>>(64);

    registry_b
        .spawn_engine_inner_now(
            community_id,
            membership_key,
            alice_addr,
            false,
            bob_pub_tx,
            bob_sub_rx,
            harmony_app::community_state_sync::CatchUpChannels::none(),
        )
        .await
        .expect("spawn bob engine");
    let bob_engine = registry_b
        .engine_arc(&community_id)
        .await
        .expect("bob engine");

    // Pre-seed Bob's engine with Alice's bootstrap Join (admin → power 100)
    // — same OOB pattern as test 1's cold-cache simulation. Without this,
    // Bob's prior_state has no admin and his own Join below would fail
    // verify (admin's pubkey is required for Join sig binding via
    // identity_resolver, but more importantly Bob's verify_event needs
    // an admin in the community context).
    let alice_join_at = Hlc {
        wall_ms: 100_000,
        logical: 0,
        device_id: "alice-dev".into(),
    };
    let alice_join_payload = EventPayload {
        id: [0x10; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: alice_addr,
        at: alice_join_at,
    };
    let alice_join = with_join_cert(
        sign_event(&alice_join_payload, alice_sk.as_ref()).expect("sign alice join"),
        &alice,
    );
    let outcome = bob_engine
        .insert_local_event(alice_join)
        .await
        .expect("bob OOB-seeds Alice's bootstrap Join");
    assert_eq!(outcome, InsertOutcome::Inserted);

    // Bob signs + inserts his own Join. Open community → no countersig
    // required. After this, Bob's prior_state has Bob as Joined with
    // default power 0 (well below POWER_THRESHOLDS.kick = 50).
    let bob_join_payload = EventPayload {
        id: [0x20; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: bob_addr,
        at: Hlc {
            wall_ms: 150_000,
            logical: 0,
            device_id: "bob-dev".into(),
        },
    };
    let bob_join = with_join_cert(
        sign_event(&bob_join_payload, bob_sk.as_ref()).expect("sign bob join"),
        &bob,
    );
    let outcome = bob_engine
        .insert_local_event(bob_join)
        .await
        .expect("bob join insert");
    assert_eq!(outcome, InsertOutcome::Inserted);

    // Bob (Joined, power 0) attempts ChannelCreate. verify_event step 4
    // passes (he IS Joined); step 5's power gate fires:
    // ChannelAdminInsufficientPower (NOT ActorNotJoined).
    let bob_create_payload = EventPayload {
        id: [0x21; 16],
        community_id,
        kind: MembershipEventKind::ChannelCreate {
            channel_id: ChannelId([0x77; 16]),
            name: "spam".into(),
            write_power: 0,
            kind: ChannelKind::Text,
        },
        actor: bob_addr,
        at: Hlc {
            wall_ms: 200_000,
            logical: 0,
            device_id: "bob-dev".into(),
        },
    };
    let bob_create = sign_event(&bob_create_payload, bob_sk.as_ref()).expect("sign create");

    let outcome = bob_engine
        .insert_local_event(bob_create)
        .await
        .expect("insert call (verify-rejection is `Ok(Rejected)`, not `Err`)");
    assert!(
        matches!(
            outcome,
            InsertOutcome::Rejected(VerifyError::ChannelAdminInsufficientPower)
        ),
        "expected ChannelAdminInsufficientPower rejection (mod-power gate), got {outcome:?}"
    );
}

/// Test 3 — synthesizes Task 7's `create_community_inner` pattern: Alice
/// signs `bootstrap_join` followed immediately by a default-`#general`
/// ChannelCreate at HLC `(bootstrap.wall, bootstrap.logical+1)`. Both
/// events round-trip through the state-CRDT to Bob, whose materialized
/// state shows Alice as Joined AND the `general` channel present.
#[tokio::test]
async fn default_general_channel_round_trips_through_state_sync() {
    let alice = mint_test_owner(0xCC);
    let bob = mint_test_owner(0xDD);
    let alice_addr = alice.owner;
    let bob_addr = bob.owner;
    let alice_sk = Arc::new(alice.device_key.clone());
    let bob_sk = Arc::new(bob.device_key.clone());

    // ZEB-339: signer resolution uses the carried EnrollmentCert / materialized
    // enrolled keys, not the resolver — resolver pubs unused for membership.
    let resolver: Arc<dyn IdentityResolver> = Arc::new(TwoIdentityResolver {
        a: (alice_addr, [0u8; 64]),
        b: (bob_addr, [0u8; 64]),
    });

    let cas_op_tx = spawn_shared_cas();
    let cs_a: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx.clone(),
        Duration::from_secs(2),
    ));
    let cs_b: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx.clone(),
        Duration::from_secs(2),
    ));
    let dir_a = tempfile::tempdir().expect("dir a");
    let dir_b = tempfile::tempdir().expect("dir b");

    let community_id = SpaceId([0x39; 16]);
    let membership_key = EpochKey::new([0x77; 32]);

    let (delta_a_tx, mut delta_a_rx) = mpsc::channel::<CommunityMembershipDelta>(32);
    let (delta_b_tx, mut delta_b_rx) = mpsc::channel::<CommunityMembershipDelta>(32);

    let registry_a = Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_id: "alice-dev".into(),
        content_store: Arc::clone(&cs_a),
        identity_resolver: Arc::clone(&resolver),
        identity_dir: dir_a.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: Some(delta_a_tx),
        self_owner: alice_addr,
        signing_key: Arc::clone(&alice_sk),
        crdt_state: None,
        nav_emitter: None,
        presence_resync_rx: None,
    }));
    let registry_b = Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_id: "bob-dev".into(),
        content_store: Arc::clone(&cs_b),
        identity_resolver: Arc::clone(&resolver),
        identity_dir: dir_b.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: Some(delta_b_tx),
        self_owner: bob_addr,
        signing_key: Arc::clone(&bob_sk),
        crdt_state: None,
        nav_emitter: None,
        presence_resync_rx: None,
    }));

    let (alice_pub_tx, alice_pub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (alice_sub_tx, alice_sub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (bob_pub_tx, bob_pub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (bob_sub_tx, bob_sub_rx) = mpsc::channel::<Vec<u8>>(64);
    spawn_forwarder(alice_pub_rx, bob_sub_tx, bob_pub_rx, alice_sub_tx);

    registry_a
        .spawn_engine_inner_now(
            community_id,
            membership_key.clone(),
            alice_addr,
            false,
            alice_pub_tx,
            alice_sub_rx,
            harmony_app::community_state_sync::CatchUpChannels::none(),
        )
        .await
        .expect("spawn alice engine");
    registry_b
        .spawn_engine_inner_now(
            community_id,
            membership_key,
            alice_addr,
            false,
            bob_pub_tx,
            bob_sub_rx,
            harmony_app::community_state_sync::CatchUpChannels::none(),
        )
        .await
        .expect("spawn bob engine");

    let alice_engine = registry_a
        .engine_arc(&community_id)
        .await
        .expect("alice engine");
    let bob_engine = registry_b
        .engine_arc(&community_id)
        .await
        .expect("bob engine");

    // Alice's bootstrap_join.
    let alice_join_at = Hlc {
        wall_ms: 300_000,
        logical: 0,
        device_id: "alice-dev".into(),
    };
    let alice_join_payload = EventPayload {
        id: [0x30; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: alice_addr,
        at: alice_join_at.clone(),
    };
    let alice_join = with_join_cert(
        sign_event(&alice_join_payload, alice_sk.as_ref()).expect("sign join"),
        &alice,
    );
    let outcome = alice_engine
        .insert_local_event(alice_join.clone())
        .await
        .expect("alice bootstrap insert");
    assert_eq!(outcome, InsertOutcome::Inserted);
    let _alice_own_join_delta = drain_until(
        &mut delta_a_rx,
        |d| matches!(d.event.kind, MembershipEventKind::Join),
        "alice own Join delta",
    )
    .await;

    // ZEB-256 Task 6 cold-cache simulation: OOB-seed Alice's bootstrap
    // Join into Bob's engine so the membership-at-HLC gate on the
    // receive side admits Alice's subsequent publishes. Mirrors the
    // canonical pre-seed in `community_open_flow_integration.rs`.
    bob_engine
        .insert_local_event(alice_join)
        .await
        .expect("bob OOB-seeds Alice's bootstrap Join");
    let _bob_seeded_alice_join_delta = drain_until(
        &mut delta_b_rx,
        |d| matches!(d.event.kind, MembershipEventKind::Join) && d.event.actor == alice_addr,
        "bob delta from OOB-seeded Alice Join",
    )
    .await;

    // Default-#general ChannelCreate, mirroring Task 7's HLC ordering:
    // `at = (bootstrap.wall_ms, bootstrap.logical + 1, bootstrap.device_id)`
    // — keeps Join < ChannelCreate deterministic without a wall-clock tick.
    let default_ch_id = ChannelId([0x55; 16]);
    let default_create_at = Hlc {
        wall_ms: alice_join_at.wall_ms,
        logical: alice_join_at.logical + 1,
        device_id: alice_join_at.device_id.clone(),
    };
    let default_create_payload = EventPayload {
        id: [0x31; 16],
        community_id,
        kind: MembershipEventKind::ChannelCreate {
            channel_id: default_ch_id,
            name: "general".into(),
            write_power: 0,
            kind: ChannelKind::Text,
        },
        actor: alice_addr,
        at: default_create_at,
    };
    let default_create =
        sign_event(&default_create_payload, alice_sk.as_ref()).expect("sign default channel");
    let outcome = alice_engine
        .insert_local_event(default_create)
        .await
        .expect("alice default-channel insert");
    assert_eq!(outcome, InsertOutcome::Inserted);
    let _alice_own_channel_create_delta = drain_until(
        &mut delta_a_rx,
        |d| matches!(d.event.kind, MembershipEventKind::ChannelCreate { .. }),
        "alice own ChannelCreate delta",
    )
    .await;

    // Bob receives the ChannelCreate via the forwarder. (Alice's Join
    // was OOB-seeded earlier to satisfy the membership-at-HLC gate.)
    let _bob_received_alice_channel_create_delta = drain_until(
        &mut delta_b_rx,
        |d| matches!(d.event.kind, MembershipEventKind::ChannelCreate { .. }),
        "bob received Alice's default ChannelCreate",
    )
    .await;

    let bob_state = registry_b
        .state_for(&community_id)
        .await
        .expect("bob state");
    let bob_materialized = {
        let g = bob_state.lock().await;
        g.materialize_now(alice_addr)
    };

    // Alice is Joined.
    let alice_member = bob_materialized
        .members
        .get(&alice_addr)
        .expect("Alice in Bob's materialized members");
    assert_eq!(
        alice_member.status,
        harmony_app::community_membership::MemberStatus::Joined
    );

    // Default channel is present and live.
    let info = bob_materialized
        .channels
        .get(&default_ch_id)
        .expect("Bob has default channel");
    assert_eq!(info.name, "general");
    assert_eq!(info.write_power, 0);
    assert!(info.deleted_at.is_none());
}
