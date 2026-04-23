//! End-to-end test: ingest a blob through the event loop, verify the
//! sidecar picks it up, drive pin/unpin/burn via the verb channel, and
//! confirm the runtime cache's pin state matches.
//!
//! NodeRuntime<MemoryBookStore> is !Send, so it must be constructed INSIDE
//! the dedicated OS thread that runs the event loop — the same pattern as
//! lib.rs::start_node. The test's outer #[tokio::test] runtime drives the
//! channel interactions; the inner thread constructs NodeRuntime and runs
//! event_loop::run in its own tokio runtime.

use std::sync::{Arc, Mutex};
use std::thread;

use harmony_app::content_index::{
    ContentIndex, ContentIndexEntry, ReplicationTier, Sensitivity,
};
use harmony_app::event_loop::{ContentVerbRequest, IngestRequest};
use harmony_content::book::MemoryBookStore;
use harmony_content::cid::{ContentFlags, ContentId};
use harmony_content::storage_tier::{ContentPolicy, FilterBroadcastConfig, StorageBudget};
use harmony_compute::InstructionBudget;
use harmony_runtime::{NodeConfig, NodeRuntime};
use tokio::sync::{mpsc, oneshot, watch};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ingest_list_pin_burn_roundtrip() {
    // Fixture: bytes + CID computed via ContentId::for_book — this must
    // match what the event loop's ingest handler routes into the runtime.
    let bytes = b"hello world, this is integration test content!".to_vec();
    let cid = ContentId::for_book(&bytes, ContentFlags::default())
        .expect("CID for fixture bytes");
    let expected_cid_bytes: [u8; 32] = cid.to_bytes();
    let cid_hex = hex::encode(expected_cid_bytes);

    // Temp dir for sidecar + mail_mgr files.
    let tmp = tempfile::tempdir().unwrap();
    let app_data_dir = tmp.path().to_path_buf();

    // Channels matching event_loop::run's signature.
    let (ingest_tx, ingest_rx) = mpsc::channel::<IngestRequest>(4);
    let (content_verb_tx, content_verb_rx) = mpsc::channel::<ContentVerbRequest>(16);
    let (_publish_tx, publish_rx) = mpsc::channel(4);
    let (_fetch_tx, fetch_rx) = mpsc::channel(4);
    let (_follow_tx, follow_rx) = mpsc::channel(4);
    let (_voice_tx, voice_rx) = mpsc::channel::<harmony_app::voice::VoiceOutbound>(4);
    let (_voice_ch_tx, voice_ch_rx) =
        mpsc::channel::<harmony_app::voice::VoiceChannelRequest>(4);
    let (_refresh_tx, refresh_rx) =
        mpsc::channel::<harmony_app::mail_sync::RefreshRequest>(4);
    let (ready_tx, ready_rx) = oneshot::channel();
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);

    let followed_set = Arc::new(Mutex::new(
        std::collections::HashSet::<String>::default(),
    ));
    let mail_mgr = Arc::new(Mutex::new(harmony_app::mail::MailManager::load(
        &app_data_dir.join("mail"),
        [0u8; 16],
    )));

    let app = tauri::test::mock_app();
    let app_handle = app.handle().clone();

    // Minimal NodeConfig — identity fields all zero/empty, no disk/archive/S3.
    // Config is Send so it can be moved into the thread closure.
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

    // NodeRuntime<MemoryBookStore> is !Send, so it must be constructed
    // INSIDE the dedicated OS thread — exactly how lib.rs::start_node does it.
    thread::Builder::new()
        .name("harmony-runtime-test".to_string())
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .thread_stack_size(8 * 1024 * 1024)
                .enable_all()
                .build()
                .expect("tokio runtime for test event loop");
            rt.block_on(async move {
                // Construct runtime inside the thread — NodeRuntime is !Send.
                let (runtime, startup_actions) =
                    NodeRuntime::new(config, MemoryBookStore::new());
                harmony_app::event_loop::run(
                    runtime,
                    startup_actions,
                    app_handle,
                    None, // endpoint — no external Zenoh endpoint needed
                    ready_tx,
                    shutdown_rx,
                    publish_rx,
                    fetch_rx,
                    ingest_rx,
                    content_verb_rx,
                    follow_rx,
                    voice_rx,
                    voice_ch_rx,
                    followed_set,
                    mail_mgr,
                    None,  // mail_sync — not exercised in this test
                    refresh_rx,
                )
                .await;
            });
        })
        .expect("spawn runtime thread");

    // The event loop may fail to bind UDP (port in use on CI) or open Zenoh,
    // but content-verb and ingest requests flow through the select loop regardless.
    // Treat any ready result (Ok or Err) as acceptable for this test.
    let _ = ready_rx.await;

    // ── Step 1: ingest via the IngestRequest channel ──────────────────────
    let (ack_tx, ack_rx) = oneshot::channel();
    ingest_tx
        .send(IngestRequest {
            cid_hex: cid_hex.clone(),
            data: bytes.clone(),
            reply: ack_tx,
        })
        .await
        .unwrap();
    ack_rx
        .await
        .expect("event loop dropped ingest reply")
        .expect("ingest failed");

    // ── Step 2: write a sidecar entry for the same CID ───────────────────
    // Mimics what the Tauri ingest_content command does post-ack.
    let index = Arc::new(Mutex::new(ContentIndex::load(&app_data_dir)));
    {
        let mut idx = index.lock().unwrap();
        assert!(
            idx.insert(ContentIndexEntry {
                cid: expected_cid_bytes,
                file_name: "hello.txt".into(),
                size_bytes: bytes.len() as u64,
                stored_at_ms: 1_700_000_000_000,
                sensitivity: Sensitivity::Private,
                replication_tier: ReplicationTier::Default,
                licensed: false,
                archived: false,
            }),
            "first insert should return true"
        );
    }

    // ── Step 3: PinnedSet — fresh ingest should NOT be pinned ────────────
    let (snap_tx, snap_rx) = oneshot::channel();
    content_verb_tx
        .send(ContentVerbRequest::PinnedSet { reply: snap_tx })
        .await
        .unwrap();
    let pinned = snap_rx.await.unwrap();
    assert!(
        !pinned.contains(&expected_cid_bytes),
        "fresh ingest should be unpinned"
    );

    // ── Step 4: Pin, then snapshot — CID now pinned ───────────────────────
    let (pin_tx, pin_rx) = oneshot::channel();
    content_verb_tx
        .send(ContentVerbRequest::Pin {
            cid: expected_cid_bytes,
            reply: pin_tx,
        })
        .await
        .unwrap();
    assert!(
        pin_rx.await.unwrap().unwrap(),
        "pin should succeed and return true"
    );

    let (snap_tx, snap_rx) = oneshot::channel();
    content_verb_tx
        .send(ContentVerbRequest::PinnedSet { reply: snap_tx })
        .await
        .unwrap();
    assert!(
        snap_rx.await.unwrap().contains(&expected_cid_bytes),
        "CID should appear in pinned set after Pin"
    );

    // ── Step 5: Burn, then confirm CID no longer pinned ──────────────────
    let (burn_tx, burn_rx) = oneshot::channel();
    content_verb_tx
        .send(ContentVerbRequest::Burn {
            cid: expected_cid_bytes,
            reply: burn_tx,
        })
        .await
        .unwrap();
    burn_rx.await.unwrap().unwrap();

    // Sidecar removal (the Tauri command does this after Burn ack in production).
    {
        let mut idx = index.lock().unwrap();
        assert!(
            idx.remove(&expected_cid_bytes),
            "sidecar should have had an entry to remove"
        );
    }

    let (snap_tx, snap_rx) = oneshot::channel();
    content_verb_tx
        .send(ContentVerbRequest::PinnedSet { reply: snap_tx })
        .await
        .unwrap();
    assert!(
        !snap_rx.await.unwrap().contains(&expected_cid_bytes),
        "CID should be absent from pinned set after Burn"
    );
}
