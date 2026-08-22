//! Shared test harness for the `content_tests` binary (ZEB-183).
//!
//! Consolidates the `spawn_test_runtime` / `TestHarness` / `ingest_*` /
//! `make_leaf` / `make_entry` / `insert_top_level` / `fresh_index` boilerplate
//! that was copy-pasted verbatim across `move_content_integration.rs`,
//! `rename_content_integration.rs`, and `folder_primitive_integration.rs`
//! (and inlined five times in `content_index_integration.rs`). With one copy
//! here, an `event_loop::run` signature change or a `ContentIndexEntry` schema
//! bump has exactly one update site — the drift this ticket exists to prevent.
//!
//! Lives inside the content test binary (declared `mod harness;` in
//! `content_tests.rs`) rather than `tests/common/`: these names never cross the
//! binary boundary, so a within-binary module avoids recompiling the whole
//! runtime spinner into every other test binary.
#![allow(dead_code)] // Harness: not every helper is exercised by every consuming submodule.

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

/// Owns the channel senders + tempdir + shutdown sender for a spawned event
/// loop, and joins the runtime thread on drop so a subsequent test in the same
/// process can't race the still-draining channels.
pub struct TestHarness {
    pub ingest_tx: mpsc::Sender<IngestRequest>,
    pub verb_tx: mpsc::Sender<ContentVerbRequest>,
    /// ZEB-945: a clone of the fetch-completion sender the event loop consumes.
    /// Exposed so a test can synthetically inject a completion signal (the
    /// `fetch_complete_arm_pins_root_in_intent` site); the loop still owns the
    /// receiver, so the other consumers simply ignore this handle.
    pub fetch_completion_tx: mpsc::Sender<[u8; 32]>,
    _shutdown_tx: watch::Sender<bool>,
    _tmp: tempfile::TempDir,
    runtime_thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for TestHarness {
    fn drop(&mut self) {
        // Signal shutdown, then join the runtime thread. Joining is
        // load-bearing despite --test-threads=1: without it, the OS thread
        // keeps the event loop alive past TestHarness drop, and a subsequent
        // test in the same process can race against the still-draining
        // channels.
        let _ = self._shutdown_tx.send(true);
        if let Some(handle) = self.runtime_thread.take() {
            // Surface a runtime-thread panic as a test failure when the test is
            // otherwise green; downgrade to eprintln if Drop runs during
            // unwinding so we don't double-panic and abort the process.
            // resume_unwind preserves the original payload (and Backtrace, if
            // any) — strictly better than re-raising via panic!.
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

/// Spawn a `NodeRuntime` event loop on its own OS thread + tokio runtime and
/// return a [`TestHarness`] once it signals ready. `label` names the OS thread
/// and the runtime-build panic message (e.g. `"move"`, `"rename"`, `"folder"`).
///
/// All error paths panic (never returns `None`) so a real start failure is a
/// loud test failure, not a silent skip (the ZEB-165 / ZEB-420 anti-false-green
/// invariant).
///
/// The event loop starts with an empty `pin_intent`; use
/// [`spawn_test_runtime_with_pins`] when a test needs a root pre-seeded (ZEB-945).
pub async fn spawn_test_runtime(label: &str) -> TestHarness {
    spawn_test_runtime_with_pins(label, std::collections::HashSet::new()).await
}

/// Like [`spawn_test_runtime`], but seeds the event loop's `pin_intent` with
/// `initial_pins` — the set of root CIDs the node already intends to pin. The
/// fetch-completion arm only runs the pin cascade for CIDs it finds in this set,
/// so a test that injects a completion via [`TestHarness::fetch_completion_tx`]
/// must seed the corresponding CID here first (ZEB-945:
/// `fetch_complete_arm_pins_root_in_intent`).
pub async fn spawn_test_runtime_with_pins(
    label: &str,
    initial_pins: std::collections::HashSet<[u8; 32]>,
) -> TestHarness {
    let tmp = tempdir().unwrap();
    let app_data_dir = tmp.path().to_path_buf();

    let (ingest_tx, ingest_rx) = mpsc::channel::<IngestRequest>(8);
    // ZEB-945: verb buffer 64 (was 32) so the rapid_pin_unpin site — the one
    // runtime test that deliberately used 64 — converts onto this harness. A
    // bigger buffer only relaxes backpressure on these functional (not
    // backpressure) tests, so it's safe for the move/rename/folder sites too.
    let (verb_tx, content_verb_rx) = mpsc::channel::<ContentVerbRequest>(64);
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

    // ZEB-445: event_loop::run takes a mode-agnostic NodeEventSink; these tests
    // never assert on emissions, so an empty fan-out is sufficient.
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
    // ZEB-945: keep a clone for the returned TestHarness; the original is moved
    // into run() below (the loop owns both ends of this channel otherwise).
    let fetch_completion_tx_for_harness = fetch_completion_tx.clone();
    let pin_intent: std::collections::HashSet<[u8; 32]> = initial_pins;

    // `label` is a borrow, but the runtime thread's closure is `'static`; move an
    // owned copy in for the build-failure message.
    let rt_label = label.to_string();
    let runtime_thread = thread::Builder::new()
        .name(format!("harmony-runtime-{label}-test"))
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .thread_stack_size(8 * 1024 * 1024)
                .enable_all()
                .build()
                .unwrap_or_else(|e| panic!("tokio runtime for {rt_label} test event loop: {e}"));
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
                        // so the relay arm idles (not exercised in these tests).
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
                    None, // ZEB-703: owner_sync_engine — DM outbox not exercised in these tests
                    Vec::new(),
                    {
                        let (_tx, rx) = tokio::sync::mpsc::channel::<
                            harmony_app::event_loop::CommunityAdapterRequest,
                        >(1);
                        rx
                    },
                    {
                        // ZEB-298+ZEB-312 PR 1: voting-log adapter request channel;
                        // not exercised in these tests, tx dropped immediately.
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
                    None, // ZEB-341: profile_card_cache not exercised in these tests
                    None, // ZEB-341: profile_card_request_rx not exercised in these tests
                    None, // ZEB-884: profile_card_publisher not exercised in these tests
                    None, // ZEB-537: community_presence_request_rx not exercised in these tests
                    std::sync::Arc::new(tokio::sync::Mutex::new(
                        harmony_app::community_presence::CommunityPresenceMap::new(),
                    )), // ZEB-537: community_presence_map (throwaway; presence not exercised here)
                    None, // ZEB-815: addrbook_runtime not exercised in these tests
                    None, // Mint Phase 2 sync: not exercised in these tests
                    None, // ZEB-417 SP1: notes_sync_handles not exercised in these tests
                    None, // ZEB-418 P1: dm_inbox_sync_handles not exercised in these tests
                    None, // ZEB-418 P2: p2_sync_handles not exercised in these tests
                    None, // ZEB-458 P4 B: relay_sync_handles not exercised in these tests
                    None, // ZEB-668 S1: trust_sync_handles not exercised in these tests
                    None, // ZEB-677 S3: quorum_sync_handles not exercised in these tests
                    None, // ZEB-668 S5: fleet_keys_sync_handles not exercised in these tests
                    None, // ZEB-495: community_device_intro_sync_handles not exercised in these tests
                    None, // ZEB-321 Phase 1 Task 8: iroh handles not exercised in these tests
                    None, // ZEB-373: dial telemetry not exercised in these tests
                    harmony_app::content_store::CommunityServeAllowlist::new(), // ZEB-395: empty allowlist (no community roots published in these tests)
                    None, // ZEB-418 P2: routing_republish not exercised
                    tokio::sync::watch::channel(0u64).0, // ZEB-434: transport-epoch watch not exercised
                    std::sync::Arc::new(harmony_app::network_health::ZenohTransportPeers::new()), // ZEB-971: watchdog demand cache not exercised
                    Vec::new(), // ZEB-702 T3: republish_on_epoch — no engines exercised
                    tokio::sync::watch::channel(0u64).0, // ZEB-599: presence-resync watch not exercised
                    None, // ZEB-618: mail-root persist pair not exercised
                    None, // ZEB-621: addr_change_fanout not exercised
                    // ZEB-612 S3: announcements not exercised in these tests
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
                    harmony_app::revoked_device_projection::RevokedDeviceProjection::new(), // ZEB-679: revocation not exercised
                )
                .await;
            });
        })
        .expect("spawn runtime thread");

    match ready_rx.await {
        Ok(Ok(())) => {}
        // ZEB-446 made the Reticulum bind degradable (a 4242 collision warns and
        // falls back to an ephemeral loopback bind), so `run()` no longer returns
        // an "Address already in use" error — the old special-case arm is retired.
        // Any real start failure still panics loudly here rather than skipping
        // (the ZEB-165 / ZEB-420 anti-false-green invariant).
        Ok(Err(e)) => panic!("event loop failed to start: {e}"),
        Err(_) => panic!("event loop dropped ready signal"),
    }

    TestHarness {
        ingest_tx,
        verb_tx,
        fetch_completion_tx: fetch_completion_tx_for_harness,
        _shutdown_tx: shutdown_tx,
        _tmp: tmp,
        runtime_thread: Some(runtime_thread),
    }
}

/// Ingest a built folder's manifest + bundle through the runtime.
pub async fn ingest_folder(harness: &TestHarness, built: &folders::BuiltFolder) {
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

/// Ingest a single leaf's bytes through the runtime.
pub async fn ingest_leaf(harness: &TestHarness, cid: [u8; 32], bytes: Vec<u8>) {
    harmony_app::send_ingest(&harness.ingest_tx, hex::encode(cid), bytes, false)
        .await
        .unwrap();
}

/// Compute a leaf's book CID and return `(cid_bytes, bytes)`.
pub fn make_leaf(bytes: &[u8]) -> ([u8; 32], Vec<u8>) {
    let cid = ContentId::for_book(bytes, ContentFlags::default()).expect("for_book");
    (cid.to_bytes(), bytes.to_vec())
}

/// Build a `ContentIndexEntry` with sensible defaults for the boring fields
/// (`stored_at_ms = 1`, `Sensitivity::Private`, `ReplicationTier::Default`,
/// `licensed`/`archived`/`backup = false`, `origin = None`) and parameters for
/// the load-bearing ones. Future `ContentIndexEntry` schema additions land here
/// once; call sites that need a non-default field use struct-update syntax,
/// e.g. `ContentIndexEntry { archived: true, ..make_entry(...) }`.
pub fn make_entry(
    sidecar_id: SidecarId,
    cid: [u8; 32],
    file_name: &str,
    size_bytes: u64,
    kind: ContentKind,
    pinned: bool,
) -> ContentIndexEntry {
    ContentIndexEntry {
        sidecar_id,
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
    }
}

/// Insert a top-level sidecar entry that points at `cid`. Returns the minted
/// `SidecarId`.
pub fn insert_top_level(
    index: &Arc<Mutex<ContentIndex>>,
    cid: [u8; 32],
    file_name: &str,
    kind: ContentKind,
    pinned: bool,
    size_bytes: u64,
) -> SidecarId {
    let sid = SidecarId::new();
    let mut idx = index.lock().unwrap();
    let inserted = idx.insert(make_entry(sid, cid, file_name, size_bytes, kind, pinned));
    assert!(inserted, "fresh SidecarId must insert cleanly");
    sid
}

/// A fresh tempdir-backed `ContentIndex`. The `TempDir` is returned alongside
/// so callers keep it alive for the test — otherwise `ContentIndex::save`
/// writes vanish into a deleted directory and on-disk persistence regressions
/// slip past tests that only assert in-memory state.
pub fn fresh_index() -> (Arc<Mutex<ContentIndex>>, tempfile::TempDir) {
    let dir = tempdir().unwrap();
    let idx = ContentIndex::load(dir.path());
    (Arc::new(Mutex::new(idx)), dir)
}
