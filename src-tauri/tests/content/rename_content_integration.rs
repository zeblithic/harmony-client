//! ZEB-299 integration tests for `rename_content`.
//!
//! Harness boilerplate (`spawn_test_runtime`, `ingest_folder`,
//! `ingest_leaf`, `make_leaf`, `insert_top_level`, `fresh_index`) is
//! copied verbatim from `move_content_integration.rs`. ZEB-183 owns
//! the eventual extraction into a shared helper crate; keeping this
//! file standalone for now matches the existing test layout.

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
    runtime_thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for TestHarness {
    fn drop(&mut self) {
        // Signal shutdown, then join the runtime thread. Joining is
        // load-bearing here despite --test-threads=1: without it, the
        // OS-thread keeps the event loop alive past TestHarness drop,
        // and a subsequent test in the same process can race against
        // the still-draining channels. ZEB-183 will pull this Drop
        // pattern into the shared harness extraction.
        let _ = self._shutdown_tx.send(true);
        if let Some(handle) = self.runtime_thread.take() {
            // Surface a runtime-thread panic as a test failure when
            // the test is otherwise green; downgrade to eprintln if
            // Drop runs during unwinding so we don't double-panic and
            // abort the process. resume_unwind preserves the original
            // payload (and Backtrace, if any) — strictly better than
            // re-raising via panic!.
            if let Err(payload) = handle.join() {
                if std::thread::panicking() {
                    eprintln!("TestHarness runtime thread panicked: {:?}", payload);
                } else {
                    std::panic::resume_unwind(payload);
                }
            }
        }
    }
}

// All error paths in this fn panic, so the harness is never None.
// Return TestHarness directly (per CodeRabbit round 2) and let
// callers do `let harness = spawn_test_runtime().await;`.
async fn spawn_test_runtime() -> TestHarness {
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

    // ZEB-445: event_loop::run takes a mode-agnostic NodeEventSink; this
    // test never asserts on emissions, so an empty fan-out is sufficient.
    let event_sink: Arc<dyn harmony_app::node_event_sink::NodeEventSink> =
        Arc::new(harmony_app::node_event_sink::FanoutSink(vec![]));

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

    let runtime_thread = thread::Builder::new()
        .name("harmony-runtime-rename-test".to_string())
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .thread_stack_size(8 * 1024 * 1024)
                .enable_all()
                .build()
                .expect("tokio runtime for rename test event loop");
            rt.block_on(async move {
                let (runtime, startup_actions) = NodeRuntime::new(config, MemoryBookStore::new());
                harmony_app::event_loop::run(
                    runtime,
                    startup_actions,
                    event_sink,
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
                    {
                        // ZEB-352: voice-signal relay rx; tx dropped immediately
                        // so the relay arm idles (not exercised in this test).
                        let (_tx, rx) =
                            mpsc::channel::<harmony_app::voice_signal::VoiceSignalRequest>(1);
                        rx
                    },
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
                    Vec::new(),
                    {
                        let (_tx, rx) = tokio::sync::mpsc::channel::<
                            harmony_app::event_loop::CommunityAdapterRequest,
                        >(1);
                        rx
                    },
                    {
                        // ZEB-298+ZEB-312 PR 1: voting-log adapter request channel;
                        // not exercised in this test, tx dropped immediately.
                        let (_tx, rx) = tokio::sync::mpsc::channel::<
                            harmony_app::event_loop::VotingLogAdapterRequest,
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
                    None, // ZEB-341: profile_card_cache not exercised in this test
                    None, // ZEB-341: profile_card_request_rx not exercised in this test
                    None, // ZEB-537: community_presence_request_rx not exercised in this test
                    std::sync::Arc::new(tokio::sync::Mutex::new(
                        harmony_app::community_presence::CommunityPresenceMap::new(),
                    )), // ZEB-537: community_presence_map (throwaway; presence not exercised here)
                    None, // Mint Phase 2 sync: not exercised in this test
                    None, // ZEB-417 SP1: notes_sync_handles not exercised in this test
                    None, // ZEB-418 P1: dm_inbox_sync_handles not exercised in this test
                    None, // ZEB-418 P2: p2_sync_handles not exercised in this test
                    None, // ZEB-458 P4 B: relay_sync_handles not exercised in this test
                    None, // ZEB-668 S1: trust_sync_handles not exercised in this test
                    None, // ZEB-677 S3: quorum_sync_handles not exercised in this test
                    None, // ZEB-668 S5: fleet_keys_sync_handles not exercised in this test
                    None, // ZEB-495: community_device_intro_sync_handles not exercised in this test
                    None, // ZEB-321 Phase 1 Task 8: iroh handles not exercised in this test
                    None, // ZEB-373: dial telemetry not exercised in this test
                    harmony_app::content_store::CommunityServeAllowlist::new(), // ZEB-395: empty allowlist (no community roots published in this test)
                    None, // ZEB-418 P2: routing_republish not exercised
                    tokio::sync::watch::channel(0u64).0, // ZEB-434: transport-epoch watch not exercised
                    Vec::new(), // ZEB-702 T3: republish_on_epoch — no engines exercised
                    tokio::sync::watch::channel(0u64).0, // ZEB-599: presence-resync watch not exercised
                    None, // ZEB-618: mail-root persist pair not exercised
                    None, // ZEB-621: addr_change_fanout not exercised
                    // ZEB-612 S3: announcements not exercised in this test
                    std::sync::Arc::new(std::sync::Mutex::new(
                        harmony_app::observed_holders::ObservedHolders::new(),
                    )),
                    // ZEB-612 S3: re-announce not exercised (empty index)
                    std::sync::Arc::new(std::sync::Mutex::new(
                        harmony_app::content_index::ContentIndex::load(std::path::Path::new("")),
                    )),
                    // ZEB-669 S2: buddy records/ledger/settings not exercised
                    std::sync::Arc::new(std::sync::Mutex::new(
                        harmony_app::storage_records::StorageRecordStore::new(None),
                    )),
                    std::sync::Arc::new(std::sync::Mutex::new(
                        harmony_app::storage_ledger::StorageLedger::new(None),
                    )),
                    std::sync::Arc::new(std::sync::Mutex::new(
                        harmony_app::storage_settings::StorageSettings::default(),
                    )),
                    String::new(), // ZEB-669 S2: no owner ⇒ engine tick no-ops
                )
                .await;
            });
        })
        .expect("spawn runtime thread");

    match ready_rx.await {
        Ok(Ok(())) => {}
        // ZEB-446 made the Reticulum bind degradable (a 4242 collision warns and
        // falls back to an ephemeral loopback bind), so `run()` no longer returns
        // an "Address already in use" error — the old special-case arm is
        // retired. Any real start failure still panics loudly here rather than
        // skipping (the ZEB-165 / ZEB-420 anti-false-green invariant).
        Ok(Err(e)) => panic!("event loop failed to start: {e}"),
        Err(_) => panic!("event loop dropped ready signal"),
    }

    TestHarness {
        ingest_tx,
        verb_tx,
        _shutdown_tx: shutdown_tx,
        _tmp: tmp,
        runtime_thread: Some(runtime_thread),
    }
}

/// Ingest a built folder's manifest + bundle through the runtime.
async fn ingest_folder(harness: &TestHarness, built: &folders::BuiltFolder) {
    harmony_app::send_ingest(
        &harness.ingest_tx,
        hex::encode(built.manifest_cid.to_bytes()),
        built.manifest_bytes.clone(),
        false,
    )
    .await
    .unwrap();
    harmony_app::send_ingest(
        &harness.ingest_tx,
        hex::encode(built.bundle_cid.to_bytes()),
        built.bundle_bytes.clone(),
        false,
    )
    .await
    .unwrap();
}

async fn ingest_leaf(harness: &TestHarness, cid: [u8; 32], bytes: Vec<u8>) {
    harmony_app::send_ingest(&harness.ingest_tx, hex::encode(cid), bytes, false)
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
        backup: false,
        origin: None,
        kind,
    });
    assert!(inserted, "fresh SidecarId must insert cleanly");
    sid
}

fn fresh_index() -> (Arc<Mutex<ContentIndex>>, tempfile::TempDir) {
    // Each test owns its own tempdir-backed sidecar. The TempDir is
    // returned alongside the index so callers can keep it alive for
    // the duration of the test — without that, ContentIndex::save
    // writes vanish into a deleted directory and on-disk persistence
    // regressions slip past tests that only assert in-memory state.
    let dir = tempdir().unwrap();
    let idx = ContentIndex::load(dir.path());
    (Arc::new(Mutex::new(idx)), dir)
}

// ── Test 1: rename_top_level_file ─────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rename_top_level_file() {
    // Top-level leaf row. Renaming via IPC must change `file_name` and
    // leave `cid` untouched — bundle bytes are not rebuilt.
    let (l_cid, l_bytes) = make_leaf(b"hello world");

    let harness = spawn_test_runtime().await;
    ingest_leaf(&harness, l_cid, l_bytes.clone()).await;

    let (index, _index_dir) = fresh_index();
    let l_sid = insert_top_level(
        &index,
        l_cid,
        "hello.txt",
        ContentKind::Leaf,
        false,
        l_bytes.len() as u64,
    );

    let result = harmony_app::rename_content_impl(
        l_sid.to_string(),
        vec![hex::encode(l_cid)],
        hex::encode(l_cid),
        "hello.txt".to_string(),
        "world.txt".to_string(),
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect("rename top-level file");

    assert_eq!(result.src_new_cid, None, "top-level rename never rekeys");

    let idx = index.lock().unwrap();
    let entry = idx.get(&l_sid).expect("L entry still present");
    assert_eq!(entry.file_name, "world.txt", "file_name updated");
    assert_eq!(entry.cid, l_cid, "cid unchanged");
}

// ── Test 2: rename_top_level_folder ───────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rename_top_level_folder() {
    // Top-level folder row with children — verify children unaffected,
    // i.e., the folder's own bundle CID is stable.
    let (l_cid, l_bytes) = make_leaf(b"inner");
    let f_old = folders::build_folder(
        "F",
        &[folders::ManifestEntry {
            cid: l_cid,
            name: "L".into(),
            kind: ContentKind::Leaf,
        }],
    )
    .expect("build F");

    let harness = spawn_test_runtime().await;
    ingest_leaf(&harness, l_cid, l_bytes).await;
    ingest_folder(&harness, &f_old).await;

    let (index, _index_dir) = fresh_index();
    let f_sid = insert_top_level(
        &index,
        f_old.bundle_cid.to_bytes(),
        "Folder",
        ContentKind::Folder,
        false,
        f_old.bundle_bytes.len() as u64,
    );

    let result = harmony_app::rename_content_impl(
        f_sid.to_string(),
        vec![hex::encode(f_old.bundle_cid.to_bytes())],
        hex::encode(f_old.bundle_cid.to_bytes()),
        "Folder".to_string(),
        "Renamed Folder".to_string(),
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect("rename top-level folder");

    assert_eq!(result.src_new_cid, None);

    let idx = index.lock().unwrap();
    let entry = idx.get(&f_sid).expect("F entry still present");
    assert_eq!(entry.file_name, "Renamed Folder");
    assert_eq!(
        entry.cid,
        f_old.bundle_cid.to_bytes(),
        "folder bundle CID stable — children unaffected",
    );
}

// ── Test 3: rename_nested_one_level_deep ──────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rename_nested_one_level_deep() {
    // T contains F. Rename F → "bar". T rekeys to the manifest-with-
    // renamed-entry CID; F's own bundle CID is unchanged.
    let f_old = folders::build_folder("foo", &[]).expect("build F");
    let t_old = folders::build_folder(
        "T",
        &[folders::ManifestEntry {
            cid: f_old.bundle_cid.to_bytes(),
            name: "foo".into(),
            kind: ContentKind::Folder,
        }],
    )
    .expect("build T");

    let harness = spawn_test_runtime().await;
    ingest_folder(&harness, &f_old).await;
    ingest_folder(&harness, &t_old).await;

    let (index, _index_dir) = fresh_index();
    let t_sid = insert_top_level(
        &index,
        t_old.bundle_cid.to_bytes(),
        "T",
        ContentKind::Folder,
        false,
        t_old.bundle_bytes.len() as u64,
    );

    let result = harmony_app::rename_content_impl(
        t_sid.to_string(),
        vec![hex::encode(t_old.bundle_cid.to_bytes())],
        hex::encode(f_old.bundle_cid.to_bytes()),
        "foo".to_string(),
        "bar".to_string(),
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect("rename nested one level");

    let t_new = folders::build_folder(
        "T",
        &[folders::ManifestEntry {
            cid: f_old.bundle_cid.to_bytes(),
            name: "bar".into(),
            kind: ContentKind::Folder,
        }],
    )
    .expect("build T_new");

    assert_eq!(
        result.src_new_cid,
        Some(hex::encode(t_new.bundle_cid.to_bytes())),
    );

    let idx = index.lock().unwrap();
    let entry = idx.get(&t_sid).expect("T entry still present");
    assert_eq!(
        entry.cid,
        t_new.bundle_cid.to_bytes(),
        "T rekeyed to renamed-entry manifest CID",
    );
}

// ── Test 4: rename_nested_two_levels_deep ─────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rename_nested_two_levels_deep() {
    // T → A → L. Rename L. Whole chain rekeys.
    let (l_cid, l_bytes) = make_leaf(b"two-deep");
    let a_old = folders::build_folder(
        "A",
        &[folders::ManifestEntry {
            cid: l_cid,
            name: "L".into(),
            kind: ContentKind::Leaf,
        }],
    )
    .expect("build A");
    let t_old = folders::build_folder(
        "T",
        &[folders::ManifestEntry {
            cid: a_old.bundle_cid.to_bytes(),
            name: "A".into(),
            kind: ContentKind::Folder,
        }],
    )
    .expect("build T");

    let harness = spawn_test_runtime().await;
    ingest_leaf(&harness, l_cid, l_bytes).await;
    ingest_folder(&harness, &a_old).await;
    ingest_folder(&harness, &t_old).await;

    let (index, _index_dir) = fresh_index();
    let t_sid = insert_top_level(
        &index,
        t_old.bundle_cid.to_bytes(),
        "T",
        ContentKind::Folder,
        false,
        t_old.bundle_bytes.len() as u64,
    );

    let result = harmony_app::rename_content_impl(
        t_sid.to_string(),
        vec![
            hex::encode(t_old.bundle_cid.to_bytes()),
            hex::encode(a_old.bundle_cid.to_bytes()),
        ],
        hex::encode(l_cid),
        "L".to_string(),
        "Renamed".to_string(),
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect("rename nested two-levels");

    let a_new = folders::build_folder(
        "A",
        &[folders::ManifestEntry {
            cid: l_cid,
            name: "Renamed".into(),
            kind: ContentKind::Leaf,
        }],
    )
    .expect("build A_new");
    let t_new = folders::build_folder(
        "T",
        &[folders::ManifestEntry {
            cid: a_new.bundle_cid.to_bytes(),
            name: "A".into(),
            kind: ContentKind::Folder,
        }],
    )
    .expect("build T_new");

    assert_eq!(
        result.src_new_cid,
        Some(hex::encode(t_new.bundle_cid.to_bytes())),
    );

    let idx = index.lock().unwrap();
    let entry = idx.get(&t_sid).expect("T entry still present");
    assert_eq!(entry.cid, t_new.bundle_cid.to_bytes(), "T rekeyed");
}

// ── Test 5: rename_disambiguates_siblings_with_shared_cid ─────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rename_disambiguates_siblings_with_shared_cid() {
    // T contains two empty folders EmptyA and EmptyB — same CID,
    // different names. Rename EmptyB → "Renamed". The (name, cid) match
    // must pick the right sibling.
    let empty = folders::build_folder("", &[]).expect("build empty");
    let empty_cid = empty.bundle_cid.to_bytes();
    let t_old = folders::build_folder(
        "T",
        &[
            folders::ManifestEntry {
                cid: empty_cid,
                name: "EmptyA".into(),
                kind: ContentKind::Folder,
            },
            folders::ManifestEntry {
                cid: empty_cid,
                name: "EmptyB".into(),
                kind: ContentKind::Folder,
            },
        ],
    )
    .expect("build T");

    let harness = spawn_test_runtime().await;
    ingest_folder(&harness, &empty).await;
    ingest_folder(&harness, &t_old).await;

    let (index, _index_dir) = fresh_index();
    let t_sid = insert_top_level(
        &index,
        t_old.bundle_cid.to_bytes(),
        "T",
        ContentKind::Folder,
        false,
        t_old.bundle_bytes.len() as u64,
    );

    let result = harmony_app::rename_content_impl(
        t_sid.to_string(),
        vec![hex::encode(t_old.bundle_cid.to_bytes())],
        hex::encode(empty_cid),
        "EmptyB".to_string(),
        "Renamed".to_string(),
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect("rename shared-cid sibling");

    let t_new = folders::build_folder(
        "T",
        &[
            folders::ManifestEntry {
                cid: empty_cid,
                name: "EmptyA".into(),
                kind: ContentKind::Folder,
            },
            folders::ManifestEntry {
                cid: empty_cid,
                name: "Renamed".into(),
                kind: ContentKind::Folder,
            },
        ],
    )
    .expect("build T_new");

    assert_eq!(
        result.src_new_cid,
        Some(hex::encode(t_new.bundle_cid.to_bytes())),
        "rebuilt T must keep EmptyA and rename EmptyB → Renamed",
    );
}

// ── Test 6: rename_empty_name_rejected ────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rename_empty_name_rejected() {
    // Both "" and "   " (whitespace-only after trim) must reject.
    let (l_cid, l_bytes) = make_leaf(b"empty-reject");

    let harness = spawn_test_runtime().await;
    ingest_leaf(&harness, l_cid, l_bytes.clone()).await;

    let (index, _index_dir) = fresh_index();
    let l_sid = insert_top_level(
        &index,
        l_cid,
        "original.txt",
        ContentKind::Leaf,
        false,
        l_bytes.len() as u64,
    );

    for empty_name in ["", "   "] {
        let err = harmony_app::rename_content_impl(
            l_sid.to_string(),
            vec![hex::encode(l_cid)],
            hex::encode(l_cid),
            "original.txt".to_string(),
            empty_name.to_string(),
            harness.ingest_tx.clone(),
            harness.verb_tx.clone(),
            index.clone(),
        )
        .await
        .expect_err("empty/whitespace name must reject");
        assert!(
            err.contains("name cannot be empty"),
            "got: {err} for input {empty_name:?}",
        );
    }

    // Sidecar unchanged.
    let idx = index.lock().unwrap();
    let entry = idx.get(&l_sid).expect("L entry still present");
    assert_eq!(entry.file_name, "original.txt");
}

// ── Test 7: rename_same_name_nested_no_op ─────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rename_same_name_nested_no_op() {
    // new_name == src_child_name in the nested case — returns the
    // current top-level CID, sidecar untouched, no new bytes ingested.
    let f_old = folders::build_folder("foo", &[]).expect("build F");
    let t_old = folders::build_folder(
        "T",
        &[folders::ManifestEntry {
            cid: f_old.bundle_cid.to_bytes(),
            name: "foo".into(),
            kind: ContentKind::Folder,
        }],
    )
    .expect("build T");

    let harness = spawn_test_runtime().await;
    ingest_folder(&harness, &f_old).await;
    ingest_folder(&harness, &t_old).await;

    let (index, _index_dir) = fresh_index();
    let t_sid = insert_top_level(
        &index,
        t_old.bundle_cid.to_bytes(),
        "T",
        ContentKind::Folder,
        false,
        t_old.bundle_bytes.len() as u64,
    );
    let t_old_cid = t_old.bundle_cid.to_bytes();

    let result = harmony_app::rename_content_impl(
        t_sid.to_string(),
        vec![hex::encode(t_old_cid)],
        hex::encode(f_old.bundle_cid.to_bytes()),
        "foo".to_string(),
        "foo".to_string(),
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect("same-name nested no-op");

    assert_eq!(
        result.src_new_cid,
        Some(hex::encode(t_old_cid)),
        "same-name nested returns current top-level CID",
    );
    let idx = index.lock().unwrap();
    let entry = idx.get(&t_sid).expect("T entry still present");
    assert_eq!(entry.cid, t_old_cid, "T sidecar CID unchanged");
}

// ── Test 8: rename_same_name_top_level_no_op ──────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rename_same_name_top_level_no_op() {
    let (l_cid, l_bytes) = make_leaf(b"same-name-top");

    let harness = spawn_test_runtime().await;
    ingest_leaf(&harness, l_cid, l_bytes.clone()).await;

    let (index, _index_dir) = fresh_index();
    let l_sid = insert_top_level(
        &index,
        l_cid,
        "same.txt",
        ContentKind::Leaf,
        false,
        l_bytes.len() as u64,
    );

    let result = harmony_app::rename_content_impl(
        l_sid.to_string(),
        vec![hex::encode(l_cid)],
        hex::encode(l_cid),
        "same.txt".to_string(),
        "same.txt".to_string(),
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect("same-name top-level no-op");

    assert_eq!(result.src_new_cid, None);
    let idx = index.lock().unwrap();
    let entry = idx.get(&l_sid).expect("L entry still present");
    assert_eq!(entry.file_name, "same.txt");
    assert_eq!(entry.cid, l_cid);
}

// ── Test 9: rename_duplicate_sibling_rejected_nested ──────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rename_duplicate_sibling_rejected_nested() {
    // T contains A and B. Rename A → "B" — rejects with name-collision.
    let a_old = folders::build_folder("A", &[]).expect("build A");
    let b_old = folders::build_folder("B_inner", &[]).expect("build B");
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

    let harness = spawn_test_runtime().await;
    ingest_folder(&harness, &a_old).await;
    ingest_folder(&harness, &b_old).await;
    ingest_folder(&harness, &t_old).await;

    let (index, _index_dir) = fresh_index();
    let t_sid = insert_top_level(
        &index,
        t_old.bundle_cid.to_bytes(),
        "T",
        ContentKind::Folder,
        false,
        t_old.bundle_bytes.len() as u64,
    );
    let t_old_cid = t_old.bundle_cid.to_bytes();

    let err = harmony_app::rename_content_impl(
        t_sid.to_string(),
        vec![hex::encode(t_old_cid)],
        hex::encode(a_old.bundle_cid.to_bytes()),
        "A".to_string(),
        "B".to_string(),
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect_err("duplicate sibling must reject");
    assert!(
        err.contains("parent folder already has an entry named 'B'"),
        "got: {err}",
    );

    let idx = index.lock().unwrap();
    let entry = idx.get(&t_sid).expect("T entry still present");
    assert_eq!(entry.cid, t_old_cid, "T unchanged after rejected rename");
}

// ── Test 10: rename_duplicate_sibling_rejected_top_level ──────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rename_duplicate_sibling_rejected_top_level() {
    let (a_cid, a_bytes) = make_leaf(b"alpha");
    let (b_cid, b_bytes) = make_leaf(b"beta");

    let harness = spawn_test_runtime().await;
    ingest_leaf(&harness, a_cid, a_bytes.clone()).await;
    ingest_leaf(&harness, b_cid, b_bytes.clone()).await;

    let (index, _index_dir) = fresh_index();
    let a_sid = insert_top_level(
        &index,
        a_cid,
        "A",
        ContentKind::Leaf,
        false,
        a_bytes.len() as u64,
    );
    let _b_sid = insert_top_level(
        &index,
        b_cid,
        "B",
        ContentKind::Leaf,
        false,
        b_bytes.len() as u64,
    );

    let err = harmony_app::rename_content_impl(
        a_sid.to_string(),
        vec![hex::encode(a_cid)],
        hex::encode(a_cid),
        "A".to_string(),
        "B".to_string(),
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect_err("duplicate top-level sibling must reject");
    assert!(
        err.contains("a top-level entry named 'B' already exists"),
        "got: {err}",
    );

    let idx = index.lock().unwrap();
    let entry = idx.get(&a_sid).expect("A entry still present");
    assert_eq!(entry.file_name, "A", "A name unchanged after rejection");
}

// ── Test 11: rename_name_mismatch_rejected ────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rename_name_mismatch_rejected() {
    // Top-level: src_child_name does not match the sidecar entry name.
    let (l_cid, l_bytes) = make_leaf(b"mismatch-top");

    let harness = spawn_test_runtime().await;
    ingest_leaf(&harness, l_cid, l_bytes.clone()).await;

    let (index, _index_dir) = fresh_index();
    let l_sid = insert_top_level(
        &index,
        l_cid,
        "actual.txt",
        ContentKind::Leaf,
        false,
        l_bytes.len() as u64,
    );

    let err = harmony_app::rename_content_impl(
        l_sid.to_string(),
        vec![hex::encode(l_cid)],
        hex::encode(l_cid),
        "wrong.txt".to_string(),
        "renamed.txt".to_string(),
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect_err("top-level name mismatch must reject");
    assert!(
        err.contains(
            "src_child_name 'wrong.txt' does not match src sidecar entry name 'actual.txt'"
        ),
        "got: {err}",
    );

    // Nested: src_child_name does not match the manifest entry name.
    let f_old = folders::build_folder("actual", &[]).expect("build F");
    let t_old = folders::build_folder(
        "T",
        &[folders::ManifestEntry {
            cid: f_old.bundle_cid.to_bytes(),
            name: "actual".into(),
            kind: ContentKind::Folder,
        }],
    )
    .expect("build T");
    ingest_folder(&harness, &f_old).await;
    ingest_folder(&harness, &t_old).await;

    let t_sid = insert_top_level(
        &index,
        t_old.bundle_cid.to_bytes(),
        "T",
        ContentKind::Folder,
        false,
        t_old.bundle_bytes.len() as u64,
    );

    let err = harmony_app::rename_content_impl(
        t_sid.to_string(),
        vec![hex::encode(t_old.bundle_cid.to_bytes())],
        hex::encode(f_old.bundle_cid.to_bytes()),
        "wrong".to_string(),
        "renamed".to_string(),
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect_err("nested name mismatch must reject");
    assert!(
        err.contains("has no entry named 'wrong' pointing to child"),
        "got: {err}",
    );
}

// ── Test 12: rename_concurrent_rekey_conflict ─────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rename_concurrent_rekey_conflict() {
    // Arm the rekey-conflict hook on the cascade root. IPC must surface
    // "concurrent rekey on src_sidecar_id" and the sidecar CID must
    // stay unchanged.
    let f_old = folders::build_folder("foo", &[]).expect("build F");
    let t_old = folders::build_folder(
        "T",
        &[folders::ManifestEntry {
            cid: f_old.bundle_cid.to_bytes(),
            name: "foo".into(),
            kind: ContentKind::Folder,
        }],
    )
    .expect("build T");

    let harness = spawn_test_runtime().await;
    ingest_folder(&harness, &f_old).await;
    ingest_folder(&harness, &t_old).await;

    let (index, _index_dir) = fresh_index();
    let t_sid = insert_top_level(
        &index,
        t_old.bundle_cid.to_bytes(),
        "T",
        ContentKind::Folder,
        false,
        t_old.bundle_bytes.len() as u64,
    );
    let t_old_cid = t_old.bundle_cid.to_bytes();

    {
        let mut idx = index.lock().unwrap();
        idx.arm_next_rekey_conflict(t_sid, [0xAB; 32]);
    }

    let err = harmony_app::rename_content_impl(
        t_sid.to_string(),
        vec![hex::encode(t_old_cid)],
        hex::encode(f_old.bundle_cid.to_bytes()),
        "foo".to_string(),
        "bar".to_string(),
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect_err("armed conflict hook must surface as Err");
    assert!(
        err.contains("concurrent rekey on src_sidecar_id"),
        "got: {err}",
    );

    let idx = index.lock().unwrap();
    let entry = idx.get(&t_sid).expect("T entry still present");
    assert_eq!(
        entry.cid, t_old_cid,
        "T sidecar CID unchanged after rejected rename",
    );
}

// ── Test 13: rename_nested_preserves_pinned_status ───────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rename_nested_preserves_pinned_status() {
    // Top-level T (pinned) contains F. Renaming F rekeys T's sidecar.
    // The rekey path must preserve the `pinned` flag — losing it would
    // silently demote pinned content to evictable on every rename.
    let f_old = folders::build_folder("foo", &[]).expect("build F");
    let t_old = folders::build_folder(
        "T",
        &[folders::ManifestEntry {
            cid: f_old.bundle_cid.to_bytes(),
            name: "foo".into(),
            kind: ContentKind::Folder,
        }],
    )
    .expect("build T");

    let harness = spawn_test_runtime().await;
    ingest_folder(&harness, &f_old).await;
    ingest_folder(&harness, &t_old).await;

    let (index, _index_dir) = fresh_index();
    let t_sid = insert_top_level(
        &index,
        t_old.bundle_cid.to_bytes(),
        "T",
        ContentKind::Folder,
        true, // pinned
        t_old.bundle_bytes.len() as u64,
    );

    harmony_app::rename_content_impl(
        t_sid.to_string(),
        vec![hex::encode(t_old.bundle_cid.to_bytes())],
        hex::encode(f_old.bundle_cid.to_bytes()),
        "foo".to_string(),
        "bar".to_string(),
        harness.ingest_tx.clone(),
        harness.verb_tx.clone(),
        index.clone(),
    )
    .await
    .expect("rename nested preserves pinned");

    let idx = index.lock().unwrap();
    let entry = idx.get(&t_sid).expect("T entry still present after rekey");
    assert!(
        entry.pinned,
        "pinned flag must survive the rekey — sidecar rekey is not a re-insert",
    );
}
