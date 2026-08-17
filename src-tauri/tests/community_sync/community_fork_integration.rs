//! ZEB-285 Phase 1: end-to-end fork integration tests.
//!
//! Six tokio tests covering the fork lifecycle using the paired-engine harness
//! pattern (no real Zenoh — mpsc-channel transport backed by CommunitySyncRegistry).
//!
//! The tests exercise the lower-level building blocks directly instead of going
//! through the `fork_community` Tauri IPC (which requires full NodeState). A
//! `run_fork_inner` helper replicates the IPC's core steps using the pure
//! `mint_community_creation`, `mint_fork_event`, `mint_leave_event`, and
//! `sign_event` functions.
//!
//! Spec: docs/specs/2026-05-14-zeb-285-phase1-community-forking-design.md §7.5

use harmony_app::community_invite::{BoundedChannelLogSnapshot, PreForkSnapshot};
use harmony_app::community_membership::{
    mint_test_owner, sign_event, sign_event_with_identity, EventPayload, MembershipEventKind,
    SignedMembershipEvent, TestOwner,
};
use harmony_app::community_state_sync::{
    CommunityRegistryConfig, CommunitySyncRegistry, IdentityResolver, DEFAULT_DEBOUNCE_MS,
};
use harmony_app::content_store::{CasOp, ContentStore, RuntimeContentStore};
use harmony_app::owner_state_crypto::canonical_cbor_encode;
use harmony_app::owner_state_types::{EpochKey, Hlc, OwnerAddr, SpaceId};
use harmony_identity::PrivateIdentity;
use rand::RngCore;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};

static EVENT_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

// ── Harness helpers ─────────────────────────────────────────────────────────

/// In-memory identity resolver backed by a HashMap.
struct StaticResolver {
    map: std::collections::HashMap<OwnerAddr, [u8; 64]>,
}

#[async_trait::async_trait]
impl IdentityResolver for StaticResolver {
    async fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
        self.map.get(addr).copied()
    }
}

/// Spawn a shared in-memory CAS servicer. Both registries route through this
/// so blobs A puts are visible to B's gets.
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

/// Make a simple HLC for tests with the given wall_ms.
fn test_hlc(wall_ms: u64, seq: u32, device: &str) -> Hlc {
    Hlc {
        wall_ms,
        logical: seq,
        device_id: device.to_string(),
    }
}

/// Create an admin Join event for `owner` into `community_id`, signed with the
/// owner's enrolled device key and carrying their Master enrollment cert
/// (ZEB-339) so receiving engines learn `owner`'s enrolled device key.
fn make_join_event(
    community_id: SpaceId,
    owner: &TestOwner,
    wall_ms: u64,
    seq: u32,
    device: &str,
) -> SignedMembershipEvent {
    let id_seq = EVENT_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut id_bytes = [0u8; 16];
    id_bytes[..8].copy_from_slice(&id_seq.to_be_bytes());
    // First 8 bytes count up deterministically; last 8 stay zero
    // (sufficient for test determinism without colliding within a test run).
    let payload = EventPayload {
        id: id_bytes,
        community_id,
        kind: MembershipEventKind::Join,
        actor: owner.owner,
        at: test_hlc(wall_ms, seq, device),
    };
    let ev = sign_event(&payload, &owner.device_key).expect("sign join");
    SignedMembershipEvent {
        enrollment: Some(owner.cert.clone()),
        ..ev
    }
}

/// Result of `run_fork_inner`.
struct ForkResult {
    /// SpaceId of the newly created fork community.
    fork_space_id: SpaceId,
    /// Membership key minted for the fork.
    fork_mk: EpochKey,
    /// true iff the Fork event was inserted into the original's engine.
    visible: bool,
}

/// Replicate the core logic of `fork_community` IPC without Tauri state.
///
/// Steps:
/// 1. Mint a fresh fork community via `mint_community_creation`.
/// 2. Spawn a fork engine in `registry_fork` (or `registry_original` if same).
/// 3. Insert the bootstrap Join into the fork engine.
/// 4. Set `forked_from` on the fork engine's CommunityState.
/// 5. Build a minimal PreForkSnapshot and write it to `data_dir`.
/// 6. If `!silent`: mint a Fork event and insert into original engine.
/// 7. If `also_leave`: mint a Leave event and insert into original engine.
///
/// `registry_original` must already have the original community engine spawned
/// with `forker` as admin. `registry_fork` is where the fork engine is spawned
/// (can be the same registry for tests that only need one identity).
#[allow(clippy::too_many_arguments)]
async fn run_fork_inner(
    forker: &TestOwner,
    forker_addr: OwnerAddr,
    forker_pub: [u8; 64],
    original_community_id: SpaceId,
    _original_mk: &EpochKey,
    registry_original: &CommunitySyncRegistry,
    registry_fork: &CommunitySyncRegistry,
    fork_dir: &std::path::Path,
    fork_name: &str,
    silent: bool,
    also_leave: bool,
) -> ForkResult {
    let signing_key = Arc::new(forker.device_key.clone());

    // Step 1: mint fork community identity.
    let hlc = test_hlc(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64,
        0,
        "fork-dev",
    );
    let minted = harmony_app::mint_community_creation(
        fork_name,
        false,
        forker_addr,
        signing_key.as_ref(),
        // ZEB-339: the forker's real Master enrollment cert, attached to the
        // fork's bootstrap Join so the fork engine learns the forker's device key.
        &forker.cert,
        hlc.clone(),
    )
    .expect("mint_community_creation");

    let fork_space_id = minted.community_id;
    let fork_mk = minted.membership_key.clone();

    // Step 2: spawn the fork engine in registry_fork.
    let (fork_pub_tx, _fork_pub_rx) = mpsc::channel::<Vec<u8>>(8);
    let (_fork_sub_tx, fork_sub_rx) = mpsc::channel::<Vec<u8>>(8);
    registry_fork
        .spawn_engine_inner_now(
            fork_space_id,
            fork_mk.clone(),
            forker_addr,
            false,
            fork_pub_tx,
            fork_sub_rx,
            harmony_app::community_state_sync::CatchUpChannels::none(),
        )
        .await
        .expect("spawn fork engine");

    // Step 3: get the fork engine and insert the bootstrap Join.
    let fork_engine = registry_fork
        .engine_arc(&fork_space_id)
        .await
        .expect("fork engine arc");
    let join_outcome = fork_engine
        .insert_local_event_with_pubs(minted.bootstrap_join.clone(), forker_pub, None)
        .await
        .expect("insert bootstrap join");
    assert!(
        matches!(
            join_outcome,
            harmony_app::community_state_crdt::InsertOutcome::Inserted
        ),
        "bootstrap Join must be Inserted, got: {:?}",
        join_outcome
    );

    // Step 4: set forked_from + Phase 2 lineage fields on the fork engine's
    // CommunityState — mirrors community_fork.rs Steps after engine spawn.
    // R1-1b: also persist forked_at_wall_ms + parent_lineage so a
    // subsequent fork of THIS community reads the correct ancestry.
    let new_parent_lineage = {
        // Read forker community's existing lineage to extend.
        let original_engine = registry_original
            .engine_arc(&original_community_id)
            .await
            .expect("original engine arc");
        let state_arc = original_engine.state();
        let state_g = state_arc.lock().await;
        harmony_app::community_invite::build_parent_lineage(
            &state_g.parent_lineage,
            original_community_id,
            "test-community",
            state_g.forked_at_wall_ms,
            state_g.fork_reason.clone(),
        )
    };
    {
        let state_arc = fork_engine.state();
        let mut state_g = state_arc.lock().await;
        state_g.forked_from = Some(original_community_id);
        state_g.forked_at_wall_ms = Some(hlc.wall_ms);
        state_g.parent_lineage = new_parent_lineage.clone();
    }

    // Step 5: build a minimal PreForkSnapshot and write to disk.
    // Collect membership events from the original engine.
    let original_events: Vec<harmony_app::community_membership::SignedMembershipEvent> = {
        let original_engine = registry_original
            .engine_arc(&original_community_id)
            .await
            .expect("original engine arc");
        let state_arc = original_engine.state();
        let state_g = state_arc.lock().await;
        state_g.events().cloned().collect()
    };

    let mut identity_pubs: BTreeMap<OwnerAddr, [u8; 64]> = BTreeMap::new();
    identity_pubs.insert(forker_addr, forker_pub);

    let snapshot = PreForkSnapshot {
        original_community_id,
        original_community_name: "test-community".to_string(),
        membership_events: original_events,
        channel_log: BoundedChannelLogSnapshot::default(),
        identity_pubs,
        forked_at: hlc.clone(),
        // R1-1b: snapshot carries the new fork's lineage so that a
        // subsequent fork of THIS fork reads correct ancestry through
        // the invite path too.
        parent_lineage: new_parent_lineage,
        fork_reason: None,
    };

    // Write snapshot to disk atomically.
    let fork_community_dir = fork_dir
        .join("communities")
        .join(hex::encode(fork_space_id.0));
    std::fs::create_dir_all(&fork_community_dir).expect("create fork community dir");
    let snapshot_bytes = canonical_cbor_encode(&snapshot).expect("encode snapshot");
    let tmp_path = fork_community_dir.join("pre_fork_snapshot.bin.tmp");
    let snapshot_path = fork_community_dir.join("pre_fork_snapshot.bin");
    std::fs::write(&tmp_path, &snapshot_bytes).expect("write snapshot tmp");
    std::fs::rename(&tmp_path, &snapshot_path).expect("rename snapshot");

    // Step 6: if !silent, mint a Fork event and insert into original engine.
    let mut visible = !silent;
    if visible {
        let fork_event = harmony_app::community_fork::mint_fork_event(
            original_community_id,
            forker_addr,
            fork_space_id,
            "integration-test fork reason".to_string(),
            signing_key.as_ref(),
            test_hlc(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
                1,
                "fork-dev",
            ),
        )
        .expect("mint fork event");

        let original_engine = registry_original
            .engine_arc(&original_community_id)
            .await
            .expect("original engine arc");
        match original_engine
            .insert_local_event_with_pubs(fork_event, forker_pub, None)
            .await
        {
            Ok(outcome) => {
                if !matches!(
                    outcome,
                    harmony_app::community_state_crdt::InsertOutcome::Inserted
                        | harmony_app::community_state_crdt::InsertOutcome::AlreadyKnown
                ) {
                    visible = false;
                }
            }
            Err(e) => {
                eprintln!(
                    "run_fork_inner: insert_local_event_with_pubs failed for Fork event: {:?}",
                    e
                );
                visible = false;
            }
        }
    }

    // Step 7: if also_leave, mint a Leave event and insert into original engine.
    if also_leave {
        let leave_event = harmony_app::mint_leave_event(
            original_community_id,
            forker_addr,
            signing_key.as_ref(),
            test_hlc(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
                2,
                "fork-dev",
            ),
        )
        .expect("mint leave event");

        let original_engine = registry_original
            .engine_arc(&original_community_id)
            .await
            .expect("original engine arc");
        if let Err(e) = original_engine
            .insert_local_event_with_pubs(leave_event, forker_pub, None)
            .await
        {
            eprintln!(
                "run_fork_inner: insert_local_event_with_pubs failed for Leave event: {:?}",
                e
            );
        }
    }

    ForkResult {
        fork_space_id,
        fork_mk,
        visible,
    }
}

/// Bootstrap two CommunitySyncRegistry instances (A = admin/forker, B = observer)
/// sharing the same community and CAS. Returns the registry pair, shared community
/// SpaceId + MK, and the pub-tx channels needed to wire A → B transport.
struct PairedEngines {
    registry_a: Arc<CommunitySyncRegistry>,
    registry_b: Arc<CommunitySyncRegistry>,
    community_id: SpaceId,
    mk: EpochKey,
    id_a: TestOwner,
    a_addr: OwnerAddr,
    a_pub: [u8; 64],
    dir_a: tempfile::TempDir,
    /// Kept alive to prevent tempdir cleanup while registry_b runs.
    _dir_b: tempfile::TempDir,
}

impl PairedEngines {
    async fn bootstrap() -> Self {
        let cas_tx = spawn_shared_cas();
        let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
            cas_tx,
            Duration::from_millis(2000),
        ));

        let community_id = SpaceId({
            let mut b = [0u8; 16];
            rand::thread_rng().fill_bytes(&mut b);
            b
        });

        let id_a = mint_test_owner(0xA1);
        let a_addr = id_a.owner;
        // ZEB-339: signer resolution uses the cert / materialized enrolled keys,
        // not the resolver — so resolver pubs are unused for membership verify.
        let a_pub = [0u8; 64];
        let a_signing = Arc::new(id_a.device_key.clone());

        let id_b = mint_test_owner(0xB1);
        let b_addr = id_b.owner;
        let b_signing = Arc::new(id_b.device_key.clone());

        let b_pub = [0u8; 64];

        let mut resolver_map = std::collections::HashMap::new();
        resolver_map.insert(a_addr, a_pub);
        resolver_map.insert(b_addr, b_pub);
        let resolver: Arc<dyn IdentityResolver> = Arc::new(StaticResolver { map: resolver_map });

        let mut mk_bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut mk_bytes);
        let mk = EpochKey::new(mk_bytes);

        let dir_a = tempfile::tempdir().expect("tempdir A");
        let dir_b = tempfile::tempdir().expect("tempdir B");

        let registry_a = Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
            adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
            device_id: "a-dev".into(),
            content_store: Arc::clone(&cs),
            identity_resolver: Arc::clone(&resolver),
            identity_dir: dir_a.path().to_path_buf(),
            debounce_ms: DEFAULT_DEBOUNCE_MS,
            error_tx: None,
            delta_tx: None,
            self_owner: a_addr,
            signing_key: Arc::clone(&a_signing),
            crdt_state: None,
            nav_emitter: None,
            presence_resync_rx: None,
        }));

        let registry_b = Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
            adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
            device_id: "b-dev".into(),
            content_store: Arc::clone(&cs),
            identity_resolver: Arc::clone(&resolver),
            identity_dir: dir_b.path().to_path_buf(),
            debounce_ms: DEFAULT_DEBOUNCE_MS,
            error_tx: None,
            delta_tx: None,
            self_owner: b_addr,
            signing_key: Arc::clone(&b_signing),
            crdt_state: None,
            nav_emitter: None,
            presence_resync_rx: None,
        }));

        // Spawn the original community engine on both sides.
        // A and B are wired with dummy pub/sub channels — event delivery between
        // engines happens via direct insert_local_event_with_pubs in each test.
        let (a_pub_tx, _) = mpsc::channel::<Vec<u8>>(8);
        let (_, a_sub_rx) = mpsc::channel::<Vec<u8>>(8);
        registry_a
            .spawn_engine_inner_now(
                community_id,
                mk.clone(),
                a_addr,
                false,
                a_pub_tx,
                a_sub_rx,
                harmony_app::community_state_sync::CatchUpChannels::none(),
            )
            .await
            .expect("spawn original engine on A");

        let (b_pub_tx, _) = mpsc::channel::<Vec<u8>>(8);
        let (_, b_sub_rx) = mpsc::channel::<Vec<u8>>(8);
        registry_b
            .spawn_engine_inner_now(
                community_id,
                mk.clone(),
                a_addr,
                false,
                b_pub_tx,
                b_sub_rx,
                harmony_app::community_state_sync::CatchUpChannels::none(),
            )
            .await
            .expect("spawn original engine on B");

        // Bootstrap: insert A's Join into both engines so both have valid
        // membership before fork events arrive.
        let join_event = make_join_event(community_id, &id_a, 100, 0, "a-dev");

        // Insert into A's engine.
        let engine_a = registry_a
            .engine_arc(&community_id)
            .await
            .expect("engine A arc");
        engine_a
            .insert_local_event_with_pubs(join_event.clone(), a_pub, None)
            .await
            .expect("A join insert");

        // Insert into B's engine so B sees A as joined (for verify_event at receive).
        let engine_b = registry_b
            .engine_arc(&community_id)
            .await
            .expect("engine B arc");
        engine_b
            .insert_local_event_with_pubs(join_event, a_pub, None)
            .await
            .expect("B join insert");

        PairedEngines {
            registry_a,
            registry_b,
            community_id,
            mk,
            id_a,
            a_addr,
            a_pub,
            dir_a,
            _dir_b: dir_b,
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────

/// Test 1: visible fork announces a Fork event in the original community's log.
/// Both engine A (forker) and engine B (observer) end up holding the Fork event.
///
/// Because fork events are inserted via `insert_local_event_with_pubs` directly
/// into both engines (mirroring how production would sync them), this verifies
/// that the Fork variant is accepted by verify_event and materializes in the CRDT.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn visible_fork_announces_in_original_log() {
    let engines = PairedEngines::bootstrap().await;
    let community_id = engines.community_id;
    let a_addr = engines.a_addr;
    let a_pub = engines.a_pub;

    // Run a visible fork (silent=false).
    let result = run_fork_inner(
        &engines.id_a,
        a_addr,
        a_pub,
        community_id,
        &engines.mk,
        &engines.registry_a,
        &engines.registry_a, // fork engine lives in same registry for simplicity
        engines.dir_a.path(),
        "Forked Community",
        false, // visible
        false,
    )
    .await;

    assert!(result.visible, "non-silent fork should be visible");
    let fork_space_id = result.fork_space_id;

    // Engine A should have the Fork event in the original community's log.
    let engine_a = engines.registry_a.engine_arc(&community_id).await.unwrap();
    let state_a = engine_a.state().lock().await.clone();
    let has_fork = state_a.events().any(|e| {
        matches!(&e.kind, MembershipEventKind::Fork { fork_space_id: fid, .. } if *fid == fork_space_id)
    });
    assert!(
        has_fork,
        "engine A must have Fork event in original community log"
    );

    // Now propagate the Fork event to engine B by inserting it directly
    // (mirrors what the sync transport would do).
    let fork_event = state_a
        .events()
        .find(|e| matches!(&e.kind, MembershipEventKind::Fork { .. }))
        .cloned()
        .expect("Fork event");

    let engine_b = engines.registry_b.engine_arc(&community_id).await.unwrap();
    let outcome = engine_b
        .insert_local_event_with_pubs(fork_event, a_pub, None)
        .await
        .expect("B insert fork");
    assert!(
        matches!(
            outcome,
            harmony_app::community_state_crdt::InsertOutcome::Inserted
        ),
        "engine B must accept the Fork event; got {:?}",
        outcome
    );

    let state_b = engine_b.state().lock().await.clone();
    let b_has_fork = state_b.events().any(|e| {
        matches!(&e.kind, MembershipEventKind::Fork { fork_space_id: fid, .. } if *fid == fork_space_id)
    });
    assert!(
        b_has_fork,
        "engine B must materialize the visible Fork event"
    );

    // Engines may have already stopped if the publisher channels closed;
    // ignore TransportClosed on shutdown.
    let _ = engines.registry_a.shutdown_all().await;
    let _ = engines.registry_b.shutdown_all().await;
}

/// Test 2: silent fork does not insert a Fork event in the original community's log.
/// Engine B's view of the original community is unchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn silent_fork_leaves_original_untouched() {
    let engines = PairedEngines::bootstrap().await;
    let community_id = engines.community_id;

    // Record event count before the fork.
    let event_count_before = {
        let engine_b = engines.registry_b.engine_arc(&community_id).await.unwrap();
        engine_b.state().lock().await.event_count()
    };

    // Run a SILENT fork.
    let result = run_fork_inner(
        &engines.id_a,
        engines.a_addr,
        engines.a_pub,
        community_id,
        &engines.mk,
        &engines.registry_a,
        &engines.registry_a,
        engines.dir_a.path(),
        "Silent Fork",
        true, // silent
        false,
    )
    .await;

    assert!(!result.visible, "silent fork must not be visible");

    // Engine A's original community log must not contain any Fork event.
    let engine_a = engines.registry_a.engine_arc(&community_id).await.unwrap();
    let has_fork_in_original = engine_a
        .state()
        .lock()
        .await
        .events()
        .any(|e| matches!(&e.kind, MembershipEventKind::Fork { .. }));
    assert!(
        !has_fork_in_original,
        "silent fork must NOT insert Fork event into original"
    );

    // B's event count must be unchanged (no new events delivered).
    let event_count_after = {
        let engine_b = engines.registry_b.engine_arc(&community_id).await.unwrap();
        engine_b.state().lock().await.event_count()
    };
    assert_eq!(
        event_count_before, event_count_after,
        "silent fork must not change engine B's event count on the original"
    );

    // Engines may have already stopped if the publisher channels closed;
    // ignore TransportClosed on shutdown.
    let _ = engines.registry_a.shutdown_all().await;
    let _ = engines.registry_b.shutdown_all().await;
}

/// Test 3: fork creates an independent community with the correct invariants:
/// - own SpaceId (different from original)
/// - forked_from = Some(original_community_id)
/// - forker is the admin (power 100) in the new fork community
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fork_creates_independent_community() {
    let engines = PairedEngines::bootstrap().await;
    let community_id = engines.community_id;
    let a_addr = engines.a_addr;
    let a_pub = engines.a_pub;

    let result = run_fork_inner(
        &engines.id_a,
        a_addr,
        a_pub,
        community_id,
        &engines.mk,
        &engines.registry_a,
        &engines.registry_a,
        engines.dir_a.path(),
        "Fork Community",
        false,
        false,
    )
    .await;

    let fork_space_id = result.fork_space_id;

    // Fork must have a different SpaceId than the original.
    assert_ne!(fork_space_id, community_id, "fork must have a new SpaceId");

    // Fork engine must exist and have forked_from set.
    let fork_engine = engines
        .registry_a
        .engine_arc(&fork_space_id)
        .await
        .expect("fork engine must be spawned");
    let fork_state = fork_engine.state().lock().await.clone();

    assert_eq!(
        fork_state.forked_from,
        Some(community_id),
        "fork state must record original community id"
    );
    assert_eq!(
        fork_state.community_id, fork_space_id,
        "fork state community_id matches"
    );

    // Forker must be the admin (power 100) in the fork.
    let mat = fork_state.materialize_now(a_addr);
    let forker_power = mat.power_levels.get(&a_addr).copied().unwrap_or(0);
    assert_eq!(
        forker_power, 100,
        "forker must be power-100 admin in the fork"
    );

    // Engines may have already stopped if the publisher channels closed;
    // ignore TransportClosed on shutdown.
    let _ = engines.registry_a.shutdown_all().await;
    let _ = engines.registry_b.shutdown_all().await;
}

/// Test 4: fork invite carries the pre-fork snapshot to an invitee.
///
/// Builds a CommunityInvitePayload with forked_from + pre_fork_snapshot set
/// (mirroring what `generate_invite` does for fork communities), then checks
/// that:
/// - The payload encodes and decodes correctly.
/// - A simulated invitee writing the snapshot to its data dir can read it back.
///
/// NOTE: The full `generate_invite` → `redeem_invite_inner` path requires Tauri
/// state (NodeState, ChannelLogRegistry<R>, etc.) and cannot be driven directly
/// in an integration test. This test verifies the snapshot-carry invariant at
/// the payload encoding level — the same layer `redeem_invite_inner` calls when
/// it writes the snapshot to disk. Full end-to-end flow is covered by the manual
/// smoke test documented in spec §7.7.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fork_invite_carries_snapshot_to_invitee() {
    let engines = PairedEngines::bootstrap().await;
    let community_id = engines.community_id;
    let a_addr = engines.a_addr;
    let a_pub = engines.a_pub;

    let result = run_fork_inner(
        &engines.id_a,
        a_addr,
        a_pub,
        community_id,
        &engines.mk,
        &engines.registry_a,
        &engines.registry_a,
        engines.dir_a.path(),
        "Fork With Snapshot",
        false,
        false,
    )
    .await;

    let fork_space_id = result.fork_space_id;

    // Read the snapshot written by run_fork_inner from A's data dir.
    let snapshot_path = engines
        .dir_a
        .path()
        .join("communities")
        .join(hex::encode(fork_space_id.0))
        .join("pre_fork_snapshot.bin");
    assert!(
        snapshot_path.exists(),
        "pre_fork_snapshot.bin must exist in A's data dir"
    );

    let bytes = std::fs::read(&snapshot_path).expect("read snapshot");
    let snapshot: PreForkSnapshot = ciborium::de::from_reader(&bytes[..]).expect("decode snapshot");
    assert_eq!(
        snapshot.original_community_id, community_id,
        "snapshot must reference original community"
    );

    // Simulate building an invite payload with the snapshot bundled.
    let fork_engine = engines.registry_a.engine_arc(&fork_space_id).await.unwrap();
    let fork_state = fork_engine.state().lock().await.clone();
    assert_eq!(
        fork_state.forked_from,
        Some(community_id),
        "fork engine must have forked_from set"
    );

    // Build the CommunityInvitePayload (as generate_invite would).
    let epoch_snapshot = harmony_app::community_invite::InviteEpochSnapshot {
        epoch: 0,
        sealed_epoch_key: result.fork_mk.as_bytes().to_vec(),
        sealed_epoch_keys: Vec::new(),
        state_snapshot: harmony_app::community_invite::MaterializedCommunityState {
            members: Default::default(),
            channels: Default::default(),
            power_levels: Default::default(),
        },
    };
    let payload = harmony_app::community_invite::CommunityInvitePayload {
        inviter_signer_certs: Vec::new(),
        community_id: fork_space_id,
        epoch_snapshot,
        admin_addr: a_addr,
        community_name: "Fork With Snapshot".to_string(),
        is_invite_only: false,
        expires_at: None,
        invite_token: None,
        admin_bootstrap: None,
        inviter_identity_pub: None,
        forked_from: Some(community_id),
        pre_fork_snapshot: Some(snapshot.clone()),
        inviter_enrollment: None,
        untargeted_decrypt_key: None,
    };

    // Verify the payload encodes and decodes correctly (the invite wire format).
    let invite_url = harmony_app::build_open_invite_url(&payload).expect("build invite url");
    let decoded_payload =
        harmony_app::community_invite::decode_invite_url(&invite_url).expect("decode invite url");
    assert_eq!(decoded_payload.forked_from, Some(community_id));
    assert!(
        decoded_payload.pre_fork_snapshot.is_some(),
        "fork invite payload must carry pre_fork_snapshot"
    );
    let decoded_snap = decoded_payload.pre_fork_snapshot.unwrap();
    assert_eq!(decoded_snap.original_community_id, community_id);

    // Simulate an invitee ("engine D") writing the snapshot to its data dir.
    let dir_d = tempfile::tempdir().expect("tempdir D");
    let d_fork_dir = dir_d
        .path()
        .join("communities")
        .join(hex::encode(fork_space_id.0));
    std::fs::create_dir_all(&d_fork_dir).expect("mkdir d fork dir");
    let d_snapshot_path = d_fork_dir.join("pre_fork_snapshot.bin");
    let encoded = canonical_cbor_encode(&decoded_snap).expect("encode for D");
    std::fs::write(&d_snapshot_path, &encoded).expect("write D snapshot");

    assert!(
        d_snapshot_path.exists(),
        "engine D must have pre_fork_snapshot.bin on disk"
    );
    let d_bytes = std::fs::read(&d_snapshot_path).expect("read D snapshot");
    let d_snap: PreForkSnapshot = ciborium::de::from_reader(&d_bytes[..]).expect("decode D");
    assert_eq!(d_snap.original_community_id, community_id);

    // Engines may have already stopped if the publisher channels closed;
    // ignore TransportClosed on shutdown.
    let _ = engines.registry_a.shutdown_all().await;
    let _ = engines.registry_b.shutdown_all().await;
}

/// Test 5: fork with `also_leave=true` emits both a Fork event and a Leave event
/// in the original community's log.
///
/// EpochRotation auto-fire from Leave is a ZEB-249 responsibility — in this harness
/// the engine doesn't auto-rotate (requires the owner-state CRDT reference), so we
/// assert on Fork + Leave presence and note the rotation expectation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn also_leave_emits_fork_and_leave_events() {
    let engines = PairedEngines::bootstrap().await;
    let community_id = engines.community_id;
    let a_addr = engines.a_addr;
    let a_pub = engines.a_pub;

    let result = run_fork_inner(
        &engines.id_a,
        a_addr,
        a_pub,
        community_id,
        &engines.mk,
        &engines.registry_a,
        &engines.registry_a,
        engines.dir_a.path(),
        "Fork And Leave",
        false, // visible fork
        true,  // also_leave
    )
    .await;

    assert!(result.visible, "fork with also_leave must be visible");

    // Check A's original community for both Fork and Leave events.
    let engine_a = engines.registry_a.engine_arc(&community_id).await.unwrap();
    let state = engine_a.state().lock().await.clone();

    let has_fork = state.events().any(|e| {
        matches!(&e.kind, MembershipEventKind::Fork { fork_space_id: fid, .. }
            if *fid == result.fork_space_id)
    });
    let has_leave = state
        .events()
        .any(|e| matches!(&e.kind, MembershipEventKind::Leave) && e.actor == a_addr);

    assert!(
        has_fork,
        "Fork event must be present in original after also_leave fork"
    );
    assert!(
        has_leave,
        "Leave event must be present in original after also_leave fork"
    );

    // Verify the Leave makes the forker no longer a Joined member.
    let mat = state.materialize_now(a_addr);
    let forker_status = mat.members.get(&a_addr).map(|m| m.status);
    assert!(
        !matches!(
            forker_status,
            Some(harmony_app::community_membership::MemberStatus::Joined)
        ),
        "forker must no longer be Joined after Leave; status: {:?}",
        forker_status
    );

    // Propagate Fork + Leave to B and verify B sees them.
    let fork_event = state
        .events()
        .find(|e| matches!(&e.kind, MembershipEventKind::Fork { .. }))
        .cloned()
        .unwrap();
    let leave_event = state
        .events()
        .find(|e| matches!(&e.kind, MembershipEventKind::Leave) && e.actor == a_addr)
        .cloned()
        .unwrap();

    let engine_b = engines.registry_b.engine_arc(&community_id).await.unwrap();
    engine_b
        .insert_local_event_with_pubs(fork_event, a_pub, None)
        .await
        .expect("B insert fork");
    engine_b
        .insert_local_event_with_pubs(leave_event, a_pub, None)
        .await
        .expect("B insert leave");

    let b_state = engine_b.state().lock().await.clone();
    assert!(
        b_state
            .events()
            .any(|e| matches!(&e.kind, MembershipEventKind::Fork { .. })),
        "B must see Fork event"
    );
    assert!(
        b_state
            .events()
            .any(|e| matches!(&e.kind, MembershipEventKind::Leave) && e.actor == a_addr),
        "B must see Leave event"
    );

    // Engines may have already stopped if the publisher channels closed;
    // ignore TransportClosed on shutdown.
    let _ = engines.registry_a.shutdown_all().await;
    let _ = engines.registry_b.shutdown_all().await;
}

/// Test 6: dual-keyset verify — snapshot events verify against
/// `pre_fork_snapshot.identity_pubs`, not against the fork's live OwnerDeviceCache.
///
/// This is a pure-function test: it constructs a PreForkSnapshot with real signed
/// events and verifies each event via `verify_snapshot_event`. No engine needed.
#[test]
fn dual_keyset_verify_snapshot_events() {
    use harmony_app::community_membership::verify_snapshot_event;

    let original_id = SpaceId([0xa0; 16]);
    let id_a = PrivateIdentity::from_seed(&[0xa1; 32]);
    let a_addr = OwnerAddr(id_a.identity.address_hash);
    let a_pub = id_a.identity.to_public_bytes();

    let id_b = PrivateIdentity::from_seed(&[0xb1; 32]);
    let b_addr = OwnerAddr(id_b.identity.address_hash);
    let b_pub = id_b.identity.to_public_bytes();

    // Build signed events using sign_event_with_identity (production signing path).
    let a_join = {
        let payload = EventPayload {
            id: [0x01u8; 16],
            community_id: original_id,
            kind: MembershipEventKind::Join,
            actor: a_addr,
            at: test_hlc(1_700_000_000_000, 0, "a-dev"),
        };
        sign_event_with_identity(&payload, &id_a).expect("sign a join")
    };
    let b_join = {
        let payload = EventPayload {
            id: [0x02u8; 16],
            community_id: original_id,
            kind: MembershipEventKind::Join,
            actor: b_addr,
            at: test_hlc(1_700_000_001_000, 0, "b-dev"),
        };
        sign_event_with_identity(&payload, &id_b).expect("sign b join")
    };

    // Build the snapshot: identity_pubs contains both signers.
    let mut identity_pubs: BTreeMap<OwnerAddr, [u8; 64]> = BTreeMap::new();
    identity_pubs.insert(a_addr, a_pub);
    identity_pubs.insert(b_addr, b_pub);

    let snapshot = PreForkSnapshot {
        original_community_id: original_id,
        original_community_name: "Original Community".to_string(),
        membership_events: vec![a_join.clone(), b_join.clone()],
        channel_log: BoundedChannelLogSnapshot::default(),
        identity_pubs,
        forked_at: test_hlc(1_700_000_002_000, 0, "a-dev"),
        parent_lineage: Vec::new(),
        fork_reason: None,
    };

    // Each event must verify against the snapshot's identity_pubs.
    // This verifies against the snapshot keyset — NOT a live OwnerDeviceCache.
    verify_snapshot_event(&a_join, &snapshot)
        .expect("A's join must verify against snapshot identity_pubs");
    verify_snapshot_event(&b_join, &snapshot)
        .expect("B's join must verify against snapshot identity_pubs");

    // An event from an unknown signer (not in identity_pubs) must be rejected.
    let id_c = PrivateIdentity::from_seed(&[0xc1; 32]);
    let c_addr = OwnerAddr(id_c.identity.address_hash);
    let c_event = {
        let payload = EventPayload {
            id: [0x03u8; 16],
            community_id: original_id,
            kind: MembershipEventKind::Leave,
            actor: c_addr,
            at: test_hlc(1_700_000_003_000, 0, "c-dev"),
        };
        sign_event_with_identity(&payload, &id_c).expect("sign c leave")
    };
    let result = verify_snapshot_event(&c_event, &snapshot);
    assert!(
        matches!(
            result,
            Err(harmony_app::community_membership::VerifyError::UnknownSigner { .. })
        ),
        "unknown signer must be rejected by verify_snapshot_event; got {:?}",
        result
    );
}

// ── ZEB-287 Phase 2 spec §7.2: multi-hop lineage integration tests ────
//
// These tests verify the parent_lineage extension/cap logic at the
// integration boundary. We test the chain-construction algorithm
// directly (mirrors community_fork.rs::fork_community's Task 4 block)
// because run_fork_inner above is a manual fork composition that
// doesn't exercise the production fork-build code path.

#[test]
fn three_deep_fork_chain_preserves_lineage_through_snapshot() {
    // Simulate three generations of forks: C → B → A_fork → A_fork_2.
    // Each generation extends the previous's parent_lineage by pushing
    // the immediate-parent's entry — exercises the SHARED
    // `build_parent_lineage` helper (R1-4) used by production
    // `community_fork.rs::fork_community`, so a regression there fails
    // here too.

    use harmony_app::community_invite::{build_parent_lineage, ParentLineageEntry};

    // Generation C (top-level): no chain.
    let c_id = SpaceId([0x11; 16]);
    let c_name = "C";
    let c_forked_at: Option<u64> = None; // C is root

    // Forking C → B (B is forker_community.id at the time):
    //   B.parent_lineage = build_parent_lineage([], C.id, C.name, None)
    //                    = [C-entry]
    let b_id = SpaceId([0x22; 16]);
    let b_name = "B";
    let b_forked_at = Some(1_700_000_000_000u64);
    let b_lineage: Vec<ParentLineageEntry> =
        build_parent_lineage(&[], c_id, c_name, c_forked_at, None);
    assert_eq!(b_lineage.len(), 1);
    assert_eq!(b_lineage[0].space_id, c_id);
    assert_eq!(b_lineage[0].forked_at_wall_ms, None);

    // Forking B → A_fork:
    //   A_fork.parent_lineage = build_parent_lineage(B.parent_lineage,
    //                              B.id, B.name, B.forked_at_wall_ms)
    //                         = [C-entry, B-entry]
    let a_fork_id = SpaceId([0x33; 16]);
    let a_fork_lineage = build_parent_lineage(&b_lineage, b_id, b_name, b_forked_at, None);
    assert_eq!(a_fork_lineage.len(), 2);
    assert_eq!(a_fork_lineage[0].space_id, c_id);
    assert_eq!(a_fork_lineage[0].name, "C");
    assert_eq!(a_fork_lineage[0].forked_at_wall_ms, None);
    assert_eq!(a_fork_lineage[1].space_id, b_id);
    assert_eq!(a_fork_lineage[1].name, "B");
    assert_eq!(a_fork_lineage[1].forked_at_wall_ms, b_forked_at);

    // Forking A_fork → A_fork_2:
    //   A_fork_2.parent_lineage = [C-entry, B-entry, A_fork-entry]
    let a_fork_forked_at = Some(1_710_000_000_000u64);
    let a_fork_2_lineage =
        build_parent_lineage(&a_fork_lineage, a_fork_id, "A_fork", a_fork_forked_at, None);
    assert_eq!(a_fork_2_lineage.len(), 3);
    assert_eq!(a_fork_2_lineage[0].name, "C");
    assert_eq!(a_fork_2_lineage[1].name, "B");
    assert_eq!(a_fork_2_lineage[2].name, "A_fork");
    assert_eq!(a_fork_2_lineage[2].forked_at_wall_ms, a_fork_forked_at);
}

#[test]
fn lineage_depth_cap_truncates_root_side_through_fork_path() {
    // Spec §3.4: when forker's parent_lineage already has 16 entries and
    // we push the forker's own entry (now 17), the cap drains entry 0
    // (the oldest, root-side). After cap: 16 entries, originally [1..17).
    // Exercises the SHARED `build_parent_lineage` helper (R1-4) which
    // owns the cap logic in production.

    use harmony_app::community_invite::{build_parent_lineage, ParentLineageEntry};

    // Simulate a forker whose CommunityState already has a 16-deep chain.
    let forker_lineage: Vec<ParentLineageEntry> = (0u8..16)
        .map(|i| ParentLineageEntry {
            space_id: SpaceId([i; 16]),
            name: format!("ancestor_{i}"),
            forked_at_wall_ms: if i == 0 { None } else { Some(i as u64) },
            reason: None,
        })
        .collect();
    assert_eq!(forker_lineage.len(), 16);
    assert_eq!(forker_lineage[0].name, "ancestor_0"); // root

    // Forker is forking their own community, which has SpaceId/name/forked_at.
    let forker_id = SpaceId([0xfe; 16]);
    let forker_name = "ForkerSelf";
    let forker_forked_at = Some(1_800_000_000_000u64);

    // Drive the SHARED helper — same code path as production.
    let new_lineage = build_parent_lineage(
        &forker_lineage,
        forker_id,
        forker_name,
        forker_forked_at,
        None,
    );

    assert_eq!(new_lineage.len(), 16);
    // After cap: ancestor_0 dropped; first entry is ancestor_1.
    assert_eq!(new_lineage[0].name, "ancestor_1");
    // Last entry is the forker itself.
    assert_eq!(new_lineage[15].name, "ForkerSelf");
    assert_eq!(new_lineage[15].forked_at_wall_ms, forker_forked_at);
}

#[test]
fn phase1_snapshot_redeems_with_default_lineage() {
    // Spec §6.2: a Phase 1-shape fork-invite (no `parent_lineage`)
    // round-trips through CBOR encode/decode as empty Vec, and a Phase 2
    // client reading such an invite gets the correct default state.

    use harmony_app::community_invite::{ParentLineageEntry, PreForkSnapshot};

    let phase1_shaped_snapshot = PreForkSnapshot {
        original_community_id: SpaceId([0xc0; 16]),
        original_community_name: "Phase1Origin".to_string(),
        membership_events: vec![],
        channel_log: BoundedChannelLogSnapshot::default(),
        identity_pubs: std::collections::BTreeMap::new(),
        forked_at: test_hlc(1_710_000_000_000, 0, "phase1-dev"),
        parent_lineage: Vec::new(), // Phase 1 default — empty
        fork_reason: None,
    };

    // Encode as Phase 1 would (because skip-if-empty drops the `pl` key).
    let bytes = canonical_cbor_encode(&phase1_shaped_snapshot).expect("encode");

    // Assert the encoded form omits the `pl` key — proving Phase 2's
    // skip_serializing_if preserves Phase 1 wire format byte-identically.
    assert!(
        !bytes.windows(2).any(|w| w == b"pl"),
        "Phase 1-shape snapshot must NOT contain `pl` key"
    );

    // Decode under Phase 2 types: the missing `pl` key resolves to
    // empty Vec via #[serde(default)].
    let decoded: PreForkSnapshot =
        harmony_app::owner_state_crypto::canonical_cbor_decode(&bytes).expect("decode");
    assert_eq!(decoded.parent_lineage, Vec::<ParentLineageEntry>::new());
    // forked_at is Phase 1's existing field, must round-trip.
    assert_eq!(decoded.forked_at.wall_ms, 1_710_000_000_000);
    assert_eq!(decoded.original_community_id, SpaceId([0xc0; 16]));
}

/// R1-1b regression: `community_fork.rs::fork_community` (and its test
/// mirror `run_fork_inner`) must persist the new fork's `parent_lineage`
/// and `forked_at_wall_ms` into the new fork's `CommunityState`. Without
/// this, a subsequent fork of THIS new fork reads an empty ancestry and
/// the multi-hop chain truncates.
///
/// This test drives `run_fork_inner` twice (C → B then B → A) and asserts
/// the engine-state mutation made by both fork operations.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_fork_creation_persists_lineage_into_new_state() {
    let engines = PairedEngines::bootstrap().await;
    let c_id = engines.community_id; // The "C" community: top-level / root.
    let a_addr = engines.a_addr;
    let a_pub = engines.a_pub;

    // First fork: C → B.
    let b_result = run_fork_inner(
        &engines.id_a,
        a_addr,
        a_pub,
        c_id,
        &engines.mk,
        &engines.registry_a,
        &engines.registry_a, // same registry — A holds both engines
        engines.dir_a.path(),
        "B",
        true, // silent — keeps test focused on local persistence
        false,
    )
    .await;
    let b_id = b_result.fork_space_id;

    // Inspect B's CommunityState: it should have parent_lineage = [C-entry]
    // (C is root, so C's forked_at_wall_ms = None at the time C was the
    // forker) and forked_at_wall_ms = Some(wall_ms used in the fork mint).
    let b_engine = engines.registry_a.engine_arc(&b_id).await.unwrap();
    let b_state = b_engine.state().lock().await.clone();
    assert_eq!(b_state.forked_from, Some(c_id));
    assert!(
        b_state.forked_at_wall_ms.is_some(),
        "B's forked_at_wall_ms must be set after fork (R1-1b)"
    );
    assert_eq!(
        b_state.parent_lineage.len(),
        1,
        "B's parent_lineage must contain exactly one entry (C) after first fork (R1-1b)"
    );
    assert_eq!(
        b_state.parent_lineage[0].space_id, c_id,
        "B's parent_lineage entry must reference C (R1-1b)"
    );
    assert_eq!(
        b_state.parent_lineage[0].forked_at_wall_ms, None,
        "C is root → its forked_at_wall_ms entry must be None (R1-1b)"
    );

    // Second fork: B → A. Reads B's lineage state (set above), extends it.
    let a_result = run_fork_inner(
        &engines.id_a,
        a_addr,
        a_pub,
        b_id, // forking B this time
        &b_result.fork_mk,
        &engines.registry_a,
        &engines.registry_a,
        engines.dir_a.path(),
        "A",
        true,
        false,
    )
    .await;
    let a_id = a_result.fork_space_id;

    // Inspect A's CommunityState: parent_lineage should be [C-entry, B-entry]
    // — the chain extends through the production helper.
    let a_engine = engines.registry_a.engine_arc(&a_id).await.unwrap();
    let a_state = a_engine.state().lock().await.clone();
    assert_eq!(a_state.forked_from, Some(b_id));
    assert!(
        a_state.forked_at_wall_ms.is_some(),
        "A's forked_at_wall_ms must be set (R1-1b)"
    );
    assert_eq!(
        a_state.parent_lineage.len(),
        2,
        "A's parent_lineage must contain two entries (C, B) after second fork (R1-1b)"
    );
    assert_eq!(a_state.parent_lineage[0].space_id, c_id);
    assert_eq!(a_state.parent_lineage[0].forked_at_wall_ms, None);
    assert_eq!(a_state.parent_lineage[1].space_id, b_id);
    assert_eq!(
        a_state.parent_lineage[1].forked_at_wall_ms, b_state.forked_at_wall_ms,
        "B-entry's forked_at_wall_ms must equal B's stored value (R1-1b)"
    );
}
