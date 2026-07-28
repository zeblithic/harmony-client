//! Two-node, no-LAN, cross-WAN OPEN-community first-contact round-trip.
//!
//! The OPEN analogue of `pkarr_iroh_redeem_full_integration.rs`'s invite-only
//! `bob_joins_alice_via_iroh_handshake_option_a`. Where that test redeems a
//! *targeted invite token*, here Bob holds ONLY the open community link's
//! `community_id` + `epoch_key` (no invite, no token, no LAN), resolves Alice's
//! enumerated DHT *rendezvous slot* through a mock pkarr relay, dials her on the
//! `HARMONY_HANDSHAKE_V1` ALPN, sends a capability-proven `OpenJoinRequest`, and
//! is admitted by Alice's `IrohInviteHandshakeAcceptor` (which dispatches the
//! `0x11` open-join packet → `verify_and_admit_open_join` → `insert_local_event`).
//!
//! ## Why this proves the feature (fails on `main`)
//!
//! On `main` the open-redeem path has NO cross-WAN dial: a remote joiner inserts
//! only their *local* self-Join and waits for same-LAN Zenoh. Alice — on the far
//! side of a WAN with no shared multicast — never learns about Bob. The
//! load-bearing assertion in `bob_open_joins_alice_cross_wan_via_rendezvous`
//! (Alice's engine materializes Bob as `MemberStatus::Joined`) can ONLY pass
//! once the rendezvous-resolve → dial → beacon-admit path exists. The harness is
//! loopback-iroh-only with relays disabled (the existing invite-only harness's
//! config), so there is no LAN/Zenoh shortcut that could make Alice learn about
//! Bob other than the open-join dial under test.
//!
//! ## How Alice's rendezvous slot is seeded into the mock pkarr (the crux)
//!
//! Alice is a relay advertiser. We drive the real
//! `CommunityRendezvousPublisher::refresh_slot` (built over the SAME live
//! `PkarrPublisher` the harness spawns) which registers a pkarr publish handle
//! keyed by `rendezvous_slot_key(epoch_key, slot, current_epoch_id(now))` with
//! Alice's `ReachabilityAnnouncePayload` as the routing blob. The spawned
//! publisher task then PUTs that record into the mock relay (immediate, since
//! `register` schedules `next_publish_at = now`). We poll
//! `pkarr.resolve(rendezvous_slot_verifying_key(epoch_key, slot, epoch))` until
//! the record is visible before driving Bob's dial — identical in spirit to the
//! invite-only test's `await_pkarr_record_visible`, but keyed on the rendezvous
//! slot vk instead of the invite-token-derived ephemeral key.
//!
//! Slot selection is by advertiser rank (`slot_for_advertiser`: sort the
//! advertiser set ascending by 16-byte address, position == slot). To force a
//! given slot we choose the advertiser set accordingly: `[alice]` → Alice ranks
//! 0 → slot 0; `[lower_dummy, alice]` (Alice the higher address) → Alice ranks 1
//! → slot 1 while slot 0 stays empty (the dummy never publishes).

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use harmony_app::community_channel_log_engine::{
    ChannelLogEngineConfig, ChannelLogRegistry, ChannelLogRegistryConfig,
};
use harmony_app::community_membership::{
    materialize, sign_event, ChannelId, ChannelKind, EventPayload, MemberStatus,
    MembershipEventKind, SignedMembershipEvent,
};
use harmony_app::community_rendezvous::{rendezvous_slot_verifying_key, RENDEZVOUS_SLOT_COUNT};
use harmony_app::community_rendezvous_publisher::CommunityRendezvousPublisher;
use harmony_app::community_state_sync::{
    CommunityRegistryConfig, CommunitySyncRegistry, IdentityResolver, DEFAULT_DEBOUNCE_MS,
};
use harmony_app::content_store::{CasOp, ContentStore, RuntimeContentStore};
use harmony_app::dm_outbox::DmOutbox;
use harmony_app::event_loop::CommunityAdapterRequest;
use harmony_app::iroh_endpoint::{alpn, IrohEndpoint};
use harmony_app::iroh_invite_acceptor::IrohInviteHandshakeAcceptor;
use harmony_app::open_join_dial::{connectivity_open_join_iroh_inner, OpenJoinDialCtx};
use harmony_app::owner_state_crdt::OwnerState;
use harmony_app::owner_state_types::{DeviceIdentityHash, EpochKey, Hlc, OwnerAddr, SpaceId};
use harmony_app::reachability_record::ReachabilityAnnouncePayload;
use harmony_app::reachability_resolver::ReachabilityResolver;
use harmony_app::zenoh_iroh_transport::IrohZenohLinkManager;
use harmony_identity::PrivateIdentity;
use iroh::endpoint::{presets, Endpoint, RelayMode};
use iroh::SecretKey;
use tokio::sync::{mpsc, Mutex as TokioMutex};
use zenoh_link::LinkUnicast;

// ────────────────────────────────────────────────────────────────────────────
// Identity resolver (mirrors the invite-only harness).
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
// Helpers (mirrored from pkarr_iroh_redeem_full_integration.rs).
// ────────────────────────────────────────────────────────────────────────────

fn signing_key_from(identity: &PrivateIdentity) -> SigningKey {
    let priv_bytes = identity.to_private_bytes();
    let mut secret = [0u8; 32];
    secret.copy_from_slice(&priv_bytes[32..64]);
    SigningKey::from_bytes(&secret)
}

fn dup_identity(src: &PrivateIdentity) -> PrivateIdentity {
    PrivateIdentity::from_private_bytes(&src.to_private_bytes())
        .expect("PrivateIdentity round-trip via to/from_private_bytes")
}

/// `(OwnerAddr, 64-byte composite identity_pub)` from an Ed25519 signing key,
/// via the same X25519-from-Ed25519 derivation the open-join dialer uses.
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

async fn build_hermetic_endpoint() -> Arc<IrohEndpoint> {
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

fn spawn_shared_cas() -> mpsc::Sender<CasOp> {
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
// Two-party OPEN harness.
// ────────────────────────────────────────────────────────────────────────────

/// Everything the open-join roundtrip tests need after the shared setup.
/// Fields prefixed `_` are keep-alive handles (spawned tasks, accept loops,
/// tempdirs, the mock relay) that must outlive the dial call.
struct OpenJoinSetup {
    // ── Identities / community ──────────────────────────────────────────
    bob_comm_sk: Arc<SigningKey>,
    /// Alice's enrolled community device key — admin signer for ChannelCreate
    /// in the channel-materialization test (`insert_alice_channel`).
    alice_comm_sk: Arc<SigningKey>,
    alice_addr: OwnerAddr,
    bob_addr: OwnerAddr,
    /// The exact 64-byte composite identity pub (X25519 ‖ Ed25519) handed to
    /// `CommunityRendezvousPublisher::new`, which writes it into every slot
    /// record's `harmony_identity_pub`. Exposed so a resolve-side test can pin
    /// the identity bytes it read against the ones the publisher signed with,
    /// rather than merely asserting they are non-zero (ZEB-824).
    alice_identity_pub: [u8; 64],
    epoch_key: EpochKey,
    community_id: SpaceId,
    bob_bootstrap_join: SignedMembershipEvent,

    // ── Endpoints (for teardown) ────────────────────────────────────────
    alice_ep: Arc<IrohEndpoint>,
    bob_ep: Arc<IrohEndpoint>,

    // ── Registries / engines ────────────────────────────────────────────
    registry_alice: Arc<CommunitySyncRegistry>,
    /// Bob's registry — needed to read Bob's materialized state (channels) in
    /// the channel-materialization assertion. Existing tests only query Alice.
    registry_bob: Arc<CommunitySyncRegistry>,
    bob_engine: Arc<harmony_app::community_state_sync::CommunitySyncEngine>,
    /// ZEB-573: Bob's REAL per-channel log registry (the adapter receiver is
    /// kept alive via `_bob_channel_log_adapter_drain` below, not dropped). The
    /// channel-LOG layer is the unexercised half — `list_channel_messages` looks
    /// the engine up here, and on the open-join admit path it is never spawned
    /// in-session (config materializes, log engine does not). Used by
    /// `bob_open_join_admit_leaves_channel_log_engine_unspawned`.
    bob_channel_log_registry: Arc<ChannelLogRegistry>,
    /// Per-device HLC tracker for Bob — the same lane the production redeem /
    /// boot reconcile threads into `register_channel_log_engine` /
    /// `reconcile_from_state`.
    bob_hlc_tracker: Arc<TokioMutex<harmony_crdt_sync::ReplayTracker<String, Hlc>>>,

    // ── pkarr ───────────────────────────────────────────────────────────
    pkarr_resolver: Arc<harmony_pkarr::PkarrResolver>,
    /// The relay client behind `pkarr_resolver`. Used to mint FRESH resolvers
    /// (independent caches over the same relay store) — the resolver carries a
    /// 60s negative cache, so a key that resolved absent during cold-start
    /// stays cached-absent for 60s even after a beacon publishes. The retry in
    /// the cold-start test resolves through a fresh resolver, modelling the
    /// production retry that fires minutes later (past the negative TTL).
    relay_client: Arc<harmony_pkarr::RelayClient>,
    alice_rendezvous_publisher: CommunityRendezvousPublisher,

    // ── keep-alive ──────────────────────────────────────────────────────
    _alice_accept: tokio::task::JoinHandle<()>,
    _bob_accept: tokio::task::JoinHandle<()>,
    _relay: harmony_pkarr::testing::MockPkarrRelay,
    publisher_handle: tokio::task::JoinHandle<()>,
    /// ZEB-573: drains Bob's channel-log adapter requests so the registry's
    /// `spawn` (via reconcile) does not block on a full/closed bridge — the
    /// production analogue is `event_loop::run` owning the adapter side. Kept
    /// alive so its receiver stays open for the registry's lifetime.
    _bob_channel_log_adapter_drain: tokio::task::JoinHandle<()>,
    _dir_alice: tempfile::TempDir,
    _dir_bob: tempfile::TempDir,
}

impl OpenJoinSetup {
    /// A fresh `PkarrResolver` over the same relay store — an independent cache,
    /// so it never reads a stale negative-cache entry left by a prior resolve.
    fn fresh_resolver(&self) -> Arc<harmony_pkarr::PkarrResolver> {
        Arc::new(harmony_pkarr::PkarrResolver::new(Arc::clone(
            &self.relay_client,
        )))
    }

    /// Drive Alice's rendezvous publisher for the given advertiser set
    /// (`me = alice_addr`), then poll the mock relay (through a FRESH resolver,
    /// immune to any prior negative-cache entry) until the resulting slot record
    /// is visible. Returns the slot index Alice claimed.
    async fn publish_rendezvous_slot(&self, advertisers: Vec<OwnerAddr>) -> u16 {
        let slot =
            harmony_app::community_rendezvous::slot_for_advertiser(&advertisers, &self.alice_addr)
                .expect("alice must claim a slot for the chosen advertiser set");
        self.alice_rendezvous_publisher
            .refresh_slot(
                self.community_id,
                self.epoch_key.clone(),
                advertisers,
                self.alice_addr,
            )
            .await;
        await_rendezvous_slot_visible(&self.fresh_resolver(), &self.epoch_key, slot).await;
        slot
    }

    /// Sign an admin (`alice_comm_sk`) `ChannelCreate` at a wall-clock AFTER
    /// Alice's bootstrap Join and insert it into Alice's engine, so her harvested
    /// admit-snapshot carries an admin-signed channel event whose `verify_event`
    /// depends on Alice's own enrollment Join being materialized first. On the
    /// pre-fix single-pass apply this dropped with `SignerNotEnrolledForActor`
    /// whenever the HashMap harvest ordered the ChannelCreate ahead of the Join
    /// (ZEB-573). Asserts Alice materializes it herself (snapshot precondition).
    async fn insert_alice_channel(&self, channel_id: ChannelId, name: &str) {
        let payload = EventPayload {
            id: {
                let mut id = [0u8; 16];
                id[0] = 0xCC;
                id[1..3].copy_from_slice(&channel_id.0[0..2]);
                id
            },
            community_id: self.community_id,
            kind: MembershipEventKind::ChannelCreate {
                channel_id,
                name: name.to_string(),
                write_power: 0,
                kind: ChannelKind::Text,
            },
            actor: self.alice_addr,
            at: Hlc {
                wall_ms: 100_500,
                logical: 0,
                device_id: "alice-dev".into(),
            },
        };
        let ev =
            sign_event(&payload, self.alice_comm_sk.as_ref()).expect("sign alice ChannelCreate");
        let alice_engine = self
            .registry_alice
            .engine_arc(&self.community_id)
            .await
            .expect("alice engine arc");
        alice_engine
            .insert_local_event(ev)
            .await
            .expect("alice ChannelCreate insert must Ok");

        // Precondition: Alice's own engine materializes the channel, so the
        // beacon's harvested snapshot will actually carry it for Bob.
        let alice_state = self
            .registry_alice
            .state_for(&self.community_id)
            .await
            .expect("alice state");
        let events: Vec<_> = {
            let g = alice_state.lock().await;
            g.events().cloned().collect()
        };
        assert!(
            materialize(&events, self.alice_addr)
                .channels
                .contains_key(&channel_id),
            "Alice's own engine must materialize the channel she just created (she is \
             enrolled via her bootstrap Join), else the admit-snapshot can't carry it"
        );
    }
}

/// Stand up the full two-party OPEN harness. Mirrors
/// `setup_two_party_iroh_handshake`, but Alice's community is OPEN
/// (`is_invite_only = false`), Bob holds a spawned engine for it (so the
/// admitted snapshot can be applied), and Alice owns a
/// `CommunityRendezvousPublisher` over the live `PkarrPublisher`.
async fn setup_two_party_open_join() -> OpenJoinSetup {
    // ── 1. Identities (dual-identity model, mirrors the invite-only harness). ─
    let alice_identity = PrivateIdentity::from_seed(&[0xa1; 32]);
    let bob_identity = PrivateIdentity::from_seed(&[0xb2; 32]);
    let alice_sk = Arc::new(signing_key_from(&alice_identity));
    let bob_sk = Arc::new(signing_key_from(&bob_identity));
    let (alice_transport_addr, alice_pub) = derive_composite_owner(&alice_sk);
    let (bob_transport_addr, _bob_pub) = derive_composite_owner(&bob_sk);

    let alice_comm = harmony_app::community_membership::mint_test_owner(0xA1);
    let bob_comm = harmony_app::community_membership::mint_test_owner(0xB2);
    let alice_comm_sk = Arc::new(SigningKey::from_bytes(&alice_comm.device_key.to_bytes()));
    let bob_comm_sk = Arc::new(SigningKey::from_bytes(&bob_comm.device_key.to_bytes()));
    let alice_addr = alice_comm.owner;
    let bob_addr = bob_comm.owner;

    let resolver: Arc<dyn IdentityResolver> = Arc::new(TwoIdentityResolver {
        alice: (alice_addr, [0u8; 64]),
        bob: (bob_addr, [0u8; 64]),
    });
    let _ = (alice_transport_addr, bob_transport_addr);

    // ── 2. Iroh endpoints + link managers. ──────────────────────────────
    let alice_ep = build_hermetic_endpoint().await;
    let bob_ep = build_hermetic_endpoint().await;
    let alice_bound = alice_ep.bound_sockets();
    assert!(
        !alice_bound.is_empty(),
        "alice's hermetic endpoint must expose bound_sockets() so bob's dialer has a loopback target"
    );

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

    // ── 3. Alice's OPEN community + engine. ──────────────────────────────
    let alice_minted = harmony_app::mint_community_creation(
        "OpenJoinCommunity",
        false, // OPEN community — the whole point of this test.
        alice_addr,
        alice_comm_sk.as_ref(),
        &alice_comm.cert,
        Hlc {
            wall_ms: 100_000,
            logical: 0,
            device_id: "alice-dev".to_string(),
        },
    )
    .expect("alice mint OPEN community");
    let community_id = alice_minted.community_id;
    let epoch_key = alice_minted.membership_key.clone();

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
        signing_key: Arc::clone(&alice_comm_sk),
        crdt_state: None,
        nav_emitter: None,
        presence_resync_rx: None,
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
        signing_key: Arc::clone(&bob_comm_sk),
        crdt_state: None,
        nav_emitter: None,
        presence_resync_rx: None,
    }));

    // Spawn Alice's engine + insert her bootstrap Join.
    let (alice_pub_tx, _alice_pub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (_alice_sub_tx, alice_sub_rx) = mpsc::channel::<Vec<u8>>(64);
    registry_alice
        .spawn_engine_inner_now(
            community_id,
            epoch_key.clone(),
            alice_addr,
            false, // OPEN
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

    // Spawn Bob's engine for Alice's OPEN community so the admitted snapshot has
    // somewhere to land. Bob starts with an empty event log; the dial applies
    // Alice's bootstrap + Bob's Join from the Admitted response.
    let (bob_pub_tx, _bob_pub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (_bob_sub_tx, bob_sub_rx) = mpsc::channel::<Vec<u8>>(64);
    registry_bob
        .spawn_engine_inner_now(
            community_id,
            epoch_key.clone(),
            alice_addr,
            false, // OPEN
            bob_pub_tx,
            bob_sub_rx,
            harmony_app::community_state_sync::CatchUpChannels::none(),
        )
        .await
        .expect("spawn bob engine");
    let bob_engine = registry_bob
        .engine_arc(&community_id)
        .await
        .expect("bob engine arc");

    // ── 4. Alice's dm_outbox + crdt_state (acceptor dependencies). ──────
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

    // ── 5. Mock pkarr relay + publisher. ────────────────────────────────
    let relay = harmony_pkarr::testing::MockPkarrRelay::start().await;
    let pool = harmony_pkarr::RelayPool::new(vec![relay.base_url.clone()]);
    let client = Arc::new(harmony_pkarr::RelayClient::new(pool));
    let pkarr_publisher = Arc::new(harmony_pkarr::PkarrPublisher::new(Arc::clone(&client)));
    let publisher_handle = Arc::clone(&pkarr_publisher).spawn();
    let pkarr_resolver = Arc::new(harmony_pkarr::PkarrResolver::new(Arc::clone(&client)));

    // Alice's reachability routing record — the slot record's routing blob.
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
    // The rendezvous publisher reuses the SAME (publisher, identity_signing_key,
    // identity_pub, routing_blob_builder) inputs the member-keyed community
    // publisher takes at boot — the slot record's OUTER envelope is signed by an
    // ephemeral key derived from `epoch_key` (so any holder of the link can
    // resolve it), while `id_sk`/`id_pub` bind the inner identity.
    let alice_rendezvous_publisher = CommunityRendezvousPublisher::new(
        Arc::clone(&pkarr_publisher),
        (*alice_sk).clone(),
        alice_pub,
        Arc::new(move || alice_routing_blob.clone()),
    );

    // ── 6. Alice's handshake acceptor (handles BOTH 0x10 invite and 0x11
    //       open-join via its inbound dispatcher). ────────────────────────
    let alice_acceptor: Arc<IrohInviteHandshakeAcceptor<()>> =
        Arc::new(IrohInviteHandshakeAcceptor::<()>::with_config(
            Arc::clone(&registry_alice),
            Arc::clone(&alice_dm_outbox),
            Arc::clone(&alice_crdt_state),
            None,
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

    // Drain Bob's adapter dispatch (engine spawn produces one request).
    let (_bob_adapter_tx, mut bob_adapter_rx) = mpsc::channel::<CommunityAdapterRequest>(8);
    tokio::spawn(async move {
        while let Some(req) = bob_adapter_rx.recv().await {
            drop(req.publisher_rx);
            drop(req.subscriber_tx);
        }
    });

    // ── 7. Bob's self-signed OPEN bootstrap Join. ───────────────────────
    // OPEN community → no invite token, no countersign. The Join is signed by
    // Bob's enrolled community device key (`bob_comm_sk`) and carries Bob's
    // EnrollmentCert so the beacon's `enrolled_key_from_cert` resolves the
    // signer. CRITICAL: the `signing_key` threaded into the dial ctx is the
    // SAME `bob_comm_sk`, so the open-join packet envelope signature verifies
    // against the cert's enrolled device key (Task 5 device-hash/identity gate).
    let bob_bootstrap_join = {
        let payload = EventPayload {
            id: {
                let mut id = [0u8; 16];
                id[0] = 0xB0;
                id
            },
            community_id,
            kind: MembershipEventKind::Join,
            actor: bob_addr,
            at: Hlc {
                wall_ms: 100_200,
                logical: 0,
                device_id: "bob-dev".into(),
            },
        };
        let mut ev = sign_event(&payload, bob_comm_sk.as_ref()).expect("sign bob open join");
        ev.enrollment = Some(bob_comm.cert.clone());
        ev
    };

    // ZEB-573: build Bob's REAL channel-log registry and KEEP its adapter
    // receiver alive (production's `event_loop::run` owns the adapter side).
    // Earlier this harness dropped the receiver (`_bob_channel_log_adapter_rx`)
    // and never asserted on the registry, so the channel-LOG layer — the half
    // `list_channel_messages` reads — was completely unexercised, which is how
    // ZEB-573 (config materializes, log engine never spawns in-session) slipped
    // through. We drain requests in a spawned task so the registry's `spawn`
    // never blocks on the bridge.
    let (bob_channel_log_adapter_tx, mut bob_channel_log_adapter_rx) =
        mpsc::unbounded_channel::<harmony_app::event_loop::ChannelLogAdapterRequest>();
    let bob_channel_log_adapter_drain = tokio::spawn(async move {
        while bob_channel_log_adapter_rx.recv().await.is_some() {
            // Drop each request (we never assert on the per-channel adapter
            // wiring here — only that the registry holds a spawned engine).
        }
    });
    let bob_channel_log_registry: Arc<ChannelLogRegistry> =
        ChannelLogRegistry::new(ChannelLogRegistryConfig {
            adapter_request_tx: bob_channel_log_adapter_tx,
            sink: Arc::new(harmony_app::node_event_sink::FanoutSink(vec![])),
            identity_dir: dir_bob.path().to_path_buf(),
            self_owner: bob_addr,
            self_device_id: "bob-dev".into(),
            signing_key: Arc::clone(&bob_comm_sk),
            engine_config: ChannelLogEngineConfig::default(),
            transport_epoch_rx: None,
            // ZEB-599 Direction 1: no presence watch in this integration harness.
            presence_resync_rx: None,
        });
    let bob_hlc_tracker: Arc<TokioMutex<harmony_crdt_sync::ReplayTracker<String, Hlc>>> = Arc::new(
        TokioMutex::new(harmony_crdt_sync::ReplayTracker::new("bob-dev".into())),
    );
    OpenJoinSetup {
        bob_comm_sk,
        alice_comm_sk,
        alice_addr,
        bob_addr,
        alice_identity_pub: alice_pub,
        epoch_key,
        community_id,
        bob_bootstrap_join,
        alice_ep,
        bob_ep,
        registry_alice,
        registry_bob,
        bob_engine,
        bob_channel_log_registry,
        bob_hlc_tracker,
        pkarr_resolver,
        relay_client: client,
        alice_rendezvous_publisher,
        _alice_accept: alice_accept,
        _bob_accept: bob_accept,
        _relay: relay,
        publisher_handle,
        _bob_channel_log_adapter_drain: bob_channel_log_adapter_drain,
        _dir_alice: dir_alice,
        _dir_bob: dir_bob,
    }
}

/// Poll (≤5s) until Alice's rendezvous slot record for `slot` is visible in the
/// mock relay under `rendezvous_slot_verifying_key(epoch_key, slot, epoch)` for
/// the joiner's current wall-clock epoch. Mirrors `await_pkarr_record_visible`
/// in the invite-only test but keyed on the rendezvous slot vk.
async fn await_rendezvous_slot_visible(
    pkarr_resolver: &harmony_pkarr::PkarrResolver,
    epoch_key: &EpochKey,
    slot: u16,
) {
    let now_ms = wall_ms();
    let epoch_id = harmony_pkarr::current_epoch_id(now_ms);
    let vk = rendezvous_slot_verifying_key(epoch_key, slot, epoch_id);
    let mut visible = false;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Ok(Some(_)) = pkarr_resolver.resolve(&vk).await {
            visible = true;
            break;
        }
    }
    assert!(
        visible,
        "alice's rendezvous slot-{slot} record must appear in the mock relay within 5s \
         before driving Bob's open-join dial"
    );
}

fn wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time")
        .as_millis() as u64
}

/// Build Bob's open-join dial context for `setup`. `signing_key` is
/// `bob_comm_sk` — the SAME key that signed `bob_bootstrap_join` — so the
/// beacon's enrolled-device-key envelope-signature check passes.
fn bob_dial_ctx(setup: &OpenJoinSetup) -> OpenJoinDialCtx {
    OpenJoinDialCtx {
        community_id: setup.community_id,
        epoch_key: setup.epoch_key.clone(),
        bootstrap_join: setup.bob_bootstrap_join.clone(),
        signing_key: Arc::clone(&setup.bob_comm_sk),
        iroh_endpoint: Some(Arc::clone(&setup.bob_ep)),
        engine: Some(Arc::clone(&setup.bob_engine)),
        dial_config: harmony_app::HandshakeDialConfig {
            connect_timeout: Duration::from_millis(5_000),
            open_bi_timeout: Duration::from_millis(5_000),
            response_read_timeout: Duration::from_millis(5_000),
            write_timeout: Duration::from_millis(5_000),
        },
    }
}

/// Poll Alice's engine until it materializes Bob as `MemberStatus::Joined`.
/// This is the load-bearing cross-WAN assertion: on `main` the open path never
/// dials Alice, so she never learns about Bob and this can only time out.
async fn assert_alice_materializes_bob_joined(setup: &OpenJoinSetup) {
    let alice_state = setup
        .registry_alice
        .state_for(&setup.community_id)
        .await
        .expect("alice state must exist");
    for _ in 0..50 {
        let events: Vec<_> = {
            let g = alice_state.lock().await;
            g.events().cloned().collect()
        };
        let mat = materialize(&events, setup.alice_addr);
        if mat.members.get(&setup.bob_addr).map(|m| m.status) == Some(MemberStatus::Joined) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let events: Vec<_> = {
        let g = alice_state.lock().await;
        g.events().cloned().collect()
    };
    let mat = materialize(&events, setup.alice_addr);
    panic!(
        "Alice's engine never materialized Bob as Joined cross-WAN — the open dial→admit \
         path did not deliver Bob's Join to Alice. Bob status = {:?}; alice has {} events. \
         (On main this is expected to FAIL: no open dial path exists, so Alice never learns \
         about Bob cross-WAN.)",
        mat.members.get(&setup.bob_addr).map(|m| m.status),
        events.len(),
    );
}

/// Poll Bob's engine until it materializes the admin's channel `channel_id` from
/// the admitted snapshot. This is the ZEB-573 load-bearing assertion: the
/// admin-signed `ChannelCreate` (verify depends on Alice's enrollment Join) must
/// survive Bob's dependency-ordered apply rather than being dropped with
/// `SignerNotEnrolledForActor`. On the pre-fix single-pass apply this could time
/// out whenever the harvest ordered the ChannelCreate ahead of Alice's Join.
async fn assert_bob_materializes_channel(setup: &OpenJoinSetup, channel_id: ChannelId) {
    let bob_state = setup
        .registry_bob
        .state_for(&setup.community_id)
        .await
        .expect("bob state must exist");
    for _ in 0..50 {
        let events: Vec<_> = {
            let g = bob_state.lock().await;
            g.events().cloned().collect()
        };
        if materialize(&events, setup.alice_addr)
            .channels
            .contains_key(&channel_id)
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let events: Vec<_> = {
        let g = bob_state.lock().await;
        g.events().cloned().collect()
    };
    let mat = materialize(&events, setup.alice_addr);
    panic!(
        "Bob never materialized Alice's channel {channel_id:?} from the admitted snapshot — \
         the admin-signed ChannelCreate was dropped instead of dependency-ordered-applied \
         (ZEB-573 symptom: SignerNotEnrolledForActor on out-of-order apply). Bob has {} \
         events and {} channels: {:?}",
        events.len(),
        mat.channels.len(),
        mat.channels.keys().collect::<Vec<_>>(),
    );
}

// ────────────────────────────────────────────────────────────────────────────
// Test 1: the must-fail-on-main proof.
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bob_open_joins_alice_cross_wan_via_rendezvous() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("harmony_app=warn")),
        )
        .with_test_writer()
        .try_init();

    // Pre-pay iroh's ~30s first-bind global init OUTSIDE the budget (ZEB-347).
    harmony_app::iroh_endpoint::warm_up_iroh_global_init().await;

    tokio::time::timeout(Duration::from_secs(60), async {
        let s = setup_two_party_open_join().await;

        // Alice is the sole advertiser → ranks 0 → publishes rendezvous slot 0.
        let slot = s.publish_rendezvous_slot(vec![s.alice_addr]).await;
        assert_eq!(slot, 0, "single advertiser ranks 0 → slot 0");

        // Bob holds ONLY community_id + epoch_key (via the ctx). `now_ms` is the
        // joiner's real wall clock so `epoch_auth`'s timestamp is fresh against
        // Alice's freshness window and the resolve epoch matches the publish.
        let now_ms = wall_ms();
        let outcome = connectivity_open_join_iroh_inner(
            Arc::clone(&s.pkarr_resolver),
            bob_dial_ctx(&s),
            now_ms,
        )
        .await
        .expect("open join inner must Ok (transport failures map to outcome.status)");

        assert_eq!(
            outcome.status, "joined",
            "open-join happy path must return 'joined' (resolved slot 0 → dialed Alice on \
             HARMONY_HANDSHAKE_V1 → admitted via 0x11 open-join). Got status={:?} community_id={:?}",
            outcome.status, outcome.community_id
        );
        assert_eq!(
            outcome.community_id.as_deref(),
            Some(hex::encode(s.community_id.0).as_str()),
            "community_id must echo Alice's open community"
        );

        // ── THE load-bearing cross-WAN assertion (fails on main). ───────────
        // On main, the open-redeem path inserts only Bob's LOCAL self-Join and
        // waits for same-LAN Zenoh; there is no cross-WAN dial, so Alice never
        // learns about Bob. This passes ONLY because the new dial→admit path
        // delivered Bob's Join to Alice's engine over the loopback iroh stream.
        assert_alice_materializes_bob_joined(&s).await;

        // Teardown: abort the publisher first so it stops using the endpoints.
        s.publisher_handle.abort();
        s.alice_ep.shutdown().await;
        s.bob_ep.shutdown().await;
    })
    .await
    .expect("bob_open_joins_alice_cross_wan_via_rendezvous timed out at 60s");
}

// ────────────────────────────────────────────────────────────────────────────
// Test 2: slot-1 failover when slot-0 beacon is offline.
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn open_join_fails_over_to_slot1_when_slot0_beacon_offline() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("harmony_app=warn")),
        )
        .with_test_writer()
        .try_init();

    harmony_app::iroh_endpoint::warm_up_iroh_global_init().await;

    tokio::time::timeout(Duration::from_secs(60), async {
        let s = setup_two_party_open_join().await;

        // Force Alice onto slot 1 by giving slot 0 to a lower-address dummy
        // advertiser that never publishes. `slot_for_advertiser` sorts ascending
        // by address: the all-zero dummy ranks 0 (and stays silent), Alice ranks
        // 1. So slot 0 is DEAD and only slot 1 is live — the escalating-batch
        // resolve must widen past the dead slot 0 to find Alice.
        let dummy_lower = OwnerAddr([0u8; 16]);
        assert!(
            dummy_lower.0 < s.alice_addr.0,
            "dummy advertiser must sort below alice so alice claims slot 1"
        );
        let slot = s
            .publish_rendezvous_slot(vec![dummy_lower, s.alice_addr])
            .await;
        assert_eq!(slot, 1, "alice (higher address) ranks 1 → slot 1");

        // Confirm slot 0 really is empty in the relay (sanity: the failover is
        // genuinely exercised, not masked by an accidental slot-0 publish).
        let epoch_id = harmony_pkarr::current_epoch_id(wall_ms());
        let slot0_vk = rendezvous_slot_verifying_key(&s.epoch_key, 0, epoch_id);
        assert!(
            matches!(s.pkarr_resolver.resolve(&slot0_vk).await, Ok(None)),
            "slot 0 must be empty so the resolve genuinely widens to slot 1"
        );

        let now_ms = wall_ms();
        let outcome = connectivity_open_join_iroh_inner(
            Arc::clone(&s.pkarr_resolver),
            bob_dial_ctx(&s),
            now_ms,
        )
        .await
        .expect("open join inner must Ok");

        assert_eq!(
            outcome.status, "joined",
            "Bob must still join via slot-1 failover (escalating resolve widens past the \
             dead slot 0). Got status={:?}",
            outcome.status
        );
        assert_alice_materializes_bob_joined(&s).await;

        s.publisher_handle.abort();
        s.alice_ep.shutdown().await;
        s.bob_ep.shutdown().await;
    })
    .await
    .expect("open_join_fails_over_to_slot1_when_slot0_beacon_offline timed out at 60s");
}

// ────────────────────────────────────────────────────────────────────────────
// Test 3: cold-start is retryable (non-error), then succeeds once a beacon
// publishes.
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn open_join_cold_start_is_retryable_then_succeeds() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("harmony_app=warn")),
        )
        .with_test_writer()
        .try_init();

    harmony_app::iroh_endpoint::warm_up_iroh_global_init().await;

    tokio::time::timeout(Duration::from_secs(60), async {
        let s = setup_two_party_open_join().await;

        // ── Cold start: NO slot published → no beacon resolves. ─────────────
        // This must be a RETRYABLE non-error outcome, not an Err and not a join.
        let now_ms = wall_ms();
        let cold = connectivity_open_join_iroh_inner(
            Arc::clone(&s.pkarr_resolver),
            bob_dial_ctx(&s),
            now_ms,
        )
        .await
        .expect("cold-start open join must Ok (no_beacon_reachable is a non-error)");
        assert_eq!(
            cold.status, "no_beacon_reachable",
            "with no beacon published, the open join must return the retryable \
             'no_beacon_reachable' status (NOT an Err, NOT 'joined'). Got {:?}",
            cold.status
        );

        // Alice must NOT know about Bob yet (nothing was dialed).
        {
            let alice_state = s
                .registry_alice
                .state_for(&s.community_id)
                .await
                .expect("alice state");
            let events: Vec<_> = {
                let g = alice_state.lock().await;
                g.events().cloned().collect()
            };
            let mat = materialize(&events, s.alice_addr);
            assert!(
                !mat.members.contains_key(&s.bob_addr),
                "Alice must not know Bob during cold start (no dial happened)"
            );
        }

        // ── Now Alice publishes slot 0 and Bob re-dials → joins. ────────────
        let slot = s.publish_rendezvous_slot(vec![s.alice_addr]).await;
        assert_eq!(slot, 0);

        // The retry resolves through a FRESH resolver. Bob's cold-start dial
        // above negatively cached the (then-absent) slot-0 key for 60s; in
        // production the retry fires minutes later (transport-epoch re-arm),
        // well past that TTL, so a fresh-cache resolver faithfully models the
        // expired-negative-cache state rather than wedging the test on the TTL.
        let now_ms = wall_ms();
        let warm = connectivity_open_join_iroh_inner(s.fresh_resolver(), bob_dial_ctx(&s), now_ms)
            .await
            .expect("retry open join must Ok");
        assert_eq!(
            warm.status, "joined",
            "after a beacon publishes, the retried open join must succeed. Got {:?}",
            warm.status
        );
        assert_alice_materializes_bob_joined(&s).await;

        s.publisher_handle.abort();
        s.alice_ep.shutdown().await;
        s.bob_ep.shutdown().await;
    })
    .await
    .expect("open_join_cold_start_is_retryable_then_succeeds timed out at 60s");
}

// ────────────────────────────────────────────────────────────────────────────
// Test 4: the joiner materializes the admin's channel from the admitted
// snapshot (ZEB-573 regression guard). The existing tests only assert the
// membership direction (Alice learns Bob joined); this asserts the OTHER
// direction — Bob applies Alice's admin-signed ChannelCreate, whose verify
// depends on Alice's enrollment Join being applied first. The pre-fix
// single-pass apply dropped it with SignerNotEnrolledForActor whenever the
// HashMap harvest ordered ChannelCreate ahead of the Join; the merged fix
// (dependency-ordered apply: pre-sort by event_sort_key + retry-until-stable)
// lands it. Closes the "channels parity-only (not asserted)" gap that let
// ZEB-573 through.
//
// SCOPE — end-to-end guard, NOT a deterministic ordering guard: the harness
// can't force the beacon's `g.events.values()` HashMap harvest order, so on a
// REVERT of the apply-order fix this catches the regression only
// probabilistically (when the harvest happens to put ChannelCreate ahead of
// Alice's Join). The DETERMINISTIC worst-case ordering is pinned separately by
// the unit test `admitted_snapshot_applies_enrollment_dependent_events_out_of_order`
// (open_join_dial.rs), which constructs HLC-inverted events directly. The two
// are complementary: that unit test proves the apply logic deterministically;
// this proves the full open-join cold path delivers channels end-to-end. On
// shipped (sorted-apply) code this test is non-flaky — the pre-sort makes the
// apply order deterministic regardless of harvest order.
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bob_materializes_alice_channel_via_open_join() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("harmony_app=warn")),
        )
        .with_test_writer()
        .try_init();

    harmony_app::iroh_endpoint::warm_up_iroh_global_init().await;

    tokio::time::timeout(Duration::from_secs(60), async {
        let s = setup_two_party_open_join().await;

        // Alice (admin) creates #general BEFORE Bob dials, so her harvested
        // admit-snapshot carries an admin-signed ChannelCreate.
        let channel_id = ChannelId([0xC1; 16]);
        s.insert_alice_channel(channel_id, "general").await;

        // Alice is sole advertiser → slot 0.
        let slot = s.publish_rendezvous_slot(vec![s.alice_addr]).await;
        assert_eq!(slot, 0, "single advertiser ranks 0 → slot 0");

        let now_ms = wall_ms();
        let outcome = connectivity_open_join_iroh_inner(
            Arc::clone(&s.pkarr_resolver),
            bob_dial_ctx(&s),
            now_ms,
        )
        .await
        .expect("open join inner must Ok");
        assert_eq!(
            outcome.status, "joined",
            "open-join happy path must join. Got status={:?}",
            outcome.status
        );

        // Membership converges (existing guarantee) AND Bob materializes Alice's
        // channel from the admitted snapshot (the ZEB-573 fix).
        assert_alice_materializes_bob_joined(&s).await;
        assert_bob_materializes_channel(&s, channel_id).await;

        s.publisher_handle.abort();
        s.alice_ep.shutdown().await;
        s.bob_ep.shutdown().await;
    })
    .await
    .expect("bob_materializes_alice_channel_via_open_join timed out at 60s");
}

// ────────────────────────────────────────────────────────────────────────────
// ZEB-573: the channel-LOG layer (the half `list_channel_messages` reads).
//
// The test above (`bob_materializes_alice_channel_via_open_join`) asserts only
// the CONFIG layer: `materialize(events).channels.contains_key(channel_id)`.
// #347 fixed that (dependency-ordered apply). But a URL-only OPEN joiner ALSO
// needs the per-channel LOG engine spawned in-session — without it
// `list_channel_messages` returns "no engine for <community>/<channel>" until
// the next app restart's boot reconcile.
//
// This test drives the FULL production open-join redeem
// (`redeem_invite_inner_with_overrides` with `open_join_iroh` set) — the exact
// path the IPC `connectivity_open_join_iroh` uses — against Bob's REAL
// channel-log registry, and asserts the admitted channel's LOG engine is present
// when redeem returns. On a branch WITHOUT the in-session reconcile, the
// admitted `ChannelCreate` materializes config but the log engine is only
// spawned via the racy delta-consumer path (not synchronized with
// `channel_log_tx.commit()`), so it is RELIABLY absent in-session → this fails.
// The fix (an in-session `reconcile_from_state` after the Space + channel-log
// commits) spawns it deterministically → this passes.
// ────────────────────────────────────────────────────────────────────────────

/// Build a token-less OPEN invite URL for Alice's community carrying her current
/// materialized state (members + channels). Mirrors `generate_invite_impl`'s
/// open arm: `sealed_epoch_key` is the raw 32-byte `EpochKey`, `is_invite_only`
/// is false, and `state_snapshot` is frozen from Alice's engine.
async fn build_open_invite_url_for_alice(s: &OpenJoinSetup) -> String {
    use harmony_app::community_invite::{
        CommunityInvitePayload, InviteEpochSnapshot, MaterializedCommunityState,
    };

    let alice_state = s
        .registry_alice
        .state_for(&s.community_id)
        .await
        .expect("alice state");
    let events: Vec<_> = {
        let g = alice_state.lock().await;
        g.events().cloned().collect()
    };
    let mat = materialize(&events, s.alice_addr);

    let payload = CommunityInvitePayload {
        inviter_signer_certs: Vec::new(),
        community_id: s.community_id,
        epoch_snapshot: InviteEpochSnapshot {
            epoch: 0,
            // OPEN: raw 32-byte epoch key (the link capability).
            sealed_epoch_key: s.epoch_key.as_bytes().to_vec(),
            sealed_epoch_keys: Vec::new(),
            state_snapshot: MaterializedCommunityState {
                members: mat.members.clone(),
                channels: mat.channels.clone(),
                power_levels: mat.power_levels.clone(),
            },
        },
        admin_addr: s.alice_addr,
        community_name: "OpenJoinCommunity".to_string(),
        is_invite_only: false,
        expires_at: None,
        invite_token: None,
        admin_bootstrap: None,
        admin_identity_pub: None,
        forked_from: None,
        pre_fork_snapshot: None,
        inviter_enrollment: None,
        untargeted_decrypt_key: None,
    };
    harmony_app::build_open_invite_url(&payload).expect("encode open invite url")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bob_open_join_redeem_spawns_channel_log_engine_in_session() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("harmony_app=warn")),
        )
        .with_test_writer()
        .try_init();

    harmony_app::iroh_endpoint::warm_up_iroh_global_init().await;

    tokio::time::timeout(Duration::from_secs(60), async {
        let s = setup_two_party_open_join().await;

        // Alice (admin) creates #general BEFORE Bob joins, so her harvested
        // admit-snapshot AND her invite-URL state_snapshot both carry the
        // admin-signed ChannelCreate.
        let channel_id = ChannelId([0xC1; 16]);
        s.insert_alice_channel(channel_id, "general").await;

        // Alice is sole advertiser → slot 0 (Bob's open-join dial resolves it).
        let slot = s.publish_rendezvous_slot(vec![s.alice_addr]).await;
        assert_eq!(slot, 0, "single advertiser ranks 0 → slot 0");

        // Bob's redeem-side handles. The redeem path spawns its OWN community
        // engine via `spawn_engine_with_guard` into `registry_bob` (idempotent
        // with the harness pre-spawn), inserts Bob's self-Join, dials Alice's
        // beacon, applies the admitted snapshot, commits the Space + channel-log
        // transaction, and (with the fix) reconciles the channel-log engines.
        let bob_crdt_state = Arc::new(TokioMutex::new(OwnerState::default()));
        let (bob_adapter_tx, mut bob_adapter_rx) = mpsc::channel::<CommunityAdapterRequest>(8);
        let _bob_adapter_drain = tokio::spawn(async move {
            while let Some(req) = bob_adapter_rx.recv().await {
                drop(req.publisher_rx);
                drop(req.subscriber_tx);
            }
        });
        // dm_outbox: redeem reads `signing_key` for joiner-identity derivation and
        // resolves destinations from the owner cache (empty here → open path needs
        // no Reticulum fan-out). Bob's community signer = `bob_comm_sk`.
        let bob_identity = PrivateIdentity::from_seed(&[0xb2; 32]);
        let bob_dm_outbox = Arc::new(TokioMutex::new(DmOutbox::new(
            "bob-dev".into(),
            s.bob_addr,
            DeviceIdentityHash(bob_identity.identity.address_hash),
            Arc::clone(&s.bob_comm_sk),
            Arc::new(dup_identity(&bob_identity)),
            Arc::clone(&s.bob_comm_sk),
            harmony_app::community_membership::mint_test_owner(0xB2).cert,
        )));

        let invite_url = build_open_invite_url_for_alice(&s).await;

        let dto = harmony_app::redeem_invite_inner_with_overrides(
            invite_url,
            Arc::clone(&bob_crdt_state),
            Arc::clone(&s.bob_hlc_tracker),
            "bob-dev".to_string(),
            s.bob_addr,
            Arc::clone(&s.bob_comm_sk),
            harmony_app::community_membership::mint_test_owner(0xB2).cert,
            Arc::clone(&s.registry_bob),
            bob_adapter_tx,
            None, // no transport-epoch watch
            Arc::clone(&bob_dm_outbox),
            Arc::clone(&s.bob_channel_log_registry),
            || Ok(()), // no generation fence in this harness
            None,      // identity_dir: no pre_fork_snapshot write
            harmony_app::RedeemInviteOverrides {
                open_join_iroh: Some(harmony_app::OpenJoinIrohDeps {
                    pkarr_resolver: Arc::clone(&s.pkarr_resolver),
                    iroh_endpoint: Some(Arc::clone(&s.bob_ep)),
                    dial_config: harmony_app::HandshakeDialConfig {
                        connect_timeout: Duration::from_millis(10_000),
                        open_bi_timeout: Duration::from_millis(10_000),
                        response_read_timeout: Duration::from_millis(10_000),
                        write_timeout: Duration::from_millis(10_000),
                    },
                }),
                ..Default::default()
            },
        )
        .await
        .expect("open-join redeem must Ok");

        assert_eq!(
            dto.status.as_deref(),
            Some("joined"),
            "open-join redeem must report 'joined' (beacon admitted Bob). Got {:?}",
            dto.status
        );

        // CONFIG layer (the #347 guarantee) — Bob materialized Alice's channel.
        assert_bob_materializes_channel(&s, channel_id).await;

        // LOG layer (the ZEB-573 fix) — the per-channel log engine the admitted
        // ChannelCreate implies must be spawned IN-SESSION. Pre-fix this is None
        // (the admit materialized config but never spawned the log engine; that
        // only happens at the next boot reconcile). `list_channel_messages` looks
        // the engine up here, so a None means "no engine for <community>/<channel>".
        let log_engine = s
            .bob_channel_log_registry
            .engine(&s.community_id, &channel_id)
            .await;
        assert!(
            log_engine.is_some(),
            "ZEB-573: Bob's open-join redeem materialized the channel CONFIG but did NOT \
             spawn the per-channel LOG engine in-session — list_channel_messages would \
             return 'no engine for {}/{}'. The admitted ChannelCreate must trigger an \
             in-session reconcile (mirroring start_node) so the log engine spawns before \
             redeem returns, not only at the next app restart.",
            hex::encode(s.community_id.0),
            hex::encode(channel_id.0),
        );

        s.publisher_handle.abort();
        s.alice_ep.shutdown().await;
        s.bob_ep.shutdown().await;
    })
    .await
    .expect("bob_open_join_redeem_spawns_channel_log_engine_in_session timed out at 60s");
}

// ────────────────────────────────────────────────────────────────────────────
// ZEB-824: the identity-preserving, self-filtering rendezvous resolve.
//
// A member-side gateway dial (unlike an open-join joiner) must know WHO the
// beacon is before trusting it, and must never dial itself. Both properties are
// resolve-layer concerns, so they are pinned here against the same live
// mock-relay harness the open-join tests use.
// ────────────────────────────────────────────────────────────────────────────

/// ZEB-824: the identified resolve returns the beacon's identity alongside the
/// payload, so a member-side caller can derive the beacon's OwnerAddr.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn identified_resolve_returns_beacon_identity() {
    harmony_app::iroh_endpoint::warm_up_iroh_global_init().await;

    tokio::time::timeout(Duration::from_secs(60), async {
        let setup = setup_two_party_open_join().await;
        let slot = setup.publish_rendezvous_slot(vec![setup.alice_addr]).await;
        assert_eq!(slot, 0, "single advertiser ranks 0 → slot 0");
        await_rendezvous_slot_visible(&setup.pkarr_resolver, &setup.epoch_key, slot).await;

        let res = harmony_app::community_rendezvous::resolve_rendezvous_identified(
            &setup.pkarr_resolver,
            &setup.epoch_key,
            [0xEE; 32], // NOT alice's endpoint id — no self-filtering here
            wall_ms(),
            &harmony_app::community_rendezvous::rendezvous_config_from_env(),
        )
        .await;

        // Happy path against a live mock relay: no slot probe may error at
        // the pkarr layer, or the ResolveError split (review r1 finding 2)
        // would misreport this very scenario.
        assert_eq!(
            res.resolve_errors, 0,
            "a clean resolve must report zero pkarr-layer probe errors"
        );
        // Slot 0 answers on the FIRST batch when we are not filtering ourselves.
        // Paired with the `batches_tried == curve.len()` assertion in
        // `identified_resolve_filters_own_endpoint`, this makes the widening
        // claim a real differential: same curve, same published record, and the
        // batch count moves 1 → full-curve purely because of the self-filter.
        assert_eq!(
            res.outcome.batches_tried, 1,
            "an unfiltered live slot 0 must resolve on the first batch"
        );
        assert_eq!(
            res.outcome.winning_slot,
            Some(0),
            "slot 0 is the live beacon"
        );

        let beacon = res
            .outcome
            .payload
            .expect("alice's slot-0 beacon must resolve");
        assert_eq!(
            beacon.payload.iroh_node_id,
            *setup.alice_ep.node_id().as_bytes(),
            "payload must be alice's endpoint"
        );
        // The outer record's identity must ride along (this is what the plain
        // resolve_rendezvous throws away) — pinned to the EXACT bytes the
        // publisher signed the record with. A mere non-zero check would also
        // pass if the resolver returned `rec.inner_sig`, which is likewise
        // `[u8; 64]` and likewise non-zero.
        //
        // Since the membership gate was removed (spec §5c, epoch-envelope
        // trust), this assertion matters MORE than it did when it was written.
        // That wrong-field bug used to be caught downstream — the gate would
        // reject every beacon, loudly and permanently. Now nothing downstream
        // inspects these bytes: the driver derives the seed key straight from
        // them, so a wrong field would silently seed every beacon under a
        // garbage owner. Quieter, and worse. This is the only check standing
        // between that bug and the dial view.
        assert_eq!(
            beacon.beacon_identity_pub, setup.alice_identity_pub,
            "beacon_identity_pub must be the record's harmony_identity_pub — the composite \
             X25519 ‖ Ed25519 pub the rendezvous publisher was constructed with — and not \
             some other 64-byte field of the record"
        );

        setup.publisher_handle.abort();
        setup.alice_ep.shutdown().await;
        setup.bob_ep.shutdown().await;
    })
    .await
    .expect("identified_resolve_returns_beacon_identity timed out at 60s");
}

/// ZEB-824 self-dial hazard: a member that IS the beacon must see its own slot
/// as empty (spec §5, decode-layer self-filter). With only slot 0 published,
/// filtering self leaves nothing to resolve.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn identified_resolve_filters_own_endpoint() {
    harmony_app::iroh_endpoint::warm_up_iroh_global_init().await;

    tokio::time::timeout(Duration::from_secs(60), async {
        let setup = setup_two_party_open_join().await;
        let slot = setup.publish_rendezvous_slot(vec![setup.alice_addr]).await;
        assert_eq!(slot, 0, "single advertiser ranks 0 → slot 0");
        await_rendezvous_slot_visible(&setup.pkarr_resolver, &setup.epoch_key, slot).await;

        let cfg = harmony_app::community_rendezvous::rendezvous_config_from_env();
        let res = harmony_app::community_rendezvous::resolve_rendezvous_identified(
            &setup.pkarr_resolver,
            &setup.epoch_key,
            *setup.alice_ep.node_id().as_bytes(), // we ARE alice
            wall_ms(),
            &cfg,
        )
        .await;

        // Happy path at the pkarr layer even though nothing resolves: the
        // self-filter is a decode-layer decision, not a probe failure, so it
        // must NOT count as a resolve error (review r1 finding 2 — this is
        // exactly the "no beacon published" proof-shaped absence).
        assert_eq!(
            res.resolve_errors, 0,
            "a self-filtered miss must report zero pkarr-layer probe errors"
        );
        assert!(
            res.outcome.payload.is_none(),
            "own beacon record must be filtered, not returned as a dial candidate"
        );
        assert_eq!(
            res.outcome.winning_slot, None,
            "a filtered slot must not be reported as a winner"
        );
        // The OTHER half of the spec §5 claim: filtering happens at the decode
        // layer, so our own slot reads as EMPTY and the escalating driver keeps
        // widening — it must exhaust the whole curve rather than stopping at the
        // batch that contained our record. Had the filter instead short-circuited
        // the driver (or had slot 0 answered), this would be 1, not the full
        // curve length.
        assert_eq!(
            res.outcome.batches_tried,
            cfg.batch_curve.len(),
            "self-filtered slot 0 must read as empty so the driver widens through \
             every batch in the curve"
        );

        setup.publisher_handle.abort();
        setup.alice_ep.shutdown().await;
        setup.bob_ep.shutdown().await;
    })
    .await
    .expect("identified_resolve_filters_own_endpoint timed out at 60s");
}

/// ZEB-824 headline scenario (spec §8): a member with an EMPTY reachability
/// resolver — rebuilt node, no addrbook sidecar, LAN scouting off — bootstraps a
/// dial candidate out of the rendezvous beacon in a single pass, against the
/// live mock relay.
///
/// **Trust model (Jake's decision, spec §5c): epoch-envelope only**, the same
/// trust open-join already places in this record family. The driver does NOT
/// check the beacon identity against membership — that check compared the
/// repo's two non-convergent 16-byte notions and so rejected every beacon,
/// making the feature a no-op. ZEB-827 carries the principled binding.
///
/// The flavors here are therefore PRODUCTION-FAITHFUL and deliberately
/// mismatched, which is the point:
/// - the member set is keyed by Alice's MASTER addr (`alice_addr`), matching
///   what `ProdGatewayDialCtx::members_of` really returns — verified
///   experimentally on this harness — and it now only drives the STARVED
///   predicate;
/// - the seed lands under the COMPOSITE device-address owner derived from the
///   record's identity bytes, so that is what the assertions read.
///
/// Kick coverage lives in the driver's own unit test
/// (`community_gateway_dial_driver::tests::explicit_kick_fires_when_the_seed_raises_no_auto_kick`);
/// the supervisor dirty-set accessor it asserts on is `cfg(test)`-gated and so
/// invisible from an integration test. Here the seed is the assertion.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gateway_dial_driver_bootstraps_from_rendezvous_beacon() {
    harmony_app::iroh_endpoint::warm_up_iroh_global_init().await;

    tokio::time::timeout(Duration::from_secs(60), async {
        let setup = setup_two_party_open_join().await;
        let slot = setup.publish_rendezvous_slot(vec![setup.alice_addr]).await;
        assert_eq!(slot, 0, "single advertiser ranks 0 → slot 0");
        await_rendezvous_slot_visible(&setup.pkarr_resolver, &setup.epoch_key, slot).await;
        let alice_node_id = *setup.alice_ep.node_id().as_bytes();

        // The owner the driver derives from the record's identity bytes — the
        // composite device-address hash, and the key the seed lands under.
        // Distinct from `alice_addr` (her master addr, used for the member set
        // below); that split is the production reality, not a harness quirk.
        let alice_beacon_owner = OwnerAddr(
            harmony_identity::Identity::from_public_bytes(&setup.alice_identity_pub)
                .expect("alice's published identity bytes must parse")
                .address_hash,
        );

        // A local `GatewayDialCtx`: the traits are pub, but the lib's
        // `cfg(test)` stubs are not visible from an integration test. Alice is
        // this community's one Joined member (self excluded, as production's
        // `ProdGatewayDialCtx::members_of` does), keyed by her MASTER addr —
        // which is all `members_of` feeds now: the starved predicate.
        struct ItCtx {
            community: SpaceId,
            alice: OwnerAddr,
            key: EpochKey,
        }
        #[async_trait::async_trait]
        impl harmony_app::community_gateway_dial_driver::GatewayDialCtx for ItCtx {
            async fn members_of(&self, c: &SpaceId) -> Vec<OwnerAddr> {
                if *c == self.community {
                    vec![self.alice]
                } else {
                    vec![]
                }
            }
            async fn epoch_key_of(&self, _c: &SpaceId) -> Option<EpochKey> {
                Some(self.key.clone())
            }
        }

        // Bob's dial view as it comes up from a rebuild: empty, so the community
        // is starved on the very first pass and the beacon is his only candidate
        // source.
        let resolver = Arc::new(ReachabilityResolver::new());
        assert!(
            resolver.resolve_by_node_id(&alice_node_id).is_none(),
            "precondition: bob's dial view must start empty, or the seed proves nothing"
        );

        let community = setup.community_id;
        let telemetry = Arc::new(harmony_app::network_health::GatewayBootstrapTelemetry::new());
        let driver = harmony_app::community_gateway_dial_driver::CommunityGatewayDialDriver::new(
            Arc::new(ItCtx {
                community,
                alice: setup.alice_addr,
                key: setup.epoch_key.clone(),
            }),
            Arc::new(
                harmony_app::community_gateway_dial_driver::ProdBeaconResolver {
                    pkarr: Arc::clone(&setup.pkarr_resolver),
                    self_endpoint_id: *setup.bob_ep.node_id().as_bytes(),
                },
            ),
            Arc::clone(&resolver),
            Arc::new(move || vec![community]),
        )
        .with_telemetry(Arc::clone(&telemetry));

        driver.run_one_pass().await;

        let (owner, payload) = resolver
            .resolve_by_node_id(&alice_node_id)
            .expect("alice's beacon must be seeded from the mock relay in one pass");
        assert_eq!(
            owner, alice_beacon_owner,
            "the seed must be keyed by the composite OwnerAddr derived from the record's \
             identity, not by the epoch-key holder and not by her master addr"
        );
        assert_ne!(
            alice_beacon_owner, setup.alice_addr,
            "precondition: the two owner flavors really are distinct here, so the \
             assertion above is pinning the composite one specifically"
        );
        assert_eq!(
            payload.iroh_node_id, alice_node_id,
            "the seeded routing blob must be alice's endpoint — the thing the reconnect \
             supervisor's record gate needs before it will dial her"
        );
        // Review INFO-1: pin the verdict too, so a future regression reports
        // WHICH outcome the pass reached rather than only "no seed".
        let summary = telemetry.summary();
        assert_eq!(
            summary
                .per_community
                .iter()
                .find(|row| row.community_short == hex::encode(&community.0[..4]))
                .map(|row| row.outcome.as_str()),
            Some("beaconSeeded"),
            "the pass must record the seed verdict for this community"
        );
        assert_eq!(summary.beacons_seeded, 1);

        setup.publisher_handle.abort();
        setup.alice_ep.shutdown().await;
        setup.bob_ep.shutdown().await;
    })
    .await
    .expect("gateway_dial_driver_bootstraps_from_rendezvous_beacon timed out at 60s");
}

// The escalating-batch failover test (Test 2) needs ≥2 slots to widen past a
// dead slot 0; reference the const so an accidental N drift surfaces here.
const _: () = assert!(RENDEZVOUS_SLOT_COUNT >= 2, "failover test needs >= 2 slots");
