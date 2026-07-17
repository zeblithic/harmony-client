//! ZEB-158 slice 1: end-to-end tests for folder create/list.
//!
//! Data-layer tests (Tasks 5–6) drive build_folder + sidecar directly.
//! Full event-loop harness tests (Task 7) validate pin cascade + list_folder.
//! Pattern follows content_index_integration.rs's style; integration
//! tests only reach `pub` symbols.

use tempfile::tempdir;

use harmony_app::content_index::{
    ContentIndex, ContentIndexEntry, ContentKind, ReplicationTier, Sensitivity, SidecarId,
};
use harmony_app::folders;

#[test]
fn create_folder_at_root_then_list_shows_it() {
    let dir = tempdir().unwrap();
    let mut idx = ContentIndex::load(dir.path());

    // Build what create_folder_at_root would build: an empty folder.
    let built = folders::build_folder("Photos", &[]).expect("build");

    // Insert the sidecar entry that create_folder_at_root would insert.
    let inserted = idx.insert(ContentIndexEntry {
        sidecar_id: SidecarId::new(),
        cid: built.bundle_cid.to_bytes(),
        file_name: "Photos".into(),
        size_bytes: built.bundle_bytes.len() as u64,
        stored_at_ms: 1,
        sensitivity: Sensitivity::Private,
        replication_tier: ReplicationTier::Default,
        licensed: false,
        archived: false,
        pinned: false,
        backup: false,
        origin: None,
        kind: ContentKind::Folder,
    });
    assert!(inserted, "new entry inserted");

    // Inspect the sidecar — one row with kind Folder, name Photos.
    let rows: Vec<&ContentIndexEntry> = idx.entries().collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, ContentKind::Folder);
    assert_eq!(rows[0].file_name, "Photos");
    assert!(rows[0].size_bytes > 0);
    assert!(!rows[0].pinned);

    // Bundle bytes of an empty folder are the 32-byte manifest CID.
    assert_eq!(built.bundle_bytes.len(), 32);
}

#[test]
fn create_nested_folder_updates_top_level_root_cid() {
    // Build root "Photos" folder at depth 0. Sidecar entry is keyed by
    // a fresh SidecarId; its `cid` field holds Photos's bundle CID
    // (call that root_v1).
    let dir = tempdir().unwrap();
    let mut idx = ContentIndex::load(dir.path());
    let photos_v1 = folders::build_folder("Photos", &[]).expect("build v1");

    let photos_sid = SidecarId::new();
    idx.insert(ContentIndexEntry {
        sidecar_id: photos_sid,
        cid: photos_v1.bundle_cid.to_bytes(),
        file_name: "Photos".into(),
        size_bytes: photos_v1.bundle_bytes.len() as u64,
        stored_at_ms: 1,
        sensitivity: Sensitivity::Private,
        replication_tier: ReplicationTier::Default,
        licensed: false,
        archived: false,
        pinned: true, // must survive rekey
        backup: false,
        origin: None,
        kind: ContentKind::Folder,
    });

    // Now simulate creating "2026" inside Photos. This should produce:
    //   - A new empty "2026" folder (child of the new Photos bundle).
    //   - A new Photos bundle (root_v2) whose manifest lists "2026".
    //   - Sidecar rekey: same SidecarId, but the entry's `cid` field
    //     flips from photos_v1 to photos_v2. (ZEB-164's symlink-style
    //     model keys entries by SidecarId, not CID — the map key is
    //     stable across rekey.)
    //   - Sidecar's pinned flag still true after rekey.
    let sub = folders::build_folder("2026", &[]).expect("sub build");
    let photos_v2 = folders::build_folder(
        "Photos",
        &[folders::ManifestEntry {
            cid: sub.bundle_cid.to_bytes(),
            name: "2026".into(),
            kind: ContentKind::Folder,
        }],
    )
    .expect("v2 build");

    let rekeyed = idx.rekey(
        &photos_sid,
        photos_v1.bundle_cid.to_bytes(),
        photos_v2.bundle_cid.to_bytes(),
        photos_v2.bundle_bytes.len() as u64,
        /* new_stored_at_ms */ 2,
    );
    assert!(rekeyed.is_ok(), "rekey succeeds");

    // Under the symlink-style model (ZEB-164), rekey mutates the entry's
    // CID in place rather than re-keying the map: same SidecarId, new CID.
    let after = idx.get(&photos_sid).expect("rekeyed entry present");
    assert_eq!(after.cid, photos_v2.bundle_cid.to_bytes(), "cid is now v2");
    assert!(after.pinned, "pinned survives rekey");
    assert_eq!(after.kind, ContentKind::Folder);
    assert_eq!(after.file_name, "Photos");
    assert!(
        idx.entries_for_cid(&photos_v1.bundle_cid.to_bytes())
            .next()
            .is_none(),
        "no sidecar entry references the old CID"
    );
}

// ── Task 7: Event-loop harness tests ────────────────────────────────────────

use std::sync::{Arc, Mutex};
use std::thread;

use harmony_app::event_loop::{ContentVerbRequest, IngestRequest};
use harmony_compute::InstructionBudget;
use harmony_content::book::MemoryBookStore;
use harmony_content::storage_tier::{ContentPolicy, FilterBroadcastConfig, StorageBudget};
use harmony_runtime::{NodeConfig, NodeRuntime};
use tokio::sync::{mpsc, oneshot, watch};

/// All channel ends the outer test needs to drive the event loop.
struct TestHarness {
    pub ingest_tx: mpsc::Sender<IngestRequest>,
    pub verb_tx: mpsc::Sender<ContentVerbRequest>,
    /// Kept alive so the event loop keeps running; dropped to shut down.
    _shutdown_tx: watch::Sender<bool>,
    /// Tempdir kept alive for the runtime's lifetime; cleaned up on drop.
    _tmp: tempfile::TempDir,
}

impl Drop for TestHarness {
    fn drop(&mut self) {
        // Ignore send error — event loop may have already exited.
        let _ = self._shutdown_tx.send(true);
    }
}

/// Spawn a fresh NodeRuntime on its own OS thread with its own tokio runtime,
/// exactly matching the content_index_integration.rs harness pattern. Returns
/// the harness once the event loop signals ready; panics on any start failure.
/// (ZEB-446 made the Reticulum bind degradable, so a 4242 collision no longer
/// fails startup — see the `ready_rx` match below.)
async fn spawn_test_runtime() -> TestHarness {
    let tmp = tempfile::tempdir().unwrap();
    let app_data_dir = tmp.path().to_path_buf();

    let (ingest_tx, ingest_rx) = mpsc::channel::<IngestRequest>(4);
    let (verb_tx, content_verb_rx) = mpsc::channel::<ContentVerbRequest>(16);
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

    thread::Builder::new()
        .name("harmony-runtime-folder-test".to_string())
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .thread_stack_size(8 * 1024 * 1024)
                .enable_all()
                .build()
                .expect("tokio runtime for folder test event loop");
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
                    None,       // pairing_in_tx — pairing not exercised in this test
                    None,       // sync_handles — SyncEngine not exercised in this test
                    None,       // dm_outbox — DM outbox not exercised in this test
                    None,       // dm_transport — DM outbox not exercised in this test
                    None,       // crdt_state — DM outbox not exercised in this test
                    Vec::new(), // community_adapters — Phase 2 community sync not exercised in this test
                    {
                        // Phase 3 Task 9: on-demand adapter request channel; not
                        // exercised in this folder-primitive test, so the rx is
                        // simply held and the matching tx is dropped immediately
                        // (next .recv() yields None, the select arm idles).
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
                    None, // ZEB-262 Phase 4 Task 9: community_registry not exercised in this test
                    {
                        // ZEB-270 Phase 3 Task 4.5: channel-log adapter
                        // request rx; tx dropped immediately so the
                        // select arm idles.
                        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<
                            harmony_app::event_loop::ChannelLogAdapterRequest,
                        >();
                        rx
                    },
                    None, // ZEB-218 Sub-D Phase 1: library_directory not exercised in this test
                    None, // ZEB-218 Sub-D Phase 1: library_request_rx not exercised in this test
                    None, // ZEB-281 Sub-D Phase 4: profile_broadcast_cache not exercised in this test
                    None, // ZEB-281 Sub-D Phase 4: profile_broadcast_request_rx not exercised in this test
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
        Ok(Ok(())) => {} // proceed
        // ZEB-446 made the Reticulum bind degradable (a 4242 collision warns and
        // falls back to an ephemeral loopback bind), so `run()` no longer returns
        // an "Address already in use" error here. A real start failure now fails
        // loudly instead of being silently skipped (ZEB-420).
        Ok(Err(e)) => panic!("event loop failed to start: {e}"),
        Err(_) => panic!("event loop dropped ready signal"),
    }

    // Move tempdir into the harness so it's cleaned up on Drop instead
    // of leaking via std::mem::forget (caught by PR #55 review).
    TestHarness {
        ingest_tx,
        verb_tx,
        _shutdown_tx: shutdown_tx,
        _tmp: tmp,
    }
}

// ── Test 1: pin_folder_cascades_to_nested_leaf ────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pin_folder_cascades_to_nested_leaf() {
    use harmony_content::cid::{ContentFlags, ContentId};

    let leaf_bytes = b"hello world".to_vec();
    let leaf_cid = ContentId::for_book(&leaf_bytes, ContentFlags::default()).unwrap();
    let folder = folders::build_folder(
        "FolderWithLeaf",
        &[folders::ManifestEntry {
            cid: leaf_cid.to_bytes(),
            name: "hello.txt".into(),
            kind: ContentKind::Leaf,
        }],
    )
    .expect("build folder");

    let harness = spawn_test_runtime().await;

    harmony_app::send_ingest(
        &harness.ingest_tx,
        hex::encode(leaf_cid.to_bytes()),
        leaf_bytes,
        false,
    )
    .await
    .unwrap();
    harmony_app::send_ingest(
        &harness.ingest_tx,
        hex::encode(folder.manifest_cid.to_bytes()),
        folder.manifest_bytes,
        false,
    )
    .await
    .unwrap();
    harmony_app::send_ingest(
        &harness.ingest_tx,
        hex::encode(folder.bundle_cid.to_bytes()),
        folder.bundle_bytes,
        false,
    )
    .await
    .unwrap();

    // Pin the folder root.
    let (reply_tx, reply_rx) = oneshot::channel();
    harness
        .verb_tx
        .send(ContentVerbRequest::Pin {
            cid: folder.bundle_cid.to_bytes(),
            reply: reply_tx,
        })
        .await
        .unwrap();
    assert!(reply_rx.await.unwrap().unwrap());

    // Inspect the pinned set — cascade should include all three CIDs.
    let (reply_tx, reply_rx) = oneshot::channel();
    harness
        .verb_tx
        .send(ContentVerbRequest::PinnedSet { reply: reply_tx })
        .await
        .unwrap();
    let pinned = reply_rx.await.unwrap();

    assert!(
        pinned.contains(&folder.bundle_cid.to_bytes()),
        "folder pinned"
    );
    assert!(
        pinned.contains(&folder.manifest_cid.to_bytes()),
        "manifest pinned via cascade"
    );
    assert!(
        pinned.contains(&leaf_cid.to_bytes()),
        "leaf pinned via cascade"
    );
}

// ── Test 2: pin_intent_survives_restart_for_folder ────────────────────────

#[test]
fn pin_intent_survives_restart_for_folder() {
    let dir = tempdir().unwrap();
    // Capture the sid before the first scope ends so the post-reload lookup
    // proves SidecarId persistence, not just "some folder entry survived".
    let sid = SidecarId::new();

    {
        let mut idx = ContentIndex::load(dir.path());
        let built = folders::build_folder("Pinned", &[]).expect("build");
        idx.insert(ContentIndexEntry {
            sidecar_id: sid,
            cid: built.bundle_cid.to_bytes(),
            file_name: "Pinned".into(),
            size_bytes: built.bundle_bytes.len() as u64,
            stored_at_ms: 1,
            sensitivity: Sensitivity::Private,
            replication_tier: ReplicationTier::Default,
            licensed: false,
            archived: false,
            pinned: true,
            backup: false,
            origin: None,
            kind: ContentKind::Folder,
        });
    }

    let idx = ContentIndex::load(dir.path());
    let entry = idx
        .get(&sid)
        .expect("folder entry persisted under same sid");
    assert_eq!(entry.kind, ContentKind::Folder);
    assert_eq!(entry.file_name, "Pinned");
    assert!(entry.pinned, "pin intent survives reload");
}

// ── Test 3: list_folder_end_to_end_with_two_children ─────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_folder_end_to_end_with_two_children() {
    use harmony_content::cid::{ContentFlags, ContentId};

    let bytes_a = b"alpha".to_vec();
    let bytes_b = b"beta".to_vec();
    let cid_a = ContentId::for_book(&bytes_a, ContentFlags::default()).unwrap();
    let cid_b = ContentId::for_book(&bytes_b, ContentFlags::default()).unwrap();
    let folder = folders::build_folder(
        "TwoChildren",
        &[
            folders::ManifestEntry {
                cid: cid_a.to_bytes(),
                name: "a.txt".into(),
                kind: ContentKind::Leaf,
            },
            folders::ManifestEntry {
                cid: cid_b.to_bytes(),
                name: "b.txt".into(),
                kind: ContentKind::Leaf,
            },
        ],
    )
    .expect("build");

    let harness = spawn_test_runtime().await;
    harmony_app::send_ingest(
        &harness.ingest_tx,
        hex::encode(cid_a.to_bytes()),
        bytes_a,
        false,
    )
    .await
    .unwrap();
    harmony_app::send_ingest(
        &harness.ingest_tx,
        hex::encode(cid_b.to_bytes()),
        bytes_b,
        false,
    )
    .await
    .unwrap();
    harmony_app::send_ingest(
        &harness.ingest_tx,
        hex::encode(folder.manifest_cid.to_bytes()),
        folder.manifest_bytes,
        false,
    )
    .await
    .unwrap();
    harmony_app::send_ingest(
        &harness.ingest_tx,
        hex::encode(folder.bundle_cid.to_bytes()),
        folder.bundle_bytes,
        false,
    )
    .await
    .unwrap();

    let empty_pinned = std::collections::HashSet::new();
    let rows = harmony_app::list_folder(
        hex::encode(folder.bundle_cid.to_bytes()),
        harness.verb_tx.clone(),
        &empty_pinned,
    )
    .await
    .expect("list_folder succeeds");

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].name, "a.txt");
    assert_eq!(rows[0].kind, "leaf");
    assert_eq!(rows[0].cid, hex::encode(cid_a.to_bytes()));
    assert_eq!(rows[1].name, "b.txt");
    assert!(!rows[0].pinned);
    assert!(!rows[1].pinned);
}

// ── Test 4: list_folder_empty_returns_empty_vec ───────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_folder_empty_returns_empty_vec() {
    let folder = folders::build_folder("Empty", &[]).expect("build");

    let harness = spawn_test_runtime().await;
    harmony_app::send_ingest(
        &harness.ingest_tx,
        hex::encode(folder.manifest_cid.to_bytes()),
        folder.manifest_bytes,
        false,
    )
    .await
    .unwrap();
    harmony_app::send_ingest(
        &harness.ingest_tx,
        hex::encode(folder.bundle_cid.to_bytes()),
        folder.bundle_bytes,
        false,
    )
    .await
    .unwrap();

    let empty_pinned = std::collections::HashSet::new();
    let rows = harmony_app::list_folder(
        hex::encode(folder.bundle_cid.to_bytes()),
        harness.verb_tx.clone(),
        &empty_pinned,
    )
    .await
    .expect("list_folder succeeds on empty folder");

    assert!(rows.is_empty(), "empty folder returns empty Vec");
}

// ── Test 5: list_folder_not_in_cache_returns_empty ────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_folder_not_in_cache_returns_empty() {
    let harness = spawn_test_runtime().await;

    let random_cid_hex = hex::encode([0x42u8; 32]);
    let empty_pinned = std::collections::HashSet::new();
    let rows = harmony_app::list_folder(random_cid_hex, harness.verb_tx.clone(), &empty_pinned)
        .await
        .expect("not-in-cache returns Ok(empty), not Err");

    assert!(rows.is_empty(), "cold cache returns empty, not an error");
}

// ── Test 6: list_folder_malformed_manifest_returns_error ─────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_folder_malformed_manifest_returns_error() {
    use harmony_content::bundle::BundleBuilder;
    use harmony_content::cid::{ContentFlags, ContentId};

    let bad_manifest = b"definitely not a folder manifest".to_vec();
    let bad_manifest_cid = ContentId::for_book(&bad_manifest, ContentFlags::default()).unwrap();
    let leaf_bytes = b"leaf".to_vec();
    let leaf_cid = ContentId::for_book(&leaf_bytes, ContentFlags::default()).unwrap();

    let mut builder = BundleBuilder::new();
    builder.add(bad_manifest_cid);
    builder.add(leaf_cid);
    let (bundle_bytes, bundle_cid) = builder.build_with_flags(ContentFlags::default()).unwrap();

    let harness = spawn_test_runtime().await;
    harmony_app::send_ingest(
        &harness.ingest_tx,
        hex::encode(leaf_cid.to_bytes()),
        leaf_bytes,
        false,
    )
    .await
    .unwrap();
    harmony_app::send_ingest(
        &harness.ingest_tx,
        hex::encode(bad_manifest_cid.to_bytes()),
        bad_manifest,
        false,
    )
    .await
    .unwrap();
    harmony_app::send_ingest(
        &harness.ingest_tx,
        hex::encode(bundle_cid.to_bytes()),
        bundle_bytes,
        false,
    )
    .await
    .unwrap();

    let empty_pinned = std::collections::HashSet::new();
    let err = harmony_app::list_folder(
        hex::encode(bundle_cid.to_bytes()),
        harness.verb_tx.clone(),
        &empty_pinned,
    )
    .await
    .expect_err("malformed manifest must surface an error, not an empty Vec");

    assert!(
        err.contains("manifest parse"),
        "error mentions manifest parse: {err}"
    );
}
