//! ZEB-911 slice 1: any Joined member — not just the inviter — accepts the
//! invite redeem handshake.
//!
//! Before ZEB-911, `verify_packet_pure` carried a `token.inviter == self_owner`
//! step, so only the node that MINTED the invite token could complete the
//! handshake. That step is gone; the token signature is now checked against the
//! MINTER's enrolled device keys resolved out of the receiver's materialized
//! membership, and *eligibility to act* moved into `handle_unicast` as a
//! membership-state check (self Joined + power >= `power_thresholds.invite`).
//!
//! These tests drive `community_invite::handle_unicast` directly against a
//! WITNESS's engine — a Joined, non-admin, power-0 member who did not mint the
//! token — and pin the three outcomes that define the new contract:
//!
//!   * a witness accepts an admin-minted token and auto-counter-signs it, so
//!     the joiner materializes as `Joined` without the admin ever being online;
//!   * a receiver who is not in the community's CRDT is refused with
//!     `SelfNotJoined` and commits nothing;
//!   * a token whose `inviter` cannot be resolved to any enrolled device key in
//!     the receiver's materialized membership is refused with
//!     `InviteTokenSignerUnknown` — the gate that keeps "verify against the
//!     minter's keys" from degrading into "verify against anybody's keys".
//!
//! Engine-level idioms (CAS servicer, registry config, `mint_test_owner`
//! signing model, JoinCountersign construction, bounded countersign poll) follow
//! `community_pending_join_integration.rs`; the packet/outbox construction
//! follows `community_invite_only_integration.rs`.

use ed25519_dalek::Signer;
use harmony_app::community_invite::{
    self, build_signed_invite_packet, canonical_invite_token_bytes, device_hash_from_identity_pub,
    encode_packet, CommunityInviteSigned, CommunityInviteVerifyError, InviteToken,
};
use harmony_app::community_membership::{
    materialize, mint_test_owner, sign_event, EventId, EventPayload, MemberStatus,
    MembershipEventKind, SignedMembershipEvent, TestOwner,
};
use harmony_app::community_state_crdt::InsertOutcome;
use harmony_app::community_state_sync::{
    CatchUpChannels, CommunityRegistryConfig, CommunitySyncRegistry, IdentityResolver,
    DEFAULT_DEBOUNCE_MS,
};
use harmony_app::content_store::{CasOp, ContentStore, RuntimeContentStore};
use harmony_app::dm_outbox::DmOutbox;
use harmony_app::owner_state_crdt::OwnerState;
use harmony_app::owner_state_types::{DeviceIdentityHash, Hlc, OwnerAddr, SpaceId};
use harmony_identity::PrivateIdentity;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex as TokioMutex};

/// ZEB-339 removed the resolver gate from membership verification — actors are
/// authenticated via their carried `EnrollmentCert` or their materialized
/// `enrolled_device_keys` — so these tests never need a populated owner→identity
/// cache. The registry still requires *an* resolver; this one always misses.
struct NoIdentityResolver;

#[async_trait::async_trait]
impl IdentityResolver for NoIdentityResolver {
    async fn resolve(&self, _addr: &OwnerAddr) -> Option<[u8; 64]> {
        None
    }
}

/// Reach into `PrivateIdentity`'s ed25519 seed the way the open-flow and
/// invite-only integration tests do (canonical 32-byte seed lives in bytes
/// 32..64 of `to_private_bytes()`).
fn transport_signing_key(identity: &PrivateIdentity) -> ed25519_dalek::SigningKey {
    let priv_bytes = identity.to_private_bytes();
    let mut secret = [0u8; 32];
    secret.copy_from_slice(&priv_bytes[32..64]);
    ed25519_dalek::SigningKey::from_bytes(&secret)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// In-memory CAS servicer — the engine's state-root publish path needs a
/// content store, but nothing in these tests reads back what it wrote.
fn make_content_store() -> Arc<dyn ContentStore> {
    let (cas_op_tx, mut cas_op_rx) = mpsc::channel::<CasOp>(64);
    tokio::spawn(async move {
        while let Some(op) = cas_op_rx.recv().await {
            match op {
                CasOp::PutLocal { reply, .. } => {
                    if let Some(r) = reply {
                        let _ = r.send(Ok(()));
                    }
                }
                CasOp::GetOrFetch { reply, .. } => {
                    let _ = reply.send(Ok(None));
                }
                CasOp::GetLocal { reply, .. } => {
                    let _ = reply.send(None);
                }
                CasOp::AllowServeSubtree { reply, .. } => {
                    let _ = reply.send(Ok(0));
                }
            }
        }
    });
    Arc::new(RuntimeContentStore::new(cas_op_tx, Duration::from_secs(2)))
}

/// Sign an `InviteToken` with `signer_key` over the canonical token bytes.
/// ZEB-339: the token sig verifies against the minter's ENROLLED device key
/// (`verify_invite_token_sig_with_enrolled`), so this signs with a device key,
/// never an identity key.
fn signed_token(
    signer_key: &ed25519_dalek::SigningKey,
    inviter: OwnerAddr,
    invitee_hint: Option<OwnerAddr>,
    minted_wall_ms: u64,
    expires_at: Option<u64>,
) -> InviteToken {
    let mut tok = InviteToken {
        inviter,
        invitee_hint,
        minted_at: Hlc {
            wall_ms: minted_wall_ms,
            logical: 0,
            device_id: "minter-dev".into(),
        },
        expires_at,
        sig: [0u8; 64],
    };
    let bytes = canonical_invite_token_bytes(&tok).expect("canonical token bytes");
    tok.sig = signer_key.sign(&bytes).to_bytes();
    tok
}

/// Build a `PendingJoin` signed by `owner`'s enrolled device key, carrying
/// `owner`'s Master `EnrollmentCert` (the identity-introducing event shape).
fn pending_join_event(
    owner: &TestOwner,
    community_id: SpaceId,
    invite_token: InviteToken,
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

/// Build a `JoinCountersign` from `signer` targeting `target_event_id`.
/// Steady-state event: no cert attached — the actor's enrolled device key is
/// already in the materialized membership by the time this verifies.
fn countersign_event(
    signer: &TestOwner,
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
        actor: signer.owner,
        at: Hlc {
            wall_ms,
            logical: 0,
            device_id: device_id.into(),
        },
    };
    sign_event(&payload, &signer.device_key).expect("sign JoinCountersign")
}

/// Wire bytes for a redeem handshake packet: the joiner's `PendingJoin` wrapped
/// in a `CommunityInviteSigned` envelope signed by the joiner's TRANSPORT
/// identity. The two key layers are deliberately distinct — the membership
/// event is signed by the joiner's enrolled community device key, the envelope
/// by the Reticulum/iroh identity whose hash the packet commits to.
fn redeem_packet_bytes(
    community_id: SpaceId,
    join_event: SignedMembershipEvent,
    invite_token: InviteToken,
    joiner_transport: &PrivateIdentity,
    created_wall_ms: u64,
) -> Vec<u8> {
    let joiner_identity_pub = joiner_transport.identity.to_public_bytes();
    let signed = CommunityInviteSigned {
        community_id,
        join_event,
        invite_token,
        joiner_identity_pub,
        signing_device_hash: DeviceIdentityHash(device_hash_from_identity_pub(
            &joiner_identity_pub,
        )),
        created_at: Hlc {
            wall_ms: created_wall_ms,
            logical: 0,
            device_id: "joiner-dev".into(),
        },
    };
    let packet = build_signed_invite_packet(signed, &transport_signing_key(joiner_transport))
        .expect("build invite packet");
    encode_packet(&packet).expect("encode invite packet")
}

/// A `DmOutbox` whose only field `handle_unicast` reads is `self_owner`.
/// `new_synthetic` bypasses the owner_id debug_assert in `DmOutbox::new`
/// (the addresses here are community owner_ids, not Reticulum-derived).
fn outbox_for(self_owner: OwnerAddr, seed: u8) -> Arc<TokioMutex<DmOutbox>> {
    let owner = mint_test_owner(seed);
    let device_key = Arc::new(owner.device_key.clone());
    let transport = PrivateIdentity::from_seed(&[seed; 32]);
    Arc::new(TokioMutex::new(DmOutbox::new_synthetic(
        "receiver-dev".into(),
        self_owner,
        DeviceIdentityHash(transport.identity.address_hash),
        Arc::clone(&device_key),
        Arc::new(transport),
        device_key,
        owner.cert,
    )))
}

/// A community whose CRDT (held by the WITNESS's registry) contains the admin's
/// bootstrap Join plus the pair that makes the witness a Joined, non-admin,
/// power-0 member: the witness's `PendingJoin` (admin-minted token) and the
/// admin's `JoinCountersign` targeting it.
struct WitnessFixture {
    registry: Arc<CommunitySyncRegistry>,
    community_id: SpaceId,
    admin: TestOwner,
    witness: TestOwner,
    joiner: TestOwner,
    joiner_transport: PrivateIdentity,
    crdt_state: Arc<TokioMutex<OwnerState>>,
    now: u64,
    // Keepalives: the tempdir backs the registry's persist paths, and dropping
    // the subscriber sender would latch the engine's `inbound_closed`.
    _dir: tempfile::TempDir,
    _sub_tx: mpsc::Sender<Vec<u8>>,
}

async fn setup_witness_community() -> WitnessFixture {
    let now = now_ms();
    let admin = mint_test_owner(0x51);
    let witness = mint_test_owner(0x52);
    let joiner = mint_test_owner(0x53);
    let joiner_transport = PrivateIdentity::from_seed(&[0x53; 32]);

    let dir = tempfile::tempdir().expect("tempdir");
    let resolver: Arc<dyn IdentityResolver> = Arc::new(NoIdentityResolver);

    // The registry models the WITNESS's node: self_owner + signing key are the
    // witness's, so the auto-counter-sign hook signs as the witness.
    let registry = Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
        device_id: "witness-dev".into(),
        content_store: make_content_store(),
        identity_resolver: resolver,
        identity_dir: dir.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: None,
        self_owner: witness.owner,
        signing_key: Arc::new(witness.device_key.clone()),
        crdt_state: None,
        nav_emitter: None,
        presence_resync_rx: None,
    }));

    let minted = harmony_app::mint_community_creation(
        "Witnessed",
        true,
        admin.owner,
        &admin.device_key,
        &admin.cert,
        Hlc {
            wall_ms: now - 60_000,
            logical: 0,
            device_id: "admin-dev".into(),
        },
    )
    .expect("mint community");
    let community_id = minted.community_id;

    let (pub_tx, mut pub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (sub_tx, sub_rx) = mpsc::channel::<Vec<u8>>(64);
    // Drain the publish side so a debounce flush can never wedge on backpressure.
    tokio::spawn(async move { while pub_rx.recv().await.is_some() {} });

    registry
        .spawn_engine_inner_now(
            community_id,
            minted.membership_key.clone(),
            admin.owner,
            true,
            pub_tx,
            sub_rx,
            CatchUpChannels::none(),
        )
        .await
        .expect("spawn witness engine");
    let engine = registry.engine_arc(&community_id).await.expect("engine");

    // 1. Admin's bootstrap Join — the community's trust root, and the source of
    //    the admin's enrolled device key that every token check resolves through.
    let outcome = engine
        .insert_local_event(minted.bootstrap_join.clone())
        .await
        .expect("admin bootstrap insert");
    assert!(
        matches!(outcome, InsertOutcome::Inserted),
        "admin bootstrap must be Inserted; got {outcome:?}"
    );

    // 2 + 3. The witness's own admission: PendingJoin against an admin-minted
    //        token, then the admin's JoinCountersign that completes it. The
    //        auto-counter-sign hook fires on step 2 but finds self not yet
    //        Joined and returns without minting anything.
    let witness_pending = pending_join_event(
        &witness,
        community_id,
        signed_token(
            &admin.device_key,
            admin.owner,
            Some(witness.owner),
            now - 58_000,
            Some(now + 3_600_000),
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
        &admin,
        community_id,
        witness_pending_id,
        [0xC1_u8; 16],
        now - 40_000,
        "admin-dev",
    );
    let outcome = engine
        .insert_local_event(admin_cs)
        .await
        .expect("admin JoinCountersign insert");
    assert!(
        matches!(outcome, InsertOutcome::Inserted),
        "admin JoinCountersign must be Inserted; got {outcome:?}"
    );

    // Precondition for every test below: the witness is Joined and is NOT the
    // admin — exactly the role ZEB-911 newly empowers.
    let state = registry.state_for(&community_id).await.expect("state");
    let events: Vec<_> = {
        let g = state.lock().await;
        g.events().cloned().collect()
    };
    let mat = materialize(&events, admin.owner);
    assert_eq!(
        mat.members.get(&witness.owner).map(|m| m.status),
        Some(MemberStatus::Joined),
        "fixture precondition: witness must materialize as Joined"
    );
    assert_ne!(
        witness.owner, admin.owner,
        "fixture precondition: witness must not be the admin"
    );
    assert_eq!(
        mat.power_levels.get(&witness.owner).copied().unwrap_or(0),
        0,
        "fixture precondition: witness is a plain power-0 member"
    );

    WitnessFixture {
        registry,
        community_id,
        admin,
        witness,
        joiner,
        joiner_transport,
        crdt_state: Arc::new(TokioMutex::new(OwnerState::default())),
        now,
        _dir: dir,
        _sub_tx: sub_tx,
    }
}

impl WitnessFixture {
    /// The joiner's redeem packet, its token minted by `minter` and attributed
    /// to `inviter`. The happy path passes the admin for both; the
    /// unknown-signer test passes an owner absent from the CRDT.
    fn joiner_packet(
        &self,
        minter_key: &ed25519_dalek::SigningKey,
        inviter: OwnerAddr,
        pending_id: EventId,
    ) -> Vec<u8> {
        let token = signed_token(
            minter_key,
            inviter,
            Some(self.joiner.owner),
            self.now - 20_000,
            Some(self.now + 3_600_000),
        );
        let join_event = pending_join_event(
            &self.joiner,
            self.community_id,
            token.clone(),
            pending_id,
            self.now - 10_000,
            "joiner-dev",
        );
        redeem_packet_bytes(
            self.community_id,
            join_event,
            token,
            &self.joiner_transport,
            self.now - 5_000,
        )
    }

    async fn events(&self) -> Vec<SignedMembershipEvent> {
        let state = self
            .registry
            .state_for(&self.community_id)
            .await
            .expect("state");
        let g = state.lock().await;
        g.events().cloned().collect()
    }

    async fn has_pending_join_from_joiner(&self) -> bool {
        self.events().await.iter().any(|e| {
            e.actor == self.joiner.owner
                && matches!(&e.kind, MembershipEventKind::PendingJoin { .. })
        })
    }
}

/// The core ZEB-911 claim: a Joined, non-admin, power-0 WITNESS accepts a
/// redeem handshake for a token it did not mint, commits the joiner's
/// PendingJoin, and auto-counter-signs it — so the joiner reaches `Joined`
/// with the admin entirely offline.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zeb911_witness_accepts_and_countersigns_pending_join() {
    let fx = setup_witness_community().await;
    let pending_id: EventId = [0x91; 16];
    let packet = fx.joiner_packet(&fx.admin.device_key, fx.admin.owner, pending_id);

    // The receiver is the WITNESS, not the token's minter.
    let outbox = outbox_for(fx.witness.owner, 0x62);
    community_invite::handle_unicast(&fx.registry, &outbox, &fx.crdt_state, packet, None::<&()>)
        .await
        .expect("witness must accept an admin-minted redeem handshake");

    assert!(
        fx.has_pending_join_from_joiner().await,
        "witness's CRDT must contain the joiner's PendingJoin after acceptance"
    );

    // Auto-counter-sign is a spawned post-insert hook, so poll rather than
    // sleep. Bound matches the sibling assertion in
    // `community_pending_join_integration::admin_engine_auto_counter_signs_on_pending_join_insert`.
    let state = fx
        .registry
        .state_for(&fx.community_id)
        .await
        .expect("state");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            {
                let g = state.lock().await;
                let found = g.events().any(|e| {
                    e.actor == fx.witness.owner
                        && matches!(
                            &e.kind,
                            MembershipEventKind::JoinCountersign { target_event_id }
                            if *target_event_id == pending_id
                        )
                });
                if found {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("timed out waiting for the WITNESS's auto-minted JoinCountersign");

    // The witness's countersign is authoritative: the joiner materializes Joined
    // without any admin-authored event about them.
    let events = fx.events().await;
    let mat = materialize(&events, fx.admin.owner);
    assert_eq!(
        mat.members.get(&fx.joiner.owner).map(|m| m.status),
        Some(MemberStatus::Joined),
        "joiner must materialize as Joined off the witness's countersign alone"
    );
    assert!(
        !events.iter().any(|e| e.actor == fx.admin.owner
            && matches!(
                &e.kind,
                MembershipEventKind::JoinCountersign { target_event_id }
                if *target_event_id == pending_id
            )),
        "the admin must not have countersigned — the witness alone admitted the joiner"
    );
}

/// ZEB-954 e2e: a MODERATOR-minted invite-only invite — its token `inviter` a
/// Joined, non-admin member (the witness), NOT the admin — converges all the
/// way to `Joined` through the witness auto-countersign path. This is the
/// convergence that does NOT need the cold-publish fast-path: admission runs
/// against the receiving member's OWN full materialized state (`verify_event`
/// P2/P5), where the inviter's Joined + invite-power status and token signature
/// are authoritative and cannot be omitted. Pre-ZEB-954, P2 hard-required
/// `inviter == admin`, so this joiner's PendingJoin was rejected at insert and
/// never committed; the relaxed P2 admits it and the join converges.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zeb954_moderator_minted_invite_converges_to_joined_via_witness() {
    let fx = setup_witness_community().await;
    let pending_id: EventId = [0x93; 16];
    // Token minted BY the moderator (the non-admin witness) and attributed to
    // them as inviter — exactly the case the old admin-only P2 rejected.
    let packet = fx.joiner_packet(&fx.witness.device_key, fx.witness.owner, pending_id);

    // The receiver is the witness's node; it verifies the moderator-minted token
    // against its OWN materialized state, commits the joiner's PendingJoin, and
    // auto-counter-signs.
    let outbox = outbox_for(fx.witness.owner, 0x64);
    community_invite::handle_unicast(&fx.registry, &outbox, &fx.crdt_state, packet, None::<&()>)
        .await
        .expect("witness must accept a moderator-minted redeem handshake");

    assert!(
        fx.has_pending_join_from_joiner().await,
        "the moderator-minted joiner's PendingJoin must commit — P2 admits a Joined non-admin inviter"
    );

    // Auto-counter-sign is a spawned post-insert hook, so poll rather than sleep.
    let state = fx
        .registry
        .state_for(&fx.community_id)
        .await
        .expect("state");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            {
                let g = state.lock().await;
                let found = g.events().any(|e| {
                    e.actor == fx.witness.owner
                        && matches!(
                            &e.kind,
                            MembershipEventKind::JoinCountersign { target_event_id }
                            if *target_event_id == pending_id
                        )
                });
                if found {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("timed out waiting for the witness's auto-minted JoinCountersign");

    // End-to-end: the moderator-invited joiner materializes Joined — no
    // admin-authored event about them, no cold-publish bootstrap involved.
    let events = fx.events().await;
    let mat = materialize(&events, fx.admin.owner);
    assert_eq!(
        mat.members.get(&fx.joiner.owner).map(|m| m.status),
        Some(MemberStatus::Joined),
        "moderator-invited joiner must reach Joined via the witness countersign"
    );
    // Guard against the token silently attributing to the admin: the committed
    // PendingJoin must carry the non-admin moderator as its inviter.
    assert!(
        events.iter().any(|e| {
            e.actor == fx.joiner.owner
                && matches!(
                    &e.kind,
                    MembershipEventKind::PendingJoin { invite_token }
                    if invite_token.inviter == fx.witness.owner
                        && invite_token.inviter != fx.admin.owner
                )
        }),
        "the joiner's committed PendingJoin must carry the non-admin moderator as inviter"
    );
}

/// Eligibility is a membership-state check, not a transport one: a receiver
/// absent from the community's CRDT is refused with `SelfNotJoined` and commits
/// nothing, even though the packet itself is fully valid (it is the exact
/// packet the witness accepts above).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zeb911_non_member_receiver_rejects_self_not_joined() {
    let fx = setup_witness_community().await;
    let packet = fx.joiner_packet(&fx.admin.device_key, fx.admin.owner, [0x92; 16]);

    // An owner that appears nowhere in the CRDT.
    let stranger = mint_test_owner(0x63).owner;
    let outbox = outbox_for(stranger, 0x64);
    let err = community_invite::handle_unicast(
        &fx.registry,
        &outbox,
        &fx.crdt_state,
        packet,
        None::<&()>,
    )
    .await
    .expect_err("a non-member receiver must refuse the handshake");
    assert_eq!(err, CommunityInviteVerifyError::SelfNotJoined);

    assert!(
        !fx.has_pending_join_from_joiner().await,
        "a refused handshake must commit nothing to the CRDT"
    );
}

/// The gate that keeps the witness model from widening the trust surface: the
/// token's `inviter` must resolve to enrolled device keys in the receiver's own
/// materialized membership. An `inviter` absent from the CRDT yields no keys to
/// try, and is refused as `InviteTokenSignerUnknown` before the pure verify runs
/// — a self-signed token from a stranger can never be accepted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zeb911_unknown_token_signer_rejected() {
    let fx = setup_witness_community().await;
    let outsider = mint_test_owner(0x65);
    let packet = fx.joiner_packet(&outsider.device_key, outsider.owner, [0x93; 16]);

    let outbox = outbox_for(fx.witness.owner, 0x66);
    let err = community_invite::handle_unicast(
        &fx.registry,
        &outbox,
        &fx.crdt_state,
        packet,
        None::<&()>,
    )
    .await
    .expect_err("a token from an unknown minter must be refused");
    assert_eq!(
        err,
        CommunityInviteVerifyError::InviteTokenSignerUnknown {
            signer: outsider.owner
        }
    );

    assert!(
        !fx.has_pending_join_from_joiner().await,
        "an unknown-minter handshake must commit nothing to the CRDT"
    );
}

/// Regression guard on the path ZEB-911 refactored *around*: the minter's own
/// node still accepts its own token. `pkarr_iroh_redeem_full_integration::
/// bob_joins_alice_via_iroh_handshake_option_a` covers this end-to-end over a
/// real iroh stream; this is the hermetic direct-call sibling, so a regression
/// in `handle_unicast`'s eligibility/`token_signer_keys` resolution surfaces
/// here without any networking in the failure path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zeb911_admin_self_accept_regression() {
    let fx = setup_witness_community().await;
    let pending_id: EventId = [0x94; 16];
    let packet = fx.joiner_packet(&fx.admin.device_key, fx.admin.owner, pending_id);

    // Same engine, but the receiver identity is now the admin (who minted the
    // token) — the pre-ZEB-911 `token.inviter == self_owner` case.
    let outbox = outbox_for(fx.admin.owner, 0x67);
    community_invite::handle_unicast(&fx.registry, &outbox, &fx.crdt_state, packet, None::<&()>)
        .await
        .expect("the minter's own node must still accept its own token");

    assert!(
        fx.has_pending_join_from_joiner().await,
        "admin acceptance must commit the joiner's PendingJoin"
    );
}
