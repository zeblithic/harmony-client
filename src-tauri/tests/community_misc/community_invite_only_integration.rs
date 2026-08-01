//! Two-engine invite-only round-trip — ZEB-262 Phase 4 Task 9.
//!
//! Exercises the full invite-only redemption path:
//!   1. Alice creates invite-only community + bootstrap-Joins
//!   2. Alice generates an invite URL (with InviteToken signed by Alice)
//!   3. Bob calls `redeem_invite_inner`, which builds + sends a
//!      `CommunityInvitePacket` via the Reticulum unicast forwarder
//!   4. Forwarder routes Bob's packet → Alice's
//!      `community_invite::handle_unicast` directly (no event_loop in
//!      this test; the discriminant pre-dispatch is unit-tested in
//!      `inbound_packet::tests` — Task 9 fix-up)
//!   5. `handle_unicast` verifies the chain, counter-signs Bob's Join,
//!      inserts via `engine.insert_local_event`. Alice's engine debounces
//!      then publishes the counter-signed Join via Phase 2's state-root
//!      publish channel.
//!   6. Phase 2 publish forwarder bridges Alice's outbound publish to
//!      Bob's engine subscriber. Bob's engine merges the counter-signed
//!      Join, fires the post-Inserted hook → Bob's
//!      `pending_redemptions[event_id]` oneshot fires → Bob's
//!      `redeem_invite_inner` returns Ok.
//!   7. Both Alice and Bob have the counter-signed Join in their CRDT;
//!      Bob materializes as Joined on Alice's side.
//!
//! The test wires:
//!   - Bob's `unicast_send_tx` → Alice's `handle_unicast` (via
//!     forwarder task); then Alice's engine debounces + emits the
//!     state-root publish via Phase 2's `publisher_tx`.
//!   - Alice's publisher_tx → Bob's subscriber_rx (and vice versa) via
//!     the same Phase 2 round-trip pattern as
//!     `community_open_flow_integration::open_community_create_redeem_leave_round_trip`.

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
use harmony_app::dm_signing::{ed25519_pub_to_x25519, seal_to_owner};
use harmony_app::event_loop::CommunityAdapterRequest;
use harmony_app::owner_state_crdt::OwnerState;
use harmony_app::owner_state_types::{DeviceIdentityHash, Hlc, OwnerAddr, OwnerDeviceEntry};
use harmony_identity::PrivateIdentity;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex as TokioMutex};

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

/// Reach into `PrivateIdentity`'s ed25519 seed the same way production
/// and the open-flow integration test do (canonical 32-byte ed25519 seed
/// lives in bytes 32..64 of `to_private_bytes()`).
fn signing_key_from(identity: &PrivateIdentity) -> ed25519_dalek::SigningKey {
    let priv_bytes = identity.to_private_bytes();
    let mut secret = [0u8; 32];
    secret.copy_from_slice(&priv_bytes[32..64]);
    ed25519_dalek::SigningKey::from_bytes(&secret)
}

/// `PrivateIdentity` is `!Clone`. `from_private_bytes` round-trips the
/// bytes so two `Arc<PrivateIdentity>` (one in Alice's dm_outbox, one in
/// the Arc we keep for the InviteToken sig) point at byte-identical
/// (but distinct) instances.
fn dup_identity(src: &PrivateIdentity) -> PrivateIdentity {
    PrivateIdentity::from_private_bytes(&src.to_private_bytes())
        .expect("PrivateIdentity round-trip via to/from_private_bytes")
}

/// Process-global mutex serializing mutations to the
/// `HARMONY_REDEEM_INVITE_TIMEOUT_MS` env var across parallel tests.
///
/// CR Major (PR #106 R7): `std::env::set_var` is process-global.
/// Multiple tests can race on it under `cargo nextest`'s default
/// parallel test-thread model.  Holding this mutex for the guard's
/// entire lifetime serializes the set + test + restore sequence so
/// that no two tests overlap their env-var window.
static REDEEM_TIMEOUT_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// RAII guard for the redeem-invite timeout env var. Captures the
/// pre-existing value (if any) and restores or removes it on Drop, so a
/// panicking test cannot leak the override into subsequent tests in
/// this binary. (Cross-binary leakage is moot — each test binary is a
/// separate OS process.)
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
        std::env::set_var("HARMONY_REDEEM_INVITE_TIMEOUT_MS", value);
        Self { _lock: lock, prior }
    }
}
impl Drop for RedeemTimeoutGuard {
    fn drop(&mut self) {
        match self.prior.take() {
            Some(v) => std::env::set_var("HARMONY_REDEEM_INVITE_TIMEOUT_MS", v),
            None => std::env::remove_var("HARMONY_REDEEM_INVITE_TIMEOUT_MS"),
        }
        // _lock drops here, releasing REDEEM_TIMEOUT_ENV_LOCK.
    }
}

/// Full happy-path test: Bob redeems Alice's invite-only invite,
/// counter-signed Join converges on both engines, Alice's CRDT shows
/// Bob as Joined.
///
/// ZEB-254 Task 9 note: `handle_unicast` now accepts the PendingJoin shape
/// and inserts it into the admin's engine AS-IS. The complete round-trip
/// (PendingJoin → admin inserts → post-Inserted hook auto-emits
/// JoinCountersign → joiner becomes Joined) requires Task 10's
/// post-Inserted hook to ship the auto-emit logic. This test will be
/// re-enabled and updated in Task 10 / Task 15.
#[ignore = "ZEB-254 Task 10: admin inserts PendingJoin (Task 9 done); auto-emit JoinCountersign hook required for full round-trip"]
#[tokio::test]
async fn alice_redeems_invite_only_against_bob_admin() {
    // Short timeout: the round-trip is bounded by debounce_ms + a few
    // mpsc hops; 5s is generous. (Bob's `redeem_invite_inner` reads
    // `HARMONY_REDEEM_INVITE_TIMEOUT_MS`; default 15s would mask a
    // wedge under CI.) Guard restores any prior value on Drop.
    let _timeout_guard = RedeemTimeoutGuard::set("5000");

    let alice = PrivateIdentity::from_seed(&[0xa1; 32]);
    let bob = PrivateIdentity::from_seed(&[0xb2; 32]);
    let alice_addr = OwnerAddr(alice.identity.address_hash);
    let bob_addr = OwnerAddr(bob.identity.address_hash);
    let alice_pub = alice.identity.to_public_bytes();
    let bob_pub = bob.identity.to_public_bytes();
    let alice_sk = Arc::new(signing_key_from(&alice));
    let bob_sk = Arc::new(signing_key_from(&bob));

    let resolver: Arc<dyn IdentityResolver> = Arc::new(TwoIdentityResolver {
        a: (alice_addr, alice_pub),
        b: (bob_addr, bob_pub),
    });

    // Shared in-memory CAS — both registries route through the same
    // backing store. Mirrors the open-flow round-trip pattern.
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

    let registry_a = Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
        device_id: "alice-dev".into(),
        content_store: Arc::clone(&cs_a),
        identity_resolver: Arc::clone(&resolver),
        identity_dir: dir_a.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: None,
        self_owner: alice_addr,
        signing_key: Arc::clone(&alice_sk),
        crdt_state: None,
        nav_emitter: None,
        presence_resync_rx: None,
    }));
    // ZEB-790: Bob's single adoption floor — Bob's registry, channel-log
    // registry, and redeem_invite_inner all model ONE node (Bob) and share it.
    // (Alice's registry_a above holds its own floor — a separate node.)
    let bob_adopt_floor = harmony_app::hlc_adopt_floor::HlcAdoptFloor::new();
    let registry_b = Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
        adopt_floor: bob_adopt_floor.clone(),
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

    // Bob's owner-state CRDT and HLC tracker. Bob's redeem_invite_inner
    // mutates both. Alice's CRDT only carries Alice's bootstrap (no
    // Space row mutations in this test — handle_unicast doesn't touch
    // owner-state, only the per-community CRDT inside the engine).
    //
    // Pre-seed Bob's OwnerDeviceCache with Alice's identity hash so
    // `resolve_destinations_for_owner` (called by redeem_invite_inner
    // during the invite-only send path) finds at least one route
    // candidate. Production seeds this via the bootstrap pairing /
    // RegisterDevice flow; in this focused test we install the
    // minimum that makes the unicast send dispatch fire.
    let mut bob_owner_state = OwnerState::default();
    bob_owner_state.owner_device_cache.devices.insert(
        alice_addr,
        OwnerDeviceEntry {
            devices: vec![DeviceIdentityHash(alice.identity.address_hash)],
            device_identity_pubs: vec![Some(alice_pub)],
            device_tunnel_contacts: vec![None],
            learned_at: Hlc {
                wall_ms: 50_000,
                logical: 0,
                device_id: "bob-dev".into(),
            },
        },
    );
    let crdt_b = Arc::new(TokioMutex::new(bob_owner_state));
    let tracker_b: Arc<TokioMutex<harmony_crdt_sync::ReplayTracker<String, Hlc>>> = Arc::new(
        TokioMutex::new(harmony_crdt_sync::ReplayTracker::new("bob-dev".into())),
    );
    // Alice still needs a CRDT handle for handle_unicast's signature
    // (it takes &Arc<Mutex<OwnerState>>). Empty default state suffices —
    // handle_unicast doesn't read owner-state on the receive side; the
    // arg is plumbed for future expansion.
    // ZEB-473 (Move 1a): previously consumed by the now-removed Bob-unicast →
    // Alice-handle_unicast forwarder; kept (prefixed) for documentation.
    let _crdt_a = Arc::new(TokioMutex::new(OwnerState::default()));

    // ZEB-339: supply synthetic community_signing_key + enrollment_cert per
    // outbox. Use new_synthetic because alice_addr/bob_addr are Reticulum-
    // derived addresses (from PrivateIdentity) that don't match the cert
    // owner_id from mint_test_owner. This is intentional test scaffolding
    // (cert wiring incomplete until Task 10); new_synthetic bypasses the
    // owner_id debug_assert in DmOutbox::new.
    let alice_test_owner_invite = harmony_app::community_membership::mint_test_owner(0xD7);
    let alice_community_sk_invite = Arc::new(ed25519_dalek::SigningKey::from_bytes(
        &alice_test_owner_invite.device_key.to_bytes(),
    ));
    let alice_enrollment_invite = alice_test_owner_invite.cert;
    let bob_test_owner_invite = harmony_app::community_membership::mint_test_owner(0xD8);
    let bob_community_sk_invite = Arc::new(ed25519_dalek::SigningKey::from_bytes(
        &bob_test_owner_invite.device_key.to_bytes(),
    ));
    let bob_enrollment_invite = bob_test_owner_invite.cert;
    // Alice's dm_outbox carries the PrivateIdentity that handle_unicast
    // grabs to verify the InviteToken sig + countersign Bob's Join.
    // ZEB-473 (Move 1a): consumed only by the now-removed unicast forwarder;
    // kept (prefixed) so the dual-identity setup stays documented.
    let _alice_dm_outbox = Arc::new(TokioMutex::new(DmOutbox::new_synthetic(
        "alice-dev".into(),
        alice_addr,
        DeviceIdentityHash(alice.identity.address_hash),
        Arc::clone(&alice_sk),
        Arc::new(dup_identity(&alice)),
        alice_community_sk_invite,
        alice_enrollment_invite,
    )));
    // Bob's dm_outbox: redeem_invite_inner reads bob's
    // private_identity + signing_key under its lock.
    let bob_dm_outbox = Arc::new(TokioMutex::new(DmOutbox::new_synthetic(
        "bob-dev".into(),
        bob_addr,
        DeviceIdentityHash(bob.identity.address_hash),
        Arc::clone(&bob_sk),
        Arc::new(dup_identity(&bob)),
        bob_community_sk_invite,
        bob_enrollment_invite,
    )));

    // Bob's adapter request channel. Bob's redeem_invite_inner
    // dispatches a CommunityAdapterRequest here; this drainer task
    // captures the publisher_rx + subscriber_tx halves so the outer
    // forwarder block (below) can wire the Phase 2 publish/subscribe
    // round-trip between Alice and Bob.
    let (bob_adapter_tx, mut bob_adapter_rx) = mpsc::channel::<CommunityAdapterRequest>(8);

    // Alice creates invite-only community + spawns engine + bootstrap
    // Joins. Spawn a publisher channel + (mostly-unused) subscriber rx
    // for Alice's engine. The publisher_rx is drained by the forwarder
    // below; the subscriber_tx is held alive (kept by the adapter
    // wiring closure) so Alice's engine doesn't latch off `inbound_closed`.
    let alice_minted = harmony_app::mint_community_creation(
        "InviteOnly",
        true,
        alice_addr,
        alice_sk.as_ref(),
        // ZEB-339: synthetic cert (compile/wiring only — allowed-RED until Task 10).
        &harmony_app::community_membership::mint_test_owner(0).cert,
        Hlc {
            wall_ms: 100_000,
            logical: 0,
            device_id: "alice-dev".to_string(),
        },
    )
    .expect("alice mint");
    let community_id = alice_minted.community_id;

    let (alice_pub_tx, mut alice_pub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (alice_sub_tx, alice_sub_rx) = mpsc::channel::<Vec<u8>>(64);
    registry_a
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
    let alice_engine = registry_a
        .engine_arc(&community_id)
        .await
        .expect("alice engine");
    alice_engine
        .insert_local_event(alice_minted.bootstrap_join.clone())
        .await
        .expect("alice bootstrap insert");

    // bob_adapter_rx is held here; redeem_invite_inner will dispatch a
    // CommunityAdapterRequest when it spawns Bob's engine. The forwarder
    // block below consumes that request to wire the publish/subscribe
    // round-trip between Alice and Bob.

    // Build Alice's InviteToken (signed by Alice over the canonical
    // CBOR of (inviter, invitee_hint, minted_at)).
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
        sig: [0u8; 64], // placeholder; canonical_invite_token_bytes excludes sig
    };
    let token_payload_bytes =
        canonical_invite_token_bytes(&invite_token_unsigned).expect("canonical token bytes");
    let token_sig = alice.sign(&token_payload_bytes);
    let invite_token = InviteToken {
        inviter: alice_addr,
        invitee_hint: Some(bob_addr),
        minted_at: token_minted_at,
        expires_at: None,
        sig: token_sig,
    };

    // M3: For invite-only communities, the epoch key must be sealed to the
    // invitee's (Bob's) X25519 public key — derived from Bob's Ed25519 signing
    // key via the standard birational map. Alice seals the 32-byte epoch key
    // so Bob's `mint_redemption` can decrypt it with `open_from_owner`.
    let bob_x25519_pub = {
        let verifying_bytes = bob_sk.verifying_key().to_bytes();
        ed25519_pub_to_x25519(&verifying_bytes).expect("bob ed25519→x25519 conversion")
    };
    let sealed_epoch_key = seal_to_owner(&bob_x25519_pub, alice_minted.membership_key.as_bytes())
        .expect("seal epoch key to bob");

    let invite_url = community_invite::encode_invite_url(&CommunityInvitePayload {
        inviter_signer_certs: Vec::new(),
        community_id,
        epoch_snapshot: InviteEpochSnapshot {
            epoch: 0,
            // ZEB-369: targeted invite — sealed envelope rides in sealed_epoch_keys.
            sealed_epoch_key: Vec::new(),
            sealed_epoch_keys: vec![sealed_epoch_key],
            state_snapshot: MaterializedCommunityState::default(),
        },
        admin_addr: alice_addr,
        community_name: "InviteOnly".into(),
        is_invite_only: true,
        expires_at: None,
        invite_token: Some(invite_token),
        admin_bootstrap: Some(alice_minted.bootstrap_join.clone()),
        admin_identity_pub: Some(alice.identity.to_public_bytes()),
        forked_from: None,
        pre_fork_snapshot: None,
        inviter_enrollment: None,
        untargeted_decrypt_key: None,
    })
    .expect("encode URL");

    // ZEB-473 (Move 1a): the Bob-outbound-unicast → Alice-handle_unicast
    // forwarder was removed with the Reticulum carrier. `redeem_invite_inner`
    // no longer fans a PendingJoin unicast packet out; Bob's PendingJoin reaches
    // Alice via the CRDT state-root publish forwarder wired below (Bob's
    // publisher_rx → Alice's sub_tx), which is the production delivery path.

    // ZEB-260 (Case A FIXED): admin's signed bootstrap is now plumbed
    // through the invite URL (CommunityInvitePayload.admin_bootstrap +
    // admin_identity_pub). redeem_invite_inner verifies and inserts it
    // into Bob's engine BEFORE sending the unicast packet, so by the
    // time Alice's publish-back arrives, Bob's CRDT already has Alice
    // as Joined and the membership-at-HLC gate admits.
    //
    // The OOB pre-seed (pre-spawn Bob's engine + insert_local_event)
    // has been removed — its presence would mask regressions in the
    // production path. redeem_invite_inner now dispatches a
    // CommunityAdapterRequest when it spawns Bob's engine; the
    // forwarder below consumes that request and wires Bob's
    // publisher_rx → Alice's sub_tx, and Alice's pub_rx → Bob's
    // subscriber_tx, mirroring community_open_flow_integration.rs.
    let alice_sub_tx_for_fwd = alice_sub_tx.clone();
    tokio::spawn(async move {
        // Wait for redeem_invite_inner's adapter dispatch.
        if let Some(req) = bob_adapter_rx.recv().await {
            // Bob → Alice: drain Bob's publisher_rx, send to Alice's sub_tx.
            let mut bob_pub_rx = req.publisher_rx;
            let alice_sub_tx = alice_sub_tx_for_fwd.clone();
            tokio::spawn(async move {
                while let Some(bytes) = bob_pub_rx.recv().await {
                    if alice_sub_tx.send(bytes).await.is_err() {
                        break;
                    }
                }
            });
            // Alice → Bob: drain alice_pub_rx, send to Bob's subscriber_tx.
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

    // ZEB-271: ChannelLogRegistry required by the new redeem_invite_inner
    // signature. Build a minimal instance with a dummy adapter bridge for
    // Bob's side. No Zenoh drainer is needed — sends succeed (receiver alive)
    // but the request just queues; harmless since the channel-log is not the
    // focus of this integration test.
    let (bob_channel_log_adapter_tx, _bob_channel_log_adapter_rx) =
        mpsc::unbounded_channel::<harmony_app::event_loop::ChannelLogAdapterRequest>();
    // ZEB-445: registry takes a mode-agnostic NodeEventSink; this test never
    // asserts on channel-log emissions, so an empty fan-out is sufficient.
    let bob_channel_log_registry = ChannelLogRegistry::new(ChannelLogRegistryConfig {
        adapter_request_tx: bob_channel_log_adapter_tx,
        sink: Arc::new(harmony_app::node_event_sink::FanoutSink(vec![])),
        identity_dir: dir_b.path().to_path_buf(),
        self_owner: bob_addr,
        self_device_id: "bob-dev".into(),
        signing_key: Arc::clone(&bob_sk),
        adopt_floor: bob_adopt_floor.clone(),
        engine_config: ChannelLogEngineConfig::default(),
        transport_epoch_rx: None,
        // ZEB-599 Direction 1: no presence watch in this integration harness.
        presence_resync_rx: None,
    });

    // Bob redeems. redeem_invite_inner spawns Bob's engine (fresh, no
    // pre-spawn), inserts alice's bootstrap_join from the invite URL
    // into Bob's engine, sends the unicast packet to Alice, Alice
    // counter-signs + inserts, Alice's engine publishes, Bob's engine
    // receives via the forwarder wired above, merges, fires the
    // pending_redemptions oneshot.
    let result = harmony_app::redeem_invite_inner(
        invite_url,
        Arc::clone(&crdt_b),
        Arc::clone(&tracker_b),
        bob_adopt_floor.clone(),
        "bob-dev".into(),
        bob_addr,
        Arc::clone(&bob_sk),
        // ZEB-339: synthetic cert (compile/wiring only — allowed-RED until Task 10).
        harmony_app::community_membership::mint_test_owner(0).cert,
        Arc::clone(&registry_b),
        bob_adapter_tx,
        None, // ZEB-434: no transport-epoch watch in this test
        Arc::clone(&bob_dm_outbox),
        bob_channel_log_registry,
        || Ok(()),
        None, // ZEB-325: invite-only integration test uses Reticulum-required semantics.
    )
    .await;

    let dto = result.expect("invite-only redeem must succeed");
    // ZEB-265: redeem_invite returns the invite's community_name + kind
    // so the frontend can render a real name (no more
    // `Community ${id.slice(0,8)}` placeholder) without re-decoding the
    // URL.
    assert_eq!(dto.community_id, hex::encode(community_id.0));
    assert_eq!(dto.community_name, "InviteOnly");
    assert!(dto.is_invite_only);
    // ZEB-254 Task 8: counter-sign came back within 5s (fast-path hit)
    // so pending must be false.
    assert!(
        !dto.pending,
        "counter-sign landed in time; pending must be false"
    );

    // Alice's engine has admin Join + counter-signed Bob Join. Bob
    // materializes as Joined on Alice's side (counter-sig completes
    // the invite-only authorization gate).
    let alice_state = registry_a
        .state_for(&community_id)
        .await
        .expect("alice state");
    let alice_events: Vec<_> = {
        let g = alice_state.lock().await;
        g.events().cloned().collect()
    };
    assert_eq!(
        alice_events.len(),
        2,
        "alice should hold admin Join + counter-signed Bob Join"
    );
    let mat_a = materialize(&alice_events, alice_addr);
    assert_eq!(
        mat_a.members.get(&bob_addr).map(|m| m.status),
        Some(MemberStatus::Joined),
        "Bob must materialize as Joined on Alice's side"
    );
    assert_eq!(
        mat_a.members.get(&alice_addr).map(|m| m.status),
        Some(MemberStatus::Joined),
        "Alice must remain Joined"
    );
    // ZEB-260: Bob's CRDT now holds admin's bootstrap (inserted by
    // redeem_invite_inner from the invite URL's admin_bootstrap field)
    // AND his own counter-signed Join (merged from Alice's publish-back).
    let bob_state = registry_b
        .state_for(&community_id)
        .await
        .expect("bob state");
    let bob_events: Vec<_> = {
        let g = bob_state.lock().await;
        g.events().cloned().collect()
    };
    assert_eq!(
        bob_events.len(),
        2,
        "bob should hold admin Join + his counter-signed Join after redeem"
    );
    let mat_b = materialize(&bob_events, alice_addr);
    assert_eq!(
        mat_b.members.get(&alice_addr).map(|m| m.status),
        Some(MemberStatus::Joined),
        "Alice must be Joined in Bob's view (admin bootstrap landed)"
    );
    assert_eq!(
        mat_b.members.get(&bob_addr).map(|m| m.status),
        Some(MemberStatus::Joined),
        "Bob must be Joined in Bob's view (publish-back merged)"
    );
    // RedeemTimeoutGuard restores the prior env-var on Drop here.
}

/// ZEB-260 tampering test: an invite URL whose admin_bootstrap has been
/// modified post-mint (signature flipped) MUST fail at the verify chain
/// before any engine spawn / unicast / commit. The error must surface as
/// `BootstrapSignatureInvalid` (the chain-step → variant mapping is part
/// of the security contract — frontend telemetry keys off these tags).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn community_invite_only_tampered_admin_bootstrap_rejects() {
    use harmony_app::community_invite::{
        decode_invite_url, encode_invite_url, verify_admin_bootstrap, RedeemBootstrapVerifyError,
    };
    use harmony_app::community_membership::{sign_event, EventPayload, MembershipEventKind};
    use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};

    // ZEB-339: build alice's bootstrap from a minted owner so it carries a
    // valid Master EnrollmentCert bound to her device key. This makes the
    // event pass encode_invite_url's embedded-cert gate AND lets
    // verify_admin_bootstrap resolve the signer via enrolled_key_from_cert,
    // so the tampered signature surfaces as BootstrapSignatureInvalid for the
    // RIGHT reason (sig mismatch under the enrolled key) rather than tripping
    // an earlier gate.
    let alice_owner = harmony_app::community_membership::mint_test_owner(0x5A);
    let alice_addr = alice_owner.owner;
    let alice_sk = alice_owner.device_key.clone();

    let bob_identity = PrivateIdentity::from_seed(&[0xBB; 32]);
    let bob_addr = OwnerAddr(bob_identity.identity.address_hash);

    let community_id = SpaceId([0x37; 16]);
    let bootstrap_payload = EventPayload {
        id: [0xCC; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: alice_addr,
        at: Hlc {
            wall_ms: 1_700_000_000_000,
            logical: 0,
            device_id: "alice-dev".into(),
        },
    };
    let mut alice_bootstrap = sign_event(&bootstrap_payload, &alice_sk).expect("sign");
    // Embed alice's enrollment cert so the bootstrap clears the ZEB-339 gate;
    // verification then fails on the tampered signature, not a missing cert.
    alice_bootstrap.enrollment = Some(alice_owner.cert.clone());
    // Tamper: flip a single bit in the signature.
    alice_bootstrap.sig[0] ^= 0x01;

    // sealed_epoch_key must be 92 bytes for invite-only (32 ephemeral_pub +
    // 12 nonce + 32 ct + 16 tag). The exact value is irrelevant for this
    // test — we're asserting on the tampered admin_bootstrap.sig rejection,
    // not the key content. Use a valid-length vector of 0xDD bytes.
    // ZEB-249 PR #106 R5: encode_invite_url now enforces this length.
    let invite_url = encode_invite_url(&CommunityInvitePayload {
        inviter_signer_certs: Vec::new(),
        community_id,
        epoch_snapshot: InviteEpochSnapshot {
            epoch: 0,
            // ZEB-369: targeted invite (invitee_hint = Some) — the envelope rides
            // in sealed_epoch_keys (one ≥92-byte entry) with sealed_epoch_key empty.
            sealed_epoch_key: Vec::new(),
            sealed_epoch_keys: vec![vec![0xDD; 92]],
            state_snapshot: MaterializedCommunityState::default(),
        },
        admin_addr: alice_addr,
        community_name: "TamperedTest".into(),
        is_invite_only: true,
        expires_at: None,
        invite_token: Some(InviteToken {
            inviter: alice_addr,
            invitee_hint: Some(bob_addr),
            minted_at: bootstrap_payload.at.clone(),
            expires_at: None,
            sig: [0xEE; 64],
        }),
        admin_bootstrap: Some(alice_bootstrap),
        // admin_identity_pub is ignored by post-ZEB-339 verify_admin_bootstrap
        // (which uses the cert's device key exclusively); only presence matters
        // for the encode gate.
        admin_identity_pub: Some([0u8; 64]),
        forked_from: None,
        pre_fork_snapshot: None,
        // ZEB-339: encode_invite_url requires invite-only payloads to carry the
        // inviter's EnrollmentCert. Its content is irrelevant to this test (we
        // assert on the tampered admin_bootstrap.sig rejection), so any valid
        // cert satisfies the presence check.
        inviter_enrollment: Some(harmony_app::community_membership::mint_test_owner(0xAA).cert),
        untargeted_decrypt_key: None,
    })
    .expect("encode URL");

    // Decode + verify directly. verify_admin_bootstrap is pure, so we
    // can pin the error variant + telemetry tag without spinning up
    // engines, channels, or CRDT state.
    let decoded = decode_invite_url(&invite_url).expect("decode");
    let err = verify_admin_bootstrap(&decoded).expect_err("tampered must reject");
    assert_eq!(
        err,
        RedeemBootstrapVerifyError::BootstrapSignatureInvalid,
        "tampered admin_bootstrap.sig must surface as BootstrapSignatureInvalid"
    );
    // Telemetry tag is part of the security contract — pin it.
    assert_eq!(err.reason_tag(), "bootstrap_signature_invalid");
}
