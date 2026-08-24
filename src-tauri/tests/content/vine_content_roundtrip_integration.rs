//! ZEB-286: Heavy NodeRuntime-spin-up tests for the Vine content
//! round-trip (ingest_content on creator → fetch_content on recipient).
//!
//! Modeled on `content_index_integration.rs`. NodeRuntime is `!Send`, so it
//! must be constructed INSIDE the dedicated OS thread that runs the
//! event loop — the same pattern as lib.rs::start_node. The outer
//! `#[tokio::test]` runtime drives the channel interactions; the inner
//! thread constructs NodeRuntime and runs `event_loop::run` in its own
//! tokio runtime.
//!
//! # Architecture
//!
//! ONE NodeRuntime acts as both creator and recipient CAS. The descriptor
//! flow between conceptual peers is modeled with two in-process
//! `VineFeedCache` instances (no real Zenoh transport involved).
//!
//! Content ingest uses the `IngestRequest` channel (same path as the
//! production `ingest_content` Tauri command). Content retrieval uses
//! `ContentVerbRequest::ReadBytes` which reads directly from the
//! StorageTier cache — equivalent to what a recipient peer would
//! expose after a successful CAS fetch completes. The descriptor flow
//! is exercised via `VineFeedCache::on_descriptor_sample` directly in
//! the main test thread.
//!
//! Full design: `docs/specs/2026-05-13-zeb-286-vine-integration-test-design.md`.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use harmony_app::event_loop::{ContentVerbRequest, IngestRequest};
use harmony_app::vine_feed_cache::{DescriptorOutcome, VineFeedCache};
use harmony_app::{VineDescriptorPayload, VineVideoDto};
use harmony_compute::InstructionBudget;
use harmony_content::book::MemoryBookStore;
use harmony_content::cid::{ContentFlags, ContentId};
use harmony_content::storage_tier::{ContentPolicy, FilterBroadcastConfig, StorageBudget};
use harmony_runtime::{NodeConfig, NodeRuntime};
use tokio::sync::{mpsc, oneshot, watch};

// ── Shared helpers ────────────────────────────────────────────────────────────

/// Minimal `NodeConfig` that matches the sibling test pattern. All identity
/// fields are zeroed/empty; no disk, archive, or S3.
fn make_node_config() -> NodeConfig {
    NodeConfig {
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
    }
}

/// Build a minimal `VineDescriptorPayload` for use in cache tests.
/// `creator` is a signer NAME — translated to its derived address and
/// creator-signed (ZEB-673 strict wire admission).
fn make_vine_descriptor(vine_id: &str, creator: &str, video_cid_hex: &str) -> Vec<u8> {
    let mut payload = VineDescriptorPayload {
        id: vine_id.to_string(),
        creator_address: crate::vine_signing_testutil::addr(creator),
        creator_name: "Test Creator".to_string(),
        // created_at is SECONDS (compared against now_ms/1000). The now_ms args
        // below are ms (~2023); this is the matching seconds value.
        created_at: 1_700_000_000,
        video_cid: video_cid_hex.to_string(),
        title: Some("Test Vine".to_string()),
        reshare_of: None,
        original_creator_address: None,
        original_creator_name: None,
        identity_pub: None,
        sig: None,
        device_sig: None,
    };
    harmony_app::vine_signing::sign_descriptor(
        &crate::vine_signing_testutil::signer_for(creator),
        &mut payload,
    );
    serde_json::to_vec(&payload).expect("descriptor serialization")
}

/// Spawn a NodeRuntime event loop on a dedicated OS thread (NodeRuntime is
/// `!Send`). Returns the channels used to drive the loop from test code,
/// plus a `oneshot::Receiver<Result<(), String>>` that fires when startup
/// completes.
///
/// All unused channel senders are prefixed with `_` and held by the caller
/// so the event loop's receive arms don't see unexpected closes mid-test.
struct EventLoopHandles {
    ingest_tx: mpsc::Sender<IngestRequest>,
    content_verb_tx: mpsc::Sender<ContentVerbRequest>,
    #[allow(dead_code)]
    cas_op_tx: mpsc::Sender<harmony_app::content_store::CasOp>,
    ready_rx: oneshot::Receiver<Result<(), String>>,
    /// Held alive so the event loop doesn't shut down prematurely.
    _shutdown_tx: watch::Sender<bool>,
    /// All unused sender halves — kept alive so the event loop select arms
    /// don't see immediate channel close.
    _unused: UnusedSenders,
    /// Keeps the test's app_data_dir alive across the event loop thread's
    /// lifetime. Without this, the TempDir destructor deletes the
    /// directory as soon as `spawn_event_loop` returns, even though
    /// the spawned thread continues to read/write under it.
    _tmp: tempfile::TempDir,
}

#[allow(dead_code)]
struct UnusedSenders {
    publish_tx: mpsc::Sender<harmony_app::event_loop::PublishRequest>,
    fetch_tx: mpsc::Sender<harmony_app::event_loop::FetchRequest>,
    follow_tx: mpsc::Sender<harmony_app::event_loop::FollowRequest>,
    voice_tx: mpsc::Sender<harmony_app::voice::VoiceOutbound>,
    voice_ch_tx: mpsc::Sender<harmony_app::voice::VoiceChannelRequest>,
    refresh_tx: mpsc::Sender<harmony_app::mail_sync::RefreshRequest>,
    /// Disconnected dummy sender. The real `fetch_completion_tx` was
    /// moved into `event_loop::run`; this field exists only as a slot
    /// keeping `UnusedSenders` symmetric with the event_loop's
    /// channel set. Sending on it succeeds but the bytes go nowhere.
    fetch_completion_tx: mpsc::Sender<[u8; 32]>,
}

fn spawn_event_loop(
    thread_name: &str,
    config: NodeConfig,
    vine_feed_cache: Arc<Mutex<VineFeedCache>>,
) -> EventLoopHandles {
    let tmp = tempfile::tempdir().expect("tempdir");
    let app_data_dir = tmp.path().to_path_buf();

    let (ingest_tx, ingest_rx) = mpsc::channel::<IngestRequest>(4);
    let (content_verb_tx, content_verb_rx) = mpsc::channel::<ContentVerbRequest>(16);
    let (publish_tx, publish_rx) = mpsc::channel(4);
    let (fetch_tx, fetch_rx) = mpsc::channel(4);
    let (follow_tx, follow_rx) = mpsc::channel(4);
    let (voice_tx, voice_rx) = mpsc::channel::<harmony_app::voice::VoiceOutbound>(4);
    let (voice_ch_tx, voice_ch_rx) = mpsc::channel::<harmony_app::voice::VoiceChannelRequest>(4);
    let (refresh_tx, refresh_rx) = mpsc::channel::<harmony_app::mail_sync::RefreshRequest>(4);
    let (cas_op_tx, cas_op_rx) = mpsc::channel::<harmony_app::content_store::CasOp>(8);
    let (ready_tx, ready_rx) = oneshot::channel();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (fetch_completion_tx, fetch_completion_rx) = mpsc::channel::<[u8; 32]>(4);

    let followed_set = Arc::new(Mutex::new(HashSet::<String>::default()));
    let mail_mgr = Arc::new(Mutex::new(harmony_app::mail::MailManager::load(
        Some(&harmony_app::device_dataset_file::test_cipher()),
        &app_data_dir.join("mail"),
        [0u8; 16],
    )));

    // ZEB-445: event_loop::run takes a mode-agnostic NodeEventSink; this
    // test never asserts on emissions, so an empty fan-out is sufficient.
    let event_sink: Arc<dyn harmony_app::node_event_sink::NodeEventSink> =
        Arc::new(harmony_app::node_event_sink::FanoutSink(vec![]));

    let pin_intent: HashSet<[u8; 32]> = HashSet::new();
    let cas_op_tx_clone = cas_op_tx.clone();

    thread::Builder::new()
        .name(thread_name.to_string())
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
                    None, // endpoint — no external Zenoh endpoint needed
                    ready_tx,
                    shutdown_rx,
                    publish_rx,
                    fetch_rx,
                    ingest_rx,
                    content_verb_rx,
                    cas_op_tx_clone,
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
                    None, // ZEB-884: profile_card_publisher not exercised in this test
                    None, // ZEB-537: community_presence_request_rx not exercised in this test
                    std::sync::Arc::new(tokio::sync::Mutex::new(
                        harmony_app::community_presence::CommunityPresenceMap::new(),
                    )), // ZEB-537: community_presence_map (throwaway; presence not exercised here)
                    None, // ZEB-815: addrbook_runtime not exercised in this test
                    None, // Mint Phase 2 sync: not exercised in this test
                    None, // ZEB-417 SP1: notes_sync_handles not exercised in this test
                    None, // ZEB-418 P1: dm_inbox_sync_handles not exercised in this test
                    None, // ZEB-418 P2: p2_sync_handles not exercised in this test
                    None, // ZEB-458 P4 B: relay_sync_handles not exercised in this test
                    None, // ZEB-668 S1: trust_sync_handles not exercised in this test
                    None, // ZEB-677 S3: quorum_sync_handles not exercised in this test
                    None, // ZEB-668 S5: fleet_keys_sync_handles not exercised in this test
                    None, // ZEB-495: community_device_intro_sync_handles not exercised in this test
                    None, // ZEB-977: contacts_sync_handles not exercised in this test
                    None, // ZEB-321 Phase 1 Task 8: iroh handles not exercised in this test
                    None, // ZEB-373: dial telemetry not exercised in this test
                    harmony_app::content_store::CommunityServeAllowlist::new(), // ZEB-395: empty allowlist (no community roots published in this test)
                    None, // ZEB-418 P2: routing_republish not exercised
                    tokio::sync::watch::channel(0u64).0, // ZEB-434: transport-epoch watch not exercised
                    std::sync::Arc::new(harmony_app::network_health::ZenohTransportPeers::new()), // ZEB-971: watchdog demand cache not exercised
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
                        harmony_app::content_index::ContentIndex::load(
                            None,
                            std::path::Path::new(""),
                        ),
                    )),
                    // ZEB-669 S2: buddy records/ledger/settings not exercised
                    std::sync::Arc::new(std::sync::Mutex::new(
                        harmony_app::storage_records::StorageRecordStore::new(None, None),
                    )),
                    std::sync::Arc::new(std::sync::Mutex::new(
                        harmony_app::storage_ledger::StorageLedger::new(None, None),
                    )),
                    std::sync::Arc::new(std::sync::Mutex::new(
                        harmony_app::storage_settings::StorageSettings::default(),
                    )),
                    String::new(), // ZEB-669 S2: no owner ⇒ engine tick no-ops
                    harmony_app::revoked_device_projection::RevokedDeviceProjection::new(), // ZEB-679: revocation not exercised
                )
                .await;
            });
        })
        .expect("spawn runtime thread");

    EventLoopHandles {
        ingest_tx,
        content_verb_tx,
        cas_op_tx,
        ready_rx,
        _shutdown_tx: shutdown_tx,
        _tmp: tmp,
        _unused: UnusedSenders {
            publish_tx,
            fetch_tx,
            follow_tx,
            voice_tx,
            voice_ch_tx,
            refresh_tx,
            fetch_completion_tx: {
                // The channel was already moved into the event loop thread;
                // we create a dummy one just to satisfy the struct field.
                // The actual fetch_completion_tx is held by the event loop.
                // Instead, return a disconnected sender clone by creating a
                // fresh channel and dropping the receiver.
                let (tx, _rx) = mpsc::channel(1);
                tx
            },
        },
    }
}

/// Await the event-loop ready signal, panicking on any start failure.
///
/// ZEB-446 made the Reticulum bind degradable (a 4242 collision warns and falls
/// back to an ephemeral loopback bind), so `run()` no longer returns an "Address
/// already in use" error. A real start failure now fails loudly instead of being
/// silently skipped (ZEB-420).
async fn await_ready(ready_rx: oneshot::Receiver<Result<(), String>>) {
    match ready_rx.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => panic!("event loop failed to start: {e}"),
        Err(_) => panic!("event loop dropped ready signal"),
    }
}

// ── Test 1: creator ingests video, recipient fetches bytes ────────────────────

/// End-to-end vine CAS round-trip:
///
/// 1. Creator ingests vine-sized video bytes via `IngestRequest`.
/// 2. The descriptor (with the ingested CID) is fed to the recipient's
///    `VineFeedCache` via `on_descriptor_sample`.
/// 3. Recipient calls `ContentVerbRequest::ReadBytes` (production CAS
///    fetch path) and gets the same bytes back.
/// 4. `VineFeedCache::list_descriptors()` returns the vine with the
///    matching `video_cid`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn creator_ingests_video_recipient_fetches_bytes() {
    // ── Fixture: vine-sized video bytes (64 KiB deterministic pattern) ──
    let video_bytes: Vec<u8> = (0..64 * 1024).map(|i| ((i * 31) % 251) as u8).collect();
    let cid = ContentId::for_book(&video_bytes, ContentFlags::default())
        .expect("CID for vine fixture bytes");
    let cid_bytes: [u8; 32] = cid.to_bytes();
    let cid_hex = hex::encode(cid_bytes);

    // ── Recipient cache (in-process, no Zenoh) ───────────────────────────
    let recipient_cache = Arc::new(Mutex::new(VineFeedCache::new()));

    let handles = spawn_event_loop(
        "vine-roundtrip-test",
        make_node_config(),
        Arc::clone(&recipient_cache),
    );

    await_ready(handles.ready_rx).await;

    // ── Step 1: ingest via IngestRequest (creator side) ─────────────────
    let (ack_tx, ack_rx) = oneshot::channel();
    handles
        .ingest_tx
        .send(IngestRequest {
            cid_hex: cid_hex.clone(),
            data: video_bytes.clone(),
            serveable: false,
            reply: ack_tx,
        })
        .await
        .unwrap();
    ack_rx
        .await
        .expect("event loop dropped ingest reply")
        .expect("ingest failed");

    // ── Step 2: model descriptor arrival on recipient cache ──────────────
    let vine_id = "test-vine-001";
    let creator_addr = crate::vine_signing_testutil::addr("rt-creator");
    let descriptor_payload = make_vine_descriptor(vine_id, "rt-creator", &cid_hex);
    let topic = format!("harmony/vines/{creator_addr}");
    let followed: HashSet<String> = HashSet::new();

    let outcome = recipient_cache.lock().unwrap().on_descriptor_sample(
        &topic,
        &descriptor_payload,
        &followed,
        1_700_000_001_000,
    );
    assert!(
        matches!(outcome, Some(DescriptorOutcome::Inserted { .. })),
        "descriptor should insert on first arrival; got: {outcome:?}"
    );

    // ── Step 3: recipient reads bytes via ReadBytes (CAS fetch) ──────────
    let (read_tx, read_rx) = oneshot::channel();
    handles
        .content_verb_tx
        .send(ContentVerbRequest::ReadBytes {
            cid: cid_bytes,
            reply: read_tx,
        })
        .await
        .unwrap();
    let fetched = read_rx
        .await
        .expect("event loop dropped ReadBytes reply")
        .expect("CID not found in cache after ingest — CAS round-trip failed");
    assert_eq!(
        fetched, video_bytes,
        "fetched bytes must match ingested video bytes"
    );

    // ── Step 4: list_descriptors returns the vine with the correct CID ───
    let dtos: Vec<VineVideoDto> = recipient_cache.lock().unwrap().list_descriptors();
    assert_eq!(
        dtos.len(),
        1,
        "exactly one vine should be in recipient cache"
    );
    assert_eq!(
        dtos[0].video_cid, cid_hex,
        "descriptor video_cid must match ingested CID"
    );
    assert_eq!(dtos[0].id, vine_id, "descriptor id must match vine_id");
}

// ── Test 1b: the full vine round-trip in one flow ────────────────────────────

/// ZEB-546: walks the whole Vine round-trip a real user exercises, as one
/// deterministic flow — publish (creator ingests the video into CAS) → the
/// descriptor lands in the recipient's feed → the recipient marks it viewed
/// (persists, local-only, idempotent) → the recipient fetches the video bytes
/// via CAS → the recipient reshares it and the reshare carries reshare-of +
/// original-creator attribution. The descriptor hop is modeled in-process (no
/// live Zenoh transport — see the module header); the content and cache paths
/// are the real production channels. This is a single named guard for the
/// integration of legs otherwise asserted in isolation across the vine suite.
/// (A live two-engine Zenoh descriptor-propagation run is tracked separately.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vine_full_round_trip_publish_feed_view_fetch_reshare() {
    let video_bytes: Vec<u8> = (0..32 * 1024).map(|i| ((i * 17) % 251) as u8).collect();
    let cid = ContentId::for_book(&video_bytes, ContentFlags::default())
        .expect("CID for vine fixture bytes");
    let cid_bytes: [u8; 32] = cid.to_bytes();
    let cid_hex = hex::encode(cid_bytes);

    let recipient_cache = Arc::new(Mutex::new(VineFeedCache::new()));
    let handles = spawn_event_loop(
        "vine-full-round-trip-test",
        make_node_config(),
        Arc::clone(&recipient_cache),
    );
    await_ready(handles.ready_rx).await;

    // ── 1. Publish: creator ingests the video into CAS ───────────────────
    let (ack_tx, ack_rx) = oneshot::channel();
    handles
        .ingest_tx
        .send(IngestRequest {
            cid_hex: cid_hex.clone(),
            data: video_bytes.clone(),
            serveable: false,
            reply: ack_tx,
        })
        .await
        .unwrap();
    ack_rx
        .await
        .expect("event loop dropped ingest reply")
        .expect("ingest failed");

    // ── 2. Feed: the descriptor lands in the recipient's feed ────────────
    let vine_id = "rt-vine-001";
    let creator_addr = crate::vine_signing_testutil::addr("rt-creator");
    let followed: HashSet<String> = HashSet::new(); // creator not followed → discover bucket
    let outcome = recipient_cache.lock().unwrap().on_descriptor_sample(
        &format!("harmony/vines/{creator_addr}"),
        &make_vine_descriptor(vine_id, "rt-creator", &cid_hex),
        &followed,
        1_700_000_001_000,
    );
    assert!(
        matches!(outcome, Some(DescriptorOutcome::Inserted { .. })),
        "descriptor should insert on first arrival; got {outcome:?}"
    );
    {
        let dtos = recipient_cache.lock().unwrap().list_descriptors();
        assert_eq!(dtos.len(), 1, "exactly the published vine is in the feed");
        assert_eq!(dtos[0].id, vine_id);
        assert_eq!(dtos[0].video_cid, cid_hex);
        assert!(!dtos[0].viewed, "freshly arrived vine starts unviewed");
    }

    // ── 3. View: mark-viewed persists, idempotent, local-only ────────────
    assert!(
        recipient_cache
            .lock()
            .unwrap()
            .mark_viewed(vine_id.to_string()),
        "first mark_viewed returns true"
    );
    assert!(
        !recipient_cache
            .lock()
            .unwrap()
            .mark_viewed(vine_id.to_string()),
        "repeat mark_viewed is idempotent (no double-count)"
    );
    assert!(
        recipient_cache.lock().unwrap().list_descriptors()[0].viewed,
        "viewed state persists on the descriptor"
    );

    // ── 4. Fetch: the recipient reads the video bytes back via CAS ───────
    let (read_tx, read_rx) = oneshot::channel();
    handles
        .content_verb_tx
        .send(ContentVerbRequest::ReadBytes {
            cid: cid_bytes,
            reply: read_tx,
        })
        .await
        .unwrap();
    let fetched = read_rx
        .await
        .expect("event loop dropped ReadBytes reply")
        .expect("CID absent after ingest — CAS round-trip failed");
    assert_eq!(
        fetched, video_bytes,
        "fetched bytes must match the published video"
    );

    // ── 5. Reshare: the reshare descriptor carries attribution to origin ─
    let resharer_addr = crate::vine_signing_testutil::addr("rt-resharer");
    let reshare_id = "rt-vine-reshare-001";
    let mut reshare_payload = VineDescriptorPayload {
        id: reshare_id.to_string(),
        creator_address: resharer_addr.clone(),
        creator_name: "Resharer".to_string(),
        // Seconds (see the descriptor above); 2 s after the first descriptor.
        created_at: 1_700_000_002,
        video_cid: cid_hex.clone(), // a reshare points at the same video CID
        title: None,
        reshare_of: Some(vine_id.to_string()),
        original_creator_address: Some(creator_addr.to_string()),
        original_creator_name: Some("Test Creator".to_string()),
        identity_pub: None,
        sig: None,
        device_sig: None,
    };
    harmony_app::vine_signing::sign_descriptor(
        &crate::vine_signing_testutil::signer_for("rt-resharer"),
        &mut reshare_payload,
    );
    let reshare_outcome = recipient_cache.lock().unwrap().on_descriptor_sample(
        &format!("harmony/vines/{resharer_addr}"),
        &serde_json::to_vec(&reshare_payload).expect("reshare descriptor serialization"),
        &followed,
        1_700_000_002_000,
    );
    assert!(
        matches!(reshare_outcome, Some(DescriptorOutcome::Inserted { .. })),
        "reshare descriptor should insert; got {reshare_outcome:?}"
    );

    let dtos = recipient_cache.lock().unwrap().list_descriptors();
    assert_eq!(dtos.len(), 2, "original + reshare both present in the feed");
    let reshare = dtos
        .iter()
        .find(|d| d.id == reshare_id)
        .expect("reshare present in feed");
    assert_eq!(
        reshare.reshare_of.as_deref(),
        Some(vine_id),
        "reshare links back to the original vine id"
    );
    assert_eq!(
        reshare.original_creator_address.as_deref(),
        Some(creator_addr.as_str()),
        "reshare attributes the original creator's address"
    );
    assert_eq!(
        reshare.original_creator_name.as_deref(),
        Some("Test Creator"),
        "reshare carries the original creator's display name"
    );
}

// ── Test 2: fetch for unknown CID returns Err (bounded timeout) ──────────────

/// Verifies that requesting bytes for a CID that was never ingested does NOT
/// hang forever. `ContentVerbRequest::ReadBytes` returns `None` for a cache
/// miss (the CID is absent). The test asserts `None` is returned within a
/// bounded wall-clock window.
///
/// This covers the frontend "no peer has this video" case: the Tauri command
/// receives `None`, maps it to an error, and the UI shows a graceful fallback.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fetch_content_for_unknown_cid_returns_err() {
    // Fabricate a CID that was never ingested.
    let phantom_bytes = b"this content was never ingested".to_vec();
    let phantom_cid = ContentId::for_book(&phantom_bytes, ContentFlags::default())
        .expect("CID for phantom bytes");
    let phantom_cid_bytes: [u8; 32] = phantom_cid.to_bytes();

    let vine_cache = Arc::new(Mutex::new(VineFeedCache::new()));
    let handles = spawn_event_loop(
        "vine-unknown-cid-test",
        make_node_config(),
        Arc::clone(&vine_cache),
    );

    await_ready(handles.ready_rx).await;

    // ReadBytes for a never-ingested CID must return None promptly.
    let (read_tx, read_rx) = oneshot::channel();
    handles
        .content_verb_tx
        .send(ContentVerbRequest::ReadBytes {
            cid: phantom_cid_bytes,
            reply: read_tx,
        })
        .await
        .unwrap();

    let result = tokio::time::timeout(Duration::from_secs(2), read_rx)
        .await
        .expect("ReadBytes for unknown CID must complete within 2s — must not hang")
        .expect("event loop dropped ReadBytes reply");

    assert!(
        result.is_none(),
        "ReadBytes for a never-ingested CID must return None; got Some({result:?})"
    );
}

// ── Test 3: descriptor arrives before content; retry after ingest succeeds ───

/// Production-realistic ordering where the vine descriptor arrives on the
/// recipient before the video bytes have been transferred (Zenoh pub/sub
/// vs CAS pull are independent). Models the retry behavior:
///
/// 1. Descriptor flows to recipient cache (content not yet present).
/// 2. First `ReadBytes` call: `None` (cache miss — content absent).
/// 3. Content bytes are ingested (simulating the CAS pull completing).
/// 4. Second `ReadBytes` call: `Some(bytes)` (cache hit — content present).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn descriptor_arrives_before_video_cid_resolves_fetch_content_retry() {
    // ── Fixture: video bytes + their CID ────────────────────────────────
    let video_bytes: Vec<u8> = (0..128 * 1024).map(|i| ((i * 17) % 251) as u8).collect();
    let cid = ContentId::for_book(&video_bytes, ContentFlags::default())
        .expect("CID for vine fixture bytes");
    let cid_bytes: [u8; 32] = cid.to_bytes();
    let cid_hex = hex::encode(cid_bytes);

    let recipient_cache = Arc::new(Mutex::new(VineFeedCache::new()));

    let handles = spawn_event_loop(
        "vine-retry-ordering-test",
        make_node_config(),
        Arc::clone(&recipient_cache),
    );

    await_ready(handles.ready_rx).await;

    // ── Step 1: descriptor arrives on recipient (content not yet present) ─
    let vine_id = "test-vine-002";
    let creator_addr = crate::vine_signing_testutil::addr("rt-creator-2");
    let descriptor_payload = make_vine_descriptor(vine_id, "rt-creator-2", &cid_hex);
    let topic = format!("harmony/vines/{creator_addr}");
    let followed: HashSet<String> = HashSet::new();

    let outcome = recipient_cache.lock().unwrap().on_descriptor_sample(
        &topic,
        &descriptor_payload,
        &followed,
        1_700_000_002_000,
    );
    assert!(
        matches!(outcome, Some(DescriptorOutcome::Inserted { .. })),
        "descriptor should insert before content lands; got: {outcome:?}"
    );

    // ── Step 2: first fetch attempt — content not yet ingested → None ────
    let (read_tx, read_rx) = oneshot::channel();
    handles
        .content_verb_tx
        .send(ContentVerbRequest::ReadBytes {
            cid: cid_bytes,
            reply: read_tx,
        })
        .await
        .unwrap();
    let first_result = tokio::time::timeout(Duration::from_secs(2), read_rx)
        .await
        .expect("first ReadBytes must complete within 2s")
        .expect("event loop dropped first ReadBytes reply");
    assert!(
        first_result.is_none(),
        "first fetch before ingest must return None (content not yet present)"
    );

    // ── Step 3: ingest the bytes (CAS pull completes) ────────────────────
    let (ack_tx, ack_rx) = oneshot::channel();
    handles
        .ingest_tx
        .send(IngestRequest {
            cid_hex: cid_hex.clone(),
            data: video_bytes.clone(),
            serveable: false,
            reply: ack_tx,
        })
        .await
        .unwrap();
    ack_rx
        .await
        .expect("event loop dropped ingest reply")
        .expect("ingest failed");

    // ── Step 4: second fetch attempt — content now present → Some ────────
    let (read_tx, read_rx) = oneshot::channel();
    handles
        .content_verb_tx
        .send(ContentVerbRequest::ReadBytes {
            cid: cid_bytes,
            reply: read_tx,
        })
        .await
        .unwrap();
    let second_result = tokio::time::timeout(Duration::from_secs(2), read_rx)
        .await
        .expect("second ReadBytes must complete within 2s")
        .expect("event loop dropped second ReadBytes reply");

    let fetched =
        second_result.expect("second fetch after ingest must return Some — CAS retry succeeded");
    assert_eq!(
        fetched, video_bytes,
        "re-fetched bytes must match the originally ingested video bytes"
    );

    // ── Step 5: cache still reflects the descriptor ───────────────────────
    let dtos: Vec<VineVideoDto> = recipient_cache.lock().unwrap().list_descriptors();
    assert_eq!(
        dtos.len(),
        1,
        "descriptor must persist in cache after content ingest"
    );
    assert_eq!(
        dtos[0].video_cid, cid_hex,
        "cached descriptor video_cid must still match"
    );
}
