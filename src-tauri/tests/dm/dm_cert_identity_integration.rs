//! ZEB-580 S1 Task 7: end-to-end #2 (cert-anchored, enrolled-device) DM identity
//! round-trip — the ZEB-504 non-regression gate.
//!
//! This proves the full S1 stack works TOGETHER over the real two-node iroh
//! friend-handshake harness (built on `friend_token_roundtrip_integration.rs`),
//! using REAL `mint_owner` identities (non-zero X25519 — the ONLY identities
//! that exercise the #2 path; `mint_test_owner` ships a zeroed device X25519 and
//! silently degrades every party to #3, so it cannot cover this gate).
//!
//! Flow proven:
//!   1. Two owners A (Alice) + B (Bob) mint real identities and complete a real
//!      friend handshake over `harmony/friend/v1`.
//!   2. After the handshake, each side's `OwnerDeviceCache` holds the PEER's
//!      cert-derived #2 DM identity (`device2_signing_hash(cert)` →
//!      `Some(device2_combined_pub(cert))`) — the acceptor caches the requester's
//!      #2 in `process_friend_request`; the dialer caches the inviter's #2 in
//!      `apply_handshaked_friend`. NOT the self-asserted wire #3 bundle.
//!   3. Alice sends a DM to Bob. Its CidNotify is produced by the REAL send-side
//!      path — `drain() → push_deposit_candidate → build_cidnotify_packet_bytes →
//!      dm_signing_material()`, the exact production code that picks #2-vs-#3 (a
//!      capturing deposit-client mock hands us the signed wire bytes). Those
//!      bytes are stamped with Alice's #2 (asserted `!=` her #3 hash) and verify
//!      under her #2 combined pub. Bob then resolves the signer from his
//!      handshake-seeded cache — provably Alice's #2 (his cache holds exactly the
//!      #2 device; the #3 hash is absent).
//!   4. Bob receives + decrypts the message payload via the production
//!      `ingest_dm_packet` receive path fed the REAL drained CidNotify
//!      bytes (delivery not regressed) and emits `dm-received` with the
//!      round-tripped plaintext.
//!   5. The invite the REAL deposit path rebuilds (`build_invite_packet_bytes →
//!      dm_invite_material()`) carries `inviter_enrollment = Some(A #2 cert)`, and
//!      Bob's REAL accept path (`DmOutbox::handle_invite → apply_invite →
//!      run_invite_accept_tail`) auto-accepts it (Bob is Alice's active friend
//!      from the handshake) — bootstrapping the DM Space AND caching Alice's #2
//!      (`device2_signing_hash(cert)` → `Some(device2_combined_pub(cert))`, no #3).
//!
//! ## Coverage (per the Task 7 brief's "assert the strongest subset")
//!
//! All five assertions are covered end-to-end against production code paths: the
//! real iroh friend handshake (1, 2), the real send-side signing selection via
//! `drain`/`dm_signing_material` (3), the real `ingest_dm_packet` receive
//! path (4), and the real `handle_invite → apply_invite` cert-anchored accept
//! path (5). Both wire packets (CidNotify + invite) are pulled from the REAL
//! deposit rung, so a regression leaving the sender on #3 surfaces here.
//!
//! The one substitution is TRANSPORT, not logic: the deposit-capture mock + a
//! shared in-memory CAS stand in for the live iroh DM tunnel / butler carrier
//! (an in-process tunnel is out of this harness's scope). The bytes crossing that
//! boundary are the exact production-signed packets. Live first-contact over a
//! real cross-WAN tunnel remains the ZEB-504 live-fleet shape.

use std::collections::BTreeMap;
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
    ContentId, DeviceIdentityHash, DmContentKey, Hlc, OwnerAddr, OwnerDeviceEntry, Space, SpaceId,
    SpaceKind,
};
use harmony_app::reachability_resolver::ReachabilityResolver;
use harmony_app::zenoh_iroh_transport::IrohZenohLinkManager;
use iroh::endpoint::{presets, Endpoint, RelayMode};
use iroh::SecretKey;
use tokio::sync::Mutex as TokioMutex;
use zenoh_link::LinkUnicast;

/// A real minted owner, shaped like `community_membership::TestOwner` but built
/// from `harmony_owner::lifecycle::mint_owner` so its device cert carries a REAL
/// (non-zero) X25519 pub — the precondition for `device2_signing_hash` to yield
/// a #2 identity instead of degrading to #3.
struct RealOwner {
    owner: OwnerAddr,
    /// The enrolled device's #2 signing key (Ed25519). Its verifying key matches
    /// `cert.device_pubkeys.classical.ed25519_verify`.
    device_key: SigningKey,
    cert: harmony_owner::certs::EnrollmentCert,
}

/// Mint a real owner and select the EnrollmentCert belonging to its enrolled
/// device signing key (NOT `.values().next()` — if `mint_owner` ever enrolls
/// more than one device, an arbitrary pick would return a cert whose key we
/// don't hold; mirrors `dm_signing::tests::minted_device_cert`).
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
    // The cert's X25519 half must be non-zero, else this whole test would
    // silently exercise the #3 degrade path and prove nothing about #2.
    assert_ne!(
        cert.device_pubkeys.classical.x25519_pub, [0u8; 32],
        "mint_owner must ship a real (non-zero) device X25519 — the #2 precondition"
    );
    RealOwner {
        owner: OwnerAddr(cert.owner_id),
        device_key: minted.device_signing_key,
        cert,
    }
}

/// Build a hermetic iroh endpoint on loopback (no address-lookup, no pkarr
/// publisher, no DERP relays), registering the zenoh + handshake + friend ALPNs.
/// Copied from `friend_token_roundtrip_integration.rs` (sibling module in this
/// same `dm_tests` binary; its helpers are module-private).
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

/// 64-byte composite identity_pub (`X25519_pub(32) ‖ Ed25519_pub(32)`) for a
/// transport signing key (used only to sign the Case-A pkarr routing record).
fn identity_pub_for(sk: &SigningKey) -> [u8; 64] {
    let x25519_priv = harmony_app::dm_signing::ed25519_priv_to_x25519(sk);
    let x25519_pub = x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(*x25519_priv));
    let ed25519_pub = sk.verifying_key().to_bytes();
    let mut combined = [0u8; 64];
    combined[..32].copy_from_slice(x25519_pub.as_bytes());
    combined[32..].copy_from_slice(&ed25519_pub);
    combined
}

/// Wait (≤5s) for Alice's Case-A pkarr record (keyed on the friend token's
/// signature for the current epoch) to become visible in the mock relay.
async fn await_pkarr_record_visible(
    pkarr_resolver: &harmony_pkarr::PkarrResolver,
    token_sig: &[u8; 64],
) {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time")
        .as_millis() as u64;
    let epoch_id = harmony_pkarr::current_epoch_id(now_ms);
    let probe_signing = harmony_pkarr::derive_ephemeral_key(
        harmony_pkarr::PkarrCase::Invite,
        token_sig,
        &epoch_id.to_be_bytes(),
    );
    let probe_verifying = probe_signing.verifying_key();
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Ok(Some(_)) = pkarr_resolver.resolve(&probe_verifying).await {
            return;
        }
    }
    panic!("alice's friend-token pkarr record did not appear within 5s");
}

/// No-op dispatcher for the multiplexer's invite + PEX slots (only the friend
/// ALPN is ever dialed in this test).
struct NoopDispatcher;

#[async_trait::async_trait]
impl harmony_app::iroh_invite_acceptor::IrohHandshakeDispatcher for NoopDispatcher {
    async fn handle_connection(&self, _conn: iroh::endpoint::Connection) {}
}

/// Records emitted node events so the test can assert the `dm-received` event.
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

/// Capturing butler-deposit mock: records every `ButlerDepositRequest` the REAL
/// drain path builds (so the test can pull out the production-signed CidNotify +
/// invite wire bytes) and reports `Failed` so the outbox entry stays Pending
/// (no state mutation from the capture itself).
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

/// A DM `Space` with a fixed content_key, shared byte-for-byte by both parties
/// so their AAD (`space.dedupe_key()`) — and thus decryption — agree. Mirrors
/// `dm_thread_integration::make_dm_space`.
fn make_dm_space(id: SpaceId, a: OwnerAddr, b: OwnerAddr, content_key: DmContentKey) -> Space {
    let mut members = vec![a, b];
    members.sort();
    Space {
        id,
        kind: SpaceKind::Dm,
        parent: None,
        community_id: None,
        name: "Alice <-> Bob".into(),
        transport: None,
        members,
        custom_name: None,
        notification_pref: None,
        left_at: None,
        created_at: Hlc {
            wall_ms: 100,
            logical: 0,
            device_id: "alice-dev".into(),
        },
        updated_at: Hlc {
            wall_ms: 100,
            logical: 0,
            device_id: "alice-dev".into(),
        },
        content_key: Some(content_key),
        prior_content_keys: vec![],
        current_epoch: None,
        current_epoch_key: None,
        old_epoch_keys: BTreeMap::new(),
        admin_addr: None,
        is_invite_only: None,
        shared_in_profile: false,
        pending_join_at: None,
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time")
        .as_millis() as u64
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cert_anchored_dm_roundtrip_end_to_end() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("harmony_app=warn")),
        )
        .with_test_writer()
        .try_init();

    // Pre-pay iroh's first-bind global init OUTSIDE the asserted budget (ZEB-374
    // pattern) so full-suite contention can't push the handshake past timeout.
    harmony_app::iroh_endpoint::warm_up_iroh_global_init().await;

    tokio::time::timeout(Duration::from_secs(60), async {
        // ── 1. Real identities (non-zero X25519 → #2 path is live). ──────
        let alice = mint_real_owner(1_700_000_000);
        let bob = mint_real_owner(1_700_000_001);
        assert_ne!(alice.owner, bob.owner, "distinct minted owners");
        let alice_device2 = Arc::new(SigningKey::from_bytes(&alice.device_key.to_bytes()));
        let bob_device2 = Arc::new(SigningKey::from_bytes(&bob.device_key.to_bytes()));

        // Precompute each party's cert-derived #2 identity (hash + combined pub)
        // — the values the handshake MUST cache and the DM MUST sign with.
        let alice_d2_hash = harmony_app::dm_signing::device2_signing_hash(&alice.cert)
            .expect("alice real cert yields a #2 hash");
        let alice_d2_pub = harmony_app::dm_signing::device2_combined_pub(&alice.cert);
        let bob_d2_hash = harmony_app::dm_signing::device2_signing_hash(&bob.cert)
            .expect("bob real cert yields a #2 hash");
        let bob_d2_pub = harmony_app::dm_signing::device2_combined_pub(&bob.cert);

        // Alice's transport signing key (pkarr record signer) — independent of
        // her owner identity (identity-match is skipped for friend tokens).
        let alice_transport_sk = SigningKey::from_bytes(&[0x5a; 32]);
        let alice_transport_pub = identity_pub_for(&alice_transport_sk);

        // ── 2. Iroh endpoints + link managers (one per "process"). ───────
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

        // ── 3. Alice acceptor dependencies. ──────────────────────────────
        let alice_crdt_state = Arc::new(TokioMutex::new(OwnerState::default()));
        let alice_hlc_tracker = Arc::new(TokioMutex::new(harmony_crdt_sync::ReplayTracker::new(
            "alice-dev".to_string(),
        )));
        let alice_keytree =
            Arc::new(harmony_app::owner_state_crypto::KeyTree::derive(&[0xA1; 32]).expect("kt"));

        // ── 4. Mock pkarr relay + Case-A friend-token publisher. ─────────
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

        // ── 5. Install Alice's friend acceptor behind the multiplexer. ───
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

        // ── 6. Alice mints + publishes a friend token. ───────────────────
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
        await_pkarr_record_visible(&pkarr_resolver, &token_sig).await;

        // ── 7. Bob redeems (real handshake over harmony/friend/v1). ───────
        let bob_crdt_state = Arc::new(TokioMutex::new(OwnerState::default()));
        let bob_hlc_tracker = Arc::new(TokioMutex::new(harmony_crdt_sync::ReplayTracker::new(
            "bob-dev".to_string(),
        )));
        let bob_keytree =
            Arc::new(harmony_app::owner_state_crypto::KeyTree::derive(&[0xB2; 32]).expect("kt"));

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

        // ── ASSERTION 1: both sides are mutual Active friends. ───────────
        {
            let g = bob_crdt_state.lock().await;
            assert_eq!(
                g.friend_graph.friends.get(&alice.owner).map(|e| e.status),
                Some(FriendStatus::Active),
                "Bob holds Alice as an Active friend"
            );
        }
        {
            let g = alice_crdt_state.lock().await;
            assert_eq!(
                g.friend_graph.friends.get(&bob.owner).map(|e| e.status),
                Some(FriendStatus::Active),
                "Alice holds Bob as an Active friend"
            );
        }

        // ── ASSERTION 2: each side cached the PEER's cert-derived #2. ─────
        // Helper: the handshake caches the peer as EXACTLY the singleton #2
        // device (`device2_signing_hash → Some(device2_combined_pub)`) — proving
        // the cert-attested #2 seeded the cache, NOT some other device (a #3
        // transport hash would make this a >1-entry or different-hash set).
        let assert_cached_device2 = |cache_entry: &OwnerDeviceEntry,
                                     d2_hash: DeviceIdentityHash,
                                     d2_pub: [u8; 64],
                                     who: &str| {
            assert_eq!(
                cache_entry.devices,
                vec![d2_hash],
                "{who}: cache holds exactly the peer's #2 device (singleton — not a #3 hash)"
            );
            assert_eq!(
                cache_entry.device_identity_pubs,
                vec![Some(d2_pub)],
                "{who}: cached pub is the peer's #2 combined pub"
            );
        };
        {
            let g = bob_crdt_state.lock().await;
            let entry = g
                .owner_device_cache
                .devices
                .get(&alice.owner)
                .expect("Bob cached Alice's device set");
            assert_cached_device2(entry, alice_d2_hash, alice_d2_pub, "Bob→Alice");
        }
        {
            let g = alice_crdt_state.lock().await;
            let entry = g
                .owner_device_cache
                .devices
                .get(&bob.owner)
                .expect("Alice cached Bob's device set");
            assert_cached_device2(entry, bob_d2_hash, bob_d2_pub, "Alice→Bob");
        }

        // ── DM round-trip fixture: a shared DM Space + shared CAS. ────────
        // The DM Space is normally bootstrapped by a DmInvite (assertion 5);
        // here we seed it directly on both sides (byte-identical struct → equal
        // AAD) so assertions 3+4 isolate the #2 SIGN→VERIFY→DECRYPT path against
        // the handshake-seeded cache. The blob rides a shared in-memory CAS
        // (content-addressed store; the live tunnel/deposit carries it — sharing
        // the CAS is the in-process stand-in for that transport).
        let content_key = DmContentKey::new([0x5c; 32]);
        let space_seed = make_dm_space(SpaceId([0x77; 16]), alice.owner, bob.owner, content_key);
        let dm_space_id = {
            let mut g = alice_crdt_state.lock().await;
            g.apply_space_with_canonicalization(space_seed.clone());
            // Read back the canonical id (canonicalization is deterministic on
            // members, so Bob's insert lands on the same id).
            g.spaces
                .values()
                .find(|s| s.kind == SpaceKind::Dm)
                .map(|s| s.id)
                .expect("alice has the DM space")
        };
        {
            let mut g = bob_crdt_state.lock().await;
            g.apply_space_with_canonicalization(space_seed);
            assert!(
                g.spaces.contains_key(&dm_space_id),
                "Bob's DM space canonicalizes to the same id as Alice's"
            );
        }

        let cas: Arc<dyn harmony_app::content_store::ContentStore> =
            Arc::new(harmony_app::content_store::InMemoryStub::default());

        // Alice's REAL DmOutbox — built via `DmOutbox::new` with her real cert,
        // so `dm_signing_material()` selects her #2 (community_signing_key =
        // enrolled device key, hash = device2_signing_hash(cert)). The #3 fields
        // are a distinct self-consistent transport identity (never exercised on
        // this path, but must differ from #2 so "not #3" is a real assertion).
        let alice_priv3 = Arc::new(harmony_identity::PrivateIdentity::from_seed(&[0x31; 32]));
        let alice_hash3 = DeviceIdentityHash(alice_priv3.public_identity().address_hash);
        assert_ne!(
            alice_hash3, alice_d2_hash,
            "Alice's #3 transport hash must differ from her #2 DM hash"
        );
        let alice_priv3_bytes = alice_priv3.to_private_bytes();
        let alice_sk3 = Arc::new(SigningKey::from_bytes(
            &<[u8; 32]>::try_from(&alice_priv3_bytes[32..64]).expect("ed25519 seed"),
        ));
        let alice_d2_key = Arc::new(SigningKey::from_bytes(&alice.device_key.to_bytes()));
        let mut alice_outbox = harmony_app::dm_outbox::DmOutbox::new(
            "alice-dev".into(),
            alice.owner,
            alice_hash3,
            alice_sk3,
            Arc::clone(&alice_priv3),
            Arc::clone(&alice_d2_key),
            alice.cert.clone(),
        );

        // Alice sends the DM: encrypt + store blob in the shared CAS + mint an
        // OutboxEntry. Returns the message_cid the CidNotify references.
        let body = b"hello bob, this is a #2-signed DM".to_vec();
        let (_msg_id, message_cid): (_, ContentId) = {
            let mut g = alice_crdt_state.lock().await;
            alice_outbox
                .send_dm(
                    &mut g,
                    cas.as_ref(),
                    dm_space_id,
                    body.clone(),
                    "text/plain".into(),
                    now_ms(),
                    None,
                )
                .await
                .expect("alice send_dm ok")
        };

        // ── ASSERTION 3 (send side): drive the CidNotify through the REAL
        //    send path so the production #2-vs-#3 selection actually runs.
        //    `send_dm` only encrypts + enqueues; the signing decision lives in
        //    `build_cidnotify_packet_bytes` (which calls `dm_signing_material()`),
        //    reached via `drain → push_deposit_candidate`. We install a capturing
        //    butler-deposit mock and drain twice — the first drain's transient
        //    failure records an AttemptState, the second (after the 5s backoff
        //    window) has `pre_failure_count >= 1` and therefore builds + deposits
        //    the real signed packet. A regression that left Alice signing #3
        //    would be caught right here.
        let captured = Arc::new(StdMutex::new(Vec::<
            harmony_app::butler_deposit::ButlerDepositRequest,
        >::new()));
        alice_outbox.set_butler_deposit_client(Arc::new(CapturingDepositClient {
            captured: Arc::clone(&captured),
        }));
        let deposit_transport = harmony_app::dm_outbox::DepositOnlyDmTransport;
        let t0 = now_ms();
        {
            let mut g = alice_crdt_state.lock().await;
            alice_outbox.drain(&mut g, &deposit_transport, t0).await;
        }
        {
            let mut g = alice_crdt_state.lock().await;
            // +60s ≫ the 5s backoff window, so the (entry, recipient) pair is due.
            alice_outbox
                .drain(&mut g, &deposit_transport, t0 + 60_000)
                .await;
        }
        let deposit_req = {
            let g = captured.lock().expect("captured lock");
            g.iter()
                .find(|r| r.cidnotify_packet.is_some())
                .cloned()
                .expect("the REAL drain path built + deposited a CidNotify candidate")
        };
        assert_eq!(
            deposit_req.message_cid,
            Some(message_cid),
            "the deposited candidate references the message Alice just sent"
        );
        let cidnotify_wire = deposit_req
            .cidnotify_packet
            .clone()
            .expect("deposit carries the CidNotify wire bytes");
        // The deposit path also fail-closed-rebuilds the bootstrap invite via
        // `build_invite_packet_bytes → dm_invite_material()`; capture it for
        // assertion 5 (also production-signed #2 bytes).
        let invite_wire = deposit_req
            .invite_packet
            .clone()
            .expect("deposit carries the rebuilt DmInvite wire bytes");

        let (rx_signed, rx_signature, rx_signed_bytes) =
            match harmony_app::dm_envelope::decode_packet(&cidnotify_wire)
                .expect("decode cidnotify")
            {
                harmony_app::dm_envelope::DmPacket::CidNotify {
                    signed,
                    signature,
                    signed_bytes,
                } => (signed, signature, signed_bytes),
                other => panic!("expected CidNotify, got {other:?}"),
            };
        // The REAL builder stamped Alice's #2 DM hash, NOT her #3 transport hash,
        // and the drained bytes verify under her #2 combined pub.
        assert_eq!(
            rx_signed.signing_device_hash, alice_d2_hash,
            "the REAL drain path signed the CidNotify with Alice's #2 DM identity"
        );
        assert_ne!(
            rx_signed.signing_device_hash, alice_hash3,
            "the REAL drain path did NOT sign with Alice's #3 transport identity"
        );
        harmony_app::dm_signing::verify_dm_packet_signature(
            &rx_signed_bytes,
            &rx_signature,
            &alice_d2_pub,
            rx_signed.signing_device_hash,
        )
        .expect("the drained CidNotify bytes verify under Alice's #2 combined pub");

        // ── ASSERTION 3 (receive side): Bob resolves the signer from his
        //    handshake-seeded cache and it is provably Alice's #2. ─────────
        // `verify_dm_packet_signature` against the cached #2 combined pub is the
        // exact check `verify_cidnotify_admission` performs internally after
        // `lookup_pubkey_for_device` returns this pub. Pinning it here proves the
        // resolved pub is the #2 pub (a #3 pub would fail signature verification).
        {
            let g = bob_crdt_state.lock().await;
            let entry = g.owner_device_cache.devices.get(&alice.owner).unwrap();
            let idx = entry
                .devices
                .iter()
                .position(|d| *d == rx_signed.signing_device_hash)
                .expect("Bob's cache resolves the CidNotify's #2 signing device");
            let resolved_pub = entry.device_identity_pubs[idx].expect("cached #2 pub present");
            assert_eq!(
                resolved_pub, alice_d2_pub,
                "Bob resolves Alice's #2 combined pub (NOT a #3 pub)"
            );
            harmony_app::dm_signing::verify_dm_packet_signature(
                &rx_signed_bytes,
                &rx_signature,
                &resolved_pub,
                rx_signed.signing_device_hash,
            )
            .expect("CidNotify verifies against the cached #2 combined pub");
        }

        // ── ASSERTION 4: Bob receives + decrypts via the PRODUCTION receive
        //    path (`ingest_dm_packet`), emitting `dm-received`. ─────────────
        let events = Arc::new(StdMutex::new(Vec::<(String, serde_json::Value)>::new()));
        let sink: Arc<dyn harmony_app::node_event_sink::NodeEventSink> = Arc::new(RecordingSink {
            events: Arc::clone(&events),
        });
        // Bob's real DmOutbox — used by the ASSERTION 5 accept path below.
        let bob_priv3 = Arc::new(harmony_identity::PrivateIdentity::from_seed(&[0x32; 32]));
        let bob_hash3 = DeviceIdentityHash(bob_priv3.public_identity().address_hash);
        let bob_priv3_bytes = bob_priv3.to_private_bytes();
        let bob_sk3 = Arc::new(SigningKey::from_bytes(
            &<[u8; 32]>::try_from(&bob_priv3_bytes[32..64]).expect("ed25519 seed"),
        ));
        let bob_d2_key = Arc::new(SigningKey::from_bytes(&bob.device_key.to_bytes()));
        let bob_outbox = Arc::new(TokioMutex::new(harmony_app::dm_outbox::DmOutbox::new(
            "bob-dev".into(),
            bob.owner,
            bob_hash3,
            bob_sk3,
            Arc::clone(&bob_priv3),
            bob_d2_key,
            bob.cert.clone(),
        )));

        // The LIVE tunnel receive path takes the raw signed packet bytes (it
        // decodes internally), so feed the drained `cidnotify_wire` directly.
        // `bob_outbox` is not consumed here — the tunnel carrier owes no
        // device-cache refresh + ack fan-out; it survives for ASSERTION 5.
        let applied = harmony_app::dm_inbox_ingest::ingest_dm_packet(
            &bob_crdt_state,
            &cas,
            &sink,
            None,
            bob.owner,
            "bob-dev",
            [0u8; 32],
            &cidnotify_wire,
            &harmony_app::revoked_device_projection::RevokedDeviceProjection::new(),
            None,
        )
        .await
        .expect("Bob ingests the drained CidNotify via the production receive path");
        assert!(applied, "a newly-applied DM emits dm-received (Ok(true))");

        let dm_received: Vec<serde_json::Value> = events
            .lock()
            .expect("events lock")
            .iter()
            .filter(|(name, _)| name == "dm-received")
            .map(|(_, p)| p.clone())
            .collect();
        assert_eq!(
            dm_received.len(),
            1,
            "exactly one dm-received event delivered to Bob"
        );
        let ev = &dm_received[0];
        assert_eq!(
            ev["body"].as_str().expect("body"),
            hex::encode(&body),
            "Bob decrypted the exact plaintext Alice sent"
        );
        assert_eq!(
            ev["from"].as_str().expect("from"),
            hex::encode(alice.owner.0),
            "dm-received is attributed to Alice"
        );
        // Delivery landed in Bob's CRDT inbox (not regressed).
        {
            let g = bob_crdt_state.lock().await;
            assert!(
                g.inbox.values().any(|e| e.message_cid == message_cid),
                "Bob's CRDT inbox holds the decrypted message"
            );
        }

        // ── ASSERTION 5: the REAL deposit-rebuilt DmInvite carries Alice's #2
        //    cert, and Bob's REAL accept path caches Alice's #2. ────────────
        // The invite bytes come from the same drain/deposit candidate captured
        // above (`build_invite_packet_bytes → dm_invite_material()`), so they are
        // production-signed #2 bytes — not hand-built.
        let (inv_signed, inv_signature, inv_signed_bytes) =
            match harmony_app::dm_envelope::decode_packet(&invite_wire).expect("decode invite") {
                harmony_app::dm_envelope::DmPacket::Invite {
                    signed,
                    signature,
                    signed_bytes,
                } => (signed, signature, signed_bytes),
                other => panic!("expected Invite, got {other:?}"),
            };
        // 5a. The rebuilt invite attaches Alice's #2 EnrollmentCert.
        assert_eq!(
            inv_signed.inviter_enrollment,
            Some(Box::new(alice.cert.clone())),
            "deposit-rebuilt invite attaches Alice's #2 EnrollmentCert"
        );

        // 5b. Drive it through Bob's REAL accept path
        // (`handle_invite → apply_invite → run_invite_accept_tail`), which runs
        // the cert-anchored gates (verify_enrollment_any_issuer, cert-hash ==
        // signing_device_hash, Check B) and then writes the Space + caches #2.
        //
        // Bob is Alice's active friend (assertion 1), so apply_invite AUTO-ACCEPTS
        // rather than staging. We run against a copy of Bob's real post-handshake
        // state with Alice's device-cache entry + the DM Space removed, so the
        // accept path's cert-gated cache WRITE is observable rather than masked by
        // the handshake-seeded #2 cache (assertion 2) and the seeded Space
        // (assertions 3–4). Alice stays an active friend, so the cert-path is
        // exercised end to end.
        let mut bob_accept_state = { bob_crdt_state.lock().await.clone() };
        bob_accept_state
            .owner_device_cache
            .devices
            .remove(&alice.owner);
        bob_accept_state.spaces.remove(&dm_space_id);
        assert!(
            !bob_accept_state
                .owner_device_cache
                .devices
                .contains_key(&alice.owner),
            "precondition: no cached device for Alice before the invite is applied"
        );
        assert!(
            !bob_accept_state.spaces.contains_key(&dm_space_id),
            "precondition: DM Space absent before the invite is applied"
        );

        {
            let mut ob = bob_outbox.lock().await;
            ob.handle_invite(
                &mut bob_accept_state,
                inv_signed,
                inv_signature,
                &inv_signed_bytes,
                now_ms(),
            )
            .await
            .expect("handle_invite (real apply_invite #2 cert path) must accept");
        }

        // Accepted (not Staged): the DM Space was bootstrapped AND Alice's #2 was
        // cached from the cert.
        assert!(
            bob_accept_state.spaces.contains_key(&dm_space_id),
            "apply_invite bootstrapped the DM Space (auto-accept, not staged)"
        );
        let accepted_entry = bob_accept_state
            .owner_device_cache
            .devices
            .get(&alice.owner)
            .expect("apply_invite cached Alice's device set from the cert");
        assert_eq!(
            accepted_entry.devices,
            vec![alice_d2_hash],
            "apply_invite cached exactly Alice's #2 device (singleton — not a #3 hash)"
        );
        let acc_idx = accepted_entry
            .devices
            .iter()
            .position(|d| *d == alice_d2_hash)
            .expect("cached #2 device present");
        assert_eq!(
            accepted_entry.device_identity_pubs[acc_idx],
            Some(alice_d2_pub),
            "apply_invite cached Alice's #2 combined pub (not a #3)"
        );

        // ── Teardown. ────────────────────────────────────────────────────
        publisher_handle.abort();
        drop(relay);
        alice_accept.abort();
        bob_accept.abort();
        alice_ep.shutdown().await;
        bob_ep.shutdown().await;
    })
    .await
    .expect("cert_anchored_dm_roundtrip_end_to_end timed out at 60s");
}
