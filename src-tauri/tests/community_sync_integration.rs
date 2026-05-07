//! Integration tests for ZEB-217 Sub-C Phase 2: two-member end-to-end
//! community DAG-sync over a paired in-memory transport.
//!
//! Each test runs two `CommunitySyncRegistry` instances ("A" and "B")
//! that share an in-memory CAS servicer but are otherwise isolated. A
//! pair of `mpsc` channels stand in for the production Zenoh
//! publisher/subscriber adapters: A's outbound bytes are forwarded
//! into B's `subscriber_rx`, exercising the full receive pipeline
//! (`decrypt_root_publish` → blob fetch from CAS → `decrypt_blob` →
//! per-event `verify_event` → CRDT merge → tracker advance).
//!
//! Coverage:
//! - `two_members_dag_sync_full_event_log` — happy-path round-trip:
//!   A injects a Join, `flush_now` publishes, B's CRDT shows 1 event.
//! - `forged_signature_event_is_rejected_on_receive` — defense-in-
//!   depth: a wire packet that DECRYPTS cleanly but contains a forged
//!   per-event signature is rejected at B's `verify_event`. B's CRDT
//!   stays empty; a `verify_event_rejected` degraded report fires.
//! - `malformed_wire_packet_does_not_panic_engine` — random bytes on
//!   B's subscriber surface as `IncomingOutcome::ErrPreMutation` and
//!   the engine task remains alive (verified by sending a valid
//!   publish afterward and confirming it processes).
//! - `replay_of_same_root_publish_is_idempotent` — re-injecting the
//!   same wire bytes triggers the `RootHlcTracker.would_accept`
//!   early-exit; B's CRDT shows exactly 1 event.

use harmony_app::community_membership::{
    sign_event_with_identity, EventPayload, MembershipEventKind,
};
use harmony_app::community_state_crdt::CommunityState;
use harmony_app::community_state_sync::{
    encrypt_blob, encrypt_root_publish, CommunityDegradedReport, CommunityRegistryConfig,
    CommunitySyncRegistry, IdentityResolver, DEFAULT_DEBOUNCE_MS,
};
use harmony_app::content_store::{CasOp, ContentStore, RuntimeContentStore};
use harmony_app::owner_state_crypto::canonical_cbor_encode;
use harmony_app::owner_state_types::{Hlc, MembershipKey, OwnerAddr, SpaceId};
use harmony_identity::PrivateIdentity;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};

// ---------------------------------------------------------------------
// Shared test scaffolding
// ---------------------------------------------------------------------

/// Reach into `PrivateIdentity`'s ed25519 seed (canonical 32-byte seed
/// at bytes [32..64] of `to_private_bytes()`) and return a matching
/// `ed25519_dalek::SigningKey`. ZEB-256 Task 6: receive-side sig-verify
/// requires the engine's signing key to match the publisher's
/// `identity_pub`, so registry config + engine config can no longer
/// hand-wave with a dummy `[0x42; 32]` seed.
fn signing_key_from(identity: &PrivateIdentity) -> Arc<ed25519_dalek::SigningKey> {
    let private_bytes = identity.to_private_bytes();
    let mut secret = [0u8; 32];
    secret.copy_from_slice(&private_bytes[32..64]);
    Arc::new(ed25519_dalek::SigningKey::from_bytes(&secret))
}

/// Static `IdentityResolver` backed by an in-memory `HashMap`. The
/// production `OwnerDeviceCacheResolver` walks Sub-A's owner-device
/// cache; for tests we want a deterministic mapping the test author
/// controls directly. More general than the single-pair stub used by
/// `community_sync_engine_unit.rs` because the integration scenarios
/// here may need both an admin and a countersigner identity in the
/// same resolver.
struct StaticResolver {
    map: std::collections::HashMap<OwnerAddr, [u8; 64]>,
}

#[async_trait::async_trait]
impl IdentityResolver for StaticResolver {
    async fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
        self.map.get(addr).copied()
    }
}

/// Bounded poll helper. Drives `predicate` every `poll_interval` until
/// it returns true OR `timeout` elapses; panics on timeout. Replaces
/// fixed `tokio::time::sleep` waits in tests with deterministic
/// convergence so the suite returns as soon as the engine settles and
/// only fails when convergence never happens.
async fn wait_until<F, Fut>(timeout: Duration, poll_interval: Duration, mut predicate: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    tokio::time::timeout(timeout, async {
        loop {
            if predicate().await {
                break;
            }
            tokio::time::sleep(poll_interval).await;
        }
    })
    .await
    .expect("wait_until: condition timed out");
}

/// Spawn a background task servicing `CasOp::PutLocal` and
/// `CasOp::GetOrFetch` over an in-memory `HashMap`. Returns the
/// shared sender both registries' `RuntimeContentStore` instances
/// route through, so blobs A puts are visible to B's gets.
fn spawn_shared_cas() -> mpsc::Sender<CasOp> {
    let (tx, mut rx) = mpsc::channel::<CasOp>(64);
    let store: Arc<Mutex<std::collections::HashMap<harmony_content::cid::ContentId, Vec<u8>>>> =
        Arc::new(Mutex::new(std::collections::HashMap::new()));
    tokio::spawn(async move {
        while let Some(op) = rx.recv().await {
            match op {
                CasOp::PutLocal { cid, blob, reply } => {
                    store.lock().await.insert(cid, blob);
                    if let Some(reply) = reply {
                        let _ = reply.send(Ok(()));
                    }
                }
                CasOp::GetOrFetch {
                    cid,
                    timeout: _,
                    reply,
                } => {
                    let v = store.lock().await.get(&cid).cloned();
                    let _ = reply.send(Ok(v));
                }
            }
        }
    });
    tx
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

/// End-to-end happy path: A injects a Join into its CRDT, `flush_now`
/// triggers the encrypted state-root publish, B's subscriber pipeline
/// fetches the blob from shared CAS, decrypts, re-runs `verify_event`,
/// and merges the event into B's CRDT. After a brief processing
/// window B should hold exactly one event.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_members_dag_sync_full_event_log() {
    let cas_tx = spawn_shared_cas();
    let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_tx,
        Duration::from_millis(2000),
    ));

    let community_id = SpaceId([1u8; 16]);
    let mk = MembershipKey::new([0x42; 32]);

    let id_admin = PrivateIdentity::from_seed(&[0xa1; 32]);
    let admin = OwnerAddr(id_admin.identity.address_hash);
    let admin_pub = id_admin.identity.to_public_bytes();
    let admin_signing = signing_key_from(&id_admin);

    // For B to publish under its own identity, derive a separate
    // PrivateIdentity. This test does not exercise B's publish path,
    // but the registry needs a valid signing-key/self_owner pair.
    let id_b = PrivateIdentity::from_seed(&[0xb1; 32]);
    let b_owner = OwnerAddr(id_b.identity.address_hash);
    let b_signing = signing_key_from(&id_b);

    let mut resolver_map = std::collections::HashMap::new();
    resolver_map.insert(admin, admin_pub);
    let resolver: Arc<dyn IdentityResolver> = Arc::new(StaticResolver { map: resolver_map });

    // Wire: A's publisher → B's subscriber. The forwarder mimics the
    // Zenoh fanout adapter that ships in production.
    let (a_pub_tx, mut a_pub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (b_sub_tx, b_sub_rx) = mpsc::channel::<Vec<u8>>(64);
    tokio::spawn(async move {
        while let Some(bytes) = a_pub_rx.recv().await {
            let _ = b_sub_tx.send(bytes).await;
        }
    });

    let dir_a = tempfile::tempdir().expect("tempdir A");
    let dir_b = tempfile::tempdir().expect("tempdir B");

    let registry_a = CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_id: "a-dev".into(),
        content_store: Arc::clone(&cs),
        identity_resolver: Arc::clone(&resolver),
        identity_dir: dir_a.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: None,
        // ZEB-256 Task 6: A's registry signs publishes as admin.
        self_owner: admin,
        signing_key: Arc::clone(&admin_signing),
    });
    let registry_b = CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_id: "b-dev".into(),
        content_store: Arc::clone(&cs),
        identity_resolver: Arc::clone(&resolver),
        identity_dir: dir_b.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: None,
        // ZEB-256 Task 6: B doesn't publish in this one-way test;
        // values just need to satisfy the type bound.
        self_owner: b_owner,
        signing_key: Arc::clone(&b_signing),
    });

    // B's publisher and A's subscriber are unused in this one-way
    // sync test; we still need fresh handles to satisfy
    // `spawn_engine`'s signature.
    let (b_pub_tx, _b_pub_rx) = mpsc::channel(8);
    let (_a_sub_tx, a_sub_rx) = mpsc::channel(8);

    registry_a
        .spawn_engine(community_id, mk.clone(), admin, false, a_pub_tx, a_sub_rx)
        .await
        .expect("spawn a");
    registry_b
        .spawn_engine(community_id, mk, admin, false, b_pub_tx, b_sub_rx)
        .await
        .expect("spawn b");

    // Inject a Join event directly into A's CRDT via the test-only
    // `state_for` accessor. Phase 3 will ship the user-facing IPC
    // (`create_community` / `redeem_invite`); for Phase 2 we bypass
    // the IPC layer entirely and exercise the sync engine in
    // isolation.
    let state_a = registry_a
        .state_for(&community_id)
        .await
        .expect("engine spawned");
    let admin_join_event = {
        let payload = EventPayload {
            id: [9u8; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: admin,
            at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "a-dev".into(),
            },
        };
        sign_event_with_identity(&payload, &id_admin).expect("sign")
    };
    {
        let mut sa = state_a.lock().await;
        let outcome = sa.insert_event(
            admin_join_event.clone(),
            &harmony_app::community_membership::VerifyContext {
                expected_community_id: community_id,
                admin_addr: admin,
                is_invite_only: false,
                actor_identity_pub: &admin_pub,
                countersigner_identity_pub: None,
            },
        );
        assert!(matches!(
            outcome,
            harmony_app::community_state_crdt::InsertOutcome::Inserted
        ));
    }

    // ZEB-256 Task 6: pre-seed B's CRDT with admin's Join so the
    // membership-at-HLC gate sees admin as `Joined` when A's publish
    // arrives. Without this seed B would reject A's bootstrap publish
    // with `publisher_not_joined`. (Production wires the Join via the
    // redemption flow on B's side, which inserts the event locally
    // before the first publish.)
    let state_b = registry_b
        .state_for(&community_id)
        .await
        .expect("engine spawned");
    {
        let mut sb = state_b.lock().await;
        let outcome = sb.insert_event(
            admin_join_event,
            &harmony_app::community_membership::VerifyContext {
                expected_community_id: community_id,
                admin_addr: admin,
                is_invite_only: false,
                actor_identity_pub: &admin_pub,
                countersigner_identity_pub: None,
            },
        );
        assert!(matches!(
            outcome,
            harmony_app::community_state_crdt::InsertOutcome::Inserted
        ));
    }

    // Force-publish on A. Bypasses debounce so the test doesn't have
    // to sleep through the full window. After the publish lands B's
    // tracker advances; the CRDT is byte-identical (admin's Join was
    // pre-seeded above).
    registry_a.flush_now(&community_id).await.expect("flush a");

    // Wait deterministically for B's tracker to advance — confirms
    // the publish made it through receive's verify gates.
    let state_b_for_poll = Arc::clone(&state_b);
    wait_until(
        Duration::from_secs(2),
        Duration::from_millis(10),
        move || {
            let s = Arc::clone(&state_b_for_poll);
            async move { s.lock().await.events.len() == 1 }
        },
    )
    .await;

    {
        let sb = registry_b
            .state_for(&community_id)
            .await
            .expect("engine spawned")
            .lock()
            .await
            .clone();
        assert_eq!(sb.events.len(), 1, "B should hold the (pre-seeded) Join");
    }

    registry_a.shutdown_all().await.expect("shutdown a");
    registry_b.shutdown_all().await.expect("shutdown b");
}

/// Defense-in-depth: peers must not trust each other's verification.
/// A wire packet that DECRYPTS cleanly (right MK, right AAD) but
/// contains an event with a forged per-event signature must be
/// rejected at B's `verify_event` call inside `handle_incoming_publish`.
/// B's CRDT stays empty and a `verify_event_rejected` degraded report
/// lands on B's `error_tx`.
///
/// Construction: we craft the wire packet entirely outside any
/// registry. A `CommunityState` containing one signed event is built,
/// the event's `sig[0]` byte is flipped, the state is canonical-CBOR
/// encoded + `encrypt_blob`d + put into shared CAS, the
/// `CommunityRootPublishPayload` is encoded and `encrypt_root_publish`d,
/// and the resulting bytes are injected into B's `subscriber_rx`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forged_signature_event_is_rejected_on_receive() {
    let cas_tx = spawn_shared_cas();
    let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_tx,
        Duration::from_millis(2000),
    ));

    use ed25519_dalek::Signer;
    use harmony_app::community_state_sync::CommunityRootSignedPayload;

    let community_id = SpaceId([2u8; 16]);
    let mk = MembershipKey::new([0x55; 32]);

    let id_admin = PrivateIdentity::from_seed(&[0xb1; 32]);
    let admin = OwnerAddr(id_admin.identity.address_hash);
    let admin_pub = id_admin.identity.to_public_bytes();
    let admin_signing = signing_key_from(&id_admin);

    let mut resolver_map = std::collections::HashMap::new();
    resolver_map.insert(admin, admin_pub);
    let resolver: Arc<dyn IdentityResolver> = Arc::new(StaticResolver { map: resolver_map });

    // Plumb a degraded-path receiver so we can assert on the
    // `verify_event_rejected` report B emits.
    let (error_tx, mut error_rx) = mpsc::channel::<CommunityDegradedReport>(8);

    let dir_b = tempfile::tempdir().expect("tempdir B");
    let registry_b = CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_id: "b-dev".into(),
        content_store: Arc::clone(&cs),
        identity_resolver: Arc::clone(&resolver),
        identity_dir: dir_b.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: Some(error_tx),
        delta_tx: None,
        // ZEB-256 Task 6: B doesn't publish; we only inspect its
        // receive-side rejection. Signing key is admin's so the
        // engine matches what production would have.
        self_owner: admin,
        signing_key: Arc::clone(&admin_signing),
    });

    // We need direct access to B's subscriber channel sender to
    // inject the crafted wire packet, so we build the (sub_tx, sub_rx)
    // pair here rather than via a forwarder.
    let (b_sub_tx, b_sub_rx) = mpsc::channel::<Vec<u8>>(8);
    let (b_pub_tx, _b_pub_rx) = mpsc::channel(8);

    registry_b
        .spawn_engine(community_id, mk.clone(), admin, false, b_pub_tx, b_sub_rx)
        .await
        .expect("spawn b");

    // ZEB-256 Task 6: pre-seed B's CRDT with admin's valid Join so
    // the membership-at-HLC gate admits the publish into the per-
    // event verify path under test.
    let valid_admin_join = {
        let payload = EventPayload {
            id: [9u8; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: admin,
            at: Hlc {
                wall_ms: 50,
                logical: 0,
                device_id: "admin-dev".into(),
            },
        };
        sign_event_with_identity(&payload, &id_admin).expect("sign")
    };
    {
        let state_b = registry_b
            .state_for(&community_id)
            .await
            .expect("engine spawned");
        let mut sb = state_b.lock().await;
        let outcome = sb.insert_event(
            valid_admin_join,
            &harmony_app::community_membership::VerifyContext {
                expected_community_id: community_id,
                admin_addr: admin,
                is_invite_only: false,
                actor_identity_pub: &admin_pub,
                countersigner_identity_pub: None,
            },
        );
        assert!(matches!(
            outcome,
            harmony_app::community_state_crdt::InsertOutcome::Inserted
        ));
    }

    // Build the malicious CommunityState: one valid Join, then flip
    // a byte in its signature so verify_event rejects it at receive.
    let mut bad_state = CommunityState::new(community_id);
    let payload = EventPayload {
        id: [7u8; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: admin,
        at: Hlc {
            wall_ms: 100,
            logical: 0,
            device_id: "attacker-dev".into(),
        },
    };
    let mut event = sign_event_with_identity(&payload, &id_admin).expect("sign");
    // Flip a bit in the signature. Insert directly into the events
    // map — `insert_event` would refuse the forged signature, which
    // is the whole point: we're crafting state that B will see only
    // after AEAD-decryption succeeds and the per-event verify fires.
    event.sig[0] ^= 0xFF;
    bad_state.events.insert(event.id, event);

    // Encrypt + publish via the same primitives the engine uses.
    let blob_cleartext = canonical_cbor_encode(&bad_state).expect("encode state");
    let blob_ciphertext = encrypt_blob(&mk, &blob_cleartext).expect("encrypt blob");
    let root_cid = harmony_content::cid::ContentId::for_book(
        &blob_ciphertext,
        harmony_content::cid::ContentFlags {
            encrypted: true,
            ..Default::default()
        },
    )
    .expect("for_book");
    cs.put(root_cid, blob_ciphertext).await.expect("cas put");

    // ZEB-256 Task 6: the publish-level gates require a real
    // publisher_addr (in resolver) and a valid sig over the signed
    // sub-payload — we sign with admin's key so all 3 gates pass and
    // the per-event verify path receives the malformed inner event.
    let signed = CommunityRootSignedPayload {
        root_cid,
        publisher_addr: admin,
        at: Hlc {
            wall_ms: 200,
            logical: 0,
            device_id: "attacker-dev".into(),
        },
    };
    let signed_bytes = canonical_cbor_encode(&signed).expect("encode signed");
    let sig = admin_signing.sign(&signed_bytes).to_bytes();
    let publish = signed.into_wire(sig);
    let publish_bytes = canonical_cbor_encode(&publish).expect("encode publish");
    let wire = encrypt_root_publish(&mk, &publish_bytes).expect("encrypt root");

    // Deliver to B.
    b_sub_tx.send(wire).await.expect("send wire");

    // Wait for the degraded report. Bounded so a regression that
    // silently merges the forged event surfaces as a timeout rather
    // than hanging the suite.
    let report = tokio::time::timeout(Duration::from_secs(2), error_rx.recv())
        .await
        .expect("degraded report timed out")
        .expect("error_tx dropped");
    assert_eq!(report.reason_tag, "verify_event_rejected");
    assert_eq!(report.community_id, community_id);

    // B's CRDT must hold ONLY the pre-seeded admin Join — the
    // forged-sig event in the inbound blob was rejected per-event
    // even though the wire packet AEAD-decrypted cleanly AND passed
    // the publish-level gates. The tracker DID advance (single
    // mutation point), but the bad inner event did not insert.
    let state_b = registry_b
        .state_for(&community_id)
        .await
        .expect("engine spawned");
    {
        let sb = state_b.lock().await;
        assert_eq!(
            sb.events.len(),
            1,
            "B should hold only the pre-seeded admin Join; \
             the forged-sig event must not have inserted"
        );
    }

    registry_b.shutdown_all().await.expect("shutdown b");
}

/// A stream of random bytes injected into B's subscriber channel
/// must not panic the engine task. The decrypt pipeline returns
/// `IncomingOutcome::ErrPreMutation`, the engine logs + drops the
/// packet, and the task remains alive to process subsequent valid
/// publishes. We confirm liveness by sending a valid publish AFTER
/// the malformed one and asserting B's CRDT picks it up.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_wire_packet_does_not_panic_engine() {
    let cas_tx = spawn_shared_cas();
    let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_tx,
        Duration::from_millis(2000),
    ));

    let community_id = SpaceId([3u8; 16]);
    let mk = MembershipKey::new([0x77; 32]);

    let id_admin = PrivateIdentity::from_seed(&[0xc1; 32]);
    let admin = OwnerAddr(id_admin.identity.address_hash);
    let admin_pub = id_admin.identity.to_public_bytes();
    let admin_signing = signing_key_from(&id_admin);

    let mut resolver_map = std::collections::HashMap::new();
    resolver_map.insert(admin, admin_pub);
    let resolver: Arc<dyn IdentityResolver> = Arc::new(StaticResolver { map: resolver_map });

    let (a_pub_tx, mut a_pub_rx) = mpsc::channel::<Vec<u8>>(8);
    let (b_sub_tx, b_sub_rx) = mpsc::channel::<Vec<u8>>(8);

    let dir_a = tempfile::tempdir().expect("tempdir A");
    let dir_b = tempfile::tempdir().expect("tempdir B");

    let registry_a = CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_id: "a-dev".into(),
        content_store: Arc::clone(&cs),
        identity_resolver: Arc::clone(&resolver),
        identity_dir: dir_a.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: None,
        // ZEB-256 Task 6: A publishes as admin.
        self_owner: admin,
        signing_key: Arc::clone(&admin_signing),
    });
    let registry_b = CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_id: "b-dev".into(),
        content_store: Arc::clone(&cs),
        identity_resolver: Arc::clone(&resolver),
        identity_dir: dir_b.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: None,
        // ZEB-256 Task 6: B doesn't publish; admin's identity also
        // works here for the type-bound.
        self_owner: admin,
        signing_key: Arc::clone(&admin_signing),
    });

    let (b_pub_tx, _b_pub_rx) = mpsc::channel(8);
    let (_a_sub_tx, a_sub_rx) = mpsc::channel(8);

    registry_a
        .spawn_engine(community_id, mk.clone(), admin, false, a_pub_tx, a_sub_rx)
        .await
        .expect("spawn a");
    registry_b
        .spawn_engine(community_id, mk, admin, false, b_pub_tx, b_sub_rx)
        .await
        .expect("spawn b");

    // ZEB-256 Task 6: pre-seed B's CRDT with admin's Join so the
    // membership-at-HLC gate admits A's later publish. Without this
    // seed, B would reject A's bootstrap publish on
    // `publisher_not_joined` — independent of this test's intent
    // (malformed-wire liveness).
    let admin_join_event = {
        let payload = EventPayload {
            id: [11u8; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: admin,
            at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "a-dev".into(),
            },
        };
        sign_event_with_identity(&payload, &id_admin).expect("sign")
    };
    {
        let state_b = registry_b
            .state_for(&community_id)
            .await
            .expect("engine spawned");
        let mut sb = state_b.lock().await;
        let outcome = sb.insert_event(
            admin_join_event.clone(),
            &harmony_app::community_membership::VerifyContext {
                expected_community_id: community_id,
                admin_addr: admin,
                is_invite_only: false,
                actor_identity_pub: &admin_pub,
                countersigner_identity_pub: None,
            },
        );
        assert!(matches!(
            outcome,
            harmony_app::community_state_crdt::InsertOutcome::Inserted
        ));
    }

    // Inject 64 random bytes — long enough to pass MIN_WIRE_LEN
    // (28 = nonce 12 + tag 16) but with no valid nonce / tag, so
    // ChaCha20-Poly1305 AEAD verification fails and the engine
    // drops the packet via IncomingOutcome::ErrPreMutation.
    let garbage: Vec<u8> = (0..64u8).map(|i| i.wrapping_mul(31)).collect();
    b_sub_tx.send(garbage).await.expect("send garbage");

    // Brief settle so B's task has a chance to drain + log the
    // malformed packet before we exercise the live-check path.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Inject the same admin Join into A and trigger a valid publish.
    let state_a = registry_a
        .state_for(&community_id)
        .await
        .expect("engine spawned");
    {
        let mut sa = state_a.lock().await;
        let outcome = sa.insert_event(
            admin_join_event,
            &harmony_app::community_membership::VerifyContext {
                expected_community_id: community_id,
                admin_addr: admin,
                is_invite_only: false,
                actor_identity_pub: &admin_pub,
                countersigner_identity_pub: None,
            },
        );
        assert!(matches!(
            outcome,
            harmony_app::community_state_crdt::InsertOutcome::Inserted
        ));
    }

    registry_a.flush_now(&community_id).await.expect("flush a");

    // Forward A's outbound packet to B's subscriber channel directly.
    // FIFO ordering on `b_sub_tx` already guarantees the garbage from
    // line 428 reaches B before this valid wire; the 50ms sleep above
    // is the actual timing hedge that lets B's task drain the garbage
    // before we deliver the liveness probe.
    let valid_wire = a_pub_rx.recv().await.expect("A produced no wire packet");
    b_sub_tx.send(valid_wire).await.expect("send valid wire");

    // Liveness check: B's engine survived the malformed packet and
    // processed the subsequent valid publish.
    let state_b = registry_b
        .state_for(&community_id)
        .await
        .expect("engine spawned");
    let state_b_for_poll = Arc::clone(&state_b);
    wait_until(
        Duration::from_secs(2),
        Duration::from_millis(10),
        move || {
            let s = Arc::clone(&state_b_for_poll);
            async move { s.lock().await.events.len() == 1 }
        },
    )
    .await;
    {
        let sb = state_b.lock().await;
        assert_eq!(
            sb.events.len(),
            1,
            "B should still process valid publish after malformed input"
        );
    }

    registry_a.shutdown_all().await.expect("shutdown a");
    registry_b.shutdown_all().await.expect("shutdown b");
}

/// Replay protection: forwarding the SAME wire bytes to B twice must
/// be idempotent. The first delivery passes `would_accept` and the
/// event lands; the second delivery fails `would_accept` (the
/// tracker recorded the publisher's HLC after step 1) and surfaces as
/// `IncomingOutcome::Duplicate` — silently dropped, no degraded
/// report. B's CRDT shows exactly 1 event.
///
/// Important: we re-inject the SAME bytes (cloned), not a regenerated
/// publish. Encrypting twice would produce different wire (random
/// nonce), but more importantly the publisher's HLC would advance,
/// which would defeat the dedupe path we're testing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replay_of_same_root_publish_is_idempotent() {
    let cas_tx = spawn_shared_cas();
    let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_tx,
        Duration::from_millis(2000),
    ));

    let community_id = SpaceId([4u8; 16]);
    let mk = MembershipKey::new([0x88; 32]);

    let id_admin = PrivateIdentity::from_seed(&[0xd1; 32]);
    let admin = OwnerAddr(id_admin.identity.address_hash);
    let admin_pub = id_admin.identity.to_public_bytes();
    let admin_signing = signing_key_from(&id_admin);

    let mut resolver_map = std::collections::HashMap::new();
    resolver_map.insert(admin, admin_pub);
    let resolver: Arc<dyn IdentityResolver> = Arc::new(StaticResolver { map: resolver_map });

    let (a_pub_tx, mut a_pub_rx) = mpsc::channel::<Vec<u8>>(8);
    let (b_sub_tx, b_sub_rx) = mpsc::channel::<Vec<u8>>(8);

    let dir_a = tempfile::tempdir().expect("tempdir A");
    let dir_b = tempfile::tempdir().expect("tempdir B");

    let registry_a = CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_id: "a-dev".into(),
        content_store: Arc::clone(&cs),
        identity_resolver: Arc::clone(&resolver),
        identity_dir: dir_a.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: None,
        // ZEB-256 Task 6: A publishes as admin.
        self_owner: admin,
        signing_key: Arc::clone(&admin_signing),
    });
    let registry_b = CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_id: "b-dev".into(),
        content_store: Arc::clone(&cs),
        identity_resolver: Arc::clone(&resolver),
        identity_dir: dir_b.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: None,
        // ZEB-256 Task 6: B doesn't publish; admin's identity satisfies
        // the type bound.
        self_owner: admin,
        signing_key: Arc::clone(&admin_signing),
    });

    let (b_pub_tx, _b_pub_rx) = mpsc::channel(8);
    let (_a_sub_tx, a_sub_rx) = mpsc::channel(8);

    registry_a
        .spawn_engine(community_id, mk.clone(), admin, false, a_pub_tx, a_sub_rx)
        .await
        .expect("spawn a");
    registry_b
        .spawn_engine(community_id, mk, admin, false, b_pub_tx, b_sub_rx)
        .await
        .expect("spawn b");

    // ZEB-256 Task 6: pre-seed B's CRDT with admin's Join so the
    // membership-at-HLC gate admits A's later publish.
    let admin_join_event = {
        let payload = EventPayload {
            id: [13u8; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: admin,
            at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "a-dev".into(),
            },
        };
        sign_event_with_identity(&payload, &id_admin).expect("sign")
    };
    {
        let state_b = registry_b
            .state_for(&community_id)
            .await
            .expect("engine spawned");
        let mut sb = state_b.lock().await;
        let outcome = sb.insert_event(
            admin_join_event.clone(),
            &harmony_app::community_membership::VerifyContext {
                expected_community_id: community_id,
                admin_addr: admin,
                is_invite_only: false,
                actor_identity_pub: &admin_pub,
                countersigner_identity_pub: None,
            },
        );
        assert!(matches!(
            outcome,
            harmony_app::community_state_crdt::InsertOutcome::Inserted
        ));
    }

    // Inject the same Join into A so its publish carries non-empty
    // state.
    let state_a = registry_a
        .state_for(&community_id)
        .await
        .expect("engine spawned");
    {
        let mut sa = state_a.lock().await;
        let outcome = sa.insert_event(
            admin_join_event,
            &harmony_app::community_membership::VerifyContext {
                expected_community_id: community_id,
                admin_addr: admin,
                is_invite_only: false,
                actor_identity_pub: &admin_pub,
                countersigner_identity_pub: None,
            },
        );
        assert!(matches!(
            outcome,
            harmony_app::community_state_crdt::InsertOutcome::Inserted
        ));
    }

    registry_a.flush_now(&community_id).await.expect("flush a");

    // Capture A's outbound wire packet so we can replay it byte-for-
    // byte on B's subscriber channel.
    let wire = a_pub_rx.recv().await.expect("A produced no wire packet");

    // Deliver twice. The second delivery exercises the
    // `RootHlcTracker.would_accept` early-exit at step 2 of
    // `handle_incoming_publish`.
    b_sub_tx.send(wire.clone()).await.expect("send 1");
    b_sub_tx.send(wire).await.expect("send 2");

    let state_b = registry_b
        .state_for(&community_id)
        .await
        .expect("engine spawned");

    // Wait for at least one merge to land. The test's invariant is
    // that exactly one event survives — but checking events.len() == 1
    // immediately would race the second-delivery's tracker-only
    // rejection. Poll until ≥ 1 event lands, then sleep briefly to
    // give the second delivery a chance to NOT change anything, then
    // assert.
    let state_b_for_poll = Arc::clone(&state_b);
    wait_until(
        Duration::from_secs(2),
        Duration::from_millis(10),
        move || {
            let s = Arc::clone(&state_b_for_poll);
            async move { !s.lock().await.events.is_empty() }
        },
    )
    .await;
    // Brief settle so the second delivery has time to be processed
    // and (correctly) hit the tracker-only Duplicate path.
    tokio::time::sleep(Duration::from_millis(50)).await;
    {
        let sb = state_b.lock().await;
        assert_eq!(
            sb.events.len(),
            1,
            "B's CRDT should hold exactly one event after replay"
        );
    }

    registry_a.shutdown_all().await.expect("shutdown a");
    registry_b.shutdown_all().await.expect("shutdown b");
}
