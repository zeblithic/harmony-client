//! ZEB-911 slice 2 (e2e): the witness discovery ladder in
//! `connectivity_redeem_invite_iroh_inner`.
//!
//! Slice 1 (`community_misc/zeb911_witness_acceptance.rs`) pins the acceptance
//! half — any Joined member's `handle_unicast` accepts a redeem packet it did
//! not mint. These tests pin the DISCOVERY half against real loopback iroh
//! endpoints and a real (mock-relayed) pkarr round trip:
//!
//!   * `zeb911_witness_redeem_admin_offline` — the admin publishes NO Case-A
//!     record; a Joined non-admin member publishes a community-rendezvous slot
//!     record pointing at its live endpoint; the joiner redeems and lands
//!     `joined`, counter-signed by the WITNESS.
//!   * `zeb911_ladder_exhausted_no_member_reachable` — same discovery, but the
//!     slot record points at a shut-down endpoint. The ladder dials and fails,
//!     so the outcome is the new `no_member_reachable`, not the legacy
//!     single-target `inviter_unreachable`.
//!   * `zeb911_no_records_at_all_stays_inviter_unreachable` — no admin record
//!     and no slot records: ZERO witness dials, so the outcome mapping must
//!     still be `inviter_unreachable` (regression guard on the
//!     `witness_dials_attempted > 0` discriminator).
//!
//! Three parties. The two-party harness in the sibling
//! `pkarr_iroh_redeem_full_integration.rs` supplies Alice (the admin who mints
//! the invite) and Bob (the joiner) plus the mock pkarr relay / publisher /
//! resolver; this file adds the third — a witness node with its own iroh
//! endpoint, `IrohInviteHandshakeAcceptor`, and engine whose CRDT carries the
//! admin bootstrap plus the witness's own completed admission.
//!
//! Alice's Case-A record is simply never registered (`invite_pub.register_invite`
//! is the only thing that publishes it), so rung 0 resolves nothing and falls
//! through the `'rung0:` block — the "admin has been offline past the record's
//! freshness window" scenario ZEB-911 exists for. Her endpoint stays bound but
//! is undiscoverable: the ladder reaches peers only via pkarr.

use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::{Signer, SigningKey};
use harmony_app::community_membership::{
    materialize, mint_test_owner, sign_event, EventId, EventPayload, MemberStatus,
    MembershipEventKind, SignedMembershipEvent, TestOwner,
};
use harmony_app::community_rendezvous::{rendezvous_slot_verifying_key, RENDEZVOUS_SLOT_COUNT};
use harmony_app::community_rendezvous_publisher::CommunityRendezvousPublisher;
use harmony_app::community_state_crdt::InsertOutcome;
use harmony_app::community_state_sync::{
    CatchUpChannels, CommunityRegistryConfig, CommunitySyncRegistry, IdentityResolver,
    DEFAULT_DEBOUNCE_MS,
};
use harmony_app::content_store::{ContentStore, RuntimeContentStore};
use harmony_app::dm_outbox::DmOutbox;
use harmony_app::iroh_endpoint::IrohEndpoint;
use harmony_app::iroh_invite_acceptor::IrohInviteHandshakeAcceptor;
use harmony_app::owner_state_crdt::OwnerState;
use harmony_app::owner_state_types::{DeviceIdentityHash, EpochKey, Hlc, OwnerAddr, SpaceId};
use harmony_app::reachability_record::ReachabilityAnnouncePayload;
use harmony_app::reachability_resolver::ReachabilityResolver;
use harmony_app::zenoh_iroh_transport::IrohZenohLinkManager;
use harmony_identity::PrivateIdentity;
use tokio::sync::{mpsc, Mutex as TokioMutex};
use zenoh_link::LinkUnicast;

use crate::pkarr_iroh_redeem_full_integration::{
    build_hermetic_endpoint, default_acceptor_config, derive_composite_owner,
    setup_two_party_iroh_handshake_with_config, signing_key_from, spawn_shared_cas,
    zeb889_build_targeted_invite, TwoPartySetup,
};

/// ZEB-339 authenticates membership actors via their carried `EnrollmentCert`
/// or their materialized `enrolled_device_keys`, so the witness registry never
/// needs a populated owner→identity cache. The registry still requires *a*
/// resolver; this one always misses.
struct NoIdentityResolver;

#[async_trait::async_trait]
impl IdentityResolver for NoIdentityResolver {
    async fn resolve(&self, _addr: &OwnerAddr) -> Option<[u8; 64]> {
        None
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Sign an `InviteToken` with `signer_key` over the canonical token bytes
/// (ZEB-339: always an enrolled *device* key, never an identity key).
fn signed_token(
    signer_key: &SigningKey,
    inviter: OwnerAddr,
    invitee_hint: Option<OwnerAddr>,
    minted_wall_ms: u64,
) -> harmony_app::community_invite::InviteToken {
    let mut tok = harmony_app::community_invite::InviteToken {
        inviter,
        invitee_hint,
        minted_at: Hlc {
            wall_ms: minted_wall_ms,
            logical: 0,
            device_id: "alice-dev".into(),
        },
        expires_at: None,
        sig: [0u8; 64],
    };
    let bytes = harmony_app::community_invite::canonical_invite_token_bytes(&tok)
        .expect("canonical token bytes");
    tok.sig = signer_key.sign(&bytes).to_bytes();
    tok
}

/// `PendingJoin` signed by `owner`'s enrolled device key and carrying its
/// Master `EnrollmentCert` (the identity-introducing event shape).
fn pending_join_event(
    owner: &TestOwner,
    community_id: SpaceId,
    invite_token: harmony_app::community_invite::InviteToken,
    id: EventId,
    wall_ms: u64,
    device_id: &str,
) -> SignedMembershipEvent {
    let payload = EventPayload {
        id,
        community_id,
        kind: MembershipEventKind::PendingJoin { invite_token },
        actor: owner.owner,
        at: Hlc {
            wall_ms,
            logical: 0,
            device_id: device_id.into(),
        },
    };
    let mut ev = sign_event(&payload, &owner.device_key).expect("sign PendingJoin");
    ev.enrollment = Some(owner.cert.clone());
    ev
}

/// `JoinCountersign` from `signer` targeting `target_event_id`. Steady-state
/// event: no cert attached — the actor's enrolled device key is already in the
/// materialized membership by the time this verifies.
fn countersign_event(
    signer_owner: OwnerAddr,
    signer_key: &SigningKey,
    community_id: SpaceId,
    target_event_id: EventId,
    id: EventId,
    wall_ms: u64,
    device_id: &str,
) -> SignedMembershipEvent {
    let payload = EventPayload {
        id,
        community_id,
        kind: MembershipEventKind::JoinCountersign { target_event_id },
        actor: signer_owner,
        at: Hlc {
            wall_ms,
            logical: 0,
            device_id: device_id.into(),
        },
    };
    sign_event(&payload, signer_key).expect("sign JoinCountersign")
}

/// A `DmOutbox` whose only field `handle_unicast` reads is `self_owner`
/// (ZEB-911 dropped the community-signing-key snapshot from that path).
/// `new_synthetic` bypasses the owner_id debug_assert in `DmOutbox::new` —
/// these addresses are community owner_ids, not Reticulum-derived.
fn outbox_for(self_owner: OwnerAddr, seed: u8) -> Arc<TokioMutex<DmOutbox>> {
    let owner = mint_test_owner(seed);
    let device_key = Arc::new(owner.device_key.clone());
    let transport = PrivateIdentity::from_seed(&[seed; 32]);
    Arc::new(TokioMutex::new(DmOutbox::new_synthetic(
        "witness-dev".into(),
        self_owner,
        DeviceIdentityHash(transport.identity.address_hash),
        Arc::clone(&device_key),
        Arc::new(transport),
        device_key,
        owner.cert,
    )))
}

/// The third party: a Joined, non-admin, power-0 member running a real
/// loopback iroh endpoint with the production handshake acceptor installed.
struct WitnessNode {
    comm: TestOwner,
    ep: Arc<IrohEndpoint>,
    registry: Arc<CommunitySyncRegistry>,
    /// Routing payload advertised in the rendezvous slot record.
    routing: ReachabilityAnnouncePayload,
    transport_sk: SigningKey,
    transport_pub: [u8; 64],
    // Keepalives: dropping the accept loop kills inbound dials; dropping the
    // subscriber sender latches the engine's `inbound_closed`; the tempdir
    // backs the registry's persist paths.
    _accept: tokio::task::JoinHandle<()>,
    _sub_tx: mpsc::Sender<Vec<u8>>,
    _dir: tempfile::TempDir,
}

/// Stand up the witness node against the community Alice minted in the shared
/// two-party harness: its CRDT is admin bootstrap + the witness's own
/// `PendingJoin` (admin-minted token) + the admin's `JoinCountersign`, i.e.
/// exactly the roster a real member replicates before the admin goes offline.
async fn spawn_witness_node(s: &TwoPartySetup) -> WitnessNode {
    let now = now_ms();
    let comm = mint_test_owner(0xC3);
    assert_ne!(
        comm.owner, s.alice_addr,
        "fixture precondition: the witness must not be the admin"
    );
    let transport_identity = PrivateIdentity::from_seed(&[0xc3; 32]);
    let transport_sk = signing_key_from(&transport_identity);
    let (_transport_addr, transport_pub) = derive_composite_owner(&transport_sk);

    // ── iroh endpoint + link manager + accept loop. ─────────────────────
    let ep = build_hermetic_endpoint().await;
    let bound = ep.bound_sockets();
    assert!(
        !bound.is_empty(),
        "the witness's hermetic endpoint must expose bound_sockets() so the \
         rendezvous record advertises a dialable loopback target"
    );
    let reachability = ReachabilityResolver::new();
    let (link_tx, _link_rx) = flume::unbounded::<LinkUnicast>();
    let link_mgr = Arc::new(IrohZenohLinkManager::new(
        Arc::clone(&ep),
        reachability,
        link_tx,
    ));
    let accept = link_mgr.spawn_accept_loop();

    // ── Engine + CRDT. ──────────────────────────────────────────────────
    let dir = tempfile::tempdir().expect("witness tempdir");
    let content_store: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        spawn_shared_cas(),
        Duration::from_secs(2),
    ));
    let registry = Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
        device_id: "witness-dev".into(),
        content_store,
        identity_resolver: Arc::new(NoIdentityResolver),
        identity_dir: dir.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: None,
        self_owner: comm.owner,
        // The auto-counter-sign hook signs with the registry's key, so the
        // countersign the joiner receives is authored by the WITNESS.
        signing_key: Arc::new(comm.device_key.clone()),
        crdt_state: None,
        nav_emitter: None,
        presence_resync_rx: None,
    }));

    let (pub_tx, mut pub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (sub_tx, sub_rx) = mpsc::channel::<Vec<u8>>(64);
    // Drain the publish side so a debounce flush can never wedge on backpressure.
    tokio::spawn(async move { while pub_rx.recv().await.is_some() {} });
    registry
        .spawn_engine_inner_now(
            s.community_id,
            s.alice_minted.membership_key.clone(),
            s.alice_addr,
            true,
            pub_tx,
            sub_rx,
            CatchUpChannels::none(),
        )
        .await
        .expect("spawn witness engine");
    let engine = registry
        .engine_arc(&s.community_id)
        .await
        .expect("witness engine arc");

    // 1. Admin's bootstrap Join — the trust root, and the source of the admin's
    //    enrolled device key that the acceptor's token-sig check resolves through.
    let outcome = engine
        .insert_local_event(s.alice_minted.bootstrap_join.clone())
        .await
        .expect("admin bootstrap insert");
    assert!(
        matches!(outcome, InsertOutcome::Inserted),
        "admin bootstrap must be Inserted; got {outcome:?}"
    );

    // 2 + 3. The witness's own admission.
    let witness_pending = pending_join_event(
        &comm,
        s.community_id,
        signed_token(
            &s.alice_comm_sk,
            s.alice_addr,
            Some(comm.owner),
            now - 58_000,
        ),
        [0x71_u8; 16],
        now - 50_000,
        "witness-dev",
    );
    let witness_pending_id = witness_pending.id;
    let outcome = engine
        .insert_local_event(witness_pending)
        .await
        .expect("witness PendingJoin insert");
    assert!(
        matches!(outcome, InsertOutcome::Inserted),
        "witness PendingJoin must be Inserted; got {outcome:?}"
    );
    let admin_cs = countersign_event(
        s.alice_addr,
        &s.alice_comm_sk,
        s.community_id,
        witness_pending_id,
        [0xC1_u8; 16],
        now - 40_000,
        "alice-dev",
    );
    let outcome = engine
        .insert_local_event(admin_cs)
        .await
        .expect("admin JoinCountersign insert");
    assert!(
        matches!(outcome, InsertOutcome::Inserted),
        "admin JoinCountersign must be Inserted; got {outcome:?}"
    );

    // Precondition for every test below: the witness is Joined and power-0 —
    // exactly the role ZEB-911 newly empowers.
    let state = registry
        .state_for(&s.community_id)
        .await
        .expect("witness state");
    let events: Vec<_> = {
        let g = state.lock().await;
        g.events().cloned().collect()
    };
    let mat = materialize(&events, s.alice_addr);
    assert_eq!(
        mat.members.get(&comm.owner).map(|m| m.status),
        Some(MemberStatus::Joined),
        "fixture precondition: the witness must materialize as Joined"
    );
    assert_eq!(
        mat.power_levels.get(&comm.owner).copied().unwrap_or(0),
        0,
        "fixture precondition: the witness is a plain power-0 member"
    );

    // ── Production handshake acceptor. ──────────────────────────────────
    // `None` for the case-A invite publisher: the witness never published the
    // invite, so there is nothing for it to unregister on consume (ZEB-874's
    // burn is deliberately acceptor-local, spec §3.3).
    let acceptor: Arc<IrohInviteHandshakeAcceptor<()>> =
        Arc::new(IrohInviteHandshakeAcceptor::<()>::with_config(
            Arc::clone(&registry),
            outbox_for(comm.owner, 0xC3),
            Arc::new(TokioMutex::new(OwnerState::default())),
            None,
            None,
            default_acceptor_config(),
        ));
    if link_mgr
        .install_handshake_dispatcher(acceptor)
        .await
        .is_err()
    {
        panic!("first install must succeed (OnceCell empty)");
    }

    let routing = ReachabilityAnnouncePayload {
        iroh_node_id: *ep.node_id().as_bytes(),
        home_relay_url: ep.home_relay().map(|r| r.to_string()).unwrap_or_default(),
        direct_addresses: bound,
        announced_at_ms: now,
        identity_signature: [0xCDu8; 64],
        butler_set: Vec::new(),
        bs_at: 0,
    };

    WitnessNode {
        comm,
        ep,
        registry,
        routing,
        transport_sk,
        transport_pub,
        _accept: accept,
        _sub_tx: sub_tx,
        _dir: dir,
    }
}

/// Publish `routing` into the mock pkarr relay under the community's
/// rendezvous slot 0, through the PRODUCTION publisher — `refresh_slot` derives
/// the slot key, wraps the payload with a membership vouch, and signs the outer
/// record exactly as a live advertiser does. Returns once the record is
/// resolvable, so the ladder can never race the first PUT.
///
/// Advertiser set = `[witness_owner]`, so `slot_for_advertiser` ranks it slot 0.
#[allow(clippy::too_many_arguments)]
async fn publish_rendezvous_slot(
    s: &TwoPartySetup,
    epoch_key: &EpochKey,
    witness_owner: OwnerAddr,
    witness_device_sk: Arc<SigningKey>,
    identity_sk: SigningKey,
    identity_pub: [u8; 64],
    routing: ReachabilityAnnouncePayload,
) -> Arc<CommunityRendezvousPublisher> {
    let blob = {
        let mut buf = Vec::new();
        ciborium::into_writer(&routing, &mut buf).expect("encode witness routing_blob");
        buf
    };
    let publisher = Arc::new(CommunityRendezvousPublisher::new(
        Arc::clone(&s.pkarr_publisher),
        identity_sk,
        identity_pub,
        witness_device_sk,
        Arc::new(move || blob.clone()),
    ));
    publisher
        .refresh_slot(
            s.community_id,
            epoch_key.clone(),
            vec![witness_owner],
            witness_owner,
        )
        .await;

    // Probe the whole epoch tolerance window (review r1): deriving from a
    // single `current_epoch_id` snapshot races an epoch-boundary crossing
    // between `refresh_slot`'s publication and this derivation. The redeem
    // path itself resolves across the window, so the readiness probe must too.
    let mut visible = false;
    'probe: for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        for epoch_id in harmony_pkarr::epoch_tolerance_window(now_ms()) {
            let slot_vk = rendezvous_slot_verifying_key(epoch_key, 0, epoch_id);
            if let Ok(Some(_)) = s.pkarr_resolver.resolve(&slot_vk).await {
                visible = true;
                break 'probe;
            }
        }
    }
    assert!(
        visible,
        "the witness's rendezvous slot-0 record must appear in the mock relay within \
         5s before driving the joiner's redeem"
    );
    publisher
}

/// Assert no admin Case-A record exists for `token_sig` in the current epoch
/// window. This is the load-bearing precondition of every test here: rung 0
/// must fall through, or a "joined" outcome would prove nothing about the
/// witness ladder.
async fn assert_no_admin_case_a_record(s: &TwoPartySetup, token_sig: &[u8; 64]) {
    for epoch_id in harmony_pkarr::epoch_tolerance_window(now_ms()) {
        let probe = harmony_pkarr::derive_ephemeral_key(
            harmony_pkarr::PkarrCase::Invite,
            token_sig,
            &epoch_id.to_be_bytes(),
        )
        .verifying_key();
        assert!(
            matches!(s.pkarr_resolver.resolve(&probe).await, Ok(None)),
            "the admin must have published NO case-A record (epoch {epoch_id}) — \
             rung 0 has to fall through for the witness ladder to be under test"
        );
    }
    assert!(
        !s.pkarr_publisher
            .active_handles()
            .await
            .contains(&format!("invite:{}", hex::encode(token_sig))),
        "the admin's case-A invite publication must never have been registered"
    );
}

/// Short dial budgets: rung 0 is skipped in every test here, so this only bounds
/// the witness dials — including test 2's dial at a shut-down endpoint, which
/// must fail well inside the outer 60s timeout.
fn witness_dial_config(connect_timeout: Duration) -> harmony_app::HandshakeDialConfig {
    harmony_app::HandshakeDialConfig {
        connect_timeout,
        open_bi_timeout: Duration::from_millis(10_000),
        response_read_timeout: Duration::from_millis(10_000),
        write_timeout: Duration::from_millis(10_000),
    }
}

/// Drive the joiner's production redeem IPC against the mock resolver.
async fn drive_joiner_redeem(
    s: &TwoPartySetup,
    invite_url: String,
    dial_config: harmony_app::HandshakeDialConfig,
) -> harmony_app::RedemptionOutcome {
    harmony_app::connectivity_redeem_invite_iroh_inner(
        invite_url,
        Some(Arc::clone(&s.pkarr_resolver)),
        Some(s.bob_reachability.clone()),
        Some(Arc::clone(&s.bob_ep)),
        Arc::clone(&s.bob_crdt_state),
        Arc::clone(&s.bob_hlc_tracker),
        s.bob_adopt_floor.clone(),
        "bob-dev".to_string(),
        s.bob_addr,
        Arc::clone(&s.bob_comm_sk),
        s.bob_comm.cert.clone(),
        Arc::clone(&s.registry_bob),
        s.bob_adapter_tx.clone(),
        None,
        Arc::clone(&s.bob_dm_outbox),
        Arc::clone(&s.bob_channel_log_registry),
        None,
        None,
        |_| {},
        |_payload: harmony_app::NavUpdatedPayload| {},
        dial_config,
        || Ok(()),
    )
    .await
    .expect("connectivity_redeem_invite_iroh_inner must Ok (it converts internal errors into outcome.status)")
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("harmony_app=warn")),
        )
        .with_test_writer()
        .try_init();
}

// ────────────────────────────────────────────────────────────────────────────
// 1. Admin offline, witness reachable → joined, counter-signed by the witness.
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zeb911_witness_redeem_admin_offline() {
    init_tracing();
    // Pre-pay iroh's first-bind global init OUTSIDE the budget (ZEB-347 pattern)
    // so full-suite contention can't charge it against the handshake.
    harmony_app::iroh_endpoint::warm_up_iroh_global_init().await;

    tokio::time::timeout(Duration::from_secs(90), async {
        let s = setup_two_party_iroh_handshake_with_config(default_acceptor_config()).await;
        let witness = spawn_witness_node(&s).await;

        // The admin mints the invite (she is the only valid minter — spec §2.1
        // leaves P2 untouched) but never registers her case-A record.
        let (_payload, invite_url, token_sig) = zeb889_build_targeted_invite(&s);
        assert_no_admin_case_a_record(&s, &token_sig).await;

        // The slot record derives from the SAME epoch key the invite seals, so
        // the joiner can derive the slot keys with nothing but the invite.
        let _rdv = publish_rendezvous_slot(
            &s,
            &s.alice_minted.membership_key,
            witness.comm.owner,
            Arc::new(witness.comm.device_key.clone()),
            witness.transport_sk.clone(),
            witness.transport_pub,
            witness.routing.clone(),
        )
        .await;

        let outcome =
            drive_joiner_redeem(&s, invite_url, witness_dial_config(Duration::from_secs(10))).await;

        assert_eq!(
            outcome.status, "joined",
            "the joiner must reach 'joined' through the WITNESS with the admin's \
             case-A record absent — got status={:?} community_id={:?}",
            outcome.status, outcome.community_id
        );
        assert_eq!(
            outcome.community_id.as_deref(),
            Some(hex::encode(s.community_id.0).as_str()),
            "community_id must echo the admin's invite"
        );

        // The witness's own engine committed the joiner's PendingJoin and
        // materializes him as Joined — the acceptance half, observed from the
        // witness rather than from the joiner.
        let witness_state = witness
            .registry
            .state_for(&s.community_id)
            .await
            .expect("witness state after redeem");
        let witness_events: Vec<SignedMembershipEvent> = {
            let g = witness_state.lock().await;
            g.events().cloned().collect()
        };
        let witness_mat = materialize(&witness_events, s.alice_addr);
        assert_eq!(
            witness_mat.members.get(&s.bob_addr).map(|m| m.status),
            Some(MemberStatus::Joined),
            "the witness's engine must materialize the joiner as Joined"
        );

        // The countersign that admitted the joiner was authored by the WITNESS,
        // not the admin — the whole point of slice 1 + slice 2 together.
        let joiner_pending_id = witness_events
            .iter()
            .find(|e| {
                e.actor == s.bob_addr && matches!(&e.kind, MembershipEventKind::PendingJoin { .. })
            })
            .map(|e| e.id)
            .expect("the witness must have committed the joiner's PendingJoin");
        let countersigners: Vec<OwnerAddr> = witness_events
            .iter()
            .filter(|e| {
                matches!(
                    &e.kind,
                    MembershipEventKind::JoinCountersign { target_event_id }
                        if *target_event_id == joiner_pending_id
                )
            })
            .map(|e| e.actor)
            .collect();
        assert_eq!(
            countersigners,
            vec![witness.comm.owner],
            "the JoinCountersign admitting the joiner must be authored by the WITNESS \
             (the admin never saw this redeem)"
        );

        // Joiner side: the countersign it applied is the witness's.
        let bob_state = s
            .registry_bob
            .state_for(&s.community_id)
            .await
            .expect("joiner state after redeem");
        let bob_events: Vec<SignedMembershipEvent> = {
            let g = bob_state.lock().await;
            g.events().cloned().collect()
        };
        // ZEB-911 chain delivery: the joiner's log carries the witness's
        // countersign for the JOINER's PendingJoin and the ADMIN's for the
        // WITNESS's own admission (both ride the handshake response so the
        // witness's countersign is verifiable on a fresh CRDT). Assert by target,
        // not globally.
        //
        // ZEB-927: the response now ships the FULL roster, which can give the
        // fresh joiner enough context to run membership reconciliation and
        // reflexively countersign its OWN pending. That self-vouch is benign
        // (verify_event's JoinCountersignActorNotJoined gate means it only ever
        // lands POST-admission, so it can never bootstrap a join) AND
        // timing-dependent — it appears under full-suite contention (CI) but not
        // in a fast isolated run. So filter the joiner's own actor and assert the
        // EXTERNAL admitter: the property this guards is that the joiner was
        // admitted by the WITNESS, not the offline admin — which the racy
        // self-vouch does not touch.
        let bob_pending_id = bob_events
            .iter()
            .find(|e| {
                e.actor == s.bob_addr && matches!(&e.kind, MembershipEventKind::PendingJoin { .. })
            })
            .map(|e| e.id)
            .expect("joiner's own PendingJoin in his CRDT");
        let joiner_countersigners: Vec<OwnerAddr> = bob_events
            .iter()
            .filter(|e| {
                matches!(
                    &e.kind,
                    MembershipEventKind::JoinCountersign { target_event_id }
                    if *target_event_id == bob_pending_id
                ) && e.actor != s.bob_addr
            })
            .map(|e| e.actor)
            .collect();
        assert_eq!(
            joiner_countersigners,
            vec![witness.comm.owner],
            "the EXTERNAL countersign admitting the JOINER must be the WITNESS's, and only the \
             witness's (the admin was offline — a stray admin countersign here would be the \
             regression this guards; the joiner's own benign self-vouch is filtered above)"
        );
        let witness_pending_id = bob_events
            .iter()
            .find(|e| {
                e.actor == witness.comm.owner
                    && matches!(&e.kind, MembershipEventKind::PendingJoin { .. })
            })
            .map(|e| e.id)
            .expect("the witness's own admission PendingJoin must ride the chain response");
        assert!(
            bob_events.iter().any(|e| {
                e.actor == s.alice_addr
                    && matches!(
                        &e.kind,
                        MembershipEventKind::JoinCountersign { target_event_id }
                        if *target_event_id == witness_pending_id
                    )
            }),
            "the admin's countersign for the WITNESS's admission must ride the chain response"
        );
        assert_eq!(
            materialize(&bob_events, s.alice_addr)
                .members
                .get(&s.bob_addr)
                .map(|m| m.status),
            Some(MemberStatus::Joined),
            "the joiner must materialize himself as Joined"
        );

        s.publisher_handle.abort();
        witness.ep.shutdown().await;
        s.alice_ep.shutdown().await;
        s.bob_ep.shutdown().await;
    })
    .await
    .expect("zeb911_witness_redeem_admin_offline timed out at 90s");
}

// ────────────────────────────────────────────────────────────────────────────
// 2. Ladder exhausted (candidate resolved, dial fails) → no_member_reachable.
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zeb911_ladder_exhausted_no_member_reachable() {
    init_tracing();
    harmony_app::iroh_endpoint::warm_up_iroh_global_init().await;

    tokio::time::timeout(Duration::from_secs(90), async {
        let s = setup_two_party_iroh_handshake_with_config(default_acceptor_config()).await;
        // A real (so structurally valid: parseable EndpointId, loopback direct
        // addrs) endpoint that is shut down before the record is published —
        // the "advertiser went offline inside the record's TTL" failure mode
        // (spec §7.4). No acceptor, no accept loop, nothing listening.
        let dead_ep = build_hermetic_endpoint().await;
        let dead_routing = ReachabilityAnnouncePayload {
            iroh_node_id: *dead_ep.node_id().as_bytes(),
            home_relay_url: String::new(),
            direct_addresses: dead_ep.bound_sockets(),
            announced_at_ms: now_ms(),
            identity_signature: [0xCDu8; 64],
            butler_set: Vec::new(),
            bs_at: 0,
        };
        dead_ep.shutdown().await;
        drop(dead_ep);

        let witness_comm = mint_test_owner(0xC3);
        let witness_transport = PrivateIdentity::from_seed(&[0xc3; 32]);
        let witness_transport_sk = signing_key_from(&witness_transport);
        let (_addr, witness_transport_pub) = derive_composite_owner(&witness_transport_sk);

        let (_payload, invite_url, token_sig) = zeb889_build_targeted_invite(&s);
        assert_no_admin_case_a_record(&s, &token_sig).await;

        let _rdv = publish_rendezvous_slot(
            &s,
            &s.alice_minted.membership_key,
            witness_comm.owner,
            Arc::new(witness_comm.device_key.clone()),
            witness_transport_sk,
            witness_transport_pub,
            dead_routing,
        )
        .await;

        let outcome =
            drive_joiner_redeem(&s, invite_url, witness_dial_config(Duration::from_secs(3))).await;

        assert_eq!(
            outcome.status, "no_member_reachable",
            "a resolved-but-undialable witness candidate must exhaust the ladder into \
             'no_member_reachable' (NOT the legacy single-target 'inviter_unreachable') — \
             got status={:?}",
            outcome.status
        );

        s.publisher_handle.abort();
        s.alice_ep.shutdown().await;
        s.bob_ep.shutdown().await;
    })
    .await
    .expect("zeb911_ladder_exhausted_no_member_reachable timed out at 90s");
}

// ────────────────────────────────────────────────────────────────────────────
// 3. Nothing published at all → zero witness dials → inviter_unreachable.
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zeb911_no_records_at_all_stays_inviter_unreachable() {
    init_tracing();
    harmony_app::iroh_endpoint::warm_up_iroh_global_init().await;

    tokio::time::timeout(Duration::from_secs(90), async {
        let s = setup_two_party_iroh_handshake_with_config(default_acceptor_config()).await;
        let (_payload, invite_url, token_sig) = zeb889_build_targeted_invite(&s);
        assert_no_admin_case_a_record(&s, &token_sig).await;

        // No rendezvous slot record either: every slot in the epoch window
        // resolves empty, so the ladder attempts ZERO dials.
        for slot in 0..RENDEZVOUS_SLOT_COUNT as u16 {
            for epoch_id in harmony_pkarr::epoch_tolerance_window(now_ms()) {
                let vk =
                    rendezvous_slot_verifying_key(&s.alice_minted.membership_key, slot, epoch_id);
                assert!(
                    matches!(s.pkarr_resolver.resolve(&vk).await, Ok(None)),
                    "no rendezvous record may exist for slot {slot} epoch {epoch_id}"
                );
            }
        }

        let outcome =
            drive_joiner_redeem(&s, invite_url, witness_dial_config(Duration::from_secs(3))).await;

        assert_eq!(
            outcome.status, "inviter_unreachable",
            "with zero witness dials attempted the outcome must stay 'inviter_unreachable' — \
             'no_member_reachable' is reserved for a ladder that actually dialed a witness. \
             Got status={:?}",
            outcome.status
        );

        s.publisher_handle.abort();
        s.alice_ep.shutdown().await;
        s.bob_ep.shutdown().await;
    })
    .await
    .expect("zeb911_no_records_at_all_stays_inviter_unreachable timed out at 90s");
}
