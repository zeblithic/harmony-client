//! ZEB-325 Phase 2c (option A): two-process integration test for the
//! direct iroh bi-stream invite handshake.
//!
//! Spins up two `IrohZenohLinkManager`s (one per simulated process)
//! with real iroh endpoints on loopback. Alice owns an admin community
//! engine + the production `IrohInviteHandshakeAcceptor` wired into
//! her link manager. Bob runs `connectivity_redeem_invite_iroh_inner`
//! against Alice's pkarr-published routing record — except this test
//! short-circuits pkarr by seeding Bob's `ReachabilityResolver`
//! directly (pkarr is covered by the Task 2 publish/resolve tests in
//! this same file's `pkarr_invite_redemption_integration.rs` peer).
//!
//! The flow exercised end-to-end:
//!
//! 1. Bob's IPC opens an iroh QUIC stream to Alice on the
//!    `harmony/handshake/v1` ALPN.
//! 2. Bob writes the length-prefixed CommunityInviteSigned packet
//!    (envelope+signature; same `0x10` discriminant production uses).
//! 3. Alice's acceptor dispatches the connection via the link
//!    manager's accept loop, decodes the packet, runs
//!    `community_invite::handle_unicast` against her engine. The
//!    insert triggers the existing auto-counter-sign post-Inserted
//!    hook (ZEB-254 Task 10).
//! 4. The acceptor polls Alice's engine state for the JoinCountersign,
//!    CBOR-encodes the event, writes the length-prefixed response,
//!    and finish()es the send half.
//! 5. Bob's IPC reads the response, calls
//!    `redeem_invite_inner_with_overrides` with `pre_minted` (so the
//!    bootstrap_join.id matches what was on the wire) and
//!    `pre_delivered_countersign`. The inner inserts the pre-delivered
//!    countersign into Bob's engine right after his local PendingJoin
//!    insert; the post-Inserted hook fires the registered oneshot on
//!    the JoinCountersign's `target_event_id`, and the await resolves
//!    immediately.
//! 6. Bob's IPC returns `outcome.status == "joined"`.

use std::collections::{BTreeMap, HashMap};
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::{Signer, SigningKey};
use harmony_app::community_channel_log_engine::{
    ChannelLogEngineConfig, ChannelLogRegistry, ChannelLogRegistryConfig,
};
use harmony_app::community_invite::{
    self, canonical_invite_token_bytes, CommunityInvitePayload, InviteEpochSnapshot, InviteToken,
    MaterializedCommunityState,
};
use harmony_app::community_membership::{materialize, MemberStatus};
use harmony_app::community_state_sync::{
    CommunityRegistryConfig, CommunitySyncRegistry, IdentityResolver, DEFAULT_DEBOUNCE_MS,
};
use harmony_app::content_store::{CasOp, ContentStore, RuntimeContentStore};
use harmony_app::dm_outbox::{DmOutbox, UnicastSendRequest};
use harmony_app::event_loop::{ChannelLogAdapterRequest, CommunityAdapterRequest};
use harmony_app::iroh_endpoint::{alpn, IrohEndpoint};
use harmony_app::iroh_invite_acceptor::IrohInviteHandshakeAcceptor;
use harmony_app::owner_state_crdt::OwnerState;
use harmony_app::owner_state_types::{DeviceIdentityHash, Hlc, OwnerAddr};
use harmony_app::reachability_record::ReachabilityAnnouncePayload;
use harmony_app::reachability_resolver::ReachabilityResolver;
use harmony_app::zenoh_iroh_transport::IrohZenohLinkManager;
use harmony_identity::PrivateIdentity;
use iroh::endpoint::{presets, Endpoint, RelayMode};
use iroh::SecretKey;
use tokio::sync::{mpsc, Mutex as TokioMutex};
use zenoh_link::LinkUnicast;

// ────────────────────────────────────────────────────────────────────────────
// ZEB-325 PR #159 F10: the prior version of this test mutated
// `HARMONY_REDEEM_INVITE_TIMEOUT_MS` via `std::env::set_var` (unsafe in
// multithreaded contexts under Rust 2024). The value being set (5000ms)
// equaled the default the inner reads, so the mutation was a no-op for
// behaviour. It has been removed; the test relies on the inner's default
// timeout and on the dialer / acceptor configs threaded explicitly
// through `HandshakeDialConfig` and `HandshakeAcceptorConfig` below.
// ────────────────────────────────────────────────────────────────────────────

// ────────────────────────────────────────────────────────────────────────────
// Two-owner identity resolver — both engines need to resolve each
// other's OwnerAddr → composite identity_pub for verify_event.
// ────────────────────────────────────────────────────────────────────────────

struct TwoIdentityResolver {
    alice: (OwnerAddr, [u8; 64]),
    bob: (OwnerAddr, [u8; 64]),
}

#[async_trait::async_trait]
impl IdentityResolver for TwoIdentityResolver {
    async fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
        if *addr == self.alice.0 {
            Some(self.alice.1)
        } else if *addr == self.bob.0 {
            Some(self.bob.1)
        } else {
            None
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Helpers (mirrored from the original Task-6 fixtures with naming
// preserved so the diff is reviewable against that reverted commit).
// ────────────────────────────────────────────────────────────────────────────

/// Extract the canonical 32-byte ed25519 seed from a `PrivateIdentity`
/// (bytes 32..64 of `to_private_bytes()`).
fn signing_key_from(identity: &PrivateIdentity) -> SigningKey {
    let priv_bytes = identity.to_private_bytes();
    let mut secret = [0u8; 32];
    secret.copy_from_slice(&priv_bytes[32..64]);
    SigningKey::from_bytes(&secret)
}

/// `PrivateIdentity` is `!Clone`; round-trip via private bytes to get a
/// byte-identical instance.
fn dup_identity(src: &PrivateIdentity) -> PrivateIdentity {
    PrivateIdentity::from_private_bytes(&src.to_private_bytes())
        .expect("PrivateIdentity round-trip via to/from_private_bytes")
}

/// Derive `(OwnerAddr, 64-byte composite identity_pub)` from an Ed25519
/// signing key, using the same X25519-from-Ed25519 derivation
/// (`ed25519_priv_to_x25519`, RFC 7748 §5 birational map) that
/// `mint_redemption` performs at lib.rs's `MembershipEventKind::PendingJoin`
/// branch when it builds the PendingJoin's `joiner_identity_pub`. See
/// the matching helper in `pkarr_invite_redemption_integration.rs`
/// (Task 3 single-engine test).
fn derive_composite_owner(sk: &SigningKey) -> (OwnerAddr, [u8; 64]) {
    let x25519_priv = harmony_app::dm_signing::ed25519_priv_to_x25519(sk);
    let x25519_pub = x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(*x25519_priv));
    let ed25519_pub = sk.verifying_key().to_bytes();
    let mut combined = [0u8; 64];
    combined[..32].copy_from_slice(x25519_pub.as_bytes());
    combined[32..].copy_from_slice(&ed25519_pub);
    let id = harmony_identity::Identity::from_public_bytes(&combined)
        .expect("composite identity_pub must round-trip via Identity::from_public_bytes");
    (OwnerAddr(id.address_hash), combined)
}

/// Build a hermetic iroh endpoint on loopback with no address-lookup,
/// no pkarr publisher, no DERP relays. Both ALPNs are registered so
/// the dialer can hit either; the accept side dispatches on negotiated
/// ALPN inside `IrohZenohLinkManager::spawn_accept_loop`.
async fn build_hermetic_endpoint() -> Arc<IrohEndpoint> {
    let secret = SecretKey::generate();
    let inner = Endpoint::builder(presets::Minimal)
        .secret_key(secret)
        .alpns(vec![
            alpn::HARMONY_ZENOH_V1.to_vec(),
            alpn::HARMONY_HANDSHAKE_V1.to_vec(),
        ])
        .relay_mode(RelayMode::Disabled)
        .clear_ip_transports()
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .expect("bind_addr loopback")
        .bind()
        .await
        .expect("bind iroh endpoint");
    Arc::new(IrohEndpoint::from_endpoint_for_integration_test(inner))
}

/// Spawn a shared in-memory CAS servicer so both engines see each
/// other's content blobs (not strictly needed for the option-A
/// handshake — which doesn't ride through CAS — but kept in case
/// future assertions exercise the full membership materialization
/// path, which still touches CAS for community-config blobs).
fn spawn_shared_cas() -> mpsc::Sender<CasOp> {
    let cas: Arc<TokioMutex<HashMap<harmony_content::cid::ContentId, Vec<u8>>>> =
        Arc::new(TokioMutex::new(HashMap::new()));
    let (cas_op_tx, mut cas_op_rx) = mpsc::channel::<CasOp>(64);
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
    cas_op_tx
}

// ────────────────────────────────────────────────────────────────────────────
// The test.
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bob_joins_alice_via_iroh_handshake_option_a() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("harmony_app=warn")),
        )
        .with_test_writer()
        .try_init();

    tokio::time::timeout(Duration::from_secs(60), async {
        // ── 1. Identities. ───────────────────────────────────────────────
        let alice_identity = PrivateIdentity::from_seed(&[0xa1; 32]);
        let bob_identity = PrivateIdentity::from_seed(&[0xb2; 32]);
        let alice_sk = Arc::new(signing_key_from(&alice_identity));
        let bob_sk = Arc::new(signing_key_from(&bob_identity));

        let (alice_addr, alice_pub) = derive_composite_owner(&alice_sk);
        let (bob_addr, bob_pub) = derive_composite_owner(&bob_sk);

        let resolver: Arc<dyn IdentityResolver> = Arc::new(TwoIdentityResolver {
            alice: (alice_addr, alice_pub),
            bob: (bob_addr, bob_pub),
        });

        // ── 2. Iroh endpoints + link managers (one per "process"). ──────
        let alice_ep = build_hermetic_endpoint().await;
        let bob_ep = build_hermetic_endpoint().await;
        let alice_bound = alice_ep.bound_sockets();
        assert!(
            !alice_bound.is_empty(),
            "alice's hermetic endpoint must expose bound_sockets() so bob's \
             dialer has a loopback target"
        );

        let alice_reachability = ReachabilityResolver::new();
        let bob_reachability = ReachabilityResolver::new();

        let (alice_link_tx, _alice_link_rx) = flume::unbounded::<LinkUnicast>();
        let alice_link_mgr = Arc::new(IrohZenohLinkManager::new(
            Arc::clone(&alice_ep),
            alice_reachability.clone(),
            alice_link_tx,
        ));
        // Acceptor is wired below (after Alice's registry + dm_outbox are
        // built). Spawn the accept loop FIRST so the loop is live before
        // the dispatcher install — early connections (before install)
        // log warn-only, but install lands before Bob dials.

        let (bob_link_tx, _bob_link_rx) = flume::unbounded::<LinkUnicast>();
        let bob_link_mgr = Arc::new(IrohZenohLinkManager::new(
            Arc::clone(&bob_ep),
            bob_reachability.clone(),
            bob_link_tx,
        ));
        let _alice_accept = alice_link_mgr.spawn_accept_loop();
        let _bob_accept = bob_link_mgr.spawn_accept_loop();

        // ── 3. Alice's community + engine. ──────────────────────────────
        let alice_minted = harmony_app::mint_community_creation(
            "OptionAHandshakeCommunity",
            true,
            alice_addr,
            alice_sk.as_ref(),
            Hlc {
                wall_ms: 100_000,
                logical: 0,
                device_id: "alice-dev".to_string(),
            },
        )
        .expect("alice mint community");
        let community_id = alice_minted.community_id;

        let cas_op_tx = spawn_shared_cas();
        let cs_alice: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
            cas_op_tx.clone(),
            Duration::from_secs(2),
        ));
        let cs_bob: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
            cas_op_tx.clone(),
            Duration::from_secs(2),
        ));

        let dir_alice = tempfile::tempdir().expect("alice tempdir");
        let dir_bob = tempfile::tempdir().expect("bob tempdir");

        let registry_alice = Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
            device_id: "alice-dev".into(),
            content_store: Arc::clone(&cs_alice),
            identity_resolver: Arc::clone(&resolver),
            identity_dir: dir_alice.path().to_path_buf(),
            debounce_ms: DEFAULT_DEBOUNCE_MS,
            error_tx: None,
            delta_tx: None,
            self_owner: alice_addr,
            signing_key: Arc::clone(&alice_sk),
            crdt_state: None,
            nav_emitter: None,
        }));
        let registry_bob = Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
            device_id: "bob-dev".into(),
            content_store: Arc::clone(&cs_bob),
            identity_resolver: Arc::clone(&resolver),
            identity_dir: dir_bob.path().to_path_buf(),
            debounce_ms: DEFAULT_DEBOUNCE_MS,
            error_tx: None,
            delta_tx: None,
            self_owner: bob_addr,
            signing_key: Arc::clone(&bob_sk),
            crdt_state: None,
            nav_emitter: None,
        }));

        // Spawn Alice's engine + insert her bootstrap Join.
        let (alice_pub_tx, _alice_pub_rx) = mpsc::channel::<Vec<u8>>(64);
        let (_alice_sub_tx, alice_sub_rx) = mpsc::channel::<Vec<u8>>(64);
        registry_alice
            .spawn_engine_inner_now(
                community_id,
                alice_minted.membership_key.clone(),
                alice_addr,
                true,
                alice_pub_tx,
                alice_sub_rx,
            )
            .await
            .expect("spawn alice engine");
        let alice_engine = registry_alice
            .engine_arc(&community_id)
            .await
            .expect("alice engine arc");
        alice_engine.bind_admin_identity_pub(alice_pub);
        alice_engine
            .insert_local_event(alice_minted.bootstrap_join.clone())
            .await
            .expect("alice bootstrap insert");
        std::mem::forget(dir_alice);

        // ── 4. Alice's dm_outbox + crdt_state (acceptor dependencies). ──
        let alice_dm_outbox = Arc::new(TokioMutex::new(DmOutbox::new(
            "alice-dev".into(),
            alice_addr,
            DeviceIdentityHash(alice_identity.identity.address_hash),
            Arc::clone(&alice_sk),
            Arc::new(dup_identity(&alice_identity)),
        )));
        let alice_crdt_state = Arc::new(TokioMutex::new(OwnerState::default()));

        // Install the production handshake acceptor onto Alice's link
        // manager. Using `None` for the app handle keeps the warn-only
        // emit_degraded path active; sufficient for the test.
        //
        // ZEB-325 PR #159 F10: pass an explicit short-deadline config
        // so a test-driven IO stall would surface as
        // `HandshakeAcceptError::IoTimeout` within seconds rather than
        // pinning the test until the 60s tokio::timeout fires above.
        let alice_acceptor: Arc<IrohInviteHandshakeAcceptor<()>> =
            Arc::new(IrohInviteHandshakeAcceptor::<()>::with_config(
                Arc::clone(&registry_alice),
                Arc::clone(&alice_dm_outbox),
                Arc::clone(&alice_crdt_state),
                None,
                harmony_app::iroh_invite_acceptor::HandshakeAcceptorConfig {
                    io_deadline: Duration::from_millis(10_000),
                    poll_deadline: Duration::from_millis(10_000),
                    poll_interval: Duration::from_millis(20),
                },
            ));
        if alice_link_mgr
            .install_handshake_dispatcher(alice_acceptor)
            .await
            .is_err()
        {
            panic!("first install must succeed (OnceCell empty)");
        }

        // ── 5. Bob's redemption setup. ──────────────────────────────────
        // Drain Bob's adapter dispatch — engine spawns produce a single
        // CommunityAdapterRequest; we drop the publisher_rx/subscriber_tx
        // halves because option A does NOT use CRDT sync round-trip.
        let (bob_adapter_tx, mut bob_adapter_rx) = mpsc::channel::<CommunityAdapterRequest>(8);
        tokio::spawn(async move {
            while let Some(req) = bob_adapter_rx.recv().await {
                drop(req.publisher_rx);
                drop(req.subscriber_tx);
            }
        });

        let (bob_unicast_tx, mut bob_unicast_rx) = mpsc::channel::<UnicastSendRequest>(8);
        tokio::spawn(async move { while bob_unicast_rx.recv().await.is_some() {} });

        let bob_dm_outbox = Arc::new(TokioMutex::new(DmOutbox::new(
            "bob-dev".into(),
            bob_addr,
            DeviceIdentityHash(bob_identity.identity.address_hash),
            Arc::clone(&bob_sk),
            Arc::new(dup_identity(&bob_identity)),
        )));

        let (bob_channel_log_adapter_tx, _bob_channel_log_adapter_rx) =
            mpsc::unbounded_channel::<ChannelLogAdapterRequest>();
        let bob_app = tauri::test::mock_app();
        let bob_channel_log_registry =
            Arc::new(ChannelLogRegistry::new(ChannelLogRegistryConfig {
                adapter_request_tx: bob_channel_log_adapter_tx,
                app: bob_app.handle().clone(),
                identity_dir: dir_bob.path().to_path_buf(),
                self_owner: bob_addr,
                self_device_id: "bob-dev".into(),
                signing_key: Arc::clone(&bob_sk),
                engine_config: ChannelLogEngineConfig::default(),
            }));

        let bob_crdt_state = Arc::new(TokioMutex::new(OwnerState::default()));
        let bob_hlc_tracker = Arc::new(TokioMutex::new(BTreeMap::<String, Hlc>::new()));

        // Seed Bob's ReachabilityResolver with Alice's routing record
        // directly. The IPC's iroh dial uses the routing record from the
        // pkarr-verified ReachabilityAnnouncePayload it decoded; bypassing
        // the pkarr step keeps this test focused on the wire handshake.
        let alice_routing = ReachabilityAnnouncePayload {
            iroh_node_id: *alice_ep.node_id().as_bytes(),
            home_relay_url: alice_ep
                .home_relay()
                .map(|r| r.to_string())
                .unwrap_or_default(),
            direct_addresses: alice_bound.clone(),
            announced_at_ms: 1_700_000_000_000,
            identity_signature: [0xCDu8; 64],
        };
        bob_reachability
            .seed_from_pkarr(
                alice_addr,
                DeviceIdentityHash([0u8; 16]),
                alice_routing.clone(),
            )
            .await;

        // ── 6. Build the invite URL (invite-only). ──────────────────────
        let token_minted_at = Hlc {
            wall_ms: 100_500,
            logical: 0,
            device_id: "alice-dev".into(),
        };
        let invite_token_unsigned = InviteToken {
            inviter: alice_addr,
            invitee_hint: Some(bob_addr),
            minted_at: token_minted_at.clone(),
            expires_at: None,
            sig: [0u8; 64],
        };
        let token_payload_bytes =
            canonical_invite_token_bytes(&invite_token_unsigned).expect("canonical token bytes");
        let token_sig: [u8; 64] = alice_sk.sign(&token_payload_bytes).to_bytes();
        let invite_token = InviteToken {
            inviter: alice_addr,
            invitee_hint: Some(bob_addr),
            minted_at: token_minted_at,
            expires_at: None,
            sig: token_sig,
        };

        let bob_x25519_pub = {
            let verifying_bytes = bob_sk.verifying_key().to_bytes();
            harmony_app::dm_signing::ed25519_pub_to_x25519(&verifying_bytes)
                .expect("bob ed25519→x25519")
        };
        let sealed_epoch_key = harmony_app::dm_signing::seal_to_owner(
            &bob_x25519_pub,
            alice_minted.membership_key.as_bytes(),
        )
        .expect("seal epoch key to bob");

        let invite_payload = CommunityInvitePayload {
            community_id,
            epoch_snapshot: InviteEpochSnapshot {
                epoch: 0,
                sealed_epoch_key,
                state_snapshot: MaterializedCommunityState::default(),
            },
            admin_addr: alice_addr,
            community_name: "OptionAHandshakeCommunity".into(),
            is_invite_only: true,
            expires_at: None,
            invite_token: Some(invite_token),
            admin_bootstrap: Some(alice_minted.bootstrap_join.clone()),
            admin_identity_pub: Some(alice_pub),
            forked_from: None,
            pre_fork_snapshot: None,
        };
        let invite_url =
            community_invite::encode_invite_url(&invite_payload).expect("encode invite");

        // The IPC's pkarr resolve step would normally find Alice's record;
        // for this test we wire `pkarr_resolver: None` and short-circuit
        // by calling a thin variant. Actually no — the IPC's case-A
        // resolve path is gated on pkarr_resolver: Some(...). We
        // construct a no-op pkarr resolver that returns Alice's record
        // for the case-A epoch keys derived from token_sig.
        //
        // Simpler: use a real `MockPkarrRelay` round-trip so the IPC
        // exercises pkarr + iroh end-to-end without any test-specific
        // short-circuit.
        let relay = harmony_pkarr::testing::MockPkarrRelay::start().await;
        let pool = harmony_pkarr::RelayPool::new(vec![relay.base_url.clone()]);
        let client = Arc::new(harmony_pkarr::RelayClient::new(pool));
        let pkarr_publisher = Arc::new(harmony_pkarr::PkarrPublisher::new(Arc::clone(&client)));
        let _publisher_handle = Arc::clone(&pkarr_publisher).spawn();

        let alice_routing_blob_for_builder = {
            let mut buf = Vec::new();
            ciborium::into_writer(&alice_routing, &mut buf).expect("encode alice routing_blob");
            buf
        };
        let alice_routing_blob_clone = alice_routing_blob_for_builder.clone();
        let invite_pub = harmony_app::pkarr_invite_publisher::PkarrInvitePublisher::new(
            Arc::clone(&pkarr_publisher),
            (*alice_sk).clone(),
            alice_pub,
            Arc::new(move || alice_routing_blob_clone.clone()),
        );
        invite_pub.register_invite(&invite_payload).await;
        let pkarr_resolver = Arc::new(harmony_pkarr::PkarrResolver::new(Arc::clone(&client)));

        // Wait for pkarr record visibility (the mock relay is async).
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time")
            .as_millis() as u64;
        let epoch_id = harmony_pkarr::current_epoch_id(now_ms);
        let probe_signing = harmony_pkarr::derive_ephemeral_key(
            harmony_pkarr::PkarrCase::Invite,
            &token_sig,
            &epoch_id.to_be_bytes(),
        );
        let probe_verifying = probe_signing.verifying_key();
        let mut record_visible = false;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if let Ok(Some(_)) = pkarr_resolver.resolve(&probe_verifying).await {
                record_visible = true;
                break;
            }
        }
        assert!(
            record_visible,
            "alice's pkarr record must appear in the mock relay within 5s before driving Bob's IPC"
        );

        // ── 7. Drive Bob's IPC. ─────────────────────────────────────────
        let outcome = harmony_app::connectivity_redeem_invite_iroh_inner(
            invite_url,
            Some(Arc::clone(&pkarr_resolver)),
            Some(bob_reachability.clone()),
            Some(Arc::clone(&bob_ep)),
            Arc::clone(&bob_crdt_state),
            Arc::clone(&bob_hlc_tracker),
            "bob-dev".to_string(),
            bob_addr,
            Arc::clone(&bob_sk),
            Arc::clone(&registry_bob),
            bob_adapter_tx,
            bob_unicast_tx,
            Arc::clone(&bob_dm_outbox),
            Arc::clone(&bob_channel_log_registry),
            None,
            |_| {},
            // ZEB-325 PR #159 F3 + F10: explicit dial timeouts (replaces
            // the prior env-var read). 10s is more than enough for
            // loopback connect / open_bi / response read; the test still
            // completes well under the outer 60s tokio::timeout guard.
            harmony_app::HandshakeDialConfig {
                connect_timeout: Duration::from_millis(10_000),
                open_bi_timeout: Duration::from_millis(10_000),
                response_read_timeout: Duration::from_millis(10_000),
            },
            // No fence — integration test doesn't drive NodeState
            // generation; mirrors the |_| Ok(()) sentinel the inner
            // unit-test rig uses.
            || Ok(()),
        )
        .await
        .expect(
            "connectivity_redeem_invite_iroh_inner must Ok (it converts internal errors \
             into outcome.status)",
        );

        // ── 8. Load-bearing assertions. ─────────────────────────────────
        assert_eq!(
            outcome.status, "joined",
            "Phase 2c option A must return 'joined' on the happy path \
             (iroh handshake completes + JoinCountersign delivered + \
             redeem_invite_inner_with_overrides commits). Got status={:?} \
             community_id={:?}.",
            outcome.status, outcome.community_id
        );
        assert_eq!(
            outcome.community_id.as_deref(),
            Some(hex::encode(community_id.0).as_str()),
            "community_id must echo Alice's invite"
        );

        // Bob's CRDT must contain ≥ 3 events: admin bootstrap (from the
        // invite payload's admin_bootstrap field), Bob's own PendingJoin,
        // and the pre-delivered JoinCountersign authored by Alice.
        let bob_state = registry_bob
            .state_for(&community_id)
            .await
            .expect("bob state must exist after redeem");
        let bob_events: Vec<_> = {
            let g = bob_state.lock().await;
            g.events.values().cloned().collect()
        };
        assert!(
            bob_events.len() >= 3,
            "Bob's CRDT must contain ≥ 3 events (admin bootstrap + PendingJoin + \
             JoinCountersign); got {}",
            bob_events.len()
        );
        let has_countersign = bob_events.iter().any(|e| {
            matches!(
                &e.kind,
                harmony_app::community_membership::MembershipEventKind::JoinCountersign { .. }
            )
        });
        assert!(
            has_countersign,
            "Bob's CRDT must contain the JoinCountersign delivered via the handshake response"
        );

        // Bob must materialize as Joined.
        let bob_materialized = materialize(&bob_events, alice_addr);
        assert_eq!(
            bob_materialized.members.get(&bob_addr).map(|m| m.status),
            Some(MemberStatus::Joined),
            "Bob must materialize as Joined after option A handshake completes"
        );

        // Alice's CRDT must also contain Bob's PendingJoin + her own
        // auto-counter-sign. Her engine inserts both as part of the
        // handle_unicast → auto_counter_sign flow.
        let alice_state = registry_alice
            .state_for(&community_id)
            .await
            .expect("alice state");
        let alice_events: Vec<_> = {
            let g = alice_state.lock().await;
            g.events.values().cloned().collect()
        };
        let alice_has_pending = alice_events.iter().any(|e| {
            matches!(
                &e.kind,
                harmony_app::community_membership::MembershipEventKind::PendingJoin { .. }
            ) && e.actor == bob_addr
        });
        assert!(
            alice_has_pending,
            "Alice's CRDT must contain Bob's PendingJoin (inserted by handle_unicast \
             during the accept side of the handshake)"
        );
        let alice_has_countersign = alice_events.iter().any(|e| {
            matches!(
                &e.kind,
                harmony_app::community_membership::MembershipEventKind::JoinCountersign { .. }
            ) && e.actor == alice_addr
        });
        assert!(
            alice_has_countersign,
            "Alice's CRDT must contain her own auto-counter-sign for Bob's PendingJoin"
        );

        // Reference unused — placate the unused-warnings drift.
        let _ = (alice_pub, bob_pub);

        // Graceful teardown.
        alice_ep.shutdown().await;
        bob_ep.shutdown().await;
    })
    .await
    .expect("bob_joins_alice_via_iroh_handshake_option_a timed out at 60s");
}
