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
    CommunityRootPublishPayload, CommunitySyncRegistry, IdentityResolver, DEFAULT_DEBOUNCE_MS,
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

impl IdentityResolver for StaticResolver {
    fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
        self.map.get(addr).copied()
    }
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
    });
    let registry_b = CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_id: "b-dev".into(),
        content_store: Arc::clone(&cs),
        identity_resolver: Arc::clone(&resolver),
        identity_dir: dir_b.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
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
    {
        let mut sa = state_a.lock().await;
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
        let event = sign_event_with_identity(&payload, &id_admin).expect("sign");
        let outcome = sa.insert_event(
            event,
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
    // to sleep through the full window.
    registry_a.flush_now(&community_id).await.expect("flush a");

    // Give B's subscriber arm a window to receive, fetch, decrypt,
    // verify, and merge. 200 ms matches the unit test in Task 8.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let state_b = registry_b
        .state_for(&community_id)
        .await
        .expect("engine spawned");
    {
        let sb = state_b.lock().await;
        assert_eq!(sb.events.len(), 1, "B should have merged A's event");
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

    let community_id = SpaceId([2u8; 16]);
    let mk = MembershipKey::new([0x55; 32]);

    let id_admin = PrivateIdentity::from_seed(&[0xb1; 32]);
    let admin = OwnerAddr(id_admin.identity.address_hash);
    let admin_pub = id_admin.identity.to_public_bytes();

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

    let publish = CommunityRootPublishPayload {
        root_cid,
        at: Hlc {
            wall_ms: 200,
            logical: 0,
            device_id: "attacker-dev".into(),
        },
    };
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

    // B's CRDT must remain empty — the event was rejected per-event
    // even though the wire packet AEAD-decrypted cleanly. The
    // tracker DID advance (single mutation point), but no event
    // was inserted.
    let state_b = registry_b
        .state_for(&community_id)
        .await
        .expect("engine spawned");
    {
        let sb = state_b.lock().await;
        assert_eq!(
            sb.events.len(),
            0,
            "forged-sig event must not land in B's CRDT"
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
    });
    let registry_b = CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_id: "b-dev".into(),
        content_store: Arc::clone(&cs),
        identity_resolver: Arc::clone(&resolver),
        identity_dir: dir_b.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
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

    // Inject 64 random bytes — long enough to pass MIN_WIRE_LEN
    // (28 = nonce 12 + tag 16) but with no valid nonce / tag, so
    // ChaCha20-Poly1305 AEAD verification fails and the engine
    // drops the packet via IncomingOutcome::ErrPreMutation.
    let garbage: Vec<u8> = (0..64u8).map(|i| i.wrapping_mul(31)).collect();
    b_sub_tx.send(garbage).await.expect("send garbage");

    // Brief settle so B's task has a chance to drain + log the
    // malformed packet before we exercise the live-check path.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Inject a Join into A and trigger a valid publish.
    let state_a = registry_a
        .state_for(&community_id)
        .await
        .expect("engine spawned");
    {
        let mut sa = state_a.lock().await;
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
        let event = sign_event_with_identity(&payload, &id_admin).expect("sign");
        let outcome = sa.insert_event(
            event,
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

    tokio::time::sleep(Duration::from_millis(300)).await;

    // Liveness check: B's engine survived the malformed packet and
    // processed the subsequent valid publish.
    let state_b = registry_b
        .state_for(&community_id)
        .await
        .expect("engine spawned");
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
    });
    let registry_b = CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_id: "b-dev".into(),
        content_store: Arc::clone(&cs),
        identity_resolver: Arc::clone(&resolver),
        identity_dir: dir_b.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
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

    // Inject one Join into A.
    let state_a = registry_a
        .state_for(&community_id)
        .await
        .expect("engine spawned");
    {
        let mut sa = state_a.lock().await;
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
        let event = sign_event_with_identity(&payload, &id_admin).expect("sign");
        let outcome = sa.insert_event(
            event,
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

    tokio::time::sleep(Duration::from_millis(300)).await;

    let state_b = registry_b
        .state_for(&community_id)
        .await
        .expect("engine spawned");
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
