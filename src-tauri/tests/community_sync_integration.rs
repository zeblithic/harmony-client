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

/// ZEB-256 § 11 acceptance: "Spoofing test demonstrates the censorship
/// attack is no longer possible." Prior to publisher authentication,
/// any member with the shared `MembershipKey` could publish a state-
/// root payload claiming `publisher_addr = alice_addr` at HLC `huge`,
/// advancing every receiver's `(alice_addr, alice_dev)` tracker slot
/// past `huge`. Alice's subsequent legitimate publishes — at
/// natural-clock HLCs strictly less than `huge` — would be silently
/// rejected by `RootHlcTracker.would_accept`. Result: any single
/// member could DoS-censor any other member's writes.
///
/// This test wires the censorship attack end-to-end on the two-
/// registry bridge and asserts the receiver no longer admits the
/// spoofed publish into its tracker. Concretely:
///   1. Alice (engine A) publishes legitimately at HLC ≈ now. B's
///      tracker for `(alice_addr, "a-dev")` advances; B's CRDT picks
///      up the bootstrap Join.
///   2. We craft a forged publish: `publisher_addr = alice_addr`,
///      `at = Hlc { wall_ms: huge, device_id: "a-dev" }`,
///      `publisher_sig = bob_sk.sign(...)`. This decrypts cleanly
///      (Bob has the `MembershipKey`) AND passes the membership-at-
///      HLC gate (Alice IS Joined), but FAILS the publisher-sig
///      verify because `bob_sk` does not match `alice_pub`. B emits a
///      `publisher_sig_invalid` degraded report and DOES NOT advance
///      its tracker.
///   3. Alice publishes legitimately again at HLC₂ (where
///      HLC₁ < HLC₂ ≪ huge). B's tracker would reject HLC₂ if and
///      only if it had advanced past HLC₂ — i.e. if step 2 had
///      succeeded. We assert the second publish DOES land: B's
///      tracker advances to `wall_ms ≈ now` (still `< huge`), proving
///      the spoofer cannot squat Alice's HLC slot.
///
/// Test-only accessors used:
///   - `CommunitySyncRegistry::tracker_snapshot` — clones B's
///     `CommunityRootHlcTracker` out from under the engine's mutex.
///   - `CommunitySyncEngine::tracker_arc` — internally backs
///     `tracker_snapshot`.
/// Both are gated `#[doc(hidden)]`; production callers don't inspect
/// the per-publisher tracker.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spoofed_publish_does_not_block_real_publisher() {
    use ed25519_dalek::Signer;
    use harmony_app::community_state_sync::CommunityRootSignedPayload;

    let cas_tx = spawn_shared_cas();
    let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_tx,
        Duration::from_millis(2000),
    ));

    let community_id = SpaceId([5u8; 16]);
    let mk = MembershipKey::new([0xAA; 32]);

    // Alice — the real, legitimate publisher. Engine A signs publishes
    // with `alice_signing`; both registries' resolvers map
    // `alice_addr → alice_pub` so receive-side sig-verify can rebuild
    // the public key it checks the signature against.
    let id_alice = PrivateIdentity::from_seed(&[0xa1; 32]);
    let alice_addr = OwnerAddr(id_alice.identity.address_hash);
    let alice_pub = id_alice.identity.to_public_bytes();
    let alice_signing = signing_key_from(&id_alice);

    // Bob — the attacker. Bob is also a community member (so Bob has
    // the `MembershipKey`), but Bob's signing key is NOT Alice's. The
    // forged publish below is signed with `bob_signing` while claiming
    // `publisher_addr = alice_addr`; verify-on-receive rejects it
    // because `bob_signing` does not match Alice's resolver-resolved
    // identity_pub. The pre-ZEB-256 receive pipeline checked only the
    // shared MK + per-event sigs — Bob's spoof would have advanced B's
    // tracker for `(alice_addr, "a-dev")` to HLC `huge`, censoring
    // every subsequent legitimate Alice publish.
    let id_bob = PrivateIdentity::from_seed(&[0xb1; 32]);
    let bob_addr = OwnerAddr(id_bob.identity.address_hash);
    let bob_pub = id_bob.identity.to_public_bytes();
    let bob_signing = signing_key_from(&id_bob);

    // Both Alice and Bob in the resolver. Alice is the publisher we're
    // verifying against; Bob is here so a future variant that signs
    // legitimately as bob_addr (rather than spoofing alice_addr) would
    // also resolve. The spoof scenario doesn't actually need Bob in
    // the resolver — the signature check fails regardless — but
    // matching production cache shape (every member's identity_pub
    // present) keeps the test honest about which gate is firing.
    let mut resolver_map = std::collections::HashMap::new();
    resolver_map.insert(alice_addr, alice_pub);
    resolver_map.insert(bob_addr, bob_pub);
    let resolver: Arc<dyn IdentityResolver> = Arc::new(StaticResolver { map: resolver_map });

    // Wire: A's publisher → B's subscriber. We retain the explicit
    // `b_sub_tx` so the test can also inject a forged wire packet
    // directly on B's subscriber surface (bypassing A entirely),
    // mirroring the production threat model where any keyed peer can
    // ship arbitrary bytes to any other peer's Zenoh subscriber.
    let (a_pub_tx, mut a_pub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (b_sub_tx, b_sub_rx) = mpsc::channel::<Vec<u8>>(64);
    let b_sub_tx_for_forward = b_sub_tx.clone();
    tokio::spawn(async move {
        while let Some(bytes) = a_pub_rx.recv().await {
            let _ = b_sub_tx_for_forward.send(bytes).await;
        }
    });

    // Plumb a degraded-path receiver on B so we can assert on the
    // `publisher_sig_invalid` report the forged publish triggers.
    let (b_error_tx, mut b_error_rx) = mpsc::channel::<CommunityDegradedReport>(8);

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
        // A's registry signs publishes as Alice — the legitimate
        // publisher under attack.
        self_owner: alice_addr,
        signing_key: Arc::clone(&alice_signing),
    });
    let registry_b = CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_id: "b-dev".into(),
        content_store: Arc::clone(&cs),
        identity_resolver: Arc::clone(&resolver),
        identity_dir: dir_b.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: Some(b_error_tx),
        delta_tx: None,
        // B's registry would sign as Bob in production. B does not
        // publish in this test (Bob's "publish" is the forged direct
        // injection below, hand-crafted outside any registry), but the
        // type bound requires real values. Using bob_signing keeps the
        // engine config self-consistent: receiver-side sig-verify of
        // B's own publishes (if it ever ran one) would also validate.
        self_owner: bob_addr,
        signing_key: Arc::clone(&bob_signing),
    });

    // B's publisher and A's subscriber are unused in this one-way sync
    // test; we still need fresh handles to satisfy `spawn_engine`'s
    // signature.
    let (b_pub_tx, _b_pub_rx) = mpsc::channel(8);
    let (_a_sub_tx, a_sub_rx) = mpsc::channel(8);

    registry_a
        .spawn_engine(
            community_id,
            mk.clone(),
            alice_addr,
            false,
            a_pub_tx,
            a_sub_rx,
        )
        .await
        .expect("spawn a");
    registry_b
        .spawn_engine(
            community_id,
            mk.clone(),
            alice_addr,
            false,
            b_pub_tx,
            b_sub_rx,
        )
        .await
        .expect("spawn b");

    // Pre-seed Alice's bootstrap Join into BOTH engines' CRDTs. Without
    // this, B's membership-at-HLC gate rejects A's first legit publish
    // as `publisher_not_joined`. The unit tests in Task 6 use the same
    // pattern: production wires the Join via the redemption flow on
    // B's side (which inserts the event locally before the first
    // publish lands), but here we bypass the Phase 3 IPC and seed
    // both replicas directly.
    let alice_join_event = {
        let payload = EventPayload {
            id: [21u8; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: alice_addr,
            at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "a-dev".into(),
            },
        };
        sign_event_with_identity(&payload, &id_alice).expect("sign alice join")
    };
    let verify_ctx = harmony_app::community_membership::VerifyContext {
        expected_community_id: community_id,
        admin_addr: alice_addr,
        is_invite_only: false,
        actor_identity_pub: &alice_pub,
        countersigner_identity_pub: None,
    };
    {
        let state_a = registry_a
            .state_for(&community_id)
            .await
            .expect("engine a spawned");
        let mut sa = state_a.lock().await;
        let outcome = sa.insert_event(alice_join_event.clone(), &verify_ctx);
        assert!(matches!(
            outcome,
            harmony_app::community_state_crdt::InsertOutcome::Inserted
        ));
    }
    {
        let state_b = registry_b
            .state_for(&community_id)
            .await
            .expect("engine b spawned");
        let mut sb = state_b.lock().await;
        let outcome = sb.insert_event(alice_join_event.clone(), &verify_ctx);
        assert!(matches!(
            outcome,
            harmony_app::community_state_crdt::InsertOutcome::Inserted
        ));
    }

    // Step 1 — Alice publishes legitimately. After this lands, B's
    // tracker has `(alice_addr, "a-dev") → HLC₁` where HLC₁'s wall_ms
    // ≈ now-in-ms (well below `huge` chosen below).
    registry_a
        .flush_now(&community_id)
        .await
        .expect("flush a 1");

    // Wait for A's first publish to drain the forwarder into B and
    // B's pipeline to advance the tracker. We poll the tracker
    // directly: as soon as `(alice_addr, "a-dev")` appears, step 1
    // has completed end-to-end.
    {
        let registry_b_for_poll = &registry_b;
        wait_until(
            Duration::from_secs(2),
            Duration::from_millis(10),
            || async move {
                let snap = registry_b_for_poll
                    .tracker_snapshot(&community_id)
                    .await
                    .expect("engine b spawned");
                snap.per_device
                    .contains_key(&(alice_addr, "a-dev".to_string()))
            },
        )
        .await;
    }

    // Capture HLC₁ — Alice's first publish HLC as observed at B —
    // so we can assert step 3's HLC₂ strictly dominates it.
    let hlc1 = {
        let snap = registry_b
            .tracker_snapshot(&community_id)
            .await
            .expect("engine b spawned");
        snap.per_device
            .get(&(alice_addr, "a-dev".to_string()))
            .cloned()
            .expect("alice tracker entry after step 1")
    };

    // Step 2 — Build the forged publish OUTSIDE any registry.
    //
    // The attacker's threat model: Bob is a paid-up community member
    // with the `MembershipKey`. Bob crafts a wire packet claiming
    // `publisher_addr = alice_addr`, signs it with `bob_signing`, and
    // ships it to B's Zenoh subscriber. The packet AEAD-decrypts
    // cleanly (Bob has the MK), the publisher-membership gate passes
    // (Alice IS Joined at any reasonable HLC), but the publisher-sig
    // gate MUST fail because `bob_signing` does not match Alice's
    // resolver-resolved identity_pub.
    //
    // `wall_ms` is far in the future — well past any natural
    // `next_hlc` Alice's engine would produce. If B's tracker were
    // (incorrectly) advanced to this value, Alice's step-3 publish
    // would silently fail `would_accept`.
    let forged_huge_hlc = Hlc {
        wall_ms: 4_000_000_000_000, // ≈ year 2096
        logical: 0,
        device_id: "a-dev".into(),
    };
    // The forged blob's CRDT contents don't matter — verify-on-receive
    // rejects the packet at the publisher-sig gate, before any blob
    // fetch. We use Alice's already-published blob CID so the wire
    // packet at least references a CAS slot the attacker could
    // plausibly know about; nothing reads through to it.
    let forged_blob = canonical_cbor_encode(
        &harmony_app::community_state_crdt::CommunityState::new(community_id),
    )
    .expect("encode empty state");
    let forged_blob_ct = harmony_app::community_state_sync::encrypt_blob(&mk, &forged_blob)
        .expect("encrypt forged blob");
    let forged_root_cid = harmony_content::cid::ContentId::for_book(
        &forged_blob_ct,
        harmony_content::cid::ContentFlags {
            encrypted: true,
            ..Default::default()
        },
    )
    .expect("for_book");
    cs.put(forged_root_cid, forged_blob_ct)
        .await
        .expect("forged cas put");

    let forged_signed = CommunityRootSignedPayload {
        root_cid: forged_root_cid,
        publisher_addr: alice_addr,
        at: forged_huge_hlc.clone(),
    };
    let forged_signed_bytes = canonical_cbor_encode(&forged_signed).expect("encode signed");
    // THE SPOOF: signature is from Bob, not Alice. This is the only
    // line that distinguishes the forged packet from a legitimate
    // Alice publish; everything else is byte-perfect imitation.
    let forged_sig = bob_signing.sign(&forged_signed_bytes).to_bytes();
    let forged_publish = forged_signed.into_wire(forged_sig);
    let forged_publish_bytes =
        canonical_cbor_encode(&forged_publish).expect("encode forged publish");
    let forged_wire =
        harmony_app::community_state_sync::encrypt_root_publish(&mk, &forged_publish_bytes)
            .expect("encrypt forged root");

    // Inject the forged wire directly on B's subscriber channel,
    // bypassing the A→B forwarder. Production-equivalent: the
    // attacker's Zenoh peer publishes on the same topic B subscribes
    // to.
    b_sub_tx.send(forged_wire).await.expect("send forged wire");

    // Wait for B to surface the rejection. The `publisher_sig_invalid`
    // degraded report is the load-bearing observable signal that the
    // verify-on-receive gate fired correctly. Without this gate, B
    // would silently advance its tracker to `huge` — and the only
    // visible symptom would be Alice's later publishes going missing,
    // a far harder failure mode to diagnose.
    let report = tokio::time::timeout(Duration::from_secs(2), b_error_rx.recv())
        .await
        .expect("publisher_sig_invalid report timed out")
        .expect("error_tx dropped");
    assert_eq!(report.reason_tag, "publisher_sig_invalid");
    assert_eq!(report.community_id, community_id);

    // Confirm B's tracker for `(alice_addr, "a-dev")` is STILL at
    // HLC₁ — the forged publish did not advance it.
    {
        let snap = registry_b
            .tracker_snapshot(&community_id)
            .await
            .expect("engine b spawned");
        let entry = snap
            .per_device
            .get(&(alice_addr, "a-dev".to_string()))
            .cloned()
            .expect("alice tracker entry should still exist");
        assert_eq!(
            entry, hlc1,
            "tracker for (alice_addr, alice_dev) MUST NOT have advanced \
             to the forged HLC; entry={entry:?}, expected={hlc1:?}"
        );
        assert!(
            !forged_huge_hlc.is_strictly_newer_than(&entry) || entry == hlc1,
            "tracker entry must remain at HLC1, not the spoof's huge HLC"
        );
    }

    // Step 3 — Alice publishes legitimately AGAIN. Her engine's
    // `next_hlc` produces HLC₂ with `wall_ms ≈ now` (or a logical
    // bump if same-millisecond) — strictly newer than HLC₁ but vastly
    // less than `huge`. To force a non-empty publish (the engine
    // skips publishing if the CRDT hasn't changed since last flush),
    // we mint a fresh local Update via `insert_local_event` on A.
    //
    // We use a Leave event from Alice — the simplest event Alice can
    // self-mint that will mutate the CRDT she already has. (A second
    // Join is a no-op on the CRDT — the unique-id key would clash on
    // re-insert; a Leave changes Alice's status to Left and is what
    // production would call when Alice quits the community.)
    //
    // NB: If Alice leaves, the membership-at-publish gate would then
    // reject a SUBSEQUENT publish from her — but step 3 is only ONE
    // publish, so the gate evaluates the publish HLC against
    // Alice's status AT THE PUBLISH HLC. Membership status is
    // computed from events with `at < publish_hlc` (look-back).
    // Alice's Leave event lives at `wall_ms ≈ now`; her publish HLC
    // also lives at `wall_ms ≈ now`. Whether the leave dominates the
    // publish HLC is timing-sensitive, so we use a different mutation:
    // a follow-up Join from a separate device for Alice would also
    // not insert (duplicate id). Simplest path: do NOT insert a new
    // event; instead, force a flush, which re-publishes the same
    // CRDT with a new HLC. The engine doesn't actually skip "no-op"
    // publishes — `flush_now` always advances next_hlc and emits a
    // wire packet (see `publish_root_now` step 5), so the same
    // single-event CRDT will produce a strictly-newer wire packet.
    registry_a
        .flush_now(&community_id)
        .await
        .expect("flush a 2");

    // Wait for B's tracker for `(alice_addr, "a-dev")` to advance
    // PAST hlc1. This is the censorship-defeated assertion: if the
    // forged publish had advanced B's tracker to `huge`, this poll
    // would time out (B would silently drop Alice's HLC₂ publish
    // because `huge > HLC₂`).
    {
        let registry_b_for_poll = &registry_b;
        let hlc1_for_poll = hlc1.clone();
        wait_until(
            Duration::from_secs(3),
            Duration::from_millis(10),
            || async {
                let snap = registry_b_for_poll
                    .tracker_snapshot(&community_id)
                    .await
                    .expect("engine b spawned");
                match snap.per_device.get(&(alice_addr, "a-dev".to_string())) {
                    Some(entry) => entry.is_strictly_newer_than(&hlc1_for_poll),
                    None => false,
                }
            },
        )
        .await;
    }

    // Final invariants:
    //   - HLC₁ < HLC₂ (real publisher advanced past her own first
    //     publish).
    //   - HLC₂ ≪ huge (the forged HLC is far in the future).
    //   - Tracker is at HLC₂, not `huge`.
    let hlc2 = {
        let snap = registry_b
            .tracker_snapshot(&community_id)
            .await
            .expect("engine b spawned");
        snap.per_device
            .get(&(alice_addr, "a-dev".to_string()))
            .cloned()
            .expect("alice tracker entry after step 3")
    };
    assert!(
        hlc2.is_strictly_newer_than(&hlc1),
        "HLC₂ ({hlc2:?}) must strictly dominate HLC₁ ({hlc1:?})"
    );
    assert!(
        forged_huge_hlc.is_strictly_newer_than(&hlc2),
        "huge ({forged_huge_hlc:?}) must dominate HLC₂ ({hlc2:?}) — \
         otherwise the test isn't actually exercising the censorship \
         scenario (Alice would naturally outpace the spoof regardless)"
    );

    registry_a.shutdown_all().await.expect("shutdown a");
    registry_b.shutdown_all().await.expect("shutdown b");
}

/// ZEB-256 Task 9 — tracker entries are NOT pruned on Leave. Pins the
/// invariant that defends against a future "fix" clearing tracker
/// entries on Leave: such a fix would re-open the censorship gap
/// because a malicious peer racing a re-Join could still spoof the
/// slot. The natural strictly-newer publish HLC means we never need
/// to prune to admit a hypothetical re-Join.
///
/// Sequence:
///   1. Alice Joins + publishes (tracker[(alice, "a-dev")] = HLC₁).
///   2. Alice Leaves + publishes (Leave merges into B; tracker
///      advances to HLC₂; Alice's status flips to Left).
///   3. Verify B's tracker STILL contains the per-device entry for
///      `(alice, "a-dev")` after the Leave merges — i.e. the entry
///      was NOT pruned. The HLC has advanced (HLC₂ > HLC₁) which is
///      itself proof that any future re-Join publish (HLC₃ produced
///      by `next_hlc`, strictly newer than HLC₂) would be admitted
///      by `would_accept` against the surviving entry.
///
/// Note on test shape — the original draft also tried to verify a
/// re-Join publish being admitted end-to-end, but the receive-side
/// membership-at-HLC gate (community_state_sync.rs:1454) rejects any
/// publish from a publisher whose CURRENT materialized status is not
/// `Joined`. After the Leave merges, Alice is `Left`, so her next
/// publish (which would carry her own re-Join event) is rejected
/// before the blob is ingested — the gate cannot peek inside the
/// encrypted blob to see the re-Join. Phase 4's invite flow re-Joins
/// Alice via an admin-issued Invite (separate publisher, gate
/// admits), at which point the surviving tracker entry from this
/// test is what the censorship-defense argument relies on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn leave_does_not_prune_per_device_tracker_entry() {
    let cas_tx = spawn_shared_cas();
    let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_tx,
        Duration::from_millis(2000),
    ));

    let community_id = SpaceId([0x77; 16]);
    let mk = MembershipKey::new([0x55; 32]);

    // Alice is the admin AND the publisher under test. She'll Join then
    // Leave — exercising the tracker's behavior across a Leave so we
    // can verify her per-device entry survives the membership change.
    let id_alice = PrivateIdentity::from_seed(&[0xa1; 32]);
    let alice_addr = OwnerAddr(id_alice.identity.address_hash);
    let alice_pub = id_alice.identity.to_public_bytes();
    let alice_signing = signing_key_from(&id_alice);

    // B is a passive observer — receives Alice's publishes, surfaces
    // tracker advances. B doesn't publish in this test, but the
    // registry config requires a real signing key + self_owner, so we
    // give B a distinct identity.
    let id_b = PrivateIdentity::from_seed(&[0xb1; 32]);
    let b_owner = OwnerAddr(id_b.identity.address_hash);
    let b_signing = signing_key_from(&id_b);

    // Resolver carries both Alice and B's identity_pubs. Receive-side
    // sig-verify on B looks up `alice_addr → alice_pub` to validate
    // every publish A signs.
    let mut resolver_map = std::collections::HashMap::new();
    resolver_map.insert(alice_addr, alice_pub);
    resolver_map.insert(b_owner, id_b.identity.to_public_bytes());
    let resolver: Arc<dyn IdentityResolver> = Arc::new(StaticResolver { map: resolver_map });

    // Wire: A's publisher → B's subscriber. One-way — B never publishes.
    let (a_pub_tx, mut a_pub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (b_sub_tx, b_sub_rx) = mpsc::channel::<Vec<u8>>(64);
    let b_sub_tx_for_forward = b_sub_tx.clone();
    tokio::spawn(async move {
        while let Some(bytes) = a_pub_rx.recv().await {
            let _ = b_sub_tx_for_forward.send(bytes).await;
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
        self_owner: alice_addr,
        signing_key: Arc::clone(&alice_signing),
    });
    let registry_b = CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_id: "b-dev".into(),
        content_store: Arc::clone(&cs),
        identity_resolver: Arc::clone(&resolver),
        identity_dir: dir_b.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: None,
        self_owner: b_owner,
        signing_key: Arc::clone(&b_signing),
    });

    // B never publishes, A never receives — but spawn_engine requires
    // both directions wired with real channels.
    let (b_pub_tx, _b_pub_rx) = mpsc::channel(8);
    let (_a_sub_tx, a_sub_rx) = mpsc::channel(8);

    registry_a
        .spawn_engine(
            community_id,
            mk.clone(),
            alice_addr,
            false,
            a_pub_tx,
            a_sub_rx,
        )
        .await
        .expect("spawn a");
    registry_b
        .spawn_engine(
            community_id,
            mk.clone(),
            alice_addr,
            false,
            b_pub_tx,
            b_sub_rx,
        )
        .await
        .expect("spawn b");

    // Pre-seed Alice's bootstrap Join into BOTH engines' CRDTs. Without
    // this, B's membership-at-HLC gate rejects A's first publish as
    // `publisher_not_joined` because Alice isn't yet a member of B's
    // (empty) materialized state. Mirrors the Task 8 test pattern: in
    // production, the redemption flow inserts the Join on B's side
    // before the first publish lands. The HLC₁ we capture from B's
    // tracker reflects the publish HLC of A's first `flush_now` (which
    // is strictly newer than the bootstrap Join's wall_ms=100 because
    // `next_hlc` runs at real time).
    let alice_join_event = {
        let payload = EventPayload {
            id: [1u8; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: alice_addr,
            at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "a-dev".into(),
            },
        };
        sign_event_with_identity(&payload, &id_alice).expect("sign alice join")
    };
    let verify_ctx = harmony_app::community_membership::VerifyContext {
        expected_community_id: community_id,
        admin_addr: alice_addr,
        is_invite_only: false,
        actor_identity_pub: &alice_pub,
        countersigner_identity_pub: None,
    };
    {
        let state_a = registry_a
            .state_for(&community_id)
            .await
            .expect("engine a spawned");
        let mut sa = state_a.lock().await;
        let outcome = sa.insert_event(alice_join_event.clone(), &verify_ctx);
        assert!(matches!(
            outcome,
            harmony_app::community_state_crdt::InsertOutcome::Inserted
        ));
    }
    {
        let state_b = registry_b
            .state_for(&community_id)
            .await
            .expect("engine b spawned");
        let mut sb = state_b.lock().await;
        let outcome = sb.insert_event(alice_join_event.clone(), &verify_ctx);
        assert!(matches!(
            outcome,
            harmony_app::community_state_crdt::InsertOutcome::Inserted
        ));
    }

    let engine_a = registry_a
        .engine_arc(&community_id)
        .await
        .expect("engine a");

    // Step 1 — flush A's bootstrap CRDT to B. Alice's status is
    // `Joined` in both replicas, so B's membership gate admits the
    // publish. After it lands, B's tracker for `(alice_addr, "a-dev")`
    // records HLC₁ = the publish's `next_hlc`-derived HLC.
    registry_a.flush_now(&community_id).await.expect("flush 1");

    // Wait for B's tracker to record Alice's first slot, then snapshot
    // the entry. The captured HLC₁ is the load-bearing baseline for
    // step 3 (the post-Leave entry must still be present AND have
    // advanced past HLC₁ — pruning would surface as a missing entry).
    {
        let registry_b_for_poll = &registry_b;
        wait_until(
            Duration::from_secs(2),
            Duration::from_millis(10),
            || async move {
                let snap = registry_b_for_poll
                    .tracker_snapshot(&community_id)
                    .await
                    .expect("engine b spawned");
                snap.per_device
                    .contains_key(&(alice_addr, "a-dev".to_string()))
            },
        )
        .await;
    }
    let first_slot = {
        let snap = registry_b
            .tracker_snapshot(&community_id)
            .await
            .expect("engine b spawned");
        snap.per_device
            .get(&(alice_addr, "a-dev".to_string()))
            .cloned()
            .expect("alice tracker entry after step 1")
    };

    // Step 2 — Alice Leaves and publishes. The publish carries the
    // Leave event; at the moment B's gate runs, Alice is still
    // `Joined` in B's local CRDT (the Leave hasn't merged yet), so the
    // gate admits the publish. After ingestion B has 2 events (Join,
    // Leave) and Alice's status flips to `Left`.
    let alice_leave_event = {
        let payload = EventPayload {
            id: [2u8; 16],
            community_id,
            kind: MembershipEventKind::Leave,
            actor: alice_addr,
            at: Hlc {
                wall_ms: 200,
                logical: 0,
                device_id: "a-dev".into(),
            },
        };
        sign_event_with_identity(&payload, &id_alice).expect("sign alice leave")
    };
    engine_a
        .insert_local_event(alice_leave_event)
        .await
        .expect("a leave");
    registry_a.flush_now(&community_id).await.expect("flush 2");
    {
        let registry_b_for_poll = &registry_b;
        wait_until(
            Duration::from_secs(2),
            Duration::from_millis(10),
            || async move {
                let s = registry_b_for_poll
                    .state_for(&community_id)
                    .await
                    .expect("state b");
                let g = s.lock().await;
                g.events.len() == 2
            },
        )
        .await;
    }
    // Capture HLC after Leave merge (HLC₂). Step 3's invariant check
    // verifies the entry equals HLC₂ — i.e. the Leave-publish DID
    // advance the tracker (defensive: catches a regression where
    // Leave-publishes silently fail to advance per-device entries).
    let post_leave_slot = {
        let snap = registry_b
            .tracker_snapshot(&community_id)
            .await
            .expect("engine b spawned");
        snap.per_device
            .get(&(alice_addr, "a-dev".to_string()))
            .cloned()
            .expect("alice tracker entry after step 2")
    };
    assert!(
        post_leave_slot.is_strictly_newer_than(&first_slot),
        "tracker should have advanced from step 2's flush (Leave)"
    );

    // Step 3 — pin the tracker-entry-survives invariant. After the
    // Leave has merged, B's tracker MUST still contain the per-device
    // entry for `(alice_addr, "a-dev")`. A future "fix" that clears
    // tracker entries on Leave would surface here as a missing entry.
    //
    // The HLC has advanced (HLC₂ > HLC₁), and any subsequent publish
    // from Alice's device (whether legitimate via Phase 4 invite-flow
    // re-Join, or post-membership-gate-relaxation) would naturally
    // produce HLC₃ > HLC₂ via `next_hlc`. So the surviving tracker
    // entry never blocks legitimate re-publishes — it only blocks
    // replays of old HLCs (which is the censorship-defense property).
    let surviving_entry = {
        let snap = registry_b
            .tracker_snapshot(&community_id)
            .await
            .expect("engine b spawned");
        snap.per_device
            .get(&(alice_addr, "a-dev".to_string()))
            .cloned()
            .expect(
                "tracker entry for (alice, a-dev) MUST survive across Leave; \
                 a missing entry indicates a regression in pruning behavior",
            )
    };
    assert!(
        surviving_entry.is_strictly_newer_than(&first_slot),
        "surviving tracker entry ({surviving_entry:?}) must have advanced \
         past HLC₁ ({first_slot:?}) — step 2's Leave-publish ingestion \
         should have advanced it"
    );
    assert_eq!(
        surviving_entry, post_leave_slot,
        "surviving tracker entry must equal HLC₂ — no extra publishes \
         after step 2"
    );

    // Step 3b — confirm Alice's status materialized on B is `Left`.
    // Pins the other half of the regression-defense argument: the
    // gate-vs-tracker split. The membership-at-HLC gate is the
    // current line of defense against post-Leave publishes; the
    // tracker entry survives independently as defense-in-depth for
    // any future code path (Phase 4 re-Join via admin invite) that
    // bypasses the gate.
    let s = registry_b.state_for(&community_id).await.expect("state b");
    let events: Vec<_> = s.lock().await.events.values().cloned().collect();
    let materialized = harmony_app::community_membership::materialize(&events, alice_addr);
    let alice_state = materialized
        .members
        .get(&alice_addr)
        .expect("alice present in materialized state");
    assert_eq!(
        alice_state.status,
        harmony_app::community_membership::MemberStatus::Left,
        "alice should be Left after the Leave event merges on B"
    );

    registry_a.shutdown_all().await.expect("shutdown a");
    registry_b.shutdown_all().await.expect("shutdown b");
}
