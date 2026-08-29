//! ZEB-580 S2 Task 8: end-to-end proof that a DM signed by a device the
//! sender's owner has had revoked (in a community shared with the recipient)
//! is dropped at verify time — the S2 non-regression gate, sibling to
//! `dm_cert_identity_integration.rs`'s S1 gate.
//!
//! Flow proven:
//!   1. Two real-minted owners, Alice + Bob, complete a REAL two-node iroh
//!      friend handshake over `harmony/friend/v1` (copied harness from
//!      `dm_cert_identity_integration.rs`) — Bob's `OwnerDeviceCache` ends up
//!      holding exactly Alice's cert-derived #2 device (singleton), the same
//!      precondition S1's assertion 2 pins in detail. That mechanism is not
//!      re-litigated here; this test isolates the NEW S2 behavior instead.
//!   2. Baseline: Alice's REAL send-side path (`drain -> push_deposit_candidate
//!      -> build_cidnotify_packet_bytes -> dm_signing_material()`) signs a
//!      CidNotify with her #2 device. Bob's REAL receive path
//!      (`dm_inbox_ingest::ingest_dm_packet`, the live tunnel entry point —
//!      it shares `verify_cidnotify_admission` with the `ProdDmInboxIngestCtx`
//!      / `ProdRelayIngestCtx` recover routes) admits +
//!      decrypts it against an EMPTY `RevokedDeviceProjection`. Delivered.
//!   3. Alice's owner is revoked in a community Bob and Alice both belong to:
//!      modeled by feeding Alice's #2 ed25519 key into Bob's
//!      `RevokedDeviceProjection` via `union_from_members` (the same
//!      aggregation the real community-membership feed drives — see
//!      `revoked_device_projection.rs`).
//!   4. Alice sends AGAIN, same #2 device, same real send path. Bob's SAME
//!      real receive path now runs `verify_cidnotify_sender_binding` against
//!      the populated projection: `SignerDeviceRevoked` — the packet is
//!      dropped (no `dm-received` event, no inbox entry, no cache mutation).
//!   5. Control: a THIRD real-minted owner, Carol, whose #2 device was never
//!      revoked, sends through the SAME `RevokedDeviceProjection` handle (the
//!      one that already contains Alice's revoked key) and delivers normally
//!      — proving the cutoff is keyed per `(owner, ed25519)`, not a per-owner
//!      blanket drop. Carol's cache entry on Bob's side is seeded directly via
//!      `OwnerState::apply_owner_device_update` (the exact call the friend
//!      handshake itself makes) rather than via a second full handshake: the
//!      handshake-populates-the-cache mechanism is already pinned end-to-end
//!      for Alice above / by S1's assertion 2, so re-deriving it for Carol
//!      would add iroh/pkarr machinery without exercising anything new. This
//!      mirrors `dm_cert_identity_integration.rs`'s own precedent of seeding
//!      the DM Space directly for its assertions 3-4.
//!
//! Every send is the REAL production-signed wire packet (drain/deposit
//! capture, not hand-built), and every receive runs through the REAL live
//! tunnel receive path `dm_inbox_ingest::ingest_dm_packet` ->
//! `verify_cidnotify_admission` -> `verify_cidnotify_sender_binding` — the
//! same cutoff the community-relay recovery (`ProdRelayIngestCtx`) route
//! shares.

use std::collections::{BTreeMap, BTreeSet};
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use ed25519_dalek::SigningKey;
use harmony_app::friend_graph::FriendStatus;
use harmony_app::friend_token::{encode_friend_token_url, mint_friend_token};
use harmony_app::iroh_endpoint::{alpn, IrohEndpoint};
use harmony_app::iroh_friend_acceptor::{
    IrohFriendHandshakeAcceptor, MultiplexHandshakeDispatcher,
};
use harmony_app::owner_state_crdt::OwnerState;
use harmony_app::owner_state_types::{
    ContentId, DeviceIdentityHash, DmContentKey, Hlc, OwnerAddr, Space, SpaceId, SpaceKind,
};
use harmony_app::reachability_resolver::ReachabilityResolver;
use harmony_app::revoked_device_projection::RevokedDeviceProjection;
use harmony_app::zenoh_iroh_transport::IrohZenohLinkManager;
use iroh::endpoint::{presets, Endpoint, RelayMode};
use iroh::SecretKey;
use tokio::sync::Mutex as TokioMutex;
use zenoh_link::LinkUnicast;

/// A real minted owner, shaped like `community_membership::TestOwner` but built
/// from `harmony_owner::lifecycle::mint_owner` so its device cert carries a REAL
/// (non-zero) X25519 pub — the precondition for `device2_signing_hash` to yield
/// a #2 identity instead of degrading to #3. Copied from
/// `dm_cert_identity_integration.rs` (sibling module in this same `dm_tests`
/// binary; its helpers are module-private).
struct RealOwner {
    owner: OwnerAddr,
    device_key: SigningKey,
    cert: harmony_owner::certs::EnrollmentCert,
    /// ZEB-685 (S3): kept so a test can reconstruct this owner's master key and
    /// sign a real `RevocationCert` for its own device (the same reconstruction
    /// `owner_commands::revoke_device_inner` does). The community-cutoff test
    /// ignores it; the DM-push cutoff test below uses it.
    recovery_artifact: harmony_owner::lifecycle::RecoveryArtifact,
}

fn mint_real_owner(ts_secs: u64) -> RealOwner {
    let minted = harmony_owner::lifecycle::mint_owner(ts_secs).expect("mint real owner");
    let device_vk = minted.device_signing_key.verifying_key().to_bytes();
    let cert = minted
        .state
        .enrollments
        .values()
        .find(|c| c.device_pubkeys.classical.ed25519_verify == device_vk)
        .expect("an enrollment cert for the minted device signing key")
        .clone();
    assert_ne!(
        cert.device_pubkeys.classical.x25519_pub, [0u8; 32],
        "mint_owner must ship a real (non-zero) device X25519 — the #2 precondition"
    );
    RealOwner {
        owner: OwnerAddr(cert.owner_id),
        device_key: minted.device_signing_key,
        cert,
        recovery_artifact: minted.recovery_artifact,
    }
}

/// Build a hermetic iroh endpoint on loopback. Copied from
/// `dm_cert_identity_integration.rs`.
async fn build_hermetic_endpoint() -> Arc<IrohEndpoint> {
    let secret = SecretKey::generate();
    let inner = Endpoint::builder(presets::Minimal)
        .secret_key(secret)
        .alpns(vec![
            alpn::HARMONY_ZENOH_V1.to_vec(),
            alpn::HARMONY_HANDSHAKE_V1.to_vec(),
            alpn::HARMONY_FRIEND_V1.to_vec(),
        ])
        .relay_mode(RelayMode::Disabled)
        .dns_resolver(harmony_app::iroh_endpoint::hermetic_dns_resolver())
        .clear_ip_transports()
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .expect("bind_addr loopback")
        .bind()
        .await
        .expect("bind iroh endpoint");
    Arc::new(IrohEndpoint::from_endpoint_for_test(inner))
}

/// 64-byte composite identity_pub for a transport signing key (used only to
/// sign the Case-A pkarr routing record). Copied from
/// `dm_cert_identity_integration.rs`.
fn identity_pub_for(sk: &SigningKey) -> [u8; 64] {
    let x25519_priv = harmony_app::dm_signing::ed25519_priv_to_x25519(sk);
    let x25519_pub = x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(*x25519_priv));
    let ed25519_pub = sk.verifying_key().to_bytes();
    let mut combined = [0u8; 64];
    combined[..32].copy_from_slice(x25519_pub.as_bytes());
    combined[32..].copy_from_slice(&ed25519_pub);
    combined
}

/// No-op dispatcher for the multiplexer's invite + PEX slots. Copied from
/// `dm_cert_identity_integration.rs`.
struct NoopDispatcher;

#[async_trait::async_trait]
impl harmony_app::iroh_invite_acceptor::IrohHandshakeDispatcher for NoopDispatcher {
    async fn handle_connection(&self, _conn: iroh::endpoint::Connection) {}
}

/// Records emitted node events so the test can assert `dm-received`. Copied
/// from `dm_cert_identity_integration.rs`.
struct RecordingSink {
    events: Arc<StdMutex<Vec<(String, serde_json::Value)>>>,
}

impl harmony_app::node_event_sink::NodeEventSink for RecordingSink {
    fn emit(&self, event: &str, payload: serde_json::Value) {
        self.events
            .lock()
            .expect("sink lock")
            .push((event.to_string(), payload));
    }
}

/// Capturing butler-deposit mock. Copied from
/// `dm_cert_identity_integration.rs`.
struct CapturingDepositClient {
    captured: Arc<StdMutex<Vec<harmony_app::butler_deposit::ButlerDepositRequest>>>,
}

#[async_trait::async_trait]
impl harmony_app::butler_deposit::ButlerDepositClient for CapturingDepositClient {
    async fn deposit(
        &self,
        req: &harmony_app::butler_deposit::ButlerDepositRequest,
    ) -> harmony_app::butler_deposit::DepositRungOutcome {
        self.captured
            .lock()
            .expect("captured lock")
            .push(req.clone());
        harmony_app::butler_deposit::DepositRungOutcome::Failed("capture-only mock".into())
    }
}

/// A DM `Space` with a fixed content_key, shared byte-for-byte by both
/// parties. Copied from `dm_cert_identity_integration.rs`.
fn make_dm_space(id: SpaceId, a: OwnerAddr, b: OwnerAddr, content_key: DmContentKey) -> Space {
    let mut members = vec![a, b];
    members.sort();
    Space {
        id,
        kind: SpaceKind::Dm,
        parent: None,
        community_id: None,
        name: "dm-revocation-cutoff-test".into(),
        transport: None,
        members,
        custom_name: None,
        notification_pref: None,
        left_at: None,
        created_at: Hlc {
            wall_ms: 100,
            logical: 0,
            device_id: "seed-dev".into(),
        },
        updated_at: Hlc {
            wall_ms: 100,
            logical: 0,
            device_id: "seed-dev".into(),
        },
        content_key: Some(content_key),
        prior_content_keys: vec![],
        current_epoch: None,
        current_epoch_key: None,
        old_epoch_keys: BTreeMap::new(),
        admin_addr: None,
        is_invite_only: None,
        shared_in_profile: false,
        read_receipt_pref: None,
        pending_join_at: None,
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time")
        .as_millis() as u64
}

/// A dummy, self-consistent #3 (legacy transport) identity — never exercised
/// on the #2 DM-signing path, but required by `DmOutbox::new`'s constructor
/// invariants. Distinct per party so cross-party confusion would fail loudly.
fn dummy_hash3_and_key(
    seed: u8,
) -> (
    DeviceIdentityHash,
    Arc<SigningKey>,
    Arc<harmony_identity::PrivateIdentity>,
) {
    let priv3 = Arc::new(harmony_identity::PrivateIdentity::from_seed(&[seed; 32]));
    let hash3 = DeviceIdentityHash(priv3.public_identity().address_hash);
    let priv3_bytes = priv3.to_private_bytes();
    let sk3 = Arc::new(SigningKey::from_bytes(
        &<[u8; 32]>::try_from(&priv3_bytes[32..64]).expect("ed25519 seed"),
    ));
    (hash3, sk3, priv3)
}

/// Send `body` from `outbox`'s owner into `dm_space_id`, then drive it through
/// two `drain` ticks — priming (records an `AttemptState`, no candidate yet)
/// and a second ≥5s-later tick (past the base backoff window, so
/// `pre_failure_count >= 1` and the deposit rung actually builds + "deposits"
/// the packet) — so the REAL production
/// `build_cidnotify_packet_bytes -> dm_signing_material()` selection runs.
/// Mirrors `dm_cert_identity_integration.rs`'s assertion-3 send-side drive,
/// factored out here because this test drives it three times.
#[allow(clippy::too_many_arguments)]
async fn send_and_capture_cidnotify(
    outbox: &mut harmony_app::dm_outbox::DmOutbox,
    state: &Arc<TokioMutex<OwnerState>>,
    cas: &Arc<dyn harmony_app::content_store::ContentStore>,
    captured: &Arc<StdMutex<Vec<harmony_app::butler_deposit::ButlerDepositRequest>>>,
    dm_space_id: SpaceId,
    body: Vec<u8>,
    base_t: u64,
) -> (
    harmony_app::dm_envelope::DmCidNotifySigned,
    ContentId,
    // ZEB-710: the raw signed CidNotify wire bytes — the LIVE receive path
    // (`ingest_dm_packet`) decodes internally, so callers feed these directly.
    Vec<u8>,
) {
    let deposit_transport = harmony_app::dm_outbox::DepositOnlyDmTransport;
    let (_msg_id, message_cid): (_, ContentId) = {
        let mut g = state.lock().await;
        outbox
            .send_dm(
                &mut g,
                cas.as_ref(),
                dm_space_id,
                body,
                "text/plain".into(),
                base_t,
                None,
            )
            .await
            .expect("send_dm ok")
    };
    {
        let mut g = state.lock().await;
        outbox.drain(&mut g, &deposit_transport, base_t).await;
    }
    {
        let mut g = state.lock().await;
        // +60s ≫ the 5s base backoff window, so the (entry, recipient) pair
        // is due — mirrors dm_cert_identity_integration.rs.
        outbox
            .drain(&mut g, &deposit_transport, base_t + 60_000)
            .await;
    }
    let deposit_req = {
        let g = captured.lock().expect("captured lock");
        g.iter()
            .find(|r| r.message_cid == Some(message_cid) && r.cidnotify_packet.is_some())
            .cloned()
            .expect("the REAL drain path built + deposited a CidNotify candidate")
    };
    let cidnotify_wire = deposit_req
        .cidnotify_packet
        .clone()
        .expect("deposit carries the CidNotify wire bytes");
    // Decode only to surface `signed` (callers assert on `signing_device_hash`);
    // the raw wire bytes are handed back for the LIVE `ingest_dm_packet` path.
    let signed =
        match harmony_app::dm_envelope::decode_packet(&cidnotify_wire).expect("decode cidnotify") {
            harmony_app::dm_envelope::DmPacket::CidNotify { signed, .. } => signed,
            other => panic!("expected CidNotify, got {other:?}"),
        };
    (signed, message_cid, cidnotify_wire)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoked_device2_dm_is_dropped_after_community_revocation() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("harmony_app=warn")),
        )
        .with_test_writer()
        .try_init();

    // Pre-pay iroh's first-bind global init OUTSIDE the asserted budget.
    harmony_app::iroh_endpoint::warm_up_iroh_global_init().await;

    tokio::time::timeout(Duration::from_secs(60), async {
        // ── 1. Real identities. Alice + Bob do a real handshake; Carol is a
        //    third real-minted owner used only for the control (step 5). ───
        let alice = mint_real_owner(1_700_200_000);
        let bob = mint_real_owner(1_700_200_001);
        let carol = mint_real_owner(1_700_200_002);
        assert_ne!(alice.owner, bob.owner, "distinct minted owners");
        assert_ne!(alice.owner, carol.owner, "distinct minted owners");
        assert_ne!(bob.owner, carol.owner, "distinct minted owners");
        let alice_device2 = Arc::new(SigningKey::from_bytes(&alice.device_key.to_bytes()));
        let bob_device2 = Arc::new(SigningKey::from_bytes(&bob.device_key.to_bytes()));
        let carol_device2 = Arc::new(SigningKey::from_bytes(&carol.device_key.to_bytes()));

        let alice_d2_hash = harmony_app::dm_signing::device2_signing_hash(&alice.cert)
            .expect("alice real cert yields a #2 hash");
        let alice_d2_pub = harmony_app::dm_signing::device2_combined_pub(&alice.cert);
        let carol_d2_hash = harmony_app::dm_signing::device2_signing_hash(&carol.cert)
            .expect("carol real cert yields a #2 hash");
        let carol_d2_pub = harmony_app::dm_signing::device2_combined_pub(&carol.cert);

        let alice_transport_sk = SigningKey::from_bytes(&[0x5a; 32]);
        let alice_transport_pub = identity_pub_for(&alice_transport_sk);

        // ── 2. Iroh endpoints + link managers for Alice + Bob only (Carol
        //    never handshakes — see module doc). ──────────────────────────
        let alice_ep = build_hermetic_endpoint().await;
        let bob_ep = build_hermetic_endpoint().await;
        let alice_bound = alice_ep.bound_sockets();
        assert!(!alice_bound.is_empty(), "alice must expose bound_sockets");

        let alice_reachability = ReachabilityResolver::new();
        let bob_reachability = ReachabilityResolver::new();
        let (alice_link_tx, _alice_link_rx) = flume::unbounded::<LinkUnicast>();
        let alice_link_mgr = Arc::new(IrohZenohLinkManager::new(
            Arc::clone(&alice_ep),
            alice_reachability.clone(),
            alice_link_tx,
        ));
        let (bob_link_tx, _bob_link_rx) = flume::unbounded::<LinkUnicast>();
        let bob_link_mgr = Arc::new(IrohZenohLinkManager::new(
            Arc::clone(&bob_ep),
            bob_reachability.clone(),
            bob_link_tx,
        ));
        let alice_accept = alice_link_mgr.spawn_accept_loop();
        let bob_accept = bob_link_mgr.spawn_accept_loop();

        let alice_crdt_state = Arc::new(TokioMutex::new(OwnerState::default()));
        let alice_hlc_tracker = Arc::new(TokioMutex::new(harmony_crdt_sync::ReplayTracker::new(
            "alice-dev".to_string(),
        )));
        let alice_keytree =
            Arc::new(harmony_app::owner_state_crypto::KeyTree::derive(&[0xA3; 32]).expect("kt"));

        // ── 3. Mock pkarr relay + Case-A friend-token publisher. ─────────
        let relay = harmony_pkarr::testing::MockPkarrRelay::start().await;
        let pool = harmony_pkarr::RelayPool::new(vec![relay.base_url.clone()]);
        let client = Arc::new(harmony_pkarr::RelayClient::new(pool));
        let pkarr_publisher = Arc::new(harmony_pkarr::PkarrPublisher::new(Arc::clone(&client)));
        let publisher_handle = Arc::clone(&pkarr_publisher).spawn();
        let pkarr_resolver = Arc::new(harmony_pkarr::PkarrResolver::new(Arc::clone(&client)));

        let alice_routing = harmony_app::reachability_record::ReachabilityAnnouncePayload {
            iroh_node_id: *alice_ep.node_id().as_bytes(),
            home_relay_url: alice_ep
                .home_relay()
                .map(|r| r.to_string())
                .unwrap_or_default(),
            direct_addresses: alice_bound.clone(),
            announced_at_ms: now_ms(),
            identity_signature: [0xCDu8; 64],
            butler_set: Vec::new(),
            bs_at: 0,
        };
        let alice_routing_blob = {
            let mut buf = Vec::new();
            ciborium::into_writer(&alice_routing, &mut buf).expect("encode alice routing_blob");
            buf
        };
        let alice_routing_blob_clone = alice_routing_blob.clone();
        let friend_pub: Arc<harmony_app::pkarr_invite_publisher::PkarrInvitePublisher> = Arc::new(
            harmony_app::pkarr_invite_publisher::PkarrInvitePublisher::new(
                Arc::clone(&pkarr_publisher),
                alice_transport_sk.clone(),
                alice_transport_pub,
                Arc::new(move || alice_routing_blob_clone.clone()),
            ),
        );

        // ── 4. Install Alice's friend acceptor behind the multiplexer. ───
        let alice_friend_acceptor: Arc<
            dyn harmony_app::iroh_invite_acceptor::IrohHandshakeDispatcher,
        > = Arc::new(IrohFriendHandshakeAcceptor::<()>::new(
            Arc::clone(&alice_crdt_state),
            Arc::clone(&alice_hlc_tracker),
            "alice-dev".to_string(),
            alice.owner,
            Some("alice".to_string()),
            alice.cert.clone(),
            Arc::clone(&alice_device2),
            Arc::clone(&alice_keytree),
            None,
            Some(Arc::clone(&friend_pub)),
        ));
        let invite_stub: Arc<dyn harmony_app::iroh_invite_acceptor::IrohHandshakeDispatcher> =
            Arc::new(NoopDispatcher);
        let pex_stub: Arc<dyn harmony_app::iroh_invite_acceptor::IrohHandshakeDispatcher> =
            Arc::new(NoopDispatcher);
        let alice_dispatcher: Arc<dyn harmony_app::iroh_invite_acceptor::IrohHandshakeDispatcher> =
            Arc::new(MultiplexHandshakeDispatcher::new(
                invite_stub,
                alice_friend_acceptor,
                pex_stub,
            ));
        if alice_link_mgr
            .install_handshake_dispatcher(alice_dispatcher)
            .await
            .is_err()
        {
            panic!("first install must succeed (OnceCell empty)");
        }

        // ── 5. Alice mints + publishes a friend token; Bob redeems (real
        //    handshake over harmony/friend/v1). ──────────────────────────
        let minted_at = Hlc {
            wall_ms: 100_500,
            logical: 0,
            device_id: "alice-dev".into(),
        };
        let token_payload = mint_friend_token(
            alice.owner,
            Some("alice".to_string()),
            minted_at,
            None,
            alice.cert.clone(),
            &alice_device2,
        )
        .expect("mint friend token");
        let token_sig = token_payload.token.sig;
        let token_url = encode_friend_token_url(&token_payload).expect("encode friend token url");

        friend_pub.register_friend_token(&token_sig, None).await;
        crate::pkarr_visibility::await_invite_record_visible(&client, &token_sig).await;

        let bob_crdt_state = Arc::new(TokioMutex::new(OwnerState::default()));
        let bob_hlc_tracker = Arc::new(TokioMutex::new(harmony_crdt_sync::ReplayTracker::new(
            "bob-dev".to_string(),
        )));
        let bob_keytree =
            Arc::new(harmony_app::owner_state_crypto::KeyTree::derive(&[0xB4; 32]).expect("kt"));

        let outcome = harmony_app::connectivity_link_friend_iroh_inner(
            token_url,
            Some(Arc::clone(&pkarr_resolver)),
            Some(Arc::clone(&bob_ep)),
            bob.owner,
            bob.cert.clone(),
            Arc::clone(&bob_device2),
            Some("bob".to_string()),
            Arc::clone(&bob_crdt_state),
            Arc::clone(&bob_hlc_tracker),
            harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
            "bob-dev".to_string(),
            Arc::clone(&bob_keytree),
            harmony_app::revoked_device_projection::RevokedDeviceProjection::new(),
            harmony_app::HandshakeDialConfig {
                connect_timeout: Duration::from_millis(10_000),
                open_bi_timeout: Duration::from_millis(10_000),
                response_read_timeout: Duration::from_millis(10_000),
                write_timeout: Duration::from_millis(10_000),
            },
            None,
            None, // self_trust_doc — no revocations carried in this test
        )
        .await
        .expect("friend handshake must succeed");
        assert_eq!(outcome.friend_addr, alice.owner, "Bob befriended Alice");

        // ── Precondition: Bob's handshake-seeded cache holds exactly
        //    Alice's cert-derived #2 device (the cutoff test's starting
        //    point — mirrors dm_cert_identity_integration.rs's assertion 2).
        {
            let g = bob_crdt_state.lock().await;
            assert_eq!(
                g.friend_graph.friends.get(&alice.owner).map(|e| e.status),
                Some(FriendStatus::Active),
                "Bob holds Alice as an Active friend"
            );
            let entry = g
                .owner_device_cache
                .devices
                .get(&alice.owner)
                .expect("Bob cached Alice's device set from the real handshake");
            assert_eq!(
                entry.devices,
                vec![alice_d2_hash],
                "Bob's cache holds exactly Alice's #2 device"
            );
            assert_eq!(
                entry.device_identity_pubs,
                vec![Some(alice_d2_pub)],
                "Bob's cached pub is Alice's #2 combined pub"
            );
        }

        // ── 6. Seed the Alice<->Bob DM Space directly on both sides (same
        //    isolation simplification dm_cert_identity_integration.rs's
        //    assertions 3-4 use). ───────────────────────────────────────
        let content_key_ab = DmContentKey::new([0x5c; 32]);
        let space_ab = make_dm_space(SpaceId([0x81; 16]), alice.owner, bob.owner, content_key_ab);
        let dm_space_ab = space_ab.id;
        {
            let mut g = alice_crdt_state.lock().await;
            g.apply_space_with_canonicalization(space_ab.clone());
            assert!(g.spaces.contains_key(&dm_space_ab));
        }
        {
            let mut g = bob_crdt_state.lock().await;
            g.apply_space_with_canonicalization(space_ab);
            assert!(g.spaces.contains_key(&dm_space_ab));
        }

        // ── 7. Carol: seed her DM Space with Bob + her #2 cache entry on
        //    Bob's side directly via `apply_owner_device_update` — the same
        //    call the friend handshake makes, invoked without a second full
        //    handshake (see module doc for why this is a legitimate
        //    simplification here). ────────────────────────────────────────
        let carol_crdt_state = Arc::new(TokioMutex::new(OwnerState::default()));
        let content_key_cb = DmContentKey::new([0x5e; 32]);
        let space_cb = make_dm_space(SpaceId([0x82; 16]), carol.owner, bob.owner, content_key_cb);
        let dm_space_cb = space_cb.id;
        {
            let mut g = carol_crdt_state.lock().await;
            g.apply_space_with_canonicalization(space_cb.clone());
            assert!(g.spaces.contains_key(&dm_space_cb));
        }
        {
            let mut g = bob_crdt_state.lock().await;
            g.apply_space_with_canonicalization(space_cb);
            assert!(g.spaces.contains_key(&dm_space_cb));
            let seed_outcome = g.apply_owner_device_update(
                carol.owner,
                vec![carol_d2_hash],
                vec![Some(carol_d2_pub)],
                Vec::new(),
                Hlc {
                    wall_ms: 100_600,
                    logical: 0,
                    device_id: "bob-dev".into(),
                },
            );
            assert!(
                matches!(
                    seed_outcome,
                    harmony_app::owner_state_crdt::ApplyOutcome::Inserted
                ),
                "Bob's cache gained a fresh entry for Carol's #2 device"
            );
        }

        // ── 8. Shared CAS + real DmOutbox instances (Alice, Bob, Carol). ──
        let cas: Arc<dyn harmony_app::content_store::ContentStore> =
            Arc::new(harmony_app::content_store::InMemoryStub::default());

        let (alice_hash3, alice_sk3, alice_priv3) = dummy_hash3_and_key(0x31);
        // ZEB-710: Bob receives via `ingest_dm_packet` (device_id "bob-dev"), so
        // he needs no #3 transport identity nor a receive-side DmOutbox.
        let (carol_hash3, carol_sk3, carol_priv3) = dummy_hash3_and_key(0x33);
        assert_ne!(
            alice_hash3, alice_d2_hash,
            "Alice's #3 transport hash must differ from her #2 DM hash"
        );

        let mut alice_outbox = harmony_app::dm_outbox::DmOutbox::new(
            "alice-dev".into(),
            alice.owner,
            alice_hash3,
            alice_sk3,
            alice_priv3,
            Arc::clone(&alice_device2),
            alice.cert.clone(),
        );
        let mut carol_outbox = harmony_app::dm_outbox::DmOutbox::new(
            "carol-dev".into(),
            carol.owner,
            carol_hash3,
            carol_sk3,
            carol_priv3,
            carol_device2,
            carol.cert.clone(),
        );

        let captured_alice = Arc::new(StdMutex::new(Vec::<
            harmony_app::butler_deposit::ButlerDepositRequest,
        >::new()));
        alice_outbox.set_butler_deposit_client(Arc::new(CapturingDepositClient {
            captured: Arc::clone(&captured_alice),
        }));
        let captured_carol = Arc::new(StdMutex::new(Vec::<
            harmony_app::butler_deposit::ButlerDepositRequest,
        >::new()));
        carol_outbox.set_butler_deposit_client(Arc::new(CapturingDepositClient {
            captured: Arc::clone(&captured_carol),
        }));

        let events = Arc::new(StdMutex::new(Vec::<(String, serde_json::Value)>::new()));
        let sink: Arc<dyn harmony_app::node_event_sink::NodeEventSink> = Arc::new(RecordingSink {
            events: Arc::clone(&events),
        });

        let t0 = now_ms();

        // ── ASSERTION 1 (baseline): Alice's #2-signed CidNotify delivers
        //    via the REAL receive path with an EMPTY revocation projection.
        let body1 = b"baseline: alice's #2 is not revoked yet".to_vec();
        let (signed1, cid1, wire1) = send_and_capture_cidnotify(
            &mut alice_outbox,
            &alice_crdt_state,
            &cas,
            &captured_alice,
            dm_space_ab,
            body1.clone(),
            t0,
        )
        .await;
        assert_eq!(
            signed1.signing_device_hash, alice_d2_hash,
            "the REAL drain path signed the baseline CidNotify with Alice's #2"
        );

        let applied1 = harmony_app::dm_inbox_ingest::ingest_dm_packet(
            &bob_crdt_state,
            &cas,
            &sink,
            None,
            bob.owner,
            "bob-dev",
            [0u8; 32],
            &wire1,
            &RevokedDeviceProjection::new(),
            None,
        )
        .await
        .expect("baseline CidNotify must deliver via the live receive path");
        assert!(applied1, "baseline delivery is a fresh insert (Ok(true))");

        {
            let dm_received: Vec<serde_json::Value> = events
                .lock()
                .expect("events lock")
                .iter()
                .filter(|(name, _)| name == "dm-received")
                .map(|(_, p)| p.clone())
                .collect();
            assert_eq!(dm_received.len(), 1, "baseline CidNotify delivered");
            assert_eq!(
                dm_received[0]["body"].as_str().expect("body"),
                hex::encode(&body1),
                "Bob decrypted the exact baseline plaintext"
            );
        }
        {
            let g = bob_crdt_state.lock().await;
            assert!(
                g.inbox.values().any(|e| e.message_cid == cid1),
                "baseline message landed in Bob's inbox"
            );
        }

        // ── Revoke Alice's #2 ed25519 in Bob's shared-community projection
        //    (models `union_from_members` fed from a materialized
        //    DeviceRetire in a community Alice and Bob both belong to). ────
        let revoked = RevokedDeviceProjection::new();
        let alice_d2_ed25519: [u8; 32] = alice.device_key.verifying_key().to_bytes();
        // Sanity: this is exactly `combined_pub[32..64]`, the bytes
        // `verify_cidnotify_sender_binding` checks against the projection.
        assert_eq!(&alice_d2_pub[32..64], &alice_d2_ed25519[..]);
        let alice_revoked_keys = BTreeSet::from([alice_d2_ed25519]);
        revoked.union_from_members(std::iter::once((alice.owner, &alice_revoked_keys)));
        assert!(revoked.is_revoked(&alice.owner, &alice_d2_ed25519));

        // ── ASSERTION 2 (cutoff): the SAME #2 device sending again is now
        //    dropped by the REAL receive path — not delivered, not applied
        //    to the inbox (`SignerDeviceRevoked` inside
        //    `verify_cidnotify_sender_binding`). ────────────────────────────
        let body2 = b"post-revocation: this must be dropped".to_vec();
        let (signed2, cid2, wire2) = send_and_capture_cidnotify(
            &mut alice_outbox,
            &alice_crdt_state,
            &cas,
            &captured_alice,
            dm_space_ab,
            body2.clone(),
            t0 + 120_000,
        )
        .await;
        assert_eq!(
            signed2.signing_device_hash, alice_d2_hash,
            "the second send is STILL signed with Alice's (now-revoked) #2"
        );
        assert_ne!(cid2, cid1, "distinct message content -> distinct CID");

        let err = harmony_app::dm_inbox_ingest::ingest_dm_packet(
            &bob_crdt_state,
            &cas,
            &sink,
            None,
            bob.owner,
            "bob-dev",
            [0u8; 32],
            &wire2,
            &revoked,
            None,
        )
        .await
        .expect_err("a CidNotify from a revoked device must be cut off");
        assert!(
            err.contains("SignerDeviceRevoked"),
            "cutoff must come from verify_cidnotify_sender_binding (got: {err})"
        );

        {
            let dm_received_count = events
                .lock()
                .expect("events lock")
                .iter()
                .filter(|(name, _)| name == "dm-received")
                .count();
            assert_eq!(
                dm_received_count, 1,
                "post-revocation CidNotify must NOT deliver (still just the baseline event)"
            );
        }
        {
            let g = bob_crdt_state.lock().await;
            assert!(
                !g.inbox.values().any(|e| e.message_cid == cid2),
                "post-revocation message must NOT land in Bob's inbox (dropped, not acked)"
            );
        }

        // ── ASSERTION 3 (control): a DIFFERENT, non-revoked #2 device
        //    (Carol's) still delivers through the SAME `revoked` handle that
        //    already carries Alice's revoked key — the projection is keyed
        //    per (owner, ed25519), not a per-owner blanket drop. ───────────
        let carol_d2_ed25519: [u8; 32] = carol.device_key.verifying_key().to_bytes();
        assert!(
            !revoked.is_revoked(&carol.owner, &carol_d2_ed25519),
            "precondition: Carol's #2 was never revoked"
        );

        let body3 = b"control: carol's #2 was never revoked".to_vec();
        let (signed3, cid3, wire3) = send_and_capture_cidnotify(
            &mut carol_outbox,
            &carol_crdt_state,
            &cas,
            &captured_carol,
            dm_space_cb,
            body3.clone(),
            t0 + 240_000,
        )
        .await;
        assert_eq!(
            signed3.signing_device_hash, carol_d2_hash,
            "the control send is signed with Carol's #2 (a different owner+key than Alice's)"
        );

        let applied3 = harmony_app::dm_inbox_ingest::ingest_dm_packet(
            &bob_crdt_state,
            &cas,
            &sink,
            None,
            bob.owner,
            "bob-dev",
            [0u8; 32],
            &wire3,
            &revoked,
            None,
        )
        .await
        .expect("Carol's non-revoked #2 must still deliver through the same handle");
        assert!(applied3, "control delivery is a fresh insert (Ok(true))");

        {
            let dm_received: Vec<serde_json::Value> = events
                .lock()
                .expect("events lock")
                .iter()
                .filter(|(name, _)| name == "dm-received")
                .map(|(_, p)| p.clone())
                .collect();
            assert_eq!(
                dm_received.len(),
                2,
                "control CidNotify delivered (baseline + control; revoked message stays dropped)"
            );
            assert_eq!(
                dm_received[1]["body"].as_str().expect("body"),
                hex::encode(&body3),
                "Bob decrypted the exact control plaintext"
            );
        }
        {
            let g = bob_crdt_state.lock().await;
            assert!(
                g.inbox.values().any(|e| e.message_cid == cid3),
                "control message landed in Bob's inbox"
            );
            assert!(
                !g.inbox.values().any(|e| e.message_cid == cid2),
                "revoked message is still absent at the end of the test"
            );
        }

        // ── Teardown. ────────────────────────────────────────────────────
        publisher_handle.abort();
        drop(relay);
        alice_accept.abort();
        bob_accept.abort();
        alice_ep.shutdown().await;
        bob_ep.shutdown().await;
    })
    .await
    .expect("revoked_device2_dm_is_dropped_after_community_revocation timed out at 60s");
}

/// ZEB-685 (S3): the DM-only-contact analogue of the community-revocation gate
/// above. Proves the SAME real receive-path cutoff (`ingest_dm_packet` ->
/// `verify_cidnotify_admission` -> `verify_cidnotify_sender_binding`), but with
/// the `RevokedDeviceProjection` populated by the S3 receive-side handler
/// `dm_outbox::handle_revocation_push` — fed a REAL master-signed
/// `RevocationCert` + the device's real `EnrollmentCert` — instead of the
/// community `union_from_members` feed. No shared community is involved; the
/// friend push is the only revocation signal.
///
/// The [16]->[32] bridge is the composition this pins: `handle_revocation_push`
/// keys the projection on `enrollment.device_pubkeys.classical.ed25519_verify`,
/// while the cutoff reads the CidNotify signer's `identity_pub[32..64]` — these
/// must be the same 32 bytes or the cutoff silently misses. Alice's #2 is cached
/// on Bob's side directly via `apply_owner_device_update` (the blessed
/// no-second-handshake simplification the control leg above uses for Carol), so
/// this test needs no live iroh/pkarr machinery.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dm_only_contact_cutoff_via_revocation_push() {
    // ── 1. Two real-minted owners: Alice's device is revoked, Bob enforces. ──
    let alice = mint_real_owner(1_700_000_000);
    let bob = mint_real_owner(1_700_000_001);

    let alice_d2_hash = harmony_app::dm_signing::device2_signing_hash(&alice.cert)
        .expect("alice real cert yields a #2 hash");
    let alice_d2_pub = harmony_app::dm_signing::device2_combined_pub(&alice.cert);
    let alice_d2_ed25519: [u8; 32] = alice.device_key.verifying_key().to_bytes();
    assert_eq!(
        &alice_d2_pub[32..64],
        &alice_d2_ed25519[..],
        "the cutoff reads combined_pub[32..64]; the bridge must land on these bytes"
    );
    let alice_device2 = Arc::new(SigningKey::from_bytes(&alice.device_key.to_bytes()));

    let alice_crdt_state = Arc::new(TokioMutex::new(OwnerState::default()));
    let bob_crdt_state = Arc::new(TokioMutex::new(OwnerState::default()));

    // ── 2. Seed the Alice<->Bob DM Space on both sides + Alice's #2 device in
    //    Bob's OwnerDeviceCache (the receive-side signer-resolution source). ──
    let content_key = DmContentKey::new([0x6c; 32]);
    let space = make_dm_space(SpaceId([0x91; 16]), alice.owner, bob.owner, content_key);
    let dm_space_id = space.id;
    {
        let mut g = alice_crdt_state.lock().await;
        g.apply_space_with_canonicalization(space.clone());
    }
    {
        let mut g = bob_crdt_state.lock().await;
        g.apply_space_with_canonicalization(space);
        let seed = g.apply_owner_device_update(
            alice.owner,
            vec![alice_d2_hash],
            vec![Some(alice_d2_pub)],
            Vec::new(),
            Hlc {
                wall_ms: 100_000,
                logical: 0,
                device_id: "bob-dev".into(),
            },
        );
        assert!(
            matches!(seed, harmony_app::owner_state_crdt::ApplyOutcome::Inserted),
            "Bob's cache gained Alice's #2 device"
        );
    }

    // ── 3. Alice's + Bob's real DmOutbox instances (Alice sends, Bob receives). ──
    let cas: Arc<dyn harmony_app::content_store::ContentStore> =
        Arc::new(harmony_app::content_store::InMemoryStub::default());
    let (alice_hash3, alice_sk3, alice_priv3) = dummy_hash3_and_key(0x41);
    let mut alice_outbox = harmony_app::dm_outbox::DmOutbox::new(
        "alice-dev".into(),
        alice.owner,
        alice_hash3,
        alice_sk3,
        alice_priv3,
        Arc::clone(&alice_device2),
        alice.cert.clone(),
    );
    let captured_alice = Arc::new(StdMutex::new(Vec::<
        harmony_app::butler_deposit::ButlerDepositRequest,
    >::new()));
    alice_outbox.set_butler_deposit_client(Arc::new(CapturingDepositClient {
        captured: Arc::clone(&captured_alice),
    }));

    // ZEB-710: Bob receives via `ingest_dm_packet` (device_id "bob-dev"), so he
    // needs no #3 transport identity nor a receive-side DmOutbox.

    let events = Arc::new(StdMutex::new(Vec::<(String, serde_json::Value)>::new()));
    let sink: Arc<dyn harmony_app::node_event_sink::NodeEventSink> = Arc::new(RecordingSink {
        events: Arc::clone(&events),
    });

    let t0 = now_ms();

    // ── ASSERTION 1 (baseline): Alice's #2-signed CidNotify delivers via the
    //    REAL receive path with an EMPTY revocation projection. ─────────────
    let body1 = b"baseline: alice's device is not revoked yet".to_vec();
    let (signed1, cid1, wire1) = send_and_capture_cidnotify(
        &mut alice_outbox,
        &alice_crdt_state,
        &cas,
        &captured_alice,
        dm_space_id,
        body1.clone(),
        t0,
    )
    .await;
    assert_eq!(
        signed1.signing_device_hash, alice_d2_hash,
        "the REAL drain path signed the baseline CidNotify with Alice's #2"
    );
    let applied1 = harmony_app::dm_inbox_ingest::ingest_dm_packet(
        &bob_crdt_state,
        &cas,
        &sink,
        None,
        bob.owner,
        "bob-dev",
        [0u8; 32],
        &wire1,
        &RevokedDeviceProjection::new(),
        None,
    )
    .await
    .expect("baseline CidNotify must deliver via the live receive path");
    assert!(applied1, "baseline delivery is a fresh insert (Ok(true))");
    {
        // Assert BOTH the event and the inbox at the baseline, so ASSERTION 2's
        // "dm_received == 1" is a genuine delta (baseline delivered, revoked
        // dropped) rather than a delta-free check that a broken baseline could
        // also satisfy. (CodeRabbit #471.)
        let dm_received = events
            .lock()
            .expect("events lock")
            .iter()
            .filter(|(name, _)| name == "dm-received")
            .count();
        assert_eq!(
            dm_received, 1,
            "baseline CidNotify delivered exactly one event"
        );
        let g = bob_crdt_state.lock().await;
        assert!(
            g.inbox.values().any(|e| e.message_cid == cid1),
            "baseline message landed in Bob's inbox"
        );
    }

    // ── 4. Revoke Alice's device via the S3 DM-push path: build her REAL
    //    master-signed RevocationCert (reconstruct the master from her recovery
    //    artifact, exactly as revoke_device_inner does), pair it with her device
    //    EnrollmentCert, and feed it through the receive-side handler onto a
    //    FRESH projection. No community materialize is involved. ─────────────
    let proj = RevokedDeviceProjection::new();
    {
        let artifact = &alice.recovery_artifact;
        let master_pubkey = artifact.master_pubkey_bundle();
        let master_sk = artifact.master_signing_key();
        let revocation = harmony_owner::certs::RevocationCert::sign_master(
            &master_sk,
            master_pubkey,
            alice.cert.device_id,
            1_700_000_500,
            harmony_owner::certs::RevocationReason::Compromised,
        )
        .expect("sign master revocation");

        let mut bob_state = bob_crdt_state.lock().await;
        let inserted = harmony_app::dm_outbox::handle_revocation_push(
            &mut bob_state,
            alice.owner,
            &revocation,
            &alice.cert,
            &proj,
        )
        .expect("valid master-signed friend RevocationPush accepted");
        // The "newly inserted" signal the persistence fix depends on (the drain
        // marks the owner-state engine dirty only on a fresh insert). (CodeRabbit #471.)
        assert!(inserted, "first push of this device reports a new insert");
    }
    assert!(
        proj.is_revoked(&alice.owner, &alice_d2_ed25519),
        "the DM-push handler fed the EXACT ed25519 the cutoff keys on ([16]->[32] bridge)"
    );

    // ── ASSERTION 2 (cutoff): the same device sending again is now dropped by
    //    the SAME real receive path — the DM-push feed closed the gap. ───────
    let body2 = b"post-revocation-push: this must be dropped".to_vec();
    let (signed2, cid2, wire2) = send_and_capture_cidnotify(
        &mut alice_outbox,
        &alice_crdt_state,
        &cas,
        &captured_alice,
        dm_space_id,
        body2.clone(),
        t0 + 120_000,
    )
    .await;
    assert_eq!(
        signed2.signing_device_hash, alice_d2_hash,
        "the second send is STILL signed with Alice's (now-revoked) #2"
    );
    assert_ne!(cid2, cid1, "distinct content -> distinct CID");
    let err = harmony_app::dm_inbox_ingest::ingest_dm_packet(
        &bob_crdt_state,
        &cas,
        &sink,
        None,
        bob.owner,
        "bob-dev",
        [0u8; 32],
        &wire2,
        &proj,
        None,
    )
    .await
    .expect_err("a CidNotify from the revocation-pushed device must be cut off");
    assert!(
        err.contains("SignerDeviceRevoked"),
        "cutoff must come from verify_cidnotify_sender_binding (got: {err})"
    );
    {
        let dm_received = events
            .lock()
            .expect("events lock")
            .iter()
            .filter(|(name, _)| name == "dm-received")
            .count();
        assert_eq!(
            dm_received, 1,
            "post-push CidNotify must NOT deliver (only the baseline event remains)"
        );
    }
    {
        let g = bob_crdt_state.lock().await;
        assert!(
            !g.inbox.values().any(|e| e.message_cid == cid2),
            "post-push message must NOT land in Bob's inbox (cut off, not acked)"
        );
    }
}
