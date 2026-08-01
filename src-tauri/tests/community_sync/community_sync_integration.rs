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
    mint_test_owner, sign_event, EventPayload, MembershipEventKind, SignedMembershipEvent,
    TestOwner,
};
use harmony_app::community_state_crdt::CommunityState;
use harmony_app::community_state_sync::{
    encrypt_blob, encrypt_root_publish, CommunityDegradedReport, CommunityRegistryConfig,
    CommunitySyncRegistry, IdentityResolver, DEFAULT_DEBOUNCE_MS,
};
use harmony_app::content_store::{CasOp, ContentStore, RuntimeContentStore};
use harmony_app::owner_state_crypto::canonical_cbor_encode;
use harmony_app::owner_state_types::{EpochKey, Hlc, OwnerAddr, SpaceId};
use harmony_identity::PrivateIdentity;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};

// ---------------------------------------------------------------------
// Shared test scaffolding
// ---------------------------------------------------------------------

/// ZEB-339: return the owner's enrolled device signing key (#2), wrapped in
/// `Arc` for the registry/engine config + mint helpers. Under the enrolled-
/// device model the actor (`owner_id`) is distinct from the signing key; both
/// the publisher-sig and verify_event paths resolve the signer from the
/// materialized enrolled key (learned from the owner's cert-bearing Join).
fn signing_key_from(owner: &TestOwner) -> Arc<ed25519_dalek::SigningKey> {
    Arc::new(owner.device_key.clone())
}

/// ZEB-339: sign a membership event with the owner's enrolled device key,
/// attaching the Master cert on identity-introducing events (Join/PendingJoin)
/// so the engine learns the owner's enrolled device key.
fn sign_event_with_identity(
    payload: &EventPayload,
    owner: &TestOwner,
) -> Result<SignedMembershipEvent, harmony_app::owner_state_crypto::CryptoError> {
    let ev = sign_event(payload, &owner.device_key)?;
    Ok(match ev.kind {
        MembershipEventKind::Join | MembershipEventKind::PendingJoin { .. } => {
            SignedMembershipEvent {
                enrollment: Some(owner.cert.clone()),
                ..ev
            }
        }
        _ => ev,
    })
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
                CasOp::PutLocal {
                    cid, blob, reply, ..
                } => {
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
                CasOp::GetLocal { cid, reply } => {
                    let v = store.lock().await.get(&cid).cloned();
                    let _ = reply.send(v);
                }
                CasOp::AllowServeSubtree { reply, .. } => {
                    let _ = reply.send(Ok(0));
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
    let mk = EpochKey::new([0x42; 32]);

    let id_admin = mint_test_owner(0xa1);
    let admin = id_admin.owner;
    let admin_pub = [0u8; 64];
    let admin_signing = signing_key_from(&id_admin);

    // For B to publish under its own identity, derive a separate
    // PrivateIdentity. This test does not exercise B's publish path,
    // but the registry needs a valid signing-key/self_owner pair.
    let id_b = mint_test_owner(0xb1);
    let b_owner = id_b.owner;
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
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
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
        crdt_state: None,
        nav_emitter: None,
        presence_resync_rx: None,
    });
    let registry_b = CommunitySyncRegistry::new(CommunityRegistryConfig {
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
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
        crdt_state: None,
        nav_emitter: None,
        presence_resync_rx: None,
    });

    // B's publisher and A's subscriber are unused in this one-way
    // sync test; we still need fresh handles to satisfy
    // `spawn_engine`'s signature.
    let (b_pub_tx, _b_pub_rx) = mpsc::channel(8);
    let (_a_sub_tx, a_sub_rx) = mpsc::channel(8);

    registry_a
        .spawn_engine_inner_now(
            community_id,
            mk.clone(),
            admin,
            false,
            a_pub_tx,
            a_sub_rx,
            harmony_app::community_state_sync::CatchUpChannels::none(),
        )
        .await
        .expect("spawn a");
    registry_b
        .spawn_engine_inner_now(
            community_id,
            mk,
            admin,
            false,
            b_pub_tx,
            b_sub_rx,
            harmony_app::community_state_sync::CatchUpChannels::none(),
        )
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
    // the publish made it through receive's verify gates. Asserting on
    // B's tracker (rather than `events.len()`) avoids pre-seed vacuity:
    // the pre-seeded Join already gives B `events.len() == 1` BEFORE
    // A's publish lands, so the count would pass even if the publish
    // had been silently dropped. Tracker advancement, in contrast, only
    // happens when the receive pipeline reaches step 11.
    {
        let registry_b_for_poll = &registry_b;
        wait_until(
            Duration::from_secs(2),
            Duration::from_millis(10),
            || async move {
                let snap = registry_b_for_poll
                    .tracker_snapshot(&community_id)
                    .await
                    .expect("engine spawned");
                snap.per_device.contains_key(&(admin, "a-dev".to_string()))
            },
        )
        .await;
    }

    {
        let snap = registry_b
            .tracker_snapshot(&community_id)
            .await
            .expect("engine spawned");
        assert!(
            snap.per_device.contains_key(&(admin, "a-dev".to_string())),
            "B's tracker MUST have recorded admin's publish from a-dev"
        );
        let sb = registry_b
            .state_for(&community_id)
            .await
            .expect("engine spawned")
            .lock()
            .await
            .clone();
        assert_eq!(
            sb.event_count(),
            1,
            "B should hold the (pre-seeded) Join — A's publish carries \
             the same event id, so the merge is a no-op on the CRDT"
        );
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
    let mk = EpochKey::new([0x55; 32]);

    let id_admin = mint_test_owner(0xb1);
    let admin = id_admin.owner;
    let admin_pub = [0u8; 64];
    let admin_signing = signing_key_from(&id_admin);

    let mut resolver_map = std::collections::HashMap::new();
    resolver_map.insert(admin, admin_pub);
    let resolver: Arc<dyn IdentityResolver> = Arc::new(StaticResolver { map: resolver_map });

    // Plumb a degraded-path receiver so we can assert on the
    // `verify_event_rejected` report B emits.
    let (error_tx, mut error_rx) = mpsc::channel::<CommunityDegradedReport>(8);

    let dir_b = tempfile::tempdir().expect("tempdir B");
    let registry_b = CommunitySyncRegistry::new(CommunityRegistryConfig {
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
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
        crdt_state: None,
        nav_emitter: None,
        presence_resync_rx: None,
    });

    // We need direct access to B's subscriber channel sender to
    // inject the crafted wire packet, so we build the (sub_tx, sub_rx)
    // pair here rather than via a forwarder.
    let (b_sub_tx, b_sub_rx) = mpsc::channel::<Vec<u8>>(8);
    let (b_pub_tx, _b_pub_rx) = mpsc::channel(8);

    registry_b
        .spawn_engine_inner_now(
            community_id,
            mk.clone(),
            admin,
            false,
            b_pub_tx,
            b_sub_rx,
            harmony_app::community_state_sync::CatchUpChannels::none(),
        )
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
    bad_state.insert_verified_for_test(event);

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
    let publish = signed.into_wire(sig, None);
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
            sb.event_count(),
            1,
            "B should hold only the pre-seeded admin Join; \
             the forged-sig event must not have inserted"
        );
    }

    // The tracker must have advanced for `(admin, "attacker-dev")`
    // to the publish HLC. The publisher gates passed (admin is the
    // legitimate publisher and signed the wrapper correctly), so
    // step 11's "single mutation point" runs — even though step 9's
    // per-event verify rejected the malformed inner event. Pinning
    // the tracker advance defends against a future regression that
    // moves the per-event verify failure UP the pipeline (e.g.,
    // turning verify_event_rejected into a pre-mutation rollback) —
    // without this assertion that regression would silently hide
    // the legitimate publisher's HLC behind a stale tracker slot.
    let publish_hlc = Hlc {
        wall_ms: 200,
        logical: 0,
        device_id: "attacker-dev".into(),
    };
    {
        let snap = registry_b
            .tracker_snapshot(&community_id)
            .await
            .expect("engine b spawned");
        let entry = snap
            .per_device
            .get(&(admin, "attacker-dev".to_string()))
            .cloned()
            .expect(
                "tracker MUST record (admin, attacker-dev) — \
                 publisher gates passed; only per-event verify \
                 failed, so step 11 single-mutation-point ran",
            );
        assert_eq!(
            entry, publish_hlc,
            "tracker entry should be the publish HLC (200, 0); \
             actual={entry:?}, expected={publish_hlc:?}"
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
    let mk = EpochKey::new([0x77; 32]);

    let id_admin = mint_test_owner(0xc1);
    let admin = id_admin.owner;
    let admin_pub = [0u8; 64];
    let admin_signing = signing_key_from(&id_admin);

    let mut resolver_map = std::collections::HashMap::new();
    resolver_map.insert(admin, admin_pub);
    let resolver: Arc<dyn IdentityResolver> = Arc::new(StaticResolver { map: resolver_map });

    let (a_pub_tx, mut a_pub_rx) = mpsc::channel::<Vec<u8>>(8);
    let (b_sub_tx, b_sub_rx) = mpsc::channel::<Vec<u8>>(8);

    let dir_a = tempfile::tempdir().expect("tempdir A");
    let dir_b = tempfile::tempdir().expect("tempdir B");

    let registry_a = CommunitySyncRegistry::new(CommunityRegistryConfig {
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
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
        crdt_state: None,
        nav_emitter: None,
        presence_resync_rx: None,
    });
    let registry_b = CommunitySyncRegistry::new(CommunityRegistryConfig {
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
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
        crdt_state: None,
        nav_emitter: None,
        presence_resync_rx: None,
    });

    let (b_pub_tx, _b_pub_rx) = mpsc::channel(8);
    let (_a_sub_tx, a_sub_rx) = mpsc::channel(8);

    registry_a
        .spawn_engine_inner_now(
            community_id,
            mk.clone(),
            admin,
            false,
            a_pub_tx,
            a_sub_rx,
            harmony_app::community_state_sync::CatchUpChannels::none(),
        )
        .await
        .expect("spawn a");
    registry_b
        .spawn_engine_inner_now(
            community_id,
            mk,
            admin,
            false,
            b_pub_tx,
            b_sub_rx,
            harmony_app::community_state_sync::CatchUpChannels::none(),
        )
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
    // processed the subsequent valid publish. We poll B's tracker
    // (NOT `events.len()`) — the pre-seeded admin Join already gives
    // B `events.len() == 1` before A's publish arrives, so a count
    // assertion would pass even if B silently dropped the publish.
    // Tracker advancement is the load-bearing observable signal that
    // the receive pipeline reached step 11 and wrote the publisher
    // slot.
    {
        let registry_b_for_poll = &registry_b;
        wait_until(
            Duration::from_secs(2),
            Duration::from_millis(10),
            || async move {
                let snap = registry_b_for_poll
                    .tracker_snapshot(&community_id)
                    .await
                    .expect("engine spawned");
                snap.per_device.contains_key(&(admin, "a-dev".to_string()))
            },
        )
        .await;
    }
    {
        let snap = registry_b
            .tracker_snapshot(&community_id)
            .await
            .expect("engine spawned");
        assert!(
            snap.per_device.contains_key(&(admin, "a-dev".to_string())),
            "B's tracker MUST advance for admin's valid publish after \
             the prior malformed-wire packet was dropped"
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
    let mk = EpochKey::new([0x88; 32]);

    let id_admin = mint_test_owner(0xd1);
    let admin = id_admin.owner;
    let admin_pub = [0u8; 64];
    let admin_signing = signing_key_from(&id_admin);

    let mut resolver_map = std::collections::HashMap::new();
    resolver_map.insert(admin, admin_pub);
    let resolver: Arc<dyn IdentityResolver> = Arc::new(StaticResolver { map: resolver_map });

    let (a_pub_tx, mut a_pub_rx) = mpsc::channel::<Vec<u8>>(8);
    let (b_sub_tx, b_sub_rx) = mpsc::channel::<Vec<u8>>(8);

    let dir_a = tempfile::tempdir().expect("tempdir A");
    let dir_b = tempfile::tempdir().expect("tempdir B");

    let registry_a = CommunitySyncRegistry::new(CommunityRegistryConfig {
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
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
        crdt_state: None,
        nav_emitter: None,
        presence_resync_rx: None,
    });
    let registry_b = CommunitySyncRegistry::new(CommunityRegistryConfig {
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
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
        crdt_state: None,
        nav_emitter: None,
        presence_resync_rx: None,
    });

    let (b_pub_tx, _b_pub_rx) = mpsc::channel(8);
    let (_a_sub_tx, a_sub_rx) = mpsc::channel(8);

    registry_a
        .spawn_engine_inner_now(
            community_id,
            mk.clone(),
            admin,
            false,
            a_pub_tx,
            a_sub_rx,
            harmony_app::community_state_sync::CatchUpChannels::none(),
        )
        .await
        .expect("spawn a");
    registry_b
        .spawn_engine_inner_now(
            community_id,
            mk,
            admin,
            false,
            b_pub_tx,
            b_sub_rx,
            harmony_app::community_state_sync::CatchUpChannels::none(),
        )
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
            },
        );
        assert!(matches!(
            outcome,
            harmony_app::community_state_crdt::InsertOutcome::Inserted
        ));
    }

    // Inject the same Join into A so admin's status is `Joined` in
    // A's local state. Then mint a SECOND distinct event on A — a
    // SetPower at a later HLC — so A's publish carries something B
    // hasn't seen. This makes the post-replay event-count assertion
    // meaningful (B should grow from 1 → 2, not stay at 1 due to
    // pre-seed vacuity).
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
            },
        );
        assert!(matches!(
            outcome,
            harmony_app::community_state_crdt::InsertOutcome::Inserted
        ));
    }
    let admin_set_power_event = {
        let payload = EventPayload {
            id: [42u8; 16],
            community_id,
            kind: MembershipEventKind::SetPower {
                target: admin,
                level: 50,
            },
            actor: admin,
            at: Hlc {
                wall_ms: 200,
                logical: 0,
                device_id: "a-dev".into(),
            },
        };
        sign_event_with_identity(&payload, &id_admin).expect("sign set_power")
    };
    {
        let mut sa = state_a.lock().await;
        let outcome = sa.insert_event(
            admin_set_power_event,
            &harmony_app::community_membership::VerifyContext {
                expected_community_id: community_id,
                admin_addr: admin,
                is_invite_only: false,
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

    // Wait for the second event (the SetPower) to merge — the test's
    // invariant is that exactly TWO events survive (1 pre-seeded Join
    // + 1 new SetPower). Without the SetPower the assertion would be
    // vacuous against the pre-seeded Join. Poll until 2 events land,
    // then sleep briefly to give the second delivery a chance to NOT
    // change anything, then re-assert.
    let state_b_for_poll = Arc::clone(&state_b);
    wait_until(
        Duration::from_secs(2),
        Duration::from_millis(10),
        move || {
            let s = Arc::clone(&state_b_for_poll);
            async move { s.lock().await.event_count() == 2 }
        },
    )
    .await;
    // Brief settle so the second delivery has time to be processed
    // and (correctly) hit the tracker-only Duplicate path.
    tokio::time::sleep(Duration::from_millis(50)).await;
    {
        let sb = state_b.lock().await;
        assert_eq!(
            sb.event_count(),
            2,
            "B's CRDT should hold exactly two events after replay \
             (pre-seeded Join + the new SetPower); a third would mean \
             the duplicate publish merged a second time, a count of 1 \
             would mean the publish never landed"
        );
    }

    registry_a.shutdown_all().await.expect("shutdown a");
    registry_b.shutdown_all().await.expect("shutdown b");
}

/// ZEB-256 § 11 acceptance: "Spoofing test demonstrates the censorship
/// attack is no longer possible." Prior to publisher authentication,
/// any member with the shared `EpochKey` could publish a state-
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
///      (Bob has the `EpochKey`) AND passes the membership-at-
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
    let mk = EpochKey::new([0xAA; 32]);

    // Alice — the real, legitimate publisher. Engine A signs publishes
    // with `alice_signing`; both registries' resolvers map
    // `alice_addr → alice_pub` so receive-side sig-verify can rebuild
    // the public key it checks the signature against.
    let id_alice = mint_test_owner(0xa1);
    let alice_addr = id_alice.owner;
    let alice_pub = [0u8; 64];
    let alice_signing = signing_key_from(&id_alice);

    // Bob — the attacker. Bob is also a community member (so Bob has
    // the `EpochKey`), but Bob's signing key is NOT Alice's. The
    // forged publish below is signed with `bob_signing` while claiming
    // `publisher_addr = alice_addr`; verify-on-receive rejects it
    // because `bob_signing` does not match Alice's resolver-resolved
    // identity_pub. The pre-ZEB-256 receive pipeline checked only the
    // shared MK + per-event sigs — Bob's spoof would have advanced B's
    // tracker for `(alice_addr, "a-dev")` to HLC `huge`, censoring
    // every subsequent legitimate Alice publish.
    let id_bob = mint_test_owner(0xb1);
    let bob_addr = id_bob.owner;
    let bob_pub = [0u8; 64];
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
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
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
        crdt_state: None,
        nav_emitter: None,
        presence_resync_rx: None,
    });
    let registry_b = CommunitySyncRegistry::new(CommunityRegistryConfig {
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
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
        crdt_state: None,
        nav_emitter: None,
        presence_resync_rx: None,
    });

    // B's publisher and A's subscriber are unused in this one-way sync
    // test; we still need fresh handles to satisfy `spawn_engine`'s
    // signature.
    let (b_pub_tx, _b_pub_rx) = mpsc::channel(8);
    let (_a_sub_tx, a_sub_rx) = mpsc::channel(8);

    registry_a
        .spawn_engine_inner_now(
            community_id,
            mk.clone(),
            alice_addr,
            false,
            a_pub_tx,
            a_sub_rx,
            harmony_app::community_state_sync::CatchUpChannels::none(),
        )
        .await
        .expect("spawn a");
    registry_b
        .spawn_engine_inner_now(
            community_id,
            mk.clone(),
            alice_addr,
            false,
            b_pub_tx,
            b_sub_rx,
            harmony_app::community_state_sync::CatchUpChannels::none(),
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
    // with the `EpochKey`. Bob crafts a wire packet claiming
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
    let forged_publish = forged_signed.into_wire(forged_sig, None);
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

    // Step 3 — Alice publishes legitimately AGAIN, this time
    // carrying a REAL CRDT mutation. Her engine's `next_hlc` produces
    // HLC₂ with `wall_ms ≈ now` (or a logical bump if same-
    // millisecond) — strictly newer than HLC₁ but vastly less than
    // `huge`.
    //
    // We mint a `SetPower { target: alice_addr, level: 100 }` event
    // through `engine.insert_local_event(...)` rather than relying on
    // `flush_now` to republish an unchanged CRDT. Alice is the
    // community admin (implicit power 100, ≥ POWER_THRESHOLDS.set_power),
    // and `target=alice` keeps the mutation a self-Update — it does
    // NOT change Alice's membership status, so the membership-at-
    // publish-HLC gate still sees her as `Joined` when the resulting
    // publish lands at B.
    //
    // Why not Leave: at HLC₂_publish > HLC₂_leave, B's
    // `prior_state_at_hlc(HLC₂_publish)` would materialize the Leave
    // and reject the publish with `PublisherNotJoined`. SetPower is
    // status-neutral.
    //
    // Why not flush_now alone: `flush_now` republishes whatever CRDT
    // already exists, and the engine today happens not to skip no-op
    // publishes — but anchoring this regression test to a no-op
    // republish couples the security guarantee to a publish-
    // scheduling implementation detail. A real mutation makes the
    // censorship-defense assertion durable across future engine
    // changes (CodeRabbit PR #88 round 2 finding).
    let alice_setpower_event = {
        let payload = EventPayload {
            id: [99u8; 16],
            community_id,
            kind: MembershipEventKind::SetPower {
                target: alice_addr,
                level: 100,
            },
            actor: alice_addr,
            at: Hlc {
                wall_ms: 150,
                logical: 0,
                device_id: "a-dev".into(),
            },
        };
        sign_event_with_identity(&payload, &id_alice).expect("sign alice setpower")
    };
    {
        let engine_a = registry_a
            .engine_arc(&community_id)
            .await
            .expect("engine a spawned");
        let outcome = engine_a
            .insert_local_event(alice_setpower_event)
            .await
            .expect("insert_local_event setpower");
        assert!(
            matches!(
                outcome,
                harmony_app::community_state_crdt::InsertOutcome::Inserted
            ),
            "SetPower insert outcome must be Inserted, got {outcome:?}"
        );
    }
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
/// re-Join publish being admitted end-to-end. The receive-side
/// membership gate uses `prior_state_at_hlc(payload.at)` (NOT
/// the publisher's *current* materialized status) — this is the
/// post-PR-88-bot-round-1 shape. The semantics are still wrong for
/// self-Re-Join, just for a more subtle reason: after Alice's Leave
/// at HLC₂ merges into B, any subsequent re-Join publish from Alice
/// must use a HLC₃ > HLC₂ (HLC monotonicity is enforced engine-side
/// via `next_hlc`). When B materializes the prior state at HLC₃, the
/// Leave at HLC₂ is included, so Alice's status is `Left` and the
/// gate rejects. The gate cannot peek inside the encrypted blob to
/// see Alice's new Join event riding alongside the publish.
///
/// Phase 4's invite flow re-Joins Alice via an admin-issued Invite
/// (separate publisher, gate admits), at which point the surviving
/// tracker entry from this test is what the censorship-defense
/// argument relies on. The same bootstrap edge applies to a brand-
/// new joiner's first self-publish; both are deferred under ZEB-260.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn leave_does_not_prune_per_device_tracker_entry() {
    let cas_tx = spawn_shared_cas();
    let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_tx,
        Duration::from_millis(2000),
    ));

    let community_id = SpaceId([0x77; 16]);
    let mk = EpochKey::new([0x55; 32]);

    // Alice is the admin AND the publisher under test. She'll Join then
    // Leave — exercising the tracker's behavior across a Leave so we
    // can verify her per-device entry survives the membership change.
    let id_alice = mint_test_owner(0xa1);
    let alice_addr = id_alice.owner;
    let alice_pub = [0u8; 64];
    let alice_signing = signing_key_from(&id_alice);

    // B is a passive observer — receives Alice's publishes, surfaces
    // tracker advances. B doesn't publish in this test, but the
    // registry config requires a real signing key + self_owner, so we
    // give B a distinct identity.
    let id_b = mint_test_owner(0xb1);
    let b_owner = id_b.owner;
    let b_signing = signing_key_from(&id_b);

    // Resolver carries both Alice and B's identity_pubs. Receive-side
    // sig-verify on B looks up `alice_addr → alice_pub` to validate
    // every publish A signs.
    let mut resolver_map = std::collections::HashMap::new();
    resolver_map.insert(alice_addr, alice_pub);
    resolver_map.insert(b_owner, [0u8; 64]);
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
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
        device_id: "a-dev".into(),
        content_store: Arc::clone(&cs),
        identity_resolver: Arc::clone(&resolver),
        identity_dir: dir_a.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: None,
        self_owner: alice_addr,
        signing_key: Arc::clone(&alice_signing),
        crdt_state: None,
        nav_emitter: None,
        presence_resync_rx: None,
    });
    let registry_b = CommunitySyncRegistry::new(CommunityRegistryConfig {
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
        device_id: "b-dev".into(),
        content_store: Arc::clone(&cs),
        identity_resolver: Arc::clone(&resolver),
        identity_dir: dir_b.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: None,
        self_owner: b_owner,
        signing_key: Arc::clone(&b_signing),
        crdt_state: None,
        nav_emitter: None,
        presence_resync_rx: None,
    });

    // B never publishes, A never receives — but spawn_engine requires
    // both directions wired with real channels.
    let (b_pub_tx, _b_pub_rx) = mpsc::channel(8);
    let (_a_sub_tx, a_sub_rx) = mpsc::channel(8);

    registry_a
        .spawn_engine_inner_now(
            community_id,
            mk.clone(),
            alice_addr,
            false,
            a_pub_tx,
            a_sub_rx,
            harmony_app::community_state_sync::CatchUpChannels::none(),
        )
        .await
        .expect("spawn a");
    registry_b
        .spawn_engine_inner_now(
            community_id,
            mk.clone(),
            alice_addr,
            false,
            b_pub_tx,
            b_sub_rx,
            harmony_app::community_state_sync::CatchUpChannels::none(),
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
    // Wait for BOTH (a) the Leave event to merge into B's CRDT AND
    // (b) B's tracker entry for (alice, "a-dev") to advance past
    // first_slot. Polling only `events.len() == 2` was racy under
    // CI: handle_incoming_publish merges events under the state
    // lock and records the tracker advance under a separate
    // tracker lock; the test could observe events.len() == 2
    // BEFORE the tracker record landed, then trip line 1771's
    // strictly-newer assertion. (This was an intermittent flake
    // that fired ~10% on the GitHub Linux runners and never
    // locally on M-series Macs.)
    {
        let registry_b_for_poll = &registry_b;
        let first_slot_for_poll = first_slot.clone();
        wait_until(
            Duration::from_secs(2),
            Duration::from_millis(10),
            || async {
                let s = registry_b_for_poll
                    .state_for(&community_id)
                    .await
                    .expect("state b");
                let events_len = {
                    let g = s.lock().await;
                    g.event_count()
                };
                if events_len != 2 {
                    return false;
                }
                let snap = registry_b_for_poll
                    .tracker_snapshot(&community_id)
                    .await
                    .expect("engine b spawned");
                snap.per_device
                    .get(&(alice_addr, "a-dev".to_string()))
                    .map(|slot| slot.is_strictly_newer_than(&first_slot_for_poll))
                    .unwrap_or(false)
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
    let events: Vec<_> = s.lock().await.events().cloned().collect();
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

// ─── ZEB-258 atomic-rollback regression test ────────────────────────
//
// Pins the invariant that `create_community_inner` does NOT mutate
// owner-state CRDT when a downstream step (engine spawn or adapter
// dispatch) fails. The pre-reorder body applied the Community Space
// row to owner-state BEFORE spawning the engine + dispatching the
// adapter, so an adapter-Closed dispatch left an orphan Space row
// committed to owner-state with no engine to publish it. The post-
// reorder body commits the Space row LAST and tears the engine down
// on any earlier failure.
//
// Test shape: invokes `create_community_inner` DIRECTLY with a
// closed adapter channel. The helper takes `&Mutex<NodeState>` (not
// `tauri::State`), so the test constructs a fresh
// `Mutex<NodeState>` and passes a borrow. The byte-identity
// assertion is now load-bearing: a regression that re-introduced the
// pre-reorder shape (apply_space FIRST) would mutate `crdt_state`
// inside `create_community_inner` BEFORE the failing
// `community_adapter_tx.try_send`, and the post-snapshot bytes would
// differ.
#[tokio::test]
async fn create_community_atomic_rollback_on_adapter_dispatch_failure() {
    use harmony_app::community_channel_log_engine::{
        ChannelLogEngineConfig, ChannelLogRegistry, ChannelLogRegistryConfig,
    };
    use harmony_app::community_state_sync::{
        CommunityRegistryConfig, CommunitySyncRegistry, IdentityResolver, DEFAULT_DEBOUNCE_MS,
    };
    use harmony_app::content_store::{ContentStore, RuntimeContentStore};
    use harmony_app::owner_state_crdt::OwnerState;
    use harmony_app::owner_state_persist::canonicalize;

    struct NopResolver;
    #[async_trait::async_trait]
    impl IdentityResolver for NopResolver {
        async fn resolve(&self, _: &OwnerAddr) -> Option<[u8; 64]> {
            None
        }
    }

    // Closed adapter receiver → the matching `try_send` inside
    // `create_community_inner` returns Closed. This is the fault we
    // inject to drive the rollback path.
    let (adapter_tx, adapter_rx) =
        mpsc::channel::<harmony_app::event_loop::CommunityAdapterRequest>(1);
    drop(adapter_rx);

    // Minimal CAS wiring — the rollback path never reaches the engine's
    // publish loop, so no real CAS traffic is exchanged. Keep the
    // receiver alive for clarity even though nothing should be sent on
    // it.
    let (cas_op_tx, _cas_op_rx) = mpsc::channel(8);
    let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx,
        Duration::from_millis(1000),
    ));

    let identity = mint_test_owner(0xab);
    let self_owner = identity.owner;
    let signing_key = signing_key_from(&identity);

    let dir = tempfile::tempdir().expect("tempdir");
    // ZEB-790: one adoption floor per simulated node — this test models a
    // single node ("test-dev"), so the registry, channel-log registry, and
    // create_community_inner all share ONE floor.
    let adopt_floor = harmony_app::hlc_adopt_floor::HlcAdoptFloor::new();
    let registry = Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
        adopt_floor: adopt_floor.clone(),
        device_id: "test-dev".into(),
        content_store: cs,
        identity_resolver: Arc::new(NopResolver),
        identity_dir: dir.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: None,
        self_owner,
        signing_key: Arc::clone(&signing_key),
        crdt_state: None,
        nav_emitter: None,
        presence_resync_rx: None,
    }));

    // ZEB-271: ChannelLogRegistry required by the new create_community_inner
    // signature. Build a minimal instance with a dummy adapter bridge — no
    // Zenoh session needed since this test returns before any channel-log
    // spawns occur (adapter dispatch fails first).
    let (channel_log_adapter_tx, _channel_log_adapter_rx) =
        mpsc::unbounded_channel::<harmony_app::event_loop::ChannelLogAdapterRequest>();
    let channel_log_registry = ChannelLogRegistry::new(ChannelLogRegistryConfig {
        adapter_request_tx: channel_log_adapter_tx,
        sink: Arc::new(harmony_app::node_event_sink::FanoutSink(vec![])),
        identity_dir: dir.path().to_path_buf(),
        self_owner,
        self_device_id: "test-dev".into(),
        signing_key: Arc::clone(&signing_key),
        adopt_floor: adopt_floor.clone(),
        engine_config: ChannelLogEngineConfig::default(),
        transport_epoch_rx: None,
        // ZEB-599 Direction 1: no presence watch in this integration harness.
        presence_resync_rx: None,
    });

    // Pre-call snapshot of owner-state's canonical byte encoding. Any
    // mutation between here and the post-call snapshot would change at
    // least one byte (the schema version byte stays put, but the body
    // CBOR encodes the spaces map, which Phase 1's `apply_space_with_
    // canonicalization` would non-trivially mutate).
    let crdt_state = Arc::new(Mutex::new(OwnerState::default()));
    let hlc_tracker = Arc::new(Mutex::new(harmony_crdt_sync::ReplayTracker::new(
        "test-dev".to_string(),
    )));
    let pre_bytes: Vec<u8> = {
        let g = crdt_state.lock().await;
        canonicalize(&g).expect("encode pre-state")
    };

    // Fence-state. The adapter-dispatch-failure path RETURNS BEFORE
    // the helper's snapshot-then-commit fence is reached, so the
    // contents of NodeState don't matter for THIS test (only the
    // helper's signature requires the borrow). A bare default works.
    let node_state = std::sync::Mutex::new(harmony_app::NodeState::default());

    // Drive `create_community_inner` directly. With a closed adapter
    // channel, the helper:
    //   1. mints Space + bootstrap_join (no side effects)
    //   2. ZEB-271: opens a CommunityTransactionGuard
    //   3. spawns the engine (success)
    //   4. `community_adapter_tx.try_send` → Closed → rollback branch:
    //      `community_registry.shutdown_engine_and_cleanup_persistence`
    //      then `return Err(...)` (guard dropped → safety-net abort)
    // crdt_state is not touched on this branch.
    let result = harmony_app::create_community_inner(
        "TestCommunity".into(),
        false,
        Arc::clone(&crdt_state),
        Arc::clone(&hlc_tracker),
        adopt_floor.clone(),
        "test-dev".into(),
        self_owner,
        Arc::clone(&signing_key),
        identity.cert.clone(),
        Arc::clone(&registry),
        adapter_tx,
        None, // ZEB-434: no transport-epoch watch in this test
        channel_log_registry,
        0, // snapshot_generation; fence not reached on this path
        &node_state,
    )
    .await;
    assert!(
        result.is_err(),
        "create_community_inner must return Err when the adapter channel is closed"
    );

    // ZEB-258 invariant: owner-state CRDT byte-identical to pre-call
    // snapshot. The post-reorder body only reaches the apply_space
    // step after every fallible step has succeeded; this test wedges
    // a Closed adapter dispatch in BEFORE that step, so a correctly
    // ordered helper leaves crdt_state untouched. A regression that
    // re-introduced the pre-reorder shape (apply_space FIRST) would
    // commit `minted.space` to crdt_state before the try_send error,
    // and the post-snapshot bytes would diverge.
    let post_bytes: Vec<u8> = {
        let g = crdt_state.lock().await;
        canonicalize(&g).expect("encode post-state")
    };
    assert_eq!(
        pre_bytes, post_bytes,
        "ZEB-258: owner-state CRDT must be byte-identical pre/post a \
         failed create_community_inner (orphan Space row would prove \
         the reorder didn't land)"
    );

    registry.shutdown_all().await.expect("shutdown");
}

// ── ZEB-262 Phase 4 Task 3: kick + set_power happy-path round-trips ───
//
// Two-engine CRDT round-trip tests for kick_from_community and
// set_power_level. Mirrors the setup pattern of
// `community_open_flow_integration.rs::open_community_create_redeem_leave_round_trip`:
// shared in-memory CAS + Reticulum-style mpsc forwarders + two
// `CommunitySyncEngine` instances + bootstrap-Join pre-seed for both
// peers (covers cold-cache transient rejection per ZEB-256 §5).
//
// After both engines hold {admin Join, B's redemption Join} we mint the
// kick/set_power event using the new `mint_kick_event` /
// `mint_set_power_event` pure helpers, insert via engine_a, and assert
// that B's local materialized state converges to the expected shape.

mod task3_kick_setpower_round_trip {
    use super::*;
    use harmony_app::community_membership::{materialize, MaterializedMembership, MemberStatus};
    use harmony_app::community_state_crdt::InsertOutcome;
    use harmony_app::community_state_sync::{
        CommunityMembershipDelta, CommunityReplayTracker, CommunitySyncEngine,
        CommunitySyncEngineConfig, PersistPaths,
    };
    use harmony_app::dm_outbox::reserve_next_hlc_for_device;
    use harmony_app::{
        mint_community_creation, mint_kick_event, mint_redemption, mint_set_power_event,
    };
    use std::collections::{BTreeMap, HashMap};

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

    async fn wait_until<F, Fut>(mut cond: F, timeout: Duration) -> bool
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if cond().await {
                return true;
            }
            if tokio::time::Instant::now() > deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Two-engine fixture: shared CAS, paired mpsc forwarders, A's
    /// bootstrap Join + B's redemption Join converged on both peers.
    /// Returns the engines, states, identity material, and minted-B
    /// payload so callers can mint kick / set_power events with a
    /// monotonic `prev_hlc` reference.
    struct Fixture {
        engine_a: CommunitySyncEngine,
        engine_b: CommunitySyncEngine,
        // state_a is held so the Arc clone passed to engine_a stays
        // alive; the assertions only inspect state_b. Underscore-
        // prefixed to silence dead_code without dropping the binding.
        _state_a: Arc<Mutex<CommunityState>>,
        state_b: Arc<Mutex<CommunityState>>,
        owner_a: OwnerAddr,
        owner_b: OwnerAddr,
        signing_a: Arc<ed25519_dalek::SigningKey>,
        community_id: SpaceId,
        minted_a_join_hlc: Hlc,
        // ZEB-790: node A's single adoption floor. engine_a mints/feeds through
        // it, and the kick/power tests reserve A-authored HLCs through the SAME
        // floor (all node A). engine_b holds its own separate floor (node B).
        adopt_floor_a: harmony_app::hlc_adopt_floor::HlcAdoptFloor,
        // Hold the temp dirs for the lifetime of the fixture so the
        // engines' persistence files don't disappear mid-test.
        _tmp_a: tempfile::TempDir,
        _tmp_b: tempfile::TempDir,
    }

    async fn build_fixture(seed_a: u8, seed_b: u8) -> Fixture {
        let identity_a = mint_test_owner(seed_a);
        let identity_b = mint_test_owner(seed_b);
        let owner_a = identity_a.owner;
        let owner_b = identity_b.owner;
        let pub_a = [0u8; 64];
        let pub_b = [0u8; 64];
        let signing_a = signing_key_from(&identity_a);
        let signing_b = signing_key_from(&identity_b);

        let resolver: Arc<dyn IdentityResolver> = Arc::new(TwoIdentityResolver {
            a: (owner_a, pub_a),
            b: (owner_b, pub_b),
        });

        // Shared in-memory CAS servicer.
        let cas: Arc<Mutex<HashMap<harmony_content::cid::ContentId, Vec<u8>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let (cas_op_tx, mut cas_op_rx) = mpsc::channel(64);
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

        // Wire: A↔B publish/subscribe forwarders.
        let (a_out_tx, mut a_out_rx) = mpsc::channel::<Vec<u8>>(64);
        let (a_in_tx, a_in_rx) = mpsc::channel::<Vec<u8>>(64);
        let (b_out_tx, mut b_out_rx) = mpsc::channel::<Vec<u8>>(64);
        let (b_in_tx, b_in_rx) = mpsc::channel::<Vec<u8>>(64);
        let a_in_for_fwd = a_in_tx.clone();
        tokio::spawn(async move {
            while let Some(bytes) = b_out_rx.recv().await {
                let _ = a_in_for_fwd.send(bytes).await;
            }
        });
        let b_in_for_fwd = b_in_tx.clone();
        tokio::spawn(async move {
            while let Some(bytes) = a_out_rx.recv().await {
                let _ = b_in_for_fwd.send(bytes).await;
            }
        });

        // A mints a fresh community + bootstrap Join.
        let minted_a = mint_community_creation(
            "TestCommunity",
            false,
            owner_a,
            &signing_a,
            &identity_a.cert,
            Hlc {
                wall_ms: 100_000,
                logical: 0,
                device_id: "a-dev".to_string(),
            },
        )
        .expect("mint create");
        let community_id = minted_a.community_id;

        let cs_a: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
            cas_op_tx.clone(),
            Duration::from_secs(2),
        ));
        let cs_b: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
            cas_op_tx.clone(),
            Duration::from_secs(2),
        ));

        let state_a = Arc::new(Mutex::new(CommunityState::new(community_id)));
        let state_b = Arc::new(Mutex::new(CommunityState::new(community_id)));
        let tracker_a = Arc::new(Mutex::new(CommunityReplayTracker::new((
            owner_a,
            "a-dev".to_string(),
        ))));
        let tracker_b = Arc::new(Mutex::new(CommunityReplayTracker::new((
            owner_b,
            "b-dev".to_string(),
        ))));

        let (delta_a_tx, mut delta_a_rx) = mpsc::channel::<CommunityMembershipDelta>(32);
        let (delta_b_tx, mut delta_b_rx) = mpsc::channel::<CommunityMembershipDelta>(32);

        let tmp_a = tempfile::tempdir().expect("tmp a");
        let tmp_b = tempfile::tempdir().expect("tmp b");

        let adopt_floor_a = harmony_app::hlc_adopt_floor::HlcAdoptFloor::new();
        let engine_a = CommunitySyncEngine::new(CommunitySyncEngineConfig {
            adopt_floor: adopt_floor_a.clone(),
            community_id,
            membership_key: minted_a.membership_key.clone(),
            admin_addr: owner_a,
            is_invite_only: false,
            device_id: "a-dev".into(),
            self_owner: owner_a,
            signing_key: Arc::clone(&signing_a),
            state: Arc::clone(&state_a),
            tracker: Arc::clone(&tracker_a),
            content_store: cs_a,
            publisher_tx: a_out_tx,
            subscriber_rx: a_in_rx,
            paths: PersistPaths {
                crdt: tmp_a.path().join("crdt.cbor"),
                replay: tmp_a.path().join("replay.cbor"),
            },
            debounce_ms: DEFAULT_DEBOUNCE_MS,
            identity_resolver: Some(Arc::clone(&resolver)),
            error_tx: None,
            delta_tx: Some(delta_a_tx),
            pending_redemptions: None,
            crdt_state: None,
            admin_identity_pub: None,
            nav_emitter: None,
            root_serve_rx: None,
        });
        let engine_b = CommunitySyncEngine::new(CommunitySyncEngineConfig {
            adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
            community_id,
            membership_key: minted_a.membership_key.clone(),
            admin_addr: owner_a,
            is_invite_only: false,
            device_id: "b-dev".into(),
            self_owner: owner_b,
            signing_key: Arc::clone(&signing_b),
            state: Arc::clone(&state_b),
            tracker: Arc::clone(&tracker_b),
            content_store: cs_b,
            publisher_tx: b_out_tx,
            subscriber_rx: b_in_rx,
            paths: PersistPaths {
                crdt: tmp_b.path().join("crdt.cbor"),
                replay: tmp_b.path().join("replay.cbor"),
            },
            debounce_ms: DEFAULT_DEBOUNCE_MS,
            identity_resolver: Some(Arc::clone(&resolver)),
            error_tx: None,
            delta_tx: Some(delta_b_tx),
            pending_redemptions: None,
            crdt_state: None,
            admin_identity_pub: None,
            nav_emitter: None,
            root_serve_rx: None,
        });

        // Step 1: A inserts its bootstrap Join.
        let outcome = engine_a
            .insert_local_event(minted_a.bootstrap_join.clone())
            .await
            .expect("A bootstrap insert");
        assert_eq!(outcome, InsertOutcome::Inserted);
        let _ = tokio::time::timeout(Duration::from_secs(1), delta_a_rx.recv()).await;

        // ZEB-256 Task 6 cold-cache simulation: B insert-locals A's
        // bootstrap Join so B's membership-at-HLC gate admits A's
        // first publish.
        let _ = engine_b
            .insert_local_event(minted_a.bootstrap_join.clone())
            .await
            .expect("B insert A's bootstrap Join (pre-seed)");
        assert!(
            wait_until(
                || async { state_b.lock().await.event_count() == 1 },
                Duration::from_secs(10),
            )
            .await,
            "B should hold A's bootstrap Join"
        );
        let _ = tokio::time::timeout(Duration::from_secs(2), delta_b_rx.recv()).await;

        // Step 2: B redeems an open invite for the same community.
        let invite_payload = harmony_app::community_invite::CommunityInvitePayload {
            inviter_signer_certs: Vec::new(),
            community_id,
            epoch_snapshot: harmony_app::community_invite::InviteEpochSnapshot {
                epoch: 0,
                sealed_epoch_key: minted_a.membership_key.as_bytes().to_vec(),
                sealed_epoch_keys: Vec::new(),
                state_snapshot: harmony_app::community_invite::MaterializedCommunityState::default(
                ),
            },
            admin_addr: owner_a,
            community_name: "TestCommunity".into(),
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
        let minted_b = mint_redemption(
            &invite_payload,
            owner_b,
            &signing_b,
            &identity_b.cert,
            Hlc {
                wall_ms: 200_000,
                logical: 0,
                device_id: "b-dev".to_string(),
            },
        )
        .expect("mint redeem");
        let redemption_outcome = engine_b
            .insert_local_event(minted_b.bootstrap_join.clone())
            .await
            .expect("B redemption insert");
        assert_eq!(redemption_outcome, InsertOutcome::Inserted);
        let _ = tokio::time::timeout(Duration::from_secs(1), delta_b_rx.recv()).await;

        // ZEB-256 Task 6: A insert-locals B's redemption Join too.
        let _ = engine_a
            .insert_local_event(minted_b.bootstrap_join.clone())
            .await
            .expect("A insert B's redemption Join");
        assert!(
            wait_until(
                || async { state_a.lock().await.event_count() == 2 },
                Duration::from_secs(10),
            )
            .await,
            "A should hold its own Join + B's redemption Join"
        );
        let _ = tokio::time::timeout(Duration::from_secs(2), delta_a_rx.recv()).await;

        Fixture {
            engine_a,
            engine_b,
            _state_a: state_a,
            state_b,
            owner_a,
            owner_b,
            signing_a,
            community_id,
            minted_a_join_hlc: minted_a.bootstrap_join.at.clone(),
            adopt_floor_a,
            _tmp_a: tmp_a,
            _tmp_b: tmp_b,
        }
    }

    /// Two-engine kick happy path: admin (A) kicks Bob (B). The Kick
    /// event materialises on both A and B as MemberStatus::Banned for B.
    #[tokio::test]
    async fn admin_kicks_member_round_trip() {
        let f = build_fixture(0xa1, 0xb2).await;

        // ZEB-267: derive the kick's HLC via the same helper production
        // uses, with a local tracker pre-seeded to A's bootstrap join
        // (A is signing the kick, so "a-dev" must track A-authored
        // HLCs). The wall-clock advance to 300_000 dominates anyway,
        // so the resulting kick HLC sorts strictly after both A's
        // bootstrap (100_000) and B's redemption (200_000). Avoids
        // hand-rolling next_hlc's wall-regression / logical-bump logic
        // at the test boundary (Greptile + CodeRabbit review).
        let kick_tracker = Arc::new(Mutex::new({
            let mut m = BTreeMap::<String, Hlc>::new();
            m.insert("a-dev".to_string(), f.minted_a_join_hlc.clone());
            m
        }));
        let kick_hlc =
            reserve_next_hlc_for_device(&kick_tracker, &f.adopt_floor_a, "a-dev", 300_000).await;
        let kick = mint_kick_event(
            f.community_id,
            f.owner_a,
            f.owner_b,
            Some("test-kick".into()),
            &f.signing_a,
            kick_hlc,
        )
        .expect("mint kick");

        let outcome = f
            .engine_a
            .insert_local_event(kick.clone())
            .await
            .expect("A insert kick");
        assert_eq!(outcome, InsertOutcome::Inserted);

        // Wait for B to converge on 3 events (admin Join + B Join + Kick).
        assert!(
            wait_until(
                || async { f.state_b.lock().await.event_count() == 3 },
                Duration::from_secs(10),
            )
            .await,
            "B should receive the Kick"
        );

        // Both peers' materialized state should show B as Banned.
        let events_b: Vec<_> = {
            let s = f.state_b.lock().await;
            s.events().cloned().collect()
        };
        let mat_b: MaterializedMembership = materialize(&events_b, f.owner_a);
        assert_eq!(
            mat_b.members.get(&f.owner_b).map(|m| m.status),
            Some(MemberStatus::Banned),
            "B's local materialization must show Bob as Banned after Kick converges"
        );

        f.engine_a.shutdown().await.expect("shutdown a");
        f.engine_b.shutdown().await.expect("shutdown b");
    }

    /// Two-engine set_power happy path: admin (A) promotes Bob (B) to
    /// power 50. After convergence B's materialization shows
    /// power_levels[Bob] == 50.
    #[tokio::test]
    async fn admin_sets_power_round_trip() {
        let f = build_fixture(0xa3, 0xb4).await;

        // ZEB-267: same HLC derivation as the kick test above —
        // pre-seed "a-dev" with A's bootstrap join, not B's, so the
        // tracker entry tracks the correct device's authored HLCs.
        let promo_tracker = Arc::new(Mutex::new({
            let mut m = BTreeMap::<String, Hlc>::new();
            m.insert("a-dev".to_string(), f.minted_a_join_hlc.clone());
            m
        }));
        let promo_hlc =
            reserve_next_hlc_for_device(&promo_tracker, &f.adopt_floor_a, "a-dev", 300_000).await;
        let promo = mint_set_power_event(
            f.community_id,
            f.owner_a,
            f.owner_b,
            50,
            &f.signing_a,
            promo_hlc,
        )
        .expect("mint set_power");

        let outcome = f
            .engine_a
            .insert_local_event(promo.clone())
            .await
            .expect("A insert set_power");
        assert_eq!(outcome, InsertOutcome::Inserted);

        assert!(
            wait_until(
                || async { f.state_b.lock().await.event_count() == 3 },
                Duration::from_secs(10),
            )
            .await,
            "B should receive the SetPower"
        );

        let events_b: Vec<_> = {
            let s = f.state_b.lock().await;
            s.events().cloned().collect()
        };
        let mat_b: MaterializedMembership = materialize(&events_b, f.owner_a);
        assert_eq!(
            mat_b.power_levels.get(&f.owner_b),
            Some(&50),
            "B's local materialization must show power_levels[Bob] == 50 after SetPower converges"
        );

        f.engine_a.shutdown().await.expect("shutdown a");
        f.engine_b.shutdown().await.expect("shutdown b");
    }
}

// ── ZEB-500: invite-only redeem when the inviter is UNREACHABLE ──────────
//
// Post-ZEB-474 (Reticulum carrier teardown) + ZEB-254 (offline
// counter-signer queue), an unreachable/timed-out inviter no longer rolls
// back the redemption — it COMMITS the owner-state Space (a durable
// latched-pending join) and returns Ok. This supersedes the pre-ZEB-474
// "rolls back when inviter unreachable" behavior this test was originally
// named for (ZEB-258-era). ZEB-500 has the full migration rationale and the
// confirmation that the commit is NOT a ZEB-258 regression.
//
// `build_unreachable_invite_only_redeem_fixture` builds a gate-passing
// invite-only URL (ZEB-497: Alice is a consistent enrolled-device owner so
// `verify_inviter_enrollment` passes) plus Bob's redeem-side deps, with NO
// reachable destination for the inviter. Two tests drive it with different
// `fence_check` closures:
//
//   * `redeem_invite_only_commits_pending_join_when_inviter_unreachable`
//     (fence = Ok)  — asserts the latched-pending COMMIT (pending=true).
//   * `redeem_invite_only_rolls_back_owner_state_on_fence_failure`
//     (fence = Err) — asserts the ZEB-258 owner-state rollback invariant
//     still holds for a GENUINE failure (the Space commit is the last
//     persistent step; a fence Err before it leaves owner-state untouched).

/// Resolver mapping the inviter's `OwnerAddr` to a placeholder pubkey. Under
/// the enrolled-device model the redeem verify path resolves the inviter's
/// signer from her materialized cert, not from this pub, and these tests stop
/// at the unreachable branch before any CRDT-sync round-trip would consult it.
struct UnreachableInviterResolver {
    addr: OwnerAddr,
    pubkey: [u8; 64],
}
#[async_trait::async_trait]
impl IdentityResolver for UnreachableInviterResolver {
    async fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
        if *addr == self.addr {
            Some(self.pubkey)
        } else {
            None
        }
    }
}

/// Everything a `redeem_invite_inner` call needs, minus the `fence_check`
/// closure (which the two tests vary). Built fresh per test — `url`,
/// `adapter_tx`, and `channel_log_registry` are consumed by the redeem call.
#[allow(dead_code)] // several fields are RAII-only (held alive, never read)
struct UnreachableRedeemFixture {
    url: String,
    crdt_state: Arc<Mutex<harmony_app::owner_state_crdt::OwnerState>>,
    hlc_tracker: Arc<Mutex<harmony_crdt_sync::ReplayTracker<String, Hlc>>>,
    registry: Arc<CommunitySyncRegistry>,
    adapter_tx: mpsc::Sender<harmony_app::event_loop::CommunityAdapterRequest>,
    dm_outbox: Arc<Mutex<harmony_app::dm_outbox::DmOutbox>>,
    channel_log_registry: Arc<harmony_app::community_channel_log_engine::ChannelLogRegistry>,
    // ZEB-790: Bob's single adoption floor — this fixture models one node
    // (Bob), so the registry, channel-log registry, and both redeem calls
    // share ONE floor.
    adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor,
    bob_owner: TestOwner,
    bob_signing_key: Arc<ed25519_dalek::SigningKey>,
    community_id: SpaceId,
    pre_bytes: Vec<u8>,
    _adapter_rx: mpsc::Receiver<harmony_app::event_loop::CommunityAdapterRequest>,
    _channel_log_adapter_rx:
        mpsc::UnboundedReceiver<harmony_app::event_loop::ChannelLogAdapterRequest>,
    _dir: tempfile::TempDir,
}

async fn build_unreachable_invite_only_redeem_fixture() -> UnreachableRedeemFixture {
    use ed25519_dalek::Signer;
    use harmony_app::community_channel_log_engine::{
        ChannelLogEngineConfig, ChannelLogRegistry, ChannelLogRegistryConfig,
    };
    use harmony_app::community_invite::{
        encode_invite_url, CommunityInvitePayload, InviteEpochSnapshot, InviteToken,
        MaterializedCommunityState,
    };
    use harmony_app::dm_outbox::DmOutbox;
    use harmony_app::owner_state_crdt::OwnerState;
    use harmony_app::owner_state_persist::canonicalize;
    use harmony_app::owner_state_types::DeviceIdentityHash;

    // ZEB-501: the redeem oneshot now fires ONLY on a real JoinCountersign, so an
    // unreachable inviter (no countersign) genuinely reaches the step-7d timeout.
    // Both tests drive that timeout with a short `redeem_timeout` passed via
    // `RedeemInviteOverrides` — NOT the process-global
    // HARMONY_REDEEM_INVITE_TIMEOUT_MS env var, so there is no cross-test env
    // race (the one Qodo/CodeAnt flagged on #293). ZEB-633: 2s, not 50ms — the
    // 50ms budget flaked once under a full-parallelism sweep (budget ≈ scheduler
    // starvation window); the duration is semantically irrelevant (the inviter
    // is unreachable, nothing can ever arrive), so a load-proof budget costs
    // only wall-clock. If it EVER flakes at 2s, capture full output (no tail
    // pipes).

    // ZEB-497/ZEB-500: Alice (the inviter/admin) is a consistent ENROLLED-DEVICE
    // owner so the redeem path's `verify_inviter_enrollment` gate PASSES — her
    // cert's owner_id == invite_token.inviter, the cert is a Master cert that
    // verifies unexpired, and the token sig verifies against the cert's device
    // key. With the gate passed, the redeem proceeds PAST it to the
    // inviter-unreachable timeout branch (the gate would otherwise fail-fast on a
    // mismatched throwaway cert). The InviteToken + bootstrap Join are signed by
    // Alice's enrolled device_key (NOT a PrivateIdentity). Seed 0xA7 is unique
    // here: master key [0xA7;32], device key [0x58;32] (=0xA7^0xFF) — no collision
    // with bob_owner 0xB2 (→0x4D) or the 0xA3 throwaway (→0x5C) in this test.
    let alice = mint_test_owner(0xA7);
    let alice_addr = alice.owner;
    // ZEB-339: Bob (the joiner) is an enrolled-device owner — his redemption
    // Join's actor = owner_id, signed by his device key, carrying his Master
    // cert (passed into redeem_invite_inner). A separate PrivateIdentity backs
    // the DM-layer DmOutbox plumbing (unrelated to community Join verification).
    let bob_owner = mint_test_owner(0xB2);
    let bob_addr = bob_owner.owner;
    let bob_signing_key = signing_key_from(&bob_owner);
    let bob = Arc::new(PrivateIdentity::from_seed(&[0xb2; 32]));
    let bob_device_hash = DeviceIdentityHash(bob.identity.address_hash);

    // Build an invite-only URL Alice would have generated for Bob. The
    // InviteToken sig is computed via Alice's enrolled `device_key`
    // (ZEB-497: the gate's `verify_invite_token_sig_device_key` checks the sig
    // against the device key bound in `inviter_enrollment`) over the canonical
    // token-payload bytes (inviter, invitee_hint, minted_at). We construct a
    // placeholder InviteToken to call `canonical_invite_token_bytes` (which only
    // reads the payload fields, ignoring `sig`), then re-construct with the real
    // sig.
    let community_id = SpaceId([0x33; 16]);
    let mk = EpochKey::new([0xaa; 32]);
    let minted_at = Hlc {
        wall_ms: 1000,
        logical: 0,
        device_id: "alice-dev".into(),
    };
    let placeholder_token = InviteToken {
        inviter: alice_addr,
        invitee_hint: Some(bob_addr),
        minted_at: minted_at.clone(),
        expires_at: None,
        sig: [0u8; 64],
    };
    let token_payload_bytes =
        harmony_app::community_invite::canonical_invite_token_bytes(&placeholder_token)
            .expect("canonical_invite_token_bytes");
    let token_sig = alice.device_key.sign(&token_payload_bytes).to_bytes();
    let invite_token = InviteToken {
        inviter: alice_addr,
        invitee_hint: Some(bob_addr),
        minted_at,
        expires_at: None,
        sig: token_sig,
    };
    // Build Alice's signed self-Join (the admin bootstrap event). This
    // is required for invite-only payloads since ZEB-260 Phase 4: without
    // it, encode_invite_url rejects with InviteOnlyMissingBootstrap, and
    // verify_admin_bootstrap would reject on the reader side.
    let admin_bootstrap = {
        let payload = harmony_app::community_membership::EventPayload {
            id: [0x10; 16],
            community_id,
            kind: harmony_app::community_membership::MembershipEventKind::Join,
            actor: alice_addr,
            at: Hlc {
                wall_ms: 900,
                logical: 0,
                device_id: "alice-dev".into(),
            },
        };
        // ZEB-497: sign with Alice's enrolled device_key (not a PrivateIdentity)
        // and attach HER OWN Master cert so the bootstrap Join is consistent with
        // the inviter_enrollment cert the redeem gate now verifies.
        let mut ev = sign_event(&payload, &alice.device_key).expect("sign admin bootstrap");
        // ZEB-339: encode_invite_url requires the bootstrap-Join to embed the
        // admin's EnrollmentCert. ZEB-497: use Alice's consistent cert
        // (cert.owner_id == alice_addr) rather than a throwaway.
        ev.enrollment = Some(alice.cert.clone());
        ev
    };
    // CR Major (PR #106 R6): use a real sealed_epoch_key so the snapshot
    // decrypts successfully — a redeem outcome must reflect the
    // inviter-unreachable / fence path under test, not an AEAD failure.
    // Seal `mk` to Bob's x25519 pubkey (derived from Bob's ed25519 signing key).
    let sealed_epoch_key = {
        use harmony_app::dm_signing::{ed25519_pub_to_x25519, seal_to_owner};
        let bob_pub32 = bob_signing_key.verifying_key().to_bytes();
        let x25519_pub = ed25519_pub_to_x25519(&bob_pub32).expect("ed25519_pub_to_x25519");
        seal_to_owner(&x25519_pub, mk.as_bytes()).expect("seal_to_owner")
    };
    assert_eq!(
        sealed_epoch_key.len(),
        92,
        "sealed_epoch_key for invite-only must be 92 bytes"
    );
    let url = encode_invite_url(&CommunityInvitePayload {
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
        community_name: "Test".into(),
        is_invite_only: true,
        expires_at: None,
        invite_token: Some(invite_token),
        admin_bootstrap: Some(admin_bootstrap),
        // ZEB-339: admin_identity_pub is inert on the post-ZEB-339 verify path
        // (verify_admin_bootstrap binds the cert, not this pub) but is required
        // present at encode. Pass an inert placeholder.
        admin_identity_pub: Some([0u8; 64]),
        forked_from: None,
        pre_fork_snapshot: None,
        // ZEB-339: invite-only payloads must carry the inviter's EnrollmentCert.
        // ZEB-497/ZEB-500: the redeem path cryptographically verifies this cert
        // (verify_inviter_enrollment) BEFORE the inviter-unreachable timeout
        // branch. Alice is now a consistent enrolled-device owner
        // (mint_test_owner(0xA7)), so the gate PASSES (cert.owner_id ==
        // invite_token.inviter == alice_addr, Master cert verifies, token sig
        // verifies against the cert's device key) and the redeem proceeds to the
        // unreachable timeout branch — see the assertions below for the current
        // latched-pending behavior.
        inviter_enrollment: Some(alice.cert.clone()),
        untargeted_decrypt_key: None,
    })
    .expect("encode URL");

    // Bob's side: registry + crdt + tracker.
    //
    // ZEB-260: with the admin-bootstrap insert added in redeem_invite_inner,
    // any insert path triggers `notify_dirty` → on shutdown the engine
    // task tries to `publish_root_now` → `content_store.put().await` waits
    // on a oneshot reply from the CAS event loop. Without a CAS servicer,
    // that await blocks forever and the rollback hangs. Mirror the stub
    // servicer pattern from `task6_admin_kicks_member_round_trip` (line
    // 2083) — drain CasOps and reply success / None for the test fixture.
    let (cas_op_tx, mut cas_op_rx) = mpsc::channel::<CasOp>(8);
    tokio::spawn(async move {
        while let Some(op) = cas_op_rx.recv().await {
            match op {
                CasOp::PutLocal { reply, .. } => {
                    if let Some(r) = reply {
                        let _ = r.send(Ok(()));
                    }
                }
                CasOp::GetOrFetch { reply, .. } => {
                    let _ = reply.send(Ok(None));
                }
                CasOp::GetLocal { reply, .. } => {
                    let _ = reply.send(None);
                }
                CasOp::AllowServeSubtree { reply, .. } => {
                    let _ = reply.send(Ok(0));
                }
            }
        }
    });
    let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx,
        Duration::from_millis(1000),
    ));
    let dir = tempfile::tempdir().expect("tempdir");
    let adopt_floor = harmony_app::hlc_adopt_floor::HlcAdoptFloor::new();
    let registry = Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
        adopt_floor: adopt_floor.clone(),
        device_id: "bob-dev".into(),
        content_store: cs,
        // ZEB-497: under the enrolled-device model the redeem verify path resolves
        // Alice's signer from her materialized cert, not from this resolver pub;
        // and this test stops at the unreachable timeout branch before any
        // CRDT-sync round-trip would consult it. Pass an inert placeholder pub.
        identity_resolver: Arc::new(UnreachableInviterResolver {
            addr: alice_addr,
            pubkey: [0u8; 64],
        }),
        identity_dir: dir.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: None,
        self_owner: bob_addr,
        signing_key: Arc::clone(&bob_signing_key),
        crdt_state: None,
        nav_emitter: None,
        presence_resync_rx: None,
    }));

    let crdt_state = Arc::new(Mutex::new(OwnerState::default()));
    let hlc_tracker = Arc::new(Mutex::new(harmony_crdt_sync::ReplayTracker::new(
        "bob-dev".to_string(),
    )));

    // ZEB-473 (Move 1a): the unicast send channel was removed from
    // `redeem_invite_inner` with the Reticulum carrier; redemption proceeds via
    // CRDT state-root sync. No admin engine is present here, so no real
    // JoinCountersign arrives — see the per-test assertions and ZEB-501 for what
    // satisfies the redeem oneshot regardless.

    // Adapter request channel — kept alive so the spawn-side dispatch
    // doesn't fail Closed. (We don't need the event_loop on the other
    // side; the test never requires the adapter task to actually run.)
    let (adapter_tx, _adapter_rx) =
        mpsc::channel::<harmony_app::event_loop::CommunityAdapterRequest>(64);

    // dm_outbox for the inner helper to read `private_identity` +
    // `signing_key` under-lock. The DmOutbox::new signature matches
    // production; we share `bob` via Arc.
    // ZEB-339: use Bob's real owner material (bob_owner, seed 0xB2) for the
    // DmOutbox community_signing_key + enrollment_cert so debug_assert in
    // DmOutbox::new passes (cert.owner_id == bob_addr.0). The dual-identity
    // pattern here: bob_signing_key (device #3, Reticulum transport) is
    // separate from community_signing_key (device #2, enrolled key from
    // bob_owner) — both owned by Bob, different key roles.
    let bob_community_sk_sync = Arc::new(ed25519_dalek::SigningKey::from_bytes(
        &bob_owner.device_key.to_bytes(),
    ));
    let bob_enrollment_sync = bob_owner.cert.clone();
    let dm_outbox = Arc::new(Mutex::new(DmOutbox::new(
        "bob-dev".into(),
        bob_addr,
        bob_device_hash,
        Arc::clone(&bob_signing_key),
        Arc::clone(&bob),
        bob_community_sk_sync,
        bob_enrollment_sync,
    )));

    // ZEB-271: ChannelLogRegistry required by the redeem_invite_inner
    // signature. Build a minimal instance with a dummy adapter bridge — no
    // Zenoh session needed; the redeem paths under test never reach
    // steady-state channel-log fan-out.
    let (channel_log_adapter_tx, _channel_log_adapter_rx) =
        mpsc::unbounded_channel::<harmony_app::event_loop::ChannelLogAdapterRequest>();
    let channel_log_registry = ChannelLogRegistry::new(ChannelLogRegistryConfig {
        adapter_request_tx: channel_log_adapter_tx,
        sink: Arc::new(harmony_app::node_event_sink::FanoutSink(vec![])),
        identity_dir: dir.path().to_path_buf(),
        self_owner: bob_addr,
        self_device_id: "bob-dev".into(),
        signing_key: Arc::clone(&bob_signing_key),
        adopt_floor: adopt_floor.clone(),
        engine_config: ChannelLogEngineConfig::default(),
        transport_epoch_rx: None,
        // ZEB-599 Direction 1: no presence watch in this integration harness.
        presence_resync_rx: None,
    });

    // Pre-call snapshot of owner-state's canonical encoding — used to detect
    // whether the redeem committed (Test A) or rolled back (Test B).
    let pre_bytes: Vec<u8> = {
        let g = crdt_state.lock().await;
        canonicalize(&g).expect("encode pre-state")
    };

    UnreachableRedeemFixture {
        url,
        crdt_state,
        hlc_tracker,
        registry,
        adapter_tx,
        dm_outbox,
        channel_log_registry,
        adopt_floor,
        bob_owner,
        bob_signing_key,
        community_id,
        pre_bytes,
        _adapter_rx,
        _channel_log_adapter_rx,
        _dir: dir,
    }
}

/// ZEB-501 / Post-ZEB-474 / ZEB-254: an unreachable inviter no longer rolls
/// back AND no longer falsely reports `joined`. With no admin to counter-sign,
/// the redeem oneshot — which now fires only on a real JoinCountersign — times
/// out, so the redeem COMMITS the owner-state Space as a *latched-pending* join:
/// `pending == true`, `pending_join_at == Some` (greyed in nav until a
/// JoinCountersign arrives). Confirmed in ZEB-500 to NOT be a ZEB-258
/// regression — ZEB-258 governs rollback on *genuine* failure, exercised by the
/// fence test below. The positive direction (counter-sign present → pending ==
/// false → Joined) is covered by `pkarr_iroh_redeem_full_integration`.
#[tokio::test]
async fn redeem_invite_only_commits_pending_join_when_inviter_unreachable() {
    use harmony_app::owner_state_persist::canonicalize;

    let fx = build_unreachable_invite_only_redeem_fixture().await;

    // fence_check = Ok: the production snapshot-then-commit fence passes. A short
    // `redeem_timeout` (ZEB-501) drives the offline-admin timeout fast without
    // mutating the process-global env var.
    let result = harmony_app::redeem_invite_inner_with_overrides(
        fx.url,
        Arc::clone(&fx.crdt_state),
        Arc::clone(&fx.hlc_tracker),
        fx.adopt_floor.clone(),
        "bob-dev".into(),
        fx.bob_owner.owner,
        Arc::clone(&fx.bob_signing_key),
        fx.bob_owner.cert.clone(),
        Arc::clone(&fx.registry),
        fx.adapter_tx,
        None, // ZEB-434: no transport-epoch watch in this test
        Arc::clone(&fx.dm_outbox),
        fx.channel_log_registry,
        || Ok(()),
        None, // identity_dir
        harmony_app::RedeemInviteOverrides {
            redeem_timeout: Some(std::time::Duration::from_secs(2)),
            ..Default::default()
        },
    )
    .await;

    let dto = result.expect("unreachable inviter must COMMIT (latched-pending join), not Err");

    // ZEB-501: with the self-fire removed, no admin counter-sign means the
    // step-7d oneshot times out → the redeem latches the join as pending.
    assert!(
        dto.pending,
        "ZEB-501: an unreachable (un-counter-signed) redeem must report \
         pending=true (latched-pending join); got {dto:?}"
    );

    // The owner-state Space row IS committed — the durable latched join, the
    // opposite of the pre-ZEB-474 rollback.
    let post_bytes: Vec<u8> = {
        let g = fx.crdt_state.lock().await;
        canonicalize(&g).expect("encode post-state")
    };
    assert_ne!(
        fx.pre_bytes, post_bytes,
        "redeem must COMMIT the Space (durable latched-pending join), not roll back"
    );
    {
        let g = fx.crdt_state.lock().await;
        let row = g
            .spaces
            .get(&fx.community_id)
            .expect("redeem committed the owner-state Space row");
        // ZEB-501: pending_join_at IS set — the join is latched pending the
        // admin's counter-sign; it ungreys when the JoinCountersign arrives.
        assert!(
            row.pending_join_at.is_some(),
            "ZEB-501: pending_join_at must be Some for an offline-admin redeem; got {:?}",
            row.pending_join_at
        );
        assert!(
            row.left_at.is_none(),
            "a fresh join must not be marked left"
        );
    }

    fx.registry.shutdown_all().await.expect("shutdown");
}

/// ZEB-258 invariant (still load-bearing for GENUINE failures): the owner-state
/// Space commit is the LAST persistent step (step 9), gated behind the
/// snapshot-then-commit fence (step 8). A fence Err *before* the commit must
/// leave owner-state byte-identical — no orphan Space row. Distinct from the
/// inviter-unreachable timeout, which now commits (test above).
#[tokio::test]
async fn redeem_invite_only_rolls_back_owner_state_on_fence_failure() {
    use harmony_app::owner_state_persist::canonicalize;

    let fx = build_unreachable_invite_only_redeem_fixture().await;

    // fence_check = Err: the production fence rejects (e.g. the node was
    // stopped, or a stop+restart raced the await chain) — a GENUINE failure.
    // A short `redeem_timeout` (ZEB-501) reaches the fence fast (no admin
    // counter-sign arrives, so the oneshot times out first).
    let result = harmony_app::redeem_invite_inner_with_overrides(
        fx.url,
        Arc::clone(&fx.crdt_state),
        Arc::clone(&fx.hlc_tracker),
        fx.adopt_floor.clone(),
        "bob-dev".into(),
        fx.bob_owner.owner,
        Arc::clone(&fx.bob_signing_key),
        fx.bob_owner.cert.clone(),
        Arc::clone(&fx.registry),
        fx.adapter_tx,
        None,
        Arc::clone(&fx.dm_outbox),
        fx.channel_log_registry,
        || Err("simulated node-stopped fence rejection".to_string()),
        None, // identity_dir
        harmony_app::RedeemInviteOverrides {
            redeem_timeout: Some(std::time::Duration::from_secs(2)),
            ..Default::default()
        },
    )
    .await;

    assert!(
        result.is_err(),
        "a fence-check rejection must surface as Err; got {result:?}"
    );

    // ZEB-258: owner-state CRDT byte-identical pre/post the failed redeem.
    let post_bytes: Vec<u8> = {
        let g = fx.crdt_state.lock().await;
        canonicalize(&g).expect("encode post-state")
    };
    assert_eq!(
        fx.pre_bytes, post_bytes,
        "ZEB-258: owner-state CRDT must be byte-identical after a genuine \
         (fence) redeem failure — an orphan Space row would prove the \
         commit-last reorder regressed"
    );

    fx.registry.shutdown_all().await.expect("shutdown");
}

// ── ZEB-815 Task 8: address-book path replaces announce events ─────────

/// ZEB-815: the address-book path replaces the CRDT membership-delta path
/// end to end. A mints its own reachability row and publishes it through
/// the REAL `publish_own_rows` — never `insert_event` — and the sealed wire
/// bytes it produces are looped back into B's `ingest_sealed_packet`, the
/// same pipeline the production live-record subscriber runs.
///
/// Confirms:
/// - B's `ReachabilityResolver` resolves admin's actor from the ingested row;
/// - neither engine's CRDT event log ever grows a `ReachabilityAnnounce`
///   ("a") or `CommunityRelayAnnounce` ("b") event — the flag-day this
///   ticket lands removed both mint sites, so routing data flows over the
///   address-book topic + snapshot exclusively;
/// - B's book survives a `save_addrbook`/`load_addrbook` sidecar round trip;
/// - the `AddrbookIngestObserver` seam — wired by every real caller of
///   `ingest_sealed_packet`, never by the function itself — surfaces the
///   applied actor set, pinning the UI-signal path with no Tauri stack.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn addrbook_replaces_announce_events_end_to_end() {
    use harmony_app::address_book_sync::{
        derive_addrbook_key, ingest_sealed_packet, publish_own_rows, AddrbookDirtyHub,
        AddrbookIngestObserver,
    };
    use harmony_app::community_address_book::{
        addrbook_path, load_addrbook, save_addrbook, AddressBookEntry, AddressBookRow,
        CommunityAddressBook,
    };
    use harmony_app::community_relay_resolver::CommunityRelayResolver;
    use harmony_app::event_loop::PublishRequest;
    use harmony_app::reachability_record::build_signed_payload_with_key;
    use harmony_app::reachability_resolver::ReachabilityResolver;
    use std::collections::BTreeSet;

    /// Records what lands on the observer seam, standing in for the
    /// production `ReachabilityUiSignals` (needs a `NetworkHealthService` +
    /// a Tauri sink neither this test nor `ingest_sealed_packet` itself
    /// has — every real caller wires the observer at the CALL SITE, so this
    /// test does exactly what `spawn_addrbook_subscriber` does).
    #[derive(Default)]
    struct RecordingIngestObserver {
        calls: std::sync::Mutex<Vec<BTreeSet<OwnerAddr>>>,
    }
    impl RecordingIngestObserver {
        fn calls(&self) -> Vec<BTreeSet<OwnerAddr>> {
            self.calls.lock().expect("observer mutex poisoned").clone()
        }
    }
    impl AddrbookIngestObserver for RecordingIngestObserver {
        fn reachability_applied(&self, actors: &BTreeSet<OwnerAddr>) {
            self.calls
                .lock()
                .expect("observer mutex poisoned")
                .push(actors.clone());
        }
    }

    let cas_tx = spawn_shared_cas();
    let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_tx,
        Duration::from_millis(2000),
    ));

    let community_id = SpaceId([3u8; 16]);
    let mk = EpochKey::new([0x77; 32]);

    let id_admin = mint_test_owner(0xa7);
    let admin = id_admin.owner;
    let admin_pub = [0u8; 64];

    let id_b = mint_test_owner(0xb8);
    let b_owner = id_b.owner;
    let b_signing = signing_key_from(&id_b);

    let mut resolver_map = std::collections::HashMap::new();
    resolver_map.insert(admin, admin_pub);
    let resolver: Arc<dyn IdentityResolver> = Arc::new(StaticResolver { map: resolver_map });

    let dir_a = tempfile::tempdir().expect("tempdir A");
    let dir_b = tempfile::tempdir().expect("tempdir B");

    let registry_a = CommunitySyncRegistry::new(CommunityRegistryConfig {
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
        device_id: "a-dev".into(),
        content_store: Arc::clone(&cs),
        identity_resolver: Arc::clone(&resolver),
        identity_dir: dir_a.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: None,
        self_owner: admin,
        signing_key: signing_key_from(&id_admin),
        crdt_state: None,
        nav_emitter: None,
        presence_resync_rx: None,
    });
    let registry_b = Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
        device_id: "b-dev".into(),
        content_store: Arc::clone(&cs),
        identity_resolver: Arc::clone(&resolver),
        identity_dir: dir_b.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: None,
        self_owner: b_owner,
        signing_key: Arc::clone(&b_signing),
        crdt_state: None,
        nav_emitter: None,
        presence_resync_rx: None,
    }));

    // Root-CRDT pub/sub channels: unused here. This test exercises the
    // address-book path exclusively (`publish_own_rows` / `ingest_sealed_packet`),
    // which is wired independently of the root-CRDT publish/subscribe pair —
    // `spawn_engine_inner_now` just needs valid handles to satisfy its
    // signature.
    let (a_pub_tx, _a_pub_rx) = mpsc::channel(8);
    let (_a_sub_tx, a_sub_rx) = mpsc::channel(8);
    let (b_pub_tx, _b_pub_rx) = mpsc::channel(8);
    let (_b_sub_tx, b_sub_rx) = mpsc::channel(8);

    registry_a
        .spawn_engine_inner_now(
            community_id,
            mk.clone(),
            admin,
            false,
            a_pub_tx,
            a_sub_rx,
            harmony_app::community_state_sync::CatchUpChannels::none(),
        )
        .await
        .expect("spawn a");
    registry_b
        .spawn_engine_inner_now(
            community_id,
            mk.clone(),
            admin,
            false,
            b_pub_tx,
            b_sub_rx,
            harmony_app::community_state_sync::CatchUpChannels::none(),
        )
        .await
        .expect("spawn b");

    // Pre-seed BOTH sides with admin's Join — mirrors
    // `two_members_dag_sync_full_event_log`: B's `beacon_signer_is_member`
    // gate (inside `ingest_sealed_packet`) needs admin materialized as an
    // enrolled Joined member before it will accept admin's address-book row.
    let admin_join_event = {
        let payload = EventPayload {
            id: [7u8; 16],
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
    let state_a = registry_a
        .state_for(&community_id)
        .await
        .expect("engine spawned");
    {
        let mut sa = state_a.lock().await;
        let outcome = sa.insert_event(
            admin_join_event.clone(),
            &harmony_app::community_membership::VerifyContext {
                expected_community_id: community_id,
                admin_addr: admin,
                is_invite_only: false,
            },
        );
        assert!(matches!(
            outcome,
            harmony_app::community_state_crdt::InsertOutcome::Inserted
        ));
    }
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
            },
        );
        assert!(matches!(
            outcome,
            harmony_app::community_state_crdt::InsertOutcome::Inserted
        ));
    }

    // ── Address-book side: A mints its own reachability row and publishes
    //    it through the REAL `publish_own_rows` path (never `insert_event`).
    let ts = 1_700_000_500_000u64;
    let row_hlc = Hlc {
        wall_ms: ts,
        logical: 0,
        device_id: "a-dev".into(),
    };
    let payload = build_signed_payload_with_key(
        [0xEE; 32],
        "https://derp.example/".into(),
        vec![],
        ts,
        &admin,
        &row_hlc,
        Vec::new(),
        0,
        &id_admin.device_key,
    )
    .expect("build signed reachability payload");
    let admin_device = id_admin.device_key.verifying_key().to_bytes();
    let row = AddressBookRow {
        entry: AddressBookEntry::Reachability(payload),
        actor: admin,
        device: admin_device,
        at: row_hlc,
        stamped_at_ms: ts,
    };

    let book_a = CommunityAddressBook::new();
    let rr_a = ReachabilityResolver::new();
    let crr_a = CommunityRelayResolver::new();
    let dirty_hub_a = AddrbookDirtyHub::new();

    let mk_for_key_fn = mk.clone();
    let key_fn = move |c: &SpaceId| {
        if *c == community_id {
            Some(derive_addrbook_key(&mk_for_key_fn, c))
        } else {
            None
        }
    };

    let (publish_tx, mut publish_rx) = mpsc::channel::<PublishRequest>(8);

    // Drain the real `PublishRequest`s A's `publish_own_rows` produces,
    // acking each so the call doesn't block on an unread reply oneshot.
    // Stashed for the loopback below rather than fed into B's ingest
    // inline, so `book_b`/`rr_b`/`crr_b` never need to cross a spawned
    // task's boundary.
    let collected: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let collected_for_drain = Arc::clone(&collected);
    let drain_handle = tokio::spawn(async move {
        while let Some(req) = publish_rx.recv().await {
            collected_for_drain.lock().await.push(req.payload);
            let _ = req.reply.send(Ok(()));
        }
    });

    let applied = publish_own_rows(
        &key_fn,
        &book_a,
        &rr_a,
        &crr_a,
        &publish_tx,
        vec![(community_id, row)],
        &dirty_hub_a,
    )
    .await;
    assert_eq!(
        applied,
        BTreeSet::from([admin]),
        "A's own reachability row must apply locally and report admin as the affected actor"
    );

    drop(publish_tx);
    drain_handle.await.expect("drain task join");
    let payloads = collected.lock().await.clone();
    assert_eq!(
        payloads.len(),
        1,
        "publish_own_rows must have sealed exactly one record onto the wire"
    );

    // ── B's side: feed A's sealed payload through the SAME
    //    `ingest_sealed_packet` pipeline the live-record subscriber runs.
    let book_b = CommunityAddressBook::new();
    let rr_b = ReachabilityResolver::new();
    let crr_b = CommunityRelayResolver::new();
    let observer = Arc::new(RecordingIngestObserver::default());

    for payload in &payloads {
        let batch = ingest_sealed_packet(
            &registry_b,
            &book_b,
            &rr_b,
            &crr_b,
            community_id,
            payload,
            ts,
        )
        .await;
        assert!(
            batch.changed_book(),
            "B's book must materially change from A's looped-back row"
        );
        // Mirrors what every real caller of `ingest_sealed_packet` does —
        // the function itself never touches the observer.
        if !batch.reachability_actors.is_empty() {
            observer.reachability_applied(&batch.reachability_actors);
        }
    }

    // (a) B's resolver resolves admin's actor from the ingested row.
    assert!(
        !rr_b.resolve(&admin).is_empty(),
        "B's ReachabilityResolver must resolve admin's actor after ingest"
    );

    // (b) Neither engine's CRDT event log ever grew a ReachabilityAnnounce
    //     ("a") or CommunityRelayAnnounce ("b") event — the flag-day this
    //     ticket lands removed both mint sites; routing data flows over the
    //     address-book path exclusively.
    for state in [&state_a, &state_b] {
        let guard = state.lock().await;
        for ev in guard.events() {
            assert!(
                !matches!(
                    ev.kind,
                    MembershipEventKind::ReachabilityAnnounce { .. }
                        | MembershipEventKind::CommunityRelayAnnounce { .. }
                ),
                "flag-day: address-book rows must never mint a CRDT event; found {:?}",
                ev.kind
            );
        }
    }

    // (c) B's book survives a sidecar round trip.
    let sidecar_dir = tempfile::tempdir().expect("tempdir sidecar");
    let sidecar_path = addrbook_path(sidecar_dir.path(), &community_id);
    let rows_before = book_b.rows_for_community(&community_id, ts);
    assert_eq!(rows_before.len(), 1, "B's book holds exactly admin's row");
    save_addrbook(&sidecar_path, &rows_before).expect("save addrbook");
    let rows_after = load_addrbook(&sidecar_path, ts);
    assert_eq!(
        rows_after, rows_before,
        "B's book rows must survive a save_addrbook/load_addrbook round trip"
    );

    // (d) The UI-signal seam surfaces the applied actor set end to end.
    assert_eq!(
        observer.calls(),
        vec![BTreeSet::from([admin])],
        "the AddrbookIngestObserver seam must surface admin as the applied actor"
    );

    registry_a.shutdown_all().await.expect("shutdown a");
    registry_b.shutdown_all().await.expect("shutdown b");
}

/// ZEB-815 Task 8 fix round (review Finding 2): confound-free coverage of the
/// SNAPSHOT query/reply DATA PATH. The test above only exercises the live
/// single-record codec (`publish_own_rows` → `seal_records` over one row);
/// Task 5's units cover the requester's cooldown/jitter/locality, not the
/// data flow; this closes that gap.
///
/// B's book holds two of admin's rows (one Reachability, one Relay — the two
/// entry kinds a snapshot must carry alike) seeded through the real per-row
/// gate (`ingest_verified_row`), so only the join precondition already proven
/// above is needed (one already-enrolled actor+device, no second identity).
/// The reply packet is built through the EXACT same call the production
/// serve side (`spawn_addrbook_snapshot_queryable`) makes —
/// `book.rows_for_community` then `seal_snapshot_bounded` — then fed into A's
/// `ingest_sealed_packet`, mirroring the requester side
/// (`request_snapshot_once`) per reply. Confirms:
/// - A's `ReachabilityResolver` resolves admin's reachability row;
/// - A's `CommunityRelayResolver` resolves admin's relay row from the SAME
///   snapshot;
/// - A's book ends up holding both rows, byte-identical to what B served;
/// - the observer surfaces exactly ONE call carrying `{admin}` — the relay
///   row must not double-count or contribute a signal of its own (mirrors
///   the `signals_reachability` invariant `address_book_sync.rs`'s own unit
///   tests pin on a hand-built `IngestBatch`; here it runs through the full
///   sealed-packet snapshot path instead).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn addrbook_snapshot_path_ingest_end_to_end() {
    use harmony_app::address_book_sync::{
        derive_addrbook_key, ingest_sealed_packet, ingest_verified_row, seal_snapshot_bounded,
        AddrbookIngestObserver, IngestOutcome, ADDRBOOK_SNAPSHOT_MAX_BYTES,
    };
    use harmony_app::community_address_book::{
        AddressBookEntry, AddressBookRow, CommunityAddressBook, UpsertOutcome,
    };
    use harmony_app::community_relay_announce::{
        build_signed_community_relay_announce, CommunityRelayEntry,
    };
    use harmony_app::community_relay_resolver::CommunityRelayResolver;
    use harmony_app::reachability_record::build_signed_payload_with_key;
    use harmony_app::reachability_resolver::ReachabilityResolver;
    use std::collections::BTreeSet;

    /// Same recorder shape as the test above — kept local (rather than
    /// shared) so each test stays self-contained per this file's convention.
    #[derive(Default)]
    struct RecordingIngestObserver {
        calls: std::sync::Mutex<Vec<BTreeSet<OwnerAddr>>>,
    }
    impl RecordingIngestObserver {
        fn calls(&self) -> Vec<BTreeSet<OwnerAddr>> {
            self.calls.lock().expect("observer mutex poisoned").clone()
        }
    }
    impl AddrbookIngestObserver for RecordingIngestObserver {
        fn reachability_applied(&self, actors: &BTreeSet<OwnerAddr>) {
            self.calls
                .lock()
                .expect("observer mutex poisoned")
                .push(actors.clone());
        }
    }

    let cas_tx = spawn_shared_cas();
    let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_tx,
        Duration::from_millis(2000),
    ));

    let community_id = SpaceId([4u8; 16]);
    let mk = EpochKey::new([0x88; 32]);

    let id_admin = mint_test_owner(0xc9);
    let admin = id_admin.owner;
    let admin_pub = [0u8; 64];

    let id_b = mint_test_owner(0xd0);
    let b_owner = id_b.owner;
    let b_signing = signing_key_from(&id_b);

    let mut resolver_map = std::collections::HashMap::new();
    resolver_map.insert(admin, admin_pub);
    let resolver: Arc<dyn IdentityResolver> = Arc::new(StaticResolver { map: resolver_map });

    let dir_a = tempfile::tempdir().expect("tempdir A");
    let dir_b = tempfile::tempdir().expect("tempdir B");

    // A is the ingesting side here (mirroring the snapshot REQUESTER), so it
    // needs `Arc` for `ingest_sealed_packet`.
    let registry_a = Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
        device_id: "a-dev".into(),
        content_store: Arc::clone(&cs),
        identity_resolver: Arc::clone(&resolver),
        identity_dir: dir_a.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: None,
        self_owner: admin,
        signing_key: signing_key_from(&id_admin),
        crdt_state: None,
        nav_emitter: None,
        presence_resync_rx: None,
    }));
    let registry_b = CommunitySyncRegistry::new(CommunityRegistryConfig {
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
        device_id: "b-dev".into(),
        content_store: Arc::clone(&cs),
        identity_resolver: Arc::clone(&resolver),
        identity_dir: dir_b.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: None,
        self_owner: b_owner,
        signing_key: Arc::clone(&b_signing),
        crdt_state: None,
        nav_emitter: None,
        presence_resync_rx: None,
    });

    // Root-CRDT pub/sub channels: unused, exactly as in the test above.
    let (a_pub_tx, _a_pub_rx) = mpsc::channel(8);
    let (_a_sub_tx, a_sub_rx) = mpsc::channel(8);
    let (b_pub_tx, _b_pub_rx) = mpsc::channel(8);
    let (_b_sub_tx, b_sub_rx) = mpsc::channel(8);

    registry_a
        .spawn_engine_inner_now(
            community_id,
            mk.clone(),
            admin,
            false,
            a_pub_tx,
            a_sub_rx,
            harmony_app::community_state_sync::CatchUpChannels::none(),
        )
        .await
        .expect("spawn a");
    registry_b
        .spawn_engine_inner_now(
            community_id,
            mk.clone(),
            admin,
            false,
            b_pub_tx,
            b_sub_rx,
            harmony_app::community_state_sync::CatchUpChannels::none(),
        )
        .await
        .expect("spawn b");

    // Pre-seed BOTH sides with admin's Join — A's `ingest_sealed_packet`
    // membership gate needs admin materialized as an enrolled Joined member
    // before it will accept admin's rows from the snapshot.
    let admin_join_event = {
        let payload = EventPayload {
            id: [8u8; 16],
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
    let state_a = registry_a
        .state_for(&community_id)
        .await
        .expect("engine spawned");
    {
        let mut sa = state_a.lock().await;
        let outcome = sa.insert_event(
            admin_join_event.clone(),
            &harmony_app::community_membership::VerifyContext {
                expected_community_id: community_id,
                admin_addr: admin,
                is_invite_only: false,
            },
        );
        assert!(matches!(
            outcome,
            harmony_app::community_state_crdt::InsertOutcome::Inserted
        ));
    }
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
            },
        );
        assert!(matches!(
            outcome,
            harmony_app::community_state_crdt::InsertOutcome::Inserted
        ));
    }

    // ── B already holds two of admin's rows (however they got there — this
    //    test is about the SNAPSHOT round trip, not row provenance, so seed
    //    directly through the real per-row gate rather than re-running the
    //    publish path above). One Reachability, one Relay.
    let book_b = CommunityAddressBook::new();
    let rr_b = ReachabilityResolver::new();
    let crr_b = CommunityRelayResolver::new();

    let ts = 1_700_000_600_000u64;
    let row_hlc = Hlc {
        wall_ms: ts,
        logical: 0,
        device_id: "a-dev".into(),
    };
    let admin_device = id_admin.device_key.verifying_key().to_bytes();

    let reach_payload = build_signed_payload_with_key(
        [0xFA; 32],
        "https://derp.example/".into(),
        vec![],
        ts,
        &admin,
        &row_hlc,
        Vec::new(),
        0,
        &id_admin.device_key,
    )
    .expect("build signed reachability payload");
    let reach_row = AddressBookRow {
        entry: AddressBookEntry::Reachability(reach_payload),
        actor: admin,
        device: admin_device,
        at: row_hlc.clone(),
        stamped_at_ms: ts,
    };
    assert_eq!(
        ingest_verified_row(&book_b, &rr_b, &crr_b, community_id, reach_row.clone(), ts),
        IngestOutcome::Applied(UpsertOutcome::Inserted),
        "B's own reachability row must seed through the real ingest gate"
    );

    let relay_entry = CommunityRelayEntry {
        relay_device_id: [0xD9; 16],
        iroh_endpoint_id: [0x66; 32],
        relay_device_ed25519_verify: admin_device,
        home_relay: "https://r.example/".into(),
    };
    let relay_payload = build_signed_community_relay_announce(
        relay_entry,
        ts,
        &admin,
        &row_hlc,
        &id_admin.device_key,
    )
    .expect("build signed relay payload");
    let relay_row = AddressBookRow {
        entry: AddressBookEntry::Relay(relay_payload),
        actor: admin,
        device: admin_device,
        at: row_hlc,
        stamped_at_ms: ts,
    };
    assert_eq!(
        ingest_verified_row(&book_b, &rr_b, &crr_b, community_id, relay_row.clone(), ts),
        IngestOutcome::Applied(UpsertOutcome::Inserted),
        "B's own relay row must seed through the real ingest gate"
    );

    // ── Build the snapshot reply through the EXACT same call the production
    //    queryable makes: fetch this community's fresh rows, then
    //    `seal_snapshot_bounded` under the shared address-book key.
    let key = derive_addrbook_key(&mk, &community_id);
    let rows_to_serve = book_b.rows_for_community(&community_id, ts);
    assert_eq!(rows_to_serve.len(), 2, "B's book holds both seeded rows");
    let packet = seal_snapshot_bounded(
        &key,
        &community_id,
        rows_to_serve,
        ADDRBOOK_SNAPSHOT_MAX_BYTES,
    )
    .expect("seal snapshot reply");

    // ── A ingests the snapshot reply through the SAME `ingest_sealed_packet`
    //    the production requester calls per reply, then feeds the applied
    //    actors to the observer exactly as every real caller does (the
    //    function itself never touches it).
    let book_a = CommunityAddressBook::new();
    let rr_a = ReachabilityResolver::new();
    let crr_a = CommunityRelayResolver::new();
    let observer = Arc::new(RecordingIngestObserver::default());

    let batch = ingest_sealed_packet(
        &registry_a,
        &book_a,
        &rr_a,
        &crr_a,
        community_id,
        &packet,
        ts,
    )
    .await;
    assert!(
        batch.changed_book(),
        "A's book must materially change from B's snapshot reply"
    );
    assert_eq!(
        batch.outcomes.len(),
        2,
        "the snapshot packet must carry both of B's rows"
    );
    if !batch.reachability_actors.is_empty() {
        observer.reachability_applied(&batch.reachability_actors);
    }

    // A's resolver resolves admin's reachability row from the snapshot.
    assert!(
        !rr_a.resolve(&admin).is_empty(),
        "A's ReachabilityResolver must resolve admin's actor after snapshot ingest"
    );
    // A's relay resolver resolves admin's relay row from the SAME snapshot.
    assert!(
        !crr_a.relays_for_community(&community_id, ts).is_empty(),
        "A's CommunityRelayResolver must resolve admin's relay from the snapshot"
    );

    // A's book ends up holding both rows, byte-identical to what B served.
    let rows_at_a = book_a.rows_for_community(&community_id, ts);
    assert_eq!(
        rows_at_a.len(),
        2,
        "A's book holds both rows from the snapshot"
    );
    assert!(
        rows_at_a.contains(&reach_row),
        "A's book holds B's reachability row verbatim"
    );
    assert!(
        rows_at_a.contains(&relay_row),
        "A's book holds B's relay row verbatim"
    );

    // The observer surfaces exactly one call carrying {admin} — the relay
    // row contributes no signal of its own.
    assert_eq!(
        observer.calls(),
        vec![BTreeSet::from([admin])],
        "the observer seam surfaces admin exactly once from the snapshot round trip"
    );

    registry_a.shutdown_all().await.expect("shutdown a");
    registry_b.shutdown_all().await.expect("shutdown b");
}
