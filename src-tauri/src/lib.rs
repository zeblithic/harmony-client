use std::sync::Mutex;
use std::thread;

use harmony_compute::InstructionBudget;
use harmony_content::book::MemoryBookStore;
use harmony_content::storage_tier::{ContentPolicy, FilterBroadcastConfig, StorageBudget};
use harmony_runtime::{NodeConfig, NodeRuntime};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

pub mod community_invite;
pub mod community_membership;
pub mod community_state_crdt;
pub mod community_state_persist;
pub mod community_state_sync;
pub mod content_index;
pub mod content_store;
pub mod dm_crypto;
pub mod dm_envelope;
pub mod dm_outbox;
pub mod dm_signing;
pub mod event_loop;
pub mod folders;
mod follows;
pub mod identity;
pub mod identity_commands;
pub mod inbound_packet;
pub mod mail;
pub mod mail_sync;
pub mod owner_commands;
pub mod owner_state;
pub mod owner_state_crdt;
pub mod owner_state_crypto;
pub mod owner_state_persist;
pub mod owner_state_sync;
pub mod owner_state_types;
pub mod pairing;
pub mod pairing_commands;
pub mod recovery_cli;
pub mod recovery_policy;
mod save_dialog;
pub mod voice;

/// ZEB-262 Phase 4 Task 9: production impl of
/// `community_invite::AppHandleEmit` on `tauri::AppHandle<R>`. Lets
/// `community_invite::handle_unicast` emit
/// `community-state-sync-degraded` events without depending on `tauri`
/// directly (the trait + unit-type stub live in `community_invite.rs`
/// so tests can compile without a Tauri runtime).
impl<R: tauri::Runtime> crate::community_invite::AppHandleEmit for tauri::AppHandle<R> {
    fn emit_degraded(&self, community_id_hex: &str, reason_tag: &'static str) {
        let _ = self.emit(
            "community-state-sync-degraded",
            serde_json::json!({
                "communityId": community_id_hex,
                "reason": reason_tag,
            }),
        );
    }
}

// ── Chunked ingest (ZEB-154) ──────────────────────────────────────────────

/// Maximum bytes supported by the v1 flat-bundle chunked-ingest path.
///
/// Derived from the chunker's **minimum** chunk size — not the payload
/// maximum — because FastCDC with `ChunkerConfig::DEFAULT` emits at most
/// `ceil(N / min_chunk)` chunks. Using `min_chunk` guarantees the leaf
/// count can never exceed `MAX_BUNDLE_ENTRIES`, so `BundleBuilder` never
/// fails with a confusing "bundle full" error just below the true cap.
///
/// With the current defaults (MAX_BUNDLE_ENTRIES ≈ 32 767, min_chunk =
/// 256 KiB) this lands at ~8 GiB. Files larger than this need nested
/// bundles, which land with folder/directory support (ZEB-156 et al).
/// A flat-bundle-only v1 is intentional; see
/// docs/specs/2026-04-23-chunked-ingest-design.md (Q1).
pub(crate) const FLAT_BUNDLE_MAX: u64 = (harmony_content::bundle::MAX_BUNDLE_ENTRIES as u64)
    * (harmony_content::chunker::ChunkerConfig::DEFAULT.min_chunk as u64);

/// Dispatch decision for `ingest_content`, derived purely from file size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IngestDispatch {
    /// File fits in a single `for_book` CID — use the existing path.
    Single,
    /// File is larger than `MAX_PAYLOAD_SIZE` and must be chunked through
    /// the FastCDC chunker into a root bundle.
    Chunked,
}

/// Classify a file size into an ingest strategy, or return an error message
/// suitable for surfacing to the frontend if the file exceeds the v1 cap.
pub(crate) fn ingest_dispatch(size: u64) -> Result<IngestDispatch, String> {
    if size > FLAT_BUNDLE_MAX {
        return Err(format!(
            "file too large ({} bytes). v1 flat-bundle cap is {} bytes (~8 GiB). \
             Support for larger files lands with folder/nested-bundle support.",
            size, FLAT_BUNDLE_MAX
        ));
    }
    if size > harmony_content::cid::MAX_PAYLOAD_SIZE as u64 {
        Ok(IngestDispatch::Chunked)
    } else {
        Ok(IngestDispatch::Single)
    }
}

/// Chunk `bytes` via FastCDC and assemble the resulting leaf CIDs into a
/// flat bundle. Returns the ordered leaf (CID, slice) pairs, the raw bundle
/// payload, and the root bundle CID.
///
/// The caller is responsible for driving each `(cid, bytes)` pair through
/// the runtime's ingest channel in order, and for one final ingest of the
/// bundle payload under the root CID.
///
/// Expects `bytes.len() > MAX_PAYLOAD_SIZE` — for smaller inputs use the
/// existing single-book path.
///
/// Visibility is `pub` rather than `pub(crate)` so the integration tests
/// under `src-tauri/tests/` can drive the chunk + bundle construction
/// directly. `pub(crate)` would hide the symbol from the external test
/// crate and break `content_index_integration::chunked_ingest_pin_cascade_
/// fetch_burn_roundtrip`. Treat this as crate-internal — no external
/// consumers are expected.
#[allow(clippy::type_complexity)] // pre-existing; tracked for cleanup
pub fn chunk_and_bundle(
    bytes: &[u8],
) -> Result<
    (
        Vec<(harmony_content::cid::ContentId, &[u8])>,
        Vec<u8>,
        harmony_content::cid::ContentId,
    ),
    String,
> {
    use harmony_content::bundle::BundleBuilder;
    use harmony_content::chunker::{chunk_all, ChunkerConfig};
    use harmony_content::cid::{ContentFlags, ContentId, MAX_PAYLOAD_SIZE};

    if bytes.len() <= MAX_PAYLOAD_SIZE {
        return Err(format!(
            "chunk_and_bundle requires input larger than MAX_PAYLOAD_SIZE ({} bytes); \
             got {} bytes — use the single-book path instead",
            MAX_PAYLOAD_SIZE,
            bytes.len()
        ));
    }

    let ranges =
        chunk_all(bytes, &ChunkerConfig::DEFAULT).map_err(|e| format!("chunker error: {e:?}"))?;

    let mut leaves: Vec<(ContentId, &[u8])> = Vec::with_capacity(ranges.len());
    for range in ranges {
        let chunk = &bytes[range];
        let cid = ContentId::for_book(chunk, ContentFlags::default())
            .map_err(|e| format!("leaf CID error: {e:?}"))?;
        leaves.push((cid, chunk));
    }

    let mut builder = BundleBuilder::new();
    for (cid, _) in &leaves {
        builder.add(*cid);
    }
    let (bundle_payload, root) = builder
        .build_with_flags(ContentFlags::default())
        .map_err(|e| format!("bundle build error: {e:?}"))?;

    Ok((leaves, bundle_payload, root))
}

// ── Managed Tauri state ──────────────────────────────────────────────────

pub struct NodeState {
    /// Background thread running the event loop (NodeRuntime is !Send).
    thread: Option<thread::JoinHandle<()>>,
    /// Send `true` to shut down the event loop.
    shutdown_tx: Option<tokio::sync::watch::Sender<bool>>,
    /// Channel for routing publish requests through the event loop's session.
    publish_tx: Option<tokio::sync::mpsc::Sender<event_loop::PublishRequest>>,
    /// Channel for routing content-fetch requests through the event loop's session.
    fetch_tx: Option<tokio::sync::mpsc::Sender<event_loop::FetchRequest>>,
    /// Channel for routing content-ingest requests through the event loop.
    ingest_tx: Option<tokio::sync::mpsc::Sender<event_loop::IngestRequest>>,
    /// Channel for routing content verb (pin/unpin/burn) requests through the event loop.
    content_verb_tx: Option<tokio::sync::mpsc::Sender<event_loop::ContentVerbRequest>>,
    /// Channel for routing follow/unfollow requests through the event loop.
    follow_tx: Option<tokio::sync::mpsc::Sender<event_loop::FollowRequest>>,
    /// Channel for sending outbound voice frames to the event loop.
    voice_tx: Option<tokio::sync::mpsc::Sender<voice::VoiceOutbound>>,
    /// Channel for voice channel join/leave requests.
    voice_channel_tx: Option<tokio::sync::mpsc::Sender<voice::VoiceChannelRequest>>,
    /// Persistent follow manager (disk-backed follow list).
    follow_mgr: Option<follows::FollowManager>,
    /// Shared set of followed addresses (read by the event loop for source tagging).
    followed_set: Option<std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>>,
    /// Shared mail manager (read/written by event loop on receive, by commands for queries).
    mail_mgr: Option<std::sync::Arc<std::sync::Mutex<mail::MailManager>>>,
    /// Shared mail sync (walker + lazy body fetch). Stored here so Tauri
    /// commands (refresh_mail, fetch_mail_body) can reach it.
    mail_sync: Option<std::sync::Arc<mail_sync::MailSync>>,
    /// Disk-backed content index (pin/replication metadata).
    content_index: std::sync::Arc<std::sync::Mutex<content_index::ContentIndex>>,
    /// Monotonic connection generation (prevents stale stop_node races).
    generation: u64,
    /// Hex-encoded node address (set on startup, used to stamp outgoing messages).
    node_addr: String,
    /// ZEB-197 v2 pairing state-machine handle. `Some` while the node is
    /// running; the inner task drives the abstract pairing state machine
    /// against a `ZenohPairingTransport` bound to the running event loop.
    pairing_handle: Option<crate::pairing::state_machine::PairingHandle>,
    /// Phase 3a SyncEngine — `Some` while the node is running and an
    /// owner identity (master_seed) is available. Shutdown is called
    /// explicitly in `stop_inner` before the event-loop thread is joined.
    sync_engine: Option<std::sync::Arc<crate::owner_state_sync::SyncEngine>>,
    /// ZEB-217 Sub-C Phase 2: registry of per-community state-CRDT
    /// SyncEngines. Lifted from start_node (mirrors `sync_engine`
    /// above) so Phase 3 IPC handlers (create_community,
    /// redeem_invite, leave_community, list_community_members) can
    /// reach the engine pool without holding the per-engine Arcs
    /// directly. Shared with the event-loop ONLY through the
    /// per-community `CommunityAdapterRequest`s passed at startup;
    /// the registry itself is owned exclusively by NodeState.
    /// Shutdown (`registry.shutdown_all()`) is awaited explicitly in
    /// `stop_inner` BEFORE the event-loop thread is joined so each
    /// engine's final flush + persist runs while the Zenoh session
    /// (and thus the publisher) is still live.
    community_registry: Option<std::sync::Arc<crate::community_state_sync::CommunitySyncRegistry>>,
    /// Sender side of the community delta channel — kept for stop_node /
    /// restart to drop on shutdown so the consumer task winds down
    /// cleanly. The receiver was moved into the consumer task at
    /// start_node time; this Sender is the only handle on the
    /// engine-side senders' clone-source. Dropping closes the channel
    /// after every per-engine clone has also been dropped (which
    /// happens after `registry.shutdown_all()`).
    community_delta_tx:
        Option<tokio::sync::mpsc::Sender<crate::community_state_sync::CommunityMembershipDelta>>,
    /// ZEB-225 Sub-B Phase 2: per-process DM outbox state. Constructed in
    /// start_node alongside the SyncEngine; shared with the IPC handler
    /// (send_dm) and the event-loop drain tick.
    dm_outbox: Option<std::sync::Arc<tokio::sync::Mutex<crate::dm_outbox::DmOutbox>>>,
    /// Phase 2: in-process StubTransport. Phase 3b replaces with a real
    /// adapter that pushes RuntimeAction::SendUnicastToDevice.
    dm_transport: Option<std::sync::Arc<dyn crate::dm_outbox::DmTransport>>,
    /// CRDT state Mutex (already constructed for SyncEngine; we hold a
    /// clone so the IPC handler can lock it independently of SyncEngine).
    /// Stored as Option because identity-restore can null out everything.
    crdt_state: Option<std::sync::Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>>,
    /// HLC tracker (mirror of SyncEngine's tracker; the dm_outbox handler
    /// reads/writes the local device's entry to keep send_dm's HLCs
    /// monotone with state-root publishes).
    hlc_tracker: Option<
        std::sync::Arc<
            tokio::sync::Mutex<std::collections::BTreeMap<String, crate::owner_state_types::Hlc>>,
        >,
    >,
    /// Local device_id string + self OwnerAddr — captured at start_node
    /// time, snapshot for IPC handlers that mint OutboxEntry / HLC stamps.
    dm_device_id: Option<String>,
    dm_self_owner: Option<crate::owner_state_types::OwnerAddr>,
    /// ContentStore handle — same `Arc` SyncEngine was constructed with.
    /// Lifted onto NodeState so send_dm can write blobs through the same
    /// store SyncEngine uses for state-root publishes (RuntimeContentStore
    /// in production, InMemoryStub in some tests).
    content_store: Option<std::sync::Arc<dyn crate::content_store::ContentStore>>,
    /// ZEB-227 Path B: outbound DM unicast channel sender.
    /// `RuntimeUnicastTransport` (Task 6) holds a clone; `event_loop` drains
    /// the receiver and forwards each `UnicastSendRequest` as
    /// `RuntimeEvent::SendUnicastToDevice`. Cleared on stop_node so a
    /// restart's transport doesn't carry a stale sender.
    unicast_send_tx: Option<tokio::sync::mpsc::Sender<crate::dm_outbox::UnicastSendRequest>>,
    /// ZEB-228 Phase 4: 64-byte combined `identity_pub` for our local
    /// device (X25519_pub(32) || Ed25519_pub(32) per
    /// `harmony_identity::Identity::to_public_bytes()`). Captured in
    /// `start_node` before the in-memory `PrivateIdentity` is dropped, so
    /// `add_space` can ship it as the bootstrap pubkey on outbound
    /// `DmInvite` packets without re-deriving from the private bytes.
    /// Cleared on stop_node so a stale pub never leaks into a new
    /// identity's invites.
    dm_identity_pub_64: Option<[u8; 64]>,
    /// ZEB-217 Sub-C Phase 3 Task 9: sender used by IPC handlers
    /// (`create_community`, `redeem_invite`) to dispatch a
    /// `CommunityAdapterRequest` into the event loop, where it's
    /// drained from the `select!` and converted to a
    /// `spawn_community_state_zenoh_adapter` call against the live
    /// session. Decoupling the IPC from the session means the
    /// `Session` doesn't need to be reachable from `NodeState` — the
    /// event loop owns it exclusively. Cleared on stop_node so a
    /// restart's adapter requests don't dispatch to a dropped event
    /// loop's channel.
    community_adapter_request_tx:
        Option<tokio::sync::mpsc::Sender<crate::event_loop::CommunityAdapterRequest>>,
}

impl NodeState {
    /// True when the event-loop thread is running. Identity-restore IPCs
    /// refuse while the node is up, since the running NodeRuntime caches
    /// the old keys + zenoh subscriptions and would not pick up the new
    /// identity until restart (CodeRabbit round 5).
    pub fn is_running(&self) -> bool {
        self.thread.is_some()
    }
}

impl Default for NodeState {
    fn default() -> Self {
        Self {
            thread: None,
            shutdown_tx: None,
            publish_tx: None,
            fetch_tx: None,
            ingest_tx: None,
            content_verb_tx: None,
            follow_tx: None,
            voice_tx: None,
            voice_channel_tx: None,
            follow_mgr: None,
            followed_set: None,
            mail_mgr: None,
            mail_sync: None,
            content_index: std::sync::Arc::new(std::sync::Mutex::new(
                content_index::ContentIndex::load(std::path::Path::new("")),
            )),
            generation: 0,
            node_addr: String::new(),
            pairing_handle: None,
            sync_engine: None,
            community_registry: None,
            community_delta_tx: None,
            dm_outbox: None,
            dm_transport: None,
            crdt_state: None,
            hlc_tracker: None,
            dm_device_id: None,
            dm_self_owner: None,
            content_store: None,
            unicast_send_tx: None,
            dm_identity_pub_64: None,
            community_adapter_request_tx: None,
        }
    }
}

// ── Data types (shared with frontend via Tauri events) ───────────────────

/// Parsed capacity advertisement from a harmony-node.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CapacityUpdate {
    pub node_addr: String,
    pub model_cid: String,
    pub ready: bool,
    /// Hop distance derived from Zenoh routing: 1 = direct peer, 2 = via router.
    /// `None` when the publisher didn't include a ZenohId attachment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hop_distance: Option<u8>,
}

/// Zenoh connection status pushed to the frontend.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ZenohStatus {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Profile published to/received from the network.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfilePayload {
    pub address: String,
    pub display_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
    /// Hex-encoded CID for full-size avatar content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar_cid: Option<String>,
    /// Hex-encoded CID for thumbnail avatar content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar_mini_cid: Option<String>,
}

/// Channel message sent from the frontend.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendMessagePayload {
    /// Channel identifier (matches navNode id, e.g. "general").
    pub channel: String,
    /// Community/hub identifier (e.g. "harmony-dev").
    pub hub: String,
    pub text: String,
    pub priority: String,
    pub reply_to: Option<String>,
    /// Sender's display name (included in wire format so receivers can
    /// show it even before receiving a profile update).
    #[serde(default)]
    pub sender_name: String,
}

/// Channel message received from the network (emitted to frontend).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelMessagePayload {
    pub id: String,
    pub sender_address: String,
    pub sender_name: String,
    pub channel: String,
    pub hub: String,
    pub text: String,
    pub timestamp: u64,
    pub priority: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
}

/// Telemetry event pushed to the frontend via IPC.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TelemetryEventPayload {
    pub node_addr: String,
    pub intent: String,
    pub sequence: u64,
    pub timestamp: u64,
    pub payload: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

// ── Parsing helpers (used by event_loop.rs and tests) ────────────────────

const CAPACITY_PREFIX: &str = "harmony/compute/capacity/";

pub fn parse_capacity(key_expr: &str, payload: &[u8]) -> Option<CapacityUpdate> {
    let node_addr = key_expr.strip_prefix(CAPACITY_PREFIX)?;
    if payload.len() < 33 {
        return None;
    }
    let model_cid = hex::encode(&payload[..32]);
    let ready = payload[32] == 0x01;
    Some(CapacityUpdate {
        node_addr: node_addr.to_string(),
        model_cid,
        ready,
        hop_distance: None, // Set by emit_frontend_event after ZID matching
    })
}

pub fn parse_telemetry(wire: &[u8]) -> Option<TelemetryEventPayload> {
    let event = harmony_telemetry::decode_event(wire).ok()?;
    Some(TelemetryEventPayload {
        node_addr: event.node_addr,
        intent: event.intent,
        sequence: event.sequence,
        timestamp: event.timestamp,
        payload: event.payload,
        confidence: event.confidence,
        source: event.source,
    })
}

// ── Tauri commands ───────────────────────────────────────────────────────

/// Stop a node given its extracted handles (called outside the lock).
fn stop_handles(
    shutdown_tx: Option<tokio::sync::watch::Sender<bool>>,
    thread: Option<thread::JoinHandle<()>>,
) {
    if let Some(tx) = shutdown_tx {
        let _ = tx.send(true);
    }
    if let Some(thread) = thread {
        let _ = thread.join();
    }
}

/// Stop the running node (if any). Returns after the event loop thread exits.
/// Returns `true` if a node was actually stopped, `false` if it was a no-op.
fn stop_inner(state: &Mutex<NodeState>, expected_gen: Option<u64>) -> bool {
    let (
        shutdown_tx,
        thread,
        publish_tx,
        fetch_tx,
        ingest_tx,
        content_verb_tx,
        follow_tx,
        voice_tx,
        voice_channel_tx,
        _follow_mgr,
        _followed_set,
        _mail_sync,
        pairing_handle,
        sync_engine,
        community_registry,
        community_delta_tx,
        dm_outbox,
        dm_transport,
        crdt_state,
        hlc_tracker,
        dm_device_id,
        dm_self_owner,
        content_store,
        unicast_send_tx,
    ) = {
        let mut guard = match state.lock() {
            Ok(g) => g,
            Err(_) => return false,
        };
        if let Some(gen) = expected_gen {
            if guard.generation != gen {
                return false; // Stale stop — newer node exists
            }
        }
        guard.node_addr.clear();
        let tup = (
            guard.shutdown_tx.take(),
            guard.thread.take(),
            guard.publish_tx.take(),
            guard.fetch_tx.take(),
            guard.ingest_tx.take(),
            guard.content_verb_tx.take(),
            guard.follow_tx.take(),
            guard.voice_tx.take(),
            guard.voice_channel_tx.take(),
            guard.follow_mgr.take(),
            guard.followed_set.take(),
            // Drop mail_sync so refresh_mail / fetch_mail_body can't reach
            // a closed fetch_tx / refresh_tx after stop. Channels are
            // already gone above; the MailSync handle would just yield
            // "channel closed" errors until next start.
            guard.mail_sync.take(),
            guard.pairing_handle.take(),
            guard.sync_engine.take(),
            guard.community_registry.take(),
            guard.community_delta_tx.take(),
            guard.dm_outbox.take(),
            guard.dm_transport.take(),
            guard.crdt_state.take(),
            guard.hlc_tracker.take(),
            guard.dm_device_id.take(),
            guard.dm_self_owner.take(),
            guard.content_store.take(),
            guard.unicast_send_tx.take(),
        );
        // ZEB-228 Phase 4: clear our cached identity_pub so a restart
        // can't accidentally ship the prior identity's pub on a new
        // identity's invites. `[u8; 64]` is Copy, so just `take()` and
        // discard — no extra cleanup needed beyond the assignment.
        let _ = guard.dm_identity_pub_64.take();
        // ZEB-217 Sub-C Phase 3 Task 9: drop the on-demand
        // adapter-request sender. The event_loop's matching receiver
        // gets None on next recv(); the select arm exits cleanly.
        // Cleared even when the channel was unused (no
        // create_community calls in this lifetime) so a restart's
        // fresh `Sender` doesn't collide with a leaked one.
        let _ = guard.community_adapter_request_tx.take();
        tup
    };

    let had_node = shutdown_tx.is_some() || thread.is_some();
    // Drop the pairing handle BEFORE the publish_tx so the state machine
    // task observes its mpsc shutdown cleanly: the handle owns the JoinHandle
    // for the SM task; once dropped, the task's transport.recv path exits
    // when its owned receiver hits None. We then drop publish_tx, which
    // closes the event-loop publish channel.
    drop(pairing_handle);
    drop(publish_tx); // drop sender so event loop's recv returns None
    drop(fetch_tx);
    drop(ingest_tx);
    drop(content_verb_tx);
    drop(follow_tx);
    drop(voice_tx);
    drop(voice_channel_tx);
    // ZEB-225 Sub-B Phase 2: drop DM outbox handles after the channel
    // drops. send_dm IPC and the event-loop drain tick both clone these
    // Arcs into local scope before await, so dropping our Arc here just
    // releases our reference; any in-flight IPC keeps its own clone
    // alive for the duration of its critical section.
    drop(dm_outbox);
    drop(dm_transport);
    drop(crdt_state);
    drop(hlc_tracker);
    drop(dm_device_id);
    // OwnerAddr is Copy → use `let _` instead of drop() to satisfy
    // clippy::dropping_copy_types (the binding goes out of scope here
    // either way; the explicit binding is just for documentation).
    let _ = dm_self_owner;
    drop(content_store);
    // ZEB-227 Path B: drop the outbound unicast sender so any clone held
    // by the now-shutting-down RuntimeUnicastTransport (Task 11) sees its
    // last reference reach the close threshold. The event_loop's receiver
    // gets None on its next .recv() and the select arm de-registers.
    drop(unicast_send_tx);
    // ZEB-217 Sub-C Phase 2: shut down the per-community engine pool
    // BEFORE the owner-state SyncEngine. Each community engine drives
    // its own debounced final-publish + persist pass on
    // `shutdown()`; running this before the event-loop thread joins
    // keeps the Zenoh session (and per-community publisher tasks)
    // alive long enough for the final state-root publish to land on
    // the wire. Awaiting all engines also closes their internal
    // `error_tx` clones, which lets the start_node-spawned drain task
    // exit cleanly when its receiver returns None.
    //
    // Same `thread::scope` + ephemeral-runtime pattern as the
    // SyncEngine shutdown below — `stop_inner` is sync but reachable
    // from async contexts, and a `block_on` from inside an existing
    // runtime panics.
    if let Some(registry) = community_registry {
        std::thread::scope(|s| {
            s.spawn(|| {
                match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => {
                        if let Err(e) = rt.block_on(registry.shutdown_all()) {
                            tracing::error!(
                                error = %e,
                                "CommunitySyncRegistry shutdown_all failed during stop_inner"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::error!(
                            error = %e,
                            "could not build ephemeral tokio runtime for \
                             CommunitySyncRegistry shutdown — final publish/persist skipped"
                        );
                    }
                }
            });
        });
    }
    // ZEB-217 Sub-C Phase 3 Task 8: drop the community delta sender after
    // `registry.shutdown_all()` completes so every per-engine clone has
    // already been released. The consumer task's receiver then observes
    // a closed channel and exits cleanly.
    drop(community_delta_tx);
    // Phase 3a: explicitly shut down the SyncEngine before joining the
    // event-loop thread. This flushes any pending debounced publish and
    // runs the final persist pass. Must run before stop_handles so the
    // engine's internal tokio task is still alive when we await it.
    //
    // `stop_inner` is sync, but it's reachable from async contexts (e.g.,
    // start_node's restart path). Calling `Runtime::block_on` on a thread
    // that already participates in a Tokio runtime panics with "Cannot
    // start a runtime from within a runtime." Host the shutdown on a
    // fresh OS thread via `thread::scope` so the new runtime sees no
    // outer runtime context.
    if let Some(engine) = sync_engine {
        std::thread::scope(|s| {
            s.spawn(|| {
                match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => {
                        if let Err(e) = rt.block_on(engine.shutdown()) {
                            tracing::error!(
                                error = %e,
                                "SyncEngine final flush failed during stop_inner"
                            );
                        }
                    }
                    Err(e) => {
                        // Without the ephemeral runtime we can't
                        // drive `engine.shutdown()` from this sync
                        // context, so the final publish + persist
                        // are skipped. Surfacing the failure loudly
                        // is the best we can do — silently dropping
                        // the last delta would corrupt next-boot
                        // state. Runtime build is essentially
                        // infallible in practice (only fails on OOM
                        // / thread-creation failure), so this path
                        // is mostly defensive.
                        tracing::error!(
                            error = %e,
                            "could not build ephemeral tokio runtime for SyncEngine \
                             shutdown — final publish/persist skipped"
                        );
                    }
                }
            });
        });
    }
    stop_handles(shutdown_tx, thread);
    had_node
}

/// Start the harmony node with an embedded NodeRuntime.
///
/// Generates/loads identity, creates the runtime, and spawns the event loop
/// as a background task. Emits `zenoh-status` events to the frontend.
#[tauri::command]
async fn start_node(
    endpoint: Option<String>,
    app: AppHandle,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    // ── Atomic stop→identity→config→spawn→store ─────────────────────
    // Everything from stop through handle registration runs under the
    // lock (with a brief drop for the blocking thread join). This
    // prevents concurrent start_node calls from racing on identity
    // generation or orphaning threads.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (publish_tx, publish_rx) = tokio::sync::mpsc::channel(64);
    let (fetch_tx, fetch_rx) = tokio::sync::mpsc::channel(64);
    let (ingest_tx, ingest_rx) = tokio::sync::mpsc::channel(64);
    // Phase 3b: CasOp channel for SyncEngine ↔ event_loop.
    // Capacity 8 is chosen because the SyncEngine serializes its publishes
    // (debounce window) so at most one PutLocal is in flight at a time;
    // GetOrFetch uses a second-mpsc-hop re-entry pattern that briefly
    // doubles the queue depth. See spec §"Risks: cas_op_tx capacity".
    let (cas_op_tx, cas_op_rx) = tokio::sync::mpsc::channel::<crate::content_store::CasOp>(8);
    // ZEB-227 Path B: outbound DM unicast channel. Sized at 256 to absorb
    // realistic group-DM fan-out spikes: a single send_dm to a group can
    // emit up to 16 members × 4 devices = 64 UnicastSendRequests, and
    // overlapping batches from concurrent send_dm + handle_cidnotify ack
    // fan-out can stack on top. 256 is "doubled-and-then-some" of that
    // single-send bound — production try_send call sites
    // (RuntimeUnicastTransport::send + handle_cidnotify ack fan-out)
    // surface Transient on full so back-pressure NEVER causes deadlock
    // even if the cap is exceeded; the larger cap just keeps that
    // recovery path off the hot path. Sender clone is lifted onto
    // NodeState so Task 11 can reach it when instantiating
    // RuntimeUnicastTransport; receiver is consumed by event_loop::run's
    // new select! arm (forwards each request as
    // RuntimeEvent::SendUnicastToDevice into NodeRuntime).
    let (unicast_send_tx, unicast_send_rx) =
        tokio::sync::mpsc::channel::<crate::dm_outbox::UnicastSendRequest>(256);
    let (follow_tx, follow_rx) = tokio::sync::mpsc::channel(64);
    let (voice_tx, voice_rx) = tokio::sync::mpsc::channel(100);
    let (voice_channel_tx, voice_channel_rx) = tokio::sync::mpsc::channel(16);
    let (content_verb_tx, content_verb_rx) =
        tokio::sync::mpsc::channel::<event_loop::ContentVerbRequest>(32);
    // Mail refresh channel. MailSync (constructed below once identity is
    // loaded) owns the sender; the event loop's select! arm services
    // RefreshRequests by issuing a Zenoh get against the gateway's
    // mail-root queryable.
    let (mail_refresh_tx, mail_refresh_rx) =
        tokio::sync::mpsc::channel::<crate::mail_sync::RefreshRequest>(8);
    // ZEB-197 pairing wire-message inbound channel. The event loop fills
    // this from `harmony/pairing/v2/lan/**` Zenoh subscription samples;
    // the ZenohPairingTransport (constructed after ready_rx) drains it.
    let (pairing_in_tx, pairing_in_rx) =
        tokio::sync::mpsc::channel::<crate::pairing::types::PairingWireMessage>(64);

    // Load the follow list from disk and create the shared followed set.
    let app_data_dir = {
        use tauri::Manager;
        app.path()
            .app_data_dir()
            .map_err(|e| format!("app_data_dir: {e}"))?
    };
    std::fs::create_dir_all(&app_data_dir).map_err(|e| format!("create app_data_dir: {e}"))?;
    let follow_mgr = follows::FollowManager::load(&app_data_dir);
    let followed_set = std::sync::Arc::new(std::sync::Mutex::new(
        follow_mgr
            .addresses()
            .into_iter()
            .collect::<std::collections::HashSet<String>>(),
    ));
    // ZEB-155: fetch-completion channel. Both halves are owned by
    // start_node so the spawned fetch task (in event_loop) can clone the
    // tx, while the main loop consumes from the rx.
    let (fetch_completion_tx, fetch_completion_rx) = tokio::sync::mpsc::channel::<[u8; 32]>(32);

    let followed_set_clone = followed_set.clone();

    // MailManager will be initialized after identity loading (needs owner address).
    // Placeholder — set below once we have our_addr_bytes.
    let mail_mgr: std::sync::Arc<std::sync::Mutex<mail::MailManager>>;

    // Stop existing node — extract handles under the lock in a tight
    // inner scope so the std `MutexGuard` (which is `!Send`) is fully
    // out of scope before the SyncEngine's `.await`. Without this
    // scoping, rustc's async generator analysis sees the guard's
    // storage slot as live across the await point and rejects the
    // function as not `Send`.
    let (
        old_shutdown,
        old_thread,
        old_publish,
        old_fetch,
        old_ingest,
        old_content_verb,
        old_follow,
        old_voice,
        old_voice_channel,
        old_pairing_handle,
        old_sync_engine,
        old_community_registry,
        old_community_delta_tx,
        old_dm_outbox,
        old_dm_transport,
        old_crdt_state,
        old_hlc_tracker,
        old_dm_device_id,
        old_dm_self_owner,
        old_content_store,
        old_unicast_send_tx,
    ) = {
        let mut guard = state.lock().map_err(|e| format!("lock error: {e}"))?;
        let tup = (
            guard.shutdown_tx.take(),
            guard.thread.take(),
            guard.publish_tx.take(),
            guard.fetch_tx.take(),
            guard.ingest_tx.take(),
            guard.content_verb_tx.take(),
            guard.follow_tx.take(),
            guard.voice_tx.take(),
            guard.voice_channel_tx.take(),
            guard.pairing_handle.take(),
            guard.sync_engine.take(),
            // ZEB-217 Sub-C Phase 2: take + shutdown the previous
            // identity's per-community engine pool. Mirrors stop_inner's
            // ordering — drain communities BEFORE the owner SyncEngine.
            guard.community_registry.take(),
            // ZEB-217 Sub-C Phase 3 Task 8: take the previous identity's
            // delta sender; dropped after `registry.shutdown_all()` below
            // so the consumer task exits cleanly.
            guard.community_delta_tx.take(),
            // ZEB-225 Sub-B Phase 2: take + drop the per-identity DM
            // handles so a restart doesn't carry stale Arc<DmOutbox> /
            // Arc<DmTransport> / Arc<OwnerState> / Arc<HlcTracker> /
            // Arc<dyn ContentStore> against the prior identity into the
            // new generation. Mirrors stop_inner's cleanup.
            guard.dm_outbox.take(),
            guard.dm_transport.take(),
            guard.crdt_state.take(),
            guard.hlc_tracker.take(),
            guard.dm_device_id.take(),
            guard.dm_self_owner.take(),
            guard.content_store.take(),
            // ZEB-227 Path B: take + drop the previous identity's outbound
            // unicast sender so the new generation gets a fresh channel.
            guard.unicast_send_tx.take(),
        );
        let _old_follow_mgr = guard.follow_mgr.take();
        let _old_followed_set = guard.followed_set.take();
        let _old_mail_mgr = guard.mail_mgr.take();
        let _old_mail_sync = guard.mail_sync.take();
        // ZEB-228 Phase 4: clear our cached identity_pub so a restart
        // can't ship the prior identity's pub on the new identity's
        // outbound DmInvites. Mirrors stop_inner's cleanup.
        let _ = guard.dm_identity_pub_64.take();
        // ZEB-217 Sub-C Phase 3 Task 9: clear the previous adapter-
        // request sender so it doesn't outlive the previous event
        // loop. The new event loop is constructed below with a fresh
        // channel pair.
        let _ = guard.community_adapter_request_tx.take();
        tup
    };

    // Drop pairing_handle BEFORE publish_tx so the SM task's transport
    // sees its receiver close after the publish channel is gone — same
    // ordering as stop_inner.
    drop(old_pairing_handle);
    drop(old_publish);
    drop(old_fetch);
    drop(old_ingest);
    drop(old_content_verb);
    drop(old_follow);
    drop(old_voice);
    drop(old_voice_channel);
    // ZEB-225 Sub-B Phase 2: drop the previous identity's DM handles so
    // the new SyncEngine/DmOutbox built below sees no stale Arc clones
    // outside the new NodeState. Same drop-order rationale as stop_inner.
    drop(old_dm_outbox);
    drop(old_dm_transport);
    drop(old_crdt_state);
    drop(old_hlc_tracker);
    drop(old_dm_device_id);
    // OwnerAddr is Copy → use `let _` instead of drop() to satisfy
    // clippy::dropping_copy_types.
    let _ = old_dm_self_owner;
    drop(old_content_store);
    // ZEB-227 Path B: drop the previous identity's outbound unicast sender
    // so the new generation's RuntimeUnicastTransport (Task 11) sees no
    // stale clones outside the new NodeState.
    drop(old_unicast_send_tx);
    // ZEB-217 Sub-C Phase 2: explicitly await the previous community
    // engine pool's shutdown BEFORE the owner SyncEngine. Mirrors
    // stop_inner's ordering — community engines need their final
    // state-root publish to land on the wire before the event-loop
    // thread joins. We're in async start_node so no thread::scope
    // juggling needed (unlike stop_inner).
    if let Some(registry) = old_community_registry {
        if let Err(e) = registry.shutdown_all().await {
            tracing::error!(
                error = %e,
                "previous CommunitySyncRegistry shutdown_all failed during start_node restart"
            );
        }
    }
    // ZEB-217 Sub-C Phase 3 Task 8: drop the prior delta sender after the
    // registry shut down so every per-engine clone is gone first; the
    // consumer task drains pending events and exits.
    drop(old_community_delta_tx);
    // Phase 3a: explicitly await the previous SyncEngine's shutdown
    // before installing the replacement, so any pending debounced
    // publish flushes and the final persist pass completes. Dropping
    // alone is best-effort — the internal task could be mid-await
    // and never observe the channel close in time. We're in async
    // start_node, so no thread::scope juggling needed.
    if let Some(engine) = old_sync_engine {
        if let Err(e) = engine.shutdown().await {
            tracing::error!(
                error = %e,
                "previous SyncEngine final flush failed during start_node restart"
            );
        }
    }
    stop_handles(old_shutdown, old_thread);

    let our_gen = {
        // ── Identity loading — no lock held here; the inner block at
        //    line ~735 re-acquires the std::Mutex to atomically register
        //    the new node handles. (Stopping the old node already ran
        //    above outside this block, so the registration race window
        //    is bounded by that re-acquisition only.)
        let id_path = identity::resolve_path(None)?;
        let id = identity::load_or_generate(&id_path)?;
        let identity::NodeIdentity { pq, ed25519 } = id;

        let our_addr_bytes: [u8; 16] = ed25519.public_identity().address_hash;
        let node_addr = hex::encode(our_addr_bytes);

        let pq_pub = pq.public_identity();
        let local_pq_identity_hash = pq_pub.address_hash;
        let local_dsa_pubkey = pq_pub.verifying_key.as_bytes();
        let local_kem_pubkey = pq_pub.encryption_key.as_bytes();
        drop(pq);

        // ZEB-228 Phase 4: capture our 64-byte combined identity_pub
        // (X25519_pub(32) || Ed25519_pub(32) per
        // `harmony_identity::Identity::to_public_bytes()`) BEFORE the
        // ed25519 PrivateIdentity is dropped below. add_space ships this
        // as the bootstrap pubkey on outbound DmInvite packets so the
        // recipient can verify the signature without a prior
        // OwnerDeviceCache entry for us.
        let identity_pub_64: [u8; 64] = ed25519.public_identity().to_public_bytes();

        let reticulum_identity_bytes = Some(zeroize::Zeroizing::new(ed25519.to_private_bytes()));
        // ZEB-262 Phase 4 Task 2: snapshot a second `PrivateIdentity` instance
        // BEFORE the local `ed25519` binding is dropped. The Reticulum/Ed25519
        // identity is the same material we'll later use on the receive-side
        // counter-sign path (`handle_invite` →
        // `community_membership::attach_countersig_with_identity`); plumbing
        // it through `DmOutbox` lets the inbound CommunityInvite handler grab
        // a reference under the dm_outbox lock without re-reading the
        // on-disk identity.
        //
        // We can't `clone()` `PrivateIdentity` (it carries `ZeroizeOnDrop`
        // and intentionally does NOT implement Clone), so we reconstruct
        // from the private bytes we just captured. Round-trip via
        // `from_private_bytes` is bit-identical: same X25519 secret + same
        // Ed25519 secret → same `Identity` (verified by
        // `dm_outbox_holds_private_identity_for_countersign`).
        let private_identity_arc = std::sync::Arc::new(
            harmony_identity::PrivateIdentity::from_private_bytes(
                reticulum_identity_bytes
                    .as_ref()
                    .expect("populated above")
                    .as_slice(),
            )
            .expect("private bytes round-trip"),
        );
        drop(ed25519);

        tracing::info!(address = %node_addr, path = %id_path.display(), "identity loaded");

        // Initialize mail manager (needs owner address from identity).
        mail_mgr = std::sync::Arc::new(std::sync::Mutex::new(mail::MailManager::load(
            &app_data_dir.join("mail"),
            our_addr_bytes,
        )));

        // Construct MailSync now that identity, mail_mgr, and the refresh
        // channel are all available. Owns a clone of fetch_tx (so commands
        // keep their own sender in AppState) and the sole refresh_tx.
        let mail_sync = std::sync::Arc::new(mail_sync::MailSync::new(
            fetch_tx.clone(),
            mail_refresh_tx,
            std::sync::Arc::clone(&mail_mgr),
            app.clone(),
        ));

        // ── Phase 3a: SyncEngine construction ──────────────────────────
        // Load the owner identity (master_seed + device_signing_key) to
        // construct the SyncEngine. This is independent of the Reticulum
        // network identity loaded above. If no owner identity exists yet
        // (pre-mint), sync_handles / sync_engine are None and the rest of
        // start_node proceeds normally.
        let identity_dir = crate::owner_commands::resolve_identity_dir()?;
        let owner_loaded = crate::owner_state::load_owner_state(
            &identity_dir,
            crate::identity::KeychainStore::new().ok(),
        )?;

        let mut sync_handles_opt: Option<crate::event_loop::SyncEngineHandles> = None;
        // ZEB-225 Sub-B Phase 2: lift the per-identity handles SyncEngine
        // depends on (device_id, self_owner, crdt_state, tracker,
        // content_store) out of the `if let Some(seed)` block so the
        // outer NodeState assignment can reach them. send_dm IPC clones
        // these from NodeState; without lifting, they'd be unreachable
        // outside the SyncEngine constructor.
        let mut device_id_for_state: Option<String> = None;
        let mut self_owner_for_state: Option<crate::owner_state_types::OwnerAddr> = None;
        let mut crdt_state_for_state: Option<
            std::sync::Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
        > = None;
        let mut tracker_for_state: Option<
            std::sync::Arc<
                tokio::sync::Mutex<
                    std::collections::BTreeMap<String, crate::owner_state_types::Hlc>,
                >,
            >,
        > = None;
        let mut content_store_for_state: Option<
            std::sync::Arc<dyn crate::content_store::ContentStore>,
        > = None;
        let mut dm_outbox_arc: Option<
            std::sync::Arc<tokio::sync::Mutex<crate::dm_outbox::DmOutbox>>,
        > = None;
        let mut dm_transport_arc: Option<std::sync::Arc<dyn crate::dm_outbox::DmTransport>> = None;
        // ZEB-217 Sub-C Phase 2 Task 13: per-community engine pool +
        // adapter requests handed to the event loop. Both stay None /
        // empty when no owner identity is loaded (registry depends on
        // crdt_state). When an owner IS loaded, we build the registry
        // inside the if-let block below, scan owner-state for joined
        // communities, spawn one engine per community, and push one
        // CommunityAdapterRequest per spawn for event_loop::run to
        // wire up against the Zenoh session.
        let mut community_registry_arc: Option<
            std::sync::Arc<crate::community_state_sync::CommunitySyncRegistry>,
        > = None;
        // ZEB-217 Sub-C Phase 3 Task 8: outer-scope holder for the
        // community delta sender. The original is created inside the
        // if-let-Some(owner_loaded) block alongside the registry; we
        // lift a clone out here so it can be stashed on NodeState below
        // for stop_node / restart cleanup.
        let mut community_delta_tx_for_state: Option<
            tokio::sync::mpsc::Sender<crate::community_state_sync::CommunityMembershipDelta>,
        > = None;
        let mut community_adapter_requests: Vec<crate::event_loop::CommunityAdapterRequest> =
            Vec::new();

        let sync_engine_arc: Option<std::sync::Arc<crate::owner_state_sync::SyncEngine>> =
            if let Some(ref loaded) = owner_loaded {
                if let Some(seed) = loaded.master_seed.as_ref() {
                    let kt = std::sync::Arc::new(
                        crate::owner_state_crypto::KeyTree::derive(seed)
                            .map_err(|e| format!("KeyTree::derive: {e}"))?,
                    );
                    let device_id = loaded
                        .device_signing_key
                        .verifying_key()
                        .to_bytes()
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect::<String>();

                    let crdt_path = identity_dir.join("owner_state_crdt.cbor");
                    let replay_path = identity_dir.join("state_root_replay.cbor");
                    let initial_crdt = crate::owner_state_persist::load_crdt(&crdt_path)
                        .map_err(|e| format!("load owner_state_crdt.cbor: {e}"))?;
                    let initial_replay = crate::owner_state_persist::load_replay(&replay_path)
                        .map_err(|e| format!("load state_root_replay.cbor: {e}"))?;

                    let crdt_state = std::sync::Arc::new(tokio::sync::Mutex::new(initial_crdt));
                    let tracker = std::sync::Arc::new(tokio::sync::Mutex::new(initial_replay));
                    // Phase 3b: real harmony-content CAS via RuntimeContentStore.
                    // Sends CasOp messages over cas_op_tx into the harmony-
                    // runtime event loop, which admits/queries through the
                    // shared NodeRuntime + StorageTier. See spec
                    // §"Architecture / High-level flow".
                    let content_store: std::sync::Arc<dyn crate::content_store::ContentStore> =
                        std::sync::Arc::new(crate::content_store::RuntimeContentStore::new(
                            cas_op_tx.clone(),
                            std::time::Duration::from_millis(
                                crate::content_store::DEFAULT_FETCH_TIMEOUT_MS,
                            ),
                        ));

                    let (out_tx, out_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
                    let (in_tx, in_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

                    let self_owner = crate::owner_state_types::OwnerAddr(loaded.state.owner_id);

                    // ZEB-225 Sub-B Phase 2: construct DmOutbox + transport
                    // alongside SyncEngine. Both share device_id + self_owner
                    // with the SyncEngine.
                    //
                    // ZEB-227 Phase 3b Task 11: DmOutbox + RuntimeUnicastTransport
                    // both consume the SAME (signing_key, signing_device_hash)
                    // pair sourced from the Reticulum identity loaded above.
                    // The Reticulum identity's `address_hash` IS the
                    // DeviceIdentityHash that peers cache in OwnerDeviceCache —
                    // signing with any other key would produce signatures that
                    // fail verification at the receiver's
                    // `verify_dm_packet_signature` (key-substitution defense:
                    // Step 1 derives the device hash from the identity_pub
                    // and rejects if it doesn't match the wire-claimed hash).
                    //
                    // SigningKey extraction: `ed25519.to_private_bytes()`
                    // returns `[32B X25519_secret][32B Ed25519_secret]` per
                    // harmony_identity::PrivateIdentity::to_private_bytes
                    // (identity.rs:308). The Ed25519 secret half occupies
                    // bytes 32..64 and constructs an ed25519_dalek::SigningKey
                    // bit-identical to the one PrivateIdentity::sign uses
                    // internally (verified by sign_dm_packet_matches_private_identity_sign
                    // in dm_signing.rs).
                    // Wrap in Zeroizing — the signing seed must be scrubbed
                    // when this scope ends, mirroring how
                    // reticulum_identity_bytes is held above (line 772).
                    // Without this the 32-byte stack copy would persist in
                    // freed stack memory until overwritten.
                    let ed25519_seed = zeroize::Zeroizing::new(
                        <[u8; 32]>::try_from(
                            &reticulum_identity_bytes
                                .as_ref()
                                .expect("reticulum_identity_bytes populated above")
                                [32..64],
                        )
                        .expect("64 - 32 == 32"),
                    );
                    let signing_key_arc =
                        std::sync::Arc::new(ed25519_dalek::SigningKey::from_bytes(&ed25519_seed));
                    let our_signing_device_hash =
                        crate::owner_state_types::DeviceIdentityHash(our_addr_bytes);
                    let outbox = std::sync::Arc::new(tokio::sync::Mutex::new(
                        crate::dm_outbox::DmOutbox::new(
                            device_id.clone(),
                            self_owner,
                            our_signing_device_hash,
                            signing_key_arc.clone(),
                            std::sync::Arc::clone(&private_identity_arc),
                        ),
                    ));
                    // Production transport: RuntimeUnicastTransport pushes
                    // signed CidNotify packets into unicast_send_tx, which
                    // event_loop::run translates into
                    // RuntimeEvent::SendUnicastToDevice. OwnerAddr →
                    // device-hash resolution happens inside drain (which
                    // has `&OwnerState` from the event-loop's mutex guard),
                    // not in the transport — splitting resolution out
                    // sidesteps the recursive-lock deadlock that broke
                    // delivery in the original Phase 3b shape.
                    let transport: std::sync::Arc<dyn crate::dm_outbox::DmTransport> =
                        std::sync::Arc::new(crate::dm_outbox::RuntimeUnicastTransport::new(
                            unicast_send_tx.clone(),
                            self_owner,
                            our_signing_device_hash,
                            std::sync::Arc::clone(&signing_key_arc),
                        ));

                    let engine = std::sync::Arc::new(crate::owner_state_sync::SyncEngine::new(
                        std::sync::Arc::clone(&kt),
                        device_id.clone(),
                        std::sync::Arc::clone(&crdt_state),
                        std::sync::Arc::clone(&tracker),
                        std::sync::Arc::clone(&content_store),
                        out_tx,
                        in_rx,
                        crate::owner_state_sync::PersistPaths {
                            crdt: crdt_path,
                            replay: replay_path,
                        },
                        crate::owner_state_sync::DEFAULT_DEBOUNCE_MS,
                    ));

                    // Topic key is the OWNER identity (16-byte address from
                    // `harmony_owner::state::OwnerState.owner_id`), not the
                    // per-device Reticulum transport address — every device
                    // bound to this owner must converge on the same Zenoh
                    // topic `harmony/owner/{addr_hex}/state-root-v1`.
                    let owner_addr_hex = hex::encode(loaded.state.owner_id);
                    sync_handles_opt = Some(crate::event_loop::SyncEngineHandles {
                        addr_hex: owner_addr_hex,
                        outbound_rx: out_rx,
                        inbound_tx: in_tx,
                    });

                    // ── ZEB-217 Sub-C Phase 2 + Phase 3 Task 8: per-community state CRDT sync ─
                    //
                    // Build the registry (owns the multi-community engine pool)
                    // along with two consumer tasks:
                    //   - `community-members-changed` (Phase 3 Task 8) — the
                    //     engine fires one `CommunityMembershipDelta` per
                    //     CRDT mutation; the consumer projects each delta
                    //     into `CommunityMembersChangedPayload` via
                    //     `delta_to_change` and emits a Tauri event so the
                    //     frontend updates the member list incrementally.
                    //   - `community-state-sync-degraded` (Phase 2) — every
                    //     spawned engine clones `community_degraded_tx`
                    //     into its `CommunitySyncEngineConfig`; the
                    //     consumer receives reports and surfaces a degraded
                    //     banner per-community.
                    //
                    // Both channels are created BEFORE the registry config
                    // so the senders can be passed into `CommunityRegistryConfig`
                    // and cloned into every per-engine config inside
                    // `spawn_engine`. Channel capacity (256) is sized for
                    // burst-tolerance under degraded / mass-receive
                    // conditions; a full channel falls back to dropping
                    // the message (`try_send`-style fire-and-forget) so a
                    // single noisy community can't starve the rest of the
                    // engine pool.
                    let (community_delta_tx, community_delta_rx) = tokio::sync::mpsc::channel::<
                        crate::community_state_sync::CommunityMembershipDelta,
                    >(256);
                    let (community_degraded_tx, community_degraded_rx) = tokio::sync::mpsc::channel::<
                        crate::community_state_sync::CommunityDegradedReport,
                    >(256);

                    let registry: std::sync::Arc<
                        crate::community_state_sync::CommunitySyncRegistry,
                    > = {
                        let resolver: std::sync::Arc<
                            dyn crate::community_state_sync::IdentityResolver,
                        > = std::sync::Arc::new(
                            crate::community_state_sync::OwnerDeviceCacheResolver::new(
                                std::sync::Arc::clone(&crdt_state),
                                self_owner,
                                identity_pub_64,
                            ),
                        );
                        let cfg = crate::community_state_sync::CommunityRegistryConfig {
                            device_id: device_id.clone(),
                            content_store: std::sync::Arc::clone(&content_store),
                            identity_resolver: resolver,
                            identity_dir: identity_dir.clone(),
                            debounce_ms: crate::community_state_sync::DEFAULT_DEBOUNCE_MS,
                            error_tx: Some(community_degraded_tx),
                            delta_tx: Some(community_delta_tx.clone()),
                            // ZEB-256 Task 6: registry holds the local
                            // identity once; every spawned engine
                            // clones into its CommunitySyncEngineConfig.
                            // Both values already exist above for the
                            // DmOutbox plumbing.
                            self_owner,
                            signing_key: std::sync::Arc::clone(&signing_key_arc),
                        };
                        std::sync::Arc::new(
                            crate::community_state_sync::CommunitySyncRegistry::new(cfg),
                        )
                    };

                    // Spawn the delta consumer: each `CommunityMembershipDelta`
                    // becomes one `community-members-changed` Tauri event.
                    // Task exits cleanly when every per-engine `delta_tx`
                    // clone AND the start_node-held clone have all
                    // dropped — which happens after `registry.shutdown_all()`
                    // and the explicit `drop(community_delta_tx)` in
                    // stop_inner / start_node restart.
                    {
                        let app_for_delta = app.clone();
                        tokio::spawn(run_community_delta_consumer(
                            community_delta_rx,
                            move |payload| {
                                let app = app_for_delta.clone();
                                async move {
                                    if let Err(e) = app.emit("community-members-changed", &payload)
                                    {
                                        tracing::warn!(
                                            error = ?e,
                                            "failed to emit community-members-changed"
                                        );
                                    }
                                }
                            },
                        ));
                    }

                    // Spawn the degraded consumer: each `CommunityDegradedReport`
                    // becomes one `community-state-sync-degraded` Tauri event.
                    // Task exits cleanly when `community_degraded_rx.recv()`
                    // returns None — happens after every engine's
                    // `error_tx` clone drops on `registry.shutdown_all()`.
                    {
                        let app_for_degraded = app.clone();
                        tokio::spawn(run_community_degraded_consumer(
                            community_degraded_rx,
                            move |payload| {
                                let app = app_for_degraded.clone();
                                async move {
                                    if let Err(e) =
                                        app.emit("community-state-sync-degraded", &payload)
                                    {
                                        tracing::warn!(
                                            error = ?e,
                                            "failed to emit community-state-sync-degraded"
                                        );
                                    }
                                }
                            },
                        ));
                    }

                    // Lift the start_node-held delta sender out for
                    // NodeState assignment so stop_node / restart can
                    // drop it after `registry.shutdown_all()` to close
                    // the consumer's channel.
                    community_delta_tx_for_state = Some(community_delta_tx);

                    // Scan owner-state for joined communities and spawn
                    // one engine per community. Each spawn allocates a
                    // pair of mpsc channels here (publisher_tx /
                    // subscriber_rx for the engine; matching
                    // publisher_rx / subscriber_tx into a
                    // CommunityAdapterRequest the event loop wires to
                    // Zenoh after `zenoh::open`). Skip any Community
                    // Space that's missing membership_key or admin_addr
                    // — those fields are MUST-be-Some-for-Community per
                    // owner_state_types.rs:1420 / 1427, so a missing
                    // value means a corrupt or partially-applied row;
                    // logging + skipping keeps boot resilient rather
                    // than crashing the node.
                    // Snapshot community metadata under the crdt_state
                    // lock first, then drop the lock before awaiting
                    // spawn_engine. Holding crdt_state across await
                    // would create a lock-order hazard with engine
                    // initialization paths and prevent other tasks from
                    // reading owner-state during boot. Adapter requests
                    // are only enqueued after spawn_engine succeeds so
                    // a failed spawn doesn't leave an orphaned channel
                    // pair for the event_loop to wire to a dead engine.
                    type CommunitySpawnSnapshot = (
                        crate::owner_state_types::SpaceId,
                        crate::owner_state_types::MembershipKey,
                        crate::owner_state_types::OwnerAddr,
                        bool,
                    );
                    let community_snapshots: Vec<CommunitySpawnSnapshot> = {
                        let state_snap = crdt_state.lock().await;
                        state_snap
                            .spaces
                            .iter()
                            .filter_map(|(space_id, space)| {
                                if space.kind
                                    != crate::owner_state_types::SpaceKind::Community
                                {
                                    return None;
                                }
                                if space.left_at.is_some() {
                                    return None;
                                }
                                let mk = match space.membership_key.as_ref() {
                                    Some(k) => k.clone(),
                                    None => {
                                        tracing::warn!(
                                            ?space_id,
                                            "community Space missing membership_key — skipping engine spawn"
                                        );
                                        return None;
                                    }
                                };
                                let admin = match space.admin_addr {
                                    Some(a) => a,
                                    None => {
                                        tracing::warn!(
                                            ?space_id,
                                            "community Space missing admin_addr — skipping engine spawn"
                                        );
                                        return None;
                                    }
                                };
                                let is_invite_only =
                                    space.is_invite_only.unwrap_or(false);
                                Some((*space_id, mk, admin, is_invite_only))
                            })
                            .collect()
                    }; // crdt_state lock released here

                    for (space_id, mk, admin, is_invite_only) in community_snapshots {
                        let (pub_tx, pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
                        let (sub_tx, sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

                        if let Err(e) = registry
                            .spawn_engine(space_id, mk, admin, is_invite_only, pub_tx, sub_rx)
                            .await
                        {
                            tracing::error!(
                                ?space_id,
                                error = %e,
                                "failed to spawn community engine"
                            );
                            // Drop pub_rx + sub_tx implicitly — no
                            // adapter request enqueued, so the
                            // event_loop won't try to wire orphan
                            // channels to a non-existent engine.
                            continue;
                        }
                        community_adapter_requests.push(
                            crate::event_loop::CommunityAdapterRequest {
                                id_hex: hex::encode(space_id.0),
                                publisher_rx: pub_rx,
                                subscriber_tx: sub_tx,
                            },
                        );
                    }

                    community_registry_arc = Some(registry);

                    // Lift the per-identity handles out for NodeState
                    // assignment below.
                    device_id_for_state = Some(device_id);
                    self_owner_for_state = Some(self_owner);
                    crdt_state_for_state = Some(crdt_state);
                    tracker_for_state = Some(tracker);
                    content_store_for_state = Some(content_store);
                    dm_outbox_arc = Some(outbox);
                    dm_transport_arc = Some(transport);

                    Some(engine)
                } else {
                    None
                }
            } else {
                None
            };

        let node_addr_for_state = node_addr.clone();
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
            node_addr,
            local_identity_hash: our_addr_bytes,
            local_pq_identity_hash,
            local_dsa_pubkey,
            local_kem_pubkey,
            reticulum_identity_bytes,
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

        // Re-acquire lock and atomically register the new node.
        // Handles are stored BEFORE awaiting ready_rx so stop_node can
        // cancel an in-flight startup via shutdown_tx.
        let mut guard = state.lock().map_err(|e| format!("lock error: {e}"))?;
        guard.generation += 1;

        // ZEB-155: load the sidecar NOW — after stop_handles has
        // quiesced the previous node and under the state lock — so any
        // pin_content / unpin_content / burn_content that raced with
        // the stop path has already durably written to disk. Concurrent
        // command handlers are blocked on state.lock(), so they cannot
        // slip a write between this load and the Arc install below.
        //
        // A narrower window remains: a mutation command that cloned
        // the OLD Arc before stop_handles and is still mid-set_pinned
        // when the NEW Arc is installed will orphan its disk write
        // (the next NEW-Arc save() overwrites). That end-to-end
        // serialization is ZEB-160's territory.
        let content_index = std::sync::Arc::new(std::sync::Mutex::new(
            content_index::ContentIndex::load(&app_data_dir),
        ));
        let pin_intent: std::collections::HashSet<[u8; 32]> = {
            let idx = content_index
                .lock()
                .map_err(|e| format!("content_index lock on startup: {e}"))?;
            // ZEB-164: multiple sidecar entries can pin the same CID. The
            // runtime pin_intent set is CID-keyed, so we dedupe here —
            // collecting into a HashSet drops duplicates without effect.
            // (Functionally identical to the pre-ZEB-164 path; the dedupe
            // is just made explicit so debug logs don't show repeated
            // restores for the same CID.)
            idx.entries().filter(|e| e.pinned).map(|e| e.cid).collect()
        };

        let ep_clone = endpoint.clone();
        let app_clone = app.clone();
        let mail_mgr_clone = mail_mgr.clone();
        let mail_sync_for_loop = std::sync::Arc::clone(&mail_sync);
        let cas_op_tx_for_loop = cas_op_tx.clone();
        let sync_handles_for_loop = sync_handles_opt;
        let dm_outbox_for_loop = dm_outbox_arc.clone();
        let dm_transport_for_loop = dm_transport_arc.clone();
        let crdt_state_for_loop = crdt_state_for_state.clone();
        // ZEB-227 Path B Task 11: extra handles for the
        // RuntimeAction::UnicastReceived interception block in event_loop.
        // cas_handle: handle_cidnotify does a 500ms-timeout cas.get; reuse
        //   the same RuntimeContentStore the SyncEngine consumes.
        // unicast_send_tx_for_loop: handle_cidnotify pushes DmAck fan-out
        //   into the same channel the production transport uses for
        //   outbound CidNotify. Same channel, both directions push.
        let cas_handle_for_loop = content_store_for_state.clone();
        let unicast_send_tx_for_loop = Some(unicast_send_tx.clone());
        // ZEB-217 Sub-C Phase 2: per-community Zenoh adapter requests.
        // Move (not clone) — the Vec carries Receiver halves the engines
        // already own the matching Sender / other-half for; only the
        // event loop reads from this Vec, no other consumer.
        let community_adapter_requests_for_loop = std::mem::take(&mut community_adapter_requests);
        // ZEB-217 Sub-C Phase 3 Task 9: on-demand adapter request channel.
        // The IPC `create_community` (and Phase 4's `redeem_invite`)
        // dispatch a `CommunityAdapterRequest` here; the event loop's
        // `select!` drains the rx and binds the per-community channel
        // halves to a Zenoh adapter against the live session. Capacity
        // 32 is sized to match peak join-burst load — one request per
        // create/redeem; full-channel falls back to a clear Err on the
        // IPC side rather than blocking under contention.
        let (community_adapter_request_tx, community_adapter_request_rx) =
            tokio::sync::mpsc::channel::<crate::event_loop::CommunityAdapterRequest>(32);
        // ZEB-262 Phase 4 Task 9: clone the community_registry handle
        // for event_loop::run BEFORE the closure capture below moves
        // `community_registry_arc` (the post-spawn `guard.community_registry`
        // assignment still needs the original handle).
        let community_registry_for_loop = community_registry_arc.clone();
        let thread_result = thread::Builder::new()
            .name("harmony-runtime".to_string())
            // Windows debug builds overflow the default ~2 MiB stack inside
            // Zenoh session setup; match the 8 MiB used throughout identity.rs.
            .stack_size(8 * 1024 * 1024)
            .spawn(move || {
                // Zenoh's `.wait()` (used by its `IntoFuture` impl) calls
                // ZRuntime::block_in_place, which panics on a current-thread
                // scheduler. A single-worker multi-thread runtime is the
                // minimum Zenoh supports.
                //
                // `.thread_stack_size(8 MiB)` covers Tokio's own worker
                // threads independently of RUST_MIN_STACK — important
                // because Cargo's `[env]` block in .cargo/config.toml only
                // propagates to binaries Cargo launches (e.g. cargo run /
                // tauri dev), not to release binaries run directly. Without
                // this call, a `tauri build` artifact would silently regress
                // to the 2 MiB default on Windows.
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(1)
                    .thread_stack_size(8 * 1024 * 1024)
                    .enable_all()
                    .build()
                    .expect("failed to create tokio runtime for harmony-runtime");
                rt.block_on(async move {
                    let (mut runtime, startup_actions) =
                        NodeRuntime::new(config, MemoryBookStore::new());

                    // ZEB-227 Path B: register our DM destination so inbound
                    // packets to it surface as RuntimeAction::UnicastReceived.
                    // Without this registration, every inbound DmInvite /
                    // DmCidNotify / DmAck would drop in the runtime as
                    // NoLocalDestination before reaching
                    // dm_outbox::handle_unicast.
                    //
                    // Our DM destination hash is computed from our local
                    // Reticulum identity hash via the same
                    // SHA256(SHA256("harmony.dm")[:10] || identity)[:16]
                    // scheme that DmOutbox::drain uses to resolve outbound
                    // destinations from OwnerDeviceCache (so a peer's
                    // outbound dest_hash for us == our registered
                    // dest_hash for ourselves).
                    //
                    // Unconditional: every node has a Reticulum identity
                    // (loaded above via identity::load_or_generate, before
                    // owner-loading). DMs themselves only flow once the owner
                    // identity is loaded (which gates DmOutbox /
                    // RuntimeUnicastTransport construction above), but the
                    // raw destination registration is harmless when no owner
                    // is loaded — it just means inbound packets surface but
                    // event_loop's UnicastReceived arm has no DmOutbox to
                    // dispatch to (and logs the drop).
                    let our_identity_hash = runtime.local_identity_hash();
                    let our_dm_dest =
                        crate::dm_signing::compute_dm_destination_hash(our_identity_hash);
                    runtime.register_local_destination(our_dm_dest);
                    tracing::info!(
                        dm_dest = hex::encode(our_dm_dest),
                        "registered DM destination for inbound DmInvite/DmCidNotify/DmAck"
                    );

                    event_loop::run(
                        runtime,
                        startup_actions,
                        app_clone,
                        ep_clone,
                        ready_tx,
                        shutdown_rx,
                        publish_rx,
                        fetch_rx,
                        ingest_rx,
                        content_verb_rx,
                        cas_op_tx_for_loop,
                        cas_op_rx,
                        follow_rx,
                        voice_rx,
                        voice_channel_rx,
                        followed_set_clone,
                        mail_mgr_clone,
                        Some(mail_sync_for_loop),
                        mail_refresh_rx,
                        pin_intent,
                        fetch_completion_tx,
                        fetch_completion_rx,
                        Some(pairing_in_tx),
                        sync_handles_for_loop,
                        dm_outbox_for_loop,
                        dm_transport_for_loop,
                        crdt_state_for_loop,
                        Some(unicast_send_rx),
                        cas_handle_for_loop,
                        unicast_send_tx_for_loop,
                        community_adapter_requests_for_loop,
                        community_adapter_request_rx,
                        community_registry_for_loop,
                    )
                    .await;
                });
            });

        // If the runtime-thread spawn fails (rare — typically only on
        // OOM / kernel-thread limits), the SyncEngine constructed
        // above has ALREADY spawned its background tokio task. We
        // can't drop the Arc<SyncEngine> here without first calling
        // `shutdown()` — that would orphan the task and silently lose
        // the final-flush path. The await must happen OUTSIDE this
        // lock-held block (the std `MutexGuard` is `!Send` across an
        // await point), so we capture the failure into a sentinel
        // and clean up below.
        let thread_install_failure: Option<String>;
        match thread_result {
            Ok(thread) => {
                guard.thread = Some(thread);
                guard.shutdown_tx = Some(shutdown_tx);
                guard.publish_tx = Some(publish_tx);
                guard.fetch_tx = Some(fetch_tx);
                guard.ingest_tx = Some(ingest_tx);
                guard.content_verb_tx = Some(content_verb_tx);
                guard.content_index = content_index;
                guard.follow_tx = Some(follow_tx);
                guard.voice_tx = Some(voice_tx);
                guard.voice_channel_tx = Some(voice_channel_tx);
                guard.follow_mgr = Some(follow_mgr);
                guard.followed_set = Some(followed_set);
                guard.mail_mgr = Some(mail_mgr);
                guard.mail_sync = Some(mail_sync);
                guard.node_addr = node_addr_for_state;
                guard.sync_engine = sync_engine_arc.clone();
                // ZEB-217 Sub-C Phase 2: stash the per-community engine
                // registry on NodeState so Phase 3 IPC handlers can
                // reach it. Cloned (Arc bump) — `community_registry_arc`
                // is also held by the failure-cleanup tuple below.
                guard.community_registry = community_registry_arc.clone();
                // ZEB-217 Sub-C Phase 3 Task 8: store the start_node-held
                // delta sender so stop_node / restart can drop it after
                // `registry.shutdown_all()` and the consumer task winds
                // down cleanly.
                guard.community_delta_tx = community_delta_tx_for_state.clone();
                // ZEB-225 Sub-B Phase 2: store DM outbox + per-identity
                // handles on NodeState for send_dm IPC + (T7) drain tick.
                guard.dm_outbox = dm_outbox_arc.clone();
                guard.dm_transport = dm_transport_arc.clone();
                guard.crdt_state = crdt_state_for_state.clone();
                guard.hlc_tracker = tracker_for_state.clone();
                guard.dm_device_id = device_id_for_state.clone();
                guard.dm_self_owner = self_owner_for_state;
                guard.content_store = content_store_for_state.clone();
                // ZEB-227 Path B: store the outbound unicast sender so
                // Task 11's RuntimeUnicastTransport instantiation in
                // start_node can clone it. The receiver was moved into
                // event_loop above; the sender remains unused-by-production
                // until Task 11 wires it to the real transport.
                guard.unicast_send_tx = Some(unicast_send_tx.clone());
                // ZEB-228 Phase 4: store our 64-byte combined identity_pub
                // so add_space can ship it as the bootstrap pubkey on
                // outbound DmInvite packets. Captured above before the
                // ed25519 PrivateIdentity was dropped.
                guard.dm_identity_pub_64 = Some(identity_pub_64);
                // ZEB-217 Sub-C Phase 3 Task 9: store the adapter-
                // request sender so create_community / Phase 4
                // redeem_invite can dispatch on-demand
                // `CommunityAdapterRequest`s into the event loop. The
                // matching rx was moved into event_loop::run above.
                guard.community_adapter_request_tx = Some(community_adapter_request_tx);
                thread_install_failure = None;
            }
            Err(e) => {
                thread_install_failure = Some(format!("failed to spawn runtime thread: {e}"));
            }
        }
        // The third + fourth tuple elements carry the SyncEngine + the
        // CommunitySyncRegistry Arcs back out of the block so the
        // failure-cleanup path below can await `shutdown()` on each
        // without holding the std `MutexGuard` across an await (the
        // guard is `!Send`). On success these Arcs are discarded;
        // NodeState already owns its own clone of each.
        (
            guard.generation,
            thread_install_failure,
            sync_engine_arc.clone(),
            community_registry_arc.clone(),
        )
    };
    let (our_gen, thread_spawn_failure, engine_for_cleanup, registry_for_cleanup) = our_gen;

    if let Some(msg) = thread_spawn_failure {
        // ZEB-217 Sub-C Phase 2: shutdown the registry FIRST so each
        // community engine's final flush completes before the owner
        // SyncEngine tears down. Mirrors stop_inner's ordering.
        if let Some(registry) = registry_for_cleanup {
            if let Err(e) = registry.shutdown_all().await {
                tracing::error!(
                    error = %e,
                    "CommunitySyncRegistry cleanup after runtime-thread spawn failure"
                );
            }
        }
        if let Some(engine) = engine_for_cleanup {
            if let Err(e) = engine.shutdown().await {
                tracing::error!(
                    error = %e,
                    "SyncEngine cleanup after runtime-thread spawn failure"
                );
            }
        }
        return Err(msg);
    }

    // Wait for the event loop to report startup success or failure.
    // stop_node can cancel this by signaling shutdown_tx (now registered).
    let result = match ready_rx.await {
        Ok(Ok(())) => 'arm: {
            // Phase 3b: cross-device sync now works through real CAS
            // (RuntimeContentStore); the Phase 3a degraded banner is
            // retired. Transport-layer failures (subscriber declare,
            // key_expr invalid, subscriber closed mid-session) still
            // fire `state-root-sync-degraded` from event_loop.rs as
            // genuine degradation signals.
            //
            // ZEB-197: spawn the pairing state machine now that the
            // event loop is up. Construct ZenohPairingTransport with
            // a clone of publish_tx (publishes go through the running
            // event loop) and the receiver half of pairing_in. Stash
            // the handle on NodeState so stop_node can drop it.
            let install_pairing = {
                let guard = state.lock().map_err(|e| format!("lock error: {e}"))?;
                if guard.generation != our_gen {
                    // A newer start_node has raced us; do not install.
                    None
                } else {
                    guard.publish_tx.as_ref().cloned()
                }
            };
            if let Some(publish_tx_clone) = install_pairing {
                // ZEB-200: pairing without persistence is the exact UX hole
                // ZEB-197 closed — a successful pair would be silently
                // dropped on next start_node. Resolve up front and surface
                // any failure as a hard start_node error so the cleanup
                // hook below tears down the running event loop.
                let identity_dir = match crate::owner_commands::resolve_identity_dir() {
                    Ok(p) => p,
                    Err(e) => {
                        break 'arm Err(format!(
                            "cannot resolve identity_dir for pairing persistence: {e}"
                        ));
                    }
                };
                let pairing_transport: std::sync::Arc<
                    dyn crate::pairing::transport::PairingTransport,
                > = std::sync::Arc::new(
                    crate::pairing::zenoh_transport::ZenohPairingTransport::new(
                        publish_tx_clone,
                        pairing_in_rx,
                    ),
                );
                let mut pairing_handle = crate::pairing::state_machine::spawn_state_machine(
                    pairing_transport,
                    std::sync::Arc::new(|| {
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0)
                    }),
                    crate::pairing::state_machine::DEFAULT_DISCOVER_REBROADCAST_INTERVAL,
                );
                // Bridge pairing state changes to a Tauri frontend event.
                // Clone state_rx before moving the handle into NodeState.
                let mut prx = pairing_handle.state_rx.clone();
                let app_clone = app.clone();
                tokio::spawn(async move {
                    loop {
                        if prx.changed().await.is_err() {
                            break;
                        }
                        let s = prx.borrow().clone();
                        let _ = app_clone.emit("pairing-state-changed", s);
                    }
                });

                // ZEB-197 persistence drainers. The pairing state machine
                // emits {Joiner,Inviter}EnrollResult on Complete; without
                // these drainers the post-Complete state lives only in RAM
                // and the user's DevicesPanel reverts on next start_node.
                //
                // Receivers are taken out of the handle (mpsc receivers are
                // single-consumer, not Clone like watch::Receiver). The
                // drainer task owns each receiver until the SM shuts down.
                // ZEB-199: install_*_state acquires OWNER_STATE_WRITE_LOCK
                // (a std::sync::Mutex). Awaiting the persist call directly on
                // the runtime would block the executor thread for the full
                // load+merge+save window — measured at ~5-50ms per pair, but
                // longer under contention with mint or other persist callers.
                // Run the sync work on the dedicated blocking pool via
                // spawn_blocking so the runtime stays responsive (zenoh sync,
                // IPC, UI events). Mirrors the run_blocking pattern used by
                // mint in owner_commands.rs.
                if let Some(mut rx) = pairing_handle.joiner_result_rx.take() {
                    let id_dir = identity_dir.clone();
                    tokio::spawn(async move {
                        while let Some(result) = rx.recv().await {
                            let id_dir = id_dir.clone();
                            let outcome = tokio::task::spawn_blocking(move || {
                                crate::pairing::persist::install_joiner_state(&id_dir, result)
                            })
                            .await;
                            match outcome {
                                Ok(Ok(())) => {
                                    tracing::info!("joiner pairing persisted successfully");
                                }
                                Ok(Err(e)) => {
                                    tracing::error!("failed to persist joiner pairing result: {e}");
                                }
                                Err(e) => {
                                    tracing::error!("joiner persist task join failed: {e}");
                                }
                            }
                        }
                    });
                }
                if let Some(mut rx) = pairing_handle.inviter_result_rx.take() {
                    let id_dir = identity_dir.clone();
                    tokio::spawn(async move {
                        while let Some(result) = rx.recv().await {
                            let id_dir = id_dir.clone();
                            let outcome = tokio::task::spawn_blocking(move || {
                                crate::pairing::persist::install_inviter_state(&id_dir, result)
                            })
                            .await;
                            match outcome {
                                Ok(Ok(())) => {
                                    tracing::info!("inviter pairing persisted successfully");
                                }
                                Ok(Err(e)) => {
                                    tracing::error!(
                                        "failed to persist inviter pairing result: {e}"
                                    );
                                }
                                Err(e) => {
                                    tracing::error!("inviter persist task join failed: {e}");
                                }
                            }
                        }
                    });
                }

                if let Ok(mut guard) = state.lock() {
                    if guard.generation == our_gen {
                        guard.pairing_handle = Some(pairing_handle);
                    }
                    // else: a newer start_node has replaced us; drop the
                    // freshly spawned handle by letting it fall out of scope.
                }
            }
            let _ = app.emit(
                "zenoh-status",
                &ZenohStatus {
                    status: "connected".to_string(),
                    endpoint,
                    error: None,
                },
            );
            Ok(())
        }
        Ok(Err(e)) => Err(e),
        Err(_) => Err("runtime thread exited before reporting startup status".to_string()),
    };

    // On startup failure, clean up stale handles — but only if the
    // generation still matches. A newer start_node may have already
    // replaced our handles; passing our generation avoids tearing
    // down the newer node.
    if result.is_err() {
        let _ = stop_inner(&state, Some(our_gen));
    }

    result
}

/// Stop the harmony node and clean up.
#[tauri::command]
fn stop_node(app: AppHandle, state: tauri::State<'_, Mutex<NodeState>>) -> Result<(), String> {
    let gen = {
        let guard = state.lock().map_err(|e| format!("lock error: {e}"))?;
        guard.generation
    };
    let stopped = stop_inner(&state, Some(gen));
    // Only emit disconnected if we actually stopped a running node.
    if stopped {
        let _ = app.emit(
            "zenoh-status",
            &ZenohStatus {
                status: "disconnected".to_string(),
                endpoint: None,
                error: None,
            },
        );
    }
    Ok(())
}

// ── Legacy command aliases (backward compat with frontend) ───────────────

/// Alias: the frontend calls `connect_zenoh` — route to `start_node`.
#[tauri::command]
async fn connect_zenoh(
    endpoint: String,
    app: AppHandle,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    start_node(Some(endpoint), app, state).await
}

/// Alias: the frontend calls `disconnect_zenoh` — route to `stop_node`.
#[tauri::command]
fn disconnect_zenoh(
    app: AppHandle,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    stop_node(app, state)
}

/// Publish a profile to the mesh network via the event loop's Zenoh session.
#[tauri::command]
async fn publish_profile(
    profile: ProfilePayload,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    if profile.address.contains('/')
        || profile.address.contains('*')
        || profile.address.contains('?')
        || profile.address.contains('#')
        || profile.address.contains('$')
        || profile.address.is_empty()
    {
        return Err(format!("invalid address: {}", profile.address));
    }

    let publish_tx = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard
            .publish_tx
            .clone()
            .ok_or_else(|| "not connected".to_string())?
    };

    let key_expr = format!("harmony/profile/{}", profile.address);
    let payload = serde_json::to_vec(&profile).map_err(|e| format!("serialize: {e}"))?;

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    publish_tx
        .send(event_loop::PublishRequest {
            key_expr,
            payload,
            reply: reply_tx,
        })
        .await
        .map_err(|_| "event loop not running".to_string())?;

    reply_rx
        .await
        .map_err(|_| "event loop dropped publish request".to_string())?
}

/// Send a channel message to the mesh network via Zenoh pub/sub.
///
/// Publishes JSON to `harmony/community/{hub}/channels/{channel}`.
/// Other nodes subscribed to that key expression will receive the message
/// and emit it to their frontends as `message-received` events.
#[tauri::command]
async fn send_message(
    message: SendMessagePayload,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    // Validate channel/hub identifiers (same rules as profile address).
    for (label, val) in [("channel", &message.channel), ("hub", &message.hub)] {
        if val.is_empty()
            || val.contains('/')
            || val.contains('*')
            || val.contains('?')
            || val.contains('#')
            || val.contains('$')
        {
            return Err(format!("invalid {label}: {val}"));
        }
    }

    let (publish_tx, node_addr) = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        let tx = guard
            .publish_tx
            .clone()
            .ok_or_else(|| "not connected".to_string())?;
        (tx, guard.node_addr.clone())
    };

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let wire = ChannelMessagePayload {
        id: format!(
            "msg-{}-{now_ms}-{:08x}",
            &node_addr[..8.min(node_addr.len())],
            rand::random::<u32>()
        ),
        sender_address: node_addr.clone(),
        sender_name: message.sender_name.clone(),
        channel: message.channel.clone(),
        hub: message.hub.clone(),
        text: message.text,
        timestamp: now_ms,
        priority: message.priority,
        reply_to: message.reply_to,
    };

    let key_expr = format!(
        "harmony/community/{}/channels/{}",
        message.hub, message.channel
    );
    let payload = serde_json::to_vec(&wire).map_err(|e| format!("serialize: {e}"))?;

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    publish_tx
        .send(event_loop::PublishRequest {
            key_expr,
            payload,
            reply: reply_tx,
        })
        .await
        .map_err(|_| "event loop not running".to_string())?;

    reply_rx
        .await
        .map_err(|_| "event loop dropped publish request".to_string())?
}

/// IPC payload for `send_dm` — both ids the frontend needs to thread the
/// optimistic / `dm-received` / `read_dm_thread` paths together.
///
/// Why both ids:
///   - `messageCid` is the content hash. `dm-received` events and
///     `read_dm_thread` results both key on this; the frontend uses it as
///     the stable identity that survives "optimistic local message"
///     becoming "self-echo from the receive path" without a duplicate.
///   - `messageId` (OutboxEntryId) is the lifecycle handle. `dm-delivered`,
///     `dm-expired`, `dm-deleted`, and `delete_outbox_entry` all key on
///     this — it's the OutboxEntry primary key.
///
/// Returning only one would force the caller to either re-fetch the other
/// (TOCTOU window) or live with a 3-way dedupe failure between the
/// optimistic-local path, the dm-received path, and the cold-start
/// scrollback path. PR #81 review surfaced exactly that bug.
#[derive(serde::Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SendDmResult {
    /// Hex-encoded OutboxEntryId (16 bytes → 32 hex chars). Use for
    /// lifecycle correlation (dm-delivered / dm-expired / delete).
    pub message_id: String,
    /// Hex-encoded ContentId (32 bytes → 64 hex chars). Use as the
    /// stable cross-path message identity for dedupe.
    pub message_cid: String,
}

/// ZEB-225 Sub-B Phase 2: send a DM into a direct/group-DM Space.
///
/// Snapshots the DmOutbox/CRDT/HLC/ContentStore handles under the NodeState
/// sync mutex, releases it (before any `.await`), then orchestrates the
/// send: encrypt+CAS+apply_outbox via `DmOutbox::send_dm`, then bump the
/// HLC tracker so the next state-root publish stamps monotonically.
///
/// Lock order (mirror in event_loop drain): dm_outbox → crdt_state → hlc_tracker.
///
/// `space_id_hex` is the 32-character hex of a 16-byte SpaceId.
/// Returns `{ messageId, messageCid }` on success — see `SendDmResult`
/// for why both are surfaced.
// PR #81 round 4: param renamed from `space_id_hex` → `space_id`.
// Tauri 2's default JS→Rust convention auto-converts camelCase keys
// to snake_case (so JS `spaceId` resolves to Rust `space_id`). The
// previous `space_id_hex` name didn't match anything the frontend
// could send (would have required JS `spaceIdHex`), so the IPC was
// silently broken. The variable is still hex-encoded on the wire —
// the doc comment + downstream `hex::decode` make that clear.
#[tauri::command]
async fn send_dm(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    space_id: String,
    content: Vec<u8>,
    mime_type: String,
) -> Result<SendDmResult, String> {
    let space_id_hex = space_id;
    // Snapshot all handles under the sync mutex; release it before any await.
    // (Per ZEB-225 spec: NodeState sync-mutex must not be held across `.await`.)
    //
    // We also capture `generation` paired-atomically with the Arcs. If
    // stop_inner detaches the Arcs (sets to None) and start_node bumps the
    // generation while the work below is in flight, the Arcs we hold are
    // orphaned: they'll write into a `crdt_state` the new node never reads
    // from. The post-check at the bottom catches that and surfaces Err.
    let (
        dm_outbox,
        _dm_transport,
        crdt_state,
        hlc_tracker,
        device_id,
        _self_owner,
        cas,
        snapshot_generation,
    ) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.dm_outbox
                .clone()
                .ok_or("node not running or no owner identity")?,
            g.dm_transport.clone().ok_or("dm_transport missing")?,
            g.crdt_state.clone().ok_or("crdt_state missing")?,
            g.hlc_tracker.clone().ok_or("hlc_tracker missing")?,
            g.dm_device_id.clone().ok_or("dm_device_id missing")?,
            g.dm_self_owner.ok_or("dm_self_owner missing")?,
            g.content_store.clone().ok_or("content_store missing")?,
            g.generation,
        )
    };

    let space_bytes = hex::decode(&space_id_hex).map_err(|e| format!("space_id hex: {e}"))?;
    let space_arr: [u8; 16] = space_bytes
        .as_slice()
        .try_into()
        .map_err(|_| format!("space_id must be 16 bytes, got {}", space_bytes.len()))?;
    let space_id_typed = crate::owner_state_types::SpaceId(space_arr);

    let wall_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    // Lock order: dm_outbox → crdt_state → hlc_tracker.
    // Mirror this order in event_loop drain (T7) to avoid deadlock.
    let mut outbox_g = dm_outbox.lock().await;
    let mut state_g = crdt_state.lock().await;
    let mut tracker_g = hlc_tracker.lock().await;
    let prev_hlc = tracker_g.get(&device_id).cloned();

    let (msg_id, msg_cid) = outbox_g
        .send_dm(
            &mut state_g,
            cas.as_ref(),
            space_id_typed,
            content,
            mime_type,
            wall_now_ms,
            prev_hlc.as_ref(),
        )
        .await
        .map_err(|e| format!("send_dm: {e}"))?;

    // Read the HLC that DmOutbox::send_dm actually minted from the
    // just-inserted OutboxEntry. Single source of truth: if next_hlc's logic
    // ever changes (Phase 3b's planned ±20% jitter, etc.), the tracker stays
    // in lockstep automatically — the prior manual re-derivation here would
    // silently desync.
    let next_hlc = state_g
        .outbox
        .get(&msg_id)
        .map(|e| e.created_at.clone())
        .ok_or("send_dm minted entry not in outbox (apply_outbox rejected?)")?;
    tracker_g.insert(device_id, next_hlc);

    // Drop the per-handle locks before re-acquiring NodeState's sync mutex.
    drop(tracker_g);
    drop(state_g);
    drop(outbox_g);

    // Post-check: the work above mutated crdt_state via the cloned Arcs. If
    // a stop+restart fired during the .await chain, our crdt_state may now
    // be detached from the live NodeState — the new node won't see this
    // entry. Surface as Err so the caller can retry against the live node.
    //
    // KNOWN RACE (ZEB-234, deferred to pre-Phase-4): the mutation already
    // happened when this check runs. If stop_inner's SyncEngine::shutdown()
    // flushes the cloned crdt_state between apply_outbox and this post-check,
    // the entry is persisted + broadcast even though we report Err. A retry
    // against the new node mints a second OutboxEntry → recipient sees a
    // duplicate DM. The proper fix is a shutdown fence (in-flight permit
    // shared between send_dm and stop_inner). Phase 2 ships with this race
    // unaddressed because no UI flow concurrently triggers stop+send;
    // ZEB-234 lands the fence before Phase 4 frontend does.
    //
    // Residual TOCTOU within this code: a stop+restart between this post-
    // check and the IPC return still produces apparent success with an
    // orphaned entry. Same fix (ZEB-234) closes this window too.
    {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        if g.generation != snapshot_generation {
            return Err(format!(
                "node generation changed during send_dm (was {}, now {}); \
                 entry was written to a detached crdt_state and won't be \
                 drained — retry against the live node",
                snapshot_generation, g.generation
            ));
        }
        // stop_inner clears DM handles (dm_outbox, crdt_state, etc → None)
        // WITHOUT bumping `generation`. So a stop_node alone (no subsequent
        // start) leaves generation unchanged but handles None. The
        // generation-only check above misses that case; verify the handles
        // are still present too.
        if g.dm_outbox.is_none() {
            return Err("node was stopped during send_dm; entry was written to a \
                 detached crdt_state and won't be drained"
                .to_string());
        }
    }

    Ok(SendDmResult {
        message_id: hex::encode(msg_id.0),
        message_cid: hex::encode(msg_cid.to_bytes()),
    })
}

// ── ZEB-228 Phase 4: read_dm_thread (cold-start scrollback) ──────────────

/// Phase 4 cold-start scrollback IPC payload — one decrypted message in a
/// DM Space's history. Hex-encoded fields are sized for the Tauri JSON
/// channel (Vec<u8> would round-trip through base64; hex is what every
/// other DM-shaped payload in this codebase uses).
#[derive(serde::Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DmThreadMessage {
    /// Hex-encoded ContentId (32 bytes → 64 hex chars).
    pub message_cid: String,
    /// Hex-encoded sender OwnerAddr (16 bytes → 32 hex chars). For
    /// self-sent messages, equals self_owner; for received messages,
    /// equals the original sender's OwnerAddr.
    pub from: String,
    /// `MessagePayload.sent_at.wall_ms` — sender's HLC at compose time.
    pub sent_at: u64,
    /// `InboxEntry.received_at.wall_ms` — local HLC at apply_inbox time.
    /// Pagination cursor: callers pass the oldest entry's value as
    /// `before_hlc` to fetch the next page.
    pub received_at: u64,
    /// Hex-encoded plaintext body (decrypted from CAS storage_blob).
    pub body: String,
    pub mime_type: String,
    /// True iff `from == self_owner` — UI uses this to right-align the
    /// bubble + skip the avatar fetch for self.
    pub is_self_outbound: bool,
    /// For self-entries (`is_self_outbound == true`): the outbox-derived
    /// delivery state — `"sending" | "delivered" | "expired" | "failed"`.
    ///
    /// `"sending"` = OutboxEntry exists with `Pending` or `Partial` status.
    /// `"expired"` = OutboxEntry exists with `Expired` status.
    /// `"delivered"` = OutboxEntry exists with `Complete` status, OR the
    ///                 OutboxEntry is gone (post-Complete GC, which means
    ///                 it WAS delivered before being collected).
    /// `None` = received entry (`is_self_outbound == false`); receivers
    ///          don't track outbox state.
    ///
    /// `read_dm_thread` joins inbox→outbox by `(space_id, message_cid)`
    /// per call. Without this field a stuck-sending self-message would
    /// render as "delivered" in scrollback even while still in the
    /// outbox awaiting delivery — Qodo flagged that on PR #81 review.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery_state: Option<String>,
    /// For self-entries: hex-encoded `OutboxEntryId` (16 bytes → 32 hex
    /// chars), populated only when the OutboxEntry is still present in
    /// `state.outbox`. The frontend's `TextMessage.canDelete` requires
    /// `messageId !== undefined` to expose the inline ⓧ button — without
    /// this field, scrollback-loaded self-messages stuck in `'sending'`
    /// or `'expired'` couldn't be deleted after a cold restart (Cursor
    /// Bugbot flagged this on PR #81 review).
    ///
    /// `None` for: received entries (no outbox row), or self-entries
    /// whose OutboxEntry was already GC'd post-Complete (in which case
    /// `delivery_state == "delivered"` and there's nothing to delete).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
}

/// Pure helper: sort InboxEntries by `received_at` descending, drop entries
/// at or past the `before_hlc` cursor, then truncate to `limit`.
///
/// Extracted so `read_dm_thread` (the IPC) and `read_dm_thread_inner` (the
/// integration-test entry point) share one implementation. Pre-extraction,
/// each had its own copy of the sort+filter+truncate body — production
/// hit the IPC's copy while tests only ever hit the inner's, so a quiet
/// divergence (e.g., flipping the cursor comparison inclusivity) would
/// never be caught.
///
/// Lock-free + pure: takes owned entries by value so the caller can
/// gather them under whatever lock and drop the lock before invoking
/// this. Returns the truncated, sorted, cursor-filtered vec.
fn filter_sort_paginate_inbox(
    entries: Vec<crate::owner_state_types::InboxEntry>,
    before_hlc: Option<u64>,
    limit: usize,
) -> Vec<crate::owner_state_types::InboxEntry> {
    let mut entries = entries;
    // Sort by received_at descending. Hlc has no Ord impl, so compare on
    // the (wall_ms, logical, device_id) tuple — same lex ordering
    // `is_strictly_newer_than` uses. Newest-first ordering means we
    // call `b.cmp(&a)` (the inversion of natural ascending order) — the
    // tuple keys below are NAMED for the element they describe so a
    // future "fix" to align names with assignments doesn't silently
    // flip the sort direction (Cursor PR #81 round 4 review).
    entries.sort_by(|a, b| {
        let key_a = (
            a.received_at.wall_ms,
            a.received_at.logical,
            &a.received_at.device_id,
        );
        let key_b = (
            b.received_at.wall_ms,
            b.received_at.logical,
            &b.received_at.device_id,
        );
        key_b.cmp(&key_a) // descending: larger keys first
    });
    if let Some(cursor) = before_hlc {
        entries.retain(|e| e.received_at.wall_ms < cursor);
    }
    entries.truncate(limit);
    entries
}

/// Pure inner implementation of `read_dm_thread`. The `#[tauri::command]`
/// shim snapshots NodeState handles, drops the sync mutex, and calls this.
///
/// Behavior matches the IPC contract:
///   1. `space_id` MUST exist in `state.spaces`; otherwise `UnknownSpace`.
///   2. The Space MUST have a `content_key`; otherwise `MissingContentKey`.
///   3. InboxEntries are filtered to `space_id`, sorted by `received_at`
///      DESCENDING (newest first), the optional `before_hlc` cursor
///      filters out entries with `received_at.wall_ms >= cursor`, then
///      truncated to `limit`.
///   4. Each surviving InboxEntry's `message_cid` is fetched from CAS and
///      decrypted via `dm_crypto::decrypt_dm_message` with the prior-keys
///      fallback (matches `handle_cidnotify`'s receive path so post-key-
///      rotation scrollback works).
///   5. Any per-entry CAS miss (`Ok(None)`) or fetch error (`Err(_)`) or
///      decrypt failure surfaces as a single `Err` with the failing
///      message_cid in the message — caller can retry. (Partial-result
///      handling is a follow-up if needed; today's UI is fine with
///      "scrollback failed, retry".)
///
/// The pure-function shape lets integration tests exercise the decrypt +
/// pagination logic without standing up a tauri::State<NodeState>.
pub async fn read_dm_thread_inner(
    state: &crate::owner_state_crdt::OwnerState,
    cas: &dyn crate::content_store::ContentStore,
    space_id: crate::owner_state_types::SpaceId,
    limit: usize,
    before_hlc: Option<u64>,
    self_owner: crate::owner_state_types::OwnerAddr,
) -> Result<Vec<DmThreadMessage>, String> {
    let space = state
        .spaces
        .get(&space_id)
        .ok_or_else(|| format!("UnknownSpace({space_id:?})"))?;
    let content_key = space
        .content_key
        .clone()
        .ok_or_else(|| format!("MissingContentKey({space_id:?})"))?;
    let prior_content_keys = space.prior_content_keys.clone();
    let aad = crate::dm_crypto::compute_aad(space).map_err(|e| format!("compute_aad: {e}"))?;

    // Gather + filter+sort+paginate via the shared helper. All in
    // memory; no .await crosses the borrow of `state`.
    let raw: Vec<crate::owner_state_types::InboxEntry> =
        state.inbox_entries_for_space(space_id).cloned().collect();
    let entries = filter_sort_paginate_inbox(raw, before_hlc, limit);
    // Snapshot the outbox so decrypt_inbox_entries can populate per-entry
    // delivery_state without re-locking. Cheap (Phase 4 scale: tens of
    // entries max).
    let outbox_snapshot = state.outbox.clone();

    decrypt_inbox_entries(
        cas,
        &content_key,
        &prior_content_keys,
        &aad,
        entries,
        self_owner,
        &outbox_snapshot,
    )
    .await
}

/// ZEB-228 Phase 4 — Cold-start DM scrollback IPC.
///
/// Returns InboxEntries for a given Space (self-sent + received), each
/// with its decrypted body + mime_type. Reverse-chronological order by
/// `received_at`. Paginated via `limit` + `before_hlc` cursor:
///
/// - `space_id_hex`: 32-character hex of a 16-byte SpaceId.
/// - `limit`: max entries per page (UI page size, typical 50).
/// - `before_hlc`: if `Some(wall_ms)`, return entries with
///   `received_at.wall_ms < before_hlc`. None = newest first page.
///
/// Decryption uses `dm_crypto::decrypt_dm_message` with the prior-keys
/// fallback (matches `handle_cidnotify`'s receive path), so scrollback
/// after a content_key rotation still surfaces older messages encrypted
/// under the previous key.
///
/// Frontend uses this on first DM-channel switch to populate the
/// TextFeed with history. To paginate: pass the oldest entry's
/// `received_at` as `before_hlc` for the next call.
// Param rename per PR #81 round 4 — see send_dm above for rationale.
#[tauri::command]
async fn read_dm_thread(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    space_id: String,
    limit: usize,
    before_hlc: Option<u64>,
) -> Result<Vec<DmThreadMessage>, String> {
    let space_id_hex = space_id;
    // Snapshot handles under the sync mutex; release before any .await.
    // (Same pattern as send_dm — NodeState's sync mutex must not span
    // .await boundaries.)
    let (crdt_state, cas, self_owner) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.crdt_state
                .clone()
                .ok_or("node not running or no owner identity")?,
            g.content_store.clone().ok_or("content_store missing")?,
            g.dm_self_owner.ok_or("dm_self_owner missing")?,
        )
    };

    let space_bytes = hex::decode(&space_id_hex).map_err(|e| format!("space_id hex: {e}"))?;
    let space_arr: [u8; 16] = space_bytes
        .as_slice()
        .try_into()
        .map_err(|_| format!("space_id must be 16 bytes, got {}", space_bytes.len()))?;
    let space_id = crate::owner_state_types::SpaceId(space_arr);

    // Two-phase: gather everything we need under the OwnerState lock
    // (no .await), drop the lock, then run the cas.get + decrypt loop
    // unlocked. This honors the locks-across-await rule (mirrors ZEB-241
    // pending refactor's pattern).
    //
    // Filter+sort+paginate runs through the shared
    // `filter_sort_paginate_inbox` helper so production exercises the
    // exact code path the integration tests exercise via
    // `read_dm_thread_inner` — no silent divergence between the two.
    let (entries, content_key, prior_content_keys, aad, outbox_snapshot) = {
        let state_guard = crdt_state.lock().await;
        let space = state_guard
            .spaces
            .get(&space_id)
            .ok_or_else(|| format!("UnknownSpace({space_id:?})"))?;
        let content_key = space
            .content_key
            .clone()
            .ok_or_else(|| format!("MissingContentKey({space_id:?})"))?;
        let prior = space.prior_content_keys.clone();
        let aad = crate::dm_crypto::compute_aad(space).map_err(|e| format!("compute_aad: {e}"))?;

        let raw: Vec<crate::owner_state_types::InboxEntry> = state_guard
            .inbox_entries_for_space(space_id)
            .cloned()
            .collect();
        let entries = filter_sort_paginate_inbox(raw, before_hlc, limit);
        // Snapshot the outbox so the decrypt loop can populate
        // per-entry delivery_state without re-locking. Cheap (Phase 4
        // scale: tens of entries max).
        let outbox_snapshot = state_guard.outbox.clone();

        (entries, content_key, prior, aad, outbox_snapshot)
    };

    decrypt_inbox_entries(
        cas.as_ref(),
        &content_key,
        &prior_content_keys,
        &aad,
        entries,
        self_owner,
        &outbox_snapshot,
    )
    .await
}

/// Helper: fetch + decrypt a pre-filtered + pre-sorted slice of
/// InboxEntries. Shared between the `tauri::command` (which gathers
/// entries under the OwnerState lock and drops it before calling this)
/// and `read_dm_thread_inner` (which the integration tests use without
/// a NodeState).
///
/// `outbox_snapshot` is a pre-cloned view of `state.outbox` so the
/// delivery-state join can run without re-acquiring the OwnerState lock.
/// Cheap at Phase 4 scale (tens of entries); if outbox grows we'd swap
/// for a `(space_id, message_cid) → status` index built once per call.
async fn decrypt_inbox_entries(
    cas: &dyn crate::content_store::ContentStore,
    content_key: &crate::owner_state_types::DmContentKey,
    prior_content_keys: &[crate::owner_state_types::DmContentKey],
    aad: &[u8],
    entries: Vec<crate::owner_state_types::InboxEntry>,
    self_owner: crate::owner_state_types::OwnerAddr,
    outbox_snapshot: &std::collections::BTreeMap<
        crate::owner_state_types::OutboxEntryId,
        crate::owner_state_types::OutboxEntry,
    >,
) -> Result<Vec<DmThreadMessage>, String> {
    let mut out: Vec<DmThreadMessage> = Vec::with_capacity(entries.len());
    for entry in entries {
        // PR #81 round 4 (Greptile P2): per-entry skip-on-error instead
        // of aborting the whole page. A single corrupted CAS blob or a
        // missing one (e.g. mid-sync state) shouldn't black-hole the
        // user's entire scrollback. Log the failure + continue; the UI
        // sees N-1 messages instead of zero. Future polish: surface a
        // placeholder "decrypt failed" Message stub so the user knows
        // something exists at that slot. Out of Phase 4 scope.
        let blob = match cas.get(&entry.message_cid).await {
            Ok(Some(b)) => b,
            Ok(None) => {
                tracing::warn!(
                    message_cid = ?entry.message_cid,
                    "read_dm_thread: blob missing in CAS — skipping entry"
                );
                continue;
            }
            Err(e) => {
                tracing::warn!(
                    message_cid = ?entry.message_cid,
                    error = ?e,
                    "read_dm_thread: cas.get failed — skipping entry"
                );
                continue;
            }
        };
        let payload =
            match crate::dm_crypto::decrypt_dm_message(content_key, prior_content_keys, aad, &blob)
            {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(
                        message_cid = ?entry.message_cid,
                        error = ?e,
                        "read_dm_thread: decrypt failed — skipping entry"
                    );
                    continue;
                }
            };
        let is_self_outbound = entry.from == self_owner;
        // Self entries: join against outbox by (space_id, message_cid).
        // Outbox is keyed by OutboxEntryId so we walk values; Phase 4
        // scale makes the linear scan acceptable.
        //
        // Missing outbox entry on a self-message → "delivered" (the
        // OutboxEntry was GC'd post-Complete, meaning it definitely
        // WAS delivered before collection). This is the same fallback
        // the frontend's loadDmThread used to apply unconditionally;
        // we narrow it here so Pending/Partial/Expired surface
        // accurately.
        // For self entries, capture BOTH the OutboxEntryId (so the
        // frontend can correlate dm-delivered/expired/deleted IPC events
        // and gate the delete button) AND the delivery_status (so
        // scrollback reflects current outbox state, not a hardcoded
        // 'delivered'). One linear scan over the snapshot serves both.
        let (delivery_state, message_id) = if is_self_outbound {
            let hit = outbox_snapshot.iter().find_map(|(id, e)| {
                if e.space_id == entry.space_id && e.message_cid == entry.message_cid {
                    Some((*id, e.delivery_status))
                } else {
                    None
                }
            });
            let state = match hit.map(|(_, s)| s) {
                Some(crate::owner_state_types::DeliveryStatus::Pending)
                | Some(crate::owner_state_types::DeliveryStatus::Partial) => "sending",
                Some(crate::owner_state_types::DeliveryStatus::Expired) => "expired",
                Some(crate::owner_state_types::DeliveryStatus::Complete) | None => "delivered",
            };
            (
                Some(state.to_string()),
                hit.map(|(id, _)| hex::encode(id.0)),
            )
        } else {
            (None, None)
        };
        out.push(DmThreadMessage {
            message_cid: hex::encode(entry.message_cid.to_bytes()),
            from: hex::encode(entry.from.0),
            sent_at: payload.sent_at.wall_ms,
            received_at: entry.received_at.wall_ms,
            body: hex::encode(&payload.body),
            mime_type: payload.mime_type,
            is_self_outbound,
            delivery_state,
            message_id,
        });
    }
    Ok(out)
}

// ── ZEB-228 Phase 4: delete_outbox_entry (manual delete) ─────────────────

/// Phase 4 — Delete a stuck or expired DM message (manual delete).
///
/// Wraps `DmOutbox::delete_dm_outbox_entry`. Removes BOTH the
/// OutboxEntry and the corresponding self-InboxEntry keyed by
/// `(space_id, message_cid)`, and clears in-flight + backoff caches.
/// On success with a non-default outcome, emits a `dm-deleted` IPC
/// event so the frontend MessageService can prune the message from
/// its local cache.
///
/// Idempotent: a missing `message_id` returns `Ok(())` without
/// emitting any event.
///
/// `message_id` is the 32-character hex of a 16-byte OutboxEntryId.
#[tauri::command]
async fn delete_outbox_entry(
    app: tauri::AppHandle,
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    message_id: String,
) -> Result<(), String> {
    // Snapshot handles under the sync mutex; release before any .await.
    // Same pattern as send_dm — NodeState's sync mutex must not span
    // .await boundaries.
    //
    // ZEB-245 (PR #81 round 6): capture `generation` paired-atomically
    // with the Arcs so the post-stop check below can detect a
    // stop+restart racing through this command. See send_dm for the
    // full rationale on why both `generation` and handle-attachment
    // need to be re-verified.
    let (dm_outbox, crdt_state, snapshot_generation) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.dm_outbox
                .clone()
                .ok_or("node not running or no owner identity")?,
            g.crdt_state.clone().ok_or("crdt_state missing")?,
            g.generation,
        )
    };

    // Decode message_id from hex → OutboxEntryId.
    let id_bytes = hex::decode(&message_id).map_err(|e| format!("message_id hex: {e}"))?;
    let id_arr: [u8; 16] = id_bytes
        .as_slice()
        .try_into()
        .map_err(|_| format!("message_id must be 16 bytes, got {}", id_bytes.len()))?;
    let outbox_entry_id = crate::owner_state_types::OutboxEntryId(id_arr);

    // Lock order: dm_outbox → crdt_state. Mirrors send_dm to avoid
    // deadlock against any concurrent send/drain.
    let outcome = {
        let mut outbox_g = dm_outbox.lock().await;
        let mut state_g = crdt_state.lock().await;
        outbox_g
            .delete_dm_outbox_entry(&mut state_g, outbox_entry_id)
            .map_err(|e| format!("delete_dm_outbox_entry: {e}"))?
    };

    // Locks dropped (block scope ended).
    //
    // ZEB-245 (PR #81 round 6): post-stop check before emitting
    // dm-deleted. If a stop+restart fired during the .await chain
    // above, our cloned `crdt_state` Arc is now detached from the
    // live NodeState — the deletion landed in an orphan that won't
    // be persisted. Emitting dm-deleted in that case would prune the
    // message from the UI even though it'll reappear on next start.
    // Surface as Err instead so the caller (App.svelte's deleteDm)
    // can re-show the message + retry against the live node.
    //
    // Mirrors send_dm's fence (lib.rs ~1762): same residual TOCTOU
    // applies — a stop_inner that flushes the cloned crdt_state
    // between mutate and post-check still persists the deletion, so
    // ZEB-234's shutdown fence is the real fix. This guard closes the
    // common case (stop_node alone, no flush) which is the only
    // detach path Phase 4 UI can actually trigger.
    {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        if g.generation != snapshot_generation {
            return Err(format!(
                "node generation changed during delete_outbox_entry (was {}, now {}); \
                 deletion was applied to a detached crdt_state and won't be persisted — \
                 retry against the live node",
                snapshot_generation, g.generation
            ));
        }
        if g.dm_outbox.is_none() {
            return Err("node was stopped during delete_outbox_entry; deletion was \
                applied to a detached crdt_state and won't be persisted"
                .to_string());
        }
    }

    // Emit IPC event only if something actually changed (idempotent
    // missing-id is no-op).
    if let (Some(space_id), Some(message_cid)) = (outcome.space_id, outcome.message_cid) {
        let _ = app.emit(
            "dm-deleted",
            serde_json::json!({
                "messageId": message_id,
                "spaceId": hex::encode(space_id.0),
                "messageCid": hex::encode(message_cid.to_bytes()),
            }),
        );
    }

    Ok(())
}

// ── ZEB-228 Phase 4: add_space (DM/GroupDm creation) ─────────────────────

/// Pure inner implementation of `add_space`'s DM/GroupDm dispatch. The
/// `#[tauri::command]` shim snapshots NodeState handles, drops the sync
/// mutex, calls this, then forwards each `UnicastSendRequest` into the
/// outbound unicast channel.
///
/// Behavior:
///   1. Validate kind ∈ {Dm, GroupDm} and the recipient list:
///      - Dm: exactly 1 recipient (total members = 2).
///      - GroupDm: 2-15 recipients (total members = 3-16).
///      - No self in recipients.
///      - No duplicate recipients.
///   2. Generate a fresh content_key (32 random bytes via OsRng, wrapped
///      in `Zeroizing` while in scope).
///   3. Build a Space CRDT entry with sorted self+recipient members,
///      Reticulum transport (empty participants — populated lazily as
///      announces flow), `created_at`/`updated_at` from a fresh HLC.
///   4. Apply locally via `apply_space_with_canonicalization`.
///   5. Build a signed `DmInvite` packet and emit one
///      `UnicastSendRequest` per device in each recipient's
///      `OwnerDeviceCache` entry. Best-effort: a recipient with no
///      cached devices yields zero outbound packets — Phase 3b's
///      handle_invite-on-first-send_dm path still recovers because the
///      sender's outbox loop will fan out the missing invite when the
///      first message ships.
///
/// Returns `(canonical_space_id, send_requests, was_merge)`:
///   - `canonical_space_id` is the SpaceId after CRDT canonicalization.
///     If `apply_space_with_canonicalization` merged the freshly-minted
///     Space into an existing one with the same dedupe key (sorted
///     members), this is the EXISTING Space's id, not the minted one
///     that just got dropped — the outer command's `state.spaces.get(&id)`
///     readback would otherwise miss a real winner.
///   - `send_requests` is the list of DmInvite UnicastSendRequests.
///     Empty when `was_merge == true` because the existing Space's
///     invites were already sent at original creation time; sending
///     duplicates here would just generate redundant network traffic
///     and (for the recipient) noisy duplicate-invite handling.
///   - `was_merge` lets the caller skip the dispatch loop without
///     re-checking the sends vec emptiness for the duplicate-create
///     case (a fresh Space with zero recipients in OwnerDeviceCache
///     also produces an empty sends list — those two empties have
///     different semantics).
///
/// The pure-function shape lets integration tests exercise the
/// validation + Space-construction + invite-build logic without
/// standing up a tauri::State<NodeState>.
#[allow(clippy::too_many_arguments)]
pub fn add_space_dm_inner(
    state: &mut crate::owner_state_crdt::OwnerState,
    signing_key: &ed25519_dalek::SigningKey,
    inviter_identity_pub: &[u8; 64],
    self_owner: crate::owner_state_types::OwnerAddr,
    our_signing_device_hash: crate::owner_state_types::DeviceIdentityHash,
    device_id: &str,
    kind: crate::owner_state_types::SpaceKind,
    name: String,
    recipients: Vec<crate::owner_state_types::OwnerAddr>,
    wall_now_ms: u64,
    prev_hlc: Option<&crate::owner_state_types::Hlc>,
) -> Result<
    (
        crate::owner_state_types::SpaceId,
        Vec<crate::dm_outbox::UnicastSendRequest>,
        bool,
    ),
    String,
> {
    use crate::owner_state_types::{
        DmContentKey, OwnerAddr, ReticulumDest, Space, SpaceId, SpaceKind, TransportBinding,
    };

    // ── 1. Validate kind + recipients. ───────────────────────────────
    if !matches!(kind, SpaceKind::Dm | SpaceKind::GroupDm) {
        return Err(format!(
            "add_space_dm_inner only handles Dm/GroupDm; got {kind:?}"
        ));
    }
    if recipients.contains(&self_owner) {
        return Err("self must not be in recipients (backend adds self automatically)".to_string());
    }
    // Defense in depth — frontend already blocks but enforce here too.
    let total_members = 1 + recipients.len();
    if total_members > 16 {
        return Err(format!(
            "DM/GroupDm cap is 16 members; got {total_members} (use a community for larger groups)"
        ));
    }
    match kind {
        SpaceKind::Dm => {
            if recipients.len() != 1 {
                return Err(format!(
                    "Dm kind requires exactly 1 recipient; got {} (use GroupDm for 2-15)",
                    recipients.len()
                ));
            }
        }
        SpaceKind::GroupDm => {
            if !(2..=15).contains(&recipients.len()) {
                return Err(format!(
                    "GroupDm requires 2-15 recipients; got {}",
                    recipients.len()
                ));
            }
        }
        _ => unreachable!("kind already restricted to Dm or GroupDm above"),
    }

    // ── 2. Build sorted+deduped member list (self + recipients). ─────
    let mut all_members: Vec<OwnerAddr> = std::iter::once(self_owner)
        .chain(recipients.iter().copied())
        .collect();
    all_members.sort();
    all_members.dedup();
    if all_members.len() != total_members {
        return Err("duplicate recipient(s) in input".to_string());
    }

    // ── 3. Generate fresh content_key. Bytes live in `Zeroizing` for
    //       the duration of this scope; `DmContentKey::new` copies the
    //       bytes into its own (also-zeroize-on-drop) wrapper. ───────
    let content_key = {
        use rand::RngCore;
        use zeroize::Zeroizing;
        let mut k = Zeroizing::new([0u8; 32]);
        rand::rngs::OsRng.fill_bytes(k.as_mut());
        DmContentKey::new(*k)
    };

    // ── 4. Mint HLCs for created_at / updated_at. Both stamped from
    //       the same `next_hlc` so a peer comparing them by lex-order
    //       sees them as equal (the typical case for fresh creation).
    //       The IPC shim's caller is responsible for keeping the HLC
    //       tracker monotone post-mint; this inner function doesn't
    //       touch the tracker. ───────────────────────────────────────
    let creation_hlc = crate::dm_outbox::next_hlc(prev_hlc, wall_now_ms, device_id);

    // ── 5. Build the Space CRDT entry. ───────────────────────────────
    let space_id = SpaceId(rand::random());
    let space = Space {
        id: space_id,
        kind,
        parent: None,
        community_id: None,
        name,
        members: all_members.clone(),
        // DM kinds always Reticulum; participants populated lazily as
        // announces propagate (Phase 3b currently leaves it empty —
        // resolution happens via OwnerDeviceCache, not the Space's
        // transport binding).
        transport: Some(TransportBinding::Reticulum {
            participants: Vec::<ReticulumDest>::new(),
        }),
        custom_name: None,
        notification_pref: None,
        left_at: None,
        created_at: creation_hlc.clone(),
        updated_at: creation_hlc.clone(),
        content_key: Some(content_key.clone()),
        prior_content_keys: vec![],
        membership_key: None,
        admin_addr: None,
        is_invite_only: None,
    };

    // Validate invariants up front — catches programmer error before we
    // mutate state. apply_space_with_canonicalization itself does NOT
    // validate (it's the receive path's job, and incoming entries from
    // remote replicas are guarded by their own decode-time checks).
    space
        .validate_invariants()
        .map_err(|e| format!("Space invariants violated: {}", e.0))?;

    // ── 6. Apply locally. apply_space_with_canonicalization returns
    //       an ApplyOutcome; we MUST check it to learn whether a dedupe
    //       merge collapsed our minted Space into an existing one (same
    //       sorted members). The earlier shape ignored the outcome and
    //       returned `space_id` unconditionally — when the merge
    //       dropped our minted id (we lost the ULID tie-break), the
    //       outer command's `state.spaces.get(&result.0)` readback
    //       missed the live entry entirely. Qodo flagged this on PR #81.
    // ───────────────────────────────────────────────────────────────
    use crate::owner_state_crdt::ApplyOutcome;
    let outcome = state.apply_space_with_canonicalization(space);
    let (canonical_space_id, was_merge) = match outcome {
        ApplyOutcome::Inserted => (space_id, false),
        ApplyOutcome::Merged { old_id: None } => {
            // Same-SpaceId update path. Our minted id collided with an
            // existing entry on the same id — practically impossible
            // for a fresh `rand::random()` SpaceId (16 bytes of
            // randomness collision). Treat as Inserted-equivalent for
            // the canonical id, but skip dispatch: the existing entry
            // already had its invites sent at original creation.
            (space_id, true)
        }
        ApplyOutcome::Merged {
            old_id: Some(loser),
        } => {
            // Cross-id dedupe merge: lex-MIN ULID wins. Our minted id
            // may be the winner OR the loser. The winner is the unique
            // entry now in `state.spaces` matching our dedupe key
            // (sorted members + kind). Walk and find it.
            //
            // Skip dispatch in this case: the existing Space's invites
            // were already sent at original creation, and re-firing
            // here would just generate redundant network traffic +
            // noisy duplicate-invite handling on the recipient side.
            let canonical = state
                .spaces
                .iter()
                .find(|(_, s)| s.kind == kind && s.members == all_members)
                .map(|(id, _)| *id)
                .ok_or_else(|| {
                    format!(
                        "add_space_dm_inner: post-merge canonical winner not found \
                         (loser={loser:?}, members={all_members:?})"
                    )
                })?;
            (canonical, true)
        }
        ApplyOutcome::Rejected(reason) => {
            return Err(format!(
                "add_space_dm_inner: apply_space_with_canonicalization rejected: {reason}"
            ));
        }
    };

    // Short-circuit on merge: nothing to dispatch (existing Space
    // already invited everyone at original creation). Returning
    // `was_merge=true` lets the outer command skip the dispatch loop
    // on a more strongly typed signal than "sends is empty" (which
    // would conflate the merge case with the legitimate "no recipient
    // devices known yet" case).
    if was_merge {
        return Ok((canonical_space_id, Vec::new(), true));
    }

    // ── 7. Build + sign the DmInvite. Our own devices come from
    //       OwnerDeviceCache (populated by Flow A); fall back to just
    //       our_signing_device_hash if no entry yet (pre-bootstrap). ──
    let our_devices: Vec<crate::owner_state_types::DeviceIdentityHash> = state
        .owner_device_cache
        .devices
        .get(&self_owner)
        .map(|e| e.devices.clone())
        .unwrap_or_else(|| vec![our_signing_device_hash]);
    // Defense in depth — sender_devices MUST contain signing_device_hash
    // (Phase 3b invariant; validated wire-side by decode_packet).
    let sender_devices = if our_devices.contains(&our_signing_device_hash) {
        our_devices
    } else {
        let mut combined = our_devices;
        combined.push(our_signing_device_hash);
        combined.sort();
        combined.dedup();
        combined
    };

    let signed_invite = crate::dm_envelope::DmInviteSigned {
        space_id: canonical_space_id,
        kind,
        members: all_members,
        inviter: self_owner,
        inviter_identity_pub: *inviter_identity_pub,
        content_key,
        sender_devices,
        signing_device_hash: our_signing_device_hash,
        created_at: creation_hlc,
    };
    let invite_packet = crate::dm_envelope::build_signed_invite(signed_invite, signing_key)
        .map_err(|e| format!("build_signed_invite: {e}"))?;
    let invite_wire = crate::dm_envelope::encode_packet(&invite_packet)
        .map_err(|e| format!("encode_packet: {e}"))?;

    // ── 8. One UnicastSendRequest per non-self recipient device. ─────
    // Note: we hold a borrow of `state.owner_device_cache` here, which
    // is fine because the `apply_space_with_canonicalization` write
    // above already returned.
    let mut sends: Vec<crate::dm_outbox::UnicastSendRequest> = Vec::new();
    for r in &recipients {
        let entry = match state.owner_device_cache.devices.get(r) {
            Some(e) => e,
            None => continue, // recipient unknown — outbox loop on first send_dm recovers
        };
        for device in &entry.devices {
            let dest_hash = crate::dm_signing::compute_dm_destination_hash(device.0);
            sends.push(crate::dm_outbox::UnicastSendRequest {
                destination_hash: dest_hash,
                packet: invite_wire.clone(),
            });
        }
    }

    Ok((canonical_space_id, sends, false))
}

/// ZEB-228 Phase 4 — Create a new Space.
///
/// For DM/GroupDm kinds: generates a fresh content_key, builds the
/// Space CRDT entry with members (self + recipients), applies it
/// locally, and dispatches a signed DmInvite to each recipient's
/// known devices via the unicast channel. Returns the new SpaceId
/// (hex-encoded).
///
/// Validation:
///   - DM kind = exactly 1 recipient (total members = 2).
///   - GroupDm kind = 2-15 recipients (total members = 3-16).
///   - Total members ≤ 16 (defense in depth — frontend also blocks).
///   - No self in recipients (caller passes recipients only; backend
///     adds self automatically).
///   - No duplicate recipients.
///
/// Other kinds (Folder, Channel, Community, PublicChannel) are not
/// yet implemented in this IPC and return Err — they have their own
/// dedicated flows (e.g., `create_folder`).
///
/// Frontend's DmCreateDialog calls this; the dispatched DmInvite
/// flows through Phase 3b's `handle_invite` on each recipient's
/// device, which auto-accepts and writes the Space + cache entry.
#[tauri::command]
async fn add_space(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    kind: String,
    name: String,
    members: Option<Vec<String>>,
) -> Result<String, String> {
    use crate::owner_state_types::{OwnerAddr, SpaceKind};

    // Parse the kind string. Accept the same wire codes the SpaceKind
    // serde-rename uses ("d", "g") AND the human-friendly forms the
    // frontend will probably send ("dm", "group-dm").
    let parsed_kind = match kind.as_str() {
        "d" | "dm" | "Dm" => SpaceKind::Dm,
        "g" | "group-dm" | "groupdm" | "GroupDm" => SpaceKind::GroupDm,
        // Other kinds are not implemented in this IPC yet (Phase 4
        // ships DM/GroupDm only). Surface as a clear Err so a future
        // frontend that tries to call add_space for, e.g., a folder
        // gets a useful diagnostic rather than silent acceptance.
        other => {
            return Err(format!(
                "add_space: unsupported kind '{other}' (Phase 4 ships Dm/GroupDm only)"
            ));
        }
    };

    // Decode each recipient OwnerAddr from hex.
    let recipients: Vec<OwnerAddr> = members
        .unwrap_or_default()
        .iter()
        .map(|hex_addr| {
            let bytes = hex::decode(hex_addr)
                .map_err(|e| format!("recipient '{hex_addr}' hex decode: {e}"))?;
            let arr: [u8; 16] = bytes.as_slice().try_into().map_err(|_| {
                format!(
                    "recipient '{hex_addr}' must be 16 bytes, got {}",
                    bytes.len()
                )
            })?;
            Ok(OwnerAddr(arr))
        })
        .collect::<Result<Vec<_>, String>>()?;

    // Snapshot all handles under the sync mutex; release before any
    // .await. (Same pattern as send_dm — NodeState's sync mutex must
    // not span .await boundaries.)
    //
    // ZEB-245 (PR #81 round 6): capture `generation` paired-atomically
    // with the Arcs so the post-stop check below can detect a
    // stop+restart racing through this command — see send_dm for the
    // full rationale on why both `generation` and handle-attachment
    // need to be re-verified.
    let (
        dm_outbox,
        crdt_state,
        hlc_tracker,
        device_id,
        self_owner,
        unicast_send_tx,
        identity_pub_64,
        snapshot_generation,
    ) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.dm_outbox
                .clone()
                .ok_or("node not running or no owner identity")?,
            g.crdt_state.clone().ok_or("crdt_state missing")?,
            g.hlc_tracker.clone().ok_or("hlc_tracker missing")?,
            g.dm_device_id.clone().ok_or("dm_device_id missing")?,
            g.dm_self_owner.ok_or("dm_self_owner missing")?,
            g.unicast_send_tx.clone().ok_or("unicast_send_tx missing")?,
            g.dm_identity_pub_64
                .ok_or("dm_identity_pub_64 missing (start_node didn't capture it?)")?,
            g.generation,
        )
    };

    let wall_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    // Lock order mirrors send_dm: dm_outbox → crdt_state → hlc_tracker.
    // We borrow signing_key + our_signing_device_hash from DmOutbox so
    // we don't double-store identity-derived material on NodeState.
    //
    // `was_merge` from the inner function tells us whether
    // apply_space_with_canonicalization collapsed our minted Space into
    // an existing one with the same dedupe key. In that case `space_id`
    // is the EXISTING (canonical winner) id — guaranteed live in
    // state.spaces — and `sends` is empty (the existing Space was
    // already invited at original creation).
    let (space_id, sends, was_merge, new_hlc) = {
        let outbox_g = dm_outbox.lock().await;
        let mut state_g = crdt_state.lock().await;
        let mut tracker_g = hlc_tracker.lock().await;
        let prev_hlc = tracker_g.get(&device_id).cloned();

        let signing_key = outbox_g.signing_key.as_ref();
        let our_signing_device_hash = outbox_g.our_signing_device_hash;

        let (canonical_id, sends, was_merge) = add_space_dm_inner(
            &mut state_g,
            signing_key,
            &identity_pub_64,
            self_owner,
            our_signing_device_hash,
            &device_id,
            parsed_kind,
            name,
            recipients,
            wall_now_ms,
            prev_hlc.as_ref(),
        )?;

        // Fetch the HLC stamped on the canonical Space — single source
        // of truth. The inner function guarantees `canonical_id` is
        // present in `state.spaces` post-apply (Inserted, same-id
        // Merged, or cross-id Merged: in all three the canonical id
        // is the live entry); the get() below should never fail.
        let stamped = state_g
            .spaces
            .get(&canonical_id)
            .map(|s| s.created_at.clone())
            .ok_or_else(|| {
                "add_space: canonical Space not in state \
                 (apply_space_with_canonicalization invariants broken?)"
                    .to_string()
            })?;
        // HLC monotonicity: a dedupe-merge can land on an EXISTING Space
        // whose `created_at` is OLDER than our local tracker's prev_hlc
        // (the existing Space was created before our most recent HLC-
        // stamping operation). Writing `stamped` to the tracker in that
        // case would regress the cursor below prev_hlc, breaking the
        // monotonicity next_hlc relies on. Only update the tracker when
        // `stamped` strictly advances from prev_hlc; otherwise leave the
        // tracker as-is. (CodeRabbit flagged this on PR #81 review.)
        let should_advance_tracker = match prev_hlc.as_ref() {
            None => true,
            Some(prev) => stamped.is_strictly_newer_than(prev),
        };
        if should_advance_tracker {
            tracker_g.insert(device_id.clone(), stamped.clone());
        }

        (canonical_id, sends, was_merge, stamped)
    };
    let _ = new_hlc; // borrowed only to pin the tracker update timing

    // ZEB-245 (PR #81 round 6): post-stop check BEFORE dispatching
    // invites. If a stop+restart fired during the .await chain above,
    // our cloned `crdt_state` Arc is now detached from the live
    // NodeState — the Space landed in an orphan that won't be
    // persisted, but if we still dispatched invites the recipients
    // would auto-accept and ship messages to a Space the sender lost
    // on restart (cross-device divergence). Suppressing the dispatch
    // when we detect detachment closes the worst-case asymmetry.
    //
    // Mirrors send_dm's fence (lib.rs ~1762) and delete_outbox_entry's
    // fence below: same residual TOCTOU applies — a stop_inner that
    // flushes the cloned crdt_state between mutate and post-check
    // still persists the Space + invites, so ZEB-234's shutdown fence
    // is the real fix. This guard closes the common case (stop_node
    // alone, no flush) which is the only detach path Phase 4 UI can
    // actually trigger.
    {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        if g.generation != snapshot_generation {
            return Err(format!(
                "node generation changed during add_space (was {}, now {}); \
                 Space was created in a detached crdt_state and won't be persisted — \
                 invites suppressed; retry against the live node",
                snapshot_generation, g.generation
            ));
        }
        if g.dm_outbox.is_none() {
            return Err("node was stopped during add_space; Space was created in a \
                detached crdt_state and won't be persisted — invites suppressed"
                .to_string());
        }
    }

    // Dispatch invites only when our minted Space actually became the
    // live entry (or extended one with same id). When `was_merge==true`
    // the existing Space's invites were already dispatched at original
    // creation; firing them again here would just generate redundant
    // network traffic + noisy duplicate-invite handling on the
    // recipient side. `sends` is also empty in that case (defense in
    // depth — the inner function guarantees this), but the explicit
    // flag check makes the semantics unambiguous.
    if !was_merge {
        // Best-effort try_send — a full channel surfaces as a dropped
        // invite, recovered by the outbox loop's first send_dm into
        // this Space (which builds + ships its own DmInvite).
        for req in sends {
            if let Err(e) = unicast_send_tx.try_send(req) {
                tracing::warn!(
                    error = %e,
                    "add_space: dropped DmInvite dispatch (channel full); outbox retry on first send_dm will recover"
                );
            }
        }
    }

    Ok(hex::encode(space_id.0))
}

/// Return the hex-encoded node address (derived from the Ed25519 identity).
///
/// The frontend uses this to identify self-sent messages in the Zenoh echo.
#[tauri::command]
fn get_node_addr(state: tauri::State<'_, Mutex<NodeState>>) -> Result<String, String> {
    let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
    if guard.node_addr.is_empty() {
        return Err("not connected".to_string());
    }
    Ok(guard.node_addr.clone())
}

// ── Vine types and commands ──────────────────────────────────────────────

/// Vine descriptor published/received over Zenoh.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VineDescriptorPayload {
    pub id: String,
    pub creator_address: String,
    pub creator_name: String,
    pub created_at: u64,
    pub video_cid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reshare_of: Option<String>,
}

/// Vine descriptor sent from the frontend to publish.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishVinePayload {
    pub video_cid: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub reshare_of: Option<String>,
    /// Creator's display name (included so receivers can display it).
    #[serde(default)]
    pub creator_name: String,
}

/// Vine video descriptor returned to the frontend (includes local viewed state).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VineVideoDto {
    pub id: String,
    pub creator_address: String,
    pub creator_name: String,
    pub created_at: u64,
    pub video_cid: String,
    pub title: Option<String>,
    pub reshare_of: Option<String>,
    pub viewed: bool,
}

/// Response returned by list_followed — one entry per followed address.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FollowEntryResponse {
    pub address: String,
    pub name: Option<String>,
    pub followed_at: u64,
}

/// Vine reaction published/received over Zenoh.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VineReactionPayload {
    pub vine_id: String,
    pub reactor_address: String,
    pub reactor_name: String,
    pub liked: bool,
    pub timestamp: u64,
}

/// Vine reaction sent from the frontend to publish.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishReactionPayload {
    pub vine_id: String,
    pub vine_creator_address: String,
    pub liked: bool,
    #[serde(default)]
    pub reactor_name: String,
}

/// Publish a vine descriptor to the mesh network via Zenoh pub/sub.
///
/// Publishes JSON to `harmony/vines/{creator_address}`.
/// Other nodes subscribed to `harmony/vines/*` will receive the descriptor
/// and emit it to their frontends as `vine-received` events.
#[tauri::command]
async fn publish_vine(
    vine: PublishVinePayload,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    if vine.video_cid.trim().is_empty() {
        return Err("video_cid is required".to_string());
    }
    if let Some(ref title) = vine.title {
        if title.len() > 140 {
            return Err("title exceeds 140 bytes".to_string());
        }
    }

    let (publish_tx, node_addr) = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        let tx = guard
            .publish_tx
            .clone()
            .ok_or_else(|| "not connected".to_string())?;
        (tx, guard.node_addr.clone())
    };

    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let wire = VineDescriptorPayload {
        id: format!(
            "vine-{}-{now_secs}-{:08x}",
            &node_addr[..8.min(node_addr.len())],
            rand::random::<u32>()
        ),
        creator_address: node_addr.clone(),
        creator_name: vine.creator_name,
        created_at: now_secs,
        video_cid: vine.video_cid,
        title: vine.title,
        reshare_of: vine.reshare_of,
    };

    let key_expr = format!("harmony/vines/{}", node_addr);
    let payload = serde_json::to_vec(&wire).map_err(|e| format!("serialize: {e}"))?;

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    publish_tx
        .send(event_loop::PublishRequest {
            key_expr,
            payload,
            reply: reply_tx,
        })
        .await
        .map_err(|_| "event loop not running".to_string())?;

    reply_rx
        .await
        .map_err(|_| "event loop dropped publish request".to_string())?
}

/// Publish a vine reaction (like/unlike) to the mesh network via Zenoh pub/sub.
///
/// Publishes JSON to `harmony/vines/{vine_creator_address}/reactions/{vine_id}/{own_addr}`.
#[tauri::command]
async fn publish_vine_reaction(
    reaction: PublishReactionPayload,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    if reaction.vine_id.trim().is_empty() {
        return Err("vine_id is required".to_string());
    }
    if reaction.vine_creator_address.trim().is_empty() {
        return Err("vine_creator_address is required".to_string());
    }

    let (publish_tx, node_addr) = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        let tx = guard
            .publish_tx
            .clone()
            .ok_or_else(|| "not connected".to_string())?;
        (tx, guard.node_addr.clone())
    };

    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let wire = VineReactionPayload {
        vine_id: reaction.vine_id.clone(),
        reactor_address: node_addr.clone(),
        reactor_name: reaction.reactor_name,
        liked: reaction.liked,
        timestamp: now_secs,
    };

    let key_expr = format!(
        "harmony/vines/{}/reactions/{}/{}",
        reaction.vine_creator_address, reaction.vine_id, node_addr
    );
    let payload = serde_json::to_vec(&wire).map_err(|e| format!("serialize: {e}"))?;

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    publish_tx
        .send(event_loop::PublishRequest {
            key_expr,
            payload,
            reply: reply_tx,
        })
        .await
        .map_err(|_| "event loop not running".to_string())?;

    reply_rx
        .await
        .map_err(|_| "event loop dropped publish request".to_string())?
}

#[tauri::command]
fn list_vine_videos() -> Vec<VineVideoDto> {
    // Future: return cached/persisted vines. Real data flows via vine-received events.
    Vec::new()
}

#[tauri::command]
async fn follow_vine_creator(
    address: String,
    name: Option<String>,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<bool, String> {
    let mut guard = state.lock().map_err(|e| format!("lock: {e}"))?;

    if address == guard.node_addr {
        return Err("cannot follow yourself".to_string());
    }

    let mgr = guard.follow_mgr.as_mut().ok_or("not connected")?;
    if !mgr.follow(address.clone(), name) {
        return Ok(false);
    }

    if let Some(ref set) = guard.followed_set {
        let mut s = set.lock().unwrap();
        s.insert(address.clone());
    }

    if let Some(ref tx) = guard.follow_tx {
        if tx
            .try_send(event_loop::FollowRequest::Follow { address })
            .is_err()
        {
            tracing::error!("follow_tx full — follow update not sent to event loop");
        }
    }

    Ok(true)
}

#[tauri::command]
async fn unfollow_vine_creator(
    address: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<bool, String> {
    let mut guard = state.lock().map_err(|e| format!("lock: {e}"))?;

    let mgr = guard.follow_mgr.as_mut().ok_or("not connected")?;
    if !mgr.unfollow(&address) {
        return Ok(false);
    }

    if let Some(ref set) = guard.followed_set {
        let mut s = set.lock().unwrap();
        s.remove(&address);
    }

    if let Some(ref tx) = guard.follow_tx {
        if tx
            .try_send(event_loop::FollowRequest::Unfollow { address })
            .is_err()
        {
            tracing::error!("follow_tx full — unfollow update not sent to event loop");
        }
    }

    Ok(true)
}

#[tauri::command]
fn list_followed(
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<Vec<FollowEntryResponse>, String> {
    let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
    let mgr = guard.follow_mgr.as_ref().ok_or("not connected")?;
    Ok(mgr
        .list()
        .into_iter()
        .map(|e| FollowEntryResponse {
            address: e.address,
            name: e.name,
            followed_at: e.followed_at,
        })
        .collect())
}

#[tauri::command]
fn mark_vine_viewed(vine_id: String) -> bool {
    // Future: persist viewed state + publish to network for cross-device sync.
    let _ = vine_id;
    true
}

// ── Content announcement types and file manager stubs ───────────────────

/// Content availability announcement received from the mesh network.
///
/// When a node stores content, it publishes to `harmony/announce/{cid_hex}`
/// with the payload size. The event loop routes these to the frontend as
/// `content-announced` IPC events.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentAnnouncementPayload {
    /// Hex-encoded CID from the key expression.
    pub cid: String,
    /// Payload size in bytes (from the 4-byte big-endian announcement body).
    pub size_bytes: u32,
}

/// Parse a content announcement from key expression + payload.
///
/// Key format: `harmony/announce/{cid_hex}`
/// Payload: 4 bytes big-endian u32 size.
pub fn parse_content_announcement(
    key_expr: &str,
    payload: &[u8],
) -> Option<ContentAnnouncementPayload> {
    let cid_hex = key_expr.strip_prefix("harmony/announce/")?;
    if cid_hex.is_empty() {
        return None;
    }
    if !cid_hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    if payload.len() < 4 {
        return None;
    }
    let size_bytes = u32::from_be_bytes(payload[..4].try_into().ok()?);
    Some(ContentAnnouncementPayload {
        cid: cid_hex.to_string(),
        size_bytes,
    })
}

/// Wire format returned by `list_content` — one entry per self-ingested
/// file the client is aware of. Joins sidecar metadata with the runtime
/// cache's pinned state snapshot. ZEB-158 slice 1 adds `kind` to
/// distinguish leaf files from folder bundles. ZEB-164 adds `sidecarId`
/// (the wire-stable handle for pin/burn/archive operations); empty for
/// manifest-derived rows where no sidecar entry exists.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentItemWire {
    /// ZEB-164: opaque per-entry stable handle. Empty string for
    /// manifest-derived rows (children inside a folder bundle that have
    /// no sidecar entry of their own). Frontend gates pin/burn/archive
    /// buttons on this being non-empty.
    pub sidecar_id: String,
    pub cid: String, // hex
    pub name: String,
    pub size_bytes: u64,
    pub stored_at: u64,           // ms since epoch
    pub sensitivity: String,      // "private" | "confidential" | "public"
    pub replication_tier: String, // "expendable" | "light" | "default" | "high" | "ultra"
    pub pinned: bool,
    pub licensed: bool,
    pub archived: bool,
    pub kind: String, // ZEB-158: "leaf" | "folder"
}

fn sensitivity_wire(s: content_index::Sensitivity) -> &'static str {
    match s {
        content_index::Sensitivity::Private => "private",
        content_index::Sensitivity::Confidential => "confidential",
        content_index::Sensitivity::Public => "public",
    }
}

fn replication_tier_wire(t: content_index::ReplicationTier) -> &'static str {
    match t {
        content_index::ReplicationTier::Expendable => "expendable",
        content_index::ReplicationTier::Light => "light",
        content_index::ReplicationTier::Default => "default",
        content_index::ReplicationTier::High => "high",
        content_index::ReplicationTier::Ultra => "ultra",
    }
}

fn kind_wire(k: content_index::ContentKind) -> &'static str {
    match k {
        content_index::ContentKind::Leaf => "leaf",
        content_index::ContentKind::Folder => "folder",
    }
}

fn parse_cid_hex(cid_hex: &str) -> Result<[u8; 32], String> {
    let bytes = hex::decode(cid_hex).map_err(|_| "invalid cid hex".to_string())?;
    <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| "cid must be 32 bytes".to_string())
}

fn parse_sidecar_id(s: &str) -> Result<content_index::SidecarId, String> {
    if s.is_empty() {
        return Err("sidecar_id is empty (manifest-derived row?)".into());
    }
    content_index::SidecarId::parse_str(s).map_err(|e| format!("invalid sidecar_id: {e}"))
}

/// Result returned to the frontend after a successful file ingest.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IngestResult {
    pub sidecar_id: String,
    pub cid: String,
    pub file_name: String,
    pub size_bytes: u64,
}

/// Result returned by `create_folder` and `create_folder_at_root`. The
/// frontend stashes `sidecar_id` immediately so subsequent operations on
/// the just-created folder (pin, archive, future move/rename) have the
/// stable handle. `cid` is provided alongside for content-addressed reads.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateFolderResult {
    pub sidecar_id: String,
    pub cid: String,
}

#[tauri::command]
async fn list_content(
    folder_cid: Option<String>,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<Vec<ContentItemWire>, String> {
    // Root listings read entry.pinned directly from the sidecar (the runtime
    // pin_intent OR-join keeps that flag authoritative), so they don't need
    // the runtime's pinned-CID snapshot. Only fetch it for folder listings,
    // where manifest-derived rows have no sidecar entry to consult.
    match folder_cid {
        None => list_root(state),
        Some(hex) => {
            let verb_tx = {
                let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
                guard
                    .content_verb_tx
                    .clone()
                    .ok_or_else(|| "runtime unavailable".to_string())?
            };
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            verb_tx
                .send(event_loop::ContentVerbRequest::PinnedSet { reply: reply_tx })
                .await
                .map_err(|_| "event loop not running".to_string())?;
            let pinned_set = reply_rx
                .await
                .map_err(|_| "event loop dropped snapshot request".to_string())?;
            list_folder(hex, verb_tx, &pinned_set).await
        }
    }
}

pub(crate) fn list_root(
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<Vec<ContentItemWire>, String> {
    let index = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard.content_index.clone()
    };
    let mut entries: Vec<ContentItemWire> = {
        let idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        idx.entries()
            .map(|e| ContentItemWire {
                sidecar_id: e.sidecar_id.to_string(),
                cid: hex::encode(e.cid),
                name: e.file_name.clone(),
                size_bytes: e.size_bytes,
                stored_at: e.stored_at_ms,
                sensitivity: sensitivity_wire(e.sensitivity).to_string(),
                replication_tier: replication_tier_wire(e.replication_tier).to_string(),
                pinned: e.pinned,
                licensed: e.licensed,
                archived: e.archived,
                kind: kind_wire(e.kind).to_string(),
            })
            .collect()
    };
    // HashMap iter is non-deterministic; sort newest-first for stable UI.
    entries.sort_by(|a, b| b.stored_at.cmp(&a.stored_at));
    Ok(entries)
}

pub async fn list_folder(
    folder_cid_hex: String,
    verb_tx: tokio::sync::mpsc::Sender<event_loop::ContentVerbRequest>,
    pinned_set: &std::collections::HashSet<[u8; 32]>,
) -> Result<Vec<ContentItemWire>, String> {
    use harmony_content::bundle::parse_bundle;

    let folder_cid = parse_cid_hex(&folder_cid_hex)?;

    // Fetch the folder's bundle bytes from the runtime cache.
    let bundle_bytes = match read_cached_bytes(&verb_tx, folder_cid).await? {
        Some(b) => b,
        None => {
            // Folder not in cache — likely evicted or never admitted.
            // Return empty (UI shows empty folder); ZEB-159 will add
            // transparent re-fetch in a follow-up.
            tracing::debug!(
                folder_cid = %folder_cid_hex,
                "list_folder: bundle not in cache; returning empty",
            );
            return Ok(vec![]);
        }
    };

    // Parse bundle child CIDs; child-0 is the manifest book.
    let child_cids: Vec<[u8; 32]> = parse_bundle(&bundle_bytes)
        .map_err(|e| format!("malformed folder bundle: {e:?}"))?
        .iter()
        .map(|c| c.to_bytes())
        .collect();
    let manifest_cid: [u8; 32] = child_cids
        .first()
        .copied()
        .ok_or_else(|| "folder bundle has no children".to_string())?;

    // Read the manifest book bytes.
    let manifest_bytes = read_cached_bytes(&verb_tx, manifest_cid)
        .await?
        .ok_or_else(|| "manifest book not in cache".to_string())?;

    let manifest = crate::folders::parse_manifest(&manifest_bytes)?;
    crate::folders::validate_manifest_matches_bundle(&manifest, &child_cids)?;

    // Synthesize wire rows. Nested items have no sidecar: sidecar_id is
    // the empty-string sentinel ("frontend: no mutations apply"); size_bytes
    // /stored_at default to 0; sensitivity/replication_tier default;
    // licensed/archived false. For manifest-derived rows we DO consult the
    // runtime pinned set — those rows have no sidecar.pinned to read, and
    // a CID currently held in cache via some other entry's pin_intent is
    // the only signal of "this content is sticking around right now".
    Ok(manifest
        .folder_manifest
        .entries
        .into_iter()
        .map(|e| ContentItemWire {
            sidecar_id: String::new(),
            cid: hex::encode(e.cid),
            name: e.name,
            size_bytes: 0,
            stored_at: 0,
            sensitivity: "private".into(),
            replication_tier: "default".into(),
            pinned: pinned_set.contains(&e.cid),
            licensed: false,
            archived: false,
            kind: kind_wire(e.kind).to_string(),
        })
        .collect())
}

#[tauri::command]
async fn pin_content(
    sidecar_id: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<bool, String> {
    let id = parse_sidecar_id(&sidecar_id)?;

    // ZEB-155 + ZEB-164: persist pin intent on the sidecar BEFORE the
    // runtime verb. After flipping the bit, look up the entry's CID so
    // we can dispatch Pin against it. The Pin verb is idempotent for
    // CIDs already in pin_intent (a sibling entry pinning the same CID
    // will have already added it).
    let (index, maybe_verb_tx) = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        (guard.content_index.clone(), guard.content_verb_tx.clone())
    };
    let cid_bytes = {
        let mut idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        idx.set_pinned(&id, true);
        idx.get(&id)
            .ok_or_else(|| "unknown sidecar_id".to_string())?
            .cid
    };

    // Sidecar already committed. Runtime Pin failures split into two
    // categories:
    //   - Deterministic refusal (Ok(Ok(false)) = pin quota exhausted):
    //     surface to the caller as Ok(false). The sidecar bit is set so
    //     intent is recorded, but the runtime answered "no, can't fit"
    //     and the user needs to know (free space, retry). The
    //     start_node sweep will retry on next start; if quota is still
    //     exhausted there too, it also gets false.
    //   - Transient runtime gaps (event loop down, dropped reply,
    //     verb_tx None, runtime returned Err): best-effort, log, return
    //     Ok(true). Intent is recorded; the start_node pin-restore
    //     sweep walks the sidecar and re-pins every entry with
    //     pinned=true, so the gap is bounded and self-healing.
    // This preserves the runtime's quota-exhausted signal that the
    // pre-best-effort code returned via the bool, while keeping the
    // best-effort behavior for transient failures that pin/unpin/burn
    // all share.
    let pinned = if let Some(verb_tx) = maybe_verb_tx {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        match verb_tx
            .send(event_loop::ContentVerbRequest::Pin {
                cid: cid_bytes,
                reply: reply_tx,
            })
            .await
        {
            Ok(()) => match reply_rx.await {
                Ok(Ok(true)) => true,
                Ok(Ok(false)) => {
                    tracing::warn!(
                        cid = %hex::encode(cid_bytes),
                        "pin_content: runtime pin quota exhausted",
                    );
                    false
                }
                Ok(Err(e)) => {
                    tracing::warn!(
                        cid = %hex::encode(cid_bytes),
                        err = %e,
                        "pin_content: runtime pin failed; pin_intent will repopulate on next start_node sweep",
                    );
                    true
                }
                Err(_) => {
                    tracing::warn!(
                        cid = %hex::encode(cid_bytes),
                        "pin_content: event loop dropped pin reply",
                    );
                    true
                }
            },
            Err(_) => {
                tracing::warn!(
                    cid = %hex::encode(cid_bytes),
                    "pin_content: event loop closed before pin send; pin_intent will repopulate on next start_node sweep",
                );
                true
            }
        }
    } else {
        tracing::warn!(
            cid = %hex::encode(cid_bytes),
            "pin_content: runtime unavailable; pin_intent will repopulate on next start_node sweep",
        );
        true
    };
    Ok(pinned)
}

#[tauri::command]
async fn unpin_content(
    sidecar_id: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<bool, String> {
    let id = parse_sidecar_id(&sidecar_id)?;

    // ZEB-164: clear sidecar intent. Then check OR-join: if some other
    // sidecar entry STILL pins this CID, leave runtime pin_intent alone
    // (the bytes are still wanted). Only dispatch Unpin to the runtime
    // when no entry references this CID with pinned=true anymore.
    let (index, maybe_verb_tx) = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        (guard.content_index.clone(), guard.content_verb_tx.clone())
    };
    let unpin_runtime_for: Option<[u8; 32]> = {
        let mut idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        idx.set_pinned(&id, false);
        let cid = idx
            .get(&id)
            .ok_or_else(|| "unknown sidecar_id".to_string())?
            .cid;
        if idx.is_cid_pinned_by_any(&cid) {
            None
        } else {
            Some(cid)
        }
    };

    let Some(cid_bytes) = unpin_runtime_for else {
        return Ok(true); // sidecar updated; another entry still pins
    };

    // Sidecar already committed. Runtime Unpin is best-effort: if the
    // event loop is gone, we have a stale pin_intent that self-corrects
    // on the next start_node pin-restore sweep. Log, don't propagate —
    // matches burn_content's RuntimeAction::Unpin branch and the
    // create_folder_nested post-rekey pattern.
    if let Some(verb_tx) = maybe_verb_tx {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        match verb_tx
            .send(event_loop::ContentVerbRequest::Unpin {
                cid: cid_bytes,
                reply: reply_tx,
            })
            .await
        {
            Ok(()) => match reply_rx.await {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => tracing::warn!(
                    cid = %hex::encode(cid_bytes),
                    err = %e,
                    "unpin_content: runtime unpin failed; pin_intent may be stale",
                ),
                Err(_) => tracing::warn!(
                    cid = %hex::encode(cid_bytes),
                    "unpin_content: event loop dropped unpin reply",
                ),
            },
            Err(_) => tracing::warn!(
                cid = %hex::encode(cid_bytes),
                "unpin_content: event loop closed before unpin send; pin_intent may be stale",
            ),
        }
    }
    Ok(true)
}

/// Burn a sidecar entry. With ZEB-164's symlink-style sidecar, burn is
/// "remove this entry from my list" — not "destroy the bytes everyone
/// shares." The runtime's `Burn` verb only fires when this entry was the
/// last reference to its CID. Otherwise we issue an `Unpin` (if the burn
/// drops the only pinning entry) or no runtime action (if siblings still
/// pin it).
#[tauri::command]
async fn burn_content(
    sidecar_id: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<bool, String> {
    let id = parse_sidecar_id(&sidecar_id)?;

    // Match pin_content/unpin_content's best-effort pattern: clone
    // maybe_verb_tx without erroring on None. The pre-existing upfront
    // ok_or_else was a weak guard anyway — runtime can die between the
    // check and the verb_tx.send().await — and asymmetry meant users
    // could pin/unpin offline but not burn. With sidecar-as-source-of-
    // truth, the entry-removal step succeeds even with the runtime down;
    // a future reconciliation pass cleans up surviving bytes.
    let (index, maybe_verb_tx) = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        (guard.content_index.clone(), guard.content_verb_tx.clone())
    };

    // Three-branch decision under a single lock acquisition: read entry's
    // CID, remove the entry, then inspect the post-state to decide which
    // (if any) runtime verb to dispatch.
    enum RuntimeAction {
        Burn([u8; 32]),
        Unpin([u8; 32]),
        Nothing,
    }
    let action = {
        let mut idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        // Snapshot the burned entry's pinned bit before removing — we
        // need it to decide whether the runtime pin_intent had this CID
        // at all. If the burned entry wasn't pinning, then runtime
        // pin_intent state is independent of any sibling (the OR-join
        // is unchanged by removing a contributor that wasn't asserting).
        let (cid, was_pinned) = match idx.get(&id) {
            Some(e) => (e.cid, e.pinned),
            None => return Ok(false), // unknown sidecar_id; no-op
        };
        idx.remove(&id);
        if idx.entries_for_cid(&cid).next().is_none() {
            RuntimeAction::Burn(cid)
        } else if was_pinned && !idx.is_cid_pinned_by_any(&cid) {
            // The burned entry was the last pinning reference; drop
            // runtime pin_intent. Without the was_pinned guard, an
            // unpinned-entry burn whose siblings are also unpinned
            // would dispatch a spurious Unpin (no-op at the cache
            // layer, but generates misleading "post-burn unpin failed"
            // warnings if the runtime path errors).
            RuntimeAction::Unpin(cid)
        } else {
            RuntimeAction::Nothing
        }
    };

    match action {
        RuntimeAction::Burn(cid) => {
            // Sidecar mutation already committed — runtime Burn is best-
            // effort. If the event loop is gone, bytes may survive until
            // W-TinyLFU evicts them or a future reconciliation pass runs.
            // Log so the desync is diagnosable. Matches the unpin /
            // post-burn-Unpin pattern.
            if let Some(verb_tx) = maybe_verb_tx {
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                match verb_tx
                    .send(event_loop::ContentVerbRequest::Burn {
                        cid,
                        reply: reply_tx,
                    })
                    .await
                {
                    Ok(()) => match reply_rx.await {
                        Ok(Ok(_)) => {}
                        Ok(Err(e)) => tracing::warn!(
                            cid = %hex::encode(cid),
                            err = %e,
                            "burn_content: runtime burn failed; bytes may survive until reconciliation",
                        ),
                        Err(_) => tracing::warn!(
                            cid = %hex::encode(cid),
                            "burn_content: event loop dropped burn reply",
                        ),
                    },
                    Err(_) => tracing::warn!(
                        cid = %hex::encode(cid),
                        "burn_content: event loop closed before burn send; bytes may survive",
                    ),
                }
            } else {
                tracing::warn!(
                    cid = %hex::encode(cid),
                    "burn_content: runtime unavailable; bytes may survive until reconciliation",
                );
            }
        }
        RuntimeAction::Unpin(cid) => {
            // Sibling entries still reference this CID, but none pin it —
            // drop runtime pin_intent so W-TinyLFU can reclaim. Best-
            // effort: any failure here is a runtime/cache desync, not a
            // user-visible regression (the sidecar mutation already
            // committed). Log so the desync is diagnosable.
            if let Some(verb_tx) = maybe_verb_tx {
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                match verb_tx
                    .send(event_loop::ContentVerbRequest::Unpin {
                        cid,
                        reply: reply_tx,
                    })
                    .await
                {
                    Ok(()) => match reply_rx.await {
                        Ok(Ok(_)) => {}
                        Ok(Err(e)) => tracing::warn!(
                            cid = %hex::encode(cid),
                            err = %e,
                            "burn_content: post-burn unpin failed; runtime may hold stale pin",
                        ),
                        Err(_) => tracing::warn!(
                            cid = %hex::encode(cid),
                            "burn_content: event loop dropped post-burn unpin reply",
                        ),
                    },
                    Err(_) => tracing::warn!(
                        cid = %hex::encode(cid),
                        "burn_content: event loop closed before post-burn unpin send",
                    ),
                }
            } else {
                tracing::warn!(
                    cid = %hex::encode(cid),
                    "burn_content: runtime unavailable for post-burn unpin; runtime may hold stale pin",
                );
            }
        }
        RuntimeAction::Nothing => {} // siblings still pin; runtime untouched
    }
    Ok(true)
}

#[tauri::command]
async fn archive_content(
    sidecar_id: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<bool, String> {
    let id = parse_sidecar_id(&sidecar_id)?;
    let index = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard.content_index.clone()
    };
    let flipped = {
        let mut idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        idx.set_archived(&id, true)
    };
    Ok(flipped)
}

#[tauri::command]
async fn set_replication_tier(
    sidecar_ids: Vec<String>,
    tier: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<u32, String> {
    let parsed_tier = match tier.as_str() {
        "expendable" => content_index::ReplicationTier::Expendable,
        "light" => content_index::ReplicationTier::Light,
        "default" => content_index::ReplicationTier::Default,
        "high" => content_index::ReplicationTier::High,
        "ultra" => content_index::ReplicationTier::Ultra,
        other => return Err(format!("unknown replication tier: {other}")),
    };
    let mut parsed_ids: Vec<content_index::SidecarId> = Vec::with_capacity(sidecar_ids.len());
    for s in &sidecar_ids {
        parsed_ids.push(parse_sidecar_id(s)?);
    }
    let index = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard.content_index.clone()
    };
    let updated = {
        let mut idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        idx.set_replication_tier(&parsed_ids, parsed_tier)
    };
    Ok(updated as u32)
}

/// Export content to the local filesystem via a save dialog.
///
/// Fetches the raw bytes for `cid` through the Zenoh content transport,
/// opens a native save-file dialog with `file_name` as the suggested name,
/// and writes the bytes to the chosen path.
#[tauri::command]
async fn export_content(
    app: tauri::AppHandle,
    cid: String,
    file_name: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<bool, String> {
    use tauri_plugin_dialog::DialogExt;

    // Validate hex CID
    if cid.is_empty() || !cid.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("invalid CID hex: {cid}"));
    }

    // 1. Fetch content bytes via the existing fetch channel.
    let fetch_tx = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard
            .fetch_tx
            .clone()
            .ok_or_else(|| "not connected".to_string())?
    };

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    fetch_tx
        .send(event_loop::FetchRequest {
            cid_hex: cid,
            reply: reply_tx,
        })
        .await
        .map_err(|_| "event loop not running".to_string())?;

    let bytes = reply_rx
        .await
        .map_err(|_| "event loop dropped fetch request".to_string())??;

    // 2. Open a native save-file dialog.
    let (path_tx, path_rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_file_name(&file_name)
        .save_file(move |path| {
            let _ = path_tx.send(path);
        });

    let file_path = path_rx
        .await
        .map_err(|_| "dialog error".to_string())?
        .ok_or_else(|| "export cancelled".to_string())?;

    // 3. Write bytes to disk.
    let path = file_path
        .as_path()
        .ok_or_else(|| "unsupported file path".to_string())?;
    tokio::fs::write(path, &bytes)
        .await
        .map_err(|e| format!("write failed: {e}"))?;

    Ok(true)
}

/// Ingest a local file into the content store via a native open-file dialog.
///
/// Opens a file picker, reads the selected file, computes a CID, and stores
/// the content in the runtime's storage tier (which handles announcement to
/// the mesh). Returns metadata so the frontend can add it to the file list.
#[tauri::command]
async fn ingest_content(
    app: tauri::AppHandle,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<IngestResult, String> {
    use harmony_content::cid::{ContentFlags, ContentId};
    use tauri_plugin_dialog::DialogExt;

    // 1. Open a native file picker dialog.
    let (path_tx, path_rx) = tokio::sync::oneshot::channel();
    app.dialog().file().pick_file(move |path| {
        let _ = path_tx.send(path);
    });
    let file_path = path_rx
        .await
        .map_err(|_| "dialog error".to_string())?
        .ok_or_else(|| "upload cancelled".to_string())?;

    // 2. Read file bytes (with size guard to avoid OOM on large files).
    let path = file_path
        .as_path()
        .ok_or_else(|| "unsupported file path".to_string())?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();
    let meta = tokio::fs::metadata(path)
        .await
        .map_err(|e| format!("read failed: {e}"))?;
    // Early reject above the flat-bundle cap, before reading the file into
    // memory. Dispatch is recomputed from actual bytes below in case the
    // file changes size between this stat and the read that follows.
    ingest_dispatch(meta.len())?;

    // OOM caveat: this materializes the full file in RAM before chunking.
    // Acceptable for v1 (FLAT_BUNDLE_MAX is ~8 GiB and realistic uploads
    // are far smaller) but a near-cap file would consume ~8 GiB of heap.
    // Streaming ingest pairs with the disk-backed storage tier — see the
    // spec's out-of-scope section. If you raise FLAT_BUNDLE_MAX without
    // landing streaming first, you are asking for OOMs.
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|e| format!("read failed: {e}"))?;
    let size_bytes = bytes.len() as u64;
    // Final dispatch decision from the bytes actually read. This closes the
    // TOCTOU window between metadata() and read(): if the file grew past the
    // cap we reject cleanly, and if it shrank below MAX_PAYLOAD_SIZE we take
    // the single-book fast path instead of tripping chunk_and_bundle's
    // precondition guard.
    let dispatch = ingest_dispatch(size_bytes)?;

    let ingest_tx = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard
            .ingest_tx
            .clone()
            .ok_or_else(|| "not connected".to_string())?
    };

    let root_cid_bytes: [u8; 32] = match dispatch {
        IngestDispatch::Single => {
            let cid = ContentId::for_book(&bytes, ContentFlags::default())
                .map_err(|e| format!("CID error: {e:?}"))?;
            let cid_hex = hex::encode(cid.to_bytes());
            send_ingest(&ingest_tx, cid_hex, bytes).await?;
            cid.to_bytes()
        }
        IngestDispatch::Chunked => {
            let (leaves, bundle_payload, root) = chunk_and_bundle(&bytes)?;
            // Ingest every leaf in order.
            for (leaf_cid, leaf_bytes) in &leaves {
                send_ingest(
                    &ingest_tx,
                    hex::encode(leaf_cid.to_bytes()),
                    leaf_bytes.to_vec(),
                )
                .await?;
            }
            // Ingest the bundle itself.
            send_ingest(&ingest_tx, hex::encode(root.to_bytes()), bundle_payload).await?;
            root.to_bytes()
        }
    };

    // Record sidecar metadata so list_content can surface this entry.
    let index = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard.content_index.clone()
    };
    let stored_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let sidecar_id = content_index::SidecarId::new();
    {
        let mut idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        let inserted = idx.insert(content_index::ContentIndexEntry {
            sidecar_id,
            cid: root_cid_bytes,
            file_name: file_name.clone(),
            size_bytes,
            stored_at_ms,
            sensitivity: content_index::Sensitivity::Private,
            replication_tier: content_index::ReplicationTier::Default,
            licensed: false,
            archived: false,
            pinned: false,
            kind: content_index::ContentKind::Leaf,
        });
        if !inserted {
            // Effectively impossible (UUID v4 collision); kept as a
            // sanity guard against future SidecarId construction bugs.
            // Pre-ZEB-164 this branch silently deduped duplicate-CID
            // re-uploads; under the symlink model, two ingests of the
            // same content produce two distinct sidecar entries, so
            // !inserted here means the freshly-minted sidecar_id was
            // already in use. Fail loudly so the caller doesn't get a
            // phantom IngestResult whose sidecar_id list_content/pin/
            // burn/archive will all reject as unknown — mirrors
            // create_folder_at_root's symmetric guard.
            tracing::error!(
                sidecar_id = %sidecar_id,
                file_name = %file_name,
                "ingest_content: sidecar_id collision (UUID v4 collision or construction bug); aborting ingest result",
            );
            return Err("sidecar_id collision".into());
        }
    }

    Ok(IngestResult {
        sidecar_id: sidecar_id.to_string(),
        cid: hex::encode(root_cid_bytes),
        file_name,
        size_bytes,
    })
}

/// Send one (cid_hex, data) pair through the ingest channel and await its ack.
///
/// Shared by `ingest_content` and `create_folder` so both commands go
/// through a single implementation (DRY; no behavior change).
pub async fn send_ingest(
    tx: &tokio::sync::mpsc::Sender<event_loop::IngestRequest>,
    cid_hex: String,
    data: Vec<u8>,
) -> Result<(), String> {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    tx.send(event_loop::IngestRequest {
        cid_hex,
        data,
        reply: reply_tx,
    })
    .await
    .map_err(|_| "event loop not running".to_string())?;
    reply_rx
        .await
        .map_err(|_| "event loop dropped ingest request".to_string())??;
    Ok(())
}

/// ZEB-164: create a new folder at the root or inside an existing folder.
/// Empty `parent_path` means root; non-empty means a walk from top-level
/// root (index 0) down to immediate parent (last element).
#[tauri::command]
async fn create_folder(
    name: String,
    parent_sidecar_id: Option<String>,
    parent_path: Vec<String>,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<CreateFolderResult, String> {
    // Defence-in-depth: the UI already trims and rejects blank names, but
    // the IPC surface is callable by anything with a Tauri handle. An empty
    // or whitespace-only label would produce folders that are hard to
    // distinguish in listings and breadcrumbs, so reject at the boundary.
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("folder name cannot be empty".to_string());
    }
    if parent_path.is_empty() {
        if parent_sidecar_id.is_some() {
            return Err("root creates must not provide parent_sidecar_id".into());
        }
        return create_folder_at_root(name, state).await;
    }
    let psid =
        parent_sidecar_id.ok_or_else(|| "nested creates require parent_sidecar_id".to_string())?;
    create_folder_nested(name, psid, parent_path, state).await
}

async fn create_folder_at_root(
    name: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<CreateFolderResult, String> {
    // Build the (empty) manifest + bundle locally. No runtime state
    // mutated yet — we can still bail cleanly on send_ingest failure.
    let built = folders::build_folder(&name, &[])?;

    let (ingest_tx, index) = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        (
            guard
                .ingest_tx
                .clone()
                .ok_or_else(|| "not connected".to_string())?,
            guard.content_index.clone(),
        )
    };
    let bundle_size = built.bundle_bytes.len() as u64;
    let stored_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    // ZEB-164: every empty folder bundle has the same CID, but multiple
    // sidecar entries can now reference that shared CID — so the slice-1
    // collision workaround ("a folder with identical contents already
    // exists") is gone. We mint a fresh sidecar_id, reserve the slot
    // before publishing bytes, and roll back if either ingest fails.
    let sidecar_id = content_index::SidecarId::new();
    {
        let mut idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        let inserted = idx.insert(content_index::ContentIndexEntry {
            sidecar_id,
            cid: built.bundle_cid.to_bytes(),
            file_name: name,
            size_bytes: bundle_size,
            stored_at_ms,
            sensitivity: content_index::Sensitivity::Private,
            replication_tier: content_index::ReplicationTier::Default,
            licensed: false,
            archived: false,
            pinned: false,
            kind: content_index::ContentKind::Folder,
        });
        if !inserted {
            // Effectively impossible (UUID v4 collision); kept as a
            // sanity guard against future SidecarId construction bugs.
            return Err("sidecar_id collision".into());
        }
    }

    // Slot reserved — now publish the bytes. ZEB-155's fetch-completion
    // recovery hook is gated on ZEB-159, so an orphan sidecar entry
    // would be unrecoverable until the user manually burned it. Roll
    // back the reservation on any ingest failure so the sidecar never
    // points at bytes that don't exist.
    if let Err(e) = send_ingest(
        &ingest_tx,
        hex::encode(built.manifest_cid.to_bytes()),
        built.manifest_bytes,
    )
    .await
    {
        if let Ok(mut idx) = index.lock() {
            idx.remove(&sidecar_id);
        }
        return Err(e);
    }
    if let Err(e) = send_ingest(
        &ingest_tx,
        hex::encode(built.bundle_cid.to_bytes()),
        built.bundle_bytes,
    )
    .await
    {
        if let Ok(mut idx) = index.lock() {
            idx.remove(&sidecar_id);
        }
        return Err(e);
    }

    Ok(CreateFolderResult {
        sidecar_id: sidecar_id.to_string(),
        cid: hex::encode(built.bundle_cid.to_bytes()),
    })
}

async fn create_folder_nested(
    name: String,
    parent_sidecar_id: String,
    parent_path: Vec<String>,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<CreateFolderResult, String> {
    use harmony_content::bundle::parse_bundle;

    let parent_id = parse_sidecar_id(&parent_sidecar_id)?;

    // Parse all path CIDs up-front; fail fast on malformed input.
    let path_cids: Vec<[u8; 32]> = parent_path
        .iter()
        .map(|h| parse_cid_hex(h))
        .collect::<Result<_, _>>()?;
    let root_old = *path_cids.first().expect("non-empty by guard above");
    let immediate_parent_cid = *path_cids.last().expect("non-empty");

    let (ingest_tx, verb_tx, index) = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        (
            guard
                .ingest_tx
                .clone()
                .ok_or_else(|| "not connected".to_string())?,
            guard
                .content_verb_tx
                .clone()
                .ok_or_else(|| "not connected".to_string())?,
            guard.content_index.clone(),
        )
    };

    // Verify the caller's claim: parent_sidecar_id maps to root_old.
    {
        let idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        let entry = idx
            .get(&parent_id)
            .ok_or_else(|| "parent_sidecar_id not in sidecar".to_string())?;
        if entry.cid != root_old {
            return Err(format!(
                "parent_sidecar_id refers to cid {} but parent_path[0] is {}",
                hex::encode(entry.cid),
                hex::encode(root_old),
            ));
        }
    }

    // The verification above and the rekey below are non-atomic — we
    // yield across multiple await points (ancestor reads, then
    // pending_ingests drain). A concurrent create_folder_nested on the
    // same parent_sidecar_id could land its rekey between our verify
    // and our rekey, so we pass root_old as the expected_old_cid to
    // ContentIndex::rekey: if the entry's current CID has shifted,
    // rekey returns RekeyError::Conflict instead of silently
    // overwriting the concurrent winner. The UI serializes per-folder
    // mutations so this is rarely hit in practice, but the
    // ingest-before-rekey reorder widened the verify→rekey window
    // from "ancestor reads" to "drain pending_ingests" — wide enough
    // that the CAS guard is now load-bearing rather than defensive.

    // 1. Build the new empty sub-folder LOCALLY. Defer all ingests so
    // that a downstream OldMissing during rekey doesn't leave orphan
    // bytes in the runtime cache (which could be announced over Zenoh
    // and waste capacity for content no sidecar entry will ever
    // reference).
    let new_child = folders::build_folder(&name, &[])?;
    let new_child_bundle_cid = new_child.bundle_cid;

    let mut pending_ingests: Vec<(String, Vec<u8>)> = Vec::new();
    pending_ingests.push((
        hex::encode(new_child.manifest_cid.to_bytes()),
        new_child.manifest_bytes,
    ));
    pending_ingests.push((
        hex::encode(new_child_bundle_cid.to_bytes()),
        new_child.bundle_bytes,
    ));

    // 2. Bottom-up walk: rebuild each ancestor LOCALLY (read-only verb
    // requests), accumulating into pending_ingests.
    let mut prev_old_cid = immediate_parent_cid;
    let mut prev_new_cid = new_child_bundle_cid.to_bytes();
    let mut last_bundle_size: u64 = pending_ingests
        .last()
        .map(|(_, b)| b.len() as u64)
        .unwrap_or(0);

    for (i, &anc_cid) in path_cids.iter().enumerate().rev() {
        let is_deepest = i == path_cids.len() - 1;

        let anc_bundle = read_cached_bytes(&verb_tx, anc_cid).await?.ok_or_else(|| {
            format!(
                "ancestor {} not in cache; cannot rebuild parent chain",
                hex::encode(anc_cid)
            )
        })?;
        let anc_child_ids =
            parse_bundle(&anc_bundle).map_err(|e| format!("malformed ancestor bundle: {e:?}"))?;
        let manifest_cid = anc_child_ids
            .first()
            .copied()
            .ok_or_else(|| "ancestor bundle has no children".to_string())?;
        let anc_children: Vec<[u8; 32]> = anc_child_ids.iter().map(|c| c.to_bytes()).collect();

        let manifest_bytes = read_cached_bytes(&verb_tx, manifest_cid.to_bytes())
            .await?
            .ok_or_else(|| "ancestor manifest not in cache".to_string())?;
        let mut manifest =
            folders::parse_manifest(&manifest_bytes).map_err(|e| format!("ancestor {e}"))?;
        folders::validate_manifest_matches_bundle(&manifest, &anc_children)
            .map_err(|e| format!("ancestor {} {e}", hex::encode(anc_cid)))?;

        if is_deepest {
            manifest
                .folder_manifest
                .entries
                .push(folders::ManifestEntry {
                    cid: prev_new_cid,
                    name: name.clone(),
                    kind: content_index::ContentKind::Folder,
                });
        } else {
            let target_idx = manifest
                .folder_manifest
                .entries
                .iter()
                .position(|e| e.cid == prev_old_cid)
                .ok_or_else(|| {
                    format!(
                        "ancestor {} has no entry pointing to child {}",
                        hex::encode(anc_cid),
                        hex::encode(prev_old_cid)
                    )
                })?;
            manifest.folder_manifest.entries[target_idx].cid = prev_new_cid;
        }

        let rebuilt = folders::build_folder("", &manifest.folder_manifest.entries)?;
        let rebuilt_bundle_cid = rebuilt.bundle_cid;
        last_bundle_size = rebuilt.bundle_bytes.len() as u64;
        pending_ingests.push((
            hex::encode(rebuilt.manifest_cid.to_bytes()),
            rebuilt.manifest_bytes,
        ));
        pending_ingests.push((
            hex::encode(rebuilt_bundle_cid.to_bytes()),
            rebuilt.bundle_bytes,
        ));

        prev_old_cid = anc_cid;
        prev_new_cid = rebuilt_bundle_cid.to_bytes();
    }

    // 3. Drain the deferred ingests BEFORE rekeying. Earlier this was
    // ordered rekey-then-ingest to avoid leaving orphan bytes in the
    // runtime cache (and being announced over Zenoh) if rekey hit
    // OldMissing — but that ordering had a strictly worse failure
    // mode: a send_ingest failure after a successful rekey would leave
    // the sidecar pointing at a chain whose bytes are missing,
    // rendering the user's folder unreadable until manual burn.
    //
    // Reversed: an ingest failure now leaves the sidecar pointing at
    // the original root_old (intact). Bytes ingested before the
    // failure become orphans, but W-TinyLFU evicts them under cache
    // pressure since nothing pins them — recoverable, vs. data-loss
    // for the user. ZEB-167 still tracks the rekey-rollback path for
    // the residual rekey-OldMissing case (would leave orphans without
    // user-visible damage).
    let new_bundle_size = last_bundle_size;
    let stored_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    for (cid_hex, bytes) in pending_ingests {
        send_ingest(&ingest_tx, cid_hex, bytes).await?;
    }

    // 4. Rekey the top-level sidecar entry. CAS-style: pass root_old
    // as the expected current CID. With ZEB-164 the CID-collision
    // branch is gone — multiple entries sharing a CID is legal —
    // so OldMissing and Conflict are the only failure modes.
    {
        let mut idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        match idx.rekey(
            &parent_id,
            root_old,
            prev_new_cid,
            new_bundle_size,
            stored_at_ms,
        ) {
            Ok(()) => {}
            Err(content_index::RekeyError::OldMissing) => {
                return Err("parent_sidecar_id removed mid-flight — nothing to rekey".to_string());
            }
            Err(content_index::RekeyError::Conflict { actual }) => {
                // A concurrent rekey on the same parent_sidecar_id
                // landed between our verify and our rekey. The new
                // bundle bytes we just ingested are orphans — W-TinyLFU
                // will evict them under cache pressure. Surface the
                // actual current CID so future retry logic could
                // rebuild from it; for now the user re-issues the
                // create from the refreshed UI state.
                return Err(format!(
                    "concurrent rekey on parent_sidecar_id (now at cid {}); retry from refreshed state",
                    hex::encode(actual)
                ));
            }
        }
    }

    // 5. Maintain the runtime pin_intent OR-join invariant for both
    // old and new CIDs. If no remaining entry pins root_old, drop it
    // from runtime pin_intent. If any entry pins prev_new_cid (this
    // entry might, depending on its persisted intent), add it.
    //
    // Both dispatches are best-effort: the sidecar has already
    // committed the rekey, so any failure here is a runtime/cache
    // desync rather than a user-visible regression. Log so the desync
    // is diagnosable. The fetch-completion hook (ZEB-155 + ZEB-159)
    // re-converges on the next fetch of the new root.
    let (drop_old, add_new) = {
        let idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        (
            !idx.is_cid_pinned_by_any(&root_old),
            idx.is_cid_pinned_by_any(&prev_new_cid),
        )
    };
    if drop_old {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        match verb_tx
            .send(event_loop::ContentVerbRequest::Unpin {
                cid: root_old,
                reply: reply_tx,
            })
            .await
        {
            Ok(()) => match reply_rx.await {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => tracing::warn!(
                    old_cid = %hex::encode(root_old),
                    err = %e,
                    "create_folder_nested: runtime unpin of old root failed; cache may hold stale pin",
                ),
                Err(_) => tracing::warn!(
                    old_cid = %hex::encode(root_old),
                    "create_folder_nested: event loop dropped unpin reply",
                ),
            },
            Err(_) => tracing::warn!(
                old_cid = %hex::encode(root_old),
                "create_folder_nested: event loop closed before unpin send; cache may hold stale pin",
            ),
        }
    }
    if add_new {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        match verb_tx
            .send(event_loop::ContentVerbRequest::Pin {
                cid: prev_new_cid,
                reply: reply_tx,
            })
            .await
        {
            Ok(()) => match reply_rx.await {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => tracing::warn!(
                    new_cid = %hex::encode(prev_new_cid),
                    err = %e,
                    "create_folder_nested: runtime pin of new root failed; sidecar pin intent will repin on next fetch",
                ),
                Err(_) => tracing::warn!(
                    new_cid = %hex::encode(prev_new_cid),
                    "create_folder_nested: event loop dropped pin reply",
                ),
            },
            Err(_) => tracing::warn!(
                new_cid = %hex::encode(prev_new_cid),
                "create_folder_nested: event loop closed before pin send; sidecar pin intent will repin on next fetch",
            ),
        }
    }

    Ok(CreateFolderResult {
        // Identity unchanged, but emit the canonical lowercase-hyphenated form
        // (via SidecarId::Display) instead of echoing the caller's raw input —
        // every other endpoint that returns a sidecar_id wire field does the same.
        sidecar_id: parent_id.to_string(),
        cid: hex::encode(prev_new_cid),
    })
}

async fn read_cached_bytes(
    verb_tx: &tokio::sync::mpsc::Sender<event_loop::ContentVerbRequest>,
    cid: [u8; 32],
) -> Result<Option<Vec<u8>>, String> {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    verb_tx
        .send(event_loop::ContentVerbRequest::ReadBytes {
            cid,
            reply: reply_tx,
        })
        .await
        .map_err(|_| "event loop not running".to_string())?;
    reply_rx
        .await
        .map_err(|_| "event loop dropped read request".to_string())
}

/// Fetch raw content bytes by hex-encoded CID via Zenoh get().
///
/// Used by the frontend to resolve avatar CIDs (and other content) into
/// displayable blob URLs.
#[tauri::command]
async fn fetch_content(
    cid: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<Vec<u8>, String> {
    // Validate hex CID
    if cid.is_empty() || !cid.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("invalid CID hex: {cid}"));
    }

    let fetch_tx = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard
            .fetch_tx
            .clone()
            .ok_or_else(|| "not connected".to_string())?
    };

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    fetch_tx
        .send(event_loop::FetchRequest {
            cid_hex: cid,
            reply: reply_tx,
        })
        .await
        .map_err(|_| "event loop not running".to_string())?;

    reply_rx
        .await
        .map_err(|_| "event loop dropped fetch request".to_string())?
}

// ── Voice commands ──────────────────────────────────────────────────────

/// Reject channel IDs that could escape the intended Zenoh key namespace.
/// Same forbidden characters as send_message's channel/hub validation.
fn validate_voice_channel_id(channel_id: &str) -> Result<(), String> {
    if channel_id.is_empty()
        || channel_id.contains('/')
        || channel_id.contains('*')
        || channel_id.contains('?')
        || channel_id.contains('#')
        || channel_id.contains('$')
    {
        return Err(format!("invalid voice channel_id: {channel_id}"));
    }
    Ok(())
}

#[tauri::command]
async fn send_voice_frame(
    payload: voice::SendVoiceFramePayload,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    validate_voice_channel_id(&payload.channel_id)?;
    let voice_tx = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard
            .voice_tx
            .clone()
            .ok_or_else(|| "not connected".to_string())?
    };
    voice_tx
        .send(voice::VoiceOutbound {
            channel_id: payload.channel_id,
            frame: payload.frame_bytes,
        })
        .await
        .map_err(|_| "event loop not running".to_string())
}

#[tauri::command]
async fn join_voice_channel(
    channel_id: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    validate_voice_channel_id(&channel_id)?;
    let tx = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard
            .voice_channel_tx
            .clone()
            .ok_or_else(|| "not connected".to_string())?
    };
    tx.send(voice::VoiceChannelRequest::Join { channel_id })
        .await
        .map_err(|_| "event loop not running".to_string())
}

#[tauri::command]
async fn leave_voice_channel(
    channel_id: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    validate_voice_channel_id(&channel_id)?;
    let tx = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard
            .voice_channel_tx
            .clone()
            .ok_or_else(|| "not connected".to_string())?
    };
    tx.send(voice::VoiceChannelRequest::Leave { channel_id })
        .await
        .map_err(|_| "event loop not running".to_string())
}

// ── Mail commands ────────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SendMailPayload {
    to: Vec<String>,
    subject: String,
    body: String,
    reply_to: Option<String>,
}

#[tauri::command]
async fn send_mail(
    payload: SendMailPayload,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    use harmony_mailbox::message::{
        unique_message_id, HarmonyMessage, MailMessageType, MessageFlags, Recipient, RecipientType,
    };

    if payload.to.is_empty() {
        return Err("at least one recipient required".to_string());
    }
    if payload.subject.is_empty() && payload.body.is_empty() {
        return Err("subject or body required".to_string());
    }

    let (publish_tx, node_addr, mail_mgr) = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        let tx = guard
            .publish_tx
            .clone()
            .ok_or_else(|| "not connected".to_string())?;
        let mgr = guard
            .mail_mgr
            .clone()
            .ok_or_else(|| "mail not initialized".to_string())?;
        (tx, guard.node_addr.clone(), mgr)
    };

    // Parse sender address
    let sender_bytes: [u8; 16] = hex::decode(&node_addr)
        .map_err(|e| format!("bad node_addr: {e}"))?
        .try_into()
        .map_err(|_| "node_addr not 16 bytes".to_string())?;

    // Parse in_reply_to
    let in_reply_to = match &payload.reply_to {
        Some(hex_str) if !hex_str.is_empty() => {
            let bytes = hex::decode(hex_str).map_err(|e| format!("bad reply_to: {e}"))?;
            let arr: [u8; 16] = bytes
                .try_into()
                .map_err(|_| "reply_to not 16 bytes".to_string())?;
            Some(arr)
        }
        _ => None,
    };

    // Parse recipients
    let recipients: Vec<Recipient> = payload
        .to
        .iter()
        .map(|addr_hex| {
            let bytes =
                hex::decode(addr_hex).map_err(|e| format!("bad recipient {addr_hex}: {e}"))?;
            let arr: [u8; 16] = bytes
                .try_into()
                .map_err(|_| format!("recipient {addr_hex} not 16 bytes"))?;
            Ok(Recipient {
                address_hash: arr,
                recipient_type: RecipientType::To,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let is_reply = in_reply_to.is_some();
    let msg = HarmonyMessage {
        version: 0x01,
        message_type: MailMessageType::Email,
        flags: MessageFlags::new(false, is_reply, false),
        timestamp: now,
        message_id: unique_message_id(),
        in_reply_to,
        sender_address: sender_bytes,
        recipients,
        subject: payload.subject,
        body: payload.body,
        attachments: vec![],
    };

    let msg_bytes = msg.to_bytes().map_err(|e| format!("serialize: {e}"))?;

    // Publish to each recipient's Zenoh key (canonical lowercase hex)
    for recipient in &msg.recipients {
        let key_expr = format!("harmony/mail/v1/{}", hex::encode(recipient.address_hash));
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        publish_tx
            .send(event_loop::PublishRequest {
                key_expr,
                payload: msg_bytes.clone(),
                reply: reply_tx,
            })
            .await
            .map_err(|_| "event loop not running".to_string())?;
        reply_rx
            .await
            .map_err(|_| "event loop dropped request".to_string())??;
    }

    // Store in sent folder only after all publishes succeed
    {
        let mut mgr = mail_mgr.lock().map_err(|e| format!("mail lock: {e}"))?;
        mgr.store_sent(&msg_bytes, &msg)?;
    }

    Ok(())
}

#[tauri::command]
fn list_mail(
    folder: String,
    page: usize,
    per_page: usize,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<Vec<mail::EntryRecord>, String> {
    let mgr_arc = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard
            .mail_mgr
            .clone()
            .ok_or_else(|| "mail not initialized".to_string())?
    }; // NodeState lock dropped — disk I/O below doesn't block other commands
    let mgr = mgr_arc.lock().map_err(|e| format!("mail lock: {e}"))?;
    Ok(mgr.list_folder(&folder, page, per_page))
}

#[tauri::command]
fn get_mail(
    message_cid: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<mail::MailDetail, String> {
    let mgr_arc = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard
            .mail_mgr
            .clone()
            .ok_or_else(|| "mail not initialized".to_string())?
    };
    let mgr = mgr_arc.lock().map_err(|e| format!("mail lock: {e}"))?;

    // Targeted O(N) scan by reference (no folder clone): only the matching
    // entry is read, even on a 10k-message inbox. If Pending, return a
    // stub MailDetail — the blob doesn't exist on disk yet, so
    // mgr.get_message would fail. Frontend recognizes body_state=Pending
    // and triggers fetch_mail_body.
    if let Some(entry) = mgr.entry_by_cid(&message_cid) {
        if entry.body_state == mail::BodyState::Pending {
            return Ok(mail::MailDetail {
                message_cid: message_cid.clone(),
                message_id: entry.message_id.clone(),
                subject: entry.subject_snippet.clone(),
                body: String::new(),
                sender_address: entry.sender_address.clone(),
                recipients: vec![],
                timestamp: entry.timestamp,
                attachments: vec![],
                is_reply: false,
                is_forward: false,
                in_reply_to: None,
                body_state: mail::BodyState::Pending,
            });
        }
    }

    // Local (or entry missing — let get_message produce the proper error).
    mgr.get_message(&message_cid)
}

#[tauri::command]
async fn refresh_mail(state: tauri::State<'_, Mutex<NodeState>>) -> Result<(), String> {
    let sync_arc = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard
            .mail_sync
            .clone()
            .ok_or_else(|| "mail_sync not initialized".to_string())?
    };
    sync_arc.refresh_now().await;
    Ok(())
}

#[tauri::command]
async fn fetch_mail_body(
    message_cid: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<mail::MailDetail, String> {
    let (sync_arc, mgr_arc) = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        let sync = guard
            .mail_sync
            .clone()
            .ok_or_else(|| "mail_sync not initialized".to_string())?;
        let mgr = guard
            .mail_mgr
            .clone()
            .ok_or_else(|| "mail not initialized".to_string())?;
        (sync, mgr)
    };

    // Decode CID hex → 32-byte array.
    let cid_bytes = hex::decode(&message_cid).map_err(|e| format!("bad cid hex: {e}"))?;
    let cid_arr: [u8; 32] = cid_bytes
        .try_into()
        .map_err(|_| "cid must be 32 bytes".to_string())?;

    // Trigger lazy fetch (no-op if already Local; writes blob + promotes entry).
    sync_arc.fetch_body(cid_arr).await?;

    // Now return the fully-Local MailDetail from the manager.
    let mgr = mgr_arc.lock().map_err(|e| format!("mail lock: {e}"))?;
    mgr.get_message(&message_cid)
}

#[tauri::command]
fn update_mail(
    message_cid: String,
    action: String,
    folder: Option<String>,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    let mgr_arc = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard
            .mail_mgr
            .clone()
            .ok_or_else(|| "mail not initialized".to_string())?
    };
    let mut mgr = mgr_arc.lock().map_err(|e| format!("mail lock: {e}"))?;
    let folder_ref = folder.as_deref();
    match action.as_str() {
        "mark_read" => mgr.mark_read(&message_cid, true, folder_ref),
        "mark_unread" => mgr.mark_read(&message_cid, false, folder_ref),
        "move_trash" => mgr.move_message(&message_cid, folder_ref, "trash"),
        "move_inbox" => mgr.move_message(&message_cid, folder_ref, "inbox"),
        "delete" => mgr.delete_message(&message_cid, folder_ref),
        _ => Err(format!("unknown action: {action}")),
    }
}

#[tauri::command]
fn get_mail_counts(
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<std::collections::HashMap<String, mail::FolderCounts>, String> {
    let mgr_arc = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard
            .mail_mgr
            .clone()
            .ok_or_else(|| "mail not initialized".to_string())?
    };
    let mgr = mgr_arc.lock().map_err(|e| format!("mail lock: {e}"))?;
    Ok(mgr.folder_counts())
}

// ── E2E test helpers (debug builds only) ────────────────────────────────

/// Close a child window by label. Used by the Playwright E2E suite to reset
/// the network-viz window between runs — without this, a leftover viz from
/// the previous run makes the ZEB-144 "Open network visualization" regression
/// guard pass vacuously on reruns.
///
/// Restricted to the `network-viz` label so a stray dev-build IPC call can't
/// take down the main window. Stripped from release binaries entirely via
/// `#[cfg(debug_assertions)]` and the matching conditional registration in
/// `run()` below.
#[cfg(debug_assertions)]
#[tauri::command]
async fn e2e_close_window(app: AppHandle, label: String) -> Result<(), String> {
    use tauri::Manager;
    if label != "network-viz" {
        return Err(format!("e2e_close_window: label '{label}' not allowed"));
    }
    if let Some(window) = app.get_webview_window(&label) {
        window.close().map_err(|e| e.to_string())?;
    }
    Ok(())
}

// ── CLI entry points ─────────────────────────────────────────────────────

/// CLI entry point for `harmony-app rotate-passphrase`.
///
/// Refusal conditions (in order):
///   1. OS keychain has an identity → refuse with explanation
///   2. HARMONY_PASSPHRASE / HARMONY_PASSPHRASE_FILE not set → refuse
///   3. --new-passphrase-file unreadable / empty → refuse
///   4. New passphrase byte-identical to old → log warning, proceed
///
/// Returns Ok(()) on successful rotation; Err on any refusal or rotation
/// failure. Caller (main.rs) translates Err into a non-zero exit.
pub fn rotate_passphrase_cli(new_passphrase_file: &std::path::Path) -> Result<(), String> {
    use identity::KeyStore as _;
    use secrecy::SecretString;

    // Refusal 1: keychain has identity, or its state can't be determined.
    // Failing closed on load() Err is important — if we can't tell whether the
    // identity is in the keychain, we must NOT rotate the encrypted file
    // (the rotation would silently target the wrong backend).
    //
    // KeychainStore::new() Err is trickier. The strict-correct posture is to
    // also fail closed, but that breaks the legitimate headless case (Linux
    // server with no Secret Service / no D-Bus session — the entire point of
    // the encrypted-file backend). The keyring crate's error type doesn't
    // cleanly distinguish "no backend on this system" from "backend present
    // but transiently unreachable", so we can't auto-discriminate. Compromise:
    // log a loud warning on new() Err so an operator on a misconfigured
    // desktop sees a signal, and proceed (the typical headless case is
    // benign). Operators with both a populated OS keychain and an .enc file
    // who hit a transient keychain failure mid-rotation could rotate the
    // wrong backend; this is a known niche risk documented here.
    match identity::KeychainStore::new() {
        Ok(kc) => match kc.load() {
            Ok(Some(_)) => {
                return Err(
                    "your identity is currently in the OS keychain; passphrase rotation only applies to headless installs. \
                     Re-encryption of keychain entries is handled by the OS when you change your login password.".to_string(),
                );
            }
            Ok(None) => {
                // Keychain reachable and empty → safe to rotate the .enc backend.
            }
            Err(e) => {
                return Err(format!(
                    "could not determine whether the identity is stored in the OS keychain — refusing to rotate to avoid acting on the wrong backend: {e}"
                ));
            }
        },
        Err(e) => {
            tracing::warn!(
                "OS keychain backend unavailable ({e}); proceeding with encrypted-file \
                 rotation. If you have a desktop install where the keychain SHOULD be \
                 reachable, this may indicate a transient or configuration issue and the \
                 rotation could affect a different backend than your active identity. \
                 On a headless install (typical case for this command) this is expected."
            );
        }
    }

    // Resolve old passphrase via the standard env chain.
    let plaintext_path = identity::resolve_path(None)?;
    let enc_path = plaintext_path.with_file_name("identity.enc");
    let old_store = identity::EncryptedFileStore::from_env(enc_path)?
        .ok_or_else(|| {
            "HARMONY_PASSPHRASE / HARMONY_PASSPHRASE_FILE not set — cannot rotate without the old passphrase".to_string()
        })?;

    // Read the new passphrase file via the same parser as HARMONY_PASSPHRASE_FILE
    // — UTF-8, exactly one trailing newline strip, empty rejection, AND the
    // 0600-mode warning the inline version was missing.
    let new_str = identity::parse_passphrase_file(new_passphrase_file).map_err(|e| {
        format!(
            "--new-passphrase-file={} {e}",
            new_passphrase_file.display()
        )
    })?;

    // Move into SecretString immediately so the plaintext String is consumed
    // (no second copy lingers on the heap unzeroed). passphrase_eq takes a
    // borrow, then rotate_passphrase moves the SecretString through.
    let candidate = SecretString::from(new_str);
    if old_store.passphrase_eq(&candidate) {
        tracing::warn!("new passphrase matches old — proceeding anyway");
    }

    // Rotate.
    identity::rotate_passphrase(&old_store, candidate)?;
    Ok(())
}

// ── ZEB-217 community IPC types ──────────────────────────────────────────

/// Frontend-facing member status. Mirrors `MemberStatus` from
/// `community_membership` but serializes with human-readable strings
/// ("joined" / "left" / "invited" / "banned") instead of the CBOR wire
/// codes ("j" / "l" / "i" / "b") — the CBOR renames exist for canonical
/// CBOR compactness on the wire, but the Tauri IPC boundary should
/// surface a string the frontend can read directly without a lookup
/// table.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum MemberStatusDto {
    Joined,
    Left,
    Invited,
    Banned,
}

impl From<crate::community_membership::MemberStatus> for MemberStatusDto {
    fn from(s: crate::community_membership::MemberStatus) -> Self {
        use crate::community_membership::MemberStatus;
        match s {
            MemberStatus::Joined => Self::Joined,
            MemberStatus::Left => Self::Left,
            MemberStatus::Invited => Self::Invited,
            MemberStatus::Banned => Self::Banned,
        }
    }
}

/// Member-list row returned by `list_community_members` IPC. Mirrors
/// the spec's MemberInfo type. `addr` is hex of OwnerAddr (16 bytes →
/// 32 chars). `display_name` is None in Phase 3 — the existing profile
/// cache lookup is wired in Phase 5.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MemberInfoDto {
    pub addr: String,
    pub display_name: Option<String>,
    pub status: MemberStatusDto,
    pub power: u8,
    pub joined_at: crate::owner_state_types::Hlc,
}

/// Project a materialized membership into the IPC DTO list, sorted by
/// power level descending then joined_at ascending. Stable for two
/// addrs at the same power+joined_at — falls through to OwnerAddr-bytes
/// comparison so the order is deterministic across calls.
pub fn member_info_for(
    m: &crate::community_membership::MaterializedMembership,
) -> Vec<MemberInfoDto> {
    let mut rows: Vec<MemberInfoDto> = m
        .members
        .iter()
        .map(|(addr, state)| MemberInfoDto {
            addr: hex::encode(addr.0),
            display_name: None,
            status: state.status.into(),
            power: m.power_levels.get(addr).copied().unwrap_or(0),
            joined_at: state.joined_at.clone(),
        })
        .collect();
    rows.sort_by(|a, b| {
        b.power
            .cmp(&a.power)
            .then_with(|| a.joined_at.wall_ms.cmp(&b.joined_at.wall_ms))
            .then_with(|| a.joined_at.logical.cmp(&b.joined_at.logical))
            .then_with(|| a.addr.cmp(&b.addr))
    });
    rows
}

/// Read-only IPC over a community's materialized member list.
/// Returns rows sorted by power desc then joined_at asc (see
/// `member_info_for`). `community_id` is the 32-char lowercase
/// hex of the 16-byte SpaceId.
///
/// Errors:
/// - `Err("invalid community_id hex: ...")` — couldn't parse hex.
/// - `Err("no community_registry — node not running?")` — start_node
///   hasn't wired the registry.
/// - `Err("no Space for community {hex} in owner-state")` — we
///   haven't joined this community (or we left and removed the Space).
/// - `Err("community Space missing admin_addr (corrupt row?)")` —
///   defensive guard; should be unreachable since `validate_invariants`
///   rejects these on apply.
/// - `Err("no engine for community {hex} — not joined or not yet
///   started")` — the community isn't in the registry's map.
#[tauri::command]
async fn list_community_members(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
) -> Result<Vec<MemberInfoDto>, String> {
    let id_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "community_id must be 16 bytes (32 hex chars)".to_string())?;
    let space_id = crate::owner_state_types::SpaceId(id_bytes);

    let (crdt_state, registry) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.crdt_state
                .clone()
                .ok_or("crdt_state missing — node not running?")?,
            g.community_registry
                .clone()
                .ok_or("no community_registry — node not running?")?,
        )
    };

    let admin_addr = {
        let s = crdt_state.lock().await;
        let space = s.spaces.get(&space_id).cloned();
        drop(s);
        let space = space.ok_or_else(|| {
            format!(
                "no Space for community {} in owner-state",
                hex::encode(space_id.0)
            )
        })?;
        if space.kind != crate::owner_state_types::SpaceKind::Community {
            return Err(format!(
                "Space {} exists but is kind {:?}, not Community",
                hex::encode(space_id.0),
                space.kind
            ));
        }
        space
            .admin_addr
            .ok_or("community Space missing admin_addr (corrupt row?)")?
    };

    let engine_state = registry.state_for(&space_id).await.ok_or_else(|| {
        format!(
            "no engine for community {} — not joined or not yet started",
            hex::encode(space_id.0)
        )
    })?;

    let materialized = {
        let g = engine_state.lock().await;
        g.materialize_now(admin_addr)
    };

    Ok(member_info_for(&materialized))
}

/// Encode a CommunityInvitePayload into the harmony://invite/ URL form.
/// Thin wrapper over `community_invite::encode_invite_url` so call sites
/// don't need to import the lower-level error type — surfaces failures
/// as `Result<String, String>` matching the IPC convention.
pub fn build_open_invite_url(
    payload: &crate::community_invite::CommunityInvitePayload,
) -> Result<String, String> {
    crate::community_invite::encode_invite_url(payload)
        .map_err(|e| format!("encode invite URL: {e}"))
}

/// Generate a `harmony://invite/...` URL for an OPEN community. The
/// returned URL carries the community id + symmetric `MembershipKey` +
/// admin addr + community name, so any holder can decrypt the
/// state-root topic and publish their own Join event.
///
/// `invitee_hint` and `expires_at` are accepted to match the spec's IPC
/// contract but are unused in Phase 3 — Phase 4 will sign an
/// `InviteToken` carrying both. Phase 3 returns a token-less payload.
///
/// Errors:
/// - `Err("invalid community_id hex: ...")` — bad hex.
/// - `Err("no community_registry — node not running?")` — registry not
///   wired (start_node hasn't run).
/// - `Err("no Space for community {hex} in owner-state")` — the
///   community isn't in our local owner-state (we haven't joined or
///   we left).
/// - `Err("community Space missing membership_key / admin_addr / kind")`
///   — defensive guard; should be unreachable since
///   `validate_invariants` rejects these on apply, but cheap to check.
#[tauri::command]
async fn generate_invite(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
    invitee_hint: Option<String>,
    expires_at: Option<u64>,
) -> Result<String, String> {
    let _ = (invitee_hint, expires_at); // Phase 4 wiring; ignored in Phase 3.

    let id_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "community_id must be 16 bytes (32 hex chars)".to_string())?;
    let space_id = crate::owner_state_types::SpaceId(id_bytes);

    let crdt_state = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        g.crdt_state
            .clone()
            .ok_or("crdt_state missing — node not running?")?
    };

    let space = {
        let s = crdt_state.lock().await;
        s.spaces.get(&space_id).cloned()
    }
    .ok_or_else(|| {
        format!(
            "no Space for community {} in owner-state",
            hex::encode(space_id.0)
        )
    })?;

    if space.kind != crate::owner_state_types::SpaceKind::Community {
        return Err(format!(
            "Space {} exists but is kind {:?}, not Community",
            hex::encode(space_id.0),
            space.kind
        ));
    }
    let mk = space
        .membership_key
        .clone()
        .ok_or("community Space missing membership_key (corrupt row?)")?;
    let admin = space
        .admin_addr
        .ok_or("community Space missing admin_addr (corrupt row?)")?;
    let is_invite_only = space.is_invite_only.unwrap_or(false);

    if is_invite_only {
        return Err(
            "Phase 3 supports OPEN communities only; invite-only generate_invite ships in Phase 4"
                .to_string(),
        );
    }

    let payload = crate::community_invite::CommunityInvitePayload {
        community_id: space_id,
        membership_key: mk,
        admin_addr: admin,
        community_name: space.name.clone(),
        is_invite_only: false,
        expires_at: None,
        invite_token: None,
    };
    build_open_invite_url(&payload)
}

// ── ZEB-217 Sub-C Phase 3 Task 9: create_community ───────────────────
//
// Mints a new community: fresh community_id, fresh MembershipKey,
// bootstrap admin self-Join SignedMembershipEvent, then applies the
// Community Space row to owner-state CRDT, advances the local HLC
// tracker, spawns a CommunitySyncEngine via the registry, hands the
// adapter wiring to event_loop through a channel, and finally
// `insert_local_event`s the bootstrap Join into the engine so the
// debounced state-root publish picks it up.
//
// The `mint_community_creation` pure function is exposed separately
// (no NodeState, no async, no I/O) so the canonical-CBOR / signing
// path is unit-testable without standing up a Tauri test harness.

/// Bundle of values produced by `mint_community_creation` — kept as a
/// plain struct so callers (the `create_community` IPC + tests) can
/// destructure cleanly.
pub struct MintedCommunity {
    pub community_id: crate::owner_state_types::SpaceId,
    pub membership_key: crate::owner_state_types::MembershipKey,
    pub space: crate::owner_state_types::Space,
    pub bootstrap_join: crate::community_membership::SignedMembershipEvent,
}

/// Pure function: mint a fresh community + signed bootstrap Join.
///
/// Generates random `community_id` (16 bytes) and `MembershipKey`
/// (32 bytes), advances HLC from `prev_hlc`, builds the Community
/// Space row, signs a self-Join `SignedMembershipEvent` with the
/// caller's ed25519 key. Returns all four artefacts so the IPC layer
/// can apply the Space, send the Join through the engine, and return
/// the hex id to the frontend.
///
/// Pure / sync / no I/O — every random byte and HLC tick is sourced
/// from the args. This lets the test (`create_community_inner_tests`)
/// cover the full mint without spawning channels, mutexes, or a Tauri
/// runtime.
pub fn mint_community_creation(
    name: &str,
    is_invite_only: bool,
    self_owner: crate::owner_state_types::OwnerAddr,
    signing_key: &ed25519_dalek::SigningKey,
    device_id: &str,
    wall_now_ms: u64,
    prev_hlc: Option<&crate::owner_state_types::Hlc>,
) -> Result<MintedCommunity, String> {
    use crate::community_membership::{sign_event, EventPayload, MembershipEventKind};
    use crate::owner_state_types::{MembershipKey, Space, SpaceId, SpaceKind};
    use rand::RngCore;

    let mut rng = rand::thread_rng();
    let mut id_bytes = [0u8; 16];
    rng.fill_bytes(&mut id_bytes);
    let community_id = SpaceId(id_bytes);

    let mut mk_bytes = [0u8; 32];
    rng.fill_bytes(&mut mk_bytes);
    let membership_key = MembershipKey::new(mk_bytes);

    let creation_hlc = crate::dm_outbox::next_hlc(prev_hlc, wall_now_ms, device_id);

    let mut event_id_bytes = [0u8; 16];
    rng.fill_bytes(&mut event_id_bytes);
    let join_payload = EventPayload {
        id: event_id_bytes,
        community_id,
        kind: MembershipEventKind::Join,
        actor: self_owner,
        at: creation_hlc.clone(),
    };
    let bootstrap_join =
        sign_event(&join_payload, signing_key).map_err(|e| format!("sign bootstrap join: {e}"))?;

    let space = Space {
        id: community_id,
        kind: SpaceKind::Community,
        parent: None,
        community_id: None,
        name: name.to_string(),
        transport: None,
        members: Vec::new(),
        custom_name: None,
        notification_pref: None,
        left_at: None,
        created_at: creation_hlc.clone(),
        updated_at: creation_hlc,
        content_key: None,
        prior_content_keys: Vec::new(),
        membership_key: Some(membership_key.clone()),
        admin_addr: Some(self_owner),
        is_invite_only: Some(is_invite_only),
    };

    Ok(MintedCommunity {
        community_id,
        membership_key,
        space,
        bootstrap_join,
    })
}

/// Internal helper for `create_community`. Takes already-snapshotted
/// handles; pure of `tauri::State`. The final generation fence
/// re-acquires the std `NodeState` lock (passed as `&Mutex<NodeState>`)
/// to guard against a stop-during-await race. ZEB-258: owner-state
/// Space commit is the LAST persistent step. Failures BEFORE the
/// commit tear down the engine + return Err with crdt_state untouched.
///
/// Takes `&Mutex<NodeState>` rather than `tauri::State<'_, Mutex<NodeState>>`
/// so integration tests can invoke the helper directly with a
/// freshly-constructed `Mutex<NodeState>` (the regression test for the
/// ZEB-258 reorder is load-bearing only when it actually drives the
/// production code path). The wrapper passes `&state_lock` (Tauri's
/// `State` auto-derefs to `&Mutex<NodeState>`).
///
/// Argument shape mirrors what `redeem_invite_inner` will look like
/// (Phase 4 Task 8 will extract it the same way) so the two IPCs share
/// a code-review pattern.
///
/// Lock-order discipline (load-bearing — flagged on PR #86 round 2):
/// the `crdt_state` `tokio::sync::Mutex` guard MUST drop before
/// `hlc_tracker.lock().await` is acquired. Holding `state_g` across
/// `tracker_g.lock().await` would (a) violate the project-wide "no
/// `.await` while holding state mutex" rule, and (b) invert lock order
/// vs callers that take `hlc_tracker` first — a deadlock risk under
/// concurrent IPCs. The post-reorder body acquires both locks only at
/// the END (tracker before crdt_state, then commit), so the rule is
/// preserved.
#[allow(clippy::too_many_arguments)]
pub async fn create_community_inner(
    name: String,
    is_invite_only: bool,
    crdt_state: std::sync::Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
    hlc_tracker: std::sync::Arc<
        tokio::sync::Mutex<std::collections::BTreeMap<String, crate::owner_state_types::Hlc>>,
    >,
    device_id: String,
    self_owner: crate::owner_state_types::OwnerAddr,
    signing_key: std::sync::Arc<ed25519_dalek::SigningKey>,
    community_registry: std::sync::Arc<crate::community_state_sync::CommunitySyncRegistry>,
    community_adapter_tx: tokio::sync::mpsc::Sender<crate::event_loop::CommunityAdapterRequest>,
    snapshot_generation: u64,
    node_state: &std::sync::Mutex<NodeState>,
) -> Result<String, String> {
    // Phase 4 unblocks invite-only minting. `is_invite_only` flows
    // through `mint_community_creation` into the Space row + engine
    // config; the verify chain enforces invite-only semantics on every
    // Join from there. The receive-side counter-sign hop ships with
    // this PR; share-side `generate_invite` for invite-only is still
    // its own work item (the IPC handler still blocks it pending an
    // InviteToken sign path).

    let wall_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    // Mint the Space + signed bootstrap Join. Read prev_hlc under the
    // tracker lock then drop the guard before signing (sign is sync;
    // releasing eagerly keeps the tracker available to other tasks).
    // ZEB-258: NO mutation of owner-state or hlc_tracker yet — the mint
    // is pure / sync and produces values, not side effects.
    let minted = {
        let prev_hlc = {
            let t = hlc_tracker.lock().await;
            t.get(&device_id).cloned()
        };
        mint_community_creation(
            &name,
            is_invite_only,
            self_owner,
            signing_key.as_ref(),
            &device_id,
            wall_now_ms,
            prev_hlc.as_ref(),
        )?
    };

    // ZEB-258: spawn engine + dispatch adapter BEFORE the owner-state
    // commit. Both can fail; both have rollback paths (engine teardown
    // via `stop_engine` — Task 7 of the Phase 4 plan will swap in
    // `shutdown_engine_and_cleanup_persistence` so the per-community
    // persistence dir is also removed). At this point owner-state is
    // unchanged.
    //
    // Channel pair shape mirrors start_node's per-community spawn
    // path: pub_tx / sub_rx feed the engine, pub_rx / sub_tx feed the
    // Zenoh adapter. The CommunityAdapterRequest carries the adapter
    // halves into event_loop via the mpsc; the event loop spawns the
    // adapter against its live session.
    let (pub_tx, pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let (sub_tx, sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

    community_registry
        .spawn_engine(
            minted.community_id,
            minted.membership_key.clone(),
            self_owner,
            is_invite_only,
            pub_tx,
            sub_rx,
        )
        .await
        .map_err(|e| format!("registry.spawn_engine: {e}"))?;

    if let Err(e) = community_adapter_tx.try_send(crate::event_loop::CommunityAdapterRequest {
        id_hex: hex::encode(minted.community_id.0),
        publisher_rx: pub_rx,
        subscriber_tx: sub_tx,
    }) {
        // Engine is in the registry but adapter wiring failed. Tear it
        // down so we don't accumulate a zombie engine. ZEB-258 win:
        // owner-state is still untouched at this point. ZEB-262 Task 7:
        // shutdown_engine_and_cleanup_persistence also removes the
        // orphan per-community persistence dir, closing the disk-leak
        // gap that the bare stop_engine call tolerated.
        if let Err(stop_err) = community_registry
            .shutdown_engine_and_cleanup_persistence(&minted.community_id)
            .await
        {
            tracing::warn!(
                error = %stop_err,
                community_id = %hex::encode(minted.community_id.0),
                "shutdown_engine_and_cleanup_persistence failed during create_community \
                 rollback (adapter dispatch)"
            );
        }
        return Err(match e {
            tokio::sync::mpsc::error::TrySendError::Full(_) => {
                "adapter request queue full; please retry".to_string()
            }
            tokio::sync::mpsc::error::TrySendError::Closed(_) => {
                "adapter request channel closed (event_loop stopped?)".to_string()
            }
        });
    }

    // Bootstrap-Join via the engine. The engine's `insert_local_event`
    // runs verify_event (which authorizes the admin self-Join via the
    // bootstrap rule) and fires `notify_dirty` on success; the debounced
    // publish picks up the event and writes to the per-community
    // state-root topic. ZEB-258: still BEFORE the owner-state commit;
    // a failure here tears the engine down with crdt_state untouched.
    let engine_arc = community_registry
        .engine_arc(&minted.community_id)
        .await
        .ok_or("engine vanished immediately after spawn — registry race")?;
    // CodeRabbit P0: a `?` early-return here would leave the spawned
    // engine + persistence dir behind. Wrap the Result and tear down
    // on Err before returning.
    let outcome = match engine_arc
        .insert_local_event(minted.bootstrap_join.clone())
        .await
    {
        Ok(o) => o,
        Err(e) => {
            if let Err(stop_err) = community_registry
                .shutdown_engine_and_cleanup_persistence(&minted.community_id)
                .await
            {
                tracing::warn!(
                    error = %stop_err,
                    community_id = %hex::encode(minted.community_id.0),
                    "shutdown failed during create_community_inner insert-err rollback"
                );
            }
            return Err(format!("engine.insert_local_event: {e}"));
        }
    };
    if !matches!(
        outcome,
        crate::community_state_crdt::InsertOutcome::Inserted
    ) {
        // Bootstrap Join didn't insert — engine state is inconsistent
        // with the user-visible "creator just made this community"
        // expectation. Tear down + bail. Owner-state still untouched.
        // ZEB-262 Task 7: cleanup also removes the per-community
        // persist dir.
        if let Err(stop_err) = community_registry
            .shutdown_engine_and_cleanup_persistence(&minted.community_id)
            .await
        {
            tracing::warn!(
                error = %stop_err,
                community_id = %hex::encode(minted.community_id.0),
                "shutdown_engine_and_cleanup_persistence failed during create_community \
                 rollback (bootstrap-Join not inserted)"
            );
        }
        return Err(format!("bootstrap Join not inserted (got {outcome:?})"));
    }

    // ZEB-258: SNAPSHOT-THEN-COMMIT FENCE. If the node generation
    // changed since we snapshotted, owner-state is on a different
    // lifetime — abort. Mirrors add_space's post-stop guard. Done
    // BEFORE the owner-state commit so a stop-during-await race is
    // caught with crdt_state untouched. Capture the verdict + the
    // current generation under the std lock then drop the guard
    // BEFORE any `.await` (`std::sync::MutexGuard` is `!Send`, so a
    // held guard across an .await would prevent the future from
    // implementing Send — see tauri::command's Send bound).
    enum FenceVerdict {
        Ok,
        GenerationChanged(u64),
        RegistryGone,
    }
    let verdict = {
        let g = node_state
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        if g.generation != snapshot_generation {
            FenceVerdict::GenerationChanged(g.generation)
        } else if g.community_registry.is_none() {
            FenceVerdict::RegistryGone
        } else {
            FenceVerdict::Ok
        }
    }; // std lock guard dropped here before any .await.
    match verdict {
        FenceVerdict::Ok => {}
        FenceVerdict::GenerationChanged(now_gen) => {
            // ZEB-262 Task 7: shutdown_engine_and_cleanup_persistence
            // tears the engine down AND removes the per-community
            // persist dir, so a fence-aborted community doesn't leave
            // an orphan crdt.cbor / replay.cbor on disk.
            if let Err(stop_err) = community_registry
                .shutdown_engine_and_cleanup_persistence(&minted.community_id)
                .await
            {
                tracing::warn!(
                    error = %stop_err,
                    community_id = %hex::encode(minted.community_id.0),
                    "shutdown_engine_and_cleanup_persistence failed during \
                     create_community fence-abort"
                );
            }
            return Err(format!(
                "node generation changed during create_community (was {}, now {}); \
                 community minted on a detached crdt_state and won't be persisted — \
                 engine spawn suppressed",
                snapshot_generation, now_gen
            ));
        }
        FenceVerdict::RegistryGone => {
            if let Err(stop_err) = community_registry
                .shutdown_engine_and_cleanup_persistence(&minted.community_id)
                .await
            {
                tracing::warn!(
                    error = %stop_err,
                    community_id = %hex::encode(minted.community_id.0),
                    "shutdown_engine_and_cleanup_persistence failed during \
                     create_community fence-abort"
                );
            }
            return Err(
                "community_registry was torn down during create_community — engine spawn \
                 suppressed"
                    .to_string(),
            );
        }
    }

    // ZEB-258: advance the HLC tracker, then COMMIT owner-state Space
    // LAST. Tracker advance comes first so under the lock-order rule
    // (tracker before crdt_state) we never hold the std `state_lock`
    // while awaiting either; the tracker is also strictly-additive so
    // a reserved-but-unused slot on a later commit failure is harmless
    // (monotonicity only requires strictly-increasing HLCs).
    {
        let mut tracker_g = hlc_tracker.lock().await;
        // Bootstrap creation: prev_hlc was either None or strictly
        // older than `created_at` (next_hlc guarantees forward step),
        // so unconditionally advancing the tracker is correct.
        tracker_g.insert(device_id.clone(), minted.space.created_at.clone());
    }

    {
        let mut state_g = crdt_state.lock().await;
        let outcome = state_g.apply_space_with_canonicalization(minted.space.clone());
        if matches!(outcome, crate::owner_state_crdt::ApplyOutcome::Rejected(_)) {
            // Owner-state rejected the Space (CRDT invariant). The
            // engine is up and the bootstrap-Join is in its log, but
            // owner-state has no Space row — tear the engine down so
            // we don't leak a zombie. Drop the state_g guard FIRST so
            // we don't hold a `tokio::sync::Mutex` guard across the
            // `.await` of the registry call. ZEB-262 Task 7: cleanup
            // also removes the per-community persist dir.
            drop(state_g);
            if let Err(stop_err) = community_registry
                .shutdown_engine_and_cleanup_persistence(&minted.community_id)
                .await
            {
                tracing::warn!(
                    error = %stop_err,
                    community_id = %hex::encode(minted.community_id.0),
                    "shutdown_engine_and_cleanup_persistence failed during \
                     create_community rollback (apply_space rejected)"
                );
            }
            return Err(format!("apply_space rejected new community: {outcome:?}"));
        }
    }

    Ok(hex::encode(minted.community_id.0))
}

/// Tauri IPC: create a fresh OPEN community.
///
/// Phase 3 ships only OPEN communities; invite-only `create_community`
/// returns `Err` until Phase 4 lands the invite-token signing.
///
/// Snapshots the relevant `NodeState` handles under the std lock, then
/// delegates to `create_community_inner`, which encodes the ZEB-258
/// reorder (owner-state Space commit is the LAST persistent step;
/// engine + adapter failures roll back with crdt_state untouched).
///
/// Adapter wiring flows through `event_loop` (not directly through
/// the Zenoh `Session` from this command's task) per spec
/// §"Architecture / Adapter wiring": this command sends a
/// `CommunityAdapterRequest` over an mpsc; the event loop's `select!`
/// drains it and calls `spawn_community_state_zenoh_adapter` against
/// the live session.
#[tauri::command]
async fn create_community(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    name: String,
    is_invite_only: bool,
) -> Result<String, String> {
    // Snapshot NodeState handles in a single guard scope, then drop
    // the std lock BEFORE any `.await`. The signing key lives inside
    // dm_outbox under a tokio Mutex, so we acquire the dm_outbox
    // handle under the std lock (Arc clone) and `.await` its lock
    // afterward.
    let (
        crdt_state,
        hlc_tracker,
        device_id,
        self_owner,
        community_registry,
        community_adapter_tx,
        dm_outbox,
        snapshot_generation,
    ) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.crdt_state
                .clone()
                .ok_or("crdt_state missing — node not running?")?,
            g.hlc_tracker.clone().ok_or("hlc_tracker missing")?,
            g.dm_device_id.clone().ok_or("dm_device_id missing")?,
            g.dm_self_owner.ok_or("dm_self_owner missing")?,
            g.community_registry
                .clone()
                .ok_or("community_registry missing — node not running?")?,
            g.community_adapter_request_tx
                .clone()
                .ok_or("community_adapter_request_tx missing")?,
            g.dm_outbox
                .clone()
                .ok_or("dm_outbox missing — no owner identity?")?,
            g.generation,
        )
    }; // std `state_lock` guard dropped here.

    // Now safe to `.await` — the std lock has been released.
    let signing_key = {
        let outbox_g = dm_outbox.lock().await;
        std::sync::Arc::clone(&outbox_g.signing_key)
    };

    create_community_inner(
        name,
        is_invite_only,
        crdt_state,
        hlc_tracker,
        device_id,
        self_owner,
        signing_key,
        community_registry,
        community_adapter_tx,
        snapshot_generation,
        &state_lock,
    )
    .await
}

#[cfg(test)]
mod create_community_inner_tests {
    use super::*;
    use crate::owner_state_types::{Hlc, OwnerAddr};
    use harmony_identity::PrivateIdentity;

    #[test]
    fn mint_creation_produces_consistent_id_join_event_and_space() {
        let identity = PrivateIdentity::from_seed(&[0xc1; 32]);
        let self_owner = OwnerAddr(identity.identity.address_hash);
        // Reach into the PrivateIdentity's signing path the same way
        // production does: the canonical 32-byte seed lives in bytes
        // 32..64 of `to_private_bytes()` (X25519_secret(32) ||
        // Ed25519_secret(32)). dm_outbox stores the SigningKey
        // constructed from those bytes; mirror that here so the test
        // signs with the same key the IPC will use in production.
        let sk_bytes_full = identity.to_private_bytes();
        let ed_seed: [u8; 32] = sk_bytes_full[32..64]
            .try_into()
            .expect("ed25519 seed slice 32..64");
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&ed_seed);

        let device_id = "creator-dev";
        let prev_hlc: Option<Hlc> = None;
        let wall_now_ms = 1_700_000_000_000u64;

        let minted = mint_community_creation(
            "Hackers United",
            false,
            self_owner,
            &signing_key,
            device_id,
            wall_now_ms,
            prev_hlc.as_ref(),
        )
        .expect("mint");

        assert_eq!(
            minted.space.kind,
            crate::owner_state_types::SpaceKind::Community
        );
        assert_eq!(minted.space.id, minted.community_id);
        assert_eq!(minted.space.admin_addr, Some(self_owner));
        assert_eq!(minted.space.is_invite_only, Some(false));
        assert!(minted.space.membership_key.is_some());
        assert_eq!(minted.space.name, "Hackers United");
        assert_eq!(minted.space.created_at.wall_ms, wall_now_ms);
        assert_eq!(&minted.space.created_at.device_id, device_id);

        assert_eq!(minted.bootstrap_join.actor, self_owner);
        assert_eq!(minted.bootstrap_join.community_id, minted.community_id);
        assert!(matches!(
            minted.bootstrap_join.kind,
            crate::community_membership::MembershipEventKind::Join
        ));
        assert_eq!(minted.bootstrap_join.at.wall_ms, wall_now_ms);
        assert!(
            minted.bootstrap_join.countersig.is_none(),
            "open / bootstrap Join carries no countersig"
        );

        // Two consecutive mints must produce DISTINCT community ids /
        // event ids / membership keys — the random source has to fire
        // per call, otherwise two communities created in a row would
        // collide. (16-byte / 32-byte randomness collision is
        // astronomically unlikely; this just guards against a
        // rng-reuse / fixed-buffer bug.)
        let minted2 = mint_community_creation(
            "Other Community",
            false,
            self_owner,
            &signing_key,
            device_id,
            wall_now_ms,
            prev_hlc.as_ref(),
        )
        .expect("mint2");
        assert_ne!(minted.community_id, minted2.community_id);
        assert_ne!(minted.bootstrap_join.id, minted2.bootstrap_join.id);
        assert_ne!(
            minted.space.membership_key.as_ref().unwrap().as_bytes(),
            minted2.space.membership_key.as_ref().unwrap().as_bytes(),
        );

        // Bootstrap signature MUST verify against self_owner's
        // identity_pub — the engine's verify_event will run the same
        // check on insert_local_event.
        let identity_pub = identity.identity.to_public_bytes();
        crate::community_membership::verify_signature(&minted.bootstrap_join, &identity_pub)
            .expect("bootstrap join signature must verify against self identity_pub");
    }
}

// ── ZEB-217 Sub-C Phase 3 Task 10: redeem_invite ─────────────────────
//
// Joiner-side mirror of `create_community`: decodes a
// `harmony://invite/...` URL, mints a signed self-Join, applies the
// derived Community Space row to owner-state CRDT, advances the local
// HLC tracker, spawns a CommunitySyncEngine via the registry (passing
// the invite's admin_addr — NOT the joining peer — so the engine's
// authority root matches the inviter's), hands the adapter wiring to
// event_loop, and finally `insert_local_event`s the self-Join so the
// debounced state-root publish picks it up.
//
// Phase 3 supports OPEN-only redemption; invite-only (with countersig
// fan-out via Reticulum) ships in Phase 4.
//
// Cross-peer dedupe: the new Space row's id is `payload.community_id`,
// IDENTICAL to the creator's Space row id. apply_space's CRDT
// last-writer-wins on (id, hlc) collapses the two rows correctly when
// the peers eventually sync. Phase 1's same-SpaceId rejection of
// community-creation field changes (admin_addr / membership_key /
// is_invite_only) defends against malicious or stale invites trying
// to drift the canonical row out from under the original creator.

/// Pure function: mint a joiner-side self-Join + derived Community
/// Space row from an invite payload.
///
/// Generates a random 16-byte event id, advances HLC from `prev_hlc`,
/// constructs the Community Space row using the invite's
/// `community_id` / `membership_key` / `admin_addr` / `community_name`
/// / `is_invite_only`, and signs a self-Join `SignedMembershipEvent`
/// (actor = `self_owner`, community_id = `payload.community_id`).
///
/// Pure / sync / no I/O — every random byte and HLC tick is sourced
/// from the args, so the test (`redeem_invite_inner_tests`) can cover
/// the full mint without spawning channels, mutexes, or a Tauri
/// runtime.
pub fn mint_redemption(
    payload: &crate::community_invite::CommunityInvitePayload,
    self_owner: crate::owner_state_types::OwnerAddr,
    signing_key: &ed25519_dalek::SigningKey,
    device_id: &str,
    wall_now_ms: u64,
    prev_hlc: Option<&crate::owner_state_types::Hlc>,
) -> Result<MintedCommunity, String> {
    use crate::community_membership::{sign_event, EventPayload, MembershipEventKind};
    use crate::owner_state_types::{Space, SpaceKind};
    use rand::RngCore;

    let mut rng = rand::thread_rng();
    let mut event_id_bytes = [0u8; 16];
    rng.fill_bytes(&mut event_id_bytes);

    let join_hlc = crate::dm_outbox::next_hlc(prev_hlc, wall_now_ms, device_id);

    let join_payload = EventPayload {
        id: event_id_bytes,
        community_id: payload.community_id,
        kind: MembershipEventKind::Join,
        actor: self_owner,
        at: join_hlc.clone(),
    };
    let bootstrap_join =
        sign_event(&join_payload, signing_key).map_err(|e| format!("sign self-join: {e}"))?;

    let space = Space {
        id: payload.community_id,
        kind: SpaceKind::Community,
        parent: None,
        community_id: None,
        name: payload.community_name.clone(),
        transport: None,
        members: Vec::new(),
        custom_name: None,
        notification_pref: None,
        left_at: None,
        created_at: join_hlc.clone(),
        updated_at: join_hlc,
        content_key: None,
        prior_content_keys: Vec::new(),
        membership_key: Some(payload.membership_key.clone()),
        admin_addr: Some(payload.admin_addr),
        // Use the invite's declared is_invite_only so the redeemer's
        // Space row matches the creator's row (Phase 1's CRDT same-
        // SpaceId rejection of community-creation field changes would
        // silently reject the redemption Space if these disagreed).
        // In Phase 3 the IPC guard rejects invite-only payloads before
        // we ever reach mint_redemption, so this currently equals
        // Some(false) at runtime — but pinning to payload.is_invite_only
        // unblocks Phase 4 invite-only redemption with no further mint
        // edits.
        is_invite_only: Some(payload.is_invite_only),
    };

    Ok(MintedCommunity {
        community_id: payload.community_id,
        membership_key: payload.membership_key.clone(),
        space,
        bootstrap_join,
    })
}

/// ZEB-262 Phase 4: invite-only `redeem_invite` inner helper. Encodes
/// the 10-step flow per spec §"Send path: redeem_invite":
///
///   1. decode URL (caller-supplied; we receive it as `url: String`)
///   2. snapshot handles (done by caller — passed in as args)
///   3. wall_now_ms
///   4. RESERVE HLC under tracker lock
///   5. mint_redemption (pure helper from Phase 3)
///   6. spawn_engine + dispatch adapter
///   7. branch on `payload.is_invite_only`:
///      - OPEN — `engine.insert_local_event(bootstrap_join)`
///      - INVITE-ONLY:
///        - 7a. register oneshot keyed on `bootstrap_join.id` via
///          `community_registry.register_pending_redemption`
///        - 7b. build + sign `CommunityInviteSigned`
///        - 7c. resolve inviter Reticulum dest(s); send packet via
///          `unicast_send_tx`
///        - 7d. await oneshot ≤ T (env
///          `HARMONY_REDEEM_INVITE_TIMEOUT_MS`, default 15s)
///   8. fence_check (generation guard via closure)
///   9. COMMIT owner-state Space (LAST step — ZEB-258 reorder)
///  10. return `Ok(hex(community_id))`
///
/// On any failure between steps 6-8, the engine is torn down via
/// `community_registry.shutdown_engine_and_cleanup_persistence` (Task
/// 7) and owner-state is byte-identical to pre-call.
///
/// **Closure-pattern fence_check.** The IPC wrapper passes a closure
/// that re-locks the std `NodeState` mutex and compares `generation`.
/// Tests pass `|| Ok(())` since they don't drive the fence. Production
/// passes a closure that captures `&Mutex<NodeState>` + the snapshot.
/// `Fn` (not `FnOnce`) so future-proof retries can re-call without
/// consuming.
#[allow(clippy::too_many_arguments)]
pub async fn redeem_invite_inner<F>(
    url: String,
    crdt_state: std::sync::Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
    hlc_tracker: std::sync::Arc<
        tokio::sync::Mutex<std::collections::BTreeMap<String, crate::owner_state_types::Hlc>>,
    >,
    device_id: String,
    self_owner: crate::owner_state_types::OwnerAddr,
    signing_key: std::sync::Arc<ed25519_dalek::SigningKey>,
    community_registry: std::sync::Arc<crate::community_state_sync::CommunitySyncRegistry>,
    community_adapter_tx: tokio::sync::mpsc::Sender<crate::event_loop::CommunityAdapterRequest>,
    unicast_send_tx: tokio::sync::mpsc::Sender<crate::dm_outbox::UnicastSendRequest>,
    dm_outbox: std::sync::Arc<tokio::sync::Mutex<crate::dm_outbox::DmOutbox>>,
    fence_check: F,
) -> Result<String, String>
where
    F: Fn() -> Result<(), String>,
{
    // 1. Decode URL.
    let payload = crate::community_invite::decode_invite_url(&url)
        .map_err(|e| format!("decode invite URL: {e}"))?;

    let wall_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    // 4. Reserve HLC under tracker lock.
    let prev_hlc = {
        let t = hlc_tracker.lock().await;
        t.get(&device_id).cloned()
    };

    // 5. Mint (pure helper — no side effects on owner-state yet).
    let minted = mint_redemption(
        &payload,
        self_owner,
        signing_key.as_ref(),
        &device_id,
        wall_now_ms,
        prev_hlc.as_ref(),
    )?;

    // Advance the HLC tracker. Strictly-additive: a reserved-but-
    // unused slot on a later commit failure is harmless (monotonicity
    // only requires strictly-increasing HLCs).
    {
        let mut tracker_g = hlc_tracker.lock().await;
        tracker_g.insert(device_id.clone(), minted.space.created_at.clone());
    }

    // 6. Spawn engine + dispatch adapter. Both can fail; failure
    //    rolls back via `shutdown_engine_and_cleanup_persistence`.
    //    Owner-state is still untouched at this point.
    //
    //    spawn_engine takes `payload.admin_addr` (the original
    //    community admin from the invite), NOT self_owner — the
    //    engine's authority root is the creator's identity. Invite-
    //    only engines spawn with `is_invite_only=true` so verify_event
    //    applies the countersig rule.
    //
    //    Re-redemption guard: `engine_arc(...).is_some()` BEFORE
    //    spawn_engine so the gating decision isn't subject to a race
    //    against owner-state add events. spawn_engine is idempotent;
    //    on a pre-existing engine, the freshly-built channels are
    //    dropped (the engine already owns its live adapter pair).
    let engine_already_existed = community_registry
        .engine_arc(&minted.community_id)
        .await
        .is_some();

    let (pub_tx, pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let (sub_tx, sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

    community_registry
        .spawn_engine(
            minted.community_id,
            minted.membership_key.clone(),
            payload.admin_addr,
            payload.is_invite_only,
            pub_tx,
            sub_rx,
        )
        .await
        .map_err(|e| format!("registry.spawn_engine: {e}"))?;

    if !engine_already_existed {
        if let Err(e) = community_adapter_tx.try_send(crate::event_loop::CommunityAdapterRequest {
            id_hex: hex::encode(minted.community_id.0),
            publisher_rx: pub_rx,
            subscriber_tx: sub_tx,
        }) {
            if let Err(stop_err) = community_registry
                .shutdown_engine_and_cleanup_persistence(&minted.community_id)
                .await
            {
                tracing::warn!(
                    error = %stop_err,
                    community_id = %hex::encode(minted.community_id.0),
                    "shutdown_engine_and_cleanup_persistence failed during redeem_invite \
                     adapter-dispatch rollback"
                );
            }
            return Err(match e {
                tokio::sync::mpsc::error::TrySendError::Full(_) => {
                    "adapter request queue full; please retry".to_string()
                }
                tokio::sync::mpsc::error::TrySendError::Closed(_) => {
                    "adapter request channel closed (event_loop stopped?)".to_string()
                }
            });
        }
    } else {
        // Idempotent re-redemption: pre-existing engine already owns
        // its live adapter pair from the original spawn. Drop the
        // freshly-built halves we no longer need.
        drop(pub_rx);
        drop(sub_tx);
    }

    // 7. Branch on payload.is_invite_only.
    if !payload.is_invite_only {
        // OPEN: insert bootstrap_join via the engine. The engine's
        // `insert_local_event` runs verify_event (which authorizes the
        // open Join via signature alone) and fires `notify_dirty` on
        // success.
        let engine_arc = community_registry
            .engine_arc(&minted.community_id)
            .await
            .ok_or("engine vanished immediately after spawn — registry race")?;
        // CodeRabbit P0: a `?` early-return here would leave the
        // spawned engine + persistence dir behind. Wrap the Result and
        // tear down on Err before returning.
        let outcome = match engine_arc
            .insert_local_event(minted.bootstrap_join.clone())
            .await
        {
            Ok(o) => o,
            Err(e) => {
                if let Err(stop_err) = community_registry
                    .shutdown_engine_and_cleanup_persistence(&minted.community_id)
                    .await
                {
                    tracing::warn!(
                        error = %stop_err,
                        community_id = %hex::encode(minted.community_id.0),
                        "shutdown failed during redeem_invite OPEN-branch insert-err rollback"
                    );
                }
                return Err(format!("engine.insert_local_event: {e}"));
            }
        };
        if !matches!(
            outcome,
            crate::community_state_crdt::InsertOutcome::Inserted
        ) {
            // Bootstrap Join didn't insert — tear down so we don't
            // leak a zombie engine.
            if let Err(stop_err) = community_registry
                .shutdown_engine_and_cleanup_persistence(&minted.community_id)
                .await
            {
                tracing::warn!(
                    error = %stop_err,
                    community_id = %hex::encode(minted.community_id.0),
                    "shutdown failed during redeem_invite OPEN-branch insert-rejected rollback"
                );
            }
            return Err(format!("self Join not inserted (got {outcome:?})"));
        }
    } else {
        // INVITE-ONLY: 7a-d.
        // The engine + persistence dir were spawned at step 6; a `?`
        // early-return on a missing invite_token would leak both
        // (Greptile P1: zombie engine on malformed URL). Tear down
        // explicitly before returning.
        let invite_token = match payload.invite_token.as_ref() {
            Some(t) => t.clone(),
            None => {
                if let Err(stop_err) = community_registry
                    .shutdown_engine_and_cleanup_persistence(&minted.community_id)
                    .await
                {
                    tracing::warn!(
                        error = %stop_err,
                        community_id = %hex::encode(minted.community_id.0),
                        "shutdown failed during redeem_invite missing-invite-token rollback"
                    );
                }
                return Err("invite-only payload missing invite_token".to_string());
            }
        };

        // 7a. Register oneshot keyed on bootstrap_join.id. Engine's
        //     insert hook (Task 7's notify_pending_redemption_in_map)
        //     fires it once the counter-signed Join lands.
        let (notify_tx, notify_rx) = tokio::sync::oneshot::channel::<()>();
        community_registry
            .register_pending_redemption(minted.bootstrap_join.id, notify_tx)
            .await;

        // 7b. Build + sign CommunityInviteSigned. Read the joiner's
        //     identity (private_identity → identity_pub +
        //     device_hash) and signing_key from the dm_outbox under
        //     its lock. Drop the guard before any further `.await`
        //     to keep the outbox available to drain ticks.
        let (joiner_pub, joiner_device_hash, sign_key_arc) = {
            let outbox_g = dm_outbox.lock().await;
            let joiner_pub = outbox_g.private_identity.identity.to_public_bytes();
            let joiner_device_hash = crate::owner_state_types::DeviceIdentityHash(
                outbox_g.private_identity.identity.address_hash,
            );
            let sign_key_arc = std::sync::Arc::clone(&outbox_g.signing_key);
            (joiner_pub, joiner_device_hash, sign_key_arc)
        };

        let signed = crate::community_invite::CommunityInviteSigned {
            community_id: minted.community_id,
            join_event: minted.bootstrap_join.clone(),
            invite_token,
            joiner_identity_pub: joiner_pub,
            signing_device_hash: joiner_device_hash,
            // CommunityInvitePacket.created_at carries the joiner's
            // wall-clock at packet construction; reusing the bootstrap
            // Join's HLC keeps the redeemer's outbound packet temporally
            // bound to the event being counter-signed.
            created_at: minted.bootstrap_join.at.clone(),
        };

        // Both encode steps below run AFTER `register_pending_redemption`,
        // so a `?` early-return would leak the registered oneshot AND
        // leave the engine + persistence dir we spawned at step 4
        // running. Roll back explicitly on either error.
        let packet = match crate::community_invite::build_signed_invite_packet(
            signed,
            sign_key_arc.as_ref(),
        ) {
            Ok(p) => p,
            Err(e) => {
                let _ = community_registry
                    .take_pending_redemption(&minted.bootstrap_join.id)
                    .await;
                if let Err(stop_err) = community_registry
                    .shutdown_engine_and_cleanup_persistence(&minted.community_id)
                    .await
                {
                    tracing::warn!(
                        error = %stop_err,
                        community_id = %hex::encode(minted.community_id.0),
                        "shutdown failed during redeem_invite build-packet rollback"
                    );
                }
                return Err(format!("build_signed_invite_packet: {e}"));
            }
        };
        let wire = match crate::community_invite::encode_packet(&packet) {
            Ok(w) => w,
            Err(e) => {
                let _ = community_registry
                    .take_pending_redemption(&minted.bootstrap_join.id)
                    .await;
                if let Err(stop_err) = community_registry
                    .shutdown_engine_and_cleanup_persistence(&minted.community_id)
                    .await
                {
                    tracing::warn!(
                        error = %stop_err,
                        community_id = %hex::encode(minted.community_id.0),
                        "shutdown failed during redeem_invite encode-packet rollback"
                    );
                }
                return Err(format!("encode_packet: {e}"));
            }
        };

        // 7c. Resolve inviter's Reticulum destination(s) and send.
        let inviter_addr = payload.admin_addr;
        let destinations = resolve_destinations_for_owner(crdt_state.as_ref(), inviter_addr).await;
        if destinations.is_empty() {
            // No known device for inviter → drop oneshot + tear down.
            let _ = community_registry
                .take_pending_redemption(&minted.bootstrap_join.id)
                .await;
            if let Err(stop_err) = community_registry
                .shutdown_engine_and_cleanup_persistence(&minted.community_id)
                .await
            {
                tracing::warn!(
                    error = %stop_err,
                    community_id = %hex::encode(minted.community_id.0),
                    "shutdown failed during redeem_invite inviter-unknown rollback"
                );
            }
            return Err(format!(
                "no known device for inviter {} — invite cannot route",
                hex::encode(inviter_addr.0)
            ));
        }
        // Per-destination fan-out with at-least-one-success semantics.
        //
        // The inviter may have multiple devices (any of which can
        // counter-sign). Reticulum unicast is best-effort per
        // destination — if even one queue-side `try_send` succeeds the
        // packet is on its way and we cannot retract it, so a partial
        // failure followed by local rollback would leave the receiver
        // counter-signing while we tear down the engine here. Track
        // success across the loop and ONLY roll back when all
        // destinations failed.
        let mut any_sent = false;
        let mut last_err: Option<String> = None;
        for destination_hash in &destinations {
            match unicast_send_tx.try_send(crate::dm_outbox::UnicastSendRequest {
                destination_hash: *destination_hash,
                packet: wire.clone(),
            }) {
                Ok(()) => any_sent = true,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        destination_hash = %hex::encode(destination_hash),
                        "redeem_invite unicast try_send failed for destination — \
                         continuing fan-out"
                    );
                    last_err = Some(e.to_string());
                }
            }
        }
        if !any_sent {
            let _ = community_registry
                .take_pending_redemption(&minted.bootstrap_join.id)
                .await;
            if let Err(stop_err) = community_registry
                .shutdown_engine_and_cleanup_persistence(&minted.community_id)
                .await
            {
                tracing::warn!(
                    error = %stop_err,
                    community_id = %hex::encode(minted.community_id.0),
                    "shutdown failed during redeem_invite unicast-send rollback"
                );
            }
            return Err(format!(
                "unicast_send_tx try_send failed for all {} destination(s){}",
                destinations.len(),
                last_err
                    .as_deref()
                    .map(|s| format!(" (last error: {s})"))
                    .unwrap_or_default()
            ));
        }

        // 7d. Await oneshot ≤ T (env-overridable for tests).
        let timeout_ms: u64 = std::env::var("HARMONY_REDEEM_INVITE_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(15_000);

        match tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), notify_rx).await {
            Ok(Ok(())) => {
                // Counter-signed Join landed — proceed to commit.
            }
            Ok(Err(_recv_err)) => {
                // Sender dropped without sending — should be
                // unreachable with the current pending_redemptions
                // shape, but treat defensively as a failure.
                if let Err(stop_err) = community_registry
                    .shutdown_engine_and_cleanup_persistence(&minted.community_id)
                    .await
                {
                    tracing::warn!(
                        error = %stop_err,
                        community_id = %hex::encode(minted.community_id.0),
                        "shutdown failed during redeem_invite oneshot-recv-err rollback"
                    );
                }
                return Err("invite-only redemption oneshot closed unexpectedly".into());
            }
            Err(_elapsed) => {
                let _ = community_registry
                    .take_pending_redemption(&minted.bootstrap_join.id)
                    .await;
                if let Err(stop_err) = community_registry
                    .shutdown_engine_and_cleanup_persistence(&minted.community_id)
                    .await
                {
                    tracing::warn!(
                        error = %stop_err,
                        community_id = %hex::encode(minted.community_id.0),
                        "shutdown failed during redeem_invite timeout rollback"
                    );
                }
                return Err(format!(
                    "invite-only redemption timed out after {}ms",
                    timeout_ms
                ));
            }
        }
    }

    // 8. SNAPSHOT-THEN-COMMIT FENCE — production wrapper re-locks
    //    the std `NodeState` mutex and compares `generation`. Tests
    //    pass `|| Ok(())`. If the node was stopped (or stop+restart
    //    raced our await chain), this returns Err with crdt_state
    //    still untouched.
    if let Err(fence_err) = fence_check() {
        if let Err(stop_err) = community_registry
            .shutdown_engine_and_cleanup_persistence(&minted.community_id)
            .await
        {
            tracing::warn!(
                error = %stop_err,
                community_id = %hex::encode(minted.community_id.0),
                "shutdown failed during redeem_invite fence-check rollback"
            );
        }
        return Err(fence_err);
    }

    // 9. COMMIT owner-state Space (LAST step — ZEB-258 reorder).
    {
        let mut state_g = crdt_state.lock().await;
        let outcome = state_g.apply_space_with_canonicalization(minted.space.clone());
        if matches!(outcome, crate::owner_state_crdt::ApplyOutcome::Rejected(_)) {
            // Drop the tokio guard before awaiting the registry call
            // (no `.await` while holding a tokio mutex guard).
            drop(state_g);
            if let Err(stop_err) = community_registry
                .shutdown_engine_and_cleanup_persistence(&minted.community_id)
                .await
            {
                tracing::warn!(
                    error = %stop_err,
                    community_id = %hex::encode(minted.community_id.0),
                    "shutdown failed during redeem_invite apply-rejected rollback"
                );
            }
            return Err(format!(
                "apply_space rejected redemption Space: {outcome:?}"
            ));
        }
    }

    // 10. Return Ok.
    Ok(hex::encode(minted.community_id.0))
}

/// Resolve `OwnerAddr` → `Vec<destination_hash>` via the joiner's
/// `OwnerDeviceCache`. Mirrors `dm_outbox::resolve_destinations`'s
/// shape; reproduced inline because the inviter-resolution path is
/// community-specific (the inviter's OwnerAddr is plumbed straight
/// from the invite payload, not from a Space row's recipient).
///
/// Returns an empty Vec when the cache has no entry for `owner` — the
/// invite-only branch interprets that as "no known device → invite
/// cannot route" and surfaces a deterministic Err.
///
/// Returns *DM destination hashes* (per `dm_signing::compute_dm_destination_hash`),
/// NOT raw `DeviceIdentityHash` bytes. `UnicastSendRequest.destination_hash`
/// is the Reticulum-layer destination keyed off the DM Destination's
/// app/aspect derivation; the dm_outbox drain path computes this same
/// derivation (`dm_outbox.rs::resolve_destinations` and the per-device
/// path) before enqueueing. Returning raw `h.0` here would route invite-
/// only packets to the wrong link-layer address.
async fn resolve_destinations_for_owner(
    crdt_state: &tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>,
    owner: crate::owner_state_types::OwnerAddr,
) -> Vec<[u8; 16]> {
    let g = crdt_state.lock().await;
    g.owner_device_cache
        .devices
        .get(&owner)
        .map(|entry| {
            entry
                .devices
                .iter()
                .map(|h| crate::dm_signing::compute_dm_destination_hash(h.0))
                .collect()
        })
        .unwrap_or_default()
}

/// Tauri IPC: redeem a community invite URL (open or invite-only).
///
/// Snapshots the relevant `NodeState` handles under the std lock, then
/// delegates to `redeem_invite_inner`, which encodes the ZEB-258
/// reorder (owner-state Space commit is the LAST step; engine + adapter
/// + invite-only oneshot failures roll back with crdt_state untouched).
///
/// Lock-order discipline (mirrors `create_community`): the std
/// `state_lock` guard MUST drop before any `.await`. The signing key
/// lives inside `dm_outbox` under a tokio Mutex, so we acquire the
/// dm_outbox handle under the std lock (Arc clone) and `.await` its
/// lock afterward.
///
/// Adapter wiring flows through `event_loop` per spec §"Architecture /
/// Adapter wiring": this command sends a `CommunityAdapterRequest` over
/// an mpsc; the event loop's `select!` drains it and calls
/// `spawn_community_state_zenoh_adapter` against the live session.
///
/// Note vs `create_community`: `spawn_engine` takes `payload.admin_addr`
/// (the original community admin from the invite), NOT `self_owner` —
/// the engine's authority root is the creator's identity, not the
/// joining peer's.
///
/// **NOT idempotent** for OPEN payloads. A second call with the same
/// invite mints a fresh self-Join event with a new (random) event_id,
/// which the CRDT accepts as a distinct event. Materialized state is
/// unchanged (LWW on `MemberState`); the event log grows by one every
/// retry. `registry.spawn_engine` IS idempotent. For invite-only the
/// behavior depends on whether the inviter counter-signs again on a
/// retry (a freshly-minted bootstrap_join.id forces a new countersig
/// dance).
#[tauri::command]
async fn redeem_invite(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    url: String,
) -> Result<String, String> {
    // Snapshot NodeState handles in a single guard scope, then drop
    // the std lock BEFORE any `.await`. Mirrors `create_community`'s
    // pattern.
    let (
        crdt_state,
        hlc_tracker,
        device_id,
        self_owner,
        community_registry,
        community_adapter_tx,
        unicast_send_tx,
        dm_outbox,
        snapshot_generation,
    ) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.crdt_state
                .clone()
                .ok_or("crdt_state missing — node not running?")?,
            g.hlc_tracker.clone().ok_or("hlc_tracker missing")?,
            g.dm_device_id.clone().ok_or("dm_device_id missing")?,
            g.dm_self_owner.ok_or("dm_self_owner missing")?,
            g.community_registry
                .clone()
                .ok_or("community_registry missing — node not running?")?,
            g.community_adapter_request_tx
                .clone()
                .ok_or("community_adapter_request_tx missing")?,
            g.unicast_send_tx
                .clone()
                .ok_or("unicast_send_tx missing — no owner identity?")?,
            g.dm_outbox
                .clone()
                .ok_or("dm_outbox missing — no owner identity?")?,
            g.generation,
        )
    }; // std lock dropped here.

    // Now safe to `.await` — the std lock has been released.
    let signing_key = {
        let outbox_g = dm_outbox.lock().await;
        std::sync::Arc::clone(&outbox_g.signing_key)
    };

    // Fence-check closure: re-locks the std NodeState mutex and
    // compares `generation`. If the node was stopped (or stop+restart
    // raced), the closure returns Err and the inner helper rolls back.
    // Captures `state_lock` (a clonable `tauri::State<'_, _>`) by move;
    // `Fn` (not FnOnce) so the inner helper retains the option of
    // re-checking on retries. The closure borrows `state_lock` through
    // its `'r` lifetime, which the awaited inner future is bounded by.
    let fence_check = {
        let state_lock = state_lock.clone();
        move || -> Result<(), String> {
            let g = state_lock
                .lock()
                .map_err(|e| format!("NodeState poisoned: {e}"))?;
            if g.generation != snapshot_generation {
                return Err(format!(
                    "node generation changed during redeem_invite (was {}, now {}); \
                     redemption minted on a detached crdt_state and won't be persisted — \
                     engine spawn suppressed",
                    snapshot_generation, g.generation
                ));
            }
            if g.community_registry.is_none() {
                return Err(
                    "community_registry was torn down during redeem_invite — engine spawn \
                     suppressed"
                        .to_string(),
                );
            }
            Ok(())
        }
    };

    redeem_invite_inner(
        url,
        crdt_state,
        hlc_tracker,
        device_id,
        self_owner,
        signing_key,
        community_registry,
        community_adapter_tx,
        unicast_send_tx,
        dm_outbox,
        fence_check,
    )
    .await
}

#[cfg(test)]
mod redeem_invite_inner_tests {
    use super::*;
    use crate::community_invite::CommunityInvitePayload;
    use crate::owner_state_types::{Hlc, MembershipKey, OwnerAddr, SpaceId};
    use harmony_identity::PrivateIdentity;

    #[test]
    fn mint_redemption_produces_self_join_and_matching_space() {
        let identity = PrivateIdentity::from_seed(&[0xab; 32]);
        let identity_pub = identity.identity.to_public_bytes();
        let self_owner = OwnerAddr(identity.identity.address_hash);
        // Mirror Task 9's test pattern: pull the canonical 32-byte
        // Ed25519 seed from bytes 32..64 of `to_private_bytes()`. The
        // production IPC borrows this same SigningKey from `dm_outbox`.
        let sk_bytes_full = identity.to_private_bytes();
        let ed_seed: [u8; 32] = sk_bytes_full[32..64]
            .try_into()
            .expect("ed25519 seed slice 32..64");
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&ed_seed);

        let payload = CommunityInvitePayload {
            community_id: SpaceId([0xee; 16]),
            membership_key: MembershipKey::new([0x77; 32]),
            admin_addr: OwnerAddr([0x33; 16]),
            community_name: "TestCom".into(),
            is_invite_only: false,
            expires_at: None,
            invite_token: None,
        };

        let device_id = "joiner-dev";
        let wall_now_ms = 1_700_000_999_000u64;
        let prev_hlc: Option<Hlc> = None;

        let minted = mint_redemption(
            &payload,
            self_owner,
            &signing_key,
            device_id,
            wall_now_ms,
            prev_hlc.as_ref(),
        )
        .expect("mint");

        assert_eq!(minted.community_id, payload.community_id);
        assert_eq!(minted.space.id, payload.community_id);
        assert_eq!(minted.space.admin_addr, Some(payload.admin_addr));
        assert_eq!(minted.space.is_invite_only, Some(false));
        assert_eq!(minted.bootstrap_join.actor, self_owner);
        assert_eq!(minted.bootstrap_join.community_id, payload.community_id);
        assert!(matches!(
            minted.bootstrap_join.kind,
            crate::community_membership::MembershipEventKind::Join
        ));

        // Self-join sig must verify against the joiner's identity_pub —
        // the engine's verify_event runs the same check on insert.
        crate::community_membership::verify_signature(&minted.bootstrap_join, &identity_pub)
            .expect("self-join signature must verify against joiner identity_pub");
    }
}

// ── ZEB-217 Sub-C Phase 3 Task 11: leave_community ───────────────────
//
// Mints a self-Leave SignedMembershipEvent and inserts it into the
// per-community engine. Does NOT mutate owner-state Space (per spec
// line 514): the Space row stays around with its existing fields so
// the user can see "you've left this community" in the UI and choose
// to remove it later via the existing `remove_space` IPC. The Leave
// event in the CRDT is what peers see + what the materialized member
// list reflects.
//
// Engine lifecycle: Phase 3 does NOT call registry.stop_engine on
// leave. The Leave event must publish to peers and the engine's
// debounced publish loop owns that — stopping immediately could race
// the publish. The user's eventual `remove_space` (or a future
// `forget_community` IPC) would call `stop_engine`; for Phase 3 the
// engine stays running.

/// Pure function: mint a self-Leave `SignedMembershipEvent` for a
/// community we currently belong to. Mirrors the
/// `mint_redemption` / `mint_community_creation` shape — pure / sync /
/// no I/O so the canonical-CBOR / signing path is unit-testable
/// without standing up a Tauri test harness.
pub fn mint_leave_event(
    community_id: crate::owner_state_types::SpaceId,
    self_owner: crate::owner_state_types::OwnerAddr,
    signing_key: &ed25519_dalek::SigningKey,
    device_id: &str,
    wall_now_ms: u64,
    prev_hlc: Option<&crate::owner_state_types::Hlc>,
) -> Result<crate::community_membership::SignedMembershipEvent, String> {
    use crate::community_membership::{sign_event, EventPayload, MembershipEventKind};
    use rand::RngCore;

    let mut rng = rand::thread_rng();
    let mut event_id_bytes = [0u8; 16];
    rng.fill_bytes(&mut event_id_bytes);

    let leave_hlc = crate::dm_outbox::next_hlc(prev_hlc, wall_now_ms, device_id);
    let payload = EventPayload {
        id: event_id_bytes,
        community_id,
        kind: MembershipEventKind::Leave,
        actor: self_owner,
        at: leave_hlc,
    };
    sign_event(&payload, signing_key).map_err(|e| format!("sign leave: {e}"))
}

/// Tauri IPC: leave a community we currently belong to.
///
/// Mints a self-Leave event, looks up the per-community engine via
/// `community_registry.engine_arc`, and inserts the event through
/// `engine.insert_local_event` so the debounced publish loop pushes
/// it to peers. Advances the local HLC tracker on success.
///
/// Owner-state Space NOT mutated (per spec line 514): the Space row
/// stays around so the UI can show "you've left this community" and
/// the user can choose to call `remove_space` later. The membership
/// CRDT is the source of truth for community membership; `Space.left_at`
/// is only meaningful for DM Spaces (which have no membership CRDT).
///
/// Snapshot-then-spawn-equivalent fence: after minting but before
/// engine ops we re-acquire the std `NodeState` lock and check
/// `generation`. If the node was stopped (or stop+restart raced), we
/// return Err so the Leave doesn't land on a soon-to-be-detached
/// engine.
///
/// Engine lookup: `engine_arc` returns `None` if no engine is running
/// for this community — surfaced as "not currently joined". We do NOT
/// spawn an engine here (unlike `redeem_invite`); leave operates only
/// on existing live engines.
///
/// HLC tracker advanced AFTER successful insert so a verify failure
/// doesn't bump the tracker into "future" state that would cause the
/// next outgoing event to skip a tick.
#[tauri::command]
async fn leave_community(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
) -> Result<(), String> {
    let id_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "community_id must be 16 bytes (32 hex chars)".to_string())?;
    let space_id = crate::owner_state_types::SpaceId(id_bytes);

    let (hlc_tracker, device_id, self_owner, community_registry, dm_outbox, snapshot_generation) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.hlc_tracker.clone().ok_or("hlc_tracker missing")?,
            g.dm_device_id.clone().ok_or("dm_device_id missing")?,
            g.dm_self_owner.ok_or("dm_self_owner missing")?,
            g.community_registry
                .clone()
                .ok_or("community_registry missing — node not running?")?,
            g.dm_outbox
                .clone()
                .ok_or("dm_outbox missing — no owner identity?")?,
            g.generation,
        )
    };

    let wall_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    // Mint the self-Leave. Read prev_hlc under the tracker lock then
    // drop the guard before signing (sign is sync; releasing eagerly
    // keeps the tracker available to other tasks). Borrow the
    // SigningKey from `dm_outbox` — same canonical local-device key
    // create_community / redeem_invite use.
    let leave = {
        let prev_hlc = {
            let t = hlc_tracker.lock().await;
            t.get(&device_id).cloned()
        };
        let outbox_g = dm_outbox.lock().await;
        let signing_key = outbox_g.signing_key.as_ref();
        mint_leave_event(
            space_id,
            self_owner,
            signing_key,
            &device_id,
            wall_now_ms,
            prev_hlc.as_ref(),
        )?
    };

    // Snapshot-then-spawn-equivalent fence: ensure node generation
    // hasn't changed. If it has, the engine we'd touch would be
    // detached from a stopped node and the Leave wouldn't be
    // persisted — surface Err rather than silently writing into a
    // doomed engine.
    {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        if g.generation != snapshot_generation {
            return Err(format!(
                "node generation changed during leave_community (was {}, now {})",
                snapshot_generation, g.generation
            ));
        }
    }

    let engine_arc = community_registry
        .engine_arc(&space_id)
        .await
        .ok_or_else(|| {
            format!(
                "no engine for community {} — not currently joined",
                hex::encode(space_id.0)
            )
        })?;
    let outcome = engine_arc
        .insert_local_event(leave.clone())
        .await
        .map_err(|e| format!("engine.insert_local_event: {e}"))?;
    if matches!(
        outcome,
        crate::community_state_crdt::InsertOutcome::Rejected(_)
    ) {
        return Err(format!("Leave rejected by CRDT verify: {outcome:?}"));
    }

    // Advance HLC tracker only on `Inserted`. `AlreadyKnown` is benign
    // (the event we minted matches one the engine already had, so the
    // tracker is at-or-past `leave.at`), but advancing on it would
    // diverge from the principle the rest of the IPCs follow:
    // "advance HLC AFTER successful insert so failures don't bump
    // tracker". Cursor Bugbot LOW finding on PR #87 round 2.
    if matches!(
        outcome,
        crate::community_state_crdt::InsertOutcome::Inserted
    ) {
        let mut t = hlc_tracker.lock().await;
        t.insert(device_id.clone(), leave.at.clone());
    }

    Ok(())
}

#[cfg(test)]
mod leave_community_inner_tests {
    use super::*;
    use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};
    use harmony_identity::PrivateIdentity;

    #[test]
    fn mint_leave_produces_self_leave_event() {
        let identity = PrivateIdentity::from_seed(&[0xab; 32]);
        let identity_pub = identity.identity.to_public_bytes();
        let self_owner = OwnerAddr(identity.identity.address_hash);
        // Mirror Task 9/10's test pattern: pull the canonical 32-byte
        // Ed25519 seed from bytes 32..64 of `to_private_bytes()`. The
        // production IPC borrows this same SigningKey from `dm_outbox`.
        let sk_bytes_full = identity.to_private_bytes();
        let ed_seed: [u8; 32] = sk_bytes_full[32..64]
            .try_into()
            .expect("ed25519 seed slice 32..64");
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&ed_seed);

        let community_id = SpaceId([0x77; 16]);
        let device_id = "leaver-dev";
        let prev_hlc: Option<Hlc> = None;
        let wall_now_ms = 1_700_000_500_000u64;

        let event = mint_leave_event(
            community_id,
            self_owner,
            &signing_key,
            device_id,
            wall_now_ms,
            prev_hlc.as_ref(),
        )
        .expect("mint");

        assert_eq!(event.actor, self_owner);
        assert_eq!(event.community_id, community_id);
        assert!(matches!(
            event.kind,
            crate::community_membership::MembershipEventKind::Leave
        ));
        assert_eq!(event.at.wall_ms, wall_now_ms);

        // Self-Leave sig must verify against the leaver's identity_pub —
        // the engine's verify_event runs the same check on insert.
        crate::community_membership::verify_signature(&event, &identity_pub)
            .expect("self-leave signature must verify against leaver identity_pub");
    }
}

// ── ZEB-262 Phase 4: kick_from_community ─────────────────────────────
//
// Mints a Kick SignedMembershipEvent and inserts it through the
// per-community engine. Power-gate enforcement happens INSIDE
// engine.insert_local_event (which calls verify_event) — actor must
// have power ≥ kick_threshold (50) AND strictly greater than target's
// power. The IPC trusts verify_event and translates VerifyError
// discriminants to user-readable strings. Pre-validating here would
// duplicate the rules and risk drift.

/// Pure function: mint a self-signed Kick event for a community we
/// belong to and have permission to moderate. Mirrors `mint_leave_event`.
#[allow(clippy::too_many_arguments)]
pub fn mint_kick_event(
    community_id: crate::owner_state_types::SpaceId,
    self_owner: crate::owner_state_types::OwnerAddr,
    target: crate::owner_state_types::OwnerAddr,
    reason: Option<String>,
    signing_key: &ed25519_dalek::SigningKey,
    device_id: &str,
    wall_now_ms: u64,
    prev_hlc: Option<&crate::owner_state_types::Hlc>,
) -> Result<crate::community_membership::SignedMembershipEvent, String> {
    use crate::community_membership::{sign_event, EventPayload, MembershipEventKind};
    use rand::RngCore;

    let mut rng = rand::thread_rng();
    let mut event_id_bytes = [0u8; 16];
    rng.fill_bytes(&mut event_id_bytes);

    let kick_hlc = crate::dm_outbox::next_hlc(prev_hlc, wall_now_ms, device_id);
    let payload = EventPayload {
        id: event_id_bytes,
        community_id,
        kind: MembershipEventKind::Kick { target, reason },
        actor: self_owner,
        at: kick_hlc,
    };
    sign_event(&payload, signing_key).map_err(|e| format!("sign kick: {e}"))
}

/// Tauri IPC: kick a member from a community.
///
/// Power-gated by `verify_event`: actor must have power ≥ 50 (kick
/// threshold) AND strictly greater than target's current power.
/// Returns Err with the VerifyError discriminant on rejection.
#[tauri::command]
async fn kick_from_community(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
    target_addr: String,
    reason: Option<String>,
) -> Result<(), String> {
    let id_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "community_id must be 16 bytes (32 hex chars)".to_string())?;
    let space_id = crate::owner_state_types::SpaceId(id_bytes);

    let target_bytes: [u8; 16] = hex::decode(&target_addr)
        .map_err(|e| format!("invalid target_addr hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "target_addr must be 16 bytes (32 hex chars)".to_string())?;
    let target = crate::owner_state_types::OwnerAddr(target_bytes);

    let (hlc_tracker, device_id, self_owner, community_registry, dm_outbox, snapshot_generation) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.hlc_tracker.clone().ok_or("hlc_tracker missing")?,
            g.dm_device_id.clone().ok_or("dm_device_id missing")?,
            g.dm_self_owner.ok_or("dm_self_owner missing")?,
            g.community_registry
                .clone()
                .ok_or("community_registry missing — node not running?")?,
            g.dm_outbox
                .clone()
                .ok_or("dm_outbox missing — no owner identity?")?,
            g.generation,
        )
    };

    let wall_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    // Mint under HLC tracker lock then drop the guard.
    let kick = {
        let prev_hlc = {
            let t = hlc_tracker.lock().await;
            t.get(&device_id).cloned()
        };
        let outbox_g = dm_outbox.lock().await;
        let signing_key = outbox_g.signing_key.as_ref();
        mint_kick_event(
            space_id,
            self_owner,
            target,
            reason,
            signing_key,
            &device_id,
            wall_now_ms,
            prev_hlc.as_ref(),
        )?
    };

    // Generation + registry fence (mirrors leave_community + the
    // create_community / redeem_invite shape). Plain generation check
    // is insufficient: stop_node nullifies `community_registry` to
    // None without bumping generation, so without the registry-presence
    // check we'd happily insert into a detached engine that's about to
    // be torn down.
    {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        if g.generation != snapshot_generation {
            return Err(format!(
                "node generation changed during kick_from_community (was {}, now {})",
                snapshot_generation, g.generation
            ));
        }
        if g.community_registry.is_none() {
            return Err(
                "community_registry detached during kick_from_community (node stopped?)"
                    .to_string(),
            );
        }
    }

    let engine_arc = community_registry
        .engine_arc(&space_id)
        .await
        .ok_or_else(|| {
            format!(
                "no engine for community {} — not currently joined",
                hex::encode(space_id.0)
            )
        })?;
    let outcome = engine_arc
        .insert_local_event(kick.clone())
        .await
        .map_err(|e| format!("engine.insert_local_event: {e}"))?;
    if matches!(
        outcome,
        crate::community_state_crdt::InsertOutcome::Rejected(_)
    ) {
        return Err(format!("Kick rejected by CRDT verify: {outcome:?}"));
    }

    // Advance HLC tracker only on `Inserted` (mirrors leave_community).
    if matches!(
        outcome,
        crate::community_state_crdt::InsertOutcome::Inserted
    ) {
        let mut t = hlc_tracker.lock().await;
        t.insert(device_id.clone(), kick.at.clone());
    }

    Ok(())
}

// ── ZEB-262 Phase 4: set_power_level ─────────────────────────────────
//
// Same shape as kick_from_community. Power-gate enforcement in
// verify_event: actor must have power ≥ set_power_threshold (100), and
// the proposed level must be in [0, POWER_THRESHOLDS.max]. Admin self-
// demote is allowed (foot-gun, but consistent with the CRDT semantics);
// any UI warning lives in Phase 5.

#[allow(clippy::too_many_arguments)]
pub fn mint_set_power_event(
    community_id: crate::owner_state_types::SpaceId,
    self_owner: crate::owner_state_types::OwnerAddr,
    target: crate::owner_state_types::OwnerAddr,
    level: u8,
    signing_key: &ed25519_dalek::SigningKey,
    device_id: &str,
    wall_now_ms: u64,
    prev_hlc: Option<&crate::owner_state_types::Hlc>,
) -> Result<crate::community_membership::SignedMembershipEvent, String> {
    use crate::community_membership::{sign_event, EventPayload, MembershipEventKind};
    use rand::RngCore;

    let mut rng = rand::thread_rng();
    let mut event_id_bytes = [0u8; 16];
    rng.fill_bytes(&mut event_id_bytes);

    let hlc = crate::dm_outbox::next_hlc(prev_hlc, wall_now_ms, device_id);
    let payload = EventPayload {
        id: event_id_bytes,
        community_id,
        kind: MembershipEventKind::SetPower { target, level },
        actor: self_owner,
        at: hlc,
    };
    sign_event(&payload, signing_key).map_err(|e| format!("sign set_power: {e}"))
}

/// Tauri IPC: set a member's power level in a community.
///
/// Power-gated by `verify_event`: actor must have power ≥ 100 (the
/// set_power threshold). Out-of-range levels are rejected by
/// verify_event as `PowerLevelOutOfRange`. Returns Err with the
/// VerifyError discriminant on rejection.
#[tauri::command]
async fn set_power_level(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
    target_addr: String,
    level: u8,
) -> Result<(), String> {
    let id_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "community_id must be 16 bytes (32 hex chars)".to_string())?;
    let space_id = crate::owner_state_types::SpaceId(id_bytes);

    let target_bytes: [u8; 16] = hex::decode(&target_addr)
        .map_err(|e| format!("invalid target_addr hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "target_addr must be 16 bytes (32 hex chars)".to_string())?;
    let target = crate::owner_state_types::OwnerAddr(target_bytes);

    let (hlc_tracker, device_id, self_owner, community_registry, dm_outbox, snapshot_generation) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.hlc_tracker.clone().ok_or("hlc_tracker missing")?,
            g.dm_device_id.clone().ok_or("dm_device_id missing")?,
            g.dm_self_owner.ok_or("dm_self_owner missing")?,
            g.community_registry
                .clone()
                .ok_or("community_registry missing — node not running?")?,
            g.dm_outbox
                .clone()
                .ok_or("dm_outbox missing — no owner identity?")?,
            g.generation,
        )
    };

    let wall_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let event = {
        let prev_hlc = {
            let t = hlc_tracker.lock().await;
            t.get(&device_id).cloned()
        };
        let outbox_g = dm_outbox.lock().await;
        let signing_key = outbox_g.signing_key.as_ref();
        mint_set_power_event(
            space_id,
            self_owner,
            target,
            level,
            signing_key,
            &device_id,
            wall_now_ms,
            prev_hlc.as_ref(),
        )?
    };

    // Generation + registry fence (see kick_from_community for
    // motivation; stop_node nullifies registry without bumping
    // generation).
    {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        if g.generation != snapshot_generation {
            return Err(format!(
                "node generation changed during set_power_level (was {}, now {})",
                snapshot_generation, g.generation
            ));
        }
        if g.community_registry.is_none() {
            return Err(
                "community_registry detached during set_power_level (node stopped?)".to_string(),
            );
        }
    }

    let engine_arc = community_registry
        .engine_arc(&space_id)
        .await
        .ok_or_else(|| {
            format!(
                "no engine for community {} — not currently joined",
                hex::encode(space_id.0)
            )
        })?;
    let outcome = engine_arc
        .insert_local_event(event.clone())
        .await
        .map_err(|e| format!("engine.insert_local_event: {e}"))?;
    if matches!(
        outcome,
        crate::community_state_crdt::InsertOutcome::Rejected(_)
    ) {
        return Err(format!("SetPower rejected by CRDT verify: {outcome:?}"));
    }

    if matches!(
        outcome,
        crate::community_state_crdt::InsertOutcome::Inserted
    ) {
        let mut t = hlc_tracker.lock().await;
        t.insert(device_id.clone(), event.at.clone());
    }

    Ok(())
}

/// Delta payload for the `community-members-changed` Tauri event.
/// Matches the spec line 561 wire shape:
/// `{ communityId, changes: [{type, target, by?, detail?}] }`. One
/// IPC event per engine-level CRDT mutation; Phase 3's engine fires
/// one delta at a time so `changes` is always a single-element array
/// in this phase. Future batch-receive optimisations can grow the
/// array without breaking the wire format. Frontend updates
/// incrementally without re-fetching the full member list.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommunityMembersChangedPayload {
    pub community_id: String,
    pub changes: Vec<MembershipChange>,
}

/// One delta in `CommunityMembersChangedPayload.changes`. Flat shape
/// per spec — `type` discriminates the event kind, `target` is the
/// entity whose membership state changed, `by` is the actor when
/// distinct from target (kick/setpower/invite), `detail` carries
/// kind-specific info (kick reason, new power level). `at_wall_ms`
/// is an extension over the spec — useful for the frontend to sort
/// or de-dupe rapid-fire deltas; documented as part of the wire
/// contract here so future consumers don't strip it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MembershipChange {
    #[serde(rename = "type")]
    pub r#type: MembershipChangeType,
    pub target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<MembershipChangeDetail>,
    pub at_wall_ms: u64,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum MembershipChangeType {
    Joined,
    Left,
    Invited,
    Kicked,
    PowerChanged,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "value", rename_all = "camelCase")]
pub enum MembershipChangeDetail {
    Reason(String),
    Level(u8),
}

/// Project a `CommunityMembershipDelta` into `(community_id_hex, change)`.
/// The caller (the start_node consumer task) wraps the change in
/// `CommunityMembersChangedPayload { community_id, changes: vec![change] }`
/// and emits the Tauri event.
///
/// Returns `None` for kinds we can't yet represent (none today; reserved
/// for forward-compat if `MembershipEventKind` grows).
pub fn delta_to_change(
    delta: &crate::community_state_sync::CommunityMembershipDelta,
) -> Option<(String, MembershipChange)> {
    let cid_hex = hex::encode(delta.community_id.0);
    let actor_hex = hex::encode(delta.event.actor.0);
    let at_wall_ms = delta.event.at.wall_ms;
    let change = match &delta.event.kind {
        crate::community_membership::MembershipEventKind::Join => MembershipChange {
            r#type: MembershipChangeType::Joined,
            target: actor_hex,
            by: None,
            detail: None,
            at_wall_ms,
        },
        crate::community_membership::MembershipEventKind::Leave => MembershipChange {
            r#type: MembershipChangeType::Left,
            target: actor_hex,
            by: None,
            detail: None,
            at_wall_ms,
        },
        crate::community_membership::MembershipEventKind::Invite { target } => MembershipChange {
            r#type: MembershipChangeType::Invited,
            target: hex::encode(target.0),
            by: Some(actor_hex),
            detail: None,
            at_wall_ms,
        },
        crate::community_membership::MembershipEventKind::Kick { target, reason } => {
            MembershipChange {
                r#type: MembershipChangeType::Kicked,
                target: hex::encode(target.0),
                by: Some(actor_hex),
                detail: reason.clone().map(MembershipChangeDetail::Reason),
                at_wall_ms,
            }
        }
        crate::community_membership::MembershipEventKind::SetPower { target, level } => {
            MembershipChange {
                r#type: MembershipChangeType::PowerChanged,
                target: hex::encode(target.0),
                by: Some(actor_hex),
                detail: Some(MembershipChangeDetail::Level(*level)),
                at_wall_ms,
            }
        }
    };
    Some((cid_hex, change))
}

/// Drain `delta_rx`, project each delta into `CommunityMembersChangedPayload`,
/// and pass to `emit`. Stops cleanly when the channel closes (last sender
/// dropped — typically on `stop_node`).
///
/// Phase 3 emits one change per IPC event (engine fires one delta per
/// CRDT mutation); the wire format leaves room for batched future
/// deltas without a contract break.
pub async fn run_community_delta_consumer<F, Fut>(
    mut delta_rx: tokio::sync::mpsc::Receiver<
        crate::community_state_sync::CommunityMembershipDelta,
    >,
    mut emit: F,
) where
    F: FnMut(CommunityMembersChangedPayload) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    while let Some(delta) = delta_rx.recv().await {
        if let Some((community_id, change)) = delta_to_change(&delta) {
            let payload = CommunityMembersChangedPayload {
                community_id,
                changes: vec![change],
            };
            emit(payload).await;
        }
    }
}

/// Mirror for `CommunityDegradedReport`. Emits `{ communityId, reason, detail }`
/// — matches the prior inline drain task's wire shape so the frontend
/// banner consumer doesn't break.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommunityStateSyncDegradedPayload {
    pub community_id: String,
    pub reason: String,
    pub detail: String,
}

/// Drain `degraded_rx` and emit each report through `emit`. Stops
/// cleanly when the channel closes (every engine's `error_tx` clone has
/// dropped — happens when `registry.shutdown_all()` finishes).
pub async fn run_community_degraded_consumer<F, Fut>(
    mut degraded_rx: tokio::sync::mpsc::Receiver<
        crate::community_state_sync::CommunityDegradedReport,
    >,
    mut emit: F,
) where
    F: FnMut(CommunityStateSyncDegradedPayload) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    while let Some(report) = degraded_rx.recv().await {
        let payload = CommunityStateSyncDegradedPayload {
            community_id: hex::encode(report.community_id.0),
            reason: report.reason_tag.to_string(),
            detail: report.detail,
        };
        emit(payload).await;
    }
}

// ── App entry point ──────────────────────────────────────────────────────

#[cfg(test)]
mod community_member_dto_tests {
    use super::{member_info_for, MemberStatusDto};
    use crate::community_membership::{MaterializedMembership, MemberState, MemberStatus};
    use crate::owner_state_types::{Hlc, OwnerAddr};
    use std::collections::BTreeMap;

    fn hlc(wall: u64, dev: &str) -> Hlc {
        Hlc {
            wall_ms: wall,
            logical: 0,
            device_id: dev.to_string(),
        }
    }

    #[test]
    fn member_info_sorts_by_power_desc_then_joined_at_asc() {
        let admin = OwnerAddr([1; 16]);
        let mod_user = OwnerAddr([2; 16]);
        let early = OwnerAddr([3; 16]);
        let late = OwnerAddr([4; 16]);

        let mut members = BTreeMap::new();
        members.insert(
            admin,
            MemberState {
                status: MemberStatus::Joined,
                joined_at: hlc(100, "a"),
                left_at: None,
            },
        );
        members.insert(
            mod_user,
            MemberState {
                status: MemberStatus::Joined,
                joined_at: hlc(200, "b"),
                left_at: None,
            },
        );
        members.insert(
            early,
            MemberState {
                status: MemberStatus::Joined,
                joined_at: hlc(150, "c"),
                left_at: None,
            },
        );
        members.insert(
            late,
            MemberState {
                status: MemberStatus::Joined,
                joined_at: hlc(300, "d"),
                left_at: None,
            },
        );

        let mut power_levels = BTreeMap::new();
        power_levels.insert(admin, 100);
        power_levels.insert(mod_user, 50);

        let materialized = MaterializedMembership {
            members,
            power_levels,
        };
        let dto = member_info_for(&materialized);

        assert_eq!(dto.len(), 4);
        assert_eq!(dto[0].addr, hex::encode(admin.0));
        assert_eq!(dto[0].power, 100);
        assert_eq!(dto[1].addr, hex::encode(mod_user.0));
        assert_eq!(dto[1].power, 50);
        assert_eq!(dto[2].addr, hex::encode(early.0));
        assert_eq!(dto[2].power, 0);
        assert_eq!(dto[3].addr, hex::encode(late.0));
        assert_eq!(dto[3].power, 0);
    }

    #[test]
    fn member_info_includes_left_and_banned_members() {
        let a = OwnerAddr([1; 16]);
        let b = OwnerAddr([2; 16]);
        let mut members = BTreeMap::new();
        members.insert(
            a,
            MemberState {
                status: MemberStatus::Left,
                joined_at: hlc(100, "x"),
                left_at: Some(hlc(200, "x")),
            },
        );
        members.insert(
            b,
            MemberState {
                status: MemberStatus::Banned,
                joined_at: hlc(50, "y"),
                left_at: Some(hlc(150, "y")),
            },
        );
        let materialized = MaterializedMembership {
            members,
            power_levels: BTreeMap::new(),
        };
        let dto = member_info_for(&materialized);
        assert_eq!(dto.len(), 2);
        let statuses: Vec<_> = dto.iter().map(|d| d.status).collect();
        assert!(statuses.contains(&MemberStatusDto::Left));
        assert!(statuses.contains(&MemberStatusDto::Banned));
    }
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(Mutex::new(NodeState::default()))
        .invoke_handler(tauri::generate_handler![
            list_vine_videos,
            follow_vine_creator,
            unfollow_vine_creator,
            list_followed,
            mark_vine_viewed,
            publish_vine,
            publish_vine_reaction,
            start_node,
            stop_node,
            connect_zenoh,
            disconnect_zenoh,
            publish_profile,
            send_message,
            send_dm,
            read_dm_thread,
            delete_outbox_entry,
            add_space,
            get_node_addr,
            list_content,
            pin_content,
            unpin_content,
            burn_content,
            archive_content,
            set_replication_tier,
            fetch_content,
            export_content,
            ingest_content,
            create_folder,
            send_voice_frame,
            join_voice_channel,
            leave_voice_channel,
            send_mail,
            list_mail,
            get_mail,
            refresh_mail,
            fetch_mail_body,
            update_mail,
            get_mail_counts,
            identity_commands::current_identity_hash,
            identity_commands::export_mnemonic_words,
            identity_commands::preview_mnemonic_identity,
            identity_commands::preview_recovery_file,
            identity_commands::export_recovery_file_to_path,
            identity_commands::restore_mnemonic_from_words,
            identity_commands::restore_recovery_from_preview_token,
            owner_commands::get_owner_state,
            owner_commands::mint_owner_identity,
            owner_commands::export_owner_recovery_file_to_path,
            owner_commands::issue_owner_recovery_token,
            save_dialog::request_export_save_path,
            pairing_commands::start_inviter_pairing,
            pairing_commands::start_joiner_pairing,
            pairing_commands::select_pairing_peer,
            pairing_commands::confirm_pairing_sas,
            pairing_commands::cancel_pairing,
            pairing_commands::get_pairing_state,
            list_community_members,
            generate_invite,
            create_community,
            redeem_invite,
            leave_community,
            kick_from_community,
            set_power_level,
            #[cfg(debug_assertions)]
            e2e_close_window,
        ])
        .run(tauri::generate_context!())
        .expect("error while running harmony");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_payload(status: u8) -> Vec<u8> {
        let mut p = vec![0xAA; 32];
        p.push(status);
        p
    }

    #[test]
    fn parse_capacity_valid_ready() {
        let result = parse_capacity(
            "harmony/compute/capacity/deadbeef01020304",
            &make_payload(0x01),
        );
        let update = result.unwrap();
        assert_eq!(update.node_addr, "deadbeef01020304");
        assert_eq!(update.model_cid, "aa".repeat(32));
        assert!(update.ready);
    }

    #[test]
    fn parse_capacity_valid_busy() {
        let result = parse_capacity("harmony/compute/capacity/node42", &make_payload(0x00));
        let update = result.unwrap();
        assert_eq!(update.node_addr, "node42");
        assert!(!update.ready);
    }

    #[test]
    fn parse_capacity_truncated() {
        let result = parse_capacity("harmony/compute/capacity/node1", &[0xAA; 10]);
        assert!(result.is_none());
    }

    #[test]
    fn parse_capacity_wrong_prefix() {
        let result = parse_capacity("harmony/telemetry/node1/health", &make_payload(0x01));
        assert!(result.is_none());
    }

    #[test]
    fn parse_capacity_empty_payload() {
        let result = parse_capacity("harmony/compute/capacity/node1", &[]);
        assert!(result.is_none());
    }

    #[test]
    fn profile_payload_roundtrip() {
        let profile = ProfilePayload {
            address: "deadbeef".to_string(),
            display_name: "Alice".to_string(),
            status_text: Some("Building".to_string()),
            avatar_url: None,
            avatar_cid: None,
            avatar_mini_cid: None,
        };
        let json = serde_json::to_vec(&profile).unwrap();
        let parsed: ProfilePayload = serde_json::from_slice(&json).unwrap();
        assert_eq!(parsed.address, "deadbeef");
        assert_eq!(parsed.display_name, "Alice");
        assert_eq!(parsed.status_text.as_deref(), Some("Building"));
        assert!(parsed.avatar_url.is_none());
    }

    #[test]
    fn profile_payload_camel_case() {
        let profile = ProfilePayload {
            address: "aa".to_string(),
            display_name: "Bob".to_string(),
            status_text: None,
            avatar_url: None,
            avatar_cid: None,
            avatar_mini_cid: None,
        };
        let json = String::from_utf8(serde_json::to_vec(&profile).unwrap()).unwrap();
        assert!(
            json.contains("\"displayName\""),
            "expected camelCase: {json}"
        );
        assert!(
            !json.contains("\"display_name\""),
            "unexpected snake_case: {json}"
        );
        assert!(
            !json.contains("statusText"),
            "None field should be skipped: {json}"
        );
    }

    #[test]
    fn parse_telemetry_valid_health() {
        let event = harmony_telemetry::TelemetryEvent {
            node_addr: "abcd1234".to_string(),
            intent: "health".to_string(),
            sequence: 1,
            timestamp: 1711600000,
            payload: serde_json::json!({"cpu_percent": 42.5, "mem_mb": 512}),
            confidence: None,
            source: None,
        };
        let wire = harmony_telemetry::encode_event(&event).unwrap();
        let result = parse_telemetry(&wire);
        let payload = result.unwrap();
        assert_eq!(payload.node_addr, "abcd1234");
        assert_eq!(payload.intent, "health");
        assert_eq!(payload.sequence, 1);
        assert_eq!(payload.timestamp, 1711600000);
    }

    #[test]
    fn parse_telemetry_valid_capacity_changed() {
        let event = harmony_telemetry::TelemetryEvent {
            node_addr: "node42".to_string(),
            intent: "capacity_changed".to_string(),
            sequence: 5,
            timestamp: 1711600100,
            payload: serde_json::json!({"model_cid": "aa".repeat(32), "ready": true}),
            confidence: None,
            source: Some("qwen3-0.6b".to_string()),
        };
        let wire = harmony_telemetry::encode_event(&event).unwrap();
        let result = parse_telemetry(&wire);
        let payload = result.unwrap();
        assert_eq!(payload.intent, "capacity_changed");
        assert_eq!(payload.source, Some("qwen3-0.6b".to_string()));
    }

    #[test]
    fn parse_telemetry_empty_payload() {
        let result = parse_telemetry(&[]);
        assert!(result.is_none());
    }

    #[test]
    fn parse_telemetry_bad_tag() {
        let result = parse_telemetry(&[0xFF, b'{', b'}']);
        assert!(result.is_none());
    }

    #[test]
    fn channel_message_roundtrip() {
        let msg = ChannelMessagePayload {
            id: "msg-abc-123".to_string(),
            sender_address: "deadbeef01020304".to_string(),
            sender_name: "Alice".to_string(),
            channel: "general".to_string(),
            hub: "harmony-dev".to_string(),
            text: "Hello, world!".to_string(),
            timestamp: 1711600000000,
            priority: "standard".to_string(),
            reply_to: None,
        };
        let json = serde_json::to_vec(&msg).unwrap();
        let parsed: ChannelMessagePayload = serde_json::from_slice(&json).unwrap();
        assert_eq!(parsed.id, "msg-abc-123");
        assert_eq!(parsed.sender_address, "deadbeef01020304");
        assert_eq!(parsed.channel, "general");
        assert_eq!(parsed.hub, "harmony-dev");
        assert_eq!(parsed.text, "Hello, world!");
        assert_eq!(parsed.timestamp, 1711600000000);
        assert!(parsed.reply_to.is_none());
    }

    #[test]
    fn channel_message_camel_case() {
        let msg = ChannelMessagePayload {
            id: "msg-1".to_string(),
            sender_address: "aa".to_string(),
            sender_name: "Bob".to_string(),
            channel: "general".to_string(),
            hub: "test".to_string(),
            text: "hi".to_string(),
            timestamp: 0,
            priority: "quiet".to_string(),
            reply_to: Some("msg-0".to_string()),
        };
        let json = String::from_utf8(serde_json::to_vec(&msg).unwrap()).unwrap();
        assert!(
            json.contains("\"senderAddress\""),
            "expected camelCase: {json}"
        );
        assert!(
            json.contains("\"replyTo\""),
            "replyTo should be present: {json}"
        );
        assert!(
            !json.contains("\"sender_address\""),
            "unexpected snake_case: {json}"
        );
    }

    #[test]
    fn send_message_payload_deserialize() {
        let json = r#"{
            "channel": "general",
            "hub": "harmony-dev",
            "text": "test message",
            "priority": "loud",
            "replyTo": "msg-42",
            "senderName": "Alice"
        }"#;
        let parsed: SendMessagePayload = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.channel, "general");
        assert_eq!(parsed.hub, "harmony-dev");
        assert_eq!(parsed.text, "test message");
        assert_eq!(parsed.priority, "loud");
        assert_eq!(parsed.reply_to.as_deref(), Some("msg-42"));
        assert_eq!(parsed.sender_name, "Alice");
    }

    #[test]
    fn send_message_payload_sender_name_defaults() {
        let json = r#"{
            "channel": "general",
            "hub": "test",
            "text": "hi",
            "priority": "standard"
        }"#;
        let parsed: SendMessagePayload = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.sender_name, "", "senderName must default to empty");
        assert!(parsed.reply_to.is_none());
    }

    #[test]
    fn vine_descriptor_roundtrip() {
        let vine = VineDescriptorPayload {
            id: "vine-abc-1234".to_string(),
            creator_address: "deadbeef01020304".to_string(),
            creator_name: "Alice".to_string(),
            created_at: 1711600000,
            video_cid: "aa".repeat(32),
            title: Some("Demo vine".to_string()),
            reshare_of: None,
        };
        let json = serde_json::to_vec(&vine).unwrap();
        let parsed: VineDescriptorPayload = serde_json::from_slice(&json).unwrap();
        assert_eq!(parsed.id, "vine-abc-1234");
        assert_eq!(parsed.creator_address, "deadbeef01020304");
        assert_eq!(parsed.creator_name, "Alice");
        assert_eq!(parsed.created_at, 1711600000);
        assert_eq!(parsed.title.as_deref(), Some("Demo vine"));
        assert!(parsed.reshare_of.is_none());
    }

    #[test]
    fn vine_descriptor_camel_case() {
        let vine = VineDescriptorPayload {
            id: "vine-1".to_string(),
            creator_address: "aa".to_string(),
            creator_name: "Bob".to_string(),
            created_at: 0,
            video_cid: "bb".to_string(),
            title: None,
            reshare_of: Some("vine-0".to_string()),
        };
        let json = String::from_utf8(serde_json::to_vec(&vine).unwrap()).unwrap();
        assert!(
            json.contains("\"creatorAddress\""),
            "expected camelCase: {json}"
        );
        assert!(json.contains("\"videoCid\""), "expected camelCase: {json}");
        assert!(
            json.contains("\"reshareOf\""),
            "reshareOf should be present: {json}"
        );
        assert!(
            !json.contains("\"creator_address\""),
            "unexpected snake_case: {json}"
        );
        assert!(
            !json.contains("\"title\""),
            "None title should be skipped: {json}"
        );
    }

    #[test]
    fn publish_vine_payload_deserialize() {
        let json = r#"{
            "videoCid": "aabbccdd",
            "title": "My vine",
            "creatorName": "Alice"
        }"#;
        let parsed: PublishVinePayload = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.video_cid, "aabbccdd");
        assert_eq!(parsed.title.as_deref(), Some("My vine"));
        assert_eq!(parsed.creator_name, "Alice");
        assert!(parsed.reshare_of.is_none());
    }

    #[test]
    fn publish_vine_payload_creator_name_defaults() {
        let json = r#"{
            "videoCid": "aabb"
        }"#;
        let parsed: PublishVinePayload = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.creator_name, "", "creatorName must default to empty");
        assert!(parsed.title.is_none());
        assert!(parsed.reshare_of.is_none());
    }

    #[test]
    fn vine_reaction_payload_roundtrip() {
        let reaction = VineReactionPayload {
            vine_id: "vine-abc-1234".to_string(),
            reactor_address: "deadbeef01020304".to_string(),
            reactor_name: "Alice".to_string(),
            liked: true,
            timestamp: 1711600000,
        };
        let json = serde_json::to_vec(&reaction).unwrap();
        let parsed: VineReactionPayload = serde_json::from_slice(&json).unwrap();
        assert_eq!(parsed.vine_id, "vine-abc-1234");
        assert_eq!(parsed.reactor_address, "deadbeef01020304");
        assert_eq!(parsed.reactor_name, "Alice");
        assert!(parsed.liked);
        assert_eq!(parsed.timestamp, 1711600000);
    }

    #[test]
    fn vine_reaction_payload_camel_case() {
        let reaction = VineReactionPayload {
            vine_id: "vine-1".to_string(),
            reactor_address: "aa".to_string(),
            reactor_name: "Bob".to_string(),
            liked: false,
            timestamp: 0,
        };
        let json = String::from_utf8(serde_json::to_vec(&reaction).unwrap()).unwrap();
        assert!(json.contains("\"vineId\""), "expected camelCase: {json}");
        assert!(
            json.contains("\"reactorAddress\""),
            "expected camelCase: {json}"
        );
        assert!(
            json.contains("\"reactorName\""),
            "expected camelCase: {json}"
        );
        assert!(
            !json.contains("\"vine_id\""),
            "unexpected snake_case: {json}"
        );
        assert!(
            !json.contains("\"reactor_address\""),
            "unexpected snake_case: {json}"
        );
        assert!(
            !json.contains("\"reactor_name\""),
            "unexpected snake_case: {json}"
        );
    }

    #[test]
    fn publish_reaction_payload_deserialize() {
        let json = r#"{
            "vineId": "vine-abc",
            "vineCreatorAddress": "deadbeef",
            "liked": true,
            "reactorName": "Alice"
        }"#;
        let parsed: PublishReactionPayload = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.vine_id, "vine-abc");
        assert_eq!(parsed.vine_creator_address, "deadbeef");
        assert!(parsed.liked);
        assert_eq!(parsed.reactor_name, "Alice");
    }

    #[test]
    fn publish_reaction_payload_reactor_name_defaults() {
        let json = r#"{
            "vineId": "vine-abc",
            "vineCreatorAddress": "deadbeef",
            "liked": true
        }"#;
        let parsed: PublishReactionPayload = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.reactor_name, "", "reactorName must default to empty");
    }

    #[test]
    fn publish_reaction_payload_liked_false() {
        let json = r#"{
        "vineId": "vine-xyz",
        "vineCreatorAddress": "aabb",
        "liked": false
    }"#;
        let parsed: PublishReactionPayload = serde_json::from_str(json).unwrap();
        assert!(!parsed.liked);
    }

    #[test]
    fn content_announcement_valid() {
        let size: u32 = 65536;
        let payload = size.to_be_bytes().to_vec();
        let result = parse_content_announcement("harmony/announce/aabbccdd11223344", &payload);
        let ann = result.unwrap();
        assert_eq!(ann.cid, "aabbccdd11223344");
        assert_eq!(ann.size_bytes, 65536);
    }

    #[test]
    fn content_announcement_camel_case() {
        let size: u32 = 1024;
        let payload = size.to_be_bytes().to_vec();
        let ann = parse_content_announcement("harmony/announce/abc123", &payload).unwrap();
        let json = serde_json::to_string(&ann).unwrap();
        assert!(json.contains("\"sizeBytes\""), "expected camelCase: {json}");
        assert!(
            !json.contains("\"size_bytes\""),
            "unexpected snake_case: {json}"
        );
    }

    #[test]
    fn content_announcement_wrong_prefix() {
        let payload = 100u32.to_be_bytes().to_vec();
        assert!(parse_content_announcement("harmony/vines/abc", &payload).is_none());
    }

    #[test]
    fn content_announcement_empty_cid() {
        let payload = 100u32.to_be_bytes().to_vec();
        assert!(parse_content_announcement("harmony/announce/", &payload).is_none());
    }

    #[test]
    fn content_announcement_short_payload() {
        assert!(parse_content_announcement("harmony/announce/abc123", &[0, 0]).is_none());
    }

    #[test]
    fn content_announcement_empty_payload() {
        assert!(parse_content_announcement("harmony/announce/abc123", &[]).is_none());
    }

    #[test]
    fn content_announcement_non_hex_cid() {
        let payload = 100u32.to_be_bytes().to_vec();
        assert!(parse_content_announcement("harmony/announce/<script>", &payload).is_none());
        assert!(parse_content_announcement("harmony/announce/xyz!", &payload).is_none());
        assert!(parse_content_announcement("harmony/announce/hello world", &payload).is_none());
    }

    #[test]
    fn list_folder_rejects_non_manifest_child_0() {
        use crate::folders::FolderManifest;

        // A bundle whose child-0 book payload is NOT a folder manifest
        // (e.g., plain UTF-8 "not a manifest" or chunked-file sentinel bytes).
        // Simulated here at the parse level — the full wiring test is the
        // integration test malformed_manifest_returns_error.
        let payload = b"definitely not a manifest";
        let parse_result: Result<FolderManifest, _> = serde_json::from_slice(payload);
        assert!(
            parse_result.is_err(),
            "bad JSON must not parse as FolderManifest"
        );
    }
}

#[cfg(test)]
mod chunked_ingest_tests {
    use super::*;
    use harmony_content::bundle::MAX_BUNDLE_ENTRIES;
    use harmony_content::cid::MAX_PAYLOAD_SIZE;

    #[test]
    fn ingest_dispatch_picks_single_for_small_sizes() {
        assert!(matches!(
            ingest_dispatch(0).unwrap(),
            IngestDispatch::Single
        ));
        assert!(matches!(
            ingest_dispatch(MAX_PAYLOAD_SIZE as u64).unwrap(),
            IngestDispatch::Single
        ));
    }

    #[test]
    fn ingest_dispatch_picks_chunked_above_single_book_ceiling() {
        assert!(matches!(
            ingest_dispatch(MAX_PAYLOAD_SIZE as u64 + 1).unwrap(),
            IngestDispatch::Chunked
        ));
    }

    #[test]
    fn ingest_dispatch_rejects_above_flat_bundle_cap() {
        let too_big = FLAT_BUNDLE_MAX + 1;
        let err = ingest_dispatch(too_big).unwrap_err();
        assert!(err.contains("file too large"), "got: {err}");
        assert!(
            err.contains("flat-bundle"),
            "message should explain the cap origin, got: {err}"
        );
    }

    #[test]
    fn ingest_dispatch_rejects_u64_max() {
        // Guard against accidental reintroduction of a `size as usize`
        // comparison — on 32-bit targets that would wrap and misclassify
        // multi-GiB sizes as Single.
        let err = ingest_dispatch(u64::MAX).unwrap_err();
        assert!(err.contains("file too large"), "got: {err}");
    }

    #[test]
    fn ingest_dispatch_accepts_exactly_flat_bundle_max() {
        // FLAT_BUNDLE_MAX is the last accepted byte count (condition is strict >).
        assert!(matches!(
            ingest_dispatch(FLAT_BUNDLE_MAX).unwrap(),
            IngestDispatch::Chunked
        ));
    }

    #[test]
    fn flat_bundle_max_matches_spec() {
        // Sanity-check the constant so a refactor of the underlying
        // harmony-content limits surfaces here. The cap uses the chunker's
        // min_chunk (not MAX_PAYLOAD_SIZE) so the leaf count can never
        // exceed MAX_BUNDLE_ENTRIES.
        assert_eq!(
            FLAT_BUNDLE_MAX,
            (MAX_BUNDLE_ENTRIES as u64)
                * (harmony_content::chunker::ChunkerConfig::DEFAULT.min_chunk as u64)
        );
    }

    use harmony_content::bundle;
    use harmony_content::cid::{CidType, ContentFlags, ContentId};

    fn synthetic_bytes(len: usize) -> Vec<u8> {
        // Deterministic, non-trivially-compressible content — cycle through
        // a small prime to force the chunker to find real cut points.
        (0..len).map(|i| ((i * 37) % 251) as u8).collect()
    }

    #[test]
    fn chunk_and_bundle_produces_bundle_root_over_leaf_cids() {
        let bytes = synthetic_bytes(3 * 1024 * 1024); // 3 MiB
        let (leaves, bundle_payload, root) =
            chunk_and_bundle(&bytes).expect("chunking must succeed");

        // Bundle root has CidType::Bundle(depth) with depth >= 1.
        match root.cid_type() {
            CidType::Bundle(d) => assert!(d >= 1, "root depth should be >= 1"),
            other => panic!("expected bundle, got {other:?}"),
        }

        // Every leaf is a book CID.
        for (leaf_cid, _data) in &leaves {
            assert_eq!(leaf_cid.cid_type(), CidType::Book, "leaves must be books");
        }

        // The bundle payload parses back to exactly those leaf CIDs in order.
        let parsed = bundle::parse_bundle(&bundle_payload).expect("bundle payload must parse");
        let expected: Vec<ContentId> = leaves.iter().map(|(c, _)| *c).collect();
        assert_eq!(parsed.to_vec(), expected);
    }

    #[test]
    fn chunk_and_bundle_leaf_bytes_sum_to_input() {
        let bytes = synthetic_bytes(3 * 1024 * 1024);
        let (leaves, _bundle_payload, _root) = chunk_and_bundle(&bytes).unwrap();
        let total: usize = leaves.iter().map(|(_, d)| d.len()).sum();
        assert_eq!(
            total,
            bytes.len(),
            "leaves must cover the full input exactly"
        );
        let reassembled: Vec<u8> = leaves.iter().flat_map(|(_, d)| d.iter().copied()).collect();
        assert_eq!(reassembled, bytes, "leaves in order must equal original");
    }

    #[test]
    fn chunk_and_bundle_leaf_cid_matches_for_book_of_its_bytes() {
        let bytes = synthetic_bytes(3 * 1024 * 1024);
        let (leaves, _bundle_payload, _root) = chunk_and_bundle(&bytes).unwrap();
        for (leaf_cid, data) in &leaves {
            let recomputed = ContentId::for_book(data, ContentFlags::default()).unwrap();
            assert_eq!(*leaf_cid, recomputed);
        }
    }

    #[test]
    fn chunk_and_bundle_rejects_single_book_sized_input() {
        // MAX_PAYLOAD_SIZE is the single-book ceiling; chunk_and_bundle
        // must reject inputs that should have gone through the single-book path.
        let bytes = synthetic_bytes(harmony_content::cid::MAX_PAYLOAD_SIZE);
        let err = chunk_and_bundle(&bytes).unwrap_err();
        assert!(err.contains("single-book"), "got: {err}");
    }

    #[test]
    fn chunk_and_bundle_accepts_exactly_max_payload_plus_one() {
        // The smallest valid input: MAX_PAYLOAD_SIZE + 1 bytes.
        let bytes = synthetic_bytes(harmony_content::cid::MAX_PAYLOAD_SIZE + 1);
        chunk_and_bundle(&bytes).expect("must succeed at the minimum valid size");
    }
}

#[cfg(test)]
mod pin_persistence_tests {
    use super::*;

    #[test]
    fn content_item_wire_serializes_sidecar_id_and_kind() {
        let id = uuid::Uuid::new_v4().as_hyphenated().to_string();
        let wire = ContentItemWire {
            sidecar_id: id.clone(),
            cid: "aa".repeat(32),
            name: "Photos".into(),
            size_bytes: 32,
            stored_at: 1,
            sensitivity: "private".into(),
            replication_tier: "default".into(),
            pinned: false,
            licensed: false,
            archived: false,
            kind: "folder".into(),
        };
        let json = serde_json::to_string(&wire).expect("serialize");
        assert!(
            json.contains(&format!("\"sidecarId\":\"{id}\"")),
            "got: {json}"
        );
        assert!(json.contains("\"kind\":\"folder\""), "got: {json}");
    }

    #[test]
    fn parse_sidecar_id_accepts_hyphenated_uuid_rejects_garbage() {
        let id = uuid::Uuid::new_v4().as_hyphenated().to_string();
        assert!(parse_sidecar_id(&id).is_ok());
        assert!(parse_sidecar_id("").is_err(), "empty rejected");
        assert!(parse_sidecar_id("not-a-uuid").is_err(), "garbage rejected");
    }
}

#[cfg(test)]
mod list_community_members_ipc_tests {
    use super::*;
    use crate::community_membership::{
        sign_event_with_identity, EventPayload, MembershipEventKind,
    };
    use crate::community_state_crdt::CommunityState;
    use crate::owner_state_types::*;
    use harmony_identity::PrivateIdentity;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    #[tokio::test]
    async fn list_members_returns_sorted_dto_for_known_community() {
        let community_id = SpaceId([5; 16]);
        let identity = PrivateIdentity::from_seed(&[0xab; 32]);
        let admin = OwnerAddr(identity.identity.address_hash);
        let identity_pub = identity.identity.to_public_bytes();

        let state = Arc::new(Mutex::new(CommunityState::new(community_id)));
        {
            let mut sa = state.lock().await;
            let payload = EventPayload {
                id: [1; 16],
                community_id,
                kind: MembershipEventKind::Join,
                actor: admin,
                at: Hlc {
                    wall_ms: 100,
                    logical: 0,
                    device_id: "x".into(),
                },
            };
            let evt = sign_event_with_identity(&payload, &identity).expect("sign");
            let outcome = sa.insert_event(
                evt,
                &crate::community_membership::VerifyContext {
                    expected_community_id: community_id,
                    admin_addr: admin,
                    is_invite_only: false,
                    actor_identity_pub: &identity_pub,
                    countersigner_identity_pub: None,
                },
            );
            assert!(
                matches!(
                    outcome,
                    crate::community_state_crdt::InsertOutcome::Inserted
                ),
                "fixture insert must succeed; got {outcome:?}"
            );
        }

        let materialized = state.lock().await.materialize_now(admin);
        let dto = member_info_for(&materialized);
        assert_eq!(dto.len(), 1);
        assert_eq!(dto[0].addr, hex::encode(admin.0));
        assert_eq!(dto[0].power, 100);
    }
}

#[cfg(test)]
mod generate_invite_helper_tests {
    use super::*;
    use crate::community_invite::{decode_invite_url, CommunityInvitePayload};
    use crate::owner_state_types::{MembershipKey, OwnerAddr, SpaceId};

    #[test]
    fn build_open_invite_payload_round_trips_via_url() {
        let payload = CommunityInvitePayload {
            community_id: SpaceId([7; 16]),
            membership_key: MembershipKey::new([0x99; 32]),
            admin_addr: OwnerAddr([0x11; 16]),
            community_name: "DoorClub".into(),
            is_invite_only: false,
            expires_at: None,
            invite_token: None,
        };
        let url = build_open_invite_url(&payload).expect("url");
        let decoded = decode_invite_url(&url).expect("decode");
        assert_eq!(decoded, payload);
        assert!(
            decoded.invite_token.is_none(),
            "open path must be token-less"
        );
    }
}

#[cfg(test)]
mod community_delta_projection_tests {
    use super::*;
    use crate::community_membership::{
        sign_event_with_identity, EventPayload, MembershipEventKind,
    };
    use crate::community_state_sync::CommunityMembershipDelta;
    use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};
    use harmony_identity::PrivateIdentity;

    fn make_delta(kind: MembershipEventKind, actor: OwnerAddr) -> CommunityMembershipDelta {
        let identity = PrivateIdentity::from_seed(&[0xee; 32]);
        let community_id = SpaceId([4; 16]);
        let payload = EventPayload {
            id: [0xab; 16],
            community_id,
            kind,
            actor,
            at: Hlc {
                wall_ms: 1234,
                logical: 0,
                device_id: "x".into(),
            },
        };
        let event = sign_event_with_identity(&payload, &identity).expect("sign");
        CommunityMembershipDelta {
            community_id,
            event,
        }
    }

    #[test]
    fn join_projects_with_target_and_no_by() {
        let actor = OwnerAddr([1; 16]);
        let (cid_hex, change) =
            delta_to_change(&make_delta(MembershipEventKind::Join, actor)).expect("Join projects");
        assert_eq!(cid_hex, hex::encode([4u8; 16]));
        assert_eq!(change.r#type, MembershipChangeType::Joined);
        assert_eq!(change.target, hex::encode(actor.0));
        assert!(change.by.is_none(), "Join is self-action; by is None");
        assert!(change.detail.is_none());
        assert_eq!(change.at_wall_ms, 1234);
    }

    #[test]
    fn leave_projects_with_target_and_no_by() {
        let actor = OwnerAddr([2; 16]);
        let (_, change) = delta_to_change(&make_delta(MembershipEventKind::Leave, actor)).unwrap();
        assert_eq!(change.r#type, MembershipChangeType::Left);
        assert_eq!(change.target, hex::encode(actor.0));
        assert!(change.by.is_none());
        assert!(change.detail.is_none());
    }

    #[test]
    fn kick_projects_with_target_by_and_reason_detail() {
        let actor = OwnerAddr([3; 16]);
        let target = OwnerAddr([4; 16]);
        let (_, change) = delta_to_change(&make_delta(
            MembershipEventKind::Kick {
                target,
                reason: Some("spam".into()),
            },
            actor,
        ))
        .unwrap();
        assert_eq!(change.r#type, MembershipChangeType::Kicked);
        assert_eq!(change.target, hex::encode(target.0));
        assert_eq!(change.by.as_deref(), Some(hex::encode(actor.0).as_str()));
        match change.detail.as_ref() {
            Some(MembershipChangeDetail::Reason(s)) => assert_eq!(s, "spam"),
            other => panic!("expected Reason detail, got {other:?}"),
        }
    }

    #[test]
    fn set_power_projects_with_target_by_and_level_detail() {
        let actor = OwnerAddr([5; 16]);
        let target = OwnerAddr([6; 16]);
        let (_, change) = delta_to_change(&make_delta(
            MembershipEventKind::SetPower { target, level: 50 },
            actor,
        ))
        .unwrap();
        assert_eq!(change.r#type, MembershipChangeType::PowerChanged);
        assert_eq!(change.target, hex::encode(target.0));
        assert_eq!(change.by.as_deref(), Some(hex::encode(actor.0).as_str()));
        match change.detail.as_ref() {
            Some(MembershipChangeDetail::Level(n)) => assert_eq!(*n, 50),
            other => panic!("expected Level detail, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod delta_consumer_task_tests {
    use super::*;
    use crate::community_membership::{
        sign_event_with_identity, EventPayload, MembershipEventKind,
    };
    use crate::community_state_sync::CommunityMembershipDelta;
    use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};
    use harmony_identity::PrivateIdentity;

    #[tokio::test]
    async fn consumer_emits_payload_via_handler() {
        let (tx, rx) = tokio::sync::mpsc::channel::<CommunityMembershipDelta>(8);
        let captured: std::sync::Arc<tokio::sync::Mutex<Vec<CommunityMembersChangedPayload>>> =
            std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let captured_for_handler = std::sync::Arc::clone(&captured);

        let handle = tokio::spawn(async move {
            run_community_delta_consumer(rx, move |payload| {
                let captured = std::sync::Arc::clone(&captured_for_handler);
                async move {
                    captured.lock().await.push(payload);
                }
            })
            .await
        });

        let identity = PrivateIdentity::from_seed(&[0xab; 32]);
        let actor = OwnerAddr(identity.identity.address_hash);
        let community_id = SpaceId([6; 16]);
        let payload = EventPayload {
            id: [9; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor,
            at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "x".into(),
            },
        };
        let event = sign_event_with_identity(&payload, &identity).unwrap();
        tx.send(CommunityMembershipDelta {
            community_id,
            event,
        })
        .await
        .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let cap = captured.lock().await;
        assert_eq!(cap.len(), 1);
        assert_eq!(cap[0].community_id, hex::encode(community_id.0));
        assert_eq!(
            cap[0].changes.len(),
            1,
            "Phase 3 emits one change per IPC event"
        );
        assert_eq!(cap[0].changes[0].r#type, MembershipChangeType::Joined);
        assert_eq!(cap[0].changes[0].target, hex::encode(actor.0));
        drop(tx);
        let _ = handle.await;
    }
}
