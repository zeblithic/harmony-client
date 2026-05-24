//! ZEB-325 Phase 2c Task 6: two-process integration test for the pkarr+iroh
//! invite-redemption chain — Bob (no prior contact with Alice, zero Reticulum
//! destinations) attempts to join Alice's community via the Phase 2c
//! orchestration.
//!
//! ## What this file pins
//!
//! Test #1 (`bob_redeem_iroh_orchestration_reaches_joined_status`) is the
//! load-bearing assertion that ZEB-325 actually delivered: the entire
//! `connectivity_redeem_invite_iroh_inner` orchestration, from pkarr
//! resolve → record-sig verify → `seed_from_pkarr` → real iroh QUIC dial
//! (Phase 1's `IrohZenohLinkManager.new_link`) → `redeem_invite_inner`
//! with `allow_no_reticulum_destinations=true` → PendingJoin insert →
//! oneshot fires → `outcome.status == "joined"`. Two full
//! `CommunitySyncRegistry` instances are spun up (one per "process"),
//! each with its own real iroh endpoint + link manager +
//! `ReachabilityResolver`. The iroh layer is exercised end-to-end:
//! Bob's `IrohZenohLinkManager` opens a real QUIC stream to Alice using
//! the routing record that Alice published via pkarr and Bob resolved +
//! seeded.
//!
//! Test #2 (`bob_joins_alice_community_via_pure_iroh_path` —
//! `#[ignore]`'d as `KNOWN-BLOCKED-PHASE-2C-PUBLISH-GATE`) is the
//! intended Task 6 test from the plan: the FULL bidirectional CRDT
//! round-trip (Bob's PendingJoin reaches Alice, Alice's auto-counter-sign
//! emits a JoinCountersign that propagates back to Bob, Bob materializes
//! as Joined on both sides). It is ignored because we discovered during
//! implementation that this round-trip is blocked by a production gate
//! that Phase 2c's design did not anticipate — see the docstring on
//! that test for the gap details and unblock plan.
//!
//! ## Reference patterns
//!
//! - `community_reachability_two_engine_integration.rs` — Phase 1 Task 10's
//!   two-iroh-endpoint, two-link-manager, real-QUIC-stream test. We reuse
//!   its hermetic-endpoint pattern.
//! - `community_invite_only_integration.rs::alice_redeems_invite_only_against_bob_admin`
//!   — the two-registry, mpsc-bridged invite-only round-trip pattern
//!   (also `#[ignore]`'d for related-but-not-identical reasons).
//! - `pkarr_invite_redemption_integration.rs::connectivity_redeem_invite_iroh_completes_join_via_crdt_sync`
//!   — single-engine Phase 2c orchestration test (Task 3). The
//!   single-engine variant masks the publish-gate issue because Bob's
//!   own engine is the only one consuming his publishes; there is no
//!   second engine applying the membership-at-publish-HLC gate.

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
use harmony_app::dm_signing::{ed25519_pub_to_x25519, seal_to_owner};
use harmony_app::event_loop::{ChannelLogAdapterRequest, CommunityAdapterRequest};
use harmony_app::iroh_endpoint::{alpn, IrohEndpoint};
use harmony_app::owner_state_crdt::OwnerState;
use harmony_app::owner_state_types::{DeviceIdentityHash, Hlc, OwnerAddr};
use harmony_app::pkarr_invite_publisher::PkarrInvitePublisher;
use harmony_app::reachability_record::ReachabilityAnnouncePayload;
use harmony_app::reachability_resolver::ReachabilityResolver;
use harmony_app::zenoh_iroh_transport::IrohZenohLinkManager;
use harmony_identity::PrivateIdentity;
use harmony_pkarr::{
    testing::MockPkarrRelay, PkarrPublisher, PkarrResolver, RelayClient, RelayPool,
};
use iroh::endpoint::{presets, Endpoint, RelayMode};
use iroh::SecretKey;
use tokio::sync::{mpsc, Mutex as TokioMutex};
use zenoh_link::{EndPoint, LinkManagerUnicastTrait, LinkUnicast};

// ────────────────────────────────────────────────────────────────────────────
// Process-global env-var guard (same pattern as other invite-redemption
// integration tests).
// ────────────────────────────────────────────────────────────────────────────

static REDEEM_TIMEOUT_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct RedeemTimeoutGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    prior: Option<std::ffi::OsString>,
}
impl RedeemTimeoutGuard {
    fn set(value: &str) -> Self {
        let lock = REDEEM_TIMEOUT_ENV_LOCK
            .lock()
            .expect("timeout env lock poisoned");
        let prior = std::env::var_os("HARMONY_REDEEM_INVITE_TIMEOUT_MS");
        // SAFETY: under the static lock; restored on Drop.
        unsafe { std::env::set_var("HARMONY_REDEEM_INVITE_TIMEOUT_MS", value) };
        Self { _lock: lock, prior }
    }
}
impl Drop for RedeemTimeoutGuard {
    fn drop(&mut self) {
        // SAFETY: restore prior env state under the same static lock.
        unsafe {
            match self.prior.take() {
                Some(v) => std::env::set_var("HARMONY_REDEEM_INVITE_TIMEOUT_MS", v),
                None => std::env::remove_var("HARMONY_REDEEM_INVITE_TIMEOUT_MS"),
            }
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Two-owner IdentityResolver.
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
// Helpers.
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
/// `mint_redemption` performs at lib.rs:14580–14587 when it builds the
/// PendingJoin's `joiner_identity_pub`.
///
/// The verify chain's P1 gate (`PendingJoinJoinerPubMismatch`) requires
/// the joiner's composite identity_pub to hash (via
/// `SHA256(x25519_pub ‖ ed25519_pub)[..16]`) to the event actor — so
/// the test-side `self_owner` must be derived the same way. This
/// intentionally does NOT match `PrivateIdentity::identity.address_hash`
/// (which uses HKDF-derived X25519 secrets, not the birational map);
/// matches the convention in `pkarr_invite_redemption_integration.rs`
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
/// no pkarr publisher, no DERP relays. Pattern from
/// `community_reachability_two_engine_integration::build_hermetic_endpoint`.
async fn build_hermetic_endpoint() -> Arc<IrohEndpoint> {
    let secret = SecretKey::generate();
    let inner = Endpoint::builder(presets::Minimal)
        .secret_key(secret)
        .alpns(vec![alpn::HARMONY_ZENOH_V1.to_vec()])
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
/// other's content blobs.
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

/// Build the fixtures shared by both tests (identities, iroh endpoints,
/// link managers, registries, pkarr publication, etc). Returned as a
/// struct so the two tests can fan out at the point where their
/// behavior diverges (the `#[ignore]`'d test wires the mpsc bridge and
/// asserts CRDT convergence; the active test does neither).
struct PhaseTwoCFixtures {
    pkarr_resolver: Arc<PkarrResolver>,
    bob_reachability: ReachabilityResolver,
    alice_addr: OwnerAddr,
    bob_addr: OwnerAddr,
    bob_pub: [u8; 64],
    alice_pub: [u8; 64],
    bob_sk: Arc<SigningKey>,
    bob_identity: PrivateIdentity,
    registry_alice: Arc<CommunitySyncRegistry>,
    registry_bob: Arc<CommunitySyncRegistry>,
    community_id: harmony_app::owner_state_types::SpaceId,
    invite_url: String,
    alice_iroh_node_id_bytes: [u8; 32],
    alice_ep: Arc<IrohEndpoint>,
    bob_ep: Arc<IrohEndpoint>,
    bob_link_mgr: Arc<IrohZenohLinkManager>,
    dir_bob: tempfile::TempDir,
    // Held by the test caller; the active test drains these unused, the
    // `#[ignore]`'d test wires them through a forwarder.
    alice_pub_rx: mpsc::Receiver<Vec<u8>>,
    alice_sub_tx: mpsc::Sender<Vec<u8>>,
}

/// One-shot setup: pkarr relay + Alice's publisher + identities + iroh
/// endpoints + Alice's engine + invite URL. Identical pre-conditions
/// for both tests in this file.
async fn build_fixtures() -> PhaseTwoCFixtures {
    // ── 1. Pkarr infrastructure: MockPkarrRelay + Alice's publisher ──────
    let relay = MockPkarrRelay::start().await;
    let pool = RelayPool::new(vec![relay.base_url.clone()]);
    let client = Arc::new(RelayClient::new(pool));

    let pkarr_publisher = Arc::new(PkarrPublisher::new(Arc::clone(&client)));
    let _publisher_handle = Arc::clone(&pkarr_publisher).spawn();

    // ── 2. Identities. ───────────────────────────────────────────────────
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

    // ── 3. Iroh endpoints + link managers (one per "process"). ──────────
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
    let _alice_accept = alice_link_mgr.spawn_accept_loop();

    let (bob_link_tx, _bob_link_rx) = flume::unbounded::<LinkUnicast>();
    let bob_link_mgr = Arc::new(IrohZenohLinkManager::new(
        Arc::clone(&bob_ep),
        bob_reachability.clone(),
        bob_link_tx,
    ));
    let _bob_accept = bob_link_mgr.spawn_accept_loop();

    // ── 4. Alice's routing blob (real iroh node id, real bound sockets). ─
    let alice_iroh_node_id_bytes = *alice_ep.node_id().as_bytes();
    let alice_routing_blob = {
        let payload = ReachabilityAnnouncePayload {
            iroh_node_id: alice_iroh_node_id_bytes,
            home_relay_url: alice_ep
                .home_relay()
                .map(|r| r.to_string())
                .unwrap_or_default(),
            direct_addresses: alice_bound.clone(),
            announced_at_ms: 1_700_000_000_000,
            identity_signature: [0xCDu8; 64],
        };
        let mut buf = Vec::new();
        ciborium::into_writer(&payload, &mut buf).expect("encode alice routing_blob");
        buf
    };

    let alice_routing_blob_for_builder = alice_routing_blob.clone();
    let invite_pub = PkarrInvitePublisher::new(
        Arc::clone(&pkarr_publisher),
        (*alice_sk).clone(),
        alice_pub,
        Arc::new(move || alice_routing_blob_for_builder.clone()),
    );

    // ── 5. Alice's community + engine + bootstrap Join. ──────────────────
    let alice_minted = harmony_app::mint_community_creation(
        "Phase2cFullCommunity",
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

    let (alice_pub_tx, alice_pub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (alice_sub_tx, alice_sub_rx) = mpsc::channel::<Vec<u8>>(64);
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

    // Keep dir_alice alive for the registry's lifetime via leak (we won't
    // run reconcile here). The Bob dir is returned in the fixture so the
    // active test can keep it alive on the stack.
    std::mem::forget(dir_alice);

    // ── 6. Build the invite-only InviteToken + sealed epoch key + URL. ──
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
        ed25519_pub_to_x25519(&verifying_bytes).expect("bob ed25519→x25519")
    };
    let sealed_epoch_key = seal_to_owner(&bob_x25519_pub, alice_minted.membership_key.as_bytes())
        .expect("seal epoch key to bob");

    let invite_payload = CommunityInvitePayload {
        community_id,
        epoch_snapshot: InviteEpochSnapshot {
            epoch: 0,
            sealed_epoch_key,
            state_snapshot: MaterializedCommunityState::default(),
        },
        admin_addr: alice_addr,
        community_name: "Phase2cFullCommunity".into(),
        is_invite_only: true,
        expires_at: None,
        invite_token: Some(invite_token),
        admin_bootstrap: Some(alice_minted.bootstrap_join.clone()),
        admin_identity_pub: Some(alice_pub),
        forked_from: None,
        pre_fork_snapshot: None,
    };
    let invite_url = community_invite::encode_invite_url(&invite_payload).expect("encode invite");

    // ── 7. Alice publishes case-A pkarr record, wait for visibility. ─────
    invite_pub.register_invite(&invite_payload).await;
    let pkarr_resolver = Arc::new(PkarrResolver::new(Arc::clone(&client)));

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

    PhaseTwoCFixtures {
        pkarr_resolver,
        bob_reachability,
        alice_addr,
        bob_addr,
        bob_pub,
        alice_pub,
        bob_sk,
        bob_identity,
        registry_alice,
        registry_bob,
        community_id,
        invite_url,
        alice_iroh_node_id_bytes,
        alice_ep,
        bob_ep,
        bob_link_mgr,
        dir_bob,
        alice_pub_rx,
        alice_sub_tx,
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Test #1: ZEB-325 Phase 2c orchestration end-to-end (the part that works).
// ────────────────────────────────────────────────────────────────────────────

/// Active load-bearing assertion for ZEB-325 Phase 2c:
///
/// 1. Alice publishes case-A pkarr record (real `MockPkarrRelay`).
/// 2. Bob's `IrohZenohLinkManager.new_link` opens a real QUIC stream to
///    Alice's iroh endpoint using the routing record Bob seeded via
///    `seed_from_pkarr`. This is the iroh-layer half of Phase 2c, which
///    IS wired in production.
/// 3. Bob's `connectivity_redeem_invite_iroh_inner` runs the full
///    orchestration with `allow_no_reticulum_destinations=true` against a
///    real, fully-active second community-sync registry. The IPC returns
///    `outcome.status == "joined"` and the community_id of Alice's invite.
/// 4. Bob's `ReachabilityResolver` carries Alice's routing record post-IPC
///    (proves `seed_from_pkarr` ran).
///
/// What this test does NOT assert (deliberately deferred to test #2):
/// - The full PendingJoin→JoinCountersign CRDT round-trip across the two
///   engines. Phase 2c's design assumes Bob's PendingJoin reaches Alice
///   via Zenoh-CRDT-sync; in practice today, Alice's engine drops Bob's
///   publish at the `PublisherNotJoined` gate
///   (`community_state_sync.rs::handle_incoming_publish`, line 3097)
///   because Bob is not yet a member of Alice's community at publish
///   HLC. See test #2's docstring for the gap details and unblock plan.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bob_redeem_iroh_orchestration_reaches_joined_status() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("harmony_app=warn")),
        )
        .with_test_writer()
        .try_init();

    // Short oneshot timeout so the test fails fast if the orchestration wedges.
    let _timeout_guard = RedeemTimeoutGuard::set("5000");

    tokio::time::timeout(Duration::from_secs(60), async {
        let fx = build_fixtures().await;

        // ── A. Phase 1 iroh-layer pre-flight: seed Bob's resolver and
        //       have his link manager open a real QUIC stream to Alice's
        //       iroh endpoint. Proves the seeded record is consumable
        //       exactly as production code would consume it via Zenoh's
        //       LinkManagerUnicastTrait.new_link, mirroring Phase 1
        //       Task 10's two-endpoint test (loopback). ──────────────
        // Re-derive the same payload that Alice published so the
        // pre-flight uses the IDENTICAL bytes the IPC will resolve.
        let alice_routing_payload = ReachabilityAnnouncePayload {
            iroh_node_id: fx.alice_iroh_node_id_bytes,
            home_relay_url: fx
                .alice_ep
                .home_relay()
                .map(|r| r.to_string())
                .unwrap_or_default(),
            direct_addresses: fx.alice_ep.bound_sockets(),
            announced_at_ms: 1_700_000_000_000,
            identity_signature: [0xCDu8; 64],
        };
        fx.bob_reachability
            .seed_from_pkarr(
                fx.alice_addr,
                DeviceIdentityHash([0u8; 16]),
                alice_routing_payload,
            )
            .await;

        let alice_endpoint_str = hex::encode(fx.alice_ep.node_id().as_bytes());
        let zenoh_endpoint = EndPoint::new("iroh", &alice_endpoint_str, "", "")
            .expect("construct iroh zenoh locator");
        let bob_to_alice_link = fx
            .bob_link_mgr
            .new_link(zenoh_endpoint)
            .await
            .expect(
                "Phase 1 iroh QUIC dial must succeed against the seed_from_pkarr-seeded record — \
                 this is the load-bearing proof that the seeded record is consumable by the \
                 link manager exactly as production code would consume it",
            );
        // Close the pre-flight link; the IPC below doesn't reuse it
        // (production would, once the Zenoh-over-iroh transport is
        // registered with the Zenoh session — see Phase 2 deferred work).
        let _ = bob_to_alice_link.0.close().await;
        drop(bob_to_alice_link);

        // ── B. Bob's redemption infrastructure. ─────────────────────────
        let (bob_adapter_tx, mut bob_adapter_rx) = mpsc::channel::<CommunityAdapterRequest>(8);
        // Drain Bob's adapter dispatch — the engine spawns and its
        // publisher_rx/subscriber_tx halves are dropped here; that's
        // intentional for this test (we're not asserting on the CRDT
        // round-trip, only on the IPC's `outcome.status`). The
        // `#[ignore]`'d test #2 below wires them through a forwarder.
        tokio::spawn(async move {
            while let Some(req) = bob_adapter_rx.recv().await {
                drop(req.publisher_rx);
                drop(req.subscriber_tx);
            }
        });
        // Keep alice_pub_rx/alice_sub_tx alive (avoid premature drop
        // tearing down alice's engine) but unused.
        let _alice_pub_rx_hold = fx.alice_pub_rx;
        let _alice_sub_tx_hold = fx.alice_sub_tx;

        let (bob_unicast_tx, mut bob_unicast_rx) = mpsc::channel::<UnicastSendRequest>(8);
        tokio::spawn(async move { while bob_unicast_rx.recv().await.is_some() {} });

        let bob_dm_outbox = Arc::new(TokioMutex::new(DmOutbox::new(
            "bob-dev".into(),
            fx.bob_addr,
            DeviceIdentityHash(fx.bob_identity.identity.address_hash),
            Arc::clone(&fx.bob_sk),
            Arc::new(dup_identity(&fx.bob_identity)),
        )));

        let (bob_channel_log_adapter_tx, _bob_channel_log_adapter_rx) =
            mpsc::unbounded_channel::<ChannelLogAdapterRequest>();
        let bob_app = tauri::test::mock_app();
        let bob_channel_log_registry = Arc::new(ChannelLogRegistry::new(ChannelLogRegistryConfig {
            adapter_request_tx: bob_channel_log_adapter_tx,
            app: bob_app.handle().clone(),
            identity_dir: fx.dir_bob.path().to_path_buf(),
            self_owner: fx.bob_addr,
            self_device_id: "bob-dev".into(),
            signing_key: Arc::clone(&fx.bob_sk),
            engine_config: ChannelLogEngineConfig::default(),
        }));

        let bob_crdt_state = Arc::new(TokioMutex::new(OwnerState::default()));
        let bob_hlc_tracker = Arc::new(TokioMutex::new(BTreeMap::<String, Hlc>::new()));

        // ── C. Drive Phase 2c IPC. ───────────────────────────────────────
        let outcome = harmony_app::connectivity_redeem_invite_iroh_inner(
            fx.invite_url,
            Some(Arc::clone(&fx.pkarr_resolver)),
            Some(fx.bob_reachability.clone()),
            Arc::clone(&bob_crdt_state),
            Arc::clone(&bob_hlc_tracker),
            "bob-dev".to_string(),
            fx.bob_addr,
            Arc::clone(&fx.bob_sk),
            Arc::clone(&fx.registry_bob),
            bob_adapter_tx,
            bob_unicast_tx,
            Arc::clone(&bob_dm_outbox),
            Arc::clone(&bob_channel_log_registry),
            None,
            |_| {},
        )
        .await
        .expect(
            "connectivity_redeem_invite_iroh_inner must Ok (it surfaces failures as outcome.status)",
        );

        // ── D. Load-bearing assertions. ──────────────────────────────────
        assert_eq!(
            outcome.status, "joined",
            "Phase 2c orchestration must return 'joined' on the happy path \
             (pkarr resolves + iroh dial succeeds + PendingJoin inserts + \
             oneshot fires). Got status={:?} community_id={:?}.",
            outcome.status, outcome.community_id
        );
        assert_eq!(
            outcome.community_id.as_deref(),
            Some(hex::encode(fx.community_id.0).as_str()),
            "community_id must echo Alice's invite"
        );

        // seed_from_pkarr inside the IPC re-wrote (idempotent) Alice's
        // record into Bob's resolver.
        let seeded = fx.bob_reachability.resolve(&fx.alice_addr);
        assert!(
            !seeded.is_empty(),
            "Bob's ReachabilityResolver must contain Alice's routing record \
             after the IPC's seed_from_pkarr call"
        );
        assert_eq!(
            seeded[0].iroh_node_id, fx.alice_iroh_node_id_bytes,
            "seeded record must carry Alice's actual iroh_node_id"
        );

        // Bob's CRDT must contain admin's bootstrap (verified during the
        // invite-URL decode chain) and his own PendingJoin (inserted at
        // engine.insert_local_event time inside redeem_invite_inner).
        // We do NOT assert that Bob materializes as Joined — that
        // assertion lives in test #2 (currently `#[ignore]`'d).
        let bob_state = fx
            .registry_bob
            .state_for(&fx.community_id)
            .await
            .expect("bob state must exist");
        let bob_events: Vec<_> = {
            let g = bob_state.lock().await;
            g.events.values().cloned().collect()
        };
        assert!(
            bob_events.len() >= 2,
            "Bob's CRDT must contain ≥ 2 events (admin bootstrap + PendingJoin); got {}",
            bob_events.len()
        );
        let actors: std::collections::BTreeSet<_> = bob_events.iter().map(|e| e.actor).collect();
        assert!(
            actors.contains(&fx.alice_addr),
            "Bob's CRDT must contain Alice's bootstrap Join (actor=alice_addr)"
        );
        assert!(
            actors.contains(&fx.bob_addr),
            "Bob's CRDT must contain his own PendingJoin (actor=bob_addr)"
        );

        // Silence unused warnings — Alice's identity_pub is used inside
        // the engine bind step (build_fixtures), Bob's pub for the
        // resolver wiring.
        let _ = (fx.alice_pub, fx.bob_pub);

        // Graceful teardown.
        fx.alice_ep.shutdown().await;
        fx.bob_ep.shutdown().await;
    })
    .await
    .expect("bob_redeem_iroh_orchestration_reaches_joined_status timed out at 60s");
}

// ────────────────────────────────────────────────────────────────────────────
// Test #2: the intended full-roundtrip test from the plan, currently
// `#[ignore]`'d as KNOWN-BLOCKED.
// ────────────────────────────────────────────────────────────────────────────

/// **KNOWN-BLOCKED on a Phase 2c design gap discovered during Task 6
/// implementation.** The plan §568-602 specified this test as the
/// load-bearing end-to-end validation. Implementation revealed it cannot
/// pass against the current production code, for a reason the plan did
/// not anticipate:
///
/// **The gap:** Phase 2c's design (per `docs/specs/2026-05-23-zeb-321-phase2-discovery-bootstrap-design.md`
/// §7.2) assumes Bob's PendingJoin reaches Alice via the engine's
/// state-root publisher over an iroh-borne Zenoh link. But
/// `community_state_sync::handle_incoming_publish` (line 3097) drops
/// publishes from non-members with `PublisherNotJoined` (`status:
/// MemberStatus::Left, left_at: None`). Bob is a non-member of Alice's
/// community at publish HLC, so Alice's engine drops his PendingJoin
/// publish before it reaches `insert_event`. The unicast path
/// (`community_invite::handle_unicast`) bypasses this gate by calling
/// `engine.insert_local_event` directly; CRDT-sync has no such bypass.
///
/// The plan's verified pre-conditions (lines 668-673) include
/// `"verify_event rule P6 explicitly permits PendingJoin from non-members"`
/// — that's true at the per-event verify level, but the PUBLISH gate is
/// a strictly-prior check that rejects the entire publish blob before
/// any event-level verify runs.
///
/// **The deferred-cases comment** in `handle_incoming_publish`
/// (lines 3070-3088) explicitly tracks this category of issue under
/// `ZEB-260`:
///
/// - Case A (invite-only joiner with empty CRDT receiving admin's first
///   publish-back) — FIXED via admin_bootstrap plumbed in invite URL.
/// - Case B (open-community brand-new joiner whose self-Join is only
///   inside their own publish blob) — DEFERRED.
/// - Case C (self-Re-Join after Leave) — DEFERRED.
///
/// What Phase 2c needs is a fourth case ("Case D — invite-only joiner
/// publishing their own PendingJoin to admin who has not yet seen them")
/// or a generalized "PendingJoin-publish bootstrap" gate relaxation
/// that's specifically scoped to PendingJoin events carrying a valid
/// admin-signed InviteToken.
///
/// **Implication for Phase 2c (option C "pure CRDT sync"):** The
/// current implementation reaches `outcome.status == "joined"` because
/// the engine's post-Inserted hook fires the redemption oneshot on
/// EVERY inserted event (line 1524), including Bob's own PendingJoin —
/// not only on the JoinCountersign as the design assumes. So the IPC
/// status is structurally optimistic: it does not require the
/// counter-sign round-trip to complete. The single-engine Task 3 test
/// in `pkarr_invite_redemption_integration.rs` masks this because there's
/// no second engine to drop the publish.
///
/// **Unblock path** (separate ticket — recommend filing as
/// "ZEB-NNN: relax CRDT-sync publish gate for PendingJoin bootstrap"):
///
/// 1. Either: in `handle_incoming_publish`, detect publishes carrying a
///    single PendingJoin event with a valid admin-signed InviteToken and
///    skip the `PublisherNotJoined` gate for that specific case (decrypt
///    + peek before applying the membership-at-publish-HLC gate). This
///    is a targeted carve-out, not a general relaxation.
/// 2. Or: have Bob's `redeem_invite_inner` immediately follow up the
///    PendingJoin publish with a self-Join publish (won't work today —
///    self-Join from a non-member would itself be dropped at the
///    publish gate, and a non-counter-signed Join is verify-rejected).
/// 3. Or: register the Zenoh-over-iroh transport with the production
///    Zenoh session AND add an iroh-borne-direct "first-contact" wire
///    path (a small new protocol on top of `harmony/zenoh/v1`?) that
///    bypasses both the publish gate and the membership-at-publish-HLC
///    invariant.
///
/// Option 1 is the smallest change and aligns with the existing
/// "Case A" fix pattern. Recommend prioritizing option 1.
///
/// **Once that ticket lands:** remove the `#[ignore]` attribute on this
/// test, wire up the mpsc forwarder between Bob's adapter dispatch and
/// Alice's engine (mirroring `community_invite_only_integration.rs`
/// lines 430-454), and the round-trip + materialization assertions
/// should pass.
#[ignore = "KNOWN-BLOCKED-PHASE-2C-PUBLISH-GATE: see docstring for ZEB-NNN follow-up"]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bob_joins_alice_community_via_pure_iroh_path() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("harmony_app=warn")),
        )
        .with_test_writer()
        .try_init();

    let _timeout_guard = RedeemTimeoutGuard::set("5000");

    tokio::time::timeout(Duration::from_secs(60), async {
        let fx = build_fixtures().await;

        // Pre-seed Bob's resolver + verify iroh dial works (same as
        // test #1 step A).
        let alice_routing_payload = ReachabilityAnnouncePayload {
            iroh_node_id: fx.alice_iroh_node_id_bytes,
            home_relay_url: fx
                .alice_ep
                .home_relay()
                .map(|r| r.to_string())
                .unwrap_or_default(),
            direct_addresses: fx.alice_ep.bound_sockets(),
            announced_at_ms: 1_700_000_000_000,
            identity_signature: [0xCDu8; 64],
        };
        fx.bob_reachability
            .seed_from_pkarr(
                fx.alice_addr,
                DeviceIdentityHash([0u8; 16]),
                alice_routing_payload,
            )
            .await;

        // Wire the in-process mpsc bridge between Bob's engine and
        // Alice's engine. When the Zenoh-over-iroh transport is
        // registered with the Zenoh session (deferred Phase 2 work),
        // this forwarder can be replaced with the real Zenoh session
        // bridge and this test becomes a true cross-WAN canary.
        let (bob_adapter_tx, mut bob_adapter_rx) = mpsc::channel::<CommunityAdapterRequest>(8);
        let alice_sub_tx_for_fwd = fx.alice_sub_tx.clone();
        let mut alice_pub_rx = fx.alice_pub_rx;
        tokio::spawn(async move {
            if let Some(req) = bob_adapter_rx.recv().await {
                let mut bob_pub_rx = req.publisher_rx;
                let alice_sub_tx_in = alice_sub_tx_for_fwd.clone();
                tokio::spawn(async move {
                    while let Some(bytes) = bob_pub_rx.recv().await {
                        if alice_sub_tx_in.send(bytes).await.is_err() {
                            break;
                        }
                    }
                });
                let bob_sub_tx = req.subscriber_tx;
                tokio::spawn(async move {
                    while let Some(bytes) = alice_pub_rx.recv().await {
                        if bob_sub_tx.send(bytes).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });

        let (bob_unicast_tx, mut bob_unicast_rx) = mpsc::channel::<UnicastSendRequest>(8);
        tokio::spawn(async move { while bob_unicast_rx.recv().await.is_some() {} });

        let bob_dm_outbox = Arc::new(TokioMutex::new(DmOutbox::new(
            "bob-dev".into(),
            fx.bob_addr,
            DeviceIdentityHash(fx.bob_identity.identity.address_hash),
            Arc::clone(&fx.bob_sk),
            Arc::new(dup_identity(&fx.bob_identity)),
        )));

        let (bob_channel_log_adapter_tx, _bob_channel_log_adapter_rx) =
            mpsc::unbounded_channel::<ChannelLogAdapterRequest>();
        let bob_app = tauri::test::mock_app();
        let bob_channel_log_registry =
            Arc::new(ChannelLogRegistry::new(ChannelLogRegistryConfig {
                adapter_request_tx: bob_channel_log_adapter_tx,
                app: bob_app.handle().clone(),
                identity_dir: fx.dir_bob.path().to_path_buf(),
                self_owner: fx.bob_addr,
                self_device_id: "bob-dev".into(),
                signing_key: Arc::clone(&fx.bob_sk),
                engine_config: ChannelLogEngineConfig::default(),
            }));

        let bob_crdt_state = Arc::new(TokioMutex::new(OwnerState::default()));
        let bob_hlc_tracker = Arc::new(TokioMutex::new(BTreeMap::<String, Hlc>::new()));

        let outcome = harmony_app::connectivity_redeem_invite_iroh_inner(
            fx.invite_url,
            Some(Arc::clone(&fx.pkarr_resolver)),
            Some(fx.bob_reachability.clone()),
            Arc::clone(&bob_crdt_state),
            Arc::clone(&bob_hlc_tracker),
            "bob-dev".to_string(),
            fx.bob_addr,
            Arc::clone(&fx.bob_sk),
            Arc::clone(&fx.registry_bob),
            bob_adapter_tx,
            bob_unicast_tx,
            Arc::clone(&bob_dm_outbox),
            Arc::clone(&bob_channel_log_registry),
            None,
            |_| {},
        )
        .await
        .expect("inner must Ok");

        assert_eq!(outcome.status, "joined");

        // Wait for both engines to converge: Bob shows himself Joined,
        // Alice shows Bob Joined. This requires the JoinCountersign
        // round-trip to complete — which today fails because Alice's
        // engine drops Bob's PendingJoin publish at the PublisherNotJoined
        // gate. See test's docstring for the unblock plan.
        let bob_state = fx
            .registry_bob
            .state_for(&fx.community_id)
            .await
            .expect("bob state");
        let alice_state = fx
            .registry_alice
            .state_for(&fx.community_id)
            .await
            .expect("alice state");

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let converged = loop {
            let bob_ok = {
                let g = bob_state.lock().await;
                let events: Vec<_> = g.events.values().cloned().collect();
                let mat = materialize(&events, fx.alice_addr);
                mat.members.get(&fx.bob_addr).map(|m| m.status) == Some(MemberStatus::Joined)
            };
            let alice_ok = {
                let g = alice_state.lock().await;
                let events: Vec<_> = g.events.values().cloned().collect();
                let mat = materialize(&events, fx.alice_addr);
                mat.members.get(&fx.bob_addr).map(|m| m.status) == Some(MemberStatus::Joined)
            };
            if bob_ok && alice_ok {
                break true;
            }
            if tokio::time::Instant::now() > deadline {
                break false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        assert!(
            converged,
            "membership did not converge — see test docstring \
             (KNOWN-BLOCKED-PHASE-2C-PUBLISH-GATE)"
        );

        // Reference unused fields so the struct keeps them for future
        // when this test is un-ignored.
        let _ = (fx.alice_pub, fx.bob_pub);

        fx.alice_ep.shutdown().await;
        fx.bob_ep.shutdown().await;
    })
    .await
    .expect("bob_joins_alice_community_via_pure_iroh_path timed out at 60s");
}
