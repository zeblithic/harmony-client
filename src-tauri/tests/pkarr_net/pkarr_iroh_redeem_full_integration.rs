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

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
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
use harmony_app::dm_outbox::DmOutbox;
use harmony_app::event_loop::{ChannelLogAdapterRequest, CommunityAdapterRequest};
use harmony_app::iroh_endpoint::{alpn, IrohEndpoint};
use harmony_app::iroh_invite_acceptor::IrohInviteHandshakeAcceptor;
use harmony_app::owner_state_crdt::OwnerState;
use harmony_app::owner_state_types::{DeviceIdentityHash, Hlc, OwnerAddr, OwnerDeviceEntry};
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
pub(crate) fn signing_key_from(identity: &PrivateIdentity) -> SigningKey {
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
pub(crate) fn derive_composite_owner(sk: &SigningKey) -> (OwnerAddr, [u8; 64]) {
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
pub(crate) async fn build_hermetic_endpoint() -> Arc<IrohEndpoint> {
    let secret = SecretKey::generate();
    let inner = Endpoint::builder(presets::Minimal)
        .secret_key(secret)
        .alpns(vec![
            alpn::HARMONY_ZENOH_V1.to_vec(),
            alpn::HARMONY_HANDSHAKE_V1.to_vec(),
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

/// Spawn a shared in-memory CAS servicer so both engines see each
/// other's content blobs (not strictly needed for the option-A
/// handshake — which doesn't ride through CAS — but kept in case
/// future assertions exercise the full membership materialization
/// path, which still touches CAS for community-config blobs).
pub(crate) fn spawn_shared_cas() -> mpsc::Sender<CasOp> {
    let cas: Arc<TokioMutex<HashMap<harmony_content::cid::ContentId, Vec<u8>>>> =
        Arc::new(TokioMutex::new(HashMap::new()));
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

// ────────────────────────────────────────────────────────────────────────────
// Shared two-party setup.
//
// ZEB-367: the targeted (`bob_joins_alice_via_iroh_handshake_option_a`) and
// untargeted (`invite_only_untargeted_generate_then_redeem_roundtrip`) tests
// share the entire two-party harness — identities, iroh endpoints + link
// managers, Alice's community engine + acceptor, Bob's redeem deps, and the
// mock pkarr relay — and differ ONLY in how the invite is sealed/minted and in
// a handful of post-redeem assertions. The block below was byte-identical
// between both before this extraction; pulling it into a single helper keeps
// the two tests in lock-step (a regression in the shared setup fails both).
//
// The helper builds Alice's acceptor with `Some(invite_pub)` (the case-A
// `PkarrInvitePublisher`, shared via `Arc`), so the ZEB-367 unregister-on-
// consume wiring (`handle_unicast` → `unregister_invite` on `Inserted`) is
// live for BOTH tests. Each test calls `invite_pub.register_invite(&payload)`
// with its own payload after constructing the invite.
// ────────────────────────────────────────────────────────────────────────────

/// Everything the two roundtrip tests need after the shared two-party setup.
/// Fields prefixed `_` are keep-alive handles (spawned tasks, accept loops,
/// tempdirs, the mock relay server) that must outlive the redeem call.
pub(crate) struct TwoPartySetup {
    // ── Identities / community ──────────────────────────────────────────
    pub(crate) alice_comm: harmony_app::community_membership::TestOwner,
    pub(crate) bob_comm: harmony_app::community_membership::TestOwner,
    pub(crate) alice_comm_sk: Arc<SigningKey>,
    pub(crate) bob_comm_sk: Arc<SigningKey>,
    pub(crate) alice_addr: OwnerAddr,
    pub(crate) bob_addr: OwnerAddr,
    pub(crate) alice_pub: [u8; 64],
    pub(crate) alice_minted: harmony_app::MintedCommunity,
    pub(crate) community_id: harmony_app::owner_state_types::SpaceId,

    // ── Endpoints (for teardown) ────────────────────────────────────────
    pub(crate) alice_ep: Arc<IrohEndpoint>,
    pub(crate) bob_ep: Arc<IrohEndpoint>,

    // ── Registries (for post-redeem assertions) ─────────────────────────
    pub(crate) registry_alice: Arc<CommunitySyncRegistry>,
    pub(crate) registry_bob: Arc<CommunitySyncRegistry>,

    // ── Bob redeem deps ─────────────────────────────────────────────────
    pub(crate) bob_reachability: ReachabilityResolver,
    pub(crate) bob_crdt_state: Arc<TokioMutex<OwnerState>>,
    pub(crate) bob_hlc_tracker: Arc<TokioMutex<harmony_crdt_sync::ReplayTracker<String, Hlc>>>,
    pub(crate) bob_dm_outbox: Arc<TokioMutex<DmOutbox>>,
    pub(crate) bob_channel_log_registry: Arc<ChannelLogRegistry>,
    // ZEB-790: Bob's single adoption floor — shared by registry_bob, Bob's
    // channel-log registry, every redeem call, and the durability SyncEngine.
    // (Alice's registry_alice holds its own floor — a separate node.)
    pub(crate) bob_adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor,
    pub(crate) bob_adapter_tx: mpsc::Sender<CommunityAdapterRequest>,
    // ZEB-473 (Move 1a): `bob_unicast_tx` removed with the Reticulum carrier.
    // `bob_unicast_count` stays at 0 structurally (no unicast producer exists);
    // the post-redeem assertion now documents that structural invariant.
    pub(crate) bob_unicast_count: Arc<AtomicUsize>,

    // ── pkarr ───────────────────────────────────────────────────────────
    pub(crate) invite_pub: Arc<harmony_app::pkarr_invite_publisher::PkarrInvitePublisher>,
    pub(crate) pkarr_resolver: Arc<harmony_pkarr::PkarrResolver>,
    pub(crate) pkarr_publisher: Arc<harmony_pkarr::PkarrPublisher>,

    // ── keep-alive ──────────────────────────────────────────────────────
    pub(crate) _alice_accept: tokio::task::JoinHandle<()>,
    pub(crate) _bob_accept: tokio::task::JoinHandle<()>,
    pub(crate) _relay: harmony_pkarr::testing::MockPkarrRelay,
    // Aborted during teardown (not just dropped) so the long-lived pkarr publisher
    // task can't bleed background work into later tests. Not underscore-prefixed:
    // it is actively used (.abort()), unlike the pure keep-alive fields above.
    pub(crate) publisher_handle: tokio::task::JoinHandle<()>,
    pub(crate) _dir_alice: tempfile::TempDir,
    pub(crate) _dir_bob: tempfile::TempDir,
}

/// The acceptor config the four happy-path roundtrips use (short wall-clock
/// budgets so a stall surfaces as a timeout in seconds, not at the outer 60s).
pub(crate) fn default_acceptor_config() -> harmony_app::iroh_invite_acceptor::HandshakeAcceptorConfig
{
    harmony_app::iroh_invite_acceptor::HandshakeAcceptorConfig {
        io_deadline: Duration::from_millis(10_000),
        poll_deadline: Duration::from_millis(10_000),
        poll_interval: Duration::from_millis(20),
    }
}

/// Thin wrapper: the happy-path roundtrips use the default acceptor config.
async fn setup_two_party_iroh_handshake() -> TwoPartySetup {
    setup_two_party_iroh_handshake_with_config(default_acceptor_config()).await
}

/// Stand up the full two-party iroh-handshake harness (identities, endpoints,
/// Alice's engine + acceptor, Bob's redeem deps, mock pkarr relay). The
/// `acceptor_config` lets the ZEB-874 negative test force a deterministic
/// post-insert failure (`poll_deadline = 0` → CountersignTimeout before the
/// countersign write); the happy-path tests pass `default_acceptor_config()`.
pub(crate) async fn setup_two_party_iroh_handshake_with_config(
    acceptor_config: harmony_app::iroh_invite_acceptor::HandshakeAcceptorConfig,
) -> TwoPartySetup {
    // ── 1. Identities. ───────────────────────────────────────────────
    // ZEB-339 dual-identity model: each party has TWO identities.
    //   - Community identity  = mint_test_owner(seed) → owner (OwnerAddr),
    //     device_key (SigningKey #2), cert (EnrollmentCert). Used as the
    //     community actor, to sign all community-membership events, and as
    //     the DmOutbox community_signing_key + enrollment_cert.
    //   - Transport identity  = PrivateIdentity::from_seed (Reticulum).
    //     Used for iroh endpoints, DeviceIdentityHash, and DmOutbox
    //     signing_key (Reticulum transport layer).

    // Transport identities (Reticulum).
    let alice_identity = PrivateIdentity::from_seed(&[0xa1; 32]);
    let bob_identity = PrivateIdentity::from_seed(&[0xb2; 32]);
    // Transport signing keys (Reticulum) — used for pkarr, DmOutbox.signing_key,
    // and joiner_identity_pub derivation in the iroh handshake packet.
    let alice_sk = Arc::new(signing_key_from(&alice_identity));
    let bob_sk = Arc::new(signing_key_from(&bob_identity));
    // Reticulum composite pubs: still needed for pkarr
    // (PkarrInvitePublisher + inviter_identity_pub in payload).
    let (alice_transport_addr, alice_pub) = derive_composite_owner(&alice_sk);
    let (bob_transport_addr, _bob_pub) = derive_composite_owner(&bob_sk);

    // Community identities (ZEB-339 owner model).
    let alice_comm = harmony_app::community_membership::mint_test_owner(0xA1);
    let bob_comm = harmony_app::community_membership::mint_test_owner(0xB2);
    let alice_comm_sk = Arc::new(ed25519_dalek::SigningKey::from_bytes(
        &alice_comm.device_key.to_bytes(),
    ));
    let bob_comm_sk = Arc::new(ed25519_dalek::SigningKey::from_bytes(
        &bob_comm.device_key.to_bytes(),
    ));
    // Community owner addresses (used as community actor).
    let alice_addr = alice_comm.owner;
    let bob_addr = bob_comm.owner;

    // Identity resolver: uses community owner addresses; the 64-byte pub
    // is not used by the cert-based ZEB-339 verify path but is kept for
    // structural compat (the Reticulum composite pubs serve double duty
    // here since the resolver is queried by OwnerAddr, which under
    // dual-identity is the community owner addr, not the Reticulum addr).
    let resolver: Arc<dyn IdentityResolver> = Arc::new(TwoIdentityResolver {
        alice: (alice_addr, [0u8; 64]),
        bob: (bob_addr, [0u8; 64]),
    });

    // Suppress unused-variable warnings for the transport addrs.
    let _ = (alice_transport_addr, bob_transport_addr);

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
    // Acceptor is wired below (after Alice's registry + dm_outbox + the
    // case-A invite publisher are built). Spawn the accept loop FIRST so
    // the loop is live before the dispatcher install — early connections
    // (before install) log warn-only, but install lands before Bob dials.

    let (bob_link_tx, _bob_link_rx) = flume::unbounded::<LinkUnicast>();
    let bob_link_mgr = Arc::new(IrohZenohLinkManager::new(
        Arc::clone(&bob_ep),
        bob_reachability.clone(),
        bob_link_tx,
    ));
    let alice_accept = alice_link_mgr.spawn_accept_loop();
    let bob_accept = bob_link_mgr.spawn_accept_loop();

    // ── 3. Alice's community + engine. ──────────────────────────────
    // ZEB-339: use alice_comm.owner as actor, alice_comm.device_key as signer,
    // and alice_comm.cert as the EnrollmentCert. The bootstrap_join will carry
    // alice_comm.cert so verify_admin_bootstrap passes (cert.owner_id == actor).
    let alice_minted = harmony_app::mint_community_creation(
        "OptionAHandshakeCommunity",
        true,
        alice_addr,
        alice_comm_sk.as_ref(),
        &alice_comm.cert,
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

    // ZEB-339: registries use the community owner addr and device signing key.
    // The auto-counter-sign task reads signing_key from the engine config
    // (sourced from registry.cfg), so alice's JoinCountersign will be
    // signed by alice_comm_sk and bear actor == alice_comm.owner.
    let registry_alice = Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
        device_id: "alice-dev".into(),
        content_store: Arc::clone(&cs_alice),
        identity_resolver: Arc::clone(&resolver),
        identity_dir: dir_alice.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: None,
        self_owner: alice_addr,
        signing_key: Arc::clone(&alice_comm_sk),
        crdt_state: None,
        nav_emitter: None,
        presence_resync_rx: None,
    }));
    // ZEB-790: Bob's single adoption floor (see TwoPartySetup.bob_adopt_floor).
    let bob_adopt_floor = harmony_app::hlc_adopt_floor::HlcAdoptFloor::new();
    let registry_bob = Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
        adopt_floor: bob_adopt_floor.clone(),
        device_id: "bob-dev".into(),
        content_store: Arc::clone(&cs_bob),
        identity_resolver: Arc::clone(&resolver),
        identity_dir: dir_bob.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: None,
        self_owner: bob_addr,
        signing_key: Arc::clone(&bob_comm_sk),
        crdt_state: None,
        nav_emitter: None,
        presence_resync_rx: None,
    }));

    // Spawn Alice's engine + insert her bootstrap Join.
    // admin_addr = alice_addr (community owner), matching alice_minted.bootstrap_join.actor.
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
            harmony_app::community_state_sync::CatchUpChannels::none(),
        )
        .await
        .expect("spawn alice engine");
    let alice_engine = registry_alice
        .engine_arc(&community_id)
        .await
        .expect("alice engine arc");
    alice_engine
        .insert_local_event(alice_minted.bootstrap_join.clone())
        .await
        .expect("alice bootstrap insert");

    // ── 4. Alice's dm_outbox + crdt_state (acceptor dependencies). ──
    // ZEB-339 dual-identity DmOutbox wiring:
    //   self_owner            = alice_comm.owner  (community actor)
    //   signing_key           = alice_sk          (Reticulum transport — unchanged)
    //   community_signing_key = alice_comm_sk     (enrolled device #2 key)
    //   enrollment_cert       = alice_comm.cert   (cert.owner_id == alice_addr ✓)
    // The three DmOutbox::new debug_asserts all pass:
    //   ① cert.verify() ✓  ② cert.owner_id == alice_addr ✓
    //   ③ cert.device_pubkeys.classical.ed25519_verify == alice_comm_sk.verifying_key() ✓
    let alice_dm_outbox = Arc::new(TokioMutex::new(DmOutbox::new(
        "alice-dev".into(),
        alice_addr,
        DeviceIdentityHash(alice_identity.identity.address_hash),
        Arc::clone(&alice_sk),
        Arc::new(dup_identity(&alice_identity)),
        Arc::clone(&alice_comm_sk),
        alice_comm.cert.clone(),
    )));
    let alice_crdt_state = Arc::new(TokioMutex::new(OwnerState::default()));

    // ── 4b. Mock pkarr relay + case-A invite publisher. ─────────────
    // Built BEFORE Alice's acceptor so the acceptor can be wired with
    // `Some(invite_pub)`, enabling the ZEB-367 unregister-on-consume path
    // (`handle_unicast` → `unregister_invite` once Bob's PendingJoin lands
    // as `Inserted`). The publisher object only depends on alice_sk /
    // alice_pub / the routing-blob builder — all available here. Each test
    // calls `register_invite(&its_own_payload)` after constructing the
    // invite; the publisher is shared via `Arc` because both the acceptor
    // and the test body hold it and `register/unregister_invite` take `&self`.
    let relay = harmony_pkarr::testing::MockPkarrRelay::start().await;
    let pool = harmony_pkarr::RelayPool::new(vec![relay.base_url.clone()]);
    let client = Arc::new(harmony_pkarr::RelayClient::new(pool));
    let pkarr_publisher = Arc::new(harmony_pkarr::PkarrPublisher::new(Arc::clone(&client)));
    let publisher_handle = Arc::clone(&pkarr_publisher).spawn();
    let pkarr_resolver = Arc::new(harmony_pkarr::PkarrResolver::new(Arc::clone(&client)));

    // Alice's routing record — both the pkarr case-A record (via the blob
    // builder below) and Bob's seeded ReachabilityResolver entry use it.
    let alice_routing = ReachabilityAnnouncePayload {
        iroh_node_id: *alice_ep.node_id().as_bytes(),
        home_relay_url: alice_ep
            .home_relay()
            .map(|r| r.to_string())
            .unwrap_or_default(),
        direct_addresses: alice_bound.clone(),
        announced_at_ms: 1_700_000_000_000,
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
    let invite_pub: Arc<harmony_app::pkarr_invite_publisher::PkarrInvitePublisher> = Arc::new(
        harmony_app::pkarr_invite_publisher::PkarrInvitePublisher::new(
            Arc::clone(&pkarr_publisher),
            (*alice_sk).clone(),
            alice_pub,
            Arc::new(move || alice_routing_blob_clone.clone()),
        ),
    );

    // Install the production handshake acceptor onto Alice's link
    // manager. Using `None` for the app handle keeps the warn-only
    // emit_degraded path active; sufficient for the test.
    //
    // ZEB-367: pass `Some(Arc::clone(&invite_pub))` so a successful invite
    // consumption unregisters the case-A pkarr publication.
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
            Some(Arc::clone(&invite_pub)),
            acceptor_config,
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

    // ZEB-325 PR #159 R6 (Cursor HIGH): the iroh-redeem path must not enqueue
    // any Reticulum unicast sends when a pre_delivered_countersign is in hand.
    // ZEB-473 (Move 1a): the unicast channel was removed entirely, so this is
    // now structurally guaranteed — `bob_unicast_count` stays at 0 with no
    // producer/drainer. Retained so the post-redeem assertion still documents
    // the invariant.
    let bob_unicast_count = Arc::new(AtomicUsize::new(0));

    // ZEB-339 dual-identity DmOutbox wiring for Bob:
    //   self_owner            = bob_comm.owner  (community actor)
    //   signing_key           = bob_sk          (Reticulum transport — unchanged)
    //   community_signing_key = bob_comm_sk     (enrolled device #2 key)
    //   enrollment_cert       = bob_comm.cert   (cert.owner_id == bob_addr ✓)
    // redeem_invite_inner_with_overrides reads dm_outbox.signing_key for
    // the Reticulum joiner_identity_pub derivation, and bob_comm_sk/cert
    // flow through via the top-level signing_key/enrollment_cert params.
    let bob_dm_outbox = Arc::new(TokioMutex::new(DmOutbox::new(
        "bob-dev".into(),
        bob_addr,
        DeviceIdentityHash(bob_identity.identity.address_hash),
        Arc::clone(&bob_sk),
        Arc::new(dup_identity(&bob_identity)),
        Arc::clone(&bob_comm_sk),
        bob_comm.cert.clone(),
    )));

    let (bob_channel_log_adapter_tx, _bob_channel_log_adapter_rx) =
        mpsc::unbounded_channel::<ChannelLogAdapterRequest>();
    // ZEB-339: channel log registry uses community owner addr and device key.
    // `ChannelLogRegistry::new` already returns `Arc<Self>`; do not re-wrap.
    // ZEB-445: registry takes a mode-agnostic NodeEventSink; this test never
    // asserts on channel-log emissions, so an empty fan-out is sufficient.
    let bob_channel_log_registry = ChannelLogRegistry::new(ChannelLogRegistryConfig {
        adapter_request_tx: bob_channel_log_adapter_tx,
        sink: Arc::new(harmony_app::node_event_sink::FanoutSink(vec![])),
        identity_dir: dir_bob.path().to_path_buf(),
        self_owner: bob_addr,
        self_device_id: "bob-dev".into(),
        signing_key: Arc::clone(&bob_comm_sk),
        adopt_floor: bob_adopt_floor.clone(),
        engine_config: ChannelLogEngineConfig::default(),
        transport_epoch_rx: None,
        // ZEB-599 Direction 1: no presence watch in this integration harness.
        presence_resync_rx: None,
    });

    let bob_crdt_state = Arc::new(TokioMutex::new(OwnerState::default()));
    let bob_hlc_tracker = Arc::new(TokioMutex::new(harmony_crdt_sync::ReplayTracker::new(
        "bob-dev".to_string(),
    )));

    // ZEB-325 PR #159 R6 (Cursor HIGH): pre-populate bob's owner_device_cache
    // with a fabricated alice device so `resolve_destinations_for_owner`
    // returns non-empty for alice_addr (community owner addr). This simulates
    // "stale Reticulum DM cache from prior interactions" — the exact condition
    // that, pre-fix, would have driven the iroh-redeem path into the Reticulum
    // fan-out branch and Err'd if every try_send failed.
    // ZEB-339: key is alice_addr (community owner), matching payload.admin_addr.
    {
        let mut g = bob_crdt_state.lock().await;
        g.owner_device_cache.devices.insert(
            alice_addr,
            OwnerDeviceEntry {
                devices: vec![DeviceIdentityHash([0xAAu8; 16])],
                device_identity_pubs: vec![None],
                device_tunnel_contacts: vec![None],
                learned_at: Hlc {
                    wall_ms: 100_000,
                    logical: 0,
                    device_id: "alice-dev".into(),
                },
            },
        );
    }

    // Seed Bob's ReachabilityResolver with Alice's routing record
    // directly. The IPC's iroh dial uses the routing record from the
    // pkarr-verified ReachabilityAnnouncePayload it decoded; bypassing
    // the pkarr step keeps this test focused on the wire handshake.
    // ZEB-339: seed key is alice_addr (community owner), matching
    // payload.admin_addr which the IPC uses as inviter_addr.
    bob_reachability
        .seed_from_pkarr(
            alice_addr,
            DeviceIdentityHash([0u8; 16]),
            None,
            alice_routing.clone(),
        )
        .await;

    TwoPartySetup {
        alice_comm,
        bob_comm,
        alice_comm_sk,
        bob_comm_sk,
        alice_addr,
        bob_addr,
        alice_pub,
        alice_minted,
        community_id,
        alice_ep,
        bob_ep,
        registry_alice,
        registry_bob,
        bob_reachability,
        bob_crdt_state,
        bob_hlc_tracker,
        bob_dm_outbox,
        bob_channel_log_registry,
        bob_adopt_floor,
        bob_adapter_tx,
        bob_unicast_count,
        invite_pub,
        pkarr_resolver,
        pkarr_publisher,
        _alice_accept: alice_accept,
        _bob_accept: bob_accept,
        _relay: relay,
        publisher_handle,
        _dir_alice: dir_alice,
        _dir_bob: dir_bob,
    }
}

/// Wait (≤5s) for Alice's case-A pkarr record — keyed on the invite token's
/// signature for the current epoch — to become visible in the mock relay.
/// Returns the probe verifying key so callers can re-resolve it later (e.g.
/// to assert the record disappears after the invite is consumed).
async fn await_pkarr_record_visible(
    pkarr_resolver: &harmony_pkarr::PkarrResolver,
    token_sig: &[u8; 64],
) -> ed25519_dalek::VerifyingKey {
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
    probe_verifying
}

// ────────────────────────────────────────────────────────────────────────────
// Targeted roundtrip (ZEB-325 Phase 2c option A).
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

    // ZEB-374 audit: setup_two_party_iroh_handshake binds two real iroh
    // endpoints INSIDE the 60s budget. Pre-pay iroh's ~30s first-bind global
    // init OUTSIDE the budget (the ZEB-347 pattern) so full-suite contention
    // can't charge it against the handshake and flake the timeout.
    harmony_app::iroh_endpoint::warm_up_iroh_global_init().await;

    tokio::time::timeout(Duration::from_secs(60), async {
        let s = setup_two_party_iroh_handshake().await;

        // ── 6. Build the invite URL (TARGETED invite-only). ─────────────
        // ZEB-339: InviteToken.inviter = alice_comm.owner (community actor).
        // The token is signed with alice_comm_sk (enrolled device #2 key).
        // P5 verify_invite_token_sig_with_enrolled resolves alice's enrolled
        // device key from materialized membership (populated after her
        // bootstrap Join is inserted) and verifies against alice_comm_sk.
        let token_minted_at = Hlc {
            wall_ms: 100_500,
            logical: 0,
            device_id: "alice-dev".into(),
        };
        let invite_token_unsigned = InviteToken {
            inviter: s.alice_addr,
            invitee_hint: Some(s.bob_addr),
            minted_at: token_minted_at.clone(),
            expires_at: None,
            sig: [0u8; 64],
        };
        let token_payload_bytes =
            canonical_invite_token_bytes(&invite_token_unsigned).expect("canonical token bytes");
        let token_sig: [u8; 64] = s.alice_comm_sk.sign(&token_payload_bytes).to_bytes();
        let invite_token = InviteToken {
            inviter: s.alice_addr,
            invitee_hint: Some(s.bob_addr),
            minted_at: token_minted_at,
            expires_at: None,
            sig: token_sig,
        };

        // ZEB-339: seal epoch key to Bob's community device key's x25519 pub.
        // mint_redemption decrypts the sealed_epoch_key using ed25519_priv_to_x25519
        // applied to the `signing_key` param (= bob_comm_sk here). So the seal
        // target must derive from bob_comm_sk, NOT from bob_sk (Reticulum).
        let bob_x25519_pub = {
            let verifying_bytes = s.bob_comm_sk.verifying_key().to_bytes();
            harmony_app::dm_signing::ed25519_pub_to_x25519(&verifying_bytes)
                .expect("bob_comm ed25519→x25519")
        };
        let sealed_epoch_key = harmony_app::dm_signing::seal_to_owner(
            &bob_x25519_pub,
            s.alice_minted.membership_key.as_bytes(),
        )
        .expect("seal epoch key to bob");

        // ZEB-339: invite payload uses community actor addresses throughout.
        // inviter_identity_pub still carries the Reticulum composite pub (alice_pub)
        // because pkarr step 6 (verify_identity_match) checks it against the
        // pkarr record's harmony_identity_pub, which is signed by alice_sk.
        let invite_payload = CommunityInvitePayload {
            inviter_signer_certs: Vec::new(),
            community_id: s.community_id,
            epoch_snapshot: InviteEpochSnapshot {
                epoch: 0,
                // ZEB-369: targeted invite — the per-device sealed envelope rides
                // in sealed_epoch_keys with sealed_epoch_key empty.
                sealed_epoch_key: Vec::new(),
                sealed_epoch_keys: vec![sealed_epoch_key],
                state_snapshot: MaterializedCommunityState::default(),
            },
            admin_addr: s.alice_addr,
            community_name: "OptionAHandshakeCommunity".into(),
            is_invite_only: true,
            expires_at: None,
            invite_token: Some(invite_token),
            admin_bootstrap: Some(s.alice_minted.bootstrap_join.clone()),
            // inviter_identity_pub = Reticulum composite pub; used only for the
            // pkarr verify_identity_match check (step 6) — not for membership.
            inviter_identity_pub: Some(s.alice_pub),
            forked_from: None,
            pre_fork_snapshot: None,
            // ZEB-339: invite-only payloads REQUIRE an inviter EnrollmentCert.
            // alice_comm.cert (cert.owner_id == alice_addr) lets enrolled_key_from_cert
            // successfully bind the cert to the actor.
            inviter_enrollment: Some(s.alice_comm.cert.clone()),
            // Targeted invite: epoch key is sealed to Bob's enrolled key, so no
            // URL-borne decrypt key is needed (or allowed by the encode guard).
            untargeted_decrypt_key: None,
        };
        let invite_url =
            community_invite::encode_invite_url(&invite_payload).expect("encode invite");

        // Publish Alice's case-A pkarr record for this invite and wait for
        // the mock relay to make it visible. The IPC resolves it end-to-end
        // (pkarr → iroh) with no test-specific short-circuit.
        s.invite_pub.register_invite(&invite_payload).await;
        let _probe_verifying = await_pkarr_record_visible(&s.pkarr_resolver, &token_sig).await;

        // ── 7. Drive Bob's IPC. ─────────────────────────────────────────
        // ZEB-325 PR #159 R4-3 (CodeRabbit NITPICK): capture nav-updated
        // emits so we can assert the iroh-redeem path emits exactly one
        // {"added", "community", ...} payload — the R3 fix would have
        // regressed silently with the prior `|_| {}` sink.
        let nav_emits: Arc<Mutex<Vec<harmony_app::NavUpdatedPayload>>> =
            Arc::new(Mutex::new(Vec::new()));
        let nav_emits_sink = Arc::clone(&nav_emits);
        // ZEB-339: self_owner = bob_comm.owner, signing_key = bob_comm_sk,
        // enrollment_cert = bob_comm.cert. mint_redemption uses signing_key
        // to sign Bob's PendingJoin event (actor = self_owner = bob_comm.owner)
        // and enrollment_cert to attach the cert. The dm_outbox.signing_key
        // (= bob_sk, Reticulum) is used for joiner_identity_pub derivation.
        let outcome = harmony_app::connectivity_redeem_invite_iroh_inner(
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
            None, // ZEB-434: no transport-epoch watch in this test
            Arc::clone(&s.bob_dm_outbox),
            Arc::clone(&s.bob_channel_log_registry),
            // ZEB-427: legacy tests predate the durability fence; pass no
            // engine (the fence logs and skips — behavior under test in
            // zeb427_iroh_redeem_fences_owner_state_space_to_disk).
            None,
            None,
            |_| {},
            // ZEB-325 PR #159 R4-3 (CodeRabbit NITPICK): record each
            // nav-updated emit into the shared Vec so we can assert on
            // the count + shape after the redeem completes. Mutex (not
            // tokio::sync::Mutex) is fine because the sink is a sync
            // `Fn` closure and the test only reads after the await.
            move |payload: harmony_app::NavUpdatedPayload| {
                nav_emits_sink
                    .lock()
                    .expect("nav_emits mutex")
                    .push(payload);
            },
            // ZEB-325 PR #159 F3 + F10: explicit dial timeouts (replaces
            // the prior env-var read). 10s is more than enough for
            // loopback connect / open_bi / response read; the test still
            // completes well under the outer 60s tokio::timeout guard.
            harmony_app::HandshakeDialConfig {
                connect_timeout: Duration::from_millis(10_000),
                open_bi_timeout: Duration::from_millis(10_000),
                response_read_timeout: Duration::from_millis(10_000),
                // ZEB-325 PR #159 R3-2: 10s budget for the request-
                // side writes mirrors the other timeouts.
                write_timeout: Duration::from_millis(10_000),
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
            Some(hex::encode(s.community_id.0).as_str()),
            "community_id must echo Alice's invite"
        );

        // ZEB-325 PR #159 R4-3 (CodeRabbit NITPICK): assert exactly one
        // nav-updated emit with the expected shape — guards the R3 fix
        // that wired nav_emit_sink into the iroh-redeem joined path.
        let emits = nav_emits.lock().expect("nav_emits mutex").clone();
        assert_eq!(
            emits.len(),
            1,
            "iroh-redeem joined path must emit exactly one nav-updated; got {} \
             emits: {:?}",
            emits.len(),
            emits
        );
        let emit = &emits[0];
        assert_eq!(emit.action, "added", "nav-updated action must be 'added'");
        assert_eq!(
            emit.kind, "community",
            "nav-updated kind must be 'community'"
        );
        assert_eq!(
            emit.space_id,
            hex::encode(s.community_id.0),
            "nav-updated space_id must match the joined community"
        );

        // ZEB-325 PR #159 R6 (Cursor HIGH) regression: when bob's owner_device_cache
        // contains a (fabricated) alice device AND the iroh path delivered
        // the JoinCountersign via the bi-stream, the Reticulum fan-out must
        // be skipped entirely — the unicast channel must have received 0
        // packets. Pre-fix, the fan-out would have fired and (because the
        // fabricated DeviceIdentityHash doesn't correspond to any real
        // Reticulum destination) eventually Err'd, rolling back the join.
        assert_eq!(
            s.bob_unicast_count.load(Ordering::Relaxed),
            0,
            "iroh-redeem path must NOT enqueue any Reticulum unicast sends \
             when pre_delivered_countersign is present (R6 regression): \
             counter={}",
            s.bob_unicast_count.load(Ordering::Relaxed)
        );

        // Bob's CRDT must contain ≥ 3 events: admin bootstrap (from the
        // invite payload's admin_bootstrap field), Bob's own PendingJoin,
        // and the pre-delivered JoinCountersign authored by Alice.
        let bob_state = s
            .registry_bob
            .state_for(&s.community_id)
            .await
            .expect("bob state must exist after redeem");
        let bob_events: Vec<_> = {
            let g = bob_state.lock().await;
            g.events().cloned().collect()
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
        let bob_materialized = materialize(&bob_events, s.alice_addr);
        assert_eq!(
            bob_materialized.members.get(&s.bob_addr).map(|m| m.status),
            Some(MemberStatus::Joined),
            "Bob must materialize as Joined after option A handshake completes"
        );

        // Alice's CRDT must also contain Bob's PendingJoin + her own
        // auto-counter-sign. Her engine inserts both as part of the
        // handle_unicast → auto_counter_sign flow.
        let alice_state = s
            .registry_alice
            .state_for(&s.community_id)
            .await
            .expect("alice state");
        let alice_events: Vec<_> = {
            let g = alice_state.lock().await;
            g.events().cloned().collect()
        };
        let alice_has_pending = alice_events.iter().any(|e| {
            matches!(
                &e.kind,
                harmony_app::community_membership::MembershipEventKind::PendingJoin { .. }
            ) && e.actor == s.bob_addr
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
            ) && e.actor == s.alice_addr
        });
        assert!(
            alice_has_countersign,
            "Alice's CRDT must contain her own auto-counter-sign for Bob's PendingJoin"
        );

        // ZEB-874 regression: the single-use invite must be burned once the
        // handshake completes — now the burn fires in the acceptor AFTER the
        // countersign response is written, not in handle_unicast on insert. The
        // deterministic signal is the publisher dropping the case-A handle from
        // its active set (re-resolving the mock relay is unreliable; the PUT
        // record lingers until TTL — see the untargeted roundtrip's note).
        let invite_handle = format!("invite:{}", hex::encode(token_sig));
        let mut handle_gone = false;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if !s
                .pkarr_publisher
                .active_handles()
                .await
                .contains(&invite_handle)
            {
                handle_gone = true;
                break;
            }
        }
        assert!(
            handle_gone,
            "ZEB-874: the case-A invite publication must be unregistered (handle \
             {invite_handle:?} dropped from active_handles) within 5s of the \
             successful handshake (acceptor burns after the countersign write)"
        );

        // Graceful teardown. Abort the long-lived pkarr publisher task FIRST so it
        // stops using the endpoints before we shut them down, and so no background
        // work leaks into later tests.
        s.publisher_handle.abort();
        s.alice_ep.shutdown().await;
        s.bob_ep.shutdown().await;
    })
    .await
    .expect("bob_joins_alice_via_iroh_handshake_option_a timed out at 60s");
}

// ────────────────────────────────────────────────────────────────────────────
// ZEB-369 targeted invite-only roundtrips (generate-shape → real-transport
// redeem). These mirror `bob_joins_alice_via_iroh_handshake_option_a` exactly
// for the transport/handshake plumbing; the ONLY thing under test is the
// ZEB-369 redeem-side try-all over the new multi-envelope wire shape
// (`sealed_epoch_keys`). The generate-side resolver is unit-tested separately
// (`resolve_invitee_device_keys_*` in lib.rs); here we hand-build the payload
// in the shape `generate_invite_impl` would produce for a targeted invite and
// prove it round-trips through the real iroh redeem path to `status=="joined"`.
// ────────────────────────────────────────────────────────────────────────────

/// Single-envelope targeted invite: Alice seals the epoch key to exactly Bob's
/// enrolled device-#2 key and ships it in `sealed_epoch_keys` (one entry,
/// `sealed_epoch_key` empty, `invitee_hint = Some(bob)`, no URL key). Bob
/// redeems with his real device key over the real iroh handshake and lands
/// `joined`. This is the ZEB-369 targeted analogue of the untargeted roundtrip.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn targeted_invite_only_generate_then_redeem_roundtrip() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("harmony_app=warn")),
        )
        .with_test_writer()
        .try_init();

    // ZEB-374 audit: pre-pay iroh's ~30s first-bind global init OUTSIDE the
    // 60s budget so full-suite contention can't flake the handshake timeout.
    harmony_app::iroh_endpoint::warm_up_iroh_global_init().await;

    tokio::time::timeout(Duration::from_secs(60), async {
        let s = setup_two_party_iroh_handshake().await;

        // Targeted token bound to Bob, signed with Alice's enrolled device key.
        let token_minted_at = Hlc {
            wall_ms: 100_500,
            logical: 0,
            device_id: "alice-dev".into(),
        };
        let invite_token_unsigned = InviteToken {
            inviter: s.alice_addr,
            invitee_hint: Some(s.bob_addr),
            minted_at: token_minted_at.clone(),
            expires_at: None,
            sig: [0u8; 64],
        };
        let token_payload_bytes =
            canonical_invite_token_bytes(&invite_token_unsigned).expect("canonical token bytes");
        let token_sig: [u8; 64] = s.alice_comm_sk.sign(&token_payload_bytes).to_bytes();
        let invite_token = InviteToken {
            inviter: s.alice_addr,
            invitee_hint: Some(s.bob_addr),
            minted_at: token_minted_at,
            expires_at: None,
            sig: token_sig,
        };

        // Seal the epoch key to Bob's device-#2 X25519 (the key mint_redemption
        // derives from the `signing_key` param = bob_comm_sk).
        let bob_x25519_pub = {
            let verifying_bytes = s.bob_comm_sk.verifying_key().to_bytes();
            harmony_app::dm_signing::ed25519_pub_to_x25519(&verifying_bytes)
                .expect("bob_comm ed25519→x25519")
        };
        let env = harmony_app::dm_signing::seal_to_owner(
            &bob_x25519_pub,
            s.alice_minted.membership_key.as_bytes(),
        )
        .expect("seal epoch key to bob");

        // ZEB-369 targeted shape: single envelope in sealed_epoch_keys, empty blob.
        let invite_payload = CommunityInvitePayload {
            inviter_signer_certs: Vec::new(),
            community_id: s.community_id,
            epoch_snapshot: InviteEpochSnapshot {
                epoch: 0,
                sealed_epoch_key: Vec::new(),
                sealed_epoch_keys: vec![env],
                state_snapshot: MaterializedCommunityState::default(),
            },
            admin_addr: s.alice_addr,
            community_name: "Zeb369TargetedCommunity".into(),
            is_invite_only: true,
            expires_at: None,
            invite_token: Some(invite_token),
            admin_bootstrap: Some(s.alice_minted.bootstrap_join.clone()),
            inviter_identity_pub: Some(s.alice_pub),
            forked_from: None,
            pre_fork_snapshot: None,
            inviter_enrollment: Some(s.alice_comm.cert.clone()),
            untargeted_decrypt_key: None,
        };
        // The shape-aware encode/decode gate must accept the targeted form.
        let invite_url =
            community_invite::encode_invite_url(&invite_payload).expect("encode targeted invite");

        s.invite_pub.register_invite(&invite_payload).await;
        let _probe_verifying = await_pkarr_record_visible(&s.pkarr_resolver, &token_sig).await;

        let outcome = harmony_app::connectivity_redeem_invite_iroh_inner(
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
            harmony_app::HandshakeDialConfig {
                connect_timeout: Duration::from_millis(10_000),
                open_bi_timeout: Duration::from_millis(10_000),
                response_read_timeout: Duration::from_millis(10_000),
                write_timeout: Duration::from_millis(10_000),
            },
            || Ok(()),
        )
        .await
        .expect("connectivity_redeem_invite_iroh_inner must Ok");

        assert_eq!(
            outcome.status, "joined",
            "targeted single-envelope invite-only redeem must return 'joined' — Bob \
             opened the only envelope (sealed to his device key) via the ZEB-369 \
             try-all path. Got status={:?} community_id={:?}.",
            outcome.status, outcome.community_id
        );
        assert_eq!(
            outcome.community_id.as_deref(),
            Some(hex::encode(s.community_id.0).as_str()),
            "community_id must echo Alice's invite"
        );

        // Bob materializes as Joined off a CRDT carrying the JoinCountersign.
        let bob_state = s
            .registry_bob
            .state_for(&s.community_id)
            .await
            .expect("bob state must exist after redeem");
        let bob_events: Vec<_> = {
            let g = bob_state.lock().await;
            g.events().cloned().collect()
        };
        let bob_materialized = materialize(&bob_events, s.alice_addr);
        assert_eq!(
            bob_materialized.members.get(&s.bob_addr).map(|m| m.status),
            Some(MemberStatus::Joined),
            "Bob must materialize as Joined after the targeted handshake completes"
        );

        s.publisher_handle.abort();
        s.alice_ep.shutdown().await;
        s.bob_ep.shutdown().await;
    })
    .await
    .expect("targeted_invite_only_generate_then_redeem_roundtrip timed out at 60s");
}

/// Multi-device targeted invite: Alice seals the epoch key to TWO device keys —
/// a throwaway device (envelope #1, which Bob CANNOT open) and Bob's real
/// device (envelope #2). Bob's device key is deliberately NOT first, so a
/// "joined" outcome proves the ZEB-369 redeem-side try-all skipped the first
/// undecryptable envelope and opened the second. This is the high-value proof
/// that "seal to all my devices, redeem on any one" works over real transport.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn targeted_invite_only_multi_device_redeem_opens_correct_envelope() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("harmony_app=warn")),
        )
        .with_test_writer()
        .try_init();

    harmony_app::iroh_endpoint::warm_up_iroh_global_init().await;

    tokio::time::timeout(Duration::from_secs(60), async {
        let s = setup_two_party_iroh_handshake().await;

        let token_minted_at = Hlc {
            wall_ms: 100_500,
            logical: 0,
            device_id: "alice-dev".into(),
        };
        let invite_token_unsigned = InviteToken {
            inviter: s.alice_addr,
            invitee_hint: Some(s.bob_addr),
            minted_at: token_minted_at.clone(),
            expires_at: None,
            sig: [0u8; 64],
        };
        let token_payload_bytes =
            canonical_invite_token_bytes(&invite_token_unsigned).expect("canonical token bytes");
        let token_sig: [u8; 64] = s.alice_comm_sk.sign(&token_payload_bytes).to_bytes();
        let invite_token = InviteToken {
            inviter: s.alice_addr,
            invitee_hint: Some(s.bob_addr),
            minted_at: token_minted_at,
            expires_at: None,
            sig: token_sig,
        };

        // Envelope #1: sealed to a THROWAWAY device key Bob does NOT hold.
        let other_device = harmony_app::community_membership::mint_test_owner(0xC3);
        let other_x25519_pub = {
            let verifying_bytes = other_device.device_key.verifying_key().to_bytes();
            harmony_app::dm_signing::ed25519_pub_to_x25519(&verifying_bytes)
                .expect("other ed25519→x25519")
        };
        let env_other = harmony_app::dm_signing::seal_to_owner(
            &other_x25519_pub,
            s.alice_minted.membership_key.as_bytes(),
        )
        .expect("seal to other device");

        // Envelope #2: sealed to Bob's real device-#2 key.
        let bob_x25519_pub = {
            let verifying_bytes = s.bob_comm_sk.verifying_key().to_bytes();
            harmony_app::dm_signing::ed25519_pub_to_x25519(&verifying_bytes)
                .expect("bob_comm ed25519→x25519")
        };
        let env_bob = harmony_app::dm_signing::seal_to_owner(
            &bob_x25519_pub,
            s.alice_minted.membership_key.as_bytes(),
        )
        .expect("seal to bob device");

        // Bob's envelope is SECOND — try-all must skip #1 and open #2.
        let invite_payload = CommunityInvitePayload {
            inviter_signer_certs: Vec::new(),
            community_id: s.community_id,
            epoch_snapshot: InviteEpochSnapshot {
                epoch: 0,
                sealed_epoch_key: Vec::new(),
                sealed_epoch_keys: vec![env_other, env_bob],
                state_snapshot: MaterializedCommunityState::default(),
            },
            admin_addr: s.alice_addr,
            community_name: "Zeb369MultiDeviceCommunity".into(),
            is_invite_only: true,
            expires_at: None,
            invite_token: Some(invite_token),
            admin_bootstrap: Some(s.alice_minted.bootstrap_join.clone()),
            inviter_identity_pub: Some(s.alice_pub),
            forked_from: None,
            pre_fork_snapshot: None,
            inviter_enrollment: Some(s.alice_comm.cert.clone()),
            untargeted_decrypt_key: None,
        };
        let invite_url = community_invite::encode_invite_url(&invite_payload)
            .expect("encode multi-device invite");

        s.invite_pub.register_invite(&invite_payload).await;
        let _probe_verifying = await_pkarr_record_visible(&s.pkarr_resolver, &token_sig).await;

        let outcome = harmony_app::connectivity_redeem_invite_iroh_inner(
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
            harmony_app::HandshakeDialConfig {
                connect_timeout: Duration::from_millis(10_000),
                open_bi_timeout: Duration::from_millis(10_000),
                response_read_timeout: Duration::from_millis(10_000),
                write_timeout: Duration::from_millis(10_000),
            },
            || Ok(()),
        )
        .await
        .expect("connectivity_redeem_invite_iroh_inner must Ok");

        assert_eq!(
            outcome.status, "joined",
            "multi-device targeted redeem must return 'joined' — Bob's envelope was \
             SECOND, so the ZEB-369 try-all had to skip the first (undecryptable) \
             envelope and open the second with his device key. Got status={:?} \
             community_id={:?}.",
            outcome.status, outcome.community_id
        );
        assert_eq!(
            outcome.community_id.as_deref(),
            Some(hex::encode(s.community_id.0).as_str()),
            "community_id must echo Alice's invite"
        );

        let bob_state = s
            .registry_bob
            .state_for(&s.community_id)
            .await
            .expect("bob state must exist after redeem");
        let bob_events: Vec<_> = {
            let g = bob_state.lock().await;
            g.events().cloned().collect()
        };
        let bob_materialized = materialize(&bob_events, s.alice_addr);
        assert_eq!(
            bob_materialized.members.get(&s.bob_addr).map(|m| m.status),
            Some(MemberStatus::Joined),
            "Bob must materialize as Joined after opening the 2nd envelope"
        );

        s.publisher_handle.abort();
        s.alice_ep.shutdown().await;
        s.bob_ep.shutdown().await;
    })
    .await
    .expect("targeted_invite_only_multi_device_redeem_opens_correct_envelope timed out at 60s");
}

// ────────────────────────────────────────────────────────────────────────────
// Untargeted roundtrip (ZEB-367 Phase 4: invite-only generate → redeem).
//
// THE proof that the untargeted invite-only crypto loop closes: Alice mints an
// UNTARGETED invite (no invitee_hint) whose epoch key is sealed to a FRESH
// EPHEMERAL X25519 key that has nothing to do with Bob. The ephemeral PRIVATE
// half rides in the URL (`untargeted_decrypt_key`). Bob redeems with his OWN
// `bob_comm_sk` as `signing_key` — a key the epoch key was never sealed to — so
// the ONLY way he can recover the epoch key is via the URL's ephemeral private
// (the `Some(ephemeral_priv)` branch in `mint_redemption`, lib.rs ~16535). A
// `joined` outcome is therefore airtight evidence that the untargeted decrypt
// path executed.
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invite_only_untargeted_generate_then_redeem_roundtrip() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("harmony_app=warn")),
        )
        .with_test_writer()
        .try_init();

    // ZEB-374 audit: setup_two_party_iroh_handshake binds two real iroh
    // endpoints INSIDE the 60s budget. Pre-pay iroh's ~30s first-bind global
    // init OUTSIDE the budget (the ZEB-347 pattern) so full-suite contention
    // can't charge it against the handshake and flake the timeout.
    harmony_app::iroh_endpoint::warm_up_iroh_global_init().await;

    tokio::time::timeout(Duration::from_secs(60), async {
        let s = setup_two_party_iroh_handshake().await;

        // ── 6. Build the invite URL (UNTARGETED invite-only). ───────────
        // Differs from the targeted test in exactly three places:
        //   (1) the epoch key is sealed via `SealRecipient::Untargeted` (fresh
        //       ephemeral key; the private half rides in the URL),
        //   (2) the token is minted with `invitee_hint = None`, and
        //   (3) `untargeted_decrypt_key` carries the ephemeral private.
        // Everything else mirrors the targeted payload.
        let token_minted_at = Hlc {
            wall_ms: 100_500,
            logical: 0,
            device_id: "alice-dev".into(),
        };
        // (2) Mint the token via the production primitive with invitee_hint = None.
        //     `&s.alice_comm_sk` derefs `Arc<SigningKey>` → `&SigningKey`.
        let invite_token = harmony_app::invite_mint::mint_invite_token(
            s.alice_addr, // inviter (community actor)
            None,         // invitee_hint = None → untargeted
            token_minted_at,
            None, // expires_at
            &s.alice_comm_sk,
        )
        .expect("mint invite token");
        // The case-A pkarr probe derives from the minted token's sig.
        let token_sig = invite_token.sig;

        // (1) Seal the epoch key to a FRESH EPHEMERAL key (NOT Bob's). The
        //     ephemeral private (sealed.untargeted_decrypt_key) is the ONLY way
        //     to recover the epoch key; Bob's enrolled key cannot open it.
        let sealed = harmony_app::invite_mint::seal_epoch_key(
            s.alice_minted.membership_key.as_bytes(),
            harmony_app::invite_mint::SealRecipient::Untargeted,
        )
        .expect("seal epoch key untargeted");
        assert_eq!(
            sealed.sealed.len(),
            92,
            "untargeted seal must produce the 92-byte X25519 envelope"
        );
        assert!(
            sealed.untargeted_decrypt_key.is_some(),
            "untargeted seal must surface the ephemeral private for the URL"
        );

        let invite_payload = CommunityInvitePayload {
            inviter_signer_certs: Vec::new(),
            community_id: s.community_id,
            epoch_snapshot: InviteEpochSnapshot {
                epoch: 0,
                // (1) sealed to the ephemeral key, not to Bob.
                sealed_epoch_key: sealed.sealed,
                sealed_epoch_keys: Vec::new(),
                state_snapshot: MaterializedCommunityState::default(),
            },
            admin_addr: s.alice_addr,
            community_name: "OptionAHandshakeCommunity".into(),
            is_invite_only: true,
            expires_at: None,
            invite_token: Some(invite_token),
            admin_bootstrap: Some(s.alice_minted.bootstrap_join.clone()),
            inviter_identity_pub: Some(s.alice_pub),
            forked_from: None,
            pre_fork_snapshot: None,
            inviter_enrollment: Some(s.alice_comm.cert.clone()),
            // (3) the ephemeral private rides in the URL — this is what lets Bob
            //     decrypt despite the seal target being a key he never held.
            untargeted_decrypt_key: sealed.untargeted_decrypt_key,
        };
        let invite_url =
            community_invite::encode_invite_url(&invite_payload).expect("encode untargeted invite");

        // Publish Alice's case-A pkarr record (keyed on the minted token sig)
        // and wait for visibility before driving Bob's IPC.
        s.invite_pub.register_invite(&invite_payload).await;
        let probe_verifying = await_pkarr_record_visible(&s.pkarr_resolver, &token_sig).await;

        // ── 7. Drive Bob's IPC. ─────────────────────────────────────────
        // Bob passes his OWN bob_comm_sk as signing_key. The epoch key was NOT
        // sealed to that key; recovery is only possible via the URL's
        // untargeted_decrypt_key. (Call signature mirrors the targeted test.)
        let nav_emits: Arc<Mutex<Vec<harmony_app::NavUpdatedPayload>>> =
            Arc::new(Mutex::new(Vec::new()));
        let nav_emits_sink = Arc::clone(&nav_emits);
        let outcome = harmony_app::connectivity_redeem_invite_iroh_inner(
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
            None, // ZEB-434: no transport-epoch watch in this test
            Arc::clone(&s.bob_dm_outbox),
            Arc::clone(&s.bob_channel_log_registry),
            // ZEB-427: legacy tests predate the durability fence; pass no
            // engine (the fence logs and skips — behavior under test in
            // zeb427_iroh_redeem_fences_owner_state_space_to_disk).
            None,
            None,
            |_| {},
            move |payload: harmony_app::NavUpdatedPayload| {
                nav_emits_sink
                    .lock()
                    .expect("nav_emits mutex")
                    .push(payload);
            },
            harmony_app::HandshakeDialConfig {
                connect_timeout: Duration::from_millis(10_000),
                open_bi_timeout: Duration::from_millis(10_000),
                response_read_timeout: Duration::from_millis(10_000),
                write_timeout: Duration::from_millis(10_000),
            },
            || Ok(()),
        )
        .await
        .expect(
            "connectivity_redeem_invite_iroh_inner must Ok (it converts internal errors \
             into outcome.status)",
        );

        // ── 8. The proof. ───────────────────────────────────────────────
        // outcome.status == "joined" is the airtight proof: Bob could ONLY have
        // recovered the epoch key via the URL's untargeted_decrypt_key, because
        // the seal target was a fresh ephemeral X25519 key Bob never held (his
        // bob_comm_sk-derived key was NOT the seal recipient). A decryption
        // failure in the untargeted branch would surface here as a non-"joined"
        // status; "joined" means the untargeted decrypt path executed correctly.
        assert_eq!(
            outcome.status, "joined",
            "untargeted invite-only redeem must return 'joined' — Bob recovered the \
             epoch key via the URL's untargeted_decrypt_key (the seal target was an \
             ephemeral key he never had). Got status={:?} community_id={:?}.",
            outcome.status, outcome.community_id
        );
        assert_eq!(
            outcome.community_id.as_deref(),
            Some(hex::encode(s.community_id.0).as_str()),
            "community_id must echo Alice's invite"
        );

        // Structural cross-checks (apply equally to the untargeted path):
        // Bob materializes as Joined off a CRDT that carries the JoinCountersign.
        let bob_state = s
            .registry_bob
            .state_for(&s.community_id)
            .await
            .expect("bob state must exist after redeem");
        let bob_events: Vec<_> = {
            let g = bob_state.lock().await;
            g.events().cloned().collect()
        };
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
        let bob_materialized = materialize(&bob_events, s.alice_addr);
        assert_eq!(
            bob_materialized.members.get(&s.bob_addr).map(|m| m.status),
            Some(MemberStatus::Joined),
            "Bob must materialize as Joined after the untargeted handshake completes"
        );

        // ZEB-367 / ZEB-874 unregister-on-consume (e2e). Alice's acceptor was
        // built with Some(invite_pub); once the handshake completes and the
        // acceptor has written the countersign back, it calls
        // unregister_invite(&invite_token.sig) (ZEB-874 moved this burn off
        // handle_unicast's insert), removing the case-A publication from the
        // publisher's active set so it stops republishing.
        //
        // NOTE: `PkarrPublisher::unregister` only stops FUTURE republishes — it
        // does not send a DELETE to the relay, so the already-PUT record lingers
        // in the mock relay until its TTL expires. We therefore observe the
        // unregister via the publisher's active-handle set (`active_handles()`),
        // not by re-resolving the relay (which would keep returning the stale
        // record for far longer than the test's 5s budget). The handle format
        // mirrors `PkarrInvitePublisher::register_invite`: `invite:{hex(sig)}`.
        // `probe_verifying` is retained for the resolve above; the
        // active-handle check is the deterministic teardown signal.
        let _ = probe_verifying;
        let invite_handle = format!("invite:{}", hex::encode(token_sig));
        let mut handle_gone = false;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if !s
                .pkarr_publisher
                .active_handles()
                .await
                .contains(&invite_handle)
            {
                handle_gone = true;
                break;
            }
        }
        assert!(
            handle_gone,
            "ZEB-367 / ZEB-874: Alice's case-A invite publication must be unregistered \
             (handle {invite_handle:?} dropped from active_handles) within 5s after \
             the invite is consumed (acceptor unregisters after the countersign write)"
        );

        // Graceful teardown. Abort the long-lived pkarr publisher task FIRST so it
        // stops using the endpoints before we shut them down, and so no background
        // work leaks into later tests.
        s.publisher_handle.abort();
        s.alice_ep.shutdown().await;
        s.bob_ep.shutdown().await;
    })
    .await
    .expect("invite_only_untargeted_generate_then_redeem_roundtrip timed out at 60s");
}

// ────────────────────────────────────────────────────────────────────────────
// ZEB-427 / ZEB-509: durability fence on the iroh-handshake redemption path.
// ZEB-427 proved the fence runs (the Space row reaches disk on commit); ZEB-509
// strengthens it to prove the redeemer persists a *usable live epoch key*, not a
// Space shell — guarding the `LiveEpochKeyMissing` symptom #307 fixed.
//
// This was the ONE join path missing ZEB-393 Bug A's durable-on-commit
// fence: `create_community`, the legacy `redeem_invite`, and
// `join_open_community` all flush the owner-state SyncEngine after
// committing the Space, but the iroh path (the one the UI drives) did
// not — so a join stayed memory-only until some unrelated owner-state
// write happened to flush the doc. On the next cold boot the membership
// vanished, and re-joining deadlocked: the persisted per-community dir
// still materialized the joiner as Active ("PendingJoin actor's prior
// state is already-engaged") while the missing Space row blocked engine
// spawn ("no engine — not currently joined"). Live repro 2026-06-10,
// documented on ZEB-427.
//
// The engine here is constructed with a debounce far longer than the
// test budget (10 min vs 60 s), so a debounced write can never satisfy
// the assertion — the persisted file can only exist because the
// explicit `flush_now` fence ran before the IPC returned.
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zeb427_iroh_redeem_fences_owner_state_space_to_disk() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("harmony_app=warn")),
        )
        .with_test_writer()
        .try_init();

    // ZEB-374 audit: setup_two_party_iroh_handshake binds two real iroh
    // endpoints INSIDE the 60s budget. Pre-pay iroh's ~30s first-bind global
    // init OUTSIDE the budget (the ZEB-347 pattern) so full-suite contention
    // can't charge it against the handshake and flake the timeout.
    harmony_app::iroh_endpoint::warm_up_iroh_global_init().await;

    tokio::time::timeout(Duration::from_secs(60), async {
        let s = setup_two_party_iroh_handshake().await;

        // Targeted invite-only invite URL — same construction as
        // `bob_joins_alice_via_iroh_handshake_option_a` (see that test
        // for the field-by-field rationale).
        let token_minted_at = Hlc {
            wall_ms: 100_500,
            logical: 0,
            device_id: "alice-dev".into(),
        };
        let invite_token_unsigned = InviteToken {
            inviter: s.alice_addr,
            invitee_hint: Some(s.bob_addr),
            minted_at: token_minted_at.clone(),
            expires_at: None,
            sig: [0u8; 64],
        };
        let token_payload_bytes =
            canonical_invite_token_bytes(&invite_token_unsigned).expect("canonical token bytes");
        let token_sig: [u8; 64] = s.alice_comm_sk.sign(&token_payload_bytes).to_bytes();
        let invite_token = InviteToken {
            inviter: s.alice_addr,
            invitee_hint: Some(s.bob_addr),
            minted_at: token_minted_at,
            expires_at: None,
            sig: token_sig,
        };
        let bob_x25519_pub = {
            let verifying_bytes = s.bob_comm_sk.verifying_key().to_bytes();
            harmony_app::dm_signing::ed25519_pub_to_x25519(&verifying_bytes)
                .expect("bob_comm ed25519→x25519")
        };
        let sealed_epoch_key = harmony_app::dm_signing::seal_to_owner(
            &bob_x25519_pub,
            s.alice_minted.membership_key.as_bytes(),
        )
        .expect("seal epoch key to bob");
        let invite_payload = CommunityInvitePayload {
            inviter_signer_certs: Vec::new(),
            community_id: s.community_id,
            epoch_snapshot: InviteEpochSnapshot {
                epoch: 0,
                // ZEB-369: targeted invite — sealed envelope rides in sealed_epoch_keys.
                sealed_epoch_key: Vec::new(),
                sealed_epoch_keys: vec![sealed_epoch_key],
                state_snapshot: MaterializedCommunityState::default(),
            },
            admin_addr: s.alice_addr,
            community_name: "Zeb427FenceCommunity".into(),
            is_invite_only: true,
            expires_at: None,
            invite_token: Some(invite_token),
            admin_bootstrap: Some(s.alice_minted.bootstrap_join.clone()),
            inviter_identity_pub: Some(s.alice_pub),
            forked_from: None,
            pre_fork_snapshot: None,
            inviter_enrollment: Some(s.alice_comm.cert.clone()),
            untargeted_decrypt_key: None,
        };
        let invite_url =
            community_invite::encode_invite_url(&invite_payload).expect("encode invite");

        s.invite_pub.register_invite(&invite_payload).await;
        let _probe_verifying = await_pkarr_record_visible(&s.pkarr_resolver, &token_sig).await;

        // ── Bob's owner-state SyncEngine over a temp identity dir. ──────
        // Shares the SAME `bob_crdt_state` Arc the redeem mutates, so the
        // fence's snapshot includes the Space row the join inserts —
        // mirroring production wiring (start_node hands the engine the
        // same Arc the IPC handlers mutate).
        let persist_dir = tempfile::tempdir().expect("persist dir");
        let persist_paths = harmony_app::owner_state_sync::PersistPaths {
            crdt: persist_dir.path().join("owner_state_crdt.cbor"),
            replay: persist_dir.path().join("state_root_replay.cbor"),
        };
        let kt = Arc::new(
            harmony_app::owner_state_crypto::KeyTree::derive(&[0xB7u8; 32]).expect("keytree"),
        );
        let (engine_pub_tx, mut engine_pub_rx) = mpsc::channel::<Vec<u8>>(16);
        // Drain the engine's outbound publishes so flush_now can never
        // block on a full channel.
        let drain = tokio::spawn(async move { while engine_pub_rx.recv().await.is_some() {} });
        let (_engine_sub_tx, engine_sub_rx) = mpsc::channel::<Vec<u8>>(16);
        let sync_engine = Arc::new(harmony_app::owner_state_sync::SyncEngine::new(
            Some(harmony_app::owner_state_crypto::FleetKeySet::new(kt)),
            "bob-dev".to_string(),
            Arc::clone(&s.bob_crdt_state),
            Arc::clone(&s.bob_hlc_tracker),
            Arc::new(harmony_app::content_store::InMemoryStub::default()) as Arc<dyn ContentStore>,
            engine_pub_tx,
            engine_sub_rx,
            persist_paths.clone(),
            harmony_app::device_dataset_file::test_cipher(),
            // Debounce >> test budget: only the explicit fence can write.
            600_000,
            s.bob_adopt_floor.clone(),
        ));

        let outcome = harmony_app::connectivity_redeem_invite_iroh_inner(
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
            None, // ZEB-434: no transport-epoch watch in this test
            Arc::clone(&s.bob_dm_outbox),
            Arc::clone(&s.bob_channel_log_registry),
            // ZEB-427: the engine under test — the fence must flush it
            // before the inner returns "joined".
            Some(Arc::clone(&sync_engine)),
            None,
            |_| {},
            |_payload: harmony_app::NavUpdatedPayload| {},
            harmony_app::HandshakeDialConfig {
                connect_timeout: Duration::from_millis(10_000),
                open_bi_timeout: Duration::from_millis(10_000),
                response_read_timeout: Duration::from_millis(10_000),
                write_timeout: Duration::from_millis(10_000),
            },
            || Ok(()),
        )
        .await
        .expect("connectivity_redeem_invite_iroh_inner must Ok");

        assert_eq!(
            outcome.status, "joined",
            "happy path must join; got status={:?} community_id={:?}",
            outcome.status, outcome.community_id
        );

        // THE load-bearing assertion: the joined community's Space row is
        // on disk by the time the IPC returns. With a 10-minute debounce
        // and no other owner-state writes in this test, only the ZEB-427
        // fence can have produced this file.
        let loaded = harmony_app::owner_state_persist::load_crdt(
            &harmony_app::device_dataset_file::test_cipher(),
            &persist_paths.crdt,
        )
        .expect(
            "owner_state_crdt.cbor must exist and decode immediately after the joined \
             outcome — the ZEB-427 durability fence did not run",
        );
        assert!(
            loaded.spaces.contains_key(&s.community_id),
            "persisted owner-state must contain the joined community's Space row; \
             persisted space ids = {:?}",
            loaded.spaces.keys().collect::<Vec<_>>()
        );

        // ZEB-509 regression guard: a persisted Space ROW is necessary but not
        // sufficient. The #307 deadlock surfaced as `LiveEpochKeyMissing` — the
        // redeemer reloaded a Space whose epoch key was absent, so it looked joined
        // yet could never serve or recover (the keystone behind the deposit→recover
        // / 3-node / cross-WAN stalls). `contains_key` above would still pass if a
        // future regression persisted a Space shell without its epoch. Assert the
        // reloaded Space carries the *exact* live epoch key Alice sealed into the
        // invite (epoch 0), proving the redeemer persisted a usable epoch — the
        // precise thing `live_epoch_key` needs on the serve/recover path.
        let reloaded_space = loaded
            .spaces
            .get(&s.community_id)
            .expect("joined community Space row present (asserted above)");
        assert_eq!(
            reloaded_space
                .current_epoch_key
                .as_ref()
                .map(|k| k.as_bytes()),
            Some(s.alice_minted.membership_key.as_bytes()),
            "reloaded redeemer Space must carry the sealed live epoch key \
             (current_epoch_key) — LiveEpochKeyMissing was the #307 symptom"
        );
        assert_eq!(
            reloaded_space.current_epoch,
            Some(0),
            "reloaded redeemer Space must carry the epoch counter (epoch 0 at join)"
        );

        // Graceful teardown (mirrors the option_a test) + engine shutdown.
        let _ = sync_engine.shutdown().await;
        drain.abort();
        s.publisher_handle.abort();
        s.alice_ep.shutdown().await;
        s.bob_ep.shutdown().await;
    })
    .await
    .expect("zeb427_iroh_redeem_fences_owner_state_space_to_disk timed out at 60s");
}

// ────────────────────────────────────────────────────────────────────────────
// ZEB-874 Tier 1: a redeem that fails AFTER the host's local insert must NOT
// burn the single-use invite. Alice's acceptor runs with poll_deadline=0. The
// acceptor's poll loop checks the deadline BEFORE scanning state (the ZEB-874
// ordering in handle_invite_handshake_inbound), so a zero deadline times out on
// the first iteration WITHOUT ever observing a countersign — the async
// auto-counter-sign task cannot win a race that never runs. The acceptor thus
// deterministically returns CountersignTimeout right after handle_unicast
// inserts Bob's PendingJoin: a post-insert, pre-delivery failure. Pre-ZEB-874
// handle_unicast had already burned the invite on that insert; post-ZEB-874 the
// burn lives after the countersign write, which is never reached, so the invite
// stays live.
//
// The test asserts both halves: (1) Alice DID insert Bob's PendingJoin, so the
// failure is genuinely post-insert (pre-ZEB-874 would have burned here); and
// (2) the invite handle is STILL in active_handles (post-ZEB-874 did not burn).
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invite_not_burned_when_handshake_fails_after_insert() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("harmony_app=warn")),
        )
        .with_test_writer()
        .try_init();

    harmony_app::iroh_endpoint::warm_up_iroh_global_init().await;

    tokio::time::timeout(Duration::from_secs(60), async {
        // poll_deadline = 0 → the acceptor CountersignTimeouts right after the
        // insert, before writing any countersign back.
        let s = setup_two_party_iroh_handshake_with_config(
            harmony_app::iroh_invite_acceptor::HandshakeAcceptorConfig {
                io_deadline: Duration::from_millis(10_000),
                poll_deadline: Duration::ZERO,
                poll_interval: Duration::from_millis(20),
            },
        )
        .await;

        // Targeted invite-only URL — same construction as
        // `bob_joins_alice_via_iroh_handshake_option_a`.
        let token_minted_at = Hlc {
            wall_ms: 100_500,
            logical: 0,
            device_id: "alice-dev".into(),
        };
        let invite_token_unsigned = InviteToken {
            inviter: s.alice_addr,
            invitee_hint: Some(s.bob_addr),
            minted_at: token_minted_at.clone(),
            expires_at: None,
            sig: [0u8; 64],
        };
        let token_payload_bytes =
            canonical_invite_token_bytes(&invite_token_unsigned).expect("canonical token bytes");
        let token_sig: [u8; 64] = s.alice_comm_sk.sign(&token_payload_bytes).to_bytes();
        let invite_token = InviteToken {
            inviter: s.alice_addr,
            invitee_hint: Some(s.bob_addr),
            minted_at: token_minted_at,
            expires_at: None,
            sig: token_sig,
        };
        let bob_x25519_pub = {
            let verifying_bytes = s.bob_comm_sk.verifying_key().to_bytes();
            harmony_app::dm_signing::ed25519_pub_to_x25519(&verifying_bytes)
                .expect("bob_comm ed25519→x25519")
        };
        let sealed_epoch_key = harmony_app::dm_signing::seal_to_owner(
            &bob_x25519_pub,
            s.alice_minted.membership_key.as_bytes(),
        )
        .expect("seal epoch key to bob");
        let invite_payload = CommunityInvitePayload {
            inviter_signer_certs: Vec::new(),
            community_id: s.community_id,
            epoch_snapshot: InviteEpochSnapshot {
                epoch: 0,
                sealed_epoch_key: Vec::new(),
                sealed_epoch_keys: vec![sealed_epoch_key],
                state_snapshot: MaterializedCommunityState::default(),
            },
            admin_addr: s.alice_addr,
            community_name: "OptionAHandshakeCommunity".into(),
            is_invite_only: true,
            expires_at: None,
            invite_token: Some(invite_token),
            admin_bootstrap: Some(s.alice_minted.bootstrap_join.clone()),
            inviter_identity_pub: Some(s.alice_pub),
            forked_from: None,
            pre_fork_snapshot: None,
            inviter_enrollment: Some(s.alice_comm.cert.clone()),
            untargeted_decrypt_key: None,
        };
        let invite_url =
            community_invite::encode_invite_url(&invite_payload).expect("encode invite");

        s.invite_pub.register_invite(&invite_payload).await;
        let _probe = await_pkarr_record_visible(&s.pkarr_resolver, &token_sig).await;
        let invite_handle = format!("invite:{}", hex::encode(token_sig));
        assert!(
            s.pkarr_publisher
                .active_handles()
                .await
                .contains(&invite_handle),
            "precondition: the case-A invite handle must be registered before the redeem"
        );

        // Drive Bob's redeem. The acceptor inserts Bob's PendingJoin, then
        // CountersignTimeouts (poll_deadline=0) before writing anything back.
        let outcome = harmony_app::connectivity_redeem_invite_iroh_inner(
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
            harmony_app::HandshakeDialConfig {
                connect_timeout: Duration::from_millis(10_000),
                open_bi_timeout: Duration::from_millis(10_000),
                response_read_timeout: Duration::from_millis(10_000),
                write_timeout: Duration::from_millis(10_000),
            },
            || Ok(()),
        )
        .await
        .expect("connectivity_redeem_invite_iroh_inner must Ok (errors → non-joined status)");

        // ZEB-899: post-write failure now latches (joined + pending) instead of
        // reporting unreachable. The ZEB-874 burn assertions below are
        // unchanged — the latch is joiner-local and must not burn the invite.
        assert_eq!(
            outcome.status, "joined",
            "ZEB-899: a post-write failure must latch as joined+pending; got status={:?}",
            outcome.status
        );
        assert!(
            outcome.pending,
            "ZEB-899: the latched join must report pending=true"
        );

        // Grace window for the spawned auto-counter-sign task and teardown to
        // settle before the final observations.
        tokio::time::sleep(Duration::from_millis(500)).await;

        // (1) Prove the failure is genuinely POST-insert: Alice's handle_unicast
        // DID insert Bob's PendingJoin, so pre-ZEB-874 the burn would have fired
        // here. Without this, the invite-still-active assertion could vacuously
        // pass on a failure that occurred before the insert.
        let alice_state = s
            .registry_alice
            .state_for(&s.community_id)
            .await
            .expect("alice state must exist");
        let alice_events: Vec<_> = {
            let g = alice_state.lock().await;
            g.events().cloned().collect()
        };
        let alice_has_pending = alice_events.iter().any(|e| {
            matches!(
                &e.kind,
                harmony_app::community_membership::MembershipEventKind::PendingJoin { .. }
            ) && e.actor == s.bob_addr
        });
        assert!(
            alice_has_pending,
            "Alice's handle_unicast must have inserted Bob's PendingJoin — the \
             invite-still-active assertion only proves the ZEB-874 invariant when the \
             failure is genuinely POST-insert (pre-ZEB-874 would have burned here)"
        );

        // (2) THE load-bearing assertion: the single-use invite must remain live
        // — pre-ZEB-874 handle_unicast burned it on the insert proven above.
        assert!(
            s.pkarr_publisher
                .active_handles()
                .await
                .contains(&invite_handle),
            "ZEB-874: a redeem that fails after the host's insert (CountersignTimeout) \
             must NOT burn the single-use invite — handle {invite_handle:?} must still \
             be in active_handles so the legitimate joiner can retry"
        );

        s.publisher_handle.abort();
        s.alice_ep.shutdown().await;
        s.bob_ep.shutdown().await;
    })
    .await
    .expect("invite_not_burned_when_handshake_fails_after_insert timed out at 60s");
}

/// ZEB-889 test helper: build the targeted invite-only payload / URL / token_sig
/// used by the mint-reuse tests (same shape as the negative test's inline copy).
pub(crate) fn zeb889_build_targeted_invite(
    s: &TwoPartySetup,
) -> (CommunityInvitePayload, String, [u8; 64]) {
    let token_minted_at = Hlc {
        wall_ms: 100_500,
        logical: 0,
        device_id: "alice-dev".into(),
    };
    let invite_token_unsigned = InviteToken {
        inviter: s.alice_addr,
        invitee_hint: Some(s.bob_addr),
        minted_at: token_minted_at.clone(),
        expires_at: None,
        sig: [0u8; 64],
    };
    let token_payload_bytes =
        canonical_invite_token_bytes(&invite_token_unsigned).expect("canonical token bytes");
    let token_sig: [u8; 64] = s.alice_comm_sk.sign(&token_payload_bytes).to_bytes();
    let invite_token = InviteToken {
        inviter: s.alice_addr,
        invitee_hint: Some(s.bob_addr),
        minted_at: token_minted_at,
        expires_at: None,
        sig: token_sig,
    };
    let bob_x25519_pub = {
        let verifying_bytes = s.bob_comm_sk.verifying_key().to_bytes();
        harmony_app::dm_signing::ed25519_pub_to_x25519(&verifying_bytes)
            .expect("bob_comm ed25519→x25519")
    };
    let sealed_epoch_key = harmony_app::dm_signing::seal_to_owner(
        &bob_x25519_pub,
        s.alice_minted.membership_key.as_bytes(),
    )
    .expect("seal epoch key to bob");
    let invite_payload = CommunityInvitePayload {
        inviter_signer_certs: Vec::new(),
        community_id: s.community_id,
        epoch_snapshot: InviteEpochSnapshot {
            epoch: 0,
            sealed_epoch_key: Vec::new(),
            sealed_epoch_keys: vec![sealed_epoch_key],
            state_snapshot: MaterializedCommunityState::default(),
        },
        admin_addr: s.alice_addr,
        community_name: "OptionAHandshakeCommunity".into(),
        is_invite_only: true,
        expires_at: None,
        invite_token: Some(invite_token),
        admin_bootstrap: Some(s.alice_minted.bootstrap_join.clone()),
        inviter_identity_pub: Some(s.alice_pub),
        forked_from: None,
        pre_fork_snapshot: None,
        inviter_enrollment: Some(s.alice_comm.cert.clone()),
        untargeted_decrypt_key: None,
    };
    let invite_url = community_invite::encode_invite_url(&invite_payload).expect("encode invite");
    (invite_payload, invite_url, token_sig)
}

/// ZEB-889: the PRODUCTION first attempt caches its minted redemption, so a
/// later retry has something to reuse. Drives `connectivity_redeem_invite_iroh_inner`
/// with `poll_deadline = 0` (the acceptor CountersignTimeouts before delivering,
/// so the redeem does NOT join) and asserts the mint was stored — the wiring the
/// seeded retry test relies on but does not itself exercise.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zeb889_first_attempt_caches_minted_redemption() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("harmony_app=warn")),
        )
        .with_test_writer()
        .try_init();

    harmony_app::iroh_endpoint::warm_up_iroh_global_init().await;

    tokio::time::timeout(Duration::from_secs(60), async {
        // poll_deadline = 0 → the acceptor CountersignTimeouts before writing the
        // countersign, so the redeem fails delivery (does NOT join). The mint is
        // stored at the mint site BEFORE the dial, so it must be cached regardless.
        let s = setup_two_party_iroh_handshake_with_config(
            harmony_app::iroh_invite_acceptor::HandshakeAcceptorConfig {
                io_deadline: Duration::from_millis(10_000),
                poll_deadline: Duration::ZERO,
                poll_interval: Duration::from_millis(20),
            },
        )
        .await;

        let (invite_payload, invite_url, token_sig) = zeb889_build_targeted_invite(&s);
        // Greptile #644: the cache is keyed by a digest of the whole payload,
        // not token.sig — mirror the production key here.
        let cache_key = invite_payload
            .redemption_mint_cache_key()
            .expect("payload cache key");
        s.invite_pub.register_invite(&invite_payload).await;
        let _probe = await_pkarr_record_visible(&s.pkarr_resolver, &token_sig).await;

        // Real wall-clock now — production reads the cache with SystemTime::now()
        // and the cache TTL-purges stale entries.
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        // Precondition: nothing cached yet.
        assert!(
            s.registry_bob
                .get_redemption_mint(cache_key, now_ms)
                .await
                .is_none(),
            "precondition: Bob has no cached mint before the first attempt"
        );

        let outcome = harmony_app::connectivity_redeem_invite_iroh_inner(
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
            |_p: harmony_app::NavUpdatedPayload| {},
            harmony_app::HandshakeDialConfig {
                connect_timeout: Duration::from_millis(10_000),
                open_bi_timeout: Duration::from_millis(10_000),
                response_read_timeout: Duration::from_millis(10_000),
                write_timeout: Duration::from_millis(10_000),
            },
            || Ok(()),
        )
        .await
        .expect("connectivity_redeem_invite_iroh_inner must Ok (errors → non-joined status)");

        // ZEB-899: the request was fully written and Alice's handle_unicast
        // committed the PendingJoin (poll_deadline=0 only suppresses the
        // RESPONSE) — a post-write failure now latches the join as pending
        // instead of falsely reporting the inviter unreachable.
        assert_eq!(
            outcome.status, "joined",
            "ZEB-899: a post-write failure (no countersign response) must latch, \
             not report unreachable; got status={:?}",
            outcome.status
        );
        assert!(
            outcome.pending,
            "ZEB-899: the latched join must report pending=true (no countersign \
             was applied in-band)"
        );
        {
            let g = s.bob_crdt_state.lock().await;
            let row = g
                .spaces
                .get(&s.community_id)
                .expect("ZEB-899: the latch must commit Bob's owner-state Space row");
            assert!(
                row.pending_join_at.is_some(),
                "ZEB-899: the latched Space row must carry pending_join_at (greyed \
                 until the JoinCountersign converges); got {:?}",
                row.pending_join_at
            );
        }

        // The load-bearing assertion: the production first attempt cached its mint,
        // so a subsequent retry can reuse it (the recovery leg this PR enables).
        assert!(
            s.registry_bob
                .get_redemption_mint(cache_key, now_ms)
                .await
                .is_some(),
            "ZEB-889: the first attempt must cache its minted redemption for retry reuse"
        );

        // ZEB-899: the latch spawned a real engine on Bob's registry — shut it
        // down deterministically so its background tasks can't outlive the test
        // (nextest leak detection). Best-effort: this harness has no live
        // adapter transport, so the final flush reports TransportClosed even
        // though the engine tasks are torn down.
        let _ = s.registry_bob.shutdown_all().await;
        s.publisher_handle.abort();
        s.alice_ep.shutdown().await;
        s.bob_ep.shutdown().await;
    })
    .await
    .expect("zeb889_first_attempt_caches_minted_redemption timed out at 60s");
}

/// ZEB-899: when the post-write LATCH itself cannot commit (here: the
/// generation fence rejects — node stopped mid-redeem), nothing landed
/// locally, so the outcome degrades to the honest legacy classification
/// (`inviter_unreachable`, which keeps the LAN-fallback affordance) — NOT
/// `join_failed` (which asserts the inviter was reached and suppresses the
/// fallback) and NOT `joined`. The mint stays cached for a later retry.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zeb899_latch_commit_failure_degrades_to_unreachable() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("harmony_app=warn")),
        )
        .with_test_writer()
        .try_init();

    harmony_app::iroh_endpoint::warm_up_iroh_global_init().await;

    tokio::time::timeout(Duration::from_secs(60), async {
        // poll_deadline = 0 → no response is written → the joiner enters latch
        // mode after its response read times out.
        let s = setup_two_party_iroh_handshake_with_config(
            harmony_app::iroh_invite_acceptor::HandshakeAcceptorConfig {
                io_deadline: Duration::from_millis(10_000),
                poll_deadline: Duration::ZERO,
                poll_interval: Duration::from_millis(20),
            },
        )
        .await;

        let (invite_payload, invite_url, token_sig) = zeb889_build_targeted_invite(&s);
        let cache_key = invite_payload
            .redemption_mint_cache_key()
            .expect("payload cache key");
        s.invite_pub.register_invite(&invite_payload).await;
        let _probe = await_pkarr_record_visible(&s.pkarr_resolver, &token_sig).await;

        let outcome = harmony_app::connectivity_redeem_invite_iroh_inner(
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
            |_p: harmony_app::NavUpdatedPayload| {},
            harmony_app::HandshakeDialConfig {
                connect_timeout: Duration::from_millis(10_000),
                open_bi_timeout: Duration::from_millis(10_000),
                response_read_timeout: Duration::from_millis(2_000),
                write_timeout: Duration::from_millis(10_000),
            },
            // The generation fence rejects: the ONLY fence evaluation on this
            // run happens inside the latch-mode inner (the handshake never
            // reaches the delivered path), so a constant Err drives the
            // latch-commit-failure arm deterministically.
            || {
                Err(harmony_app::community_invite::RedeemInviteError::new(
                    harmony_app::community_invite::RedeemInviteErrorCode::GenerationChanged,
                    "forced fence failure (ZEB-899 latch-degrade test)".to_string(),
                ))
            },
        )
        .await
        .expect("connectivity_redeem_invite_iroh_inner must Ok (errors → non-joined status)");

        assert_eq!(
            outcome.status, "inviter_unreachable",
            "ZEB-899: a failed latch commit must degrade to the legacy unreachable \
             outcome (fallback affordance intact), not join_failed/joined; got {:?}",
            outcome.status
        );
        assert!(
            !s.bob_crdt_state
                .lock()
                .await
                .spaces
                .contains_key(&s.community_id),
            "ZEB-899: a failed latch must not leave a Space row (rollback)"
        );
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        assert!(
            s.registry_bob
                .get_redemption_mint(cache_key, now_ms)
                .await
                .is_some(),
            "ZEB-899: the mint stays cached for a later retry even when the latch fails"
        );

        s.publisher_handle.abort();
        s.alice_ep.shutdown().await;
        s.bob_ep.shutdown().await;
    })
    .await
    .expect("zeb899_latch_commit_failure_degrades_to_unreachable timed out at 60s");
}

/// ZEB-889: a legitimate joiner whose first countersign delivery failed can
/// retry and redeem the still-live invite. The retry reuses the cached mint
/// (same bootstrap_join id) so the host's AlreadyKnown-retransmit path
/// re-delivers the countersign — instead of minting a fresh id P6 rejects,
/// which would leave a permanent unredeemable zombie invite.
///
/// The "first attempt landed host-side but its delivery failed" state is seeded
/// deterministically (Alice's engine holds Bob's committed P1 + a genuine CS1,
/// the invite is still registered, and Bob's registry cache holds P1's mint),
/// so a single redeem exercises the retry leg under the default (nonzero)
/// acceptor poll_deadline that delivers the already-present countersign.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zeb889_retry_reuses_mint_and_redeems_zombie_invite() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("harmony_app=warn")),
        )
        .with_test_writer()
        .try_init();

    harmony_app::iroh_endpoint::warm_up_iroh_global_init().await;

    tokio::time::timeout(Duration::from_secs(60), async {
        let s = setup_two_party_iroh_handshake().await;

        // Targeted invite-only URL (shared builder — same shape as the negative test).
        let (invite_payload, invite_url, token_sig) = zeb889_build_targeted_invite(&s);
        // Greptile #644: production keys the cache by a digest of the whole
        // payload, not token.sig — seed/read the cache with the same key.
        let cache_key = invite_payload
            .redemption_mint_cache_key()
            .expect("payload cache key");

        // Register the case-A invite so active_handles carries it (burn target).
        s.invite_pub.register_invite(&invite_payload).await;
        let _probe = await_pkarr_record_visible(&s.pkarr_resolver, &token_sig).await;
        let invite_handle = format!("invite:{}", hex::encode(token_sig));
        assert!(
            s.pkarr_publisher
                .active_handles()
                .await
                .contains(&invite_handle),
            "precondition: the case-A invite handle must be registered before the retry"
        );

        // --- Seed the "first attempt landed host-side, delivery failed" state. ---
        // 1. Mint P1 for Bob exactly as connectivity_redeem_invite_iroh_inner
        //    would, with a FIXED join_hlc so the seeded copy and Bob's reused
        //    copy are byte-identical.
        let join_hlc = Hlc {
            wall_ms: 100_600,
            logical: 0,
            device_id: "bob-dev".into(),
        };
        let p1_mint = harmony_app::mint_redemption(
            &invite_payload,
            s.bob_addr,
            s.bob_comm_sk.as_ref(),
            &s.bob_comm.cert,
            join_hlc,
        )
        .expect("mint P1 for bob");
        let p1 = p1_mint.bootstrap_join.clone();

        // 2. Seed Bob's registry cache with P1's mint (models the first attempt
        //    having stored it before its delivery failed). Use real wall-clock
        //    now — the production redeem reads the cache with SystemTime::now(),
        //    and the cache TTL-purges entries older than the retry window.
        let seed_now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        s.registry_bob
            .get_or_store_redemption_mint(cache_key, seed_now_ms, p1_mint.clone())
            .await;

        // 3. Seed Alice's engine with P1 + a genuine CS1 (Alice's JoinCountersign
        //    targeting P1.id, signed with her device key). insert_verified_for_test
        //    bypasses verify/precheck — fine, we are reproducing committed state;
        //    CS1 carries a real Alice signature so Bob's engine accepts it on
        //    delivery (Bob's redeem inserts Alice's admin bootstrap first).
        let cs1 = {
            use harmony_app::community_membership::{
                sign_event, EventPayload, MembershipEventKind,
            };
            let payload = EventPayload {
                id: [0xc5; 16],
                community_id: s.community_id,
                kind: MembershipEventKind::JoinCountersign {
                    target_event_id: p1.id,
                },
                actor: s.alice_addr,
                at: Hlc {
                    wall_ms: 100_700,
                    logical: 0,
                    device_id: "alice-dev".into(),
                },
            };
            sign_event(&payload, s.alice_comm_sk.as_ref()).expect("sign CS1")
        };
        {
            let state = s
                .registry_alice
                .state_for(&s.community_id)
                .await
                .expect("alice state exists");
            let mut g = state.lock().await;
            g.insert_verified_for_test(p1.clone());
            g.insert_verified_for_test(cs1);
        }

        // --- ZEB-899: seed Bob's LATCHED-PENDING Space — the state the
        //     post-write latch now commits on a failed first attempt (drive
        //     the same inner the latch call uses: pre_minted, no countersign,
        //     short redeem window). The retry below then runs over an
        //     EXISTING pending Space + spawned engine, which is the real
        //     retry-after-latch shape.
        let latch_dto = harmony_app::redeem_invite_inner_with_overrides(
            invite_url.clone(),
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
            || Ok(()),
            None,
            harmony_app::RedeemInviteOverrides {
                pre_minted: Some(p1_mint.clone()),
                redeem_timeout: Some(Duration::from_secs(1)),
                ..Default::default()
            },
        )
        .await
        .expect("ZEB-899: the latch seed must commit a pending Space, not Err");
        assert!(
            latch_dto.pending,
            "ZEB-899 precondition: the seeded latch must be pending; got {latch_dto:?}"
        );
        {
            let g = s.bob_crdt_state.lock().await;
            let row = g
                .spaces
                .get(&s.community_id)
                .expect("ZEB-899 precondition: latched Space row exists before the retry");
            assert!(row.pending_join_at.is_some());
        }

        // --- Drive the retry. Bob reuses P1 from cache → sends P1 → Alice
        //     AlreadyKnown → the acceptor's poll finds the seeded CS1 → delivers
        //     → Bob joins, and the acceptor burns the invite. ---
        let outcome = harmony_app::connectivity_redeem_invite_iroh_inner(
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
            |_p: harmony_app::NavUpdatedPayload| {},
            harmony_app::HandshakeDialConfig {
                connect_timeout: Duration::from_millis(10_000),
                open_bi_timeout: Duration::from_millis(10_000),
                response_read_timeout: Duration::from_millis(10_000),
                write_timeout: Duration::from_millis(10_000),
            },
            || Ok(()),
        )
        .await
        .expect("connectivity_redeem_invite_iroh_inner must Ok");

        // Load-bearing: only reuse-of-P1 lets this converge. A fresh mint would
        // hit P6 → EngineRejected → no delivery → not joined + invite still live.
        assert_eq!(
            outcome.status, "joined",
            "ZEB-889: the retry converges by reusing the cached mint; got status={:?}",
            outcome.status
        );

        // ZEB-899: the retry delivered the countersign in-band over the
        // EXISTING latched Space/engine — the join is fully ratified now.
        assert!(
            !outcome.pending,
            "ZEB-899: the reused-mint retry must complete the latched join \
             (pending=false); got {outcome:?}"
        );

        // Poll (rather than a fixed sleep) until the acceptor's post-delivery
        // burn unregisters the invite — the burn happens on the acceptor task
        // just after it writes the countersign, so it races the redeem return.
        let mut burned = false;
        for _ in 0..50 {
            if !s
                .pkarr_publisher
                .active_handles()
                .await
                .contains(&invite_handle)
            {
                burned = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            burned,
            "ZEB-889: the single-use invite must be burned once the reused-mint retry \
             delivers the countersign — handle {invite_handle:?} must no longer be active \
             (no permanent zombie)"
        );

        // The join committed, so the reuse cache entry must be evicted (not left
        // pinning the mint / its epoch key material).
        assert!(
            s.registry_bob
                .get_redemption_mint(cache_key, seed_now_ms)
                .await
                .is_none(),
            "ZEB-889: the cached mint is evicted once the join commits"
        );

        // ZEB-899: the latch seed + retry spawned real engines on Bob's
        // registry — shut them down deterministically so their background
        // tasks can't outlive the test (nextest leak detection). Best-effort:
        // this harness has no live adapter transport, so the final flush
        // reports TransportClosed even though the engine tasks are torn down.
        let _ = s.registry_bob.shutdown_all().await;
        s.publisher_handle.abort();
        s.alice_ep.shutdown().await;
        s.bob_ep.shutdown().await;
    })
    .await
    .expect("zeb889_retry_reuses_mint_and_redeems_zombie_invite timed out at 60s");
}

/// ZEB-903 T1: the re-attempt driver converges a latched-pending join on a
/// transport-epoch bump — one round-trip over the live acceptor (reused
/// cached mint → AlreadyKnown retransmit → countersign delivered), clearing
/// `pending_join_at`, burning the invite, and evicting the mint cache. The
/// driver task itself must end (demand collapsed on convergence).
///
/// The seen-version is fixed pre-spawn (CodeAnt r1), so a single post-spawn
/// bump would suffice; the poll loop still bumps each iteration purely for
/// robustness against transient first-attempt failures.
#[tokio::test(flavor = "multi_thread")]
async fn zeb903_reattempt_driver_converges_latched_join_on_epoch_bump() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("harmony_app=warn")),
        )
        .with_test_writer()
        .try_init();

    harmony_app::iroh_endpoint::warm_up_iroh_global_init().await;

    tokio::time::timeout(Duration::from_secs(90), async {
        let s = setup_two_party_iroh_handshake().await;

        let (invite_payload, invite_url, token_sig) = zeb889_build_targeted_invite(&s);
        let cache_key = invite_payload
            .redemption_mint_cache_key()
            .expect("payload cache key");

        s.invite_pub.register_invite(&invite_payload).await;
        let _probe = await_pkarr_record_visible(&s.pkarr_resolver, &token_sig).await;
        let invite_handle = format!("invite:{}", hex::encode(token_sig));

        // Seed the "first attempt landed host-side, delivery failed" state —
        // identical to zeb889_retry_reuses_mint_and_redeems_zombie_invite:
        // P1 mint cached on Bob, P1 + a genuine CS1 on Alice's engine, and a
        // latched-pending Space committed on Bob.
        let join_hlc = Hlc {
            wall_ms: 100_600,
            logical: 0,
            device_id: "bob-dev".into(),
        };
        let p1_mint = harmony_app::mint_redemption(
            &invite_payload,
            s.bob_addr,
            s.bob_comm_sk.as_ref(),
            &s.bob_comm.cert,
            join_hlc,
        )
        .expect("mint P1 for bob");
        let p1 = p1_mint.bootstrap_join.clone();
        let seed_now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        s.registry_bob
            .get_or_store_redemption_mint(cache_key, seed_now_ms, p1_mint.clone())
            .await;
        let cs1 = {
            use harmony_app::community_membership::{
                sign_event, EventPayload, MembershipEventKind,
            };
            let payload = EventPayload {
                id: [0xc9; 16],
                community_id: s.community_id,
                kind: MembershipEventKind::JoinCountersign {
                    target_event_id: p1.id,
                },
                actor: s.alice_addr,
                at: Hlc {
                    wall_ms: 100_700,
                    logical: 0,
                    device_id: "alice-dev".into(),
                },
            };
            sign_event(&payload, s.alice_comm_sk.as_ref()).expect("sign CS1")
        };
        {
            let state = s
                .registry_alice
                .state_for(&s.community_id)
                .await
                .expect("alice state exists");
            let mut g = state.lock().await;
            g.insert_verified_for_test(p1.clone());
            g.insert_verified_for_test(cs1);
        }
        let latch_dto = harmony_app::redeem_invite_inner_with_overrides(
            invite_url.clone(),
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
            || Ok(()),
            None,
            harmony_app::RedeemInviteOverrides {
                pre_minted: Some(p1_mint.clone()),
                redeem_timeout: Some(Duration::from_secs(1)),
                ..Default::default()
            },
        )
        .await
        .expect("the latch seed must commit a pending Space, not Err");
        assert!(
            latch_dto.pending,
            "precondition: seeded latch must be pending"
        );

        // Arm the driver against Bob's live handles.
        let (epoch_tx, epoch_rx) = tokio::sync::watch::channel(0u64);
        let ctx = harmony_app::latched_join_reattempt::ReattemptContext {
            invite_url: invite_url.clone(),
            pkarr_resolver: Some(Arc::clone(&s.pkarr_resolver)),
            reachability_resolver: Some(s.bob_reachability.clone()),
            iroh_endpoint: Some(Arc::clone(&s.bob_ep)),
            crdt_state: Arc::clone(&s.bob_crdt_state),
            hlc_tracker: Arc::clone(&s.bob_hlc_tracker),
            adopt_floor: s.bob_adopt_floor.clone(),
            device_id: "bob-dev".to_string(),
            self_owner: s.bob_addr,
            community_signing_key: Arc::clone(&s.bob_comm_sk),
            enrollment_cert: s.bob_comm.cert.clone(),
            community_registry: Arc::clone(&s.registry_bob),
            community_adapter_tx: s.bob_adapter_tx.clone(),
            transport_epoch_rx: Some(epoch_rx),
            dm_outbox: Arc::clone(&s.bob_dm_outbox),
            channel_log_registry: Arc::clone(&s.bob_channel_log_registry),
            sync_engine: None,
            identity_dir: None,
            sink: None,
            dial_config: harmony_app::HandshakeDialConfig {
                connect_timeout: Duration::from_millis(10_000),
                open_bi_timeout: Duration::from_millis(10_000),
                response_read_timeout: Duration::from_millis(10_000),
                write_timeout: Duration::from_millis(10_000),
            },
        };
        let handle = harmony_app::latched_join_reattempt::spawn_reattempt_driver(ctx)
            .await
            .expect("driver must arm (decodable URL + epoch watch present)");

        // Bump-and-poll until the latched Space converges to ratified.
        let mut cleared = false;
        for _ in 0..300 {
            epoch_tx.send_modify(|e| *e = e.wrapping_add(1));
            {
                let g = s.bob_crdt_state.lock().await;
                if g.spaces
                    .get(&s.community_id)
                    .is_some_and(|row| row.pending_join_at.is_none())
                {
                    cleared = true;
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            cleared,
            "ZEB-903: the re-attempt driver must clear pending_join_at after an epoch bump"
        );

        // Convergence collapses the demand — the driver task must end.
        tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .expect("driver task must end after convergence")
            .expect("driver task must not panic");

        // Same downstream effects as a manual retry: burn + evict.
        let mut burned = false;
        for _ in 0..50 {
            if !s
                .pkarr_publisher
                .active_handles()
                .await
                .contains(&invite_handle)
            {
                burned = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            burned,
            "ZEB-903: the driver-completed join must burn the single-use invite"
        );
        assert!(
            s.registry_bob
                .get_redemption_mint(cache_key, seed_now_ms)
                .await
                .is_none(),
            "ZEB-903: the cached mint is evicted once the driver completes the join"
        );

        // Best-effort: this harness has no live adapter transport, so the final
        // flush reports TransportClosed even though the engine tasks are torn down.
        let _ = s.registry_bob.shutdown_all().await;
        s.publisher_handle.abort();
        s.alice_ep.shutdown().await;
        s.bob_ep.shutdown().await;
    })
    .await
    .expect("zeb903_reattempt_driver_converges_latched_join_on_epoch_bump timed out at 90s");
}

/// ZEB-903 T2: demand collapsed — no pending Space exists, so an epoch bump
/// makes the driver exit WITHOUT attempting. Control: the context's
/// `iroh_endpoint` is `None`, so an attempt would error and loop the driver
/// back to waiting — a driver that skipped the demand check would never end
/// and this test's join-with-timeout would fail.
#[tokio::test(flavor = "multi_thread")]
async fn zeb903_reattempt_driver_exits_without_attempt_when_pending_cleared() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("harmony_app=warn")),
        )
        .with_test_writer()
        .try_init();

    harmony_app::iroh_endpoint::warm_up_iroh_global_init().await;

    tokio::time::timeout(Duration::from_secs(60), async {
        let s = setup_two_party_iroh_handshake().await;
        let (_invite_payload, invite_url, _token_sig) = zeb889_build_targeted_invite(&s);

        // No latch seed: Bob has NO Space row for the community at all.
        let (epoch_tx, epoch_rx) = tokio::sync::watch::channel(0u64);
        let ctx = harmony_app::latched_join_reattempt::ReattemptContext {
            invite_url,
            pkarr_resolver: Some(Arc::clone(&s.pkarr_resolver)),
            reachability_resolver: Some(s.bob_reachability.clone()),
            iroh_endpoint: None,
            crdt_state: Arc::clone(&s.bob_crdt_state),
            hlc_tracker: Arc::clone(&s.bob_hlc_tracker),
            adopt_floor: s.bob_adopt_floor.clone(),
            device_id: "bob-dev".to_string(),
            self_owner: s.bob_addr,
            community_signing_key: Arc::clone(&s.bob_comm_sk),
            enrollment_cert: s.bob_comm.cert.clone(),
            community_registry: Arc::clone(&s.registry_bob),
            community_adapter_tx: s.bob_adapter_tx.clone(),
            transport_epoch_rx: Some(epoch_rx),
            dm_outbox: Arc::clone(&s.bob_dm_outbox),
            channel_log_registry: Arc::clone(&s.bob_channel_log_registry),
            sync_engine: None,
            identity_dir: None,
            sink: None,
            dial_config: harmony_app::HandshakeDialConfig {
                connect_timeout: Duration::from_millis(2_000),
                open_bi_timeout: Duration::from_millis(2_000),
                response_read_timeout: Duration::from_millis(2_000),
                write_timeout: Duration::from_millis(2_000),
            },
        };
        let handle = harmony_app::latched_join_reattempt::spawn_reattempt_driver(ctx)
            .await
            .expect("driver must arm");

        // Bump-and-poll: the driver must end via the demand-collapsed branch.
        let mut ended = false;
        for _ in 0..100 {
            epoch_tx.send_modify(|e| *e = e.wrapping_add(1));
            if handle.is_finished() {
                ended = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            ended,
            "ZEB-903: with no pending Space the driver must exit on the first bump"
        );
        handle.await.expect("driver task must not panic");

        // Nothing was attempted or committed: still no Space row.
        assert!(
            !s.bob_crdt_state
                .lock()
                .await
                .spaces
                .contains_key(&s.community_id),
            "ZEB-903: the demand-collapsed driver must not commit any Space state"
        );

        let _ = s.registry_bob.shutdown_all().await;
        s.publisher_handle.abort();
        s.alice_ep.shutdown().await;
        s.bob_ep.shutdown().await;
    })
    .await
    .expect("zeb903_reattempt_driver_exits_without_attempt_when_pending_cleared timed out at 60s");
}

/// ZEB-903 T3: registry shutdown collapses the driver. The latched Space
/// stays pending (no attempt fires — the context's endpoint is `None` as in
/// T2, and the shutdown flip wins the select before any bump arrives).
#[tokio::test(flavor = "multi_thread")]
async fn zeb903_reattempt_driver_collapses_on_registry_shutdown() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("harmony_app=warn")),
        )
        .with_test_writer()
        .try_init();

    harmony_app::iroh_endpoint::warm_up_iroh_global_init().await;

    tokio::time::timeout(Duration::from_secs(60), async {
        let s = setup_two_party_iroh_handshake().await;
        let (invite_payload, invite_url, _token_sig) = zeb889_build_targeted_invite(&s);

        // Minimal latch seed (no Alice-side / cache seeding — no attempt
        // will run): mint P1 and commit the latched-pending Space.
        let join_hlc = Hlc {
            wall_ms: 100_600,
            logical: 0,
            device_id: "bob-dev".into(),
        };
        let p1_mint = harmony_app::mint_redemption(
            &invite_payload,
            s.bob_addr,
            s.bob_comm_sk.as_ref(),
            &s.bob_comm.cert,
            join_hlc,
        )
        .expect("mint P1 for bob");
        let latch_dto = harmony_app::redeem_invite_inner_with_overrides(
            invite_url.clone(),
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
            || Ok(()),
            None,
            harmony_app::RedeemInviteOverrides {
                pre_minted: Some(p1_mint),
                redeem_timeout: Some(Duration::from_secs(1)),
                ..Default::default()
            },
        )
        .await
        .expect("the latch seed must commit a pending Space, not Err");
        assert!(
            latch_dto.pending,
            "precondition: seeded latch must be pending"
        );

        let (_epoch_tx, epoch_rx) = tokio::sync::watch::channel(0u64);
        let ctx = harmony_app::latched_join_reattempt::ReattemptContext {
            invite_url,
            pkarr_resolver: Some(Arc::clone(&s.pkarr_resolver)),
            reachability_resolver: Some(s.bob_reachability.clone()),
            iroh_endpoint: None,
            crdt_state: Arc::clone(&s.bob_crdt_state),
            hlc_tracker: Arc::clone(&s.bob_hlc_tracker),
            adopt_floor: s.bob_adopt_floor.clone(),
            device_id: "bob-dev".to_string(),
            self_owner: s.bob_addr,
            community_signing_key: Arc::clone(&s.bob_comm_sk),
            enrollment_cert: s.bob_comm.cert.clone(),
            community_registry: Arc::clone(&s.registry_bob),
            community_adapter_tx: s.bob_adapter_tx.clone(),
            transport_epoch_rx: Some(epoch_rx),
            dm_outbox: Arc::clone(&s.bob_dm_outbox),
            channel_log_registry: Arc::clone(&s.bob_channel_log_registry),
            sync_engine: None,
            identity_dir: None,
            sink: None,
            dial_config: harmony_app::HandshakeDialConfig {
                connect_timeout: Duration::from_millis(2_000),
                open_bi_timeout: Duration::from_millis(2_000),
                response_read_timeout: Duration::from_millis(2_000),
                write_timeout: Duration::from_millis(2_000),
            },
        };
        let handle = harmony_app::latched_join_reattempt::spawn_reattempt_driver(ctx)
            .await
            .expect("driver must arm");

        // Registry teardown must flip the driver's shutdown watch. The flip
        // is sticky, so this wins even if the task has not polled yet.
        let _ = s.registry_bob.shutdown_all().await;
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("driver task must end on registry shutdown")
            .expect("driver task must not panic");

        // No attempt fired: the latched Space is untouched (still pending).
        {
            let g = s.bob_crdt_state.lock().await;
            let row = g
                .spaces
                .get(&s.community_id)
                .expect("latched Space row must survive the shutdown");
            assert!(
                row.pending_join_at.is_some(),
                "ZEB-903: shutdown must collapse the driver without an attempt"
            );
        }

        s.publisher_handle.abort();
        s.alice_ep.shutdown().await;
        s.bob_ep.shutdown().await;
    })
    .await
    .expect("zeb903_reattempt_driver_collapses_on_registry_shutdown timed out at 60s");
}

/// ZEB-903 (CodeAnt r1): a left community is NOT demand. Leaving retains the
/// Space row as a tombstone with `pending_join_at` intact, and the redeem
/// commit's ZEB-427 rejoin path clears `left_at` — so a driver that ignored
/// `left_at` would background-rejoin a community the user explicitly left.
/// Pin: with `left_at` set, an epoch bump makes the driver exit without
/// attempting (control as in T2: `iroh_endpoint: None` would make an attempt
/// observably loop the driver instead of ending it), and the row keeps BOTH
/// markers untouched.
#[tokio::test(flavor = "multi_thread")]
async fn zeb903_reattempt_driver_respects_left_community() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("harmony_app=warn")),
        )
        .with_test_writer()
        .try_init();

    harmony_app::iroh_endpoint::warm_up_iroh_global_init().await;

    tokio::time::timeout(Duration::from_secs(60), async {
        let s = setup_two_party_iroh_handshake().await;
        let (invite_payload, invite_url, _token_sig) = zeb889_build_targeted_invite(&s);

        // Minimal latch seed (as T3), then mark the community left the way
        // `leave_community` records it: `left_at` set, `pending_join_at`
        // NOT cleared (the tombstone shape `mark_community_space_left`
        // produces).
        let join_hlc = Hlc {
            wall_ms: 100_600,
            logical: 0,
            device_id: "bob-dev".into(),
        };
        let p1_mint = harmony_app::mint_redemption(
            &invite_payload,
            s.bob_addr,
            s.bob_comm_sk.as_ref(),
            &s.bob_comm.cert,
            join_hlc,
        )
        .expect("mint P1 for bob");
        let latch_dto = harmony_app::redeem_invite_inner_with_overrides(
            invite_url.clone(),
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
            || Ok(()),
            None,
            harmony_app::RedeemInviteOverrides {
                pre_minted: Some(p1_mint),
                redeem_timeout: Some(Duration::from_secs(1)),
                ..Default::default()
            },
        )
        .await
        .expect("the latch seed must commit a pending Space, not Err");
        assert!(
            latch_dto.pending,
            "precondition: seeded latch must be pending"
        );
        {
            let mut g = s.bob_crdt_state.lock().await;
            let row = g
                .spaces
                .get_mut(&s.community_id)
                .expect("latched Space row must exist");
            row.left_at = Some(Hlc {
                wall_ms: 100_800,
                logical: 0,
                device_id: "bob-dev".into(),
            });
            assert!(
                row.pending_join_at.is_some(),
                "precondition: the leave tombstone keeps pending_join_at set"
            );
        }

        let (epoch_tx, epoch_rx) = tokio::sync::watch::channel(0u64);
        let ctx = harmony_app::latched_join_reattempt::ReattemptContext {
            invite_url,
            pkarr_resolver: Some(Arc::clone(&s.pkarr_resolver)),
            reachability_resolver: Some(s.bob_reachability.clone()),
            iroh_endpoint: None,
            crdt_state: Arc::clone(&s.bob_crdt_state),
            hlc_tracker: Arc::clone(&s.bob_hlc_tracker),
            adopt_floor: s.bob_adopt_floor.clone(),
            device_id: "bob-dev".to_string(),
            self_owner: s.bob_addr,
            community_signing_key: Arc::clone(&s.bob_comm_sk),
            enrollment_cert: s.bob_comm.cert.clone(),
            community_registry: Arc::clone(&s.registry_bob),
            community_adapter_tx: s.bob_adapter_tx.clone(),
            transport_epoch_rx: Some(epoch_rx),
            dm_outbox: Arc::clone(&s.bob_dm_outbox),
            channel_log_registry: Arc::clone(&s.bob_channel_log_registry),
            sync_engine: None,
            identity_dir: None,
            sink: None,
            dial_config: harmony_app::HandshakeDialConfig {
                connect_timeout: Duration::from_millis(2_000),
                open_bi_timeout: Duration::from_millis(2_000),
                response_read_timeout: Duration::from_millis(2_000),
                write_timeout: Duration::from_millis(2_000),
            },
        };
        let handle = harmony_app::latched_join_reattempt::spawn_reattempt_driver(ctx)
            .await
            .expect("driver must arm");

        let mut ended = false;
        for _ in 0..100 {
            epoch_tx.send_modify(|e| *e = e.wrapping_add(1));
            if handle.is_finished() {
                ended = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            ended,
            "ZEB-903: with left_at set the driver must exit on the first bump"
        );
        handle.await.expect("driver task must not panic");

        // The tombstone is untouched: still left, still (inertly) pending.
        {
            let g = s.bob_crdt_state.lock().await;
            let row = g
                .spaces
                .get(&s.community_id)
                .expect("left Space row must survive");
            assert!(
                row.left_at.is_some(),
                "ZEB-903: the driver must never clear a leave marker"
            );
            assert!(row.pending_join_at.is_some());
        }

        let _ = s.registry_bob.shutdown_all().await;
        s.publisher_handle.abort();
        s.alice_ep.shutdown().await;
        s.bob_ep.shutdown().await;
    })
    .await
    .expect("zeb903_reattempt_driver_respects_left_community timed out at 60s");
}
