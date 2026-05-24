//! Case A integration tests: pkarr invite-redemption publish → resolve,
//! plus the Phase 2c (ZEB-325) end-to-end orchestration test that drives
//! `connectivity_redeem_invite_iroh_inner` through to a `"joined"` outcome.
//!
//! Alice publishes her iroh routing under HKDF(invite_token.sig, epoch) via
//! `PkarrInvitePublisher`. Bob independently derives the same key from the
//! invite token and resolves via `PkarrResolver`. Tests use `MockPkarrRelay`
//! as the relay, so no live DHT is needed.

use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::{Signer, SigningKey};
use harmony_app::community_invite::{
    CommunityInvitePayload, InviteEpochSnapshot, InviteToken, MaterializedCommunityState,
};
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};
use harmony_app::pkarr_invite_publisher::PkarrInvitePublisher;
use harmony_app::reachability_record::ReachabilityAnnouncePayload;
use harmony_pkarr::{
    current_epoch_id, derive_ephemeral_key, testing::MockPkarrRelay, PkarrCase, PkarrPublisher,
    PkarrResolver, RelayClient, RelayPool,
};

fn build_identity_pub(sk: &SigningKey) -> [u8; 64] {
    let mut out = [0u8; 64];
    out[32..].copy_from_slice(&sk.verifying_key().to_bytes());
    out
}

fn fixture_hlc() -> Hlc {
    Hlc {
        wall_ms: 1_700_000_000_000,
        logical: 0,
        device_id: "test".into(),
    }
}

fn fixture_invite_token(inviter: OwnerAddr) -> InviteToken {
    InviteToken {
        inviter,
        invitee_hint: None,
        minted_at: fixture_hlc(),
        expires_at: None,
        sig: [0xAA; 64],
    }
}

fn fixture_invite_payload(inviter: OwnerAddr) -> CommunityInvitePayload {
    CommunityInvitePayload {
        community_id: SpaceId([0x11; 16]),
        epoch_snapshot: InviteEpochSnapshot {
            epoch: 0,
            sealed_epoch_key: vec![0u8; 32],
            state_snapshot: MaterializedCommunityState::default(),
        },
        admin_addr: inviter,
        community_name: "Test Community".into(),
        is_invite_only: true,
        expires_at: None,
        invite_token: Some(fixture_invite_token(inviter)),
        admin_bootstrap: None,
        admin_identity_pub: None,
        forked_from: None,
        pre_fork_snapshot: None,
    }
}

fn fixture_routing_blob(iroh_node_id: [u8; 32]) -> Vec<u8> {
    let payload = ReachabilityAnnouncePayload {
        iroh_node_id,
        home_relay_url: "https://relay.test/".into(),
        direct_addresses: vec![],
        announced_at_ms: 1_700_000_000_000,
        identity_signature: [0xCD; 64],
    };
    let mut buf = Vec::new();
    ciborium::into_writer(&payload, &mut buf).expect("encode routing_blob");
    buf
}

#[tokio::test]
async fn case_a_publish_then_resolve_round_trip() {
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        // --- Setup ---
        let relay = MockPkarrRelay::start().await;
        let pool = RelayPool::new(vec![relay.base_url.clone()]);
        let client = Arc::new(RelayClient::new(pool));

        let publisher = Arc::new(PkarrPublisher::new(Arc::clone(&client)));
        let _ph = Arc::clone(&publisher).spawn();

        // Alice's identity key (deterministic for test reproducibility).
        let alice_sk = SigningKey::from_bytes(&[0x42u8; 32]);
        let alice_identity_pub = build_identity_pub(&alice_sk);
        let alice_iroh_node_id = [0xABu8; 32];

        let routing_blob_builder = {
            let iroh_id = alice_iroh_node_id;
            Arc::new(move || fixture_routing_blob(iroh_id))
        };

        let inv_pub = PkarrInvitePublisher::new(
            Arc::clone(&publisher),
            alice_sk.clone(),
            alice_identity_pub,
            routing_blob_builder,
        );

        // Alice's invite (fixed invite_token.sig = [0xAA; 64]).
        let alice_owner_addr = OwnerAddr([0x11u8; 16]);
        let invite = fixture_invite_payload(alice_owner_addr);

        // --- Case A: Alice registers the invite publication ---
        inv_pub.register_invite(&invite).await;

        // --- Bob's side: derive the same key ---
        let token_sig: [u8; 64] = [0xAAu8; 64];
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time")
            .as_millis() as u64;
        let epoch_id = current_epoch_id(now_ms);
        let bob_signing =
            derive_ephemeral_key(PkarrCase::Invite, &token_sig, &epoch_id.to_be_bytes());
        let bob_verifying = bob_signing.verifying_key();

        // --- Poll the relay until the record appears (up to 5s) ---
        let resolver = PkarrResolver::new(Arc::clone(&client));
        let mut record = None;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if let Ok(Some(rec)) = resolver.resolve(&bob_verifying).await {
                record = Some(rec);
                break;
            }
        }
        let record = record.expect("record should appear within 5s");

        // --- Verify inner signature binds to alice's identity_pub ---
        record
            .verify_inner_sig()
            .expect("inner sig must be valid (RPK2)");

        // The record's harmony_identity_pub should be alice's.
        assert_eq!(
            record.harmony_identity_pub, alice_identity_pub,
            "record must carry alice's identity_pub"
        );

        // --- Decode routing_blob and verify iroh_node_id ---
        let decoded_payload: ReachabilityAnnouncePayload =
            ciborium::from_reader(record.routing_blob.as_slice())
                .expect("routing_blob must decode as ReachabilityAnnouncePayload");
        assert_eq!(
            decoded_payload.iroh_node_id, alice_iroh_node_id,
            "decoded routing must carry alice's iroh node id"
        );
        assert_eq!(
            decoded_payload.home_relay_url, "https://relay.test/",
            "relay URL must match"
        );
    })
    .await;

    result.expect("case A integration test timed out");
}

// ── ZEB-325 Phase 2c Task 3: end-to-end orchestration test ──────────────────
//
// The previous test exercises the publish→resolve primitives. This test
// drives the full `connectivity_redeem_invite_iroh_inner` orchestration:
// pkarr-resolve → seed_from_pkarr → redeem_invite_inner. The assertion is
// status="joined" (NOT "pkarr_resolved_no_handshake") with the iroh path's
// new `allow_no_reticulum_destinations=true` semantics.
//
// Bob's setup mirrors `redeem_invite_inner_allow_no_reticulum_destinations_skips_fast_fail`
// (the Task 2 unit test): a custom registry whose self_owner is derived
// from the joiner's signing-key, so the PendingJoin actor-hash gate admits.
// `HARMONY_REDEEM_INVITE_TIMEOUT_MS=500` keeps the oneshot await fast — the
// 5s default would mask a wedge under CI.

use harmony_app::community_channel_log_engine::{
    ChannelLogEngineConfig, ChannelLogRegistry, ChannelLogRegistryConfig,
};
use harmony_app::community_state_sync::{
    CommunityRegistryConfig, CommunitySyncRegistry, IdentityResolver, DEFAULT_DEBOUNCE_MS,
};
use harmony_app::content_store::{ContentStore, RuntimeContentStore};
use harmony_app::dm_outbox::{DmOutbox, UnicastSendRequest};
use harmony_app::event_loop::{ChannelLogAdapterRequest, CommunityAdapterRequest};
use harmony_app::owner_state_crdt::OwnerState;
use harmony_app::owner_state_types::{DeviceIdentityHash, EpochKey};
use harmony_app::reachability_resolver::ReachabilityResolver;
use harmony_identity::PrivateIdentity;
use std::collections::BTreeMap;
use tokio::sync::{mpsc, Mutex as TokioMutex};

/// Process-global mutex serializing mutations to the
/// `HARMONY_REDEEM_INVITE_TIMEOUT_MS` env var across parallel tests
/// in this binary. (See `community_invite_only_integration.rs` for
/// the same pattern.)
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
        // SAFETY: set_var is unsafe in Rust 2024; this test owns its env
        // window under the static lock above.
        unsafe { std::env::set_var("HARMONY_REDEEM_INVITE_TIMEOUT_MS", value) };
        Self { _lock: lock, prior }
    }
}
impl Drop for RedeemTimeoutGuard {
    fn drop(&mut self) {
        // SAFETY: restore the prior env state under the same static lock.
        unsafe {
            match self.prior.take() {
                Some(v) => std::env::set_var("HARMONY_REDEEM_INVITE_TIMEOUT_MS", v),
                None => std::env::remove_var("HARMONY_REDEEM_INVITE_TIMEOUT_MS"),
            }
        }
    }
}

/// Two-owner identity resolver — same shape as the unit-test fixture.
struct TwoOwnerResolver {
    admin: OwnerAddr,
    admin_pub: [u8; 64],
    joiner: OwnerAddr,
    joiner_pub: [u8; 64],
}
#[async_trait::async_trait]
impl IdentityResolver for TwoOwnerResolver {
    async fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
        if *addr == self.admin {
            Some(self.admin_pub)
        } else if *addr == self.joiner {
            Some(self.joiner_pub)
        } else {
            None
        }
    }
}

fn signing_key_from_identity(identity: &PrivateIdentity) -> Arc<ed25519_dalek::SigningKey> {
    let sk_bytes_full = identity.to_private_bytes();
    let mut ed_seed = [0u8; 32];
    ed_seed.copy_from_slice(&sk_bytes_full[32..64]);
    Arc::new(ed25519_dalek::SigningKey::from_bytes(&ed_seed))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connectivity_redeem_invite_iroh_completes_join_via_crdt_sync() {
    // Short oneshot timeout so the test runs in <1s if no counter-sig
    // arrives. The 5s default would mask a wedge.
    let _timeout_guard = RedeemTimeoutGuard::set("500");

    let result = tokio::time::timeout(Duration::from_secs(30), async {
        // ── 1. Spin up MockPkarrRelay + Alice's publisher ──────────────
        let relay = MockPkarrRelay::start().await;
        let pool = RelayPool::new(vec![relay.base_url.clone()]);
        let client = Arc::new(RelayClient::new(pool));

        let publisher = Arc::new(PkarrPublisher::new(Arc::clone(&client)));
        let _publisher_handle = Arc::clone(&publisher).spawn();

        // ── 2. Alice's identity. The joiner-side PendingJoin gate checks
        //       that the invite's admin_identity_pub matches the pkarr
        //       record's harmony_identity_pub AND that the same pub bytes
        //       in the invite resolve to admin_addr via from_public_bytes.
        //       So we derive admin_addr from the same Ed25519 verifying key
        //       that signs the pkarr record — i.e., build the 64-byte
        //       pub the same way mint_redemption builds the joiner's. ──
        let alice_sk_raw = ed25519_dalek::SigningKey::from_bytes(&[0xaa; 32]);
        let alice_sk = Arc::new(alice_sk_raw.clone());
        let (alice_addr, alice_pub_64) = {
            let x25519_priv = harmony_app::dm_signing::ed25519_priv_to_x25519(&alice_sk);
            let x25519_pub =
                x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(*x25519_priv));
            let ed25519_pub = alice_sk.verifying_key().to_bytes();
            let mut combined = [0u8; 64];
            combined[..32].copy_from_slice(x25519_pub.as_bytes());
            combined[32..].copy_from_slice(&ed25519_pub);
            let id = harmony_identity::Identity::from_public_bytes(&combined)
                .expect("from_public_bytes (alice)");
            (OwnerAddr(id.address_hash), combined)
        };

        // Alice's iroh node id (used inside the routing_blob).
        let alice_iroh_node_id = [0xABu8; 32];
        let routing_blob_builder = {
            let iroh_id = alice_iroh_node_id;
            Arc::new(move || {
                let payload = ReachabilityAnnouncePayload {
                    iroh_node_id: iroh_id,
                    home_relay_url: "https://relay.test/".into(),
                    direct_addresses: vec![],
                    announced_at_ms: 1_700_000_000_000,
                    identity_signature: [0xCDu8; 64],
                };
                let mut buf = Vec::new();
                ciborium::into_writer(&payload, &mut buf).expect("encode routing_blob");
                buf
            })
        };

        let inv_pub = PkarrInvitePublisher::new(
            Arc::clone(&publisher),
            (*alice_sk).clone(),
            alice_pub_64,
            routing_blob_builder,
        );

        // ── 3. Build Alice's invite-only payload. ───────────────────────
        //       Joiner-side derivation (mirrors mint_redemption's gate).
        let joiner_identity = PrivateIdentity::from_seed(&[0xbb; 32]);
        let joiner_sk = signing_key_from_identity(&joiner_identity);
        let (joiner_addr, joiner_pub_64) = {
            let x25519_priv = harmony_app::dm_signing::ed25519_priv_to_x25519(&joiner_sk);
            let x25519_pub =
                x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(*x25519_priv));
            let ed25519_pub = joiner_sk.verifying_key().to_bytes();
            let mut combined = [0u8; 64];
            combined[..32].copy_from_slice(x25519_pub.as_bytes());
            combined[32..].copy_from_slice(&ed25519_pub);
            let id = harmony_identity::Identity::from_public_bytes(&combined)
                .expect("from_public_bytes (joiner)");
            (OwnerAddr(id.address_hash), combined)
        };

        // Build the admin's signed bootstrap-Join event. Sign with the
        // raw ed25519 signing key (community_membership::sign_event is the
        // signing-key flavor; sign_event_with_identity requires a PrivateIdentity).
        let community_id = SpaceId([0xf6; 16]);
        let membership_key = EpochKey::new([0x42; 32]);
        let admin_bootstrap = {
            let payload = harmony_app::community_membership::EventPayload {
                id: [0x11; 16],
                community_id,
                kind: harmony_app::community_membership::MembershipEventKind::Join,
                actor: alice_addr,
                at: Hlc {
                    wall_ms: 900,
                    logical: 0,
                    device_id: "alice-dev".into(),
                },
            };
            harmony_app::community_membership::sign_event(&payload, alice_sk.as_ref())
                .expect("sign admin bootstrap")
        };

        // Build the admin-signed invite-token.
        let token_minted_at = Hlc {
            wall_ms: 950,
            logical: 0,
            device_id: "alice-dev".into(),
        };
        let placeholder_token = InviteToken {
            inviter: alice_addr,
            invitee_hint: Some(joiner_addr),
            minted_at: token_minted_at.clone(),
            expires_at: None,
            sig: [0u8; 64],
        };
        let token_payload_bytes =
            harmony_app::community_invite::canonical_invite_token_bytes(&placeholder_token)
                .expect("canonical token bytes");
        let token_sig: [u8; 64] = alice_sk.sign(&token_payload_bytes).to_bytes();
        let invite_token = InviteToken {
            inviter: alice_addr,
            invitee_hint: Some(joiner_addr),
            minted_at: token_minted_at,
            expires_at: None,
            sig: token_sig,
        };

        // Seal the epoch key to the joiner's X25519 pub.
        let sealed_epoch_key = {
            let joiner_ed25519_pub32 = joiner_sk.verifying_key().to_bytes();
            let x25519_pub = harmony_app::dm_signing::ed25519_pub_to_x25519(&joiner_ed25519_pub32)
                .expect("ed25519→x25519");
            harmony_app::dm_signing::seal_to_owner(&x25519_pub, membership_key.as_bytes())
                .expect("seal epoch key")
        };

        let invite_payload = CommunityInvitePayload {
            community_id,
            epoch_snapshot: InviteEpochSnapshot {
                epoch: 0,
                sealed_epoch_key,
                state_snapshot: MaterializedCommunityState::default(),
            },
            admin_addr: alice_addr,
            community_name: "Phase2cE2ECom".into(),
            is_invite_only: true,
            expires_at: None,
            invite_token: Some(invite_token),
            admin_bootstrap: Some(admin_bootstrap),
            admin_identity_pub: Some(alice_pub_64),
            forked_from: None,
            pre_fork_snapshot: None,
        };

        // ── 4. Alice publishes case-A pkarr record. ─────────────────────
        inv_pub.register_invite(&invite_payload).await;

        // ── 5. Build Bob's full redemption resources. ───────────────────
        let tmp = tempfile::TempDir::new().expect("tempdir");

        let (cas_op_tx, _cas_op_rx) = mpsc::channel(8);
        let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
            cas_op_tx,
            Duration::from_millis(1000),
        ));
        let community_registry = Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
            device_id: "joiner-dev".into(),
            content_store: cs,
            identity_resolver: Arc::new(TwoOwnerResolver {
                admin: alice_addr,
                admin_pub: alice_pub_64,
                joiner: joiner_addr,
                joiner_pub: joiner_pub_64,
            }),
            identity_dir: tmp.path().to_path_buf(),
            debounce_ms: DEFAULT_DEBOUNCE_MS,
            error_tx: None,
            delta_tx: None,
            self_owner: joiner_addr,
            signing_key: Arc::clone(&joiner_sk),
            crdt_state: None,
            nav_emitter: None,
        }));

        let (community_adapter_tx, _community_adapter_rx) =
            mpsc::channel::<CommunityAdapterRequest>(16);
        let (unicast_send_tx, _unicast_rx) = mpsc::channel::<UnicastSendRequest>(16);

        let dm_outbox = Arc::new(TokioMutex::new(DmOutbox::new(
            "joiner-dev".into(),
            joiner_addr,
            DeviceIdentityHash(joiner_addr.0),
            Arc::clone(&joiner_sk),
            Arc::new(joiner_identity),
        )));

        let (channel_log_adapter_tx, _channel_log_adapter_rx) =
            mpsc::unbounded_channel::<ChannelLogAdapterRequest>();
        let app = tauri::test::mock_app().handle().clone();
        let channel_log_registry = Arc::new(ChannelLogRegistry::new(ChannelLogRegistryConfig {
            adapter_request_tx: channel_log_adapter_tx,
            app,
            identity_dir: tmp.path().to_path_buf(),
            self_owner: joiner_addr,
            self_device_id: "joiner-dev".into(),
            signing_key: Arc::clone(&joiner_sk),
            engine_config: ChannelLogEngineConfig::default(),
        }));

        let crdt_state = Arc::new(TokioMutex::new(OwnerState::default()));
        let hlc_tracker = Arc::new(TokioMutex::new(BTreeMap::new()));

        // ── 6. Bob's pkarr_resolver + reachability_resolver. ────────────
        let pkarr_resolver = Arc::new(PkarrResolver::new(Arc::clone(&client)));
        let reachability_resolver = ReachabilityResolver::new();

        let invite_url = harmony_app::community_invite::encode_invite_url(&invite_payload)
            .expect("encode invite url");

        // ── 7. Poll briefly until the pkarr record is published before
        //       driving the IPC; otherwise the first resolve might race
        //       the publisher's first publish loop. (Publisher spawns
        //       the publish ASAP after register_invite returns; the
        //       MockPkarrRelay round-trip takes a few ms.)
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time")
            .as_millis() as u64;
        let epoch_id = current_epoch_id(now_ms);
        let probe_signing =
            derive_ephemeral_key(PkarrCase::Invite, &token_sig, &epoch_id.to_be_bytes());
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
            "pkarr record must appear within 5s before driving the IPC"
        );

        // ── 8. Drive Bob's IPC inner. ──────────────────────────────────
        let outcome = harmony_app::connectivity_redeem_invite_iroh_inner(
            invite_url,
            Some(Arc::clone(&pkarr_resolver)),
            Some(reachability_resolver.clone()),
            Arc::clone(&crdt_state),
            Arc::clone(&hlc_tracker),
            "joiner-dev".to_string(),
            joiner_addr,
            Arc::clone(&joiner_sk),
            Arc::clone(&community_registry),
            community_adapter_tx,
            unicast_send_tx,
            Arc::clone(&dm_outbox),
            Arc::clone(&channel_log_registry),
            None,
            |_| {}, // No-op progress sink for this test (sequence covered by
                    // connectivity_redeem_invite_iroh_emits_progress_events).
        )
        .await
        .expect("inner must Ok (it converts internal errors into outcome.status)");

        // ── 9. Load-bearing assertions: status="joined", reachability
        //      resolver seeded with Alice's record (proves seed_from_pkarr
        //      ran), community_id echoed. ────────────────────────────────
        assert_eq!(
            outcome.status, "joined",
            "Phase 2c IPC must return joined (NOT pkarr_resolved_no_handshake)"
        );
        assert_eq!(
            outcome.community_id.as_deref(),
            Some(hex::encode(community_id.0).as_str()),
            "community_id must echo Alice's invite"
        );

        let seeded = reachability_resolver.resolve(&alice_addr);
        assert!(
            !seeded.is_empty(),
            "ReachabilityResolver must contain Alice's routing record after seed_from_pkarr"
        );
        assert_eq!(
            seeded[0].iroh_node_id, alice_iroh_node_id,
            "seeded record must carry Alice's iroh_node_id"
        );
    })
    .await;

    result.expect("Phase 2c end-to-end orchestration test timed out");
}

// ── ZEB-325 Phase 2c Task 4: progress-events test ──────────────────────────
//
// Asserts that connectivity_redeem_invite_iroh_inner emits the full
// `resolving → connecting → sending → awaiting_countersig → joined`
// sequence via the `progress_sink` callback. Test-strategy choice B from
// the Task 4 plan: capture stages in a Vec via the sink rather than
// mocking the AppHandle event bus — avoids needing a Tauri event
// recorder and keeps the assertion direct.
//
// Reuses the full happy-path setup from the
// `..._completes_join_via_crdt_sync` test above; both tests drive the
// same orchestration but assert orthogonal properties.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connectivity_redeem_invite_iroh_emits_progress_events() {
    let _timeout_guard = RedeemTimeoutGuard::set("500");

    let result = tokio::time::timeout(Duration::from_secs(30), async {
        let relay = MockPkarrRelay::start().await;
        let pool = RelayPool::new(vec![relay.base_url.clone()]);
        let client = Arc::new(RelayClient::new(pool));

        let publisher = Arc::new(PkarrPublisher::new(Arc::clone(&client)));
        let _publisher_handle = Arc::clone(&publisher).spawn();

        let alice_sk_raw = ed25519_dalek::SigningKey::from_bytes(&[0xaa; 32]);
        let alice_sk = Arc::new(alice_sk_raw.clone());
        let (alice_addr, alice_pub_64) = {
            let x25519_priv = harmony_app::dm_signing::ed25519_priv_to_x25519(&alice_sk);
            let x25519_pub =
                x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(*x25519_priv));
            let ed25519_pub = alice_sk.verifying_key().to_bytes();
            let mut combined = [0u8; 64];
            combined[..32].copy_from_slice(x25519_pub.as_bytes());
            combined[32..].copy_from_slice(&ed25519_pub);
            let id = harmony_identity::Identity::from_public_bytes(&combined)
                .expect("from_public_bytes (alice)");
            (OwnerAddr(id.address_hash), combined)
        };

        let alice_iroh_node_id = [0xABu8; 32];
        let routing_blob_builder = {
            let iroh_id = alice_iroh_node_id;
            Arc::new(move || {
                let payload = ReachabilityAnnouncePayload {
                    iroh_node_id: iroh_id,
                    home_relay_url: "https://relay.test/".into(),
                    direct_addresses: vec![],
                    announced_at_ms: 1_700_000_000_000,
                    identity_signature: [0xCDu8; 64],
                };
                let mut buf = Vec::new();
                ciborium::into_writer(&payload, &mut buf).expect("encode routing_blob");
                buf
            })
        };

        let inv_pub = PkarrInvitePublisher::new(
            Arc::clone(&publisher),
            (*alice_sk).clone(),
            alice_pub_64,
            routing_blob_builder,
        );

        let joiner_identity = PrivateIdentity::from_seed(&[0xbb; 32]);
        let joiner_sk = signing_key_from_identity(&joiner_identity);
        let (joiner_addr, joiner_pub_64) = {
            let x25519_priv = harmony_app::dm_signing::ed25519_priv_to_x25519(&joiner_sk);
            let x25519_pub =
                x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(*x25519_priv));
            let ed25519_pub = joiner_sk.verifying_key().to_bytes();
            let mut combined = [0u8; 64];
            combined[..32].copy_from_slice(x25519_pub.as_bytes());
            combined[32..].copy_from_slice(&ed25519_pub);
            let id = harmony_identity::Identity::from_public_bytes(&combined)
                .expect("from_public_bytes (joiner)");
            (OwnerAddr(id.address_hash), combined)
        };

        let community_id = SpaceId([0xf6; 16]);
        let membership_key = EpochKey::new([0x42; 32]);
        let admin_bootstrap = {
            let payload = harmony_app::community_membership::EventPayload {
                id: [0x11; 16],
                community_id,
                kind: harmony_app::community_membership::MembershipEventKind::Join,
                actor: alice_addr,
                at: Hlc {
                    wall_ms: 900,
                    logical: 0,
                    device_id: "alice-dev".into(),
                },
            };
            harmony_app::community_membership::sign_event(&payload, alice_sk.as_ref())
                .expect("sign admin bootstrap")
        };

        let token_minted_at = Hlc {
            wall_ms: 950,
            logical: 0,
            device_id: "alice-dev".into(),
        };
        let placeholder_token = InviteToken {
            inviter: alice_addr,
            invitee_hint: Some(joiner_addr),
            minted_at: token_minted_at.clone(),
            expires_at: None,
            sig: [0u8; 64],
        };
        let token_payload_bytes =
            harmony_app::community_invite::canonical_invite_token_bytes(&placeholder_token)
                .expect("canonical token bytes");
        let token_sig: [u8; 64] = alice_sk.sign(&token_payload_bytes).to_bytes();
        let invite_token = InviteToken {
            inviter: alice_addr,
            invitee_hint: Some(joiner_addr),
            minted_at: token_minted_at,
            expires_at: None,
            sig: token_sig,
        };

        let sealed_epoch_key = {
            let joiner_ed25519_pub32 = joiner_sk.verifying_key().to_bytes();
            let x25519_pub = harmony_app::dm_signing::ed25519_pub_to_x25519(&joiner_ed25519_pub32)
                .expect("ed25519→x25519");
            harmony_app::dm_signing::seal_to_owner(&x25519_pub, membership_key.as_bytes())
                .expect("seal epoch key")
        };

        let invite_payload = CommunityInvitePayload {
            community_id,
            epoch_snapshot: InviteEpochSnapshot {
                epoch: 0,
                sealed_epoch_key,
                state_snapshot: MaterializedCommunityState::default(),
            },
            admin_addr: alice_addr,
            community_name: "Phase2cProgressCom".into(),
            is_invite_only: true,
            expires_at: None,
            invite_token: Some(invite_token),
            admin_bootstrap: Some(admin_bootstrap),
            admin_identity_pub: Some(alice_pub_64),
            forked_from: None,
            pre_fork_snapshot: None,
        };

        inv_pub.register_invite(&invite_payload).await;

        let tmp = tempfile::TempDir::new().expect("tempdir");

        let (cas_op_tx, _cas_op_rx) = mpsc::channel(8);
        let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
            cas_op_tx,
            Duration::from_millis(1000),
        ));
        let community_registry = Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
            device_id: "joiner-dev".into(),
            content_store: cs,
            identity_resolver: Arc::new(TwoOwnerResolver {
                admin: alice_addr,
                admin_pub: alice_pub_64,
                joiner: joiner_addr,
                joiner_pub: joiner_pub_64,
            }),
            identity_dir: tmp.path().to_path_buf(),
            debounce_ms: DEFAULT_DEBOUNCE_MS,
            error_tx: None,
            delta_tx: None,
            self_owner: joiner_addr,
            signing_key: Arc::clone(&joiner_sk),
            crdt_state: None,
            nav_emitter: None,
        }));

        let (community_adapter_tx, _community_adapter_rx) =
            mpsc::channel::<CommunityAdapterRequest>(16);
        let (unicast_send_tx, _unicast_rx) = mpsc::channel::<UnicastSendRequest>(16);

        let dm_outbox = Arc::new(TokioMutex::new(DmOutbox::new(
            "joiner-dev".into(),
            joiner_addr,
            DeviceIdentityHash(joiner_addr.0),
            Arc::clone(&joiner_sk),
            Arc::new(joiner_identity),
        )));

        let (channel_log_adapter_tx, _channel_log_adapter_rx) =
            mpsc::unbounded_channel::<ChannelLogAdapterRequest>();
        let app = tauri::test::mock_app().handle().clone();
        let channel_log_registry = Arc::new(ChannelLogRegistry::new(ChannelLogRegistryConfig {
            adapter_request_tx: channel_log_adapter_tx,
            app,
            identity_dir: tmp.path().to_path_buf(),
            self_owner: joiner_addr,
            self_device_id: "joiner-dev".into(),
            signing_key: Arc::clone(&joiner_sk),
            engine_config: ChannelLogEngineConfig::default(),
        }));

        let crdt_state = Arc::new(TokioMutex::new(OwnerState::default()));
        let hlc_tracker = Arc::new(TokioMutex::new(BTreeMap::new()));

        let pkarr_resolver = Arc::new(PkarrResolver::new(Arc::clone(&client)));
        let reachability_resolver = ReachabilityResolver::new();

        let invite_url = harmony_app::community_invite::encode_invite_url(&invite_payload)
            .expect("encode invite url");

        // Wait for the pkarr record to appear (same logic as the
        // ..._completes_join_via_crdt_sync test).
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time")
            .as_millis() as u64;
        let epoch_id = current_epoch_id(now_ms);
        let probe_signing =
            derive_ephemeral_key(PkarrCase::Invite, &token_sig, &epoch_id.to_be_bytes());
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
            "pkarr record must appear within 5s before driving the IPC"
        );

        // ── Stage capture sink: pushes every emitted stage into a Vec. ──
        let captured: Arc<std::sync::Mutex<Vec<harmony_app::RedemptionStage>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured_for_sink = Arc::clone(&captured);
        let progress_sink = move |payload: harmony_app::ResolutionProgressPayload| {
            captured_for_sink
                .lock()
                .expect("captured mutex poisoned")
                .push(payload.stage);
        };

        let outcome = harmony_app::connectivity_redeem_invite_iroh_inner(
            invite_url,
            Some(Arc::clone(&pkarr_resolver)),
            Some(reachability_resolver.clone()),
            Arc::clone(&crdt_state),
            Arc::clone(&hlc_tracker),
            "joiner-dev".to_string(),
            joiner_addr,
            Arc::clone(&joiner_sk),
            Arc::clone(&community_registry),
            community_adapter_tx,
            unicast_send_tx,
            Arc::clone(&dm_outbox),
            Arc::clone(&channel_log_registry),
            None,
            progress_sink,
        )
        .await
        .expect("inner must Ok");

        assert_eq!(outcome.status, "joined", "happy path expected");

        // Load-bearing: full 5-stage sequence in order.
        let stages = captured.lock().expect("captured mutex poisoned").clone();
        assert_eq!(
            stages,
            vec![
                harmony_app::RedemptionStage::Resolving,
                harmony_app::RedemptionStage::Connecting,
                harmony_app::RedemptionStage::Sending,
                harmony_app::RedemptionStage::AwaitingCountersig,
                harmony_app::RedemptionStage::Joined,
            ],
            "expected resolving → connecting → sending → awaiting_countersig → joined"
        );
    })
    .await;

    result.expect("Phase 2c progress-events test timed out");
}
