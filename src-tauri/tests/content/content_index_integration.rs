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
    ContentIndex, ContentIndexEntry, ContentKind, ReplicationTier, Sensitivity, SidecarId,
};
use harmony_app::event_loop::{ContentVerbRequest, IngestRequest};
use harmony_compute::InstructionBudget;
use harmony_content::book::MemoryBookStore;
use harmony_content::cid::{ContentFlags, ContentId};
use harmony_content::storage_tier::{ContentPolicy, FilterBroadcastConfig, StorageBudget};
use harmony_runtime::{NodeConfig, NodeRuntime};
use tokio::sync::{mpsc, oneshot, watch};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ingest_list_pin_burn_roundtrip() {
    // Fixture: bytes + CID computed via ContentId::for_book — this must
    // match what the event loop's ingest handler routes into the runtime.
    let bytes = b"hello world, this is integration test content!".to_vec();
    let cid = ContentId::for_book(&bytes, ContentFlags::default()).expect("CID for fixture bytes");
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
    let (_voice_ch_tx, voice_ch_rx) = mpsc::channel::<harmony_app::voice::VoiceChannelRequest>(4);
    let (_refresh_tx, refresh_rx) = mpsc::channel::<harmony_app::mail_sync::RefreshRequest>(4);
    let (cas_op_tx, cas_op_rx) = mpsc::channel::<harmony_app::content_store::CasOp>(8);
    let (ready_tx, ready_rx) = oneshot::channel();
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);

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

    // ZEB-155: event_loop::run now takes pin_intent + fetch_completion
    // channel halves. This test doesn't exercise pin persistence, so an
    // empty set and a drain-only channel are sufficient.
    let (fetch_completion_tx, fetch_completion_rx) = mpsc::channel::<[u8; 32]>(4);
    let pin_intent: std::collections::HashSet<[u8; 32]> = std::collections::HashSet::new();

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
                let (runtime, startup_actions) = NodeRuntime::new(config, MemoryBookStore::new());
                harmony_app::event_loop::run(
                    runtime,
                    startup_actions,
                    event_sink,
                    None, // endpoint — no external Zenoh endpoint needed
                    ready_tx,
                    shutdown_rx,
                    publish_rx,
                    fetch_rx,
                    ingest_rx,
                    content_verb_rx,
                    cas_op_tx,
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
                    None, // mail_sync — not exercised in this test
                    refresh_rx,
                    pin_intent,
                    fetch_completion_tx,
                    fetch_completion_rx,
                    None,       // pairing_in_tx — pairing not exercised in this test
                    None,       // sync_handles — SyncEngine not exercised in this test
                    None,       // dm_outbox — DM outbox not exercised in this test
                    None,       // dm_transport — DM outbox not exercised in this test
                    None,       // crdt_state — DM outbox not exercised in this test
                    None,       // ZEB-703: owner_sync_engine — DM outbox not exercised in this test
                    Vec::new(), // community_adapters — Phase 2 community sync not exercised in this test
                    {
                        // Phase 3 Task 9: on-demand adapter request channel; tx
                        // dropped immediately so the select arm idles.
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

    // Require the event loop to report ready before we start exercising its
    // channels. If startup fails the receivers are dropped and subsequent
    // `.send(...).unwrap()` calls would panic with a confusing "channel
    // closed" error — surface a clear message here instead.
    match ready_rx.await {
        Ok(Ok(())) => {} // proceed
        // ZEB-446 made the Reticulum bind degradable (a 4242 collision warns and
        // falls back to an ephemeral loopback bind), so `run()` no longer returns
        // an "Address already in use" error. A real start failure now fails loudly
        // instead of being silently skipped (ZEB-420).
        Ok(Err(e)) => panic!("event loop failed to start: {e}"),
        Err(_) => panic!("event loop dropped ready signal"),
    }

    // ── Step 1: ingest via the IngestRequest channel ──────────────────────
    let (ack_tx, ack_rx) = oneshot::channel();
    ingest_tx
        .send(IngestRequest {
            cid_hex: cid_hex.clone(),
            data: bytes.clone(),
            serveable: false,
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
    let sid = SidecarId::new();
    {
        let mut idx = index.lock().unwrap();
        assert!(
            idx.insert(ContentIndexEntry {
                sidecar_id: sid,
                cid: expected_cid_bytes,
                file_name: "hello.txt".into(),
                size_bytes: bytes.len() as u64,
                stored_at_ms: 1_700_000_000_000,
                sensitivity: Sensitivity::Private,
                replication_tier: ReplicationTier::Default,
                licensed: false,
                archived: false,
                pinned: false,
                backup: false,
                origin: None,
                kind: ContentKind::Leaf,
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
            idx.remove(&sid),
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chunked_ingest_pin_cascade_fetch_burn_roundtrip() {
    use harmony_app::content_index::{self, ContentIndexEntry, ReplicationTier, Sensitivity};
    use harmony_app::event_loop::{ContentVerbRequest, IngestRequest};
    use harmony_app::streaming_ingest;
    use harmony_content::chunker::ChunkerConfig;
    use harmony_content::cid::CidType;
    use std::collections::HashSet;

    // ── Harness setup (copied verbatim from ingest_list_pin_burn_roundtrip) ──
    let tmp = tempfile::tempdir().unwrap();
    let app_data_dir = tmp.path().to_path_buf();

    let (ingest_tx, ingest_rx) = mpsc::channel::<IngestRequest>(4);
    let (content_verb_tx, content_verb_rx) = mpsc::channel::<ContentVerbRequest>(16);
    let (_publish_tx, publish_rx) = mpsc::channel(4);
    let (_fetch_tx, fetch_rx) = mpsc::channel(4);
    let (_follow_tx, follow_rx) = mpsc::channel(4);
    let (_voice_tx, voice_rx) = mpsc::channel::<harmony_app::voice::VoiceOutbound>(4);
    let (_voice_ch_tx, voice_ch_rx) = mpsc::channel::<harmony_app::voice::VoiceChannelRequest>(4);
    let (_refresh_tx, refresh_rx) = mpsc::channel::<harmony_app::mail_sync::RefreshRequest>(4);
    let (cas_op_tx, cas_op_rx) = mpsc::channel::<harmony_app::content_store::CasOp>(8);
    let (ready_tx, ready_rx) = oneshot::channel();
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);

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
        storage_budget: harmony_content::storage_tier::StorageBudget {
            cache_capacity: 512,
            max_pinned_bytes: 50_000_000,
        },
        compute_budget: InstructionBudget { fuel: 100_000 },
        schedule: Default::default(),
        content_policy: harmony_content::storage_tier::ContentPolicy::default(),
        filter_broadcast_config: harmony_content::storage_tier::FilterBroadcastConfig {
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

    // ZEB-155: event_loop::run now takes pin_intent + fetch_completion
    // channel halves. This test doesn't exercise pin persistence, so an
    // empty set and a drain-only channel are sufficient.
    let (fetch_completion_tx, fetch_completion_rx) = mpsc::channel::<[u8; 32]>(4);
    let pin_intent: std::collections::HashSet<[u8; 32]> = std::collections::HashSet::new();

    thread::Builder::new()
        .name("harmony-runtime-test-chunked".to_string())
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .thread_stack_size(8 * 1024 * 1024)
                .enable_all()
                .build()
                .expect("tokio runtime for test event loop");
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
                    cas_op_tx,
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
                    None,       // pairing_in_tx
                    None,       // sync_handles — SyncEngine not exercised in this test
                    None,       // dm_outbox — DM outbox not exercised in this test
                    None,       // dm_transport — DM outbox not exercised in this test
                    None,       // crdt_state — DM outbox not exercised in this test
                    None,       // ZEB-703: owner_sync_engine — DM outbox not exercised in this test
                    Vec::new(), // community_adapters — Phase 2 community sync not exercised in this test
                    {
                        // Phase 3 Task 9: on-demand adapter request channel; tx
                        // dropped immediately so the select arm idles.
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
        Ok(Ok(())) => {}
        // ZEB-446 made the Reticulum bind degradable (a 4242 collision warns and
        // falls back to an ephemeral loopback bind), so `run()` no longer returns
        // an "Address already in use" error. A real start failure now fails loudly
        // instead of being silently skipped (ZEB-420).
        Ok(Err(e)) => panic!("event loop failed to start: {e}"),
        Err(_) => panic!("event loop dropped ready signal"),
    }

    // ── Step 1: Generate 3 MiB deterministic bytes and stream-ingest ──
    let bytes: Vec<u8> = (0..3 * 1024 * 1024)
        .map(|i| ((i * 37) % 251) as u8)
        .collect();

    // Wrap `ingest_tx` in a forwarding channel that records every (cid_hex,
    // data_len) it sees on its way to the runtime. Lets the test recompute
    // `expected_descendants` (root + every leaf + every interior bundle)
    // from the actual stream rather than depending on a re-walk of the
    // ingested bundle tree. Captured pairs are kept in `captured_cids`,
    // shared with the forwarder task via Arc<Mutex<_>>.
    let captured_cids: Arc<Mutex<Vec<[u8; 32]>>> = Arc::new(Mutex::new(Vec::new()));
    let (capture_tx, mut capture_rx) = mpsc::channel::<IngestRequest>(4);
    let forwarder_real_tx = ingest_tx.clone();
    let forwarder_captured = captured_cids.clone();
    let forwarder = tokio::spawn(async move {
        while let Some(req) = capture_rx.recv().await {
            // Record the CID this send is for.
            let cid_bytes = ::hex::decode(&req.cid_hex)
                .ok()
                .and_then(|v| v.try_into().ok())
                .expect("forwarder: cid_hex must decode to 32 bytes");
            forwarder_captured.lock().unwrap().push(cid_bytes);
            // Forward to the real ingest channel and await its ack so the
            // streaming caller sees backpressure consistent with the
            // event-loop's response time.
            let (ack_tx, ack_rx) = oneshot::channel();
            forwarder_real_tx
                .send(IngestRequest {
                    cid_hex: req.cid_hex,
                    data: req.data,
                    serveable: false,
                    reply: ack_tx,
                })
                .await
                .expect("forward to event loop");
            let ack = ack_rx.await.expect("event loop ack");
            let _ = req.reply.send(ack);
        }
    });

    let (root_cid, _streamed_bytes) =
        streaming_ingest(bytes.as_slice(), &capture_tx, ChunkerConfig::DEFAULT, None)
            .await
            .expect("streaming ingest must succeed");
    drop(capture_tx);
    forwarder.await.expect("forwarder task joins cleanly");

    assert!(
        matches!(root_cid.cid_type(), CidType::Bundle(_)),
        "precondition: root must be a bundle"
    );

    // `expected_descendants` is the set of every CID streaming_ingest
    // pushed through the channel — leaves + every interior bundle + root.
    // For a 3 MiB / 256 KiB-min-chunk input this is depth-1 (≤16 leaves),
    // so the count is leaf_count + 1 (root bundle). The Pin verb is
    // expected to cascade across exactly this set.
    let expected_descendants: HashSet<[u8; 32]> =
        captured_cids.lock().unwrap().iter().copied().collect();
    assert!(
        expected_descendants.contains(&root_cid.to_bytes()),
        "captured set must include the root CID"
    );
    assert!(
        expected_descendants.len() >= 4,
        "3 MiB input should produce >= 3 leaves + root bundle (got {})",
        expected_descendants.len()
    );

    // ── Step 3: Sidecar insert for the root CID ───────────────────────
    let index = Arc::new(Mutex::new(content_index::ContentIndex::load(&app_data_dir)));
    let root_sid = SidecarId::new();
    {
        let mut idx = index.lock().unwrap();
        assert!(idx.insert(ContentIndexEntry {
            sidecar_id: root_sid,
            cid: root_cid.to_bytes(),
            file_name: "chunked.bin".into(),
            size_bytes: bytes.len() as u64,
            stored_at_ms: 1_700_000_000_000,
            sensitivity: Sensitivity::Private,
            replication_tier: ReplicationTier::Default,
            licensed: false,
            archived: false,
            pinned: false,
            backup: false,
            origin: None,
            kind: ContentKind::Leaf,
        }));
    }

    // ── Step 4: PinnedSet before pinning — empty ──────────────────────
    let (reply_tx, reply_rx) = oneshot::channel();
    content_verb_tx
        .send(ContentVerbRequest::PinnedSet { reply: reply_tx })
        .await
        .unwrap();
    let pinned = reply_rx.await.unwrap();
    assert!(pinned.is_empty(), "no pins before Pin verb");

    // ── Step 5: Pin root — expect cascade to all descendants ──────────
    let (reply_tx, reply_rx) = oneshot::channel();
    content_verb_tx
        .send(ContentVerbRequest::Pin {
            cid: root_cid.to_bytes(),
            reply: reply_tx,
        })
        .await
        .unwrap();
    let ok = reply_rx.await.unwrap().unwrap();
    assert!(ok, "Pin cascade should succeed for a freshly-ingested tree");

    let (reply_tx, reply_rx) = oneshot::channel();
    content_verb_tx
        .send(ContentVerbRequest::PinnedSet { reply: reply_tx })
        .await
        .unwrap();
    let pinned_after = reply_rx.await.unwrap();
    assert_eq!(
        pinned_after, expected_descendants,
        "Pin should cascade to root + every leaf"
    );

    // ── Step 6: Sanity — streaming ingest produced a valid tree ──────
    // The pre-ZEB-161 test reassembled bytes from the local `leaves` /
    // `bundle_payload` returned by `chunk_and_bundle`. The streaming
    // pipeline doesn't return those buffers; reassembly correctness for
    // the chunker+tree-builder is covered by the unit tests in
    // `streaming_ingest_tests` (single-shot vs. multi-feed equivalence)
    // and ZEB-161 Task 5 lands a deeper depth-2 nested-bundle round-trip
    // here. No fetch_via_zenoh exercise; that's ZEB-150 E2E.

    // ── Step 7: Burn root — expect cascade unpin ──────────────────────
    let (reply_tx, reply_rx) = oneshot::channel();
    content_verb_tx
        .send(ContentVerbRequest::Burn {
            cid: root_cid.to_bytes(),
            reply: reply_tx,
        })
        .await
        .unwrap();
    reply_rx.await.unwrap().unwrap();

    let (reply_tx, reply_rx) = oneshot::channel();
    content_verb_tx
        .send(ContentVerbRequest::PinnedSet { reply: reply_tx })
        .await
        .unwrap();
    let pinned_after_burn = reply_rx.await.unwrap();
    assert!(
        pinned_after_burn.is_empty(),
        "Burn should cascade-unpin every descendant"
    );

    // ── Step 8: Sidecar removal (mirroring the burn_content command) ──
    {
        let mut idx = index.lock().unwrap();
        assert!(idx.remove(&root_sid));
    }
}

/// ZEB-155: verify that calling `set_pinned(true)` persists across a
/// load/reload cycle. This is the minimum regression test that the
/// pin_content command must preserve when Step 4 wires set_pinned into
/// the command body. The full end-to-end Tauri-command path is covered
/// by frontend manual QA; this test fixes the data-layer contract.
#[test]
fn pin_intent_survives_reload() {
    let tmp = tempfile::tempdir().unwrap();
    let cid = [0xC1u8; 32];
    let sid = SidecarId::new();

    {
        let mut idx = ContentIndex::load(tmp.path());
        idx.insert(ContentIndexEntry {
            sidecar_id: sid,
            cid,
            file_name: "persist-me.bin".into(),
            size_bytes: 100,
            stored_at_ms: 1_700_000_000_000,
            sensitivity: Sensitivity::Private,
            replication_tier: ReplicationTier::Default,
            licensed: false,
            archived: false,
            pinned: false,
            backup: false,
            origin: None,
            kind: ContentKind::Leaf,
        });
        assert!(
            idx.set_pinned(&sid, true),
            "initial flip should report change"
        );
    }

    // Reload — simulates app restart. SidecarId is part of the persisted
    // file format, so the same `sid` resolves the entry post-reload.
    let reloaded = ContentIndex::load(tmp.path());
    let entry = reloaded.get(&sid).expect("entry must persist");
    assert!(
        entry.pinned,
        "pinned intent must survive reload (this is the ZEB-155 bug fix)"
    );
}

/// ZEB-155 + ZEB-159: when the fetch-completion arm receives a root CID
/// that's in pin_intent, the cascade pins the root (and any descendants)
/// in the runtime cache. Injected via a test-owned fetch_completion_tx
/// clone so we don't need a real peer to answer a fetch_rx request.
///
/// ZEB-159 made the real fetch_rx → cache-admission → completion path
/// work end-to-end (the spawned fetch task now admits each fetched CID
/// via a synchronous CasOp::PutLocal { reply: Some(_) } round-trip per
/// CID before signaling completion — the synchronous ordering is the
/// R1 fix for the Cursor + Qodo race finding that fire-and-forget
/// admission would let the completion arm walk a partial cache).
/// This test continues to exercise the cascade arm directly by injecting
/// completion synthetically — the synthetic path remains valuable as a
/// unit-style assertion that does not require a live Zenoh peer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fetch_complete_arm_pins_root_in_intent() {
    use std::collections::HashSet;

    let bytes = b"zeb-155 fetch-complete repin fixture".to_vec();
    let cid = ContentId::for_book(&bytes, ContentFlags::default()).expect("fixture CID");
    let cid_bytes: [u8; 32] = cid.to_bytes();
    let cid_hex = hex::encode(cid_bytes);

    let tmp = tempfile::tempdir().unwrap();
    let app_data_dir = tmp.path().to_path_buf();

    let (ingest_tx, ingest_rx) = mpsc::channel::<IngestRequest>(4);
    let (content_verb_tx, content_verb_rx) = mpsc::channel::<ContentVerbRequest>(16);
    let (_publish_tx, publish_rx) = mpsc::channel(4);
    let (_fetch_tx, fetch_rx) = mpsc::channel(4);
    let (_follow_tx, follow_rx) = mpsc::channel(4);
    let (_voice_tx, voice_rx) = mpsc::channel::<harmony_app::voice::VoiceOutbound>(4);
    let (_voice_ch_tx, voice_ch_rx) = mpsc::channel::<harmony_app::voice::VoiceChannelRequest>(4);
    let (_refresh_tx, refresh_rx) = mpsc::channel::<harmony_app::mail_sync::RefreshRequest>(4);
    let (cas_op_tx, cas_op_rx) = mpsc::channel::<harmony_app::content_store::CasOp>(8);
    let (ready_tx, ready_rx) = oneshot::channel();
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);

    // ZEB-155: the fetch-completion channel. Test keeps its own clone of
    // the sender so it can inject the completion signal synthetically.
    let (fetch_completion_tx, fetch_completion_rx) = mpsc::channel::<[u8; 32]>(8);
    let fetch_completion_tx_for_test = fetch_completion_tx.clone();

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

    // Seed pin_intent with our CID so the completion arm will recognize it.
    let mut pin_intent: HashSet<[u8; 32]> = HashSet::new();
    pin_intent.insert(cid_bytes);

    thread::Builder::new()
        .name("harmony-runtime-zeb155".to_string())
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .thread_stack_size(8 * 1024 * 1024)
                .enable_all()
                .build()
                .expect("tokio runtime");
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
                    cas_op_tx,
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
                    None,       // pairing_in_tx
                    None,       // sync_handles — SyncEngine not exercised in this test
                    None,       // dm_outbox — DM outbox not exercised in this test
                    None,       // dm_transport — DM outbox not exercised in this test
                    None,       // crdt_state — DM outbox not exercised in this test
                    None,       // ZEB-703: owner_sync_engine — DM outbox not exercised in this test
                    Vec::new(), // community_adapters — Phase 2 community sync not exercised in this test
                    {
                        // Phase 3 Task 9: on-demand adapter request channel; tx
                        // dropped immediately so the select arm idles.
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
        Ok(Ok(())) => {}
        // ZEB-446 made the Reticulum bind degradable (a 4242 collision warns and
        // falls back to an ephemeral loopback bind), so `run()` no longer returns
        // an "Address already in use" error. A real start failure now fails loudly
        // instead of being silently skipped (ZEB-420).
        Ok(Err(e)) => panic!("event loop failed to start: {e}"),
        Err(_) => panic!("event loop dropped ready signal"),
    }

    // Admit bytes for the CID by ingesting. Required because collect_descendants
    // walks the cache; pin_content is a no-op on an unadmitted CID.
    let (ack_tx, ack_rx) = oneshot::channel();
    ingest_tx
        .send(IngestRequest {
            cid_hex: cid_hex.clone(),
            data: bytes.clone(),
            serveable: false,
            reply: ack_tx,
        })
        .await
        .unwrap();
    ack_rx.await.unwrap().expect("ingest succeeded");

    // Baseline: the CID is admitted but unpinned (fresh ingest doesn't pin).
    let (snap_tx, snap_rx) = oneshot::channel();
    content_verb_tx
        .send(ContentVerbRequest::PinnedSet { reply: snap_tx })
        .await
        .unwrap();
    assert!(
        !snap_rx.await.unwrap().contains(&cid_bytes),
        "baseline: fresh ingest should not be pinned",
    );

    // Inject the completion signal. The main-loop arm will consult
    // pin_intent, find our CID, and run the cascade.
    fetch_completion_tx_for_test.send(cid_bytes).await.unwrap();

    // Poll PinnedSet until the cascade lands, or time out. The completion
    // arm and the snapshot arm are both serviced by the same select loop,
    // so we can't race them in principle, but tokio scheduling can still
    // interleave replies.
    let mut attempts = 0;
    loop {
        let (snap_tx, snap_rx) = oneshot::channel();
        content_verb_tx
            .send(ContentVerbRequest::PinnedSet { reply: snap_tx })
            .await
            .unwrap();
        if snap_rx.await.unwrap().contains(&cid_bytes) {
            break; // success
        }
        attempts += 1;
        if attempts > 20 {
            panic!(
                "fetch-completion arm did not pin the CID within ~1s \
                 (20 × 50ms); pin_intent containing the CID should \
                 trigger the cascade on completion signal",
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// ZEB-156: unpinning a folder root must not unpin a leaf that is also
/// independently pinned via a separate sidecar entry. This is the
/// integration-level guard for the transitive-sharing keep-set fix in the
/// event-loop Unpin arm — pre-fix, the cascade walked the folder bundle's
/// subtree and unpinned every descendant indiscriminately, including the
/// independently-pinned leaf. The Tauri OR-join only spots sibling-root
/// sharing (two sidecar entries with the same root CID); transitive
/// sharing (one sidecar entry's CID being a descendant of another's) is
/// invisible to it, so the event loop must close that gap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unpin_folder_leaves_independently_pinned_leaf_in_cache() {
    use harmony_content::bundle::BundleBuilder;

    // Fixture: a small leaf and a folder bundle that references it.
    // Both are admitted into the runtime via the IngestRequest channel,
    // then sidecar rows are written for each so the runtime treats them
    // as independent pinnable roots.
    let leaf_bytes = b"zeb-156 leaf payload - pinned standalone".to_vec();
    let cid_a =
        ContentId::for_book(&leaf_bytes, ContentFlags::default()).expect("CID for leaf fixture");
    let cid_a_bytes: [u8; 32] = cid_a.to_bytes();
    let cid_a_hex = hex::encode(cid_a_bytes);

    // Folder bundle: single-child manifest referencing cid_A. The
    // folder's root CID (cid_C) is structurally distinct from cid_A
    // because the bundle payload is the serialized child list, not the
    // child's bytes.
    let mut builder = BundleBuilder::new();
    builder.add(cid_a);
    let (folder_payload, cid_c) = builder
        .build_with_flags(ContentFlags::default())
        .expect("bundle build");
    let cid_c_bytes: [u8; 32] = cid_c.to_bytes();
    let cid_c_hex = hex::encode(cid_c_bytes);
    assert_ne!(
        cid_a_bytes, cid_c_bytes,
        "leaf and folder must have distinct CIDs"
    );

    let tmp = tempfile::tempdir().unwrap();
    let app_data_dir = tmp.path().to_path_buf();

    let (ingest_tx, ingest_rx) = mpsc::channel::<IngestRequest>(4);
    let (content_verb_tx, content_verb_rx) = mpsc::channel::<ContentVerbRequest>(16);
    let (_publish_tx, publish_rx) = mpsc::channel(4);
    let (_fetch_tx, fetch_rx) = mpsc::channel(4);
    let (_follow_tx, follow_rx) = mpsc::channel(4);
    let (_voice_tx, voice_rx) = mpsc::channel::<harmony_app::voice::VoiceOutbound>(4);
    let (_voice_ch_tx, voice_ch_rx) = mpsc::channel::<harmony_app::voice::VoiceChannelRequest>(4);
    let (_refresh_tx, refresh_rx) = mpsc::channel::<harmony_app::mail_sync::RefreshRequest>(4);
    let (cas_op_tx, cas_op_rx) = mpsc::channel::<harmony_app::content_store::CasOp>(8);
    let (ready_tx, ready_rx) = oneshot::channel();
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);

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
        .name("harmony-runtime-zeb156".to_string())
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .thread_stack_size(8 * 1024 * 1024)
                .enable_all()
                .build()
                .expect("tokio runtime");
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
                    cas_op_tx,
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
                    None, // ZEB-703: owner_sync_engine — DM outbox not exercised in this test
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
        // an "Address already in use" error. A real start failure now fails loudly
        // instead of being silently skipped (ZEB-420).
        Ok(Err(e)) => panic!("event loop failed to start: {e}"),
        Err(_) => panic!("event loop dropped ready signal"),
    }

    // ── Step 1: ingest the leaf bytes ───────────────────────────────────
    let (ack_tx, ack_rx) = oneshot::channel();
    ingest_tx
        .send(IngestRequest {
            cid_hex: cid_a_hex.clone(),
            data: leaf_bytes.clone(),
            serveable: false,
            reply: ack_tx,
        })
        .await
        .unwrap();
    ack_rx.await.unwrap().expect("leaf ingest failed");

    // ── Step 2: ingest the folder bundle (manifest referencing cid_A) ─
    let (ack_tx, ack_rx) = oneshot::channel();
    ingest_tx
        .send(IngestRequest {
            cid_hex: cid_c_hex.clone(),
            data: folder_payload.clone(),
            serveable: false,
            reply: ack_tx,
        })
        .await
        .unwrap();
    ack_rx.await.unwrap().expect("folder ingest failed");

    // ── Step 3: write sidecar rows for both as independent pinnable roots
    // (mimics what the Tauri ingest_content / folder upload paths do).
    let index = Arc::new(Mutex::new(ContentIndex::load(&app_data_dir)));
    let sid_a = SidecarId::new();
    let sid_c = SidecarId::new();
    {
        let mut idx = index.lock().unwrap();
        assert!(idx.insert(ContentIndexEntry {
            sidecar_id: sid_a,
            cid: cid_a_bytes,
            file_name: "leaf.txt".into(),
            size_bytes: leaf_bytes.len() as u64,
            stored_at_ms: 1_700_000_000_000,
            sensitivity: Sensitivity::Private,
            replication_tier: ReplicationTier::Default,
            licensed: false,
            archived: false,
            pinned: false,
            backup: false,
            origin: None,
            kind: ContentKind::Leaf,
        }));
        assert!(idx.insert(ContentIndexEntry {
            sidecar_id: sid_c,
            cid: cid_c_bytes,
            file_name: "folder".into(),
            size_bytes: folder_payload.len() as u64,
            stored_at_ms: 1_700_000_000_000,
            sensitivity: Sensitivity::Private,
            replication_tier: ReplicationTier::Default,
            licensed: false,
            archived: false,
            pinned: false,
            backup: false,
            origin: None,
            kind: ContentKind::Folder,
        }));
    }

    // ── Step 4: Pin both sidecar entries via the runtime's Pin verb ──
    // The Pin arm cascades over the bundle subtree, but the Pin for
    // cid_A also seeds pin_intent so cid_A remains a recognized root
    // when cid_C is later unpinned.
    for cid in [cid_a_bytes, cid_c_bytes] {
        let (reply_tx, reply_rx) = oneshot::channel();
        content_verb_tx
            .send(ContentVerbRequest::Pin {
                cid,
                reply: reply_tx,
            })
            .await
            .unwrap();
        assert!(
            reply_rx.await.unwrap().unwrap(),
            "Pin should succeed for {}",
            hex::encode(cid),
        );
    }

    // ── Step 5: confirm both CIDs are pinned in the cache (precondition).
    let (reply_tx, reply_rx) = oneshot::channel();
    content_verb_tx
        .send(ContentVerbRequest::PinnedSet { reply: reply_tx })
        .await
        .unwrap();
    let pinned_before = reply_rx.await.unwrap();
    assert!(
        pinned_before.contains(&cid_a_bytes),
        "precondition: leaf must be pinned"
    );
    assert!(
        pinned_before.contains(&cid_c_bytes),
        "precondition: folder must be pinned"
    );

    // ── Step 6: send Unpin(cid_C) ───────────────────────────────────────
    let (reply_tx, reply_rx) = oneshot::channel();
    content_verb_tx
        .send(ContentVerbRequest::Unpin {
            cid: cid_c_bytes,
            reply: reply_tx,
        })
        .await
        .unwrap();
    assert!(
        reply_rx.await.unwrap().unwrap(),
        "Unpin(folder) verb should succeed"
    );

    // ── Step 7 + 8: cid_A still pinned, cid_C unpinned ───────────────────
    let (reply_tx, reply_rx) = oneshot::channel();
    content_verb_tx
        .send(ContentVerbRequest::PinnedSet { reply: reply_tx })
        .await
        .unwrap();
    let pinned_after = reply_rx.await.unwrap();
    assert!(
        pinned_after.contains(&cid_a_bytes),
        "ZEB-156: leaf must STILL be pinned after folder unpin \
         (its sidecar entry has pinned=true and its CID is reachable \
         from the keep-set walk of remaining pin_intent roots; pre-fix \
         the cascade indiscriminately walked cid_C's subtree and \
         unpinned cid_A)",
    );
    assert!(
        !pinned_after.contains(&cid_c_bytes),
        "folder root itself must be unpinned (it was the Unpin target)",
    );
}

/// ZEB-160 integration test 8: rapid pin/unpin toggling under
/// `pin_serial_lock` keeps the sidecar's `pinned` bit and the runtime
/// cache's `is_pinned(cid)` state in agreement.
///
/// Pre-fix: each IPC dropped its sidecar mutex before
/// `verb_tx.send().await`, so two interleaved toggles could land at
/// the event loop in opposite order from their sidecar mutations —
/// final outcome: sidecar=unpinned, runtime cache=pinned (or vice
/// versa). Post-fix: a process-wide `tokio::sync::Mutex<()>` held by
/// every pin/unpin/burn IPC from sidecar mutation through reply await
/// linearises all three verbs, so the LAST committed sidecar
/// mutation's verb is also the LAST verb the event loop applies.
///
/// The IPC handlers (`pin_content`/`unpin_content`) are private, so
/// this test replicates their critical section inline: acquire the
/// same `pin_serial_lock`, mutate the sidecar, dispatch the verb, await
/// the reply — all under one lock — exactly as the production IPCs do.
/// This is the spec's allowed alternative pattern.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rapid_pin_unpin_toggling_keeps_sidecar_and_runtime_consistent() {
    let bytes = b"zeb-160 rapid toggle fixture".to_vec();
    let cid = ContentId::for_book(&bytes, ContentFlags::default()).expect("fixture CID");
    let cid_bytes: [u8; 32] = cid.to_bytes();
    let cid_hex = hex::encode(cid_bytes);

    let tmp = tempfile::tempdir().unwrap();
    let app_data_dir = tmp.path().to_path_buf();

    let (ingest_tx, ingest_rx) = mpsc::channel::<IngestRequest>(4);
    let (content_verb_tx, content_verb_rx) = mpsc::channel::<ContentVerbRequest>(64);
    let (_publish_tx, publish_rx) = mpsc::channel(4);
    let (_fetch_tx, fetch_rx) = mpsc::channel(4);
    let (_follow_tx, follow_rx) = mpsc::channel(4);
    let (_voice_tx, voice_rx) = mpsc::channel::<harmony_app::voice::VoiceOutbound>(4);
    let (_voice_ch_tx, voice_ch_rx) = mpsc::channel::<harmony_app::voice::VoiceChannelRequest>(4);
    let (_refresh_tx, refresh_rx) = mpsc::channel::<harmony_app::mail_sync::RefreshRequest>(4);
    let (cas_op_tx, cas_op_rx) = mpsc::channel::<harmony_app::content_store::CasOp>(8);
    let (ready_tx, ready_rx) = oneshot::channel();
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);

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
        .name("harmony-runtime-zeb160".to_string())
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .thread_stack_size(8 * 1024 * 1024)
                .enable_all()
                .build()
                .expect("tokio runtime");
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
                    cas_op_tx,
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
                    None, // ZEB-703: owner_sync_engine — DM outbox not exercised in this test
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
        // an "Address already in use" error. A real start failure now fails loudly
        // instead of being silently skipped (ZEB-420).
        Ok(Err(e)) => panic!("event loop failed to start: {e}"),
        Err(_) => panic!("event loop dropped ready signal"),
    }

    // ── Step 1: ingest the fixture bytes so the cache knows the CID ──
    let (ack_tx, ack_rx) = oneshot::channel();
    ingest_tx
        .send(IngestRequest {
            cid_hex: cid_hex.clone(),
            data: bytes.clone(),
            serveable: false,
            reply: ack_tx,
        })
        .await
        .unwrap();
    ack_rx.await.unwrap().expect("ingest must succeed");

    // ── Step 2: write a sidecar row with pinned=false initially ─────────
    let index = Arc::new(Mutex::new(ContentIndex::load(&app_data_dir)));
    let sid = SidecarId::new();
    {
        let mut idx = index.lock().unwrap();
        assert!(idx.insert(ContentIndexEntry {
            sidecar_id: sid,
            cid: cid_bytes,
            file_name: "rapid-toggle.txt".into(),
            size_bytes: bytes.len() as u64,
            stored_at_ms: 1_700_000_000_000,
            sensitivity: Sensitivity::Private,
            replication_tier: ReplicationTier::Default,
            licensed: false,
            archived: false,
            pinned: false,
            backup: false,
            origin: None,
            kind: ContentKind::Leaf,
        }));
    }

    // ── Step 3: shared pin_serial_lock + helper closures matching the
    // production IPC critical sections (pin_content / unpin_content).
    //
    // The production IPCs are private, so we replicate the same locked
    // region inline. The lock is the load-bearing primitive under test —
    // if it's elided, this test fails frequently; with it, the test
    // passes every time.
    let pin_serial_lock = Arc::new(tokio::sync::Mutex::new(()));

    // Pin closure: acquire serial_lock, set sidecar pinned=true, dispatch
    // ContentVerbRequest::Pin, await reply.
    let do_pin = {
        let lock = pin_serial_lock.clone();
        let index = index.clone();
        let verb_tx = content_verb_tx.clone();
        move || {
            let lock = lock.clone();
            let index = index.clone();
            let verb_tx = verb_tx.clone();
            async move {
                let _guard = lock.lock().await;
                {
                    let mut idx = index.lock().unwrap();
                    idx.set_pinned(&sid, true);
                }
                let (reply_tx, reply_rx) = oneshot::channel();
                verb_tx
                    .send(ContentVerbRequest::Pin {
                        cid: cid_bytes,
                        reply: reply_tx,
                    })
                    .await
                    .expect("verb_tx send");
                // Assert the runtime's inner Result is also Ok so an
                // event-loop-side Pin failure (cache full / quota
                // exhaustion) doesn't get masked by the outer recv
                // succeeding — CodeRabbit R2 finding.
                reply_rx
                    .await
                    .expect("pin reply")
                    .expect("runtime Pin returned Err");
            }
        }
    };

    // Unpin closure: acquire serial_lock, set sidecar pinned=false,
    // dispatch ContentVerbRequest::Unpin (no OR-join check needed here
    // because there's exactly one sidecar entry), await reply.
    let do_unpin = {
        let lock = pin_serial_lock.clone();
        let index = index.clone();
        let verb_tx = content_verb_tx.clone();
        move || {
            let lock = lock.clone();
            let index = index.clone();
            let verb_tx = verb_tx.clone();
            async move {
                let _guard = lock.lock().await;
                {
                    let mut idx = index.lock().unwrap();
                    idx.set_pinned(&sid, false);
                }
                let (reply_tx, reply_rx) = oneshot::channel();
                verb_tx
                    .send(ContentVerbRequest::Unpin {
                        cid: cid_bytes,
                        reply: reply_tx,
                    })
                    .await
                    .expect("verb_tx send");
                // Assert the runtime's inner Result — see Pin closure.
                reply_rx
                    .await
                    .expect("unpin reply")
                    .expect("runtime Unpin returned Err");
            }
        }
    };

    // ── Step 4: rapid alternation — 100 toggles interleaved across two
    // spawned task chains. Each chain serialises its half on its own
    // tokio::spawn (parallelism = 2), but ALL operations share the
    // same pin_serial_lock, so the event loop sees a fully linearised
    // sequence.
    //
    // Pin-then-unpin parity (50 of each) keeps the schedule symmetric;
    // the deterministic tail-pin at Step 5 below (not any task-side
    // bookkeeping) is what disambiguates the final expected state.
    let mut handles = Vec::with_capacity(100);
    for i in 0..100 {
        if i % 2 == 0 {
            let pin = do_pin.clone();
            handles.push(tokio::spawn(async move {
                pin().await;
            }));
        } else {
            let unpin = do_unpin.clone();
            handles.push(tokio::spawn(async move {
                unpin().await;
            }));
        }
    }
    for h in handles {
        h.await.expect("toggle task joins");
    }

    // ── Step 5: drive one last deterministic operation under the lock so
    // the final sidecar state + final verb dispatch are unambiguous.
    // (Without this, the 100 tokio::spawn'd tasks could finish in any
    // schedule order, and `last_op` would reflect whichever task ran
    // last on the executor — which is also fine, but a deterministic
    // tail-pin makes the assertion easier to read.)
    do_pin().await;

    // ── Step 6: assert sidecar and runtime cache agree on the final
    // state. Sidecar says pinned; runtime PinnedSet should contain
    // the CID. The PinnedSet query is a verb routed through the same
    // verb_tx, so by the time the reply arrives, every previously
    // dispatched verb (including the tail-pin above) has been
    // processed — the runtime cache is now stable.
    let sidecar_pinned = index.lock().unwrap().get(&sid).expect("entry").pinned;

    let (snap_tx, snap_rx) = oneshot::channel();
    content_verb_tx
        .send(ContentVerbRequest::PinnedSet { reply: snap_tx })
        .await
        .unwrap();
    let runtime_pinned_set = snap_rx.await.unwrap();
    let runtime_pinned = runtime_pinned_set.contains(&cid_bytes);

    assert_eq!(
        sidecar_pinned, runtime_pinned,
        "ZEB-160: sidecar (pinned={sidecar_pinned}) and runtime cache \
         (pinned={runtime_pinned}) must agree after rapid toggle. \
         Pre-fix, the IPC's send-after-unlock pattern let interleaved \
         verbs reach the event loop in non-sidecar order, breaking \
         this invariant non-deterministically."
    );
    assert!(
        sidecar_pinned,
        "tail-pin (Step 5) explicitly leaves the sidecar pinned"
    );
}
