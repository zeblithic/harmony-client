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

use harmony_app::community_invite::{
    self, canonical_invite_token_bytes, CommunityInvitePayload, InviteToken,
};
use harmony_app::community_membership::{materialize, MemberStatus};
use harmony_app::community_state_sync::{
    CommunityRegistryConfig, CommunitySyncRegistry, IdentityResolver, DEFAULT_DEBOUNCE_MS,
};
use harmony_app::content_store::{CasOp, ContentStore, RuntimeContentStore};
use harmony_app::dm_outbox::{DmOutbox, UnicastSendRequest};
use harmony_app::event_loop::CommunityAdapterRequest;
use harmony_app::owner_state_crdt::OwnerState;
use harmony_app::owner_state_types::{DeviceIdentityHash, Hlc, OwnerAddr, OwnerDeviceEntry};
use harmony_identity::PrivateIdentity;
use std::collections::{BTreeMap, HashMap};
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

/// RAII guard for the redeem-invite timeout env var. Captures the
/// pre-existing value (if any) and restores or removes it on Drop, so a
/// panicking test cannot leak the override into subsequent tests in
/// this binary. (Cross-binary leakage is moot — each test binary is a
/// separate OS process.)
struct RedeemTimeoutGuard {
    prior: Option<std::ffi::OsString>,
}
impl RedeemTimeoutGuard {
    fn set(value: &str) -> Self {
        let prior = std::env::var_os("HARMONY_REDEEM_INVITE_TIMEOUT_MS");
        std::env::set_var("HARMONY_REDEEM_INVITE_TIMEOUT_MS", value);
        Self { prior }
    }
}
impl Drop for RedeemTimeoutGuard {
    fn drop(&mut self) {
        match self.prior.take() {
            Some(v) => std::env::set_var("HARMONY_REDEEM_INVITE_TIMEOUT_MS", v),
            None => std::env::remove_var("HARMONY_REDEEM_INVITE_TIMEOUT_MS"),
        }
    }
}

/// Full happy-path test: Bob redeems Alice's invite-only invite,
/// counter-signed Join converges on both engines, Alice's CRDT shows
/// Bob as Joined.
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
        device_id: "alice-dev".into(),
        content_store: Arc::clone(&cs_a),
        identity_resolver: Arc::clone(&resolver),
        identity_dir: dir_a.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: None,
        self_owner: alice_addr,
        signing_key: Arc::clone(&alice_sk),
    }));
    let registry_b = Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_id: "bob-dev".into(),
        content_store: Arc::clone(&cs_b),
        identity_resolver: Arc::clone(&resolver),
        identity_dir: dir_b.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: None,
        self_owner: bob_addr,
        signing_key: Arc::clone(&bob_sk),
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
            learned_at: Hlc {
                wall_ms: 50_000,
                logical: 0,
                device_id: "bob-dev".into(),
            },
        },
    );
    let crdt_b = Arc::new(TokioMutex::new(bob_owner_state));
    let tracker_b: Arc<TokioMutex<BTreeMap<String, Hlc>>> =
        Arc::new(TokioMutex::new(BTreeMap::new()));
    // Alice still needs a CRDT handle for handle_unicast's signature
    // (it takes &Arc<Mutex<OwnerState>>). Empty default state suffices —
    // handle_unicast doesn't read owner-state on the receive side; the
    // arg is plumbed for future expansion.
    let crdt_a = Arc::new(TokioMutex::new(OwnerState::default()));

    // Alice's dm_outbox carries the PrivateIdentity that handle_unicast
    // grabs to verify the InviteToken sig + countersign Bob's Join.
    let alice_dm_outbox = Arc::new(TokioMutex::new(DmOutbox::new(
        "alice-dev".into(),
        alice_addr,
        DeviceIdentityHash(alice.identity.address_hash),
        Arc::clone(&alice_sk),
        Arc::new(dup_identity(&alice)),
    )));
    // Bob's dm_outbox: redeem_invite_inner reads bob's
    // private_identity + signing_key under its lock.
    let bob_dm_outbox = Arc::new(TokioMutex::new(DmOutbox::new(
        "bob-dev".into(),
        bob_addr,
        DeviceIdentityHash(bob.identity.address_hash),
        Arc::clone(&bob_sk),
        Arc::new(dup_identity(&bob)),
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
        "alice-dev",
        100_000,
        None,
    )
    .expect("alice mint");
    let community_id = alice_minted.community_id;

    let (alice_pub_tx, mut alice_pub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (alice_sub_tx, alice_sub_rx) = mpsc::channel::<Vec<u8>>(64);
    registry_a
        .spawn_engine(
            community_id,
            alice_minted.membership_key.clone(),
            alice_addr,
            true,
            alice_pub_tx,
            alice_sub_rx,
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

    let invite_url = community_invite::encode_invite_url(&CommunityInvitePayload {
        community_id,
        membership_key: alice_minted.membership_key.clone(),
        admin_addr: alice_addr,
        community_name: "InviteOnly".into(),
        is_invite_only: true,
        expires_at: None,
        invite_token: Some(invite_token),
        admin_bootstrap: Some(alice_minted.bootstrap_join.clone()),
        admin_identity_pub: Some(alice.identity.to_public_bytes()),
    })
    .expect("encode URL");

    // Forwarder #2: drain Bob's outbound unicast → Alice's
    // handle_unicast. The forwarder strips destination_hash (we
    // already know the packet is for Alice in this test fixture; real
    // production routes via Reticulum's identity-hash-keyed link
    // layer).
    let (bob_unicast_tx, mut bob_unicast_rx) = mpsc::channel::<UnicastSendRequest>(8);
    let registry_a_for_fwd = Arc::clone(&registry_a);
    let alice_dm_for_fwd = Arc::clone(&alice_dm_outbox);
    let crdt_a_for_fwd = Arc::clone(&crdt_a);
    tokio::spawn(async move {
        while let Some(req) = bob_unicast_rx.recv().await {
            // None app handle: tests skip Tauri event emission;
            // handle_unicast logs the rejection reason at warn level
            // (the test asserts on the engine-level Inserted outcome
            // instead).
            let _ = community_invite::handle_unicast::<()>(
                &registry_a_for_fwd,
                &alice_dm_for_fwd,
                &crdt_a_for_fwd,
                req.packet,
                None,
            )
            .await;
        }
    });

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
        "bob-dev".into(),
        bob_addr,
        Arc::clone(&bob_sk),
        Arc::clone(&registry_b),
        bob_adapter_tx,
        bob_unicast_tx,
        Arc::clone(&bob_dm_outbox),
        || Ok(()),
    )
    .await;

    assert!(
        result.is_ok(),
        "invite-only redeem must succeed; got {result:?}"
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
        g.events.values().cloned().collect()
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
        g.events.values().cloned().collect()
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
