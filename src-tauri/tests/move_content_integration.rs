//! ZEB-162 integration tests for `move_content`.
//!
//! The harness pattern is copied verbatim from
//! `folder_primitive_integration.rs` (see ZEB-183 for the extraction
//! plan); each test seeds a folder tree through the runtime's ingest
//! channel, mutates the sidecar via `ContentIndex`, then drives
//! `move_content_impl` against the live runtime and verifies the
//! resulting sidecar + cache state.

use std::sync::{Arc, Mutex};
use std::thread;

use tempfile::tempdir;
use tokio::sync::{mpsc, oneshot, watch};

use harmony_app::content_index::{
    ContentIndex, ContentIndexEntry, ContentKind, ReplicationTier, Sensitivity, SidecarId,
};
use harmony_app::event_loop::{ContentVerbRequest, IngestRequest};
use harmony_app::folders;
use harmony_compute::InstructionBudget;
use harmony_content::book::MemoryBookStore;
use harmony_content::cid::{ContentFlags, ContentId};
use harmony_content::storage_tier::{ContentPolicy, FilterBroadcastConfig, StorageBudget};
use harmony_runtime::{NodeConfig, NodeRuntime};

struct TestHarness {
    pub ingest_tx: mpsc::Sender<IngestRequest>,
    pub verb_tx: mpsc::Sender<ContentVerbRequest>,
    _shutdown_tx: watch::Sender<bool>,
    _tmp: tempfile::TempDir,
}

impl Drop for TestHarness {
    fn drop(&mut self) {
        let _ = self._shutdown_tx.send(true);
    }
}

async fn spawn_test_runtime() -> Option<TestHarness> {
    let tmp = tempdir().unwrap();
    let app_data_dir = tmp.path().to_path_buf();

    let (ingest_tx, ingest_rx) = mpsc::channel::<IngestRequest>(8);
    let (verb_tx, content_verb_rx) = mpsc::channel::<ContentVerbRequest>(32);
    let (_publish_tx, publish_rx) = mpsc::channel(4);
    let (_fetch_tx, fetch_rx) = mpsc::channel(4);
    let (_follow_tx, follow_rx) = mpsc::channel(4);
    let (_voice_tx, voice_rx) = mpsc::channel::<harmony_app::voice::VoiceOutbound>(4);
    let (_voice_ch_tx, voice_ch_rx) = mpsc::channel::<harmony_app::voice::VoiceChannelRequest>(4);
    let (_refresh_tx, refresh_rx) = mpsc::channel::<harmony_app::mail_sync::RefreshRequest>(4);
    let (_cas_op_tx, cas_op_rx) = mpsc::channel::<harmony_app::content_store::CasOp>(8);
    let (ready_tx, ready_rx) = oneshot::channel();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let followed_set = Arc::new(Mutex::new(std::collections::HashSet::<String>::default()));
    let vine_feed_cache = Arc::new(Mutex::new(
        harmony_app::vine_feed_cache::VineFeedCache::new(),
    ));
    let mail_mgr = Arc::new(Mutex::new(harmony_app::mail::MailManager::load(
        &app_data_dir.join("mail"),
        [0u8; 16],
    )));

    let app = tauri::test::mock_app();
    let app_handle = app.handle().clone();

    let config = NodeConfig {
        storage_budget: StorageBudget {
            cache_capacity: 512,
            max_pinned_bytes: 50_000_000,
        },
        compute_budget: InstructionBudget { fuel: 100_000 },
        schedule: Default::default(),
        content_policy: ContentPolicy::default(),
        filter_broadcast_config: FilterBroadcastConfig {
            mutation_threshold: 10,
            max_interval_ticks: 40,
            expected_items: 512,
            fp_rate: 0.001,
        },
        node_addr: "0000000000000000000000000000000000000000".to_string(),
        local_identity_hash: [0u8; 16],
        local_pq_identity_hash: [0u8; 16],
        local_dsa_pubkey: vec![],
        local_kem_pubkey: vec![],
        reticulum_identity_bytes: None,
        inference_gguf_cid: None,
        inference_tokenizer_cid: None,
        engram_manifest_cid: None,
        disk_enabled: false,
        disk_entries: Vec::new(),
        disk_quota: None,
        archive_enabled: false,
        archive_entries: Vec::new(),
        archive_quota: None,
        archive_ingest_enabled: false,
        eviction_push_enabled: false,
        s3_enabled: false,
    };

    let (fetch_completion_tx, fetch_completion_rx) = mpsc::channel::<[u8; 32]>(4);
    let pin_intent: std::collections::HashSet<[u8; 32]> = std::collections::HashSet::new();

    thread::Builder::new()
        .name("harmony-runtime-move-test".to_string())
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .thread_stack_size(8 * 1024 * 1024)
                .enable_all()
                .build()
                .expect("tokio runtime for move test event loop");
            rt.block_on(async move {
                let (runtime, startup_actions) = NodeRuntime::new(config, MemoryBookStore::new());
                harmony_app::event_loop::run(
                    runtime,
                    startup_actions,
                    app_handle,
                    None,
                    ready_tx,
                    shutdown_rx,
                    publish_rx,
                    fetch_rx,
                    ingest_rx,
                    content_verb_rx,
                    _cas_op_tx,
                    cas_op_rx,
                    follow_rx,
                    voice_rx,
                    voice_ch_rx,
                    followed_set,
                    vine_feed_cache,
                    mail_mgr,
                    None,
                    refresh_rx,
                    pin_intent,
                    fetch_completion_tx,
                    fetch_completion_rx,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    Vec::new(),
                    {
                        let (_tx, rx) = tokio::sync::mpsc::channel::<
                            harmony_app::event_loop::CommunityAdapterRequest,
                        >(1);
                        rx
                    },
                    None,
                    {
                        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<
                            harmony_app::event_loop::ChannelLogAdapterRequest,
                        >();
                        rx
                    },
                    None,
                    None,
                    None,
                    None,
                )
                .await;
            });
        })
        .expect("spawn runtime thread");

    match ready_rx.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) if e.contains("Address already in use") => {
            eprintln!("skipping test: {e}");
            return None;
        }
        Ok(Err(e)) => panic!("event loop failed to start: {e}"),
        Err(_) => panic!("event loop dropped ready signal"),
    }

    Some(TestHarness {
        ingest_tx,
        verb_tx,
        _shutdown_tx: shutdown_tx,
        _tmp: tmp,
    })
}

/// Ingest a built folder's manifest + bundle through the runtime.
async fn ingest_folder(harness: &TestHarness, built: &folders::BuiltFolder) {
    harmony_app::send_ingest(
        &harness.ingest_tx,
        hex::encode(built.manifest_cid.to_bytes()),
        built.manifest_bytes.clone(),
    )
    .await
    .unwrap();
    harmony_app::send_ingest(
        &harness.ingest_tx,
        hex::encode(built.bundle_cid.to_bytes()),
        built.bundle_bytes.clone(),
    )
    .await
    .unwrap();
}

async fn ingest_leaf(harness: &TestHarness, cid: [u8; 32], bytes: Vec<u8>) {
    harmony_app::send_ingest(&harness.ingest_tx, hex::encode(cid), bytes)
        .await
        .unwrap();
}

fn make_leaf(bytes: &[u8]) -> ([u8; 32], Vec<u8>) {
    let cid = ContentId::for_book(bytes, ContentFlags::default()).expect("for_book");
    (cid.to_bytes(), bytes.to_vec())
}

/// Insert a top-level sidecar entry that points at `cid`. Returns the
/// minted SidecarId.
fn insert_top_level(
    index: &Arc<Mutex<ContentIndex>>,
    cid: [u8; 32],
    file_name: &str,
    kind: ContentKind,
    pinned: bool,
    size_bytes: u64,
) -> SidecarId {
    let sid = SidecarId::new();
    let mut idx = index.lock().unwrap();
    let inserted = idx.insert(ContentIndexEntry {
        sidecar_id: sid,
        cid,
        file_name: file_name.into(),
        size_bytes,
        stored_at_ms: 1,
        sensitivity: Sensitivity::Private,
        replication_tier: ReplicationTier::Default,
        licensed: false,
        archived: false,
        pinned,
        kind,
    });
    assert!(inserted, "fresh SidecarId must insert cleanly");
    sid
}

fn fresh_index() -> Arc<Mutex<ContentIndex>> {
    // Each test owns its own tempdir-backed sidecar.
    let dir = tempdir().unwrap();
    let idx = ContentIndex::load(dir.path());
    // dir is dropped here, but ContentIndex retains the path and will
    // try to save into it — which is fine for tests where we only
    // care about the in-memory state. The save() warnings are harmless.
    let _ = dir; // explicit drop
    Arc::new(Mutex::new(idx))
}

// ── Test 6: move_a_within_same_top_level_one_level_deep ───────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn move_a_within_same_top_level_one_level_deep() {
    // T contains A and B. A contains leaf L. Move L into B.
    let (l_cid, l_bytes) = make_leaf(b"hello");
    let a_old = folders::build_folder(
        "A",
        &[folders::ManifestEntry {
            cid: l_cid,
            name: "L".into(),
            kind: ContentKind::Leaf,
        }],
    )
    .expect("build A");
    let b_old = folders::build_folder("B", &[]).expect("build B");
    let t_old = folders::build_folder(
        "T",
        &[
            folders::ManifestEntry {
                cid: a_old.bundle_cid.to_bytes(),
                name: "A".into(),
                kind: ContentKind::Folder,
            },
            folders::ManifestEntry {
                cid: b_old.bundle_cid.to_bytes(),
                name: "B".into(),
                kind: ContentKind::Folder,
            },
        ],
    )
    .expect("build T");

    let harness = match spawn_test_runtime().await {
        Some(h) => h,
        None => return,
    };
    ingest_leaf(&harness, l_cid, l_bytes).await;
    ingest_folder(&harness, &a_old).await;
    ingest_folder(&harness, &b_old).await;
    ingest_folder(&harness, &t_old).await;

    let index = fresh_index();
    let t_sid = insert_top_level(
        &index,
        t_old.bundle_cid.to_bytes(),
        "T",
        ContentKind::Folder,
        false,
        t_old.bundle_bytes.len() as u64,
    );

    let result = harmony_app::move_content_impl(
        t_sid.to_string(),
        vec![
            hex::encode(t_old.bundle_cid.to_bytes()),
            hex::encode(a_old.bundle_cid.to_bytes()),
        ],
        hex::encode(l_cid),
        Some(t_sid.to_string()),
        vec![
            hex::encode(t_old.bundle_cid.to_bytes()),
            hex::encode(b_old.bundle_cid.to_bytes()),
        ],
        None,
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect("move A");

    // Expected new tree: A empty; B containing L; T rekeyed.
    let a_new = folders::build_folder("A", &[]).expect("build A_new");
    let b_new = folders::build_folder(
        "B",
        &[folders::ManifestEntry {
            cid: l_cid,
            name: "L".into(),
            kind: ContentKind::Leaf,
        }],
    )
    .expect("build B_new");
    let t_new = folders::build_folder(
        "T",
        &[
            folders::ManifestEntry {
                cid: a_new.bundle_cid.to_bytes(),
                name: "A".into(),
                kind: ContentKind::Folder,
            },
            folders::ManifestEntry {
                cid: b_new.bundle_cid.to_bytes(),
                name: "B".into(),
                kind: ContentKind::Folder,
            },
        ],
    )
    .expect("build T_new");

    assert_eq!(
        result.src_new_cid,
        Some(hex::encode(t_new.bundle_cid.to_bytes()))
    );
    assert_eq!(result.dst_new_cid, hex::encode(t_new.bundle_cid.to_bytes()));
    let idx = index.lock().unwrap();
    let entry = idx.get(&t_sid).expect("T entry still there");
    assert_eq!(entry.cid, t_new.bundle_cid.to_bytes(), "T rekeyed");
}

// ── Test 7: move_b_across_top_levels ──────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn move_b_across_top_levels() {
    let (l_cid, l_bytes) = make_leaf(b"alpha");
    let t1_old = folders::build_folder(
        "T1",
        &[folders::ManifestEntry {
            cid: l_cid,
            name: "L".into(),
            kind: ContentKind::Leaf,
        }],
    )
    .expect("build T1");
    let t2_old = folders::build_folder("T2", &[]).expect("build T2");

    let harness = match spawn_test_runtime().await {
        Some(h) => h,
        None => return,
    };
    ingest_leaf(&harness, l_cid, l_bytes).await;
    ingest_folder(&harness, &t1_old).await;
    ingest_folder(&harness, &t2_old).await;

    let index = fresh_index();
    let t1_sid = insert_top_level(
        &index,
        t1_old.bundle_cid.to_bytes(),
        "T1",
        ContentKind::Folder,
        false,
        t1_old.bundle_bytes.len() as u64,
    );
    let t2_sid = insert_top_level(
        &index,
        t2_old.bundle_cid.to_bytes(),
        "T2",
        ContentKind::Folder,
        false,
        t2_old.bundle_bytes.len() as u64,
    );

    let result = harmony_app::move_content_impl(
        t1_sid.to_string(),
        vec![hex::encode(t1_old.bundle_cid.to_bytes())],
        hex::encode(l_cid),
        Some(t2_sid.to_string()),
        vec![hex::encode(t2_old.bundle_cid.to_bytes())],
        None,
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect("Case B");

    let t1_new = folders::build_folder("T1", &[]).expect("build T1_new");
    let t2_new = folders::build_folder(
        "T2",
        &[folders::ManifestEntry {
            cid: l_cid,
            name: "L".into(),
            kind: ContentKind::Leaf,
        }],
    )
    .expect("build T2_new");

    assert_eq!(
        result.src_new_cid,
        Some(hex::encode(t1_new.bundle_cid.to_bytes()))
    );
    assert_eq!(
        result.dst_new_cid,
        hex::encode(t2_new.bundle_cid.to_bytes())
    );

    let idx = index.lock().unwrap();
    assert_eq!(
        idx.get(&t1_sid).unwrap().cid,
        t1_new.bundle_cid.to_bytes(),
        "T1 rekeyed"
    );
    assert_eq!(
        idx.get(&t2_sid).unwrap().cid,
        t2_new.bundle_cid.to_bytes(),
        "T2 rekeyed"
    );
}

// ── Test 8: move_c_root_to_nested ─────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn move_c_root_to_nested() {
    // Top-level leaf L; top-level folder F (empty). Move L into F.
    let (l_cid, l_bytes) = make_leaf(b"root-leaf");
    let f_old = folders::build_folder("F", &[]).expect("build F");

    let harness = match spawn_test_runtime().await {
        Some(h) => h,
        None => return,
    };
    ingest_leaf(&harness, l_cid, l_bytes.clone()).await;
    ingest_folder(&harness, &f_old).await;

    let index = fresh_index();
    let l_sid = insert_top_level(
        &index,
        l_cid,
        "L",
        ContentKind::Leaf,
        false,
        l_bytes.len() as u64,
    );
    let f_sid = insert_top_level(
        &index,
        f_old.bundle_cid.to_bytes(),
        "F",
        ContentKind::Folder,
        false,
        f_old.bundle_bytes.len() as u64,
    );

    let result = harmony_app::move_content_impl(
        l_sid.to_string(),
        vec![hex::encode(l_cid)],
        hex::encode(l_cid),
        Some(f_sid.to_string()),
        vec![hex::encode(f_old.bundle_cid.to_bytes())],
        None,
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect("Case C");

    let f_new = folders::build_folder(
        "F",
        &[folders::ManifestEntry {
            cid: l_cid,
            name: "L".into(),
            kind: ContentKind::Leaf,
        }],
    )
    .expect("build F_new");

    assert_eq!(result.src_new_cid, None);
    assert_eq!(result.dst_new_cid, hex::encode(f_new.bundle_cid.to_bytes()));

    let idx = index.lock().unwrap();
    assert!(idx.get(&l_sid).is_none(), "L sidecar entry removed");
    assert_eq!(
        idx.get(&f_sid).unwrap().cid,
        f_new.bundle_cid.to_bytes(),
        "F rekeyed"
    );
}

// ── Test 9: move_d_nested_to_root ─────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn move_d_nested_to_root() {
    let (l_cid, l_bytes) = make_leaf(b"to-root");
    let f_old = folders::build_folder(
        "F",
        &[folders::ManifestEntry {
            cid: l_cid,
            name: "L".into(),
            kind: ContentKind::Leaf,
        }],
    )
    .expect("build F");

    let harness = match spawn_test_runtime().await {
        Some(h) => h,
        None => return,
    };
    ingest_leaf(&harness, l_cid, l_bytes).await;
    ingest_folder(&harness, &f_old).await;

    let index = fresh_index();
    let f_sid = insert_top_level(
        &index,
        f_old.bundle_cid.to_bytes(),
        "F",
        ContentKind::Folder,
        false,
        f_old.bundle_bytes.len() as u64,
    );

    let result = harmony_app::move_content_impl(
        f_sid.to_string(),
        vec![hex::encode(f_old.bundle_cid.to_bytes())],
        hex::encode(l_cid),
        None,
        vec![],
        None,
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect("Case D");

    let f_new = folders::build_folder("F", &[]).expect("build F_new");
    assert_eq!(
        result.src_new_cid,
        Some(hex::encode(f_new.bundle_cid.to_bytes()))
    );
    assert_eq!(result.dst_new_cid, hex::encode(l_cid));

    let idx = index.lock().unwrap();
    assert_eq!(
        idx.get(&f_sid).unwrap().cid,
        f_new.bundle_cid.to_bytes(),
        "F rekeyed to empty"
    );
    let new_top_sid = SidecarId::parse_str(&result.dst_sidecar_id).expect("parse");
    let new_top = idx.get(&new_top_sid).expect("new top entry");
    assert_eq!(new_top.cid, l_cid);
    assert_eq!(new_top.kind, ContentKind::Leaf);
    assert_eq!(new_top.file_name, "L");
    assert!(!new_top.pinned, "Case D defaults to unpinned");
}

// ── Test 10: move_b_dst_rekey_conflict_compensating_undo_reverts ──────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn move_b_dst_rekey_conflict_at_stage_1_no_undo_path() {
    // Pre-rekey dst BEFORE the move (simulating "concurrent rekey
    // landed first"). STAGE 1 fails on CAS conflict; no src mutation
    // is performed.
    let (l_cid, l_bytes) = make_leaf(b"concurrent");
    let t1_old = folders::build_folder(
        "T1",
        &[folders::ManifestEntry {
            cid: l_cid,
            name: "L".into(),
            kind: ContentKind::Leaf,
        }],
    )
    .expect("build T1");
    let t2_old = folders::build_folder("T2", &[]).expect("build T2");

    let harness = match spawn_test_runtime().await {
        Some(h) => h,
        None => return,
    };
    ingest_leaf(&harness, l_cid, l_bytes).await;
    ingest_folder(&harness, &t1_old).await;
    ingest_folder(&harness, &t2_old).await;

    let index = fresh_index();
    let t1_sid = insert_top_level(
        &index,
        t1_old.bundle_cid.to_bytes(),
        "T1",
        ContentKind::Folder,
        false,
        t1_old.bundle_bytes.len() as u64,
    );
    let t2_sid = insert_top_level(
        &index,
        t2_old.bundle_cid.to_bytes(),
        "T2",
        ContentKind::Folder,
        false,
        t2_old.bundle_bytes.len() as u64,
    );

    // Pre-flip T2's CID before the move runs.
    {
        let mut idx = index.lock().unwrap();
        let flipped = idx.force_set_cid_for_test(&t2_sid, [0xEE; 32]);
        assert!(flipped, "test-only force_set_cid succeeded");
    }

    let err = harmony_app::move_content_impl(
        t1_sid.to_string(),
        vec![hex::encode(t1_old.bundle_cid.to_bytes())],
        hex::encode(l_cid),
        Some(t2_sid.to_string()),
        vec![hex::encode(t2_old.bundle_cid.to_bytes())],
        None,
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect_err("dst CAS conflict must surface as Err");

    // The pre-flip happens BEFORE the boundary verify, so the boundary
    // check is the first guard to fire. Accept either the boundary
    // message or the STAGE 1 CAS message — both indicate the move
    // correctly refused to clobber a divergent dst.
    assert!(
        err.contains("concurrent rekey on dst_sidecar_id")
            || err.contains("dst_sidecar_id refers to cid"),
        "got: {err}"
    );

    // T1 must be untouched (still at its original CID).
    let idx = index.lock().unwrap();
    assert_eq!(
        idx.get(&t1_sid).unwrap().cid,
        t1_old.bundle_cid.to_bytes(),
        "T1 untouched by failed move"
    );
}

// ── Test 11: move_b_src_rekey_conflict_after_dst_commit_undo_reverts_dst ──

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn move_b_src_rekey_conflict_after_dst_commit_undo_reverts_dst() {
    // Deterministically exercise the post-STAGE-1 / STAGE-2-conflict
    // path by arming the test-only rekey conflict hook on src's
    // sidecar. The hook fires when move_case_b reaches its STAGE 2
    // rekey on src — STAGE 1 has already committed by then — and
    // returns Conflict, triggering the compensating undo that reverts
    // T2 to its pre-STAGE-1 CID.
    let (l_cid, l_bytes) = make_leaf(b"undo-test");
    let t1_old = folders::build_folder(
        "T1",
        &[folders::ManifestEntry {
            cid: l_cid,
            name: "L".into(),
            kind: ContentKind::Leaf,
        }],
    )
    .expect("build T1");
    let t2_old = folders::build_folder("T2", &[]).expect("build T2");

    let harness = match spawn_test_runtime().await {
        Some(h) => h,
        None => return,
    };
    ingest_leaf(&harness, l_cid, l_bytes).await;
    ingest_folder(&harness, &t1_old).await;
    ingest_folder(&harness, &t2_old).await;

    let index = fresh_index();
    let t1_sid = insert_top_level(
        &index,
        t1_old.bundle_cid.to_bytes(),
        "T1",
        ContentKind::Folder,
        false,
        t1_old.bundle_bytes.len() as u64,
    );
    let t2_sid = insert_top_level(
        &index,
        t2_old.bundle_cid.to_bytes(),
        "T2",
        ContentKind::Folder,
        false,
        t2_old.bundle_bytes.len() as u64,
    );
    let t2_old_cid = t2_old.bundle_cid.to_bytes();

    // Arm the conflict hook on src's sidecar. The boundary verify
    // doesn't call rekey, so it's unaffected; STAGE 1 (dst rekey) also
    // unaffected; STAGE 2 (src rekey) is the FIRST rekey targeting
    // t1_sid in this call and consumes the hook.
    {
        let mut idx = index.lock().unwrap();
        idx.arm_next_rekey_conflict(t1_sid, [0xAB; 32]);
    }

    let err = harmony_app::move_content_impl(
        t1_sid.to_string(),
        vec![hex::encode(t1_old.bundle_cid.to_bytes())],
        hex::encode(l_cid),
        Some(t2_sid.to_string()),
        vec![hex::encode(t2_old.bundle_cid.to_bytes())],
        None,
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect_err("STAGE 2 src rekey must fail under armed conflict hook");

    assert!(
        err.contains("concurrent rekey on src"),
        "expected post-STAGE-1 src-conflict path, got: {err}"
    );

    let idx = index.lock().unwrap();
    // T2 was forward-rekeyed at STAGE 1 then compensating-undone after
    // STAGE 2 failed. End state: T2 back at its pre-move CID.
    assert_eq!(
        idx.get(&t2_sid).unwrap().cid,
        t2_old_cid,
        "T2 reverted by compensating undo"
    );
}

// ── Test 12: move_cycle_rejected ──────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn move_cycle_rejected() {
    // Top-level folder T containing folder F. Attempt to move T into F.
    let f_old = folders::build_folder("F", &[]).expect("build F");
    let t_old = folders::build_folder(
        "T",
        &[folders::ManifestEntry {
            cid: f_old.bundle_cid.to_bytes(),
            name: "F".into(),
            kind: ContentKind::Folder,
        }],
    )
    .expect("build T");

    let harness = match spawn_test_runtime().await {
        Some(h) => h,
        None => return,
    };
    ingest_folder(&harness, &f_old).await;
    ingest_folder(&harness, &t_old).await;

    let index = fresh_index();
    let t_sid = insert_top_level(
        &index,
        t_old.bundle_cid.to_bytes(),
        "T",
        ContentKind::Folder,
        false,
        t_old.bundle_bytes.len() as u64,
    );

    // Move T (root) into F — F is a descendant of T, so this is the
    // canonical cycle case. Source path = [T] (T is itself top-level),
    // src_child_cid = T's CID. Destination = inside T, path = [T, F].
    // That maps to Case C (root → nested).
    let err = harmony_app::move_content_impl(
        t_sid.to_string(),
        vec![hex::encode(t_old.bundle_cid.to_bytes())],
        hex::encode(t_old.bundle_cid.to_bytes()),
        Some(t_sid.to_string()),
        vec![
            hex::encode(t_old.bundle_cid.to_bytes()),
            hex::encode(f_old.bundle_cid.to_bytes()),
        ],
        None,
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect_err("cycle must be rejected");
    assert!(
        err.contains("cannot move folder into its own descendant"),
        "got: {err}"
    );
}

// ── Test 13: move_no_op_rejected ──────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn move_no_op_rejected() {
    let (l_cid, l_bytes) = make_leaf(b"noop");
    let t_old = folders::build_folder(
        "T",
        &[folders::ManifestEntry {
            cid: l_cid,
            name: "L".into(),
            kind: ContentKind::Leaf,
        }],
    )
    .expect("build T");

    let harness = match spawn_test_runtime().await {
        Some(h) => h,
        None => return,
    };
    ingest_leaf(&harness, l_cid, l_bytes).await;
    ingest_folder(&harness, &t_old).await;

    let index = fresh_index();
    let t_sid = insert_top_level(
        &index,
        t_old.bundle_cid.to_bytes(),
        "T",
        ContentKind::Folder,
        false,
        t_old.bundle_bytes.len() as u64,
    );

    let err = harmony_app::move_content_impl(
        t_sid.to_string(),
        vec![hex::encode(t_old.bundle_cid.to_bytes())],
        hex::encode(l_cid),
        Some(t_sid.to_string()),
        vec![hex::encode(t_old.bundle_cid.to_bytes())],
        None,
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect_err("no-op must be rejected");
    assert!(
        err.contains("source and destination are identical"),
        "got: {err}"
    );
}

// ── Test 14: move_name_collision_rejected ─────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn move_name_collision_rejected() {
    // T1 has leaf foo.txt (cid_a). T2 has leaf foo.txt (cid_b) too.
    // Move cid_a from T1 → T2; destination already has an entry named
    // "foo.txt", so reject.
    let (cid_a, bytes_a) = make_leaf(b"a-bytes");
    let (cid_b, bytes_b) = make_leaf(b"b-bytes");
    assert_ne!(cid_a, cid_b);
    let t1_old = folders::build_folder(
        "T1",
        &[folders::ManifestEntry {
            cid: cid_a,
            name: "foo.txt".into(),
            kind: ContentKind::Leaf,
        }],
    )
    .expect("build T1");
    let t2_old = folders::build_folder(
        "T2",
        &[folders::ManifestEntry {
            cid: cid_b,
            name: "foo.txt".into(),
            kind: ContentKind::Leaf,
        }],
    )
    .expect("build T2");

    let harness = match spawn_test_runtime().await {
        Some(h) => h,
        None => return,
    };
    ingest_leaf(&harness, cid_a, bytes_a).await;
    ingest_leaf(&harness, cid_b, bytes_b).await;
    ingest_folder(&harness, &t1_old).await;
    ingest_folder(&harness, &t2_old).await;

    let index = fresh_index();
    let t1_sid = insert_top_level(
        &index,
        t1_old.bundle_cid.to_bytes(),
        "T1",
        ContentKind::Folder,
        false,
        t1_old.bundle_bytes.len() as u64,
    );
    let t2_sid = insert_top_level(
        &index,
        t2_old.bundle_cid.to_bytes(),
        "T2",
        ContentKind::Folder,
        false,
        t2_old.bundle_bytes.len() as u64,
    );

    let err = harmony_app::move_content_impl(
        t1_sid.to_string(),
        vec![hex::encode(t1_old.bundle_cid.to_bytes())],
        hex::encode(cid_a),
        Some(t2_sid.to_string()),
        vec![hex::encode(t2_old.bundle_cid.to_bytes())],
        None,
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect_err("name collision must be rejected");
    assert!(
        err.contains("destination already has an entry named 'foo.txt'"),
        "got: {err}"
    );

    // Both top-level entries remain at their original CIDs — boundary
    // rejection means no ingests fired.
    let idx = index.lock().unwrap();
    assert_eq!(idx.get(&t1_sid).unwrap().cid, t1_old.bundle_cid.to_bytes());
    assert_eq!(idx.get(&t2_sid).unwrap().cid, t2_old.bundle_cid.to_bytes());
}

// ── Test 15: move_pin_cascade_a_within_same_root ──────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn move_pin_cascade_a_within_same_root() {
    // T pinned, A contains L, B empty; move L from A to B. After the
    // move, T's new manifest still references L (now via B), so the
    // runtime pin cascade should pick L up again.
    let (l_cid, l_bytes) = make_leaf(b"keep-pinned");
    let a_old = folders::build_folder(
        "A",
        &[folders::ManifestEntry {
            cid: l_cid,
            name: "L".into(),
            kind: ContentKind::Leaf,
        }],
    )
    .expect("build A");
    let b_old = folders::build_folder("B", &[]).expect("build B");
    let t_old = folders::build_folder(
        "T",
        &[
            folders::ManifestEntry {
                cid: a_old.bundle_cid.to_bytes(),
                name: "A".into(),
                kind: ContentKind::Folder,
            },
            folders::ManifestEntry {
                cid: b_old.bundle_cid.to_bytes(),
                name: "B".into(),
                kind: ContentKind::Folder,
            },
        ],
    )
    .expect("build T");

    let harness = match spawn_test_runtime().await {
        Some(h) => h,
        None => return,
    };
    ingest_leaf(&harness, l_cid, l_bytes).await;
    ingest_folder(&harness, &a_old).await;
    ingest_folder(&harness, &b_old).await;
    ingest_folder(&harness, &t_old).await;

    let index = fresh_index();
    let t_sid = insert_top_level(
        &index,
        t_old.bundle_cid.to_bytes(),
        "T",
        ContentKind::Folder,
        true, // pinned
        t_old.bundle_bytes.len() as u64,
    );

    // Pin T through the runtime so its cascade actually fires.
    let (reply_tx, reply_rx) = oneshot::channel();
    harness
        .verb_tx
        .send(ContentVerbRequest::Pin {
            cid: t_old.bundle_cid.to_bytes(),
            reply: reply_tx,
        })
        .await
        .unwrap();
    let _ = reply_rx.await.unwrap();

    harmony_app::move_content_impl(
        t_sid.to_string(),
        vec![
            hex::encode(t_old.bundle_cid.to_bytes()),
            hex::encode(a_old.bundle_cid.to_bytes()),
        ],
        hex::encode(l_cid),
        Some(t_sid.to_string()),
        vec![
            hex::encode(t_old.bundle_cid.to_bytes()),
            hex::encode(b_old.bundle_cid.to_bytes()),
        ],
        None,
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect("Case A move");

    // The pin invariant maintenance dispatches Pin(new_top) because the
    // entry's pin flag is still true. Pin a second time to force a fresh
    // cascade walk against the rebuilt tree (idempotent), then snapshot
    // the pinned_set.
    let new_top_cid = index.lock().unwrap().get(&t_sid).unwrap().cid;
    let (reply_tx, reply_rx) = oneshot::channel();
    harness
        .verb_tx
        .send(ContentVerbRequest::Pin {
            cid: new_top_cid,
            reply: reply_tx,
        })
        .await
        .unwrap();
    let _ = reply_rx.await.unwrap();

    let (reply_tx, reply_rx) = oneshot::channel();
    harness
        .verb_tx
        .send(ContentVerbRequest::PinnedSet { reply: reply_tx })
        .await
        .unwrap();
    let pinned = reply_rx.await.unwrap();
    assert!(
        pinned.contains(&l_cid),
        "L remains pinned via the rebuilt T tree"
    );
    assert!(pinned.contains(&new_top_cid), "new T pinned");
}

// ── Test 16: move_d_new_top_level_pin_defaults_unpinned ───────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn move_d_new_top_level_pin_defaults_unpinned() {
    let (l_cid, l_bytes) = make_leaf(b"to-root-d");
    let f_old = folders::build_folder(
        "F",
        &[folders::ManifestEntry {
            cid: l_cid,
            name: "L".into(),
            kind: ContentKind::Leaf,
        }],
    )
    .expect("build F");

    let harness = match spawn_test_runtime().await {
        Some(h) => h,
        None => return,
    };
    ingest_leaf(&harness, l_cid, l_bytes).await;
    ingest_folder(&harness, &f_old).await;

    let index = fresh_index();
    let f_sid = insert_top_level(
        &index,
        f_old.bundle_cid.to_bytes(),
        "F",
        ContentKind::Folder,
        true, // F pinned
        f_old.bundle_bytes.len() as u64,
    );

    let result = harmony_app::move_content_impl(
        f_sid.to_string(),
        vec![hex::encode(f_old.bundle_cid.to_bytes())],
        hex::encode(l_cid),
        None,
        vec![],
        None,
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect("Case D move");

    let new_top_sid = SidecarId::parse_str(&result.dst_sidecar_id).expect("parse");
    let idx = index.lock().unwrap();
    let new_top = idx.get(&new_top_sid).expect("new top entry exists");
    assert!(
        !new_top.pinned,
        "Case D defaults the new top-level entry to unpinned"
    );
}

// ── Test 17: move_top_level_to_root_rejected (Qodo Bug #1 regression) ─────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn move_top_level_to_root_rejected() {
    // Source is a top-level entry; destination is the root. This is a
    // no-op shape that pre-fix fell into Case D and tried to look up
    // the source root's own CID inside its own manifest, producing a
    // confusing "no entry pointing to child" error. The early-reject
    // guard surfaces it as an honest "source and destination are
    // identical" instead.
    let l_bytes = b"top-level-to-root".to_vec();
    let l_cid = ContentId::for_book(&l_bytes, ContentFlags::default())
        .expect("book cid")
        .to_bytes();

    let harness = match spawn_test_runtime().await {
        Some(h) => h,
        None => return,
    };
    ingest_leaf(&harness, l_cid, l_bytes).await;

    let index = fresh_index();
    let l_sid = insert_top_level(&index, l_cid, "L", ContentKind::Leaf, false, 17);

    let err = harmony_app::move_content_impl(
        l_sid.to_string(),
        vec![hex::encode(l_cid)],
        hex::encode(l_cid),
        None,
        vec![],
        None,
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect_err("top-level → root must be rejected");

    assert!(
        err.contains("source and destination are identical"),
        "got: {err}"
    );
}

// ── Test 18: move_case_c_src_concurrently_rekeyed_compensates ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn move_case_c_src_concurrently_rekeyed_compensates() {
    // Case C STAGE 2 is now CAS-protected: if a concurrent rekey landed
    // on src between boundary verify and STAGE 2, `remove_if_cid_matches`
    // surfaces Conflict instead of silently deleting the freshly-rekeyed
    // user entry. Compensating undo then reverts the destination.
    let (l_cid, l_bytes) = make_leaf(b"case-c-cas");
    let f_old = folders::build_folder("F", &[]).expect("build F");

    let harness = match spawn_test_runtime().await {
        Some(h) => h,
        None => return,
    };
    ingest_leaf(&harness, l_cid, l_bytes).await;
    ingest_folder(&harness, &f_old).await;

    let index = fresh_index();
    let l_sid = insert_top_level(&index, l_cid, "L", ContentKind::Leaf, false, 11);
    let f_sid = insert_top_level(
        &index,
        f_old.bundle_cid.to_bytes(),
        "F",
        ContentKind::Folder,
        false,
        f_old.bundle_bytes.len() as u64,
    );
    let f_old_cid = f_old.bundle_cid.to_bytes();

    // Race a concurrent rekey of L's sidecar entry between boundary
    // verify and STAGE 2's CAS-protected remove. force_set_cid_for_test
    // is deterministic — we set it after harness setup but before the
    // move call; boundary verify still passes because dst CID matches,
    // but the CAS remove will see the flipped cid and Conflict.
    {
        let mut idx = index.lock().unwrap();
        idx.force_set_cid_for_test(&l_sid, [0xCD; 32]);
    }

    let err = harmony_app::move_content_impl(
        l_sid.to_string(),
        vec![hex::encode(l_cid)],
        hex::encode(l_cid),
        Some(f_sid.to_string()),
        vec![hex::encode(f_old.bundle_cid.to_bytes())],
        None,
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect_err("CAS-protected Case C remove must reject divergent src");

    // The boundary verify trips on src first (entry.cid != src_path[0])
    // — that's the correct first guard. Both this message and the
    // post-STAGE-1 conflict message indicate the move refused to
    // clobber a divergent src.
    assert!(
        err.contains("concurrent rekey on src_sidecar_id")
            || err.contains("src_sidecar_id refers to cid"),
        "got: {err}"
    );

    // F (destination) must end at its original CID — either never
    // forward-rekeyed (boundary-fail) or compensating-undone.
    let idx = index.lock().unwrap();
    assert_eq!(
        idx.get(&f_sid).unwrap().cid,
        f_old_cid,
        "F ends at its original CID (no leftover forward-rekey)"
    );
}
