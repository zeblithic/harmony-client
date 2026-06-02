//! Simplified NodeRuntime event loop for the Tauri desktop client.
//!
//! Adapted from harmony-node's event_loop.rs, stripped down to:
//! - UDP socket (Reticulum mesh broadcast/unicast)
//! - Zenoh session (pub/sub, queryables, content fetch)
//! - 250ms timer tick
//!
//! No disk/archive/S3 persistence, no inference.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use harmony_content::book::MemoryBookStore;
use harmony_runtime::{NodeRuntime, RuntimeAction, RuntimeEvent};
use tauri::{AppHandle, Emitter, Runtime};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot, watch};

/// Well-known UDP port for Reticulum mesh — must match harmony-node default
/// so that all nodes on the LAN broadcast/listen on the same port.
const RETICULUM_UDP_PORT: u16 = 4242;

/// Handles passed from `start_node` (lib.rs) into the event loop so the
/// Zenoh adapter can wire the SyncEngine's mpsc channels to Zenoh pub/sub.
///
/// Constructed in `start_node` after the SyncEngine is built; consumed
/// (via `take()`) inside `event_loop::run` once the Zenoh session is open.
pub struct SyncEngineHandles {
    /// Hex-encoded OWNER identity address (16 bytes) — used to form the
    /// state-root topic key `harmony/owner/{addr_hex}/state-root-v1`.
    /// Every device bound to one owner shares the same topic.
    pub addr_hex: String,
    /// Bytes produced by the SyncEngine for outbound Zenoh puts.
    pub outbound_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    /// Bytes received from Zenoh, forwarded into the SyncEngine.
    pub inbound_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
}

/// Mint sync Zenoh adapter handles. Mirrors `SyncEngineHandles` — constructed
/// in `start_node` alongside `MintSyncEngine::new`, consumed inside
/// `event_loop::run` once the Zenoh session is open.
pub struct MintSyncHandles {
    /// Hex-encoded OWNER identity address — forms the mint topic key
    /// `harmony/owner/{addr_hex}/mint-root-v1`.
    pub addr_hex: String,
    /// Encrypted bytes from MintSyncEngine's publish path → Zenoh put.
    pub outbound_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    /// Encrypted bytes from Zenoh → MintSyncEngine's subscribe path.
    pub inbound_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
}

/// One per-community adapter request handed from `start_node` (lib.rs)
/// into the event loop's Zenoh-session scope.
///
/// `start_node` owns the `CommunitySyncRegistry` and the
/// per-community channel pairs the registry's engines consume; the
/// matching halves (publisher_rx + subscriber_tx) need to be wired
/// to a Zenoh publisher / subscriber on
/// `harmony/community/{id_hex}/state-root-v1`. But the Zenoh
/// `Session` is opened inside `event_loop::run`, not in `start_node`,
/// so `start_node` builds one of these per joined community and
/// passes the `Vec<CommunityAdapterRequest>` into `event_loop::run`.
/// `event_loop::run` iterates the Vec after the session is open and
/// calls `spawn_community_state_zenoh_adapter` for each entry.
///
/// Mirrors the `SyncEngineHandles` cross-boundary pattern used for
/// the owner-state SyncEngine (see above) — same reason (the engine
/// constructor needs the channels' OTHER halves at start_node time,
/// before the session exists), same shape (one struct carrying the
/// halves we need to keep alive until session-open).
pub struct CommunityAdapterRequest {
    /// Hex-encoded community SpaceId (32 chars, lowercase) — used to
    /// form the per-community state-root topic key
    /// `harmony/community/{id_hex}/state-root-v1`.
    pub id_hex: String,
    /// Engine's outbound channel: bytes the engine writes here drain
    /// into Zenoh `put` on the per-community topic.
    pub publisher_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    /// Engine's inbound channel: bytes Zenoh receives on the per-
    /// community topic are forwarded here, where the engine reads
    /// them out via its paired `subscriber_rx`.
    pub subscriber_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
}

/// ZEB-298+ZEB-312 PR 1: per-community voting-log adapter request.
/// Same shape as `CommunityAdapterRequest` (state-root v1) — voting
/// is per-community + pub/sub-only (no queryable, no backfill in PR 1).
pub struct VotingLogAdapterRequest {
    /// Hex-encoded community SpaceId — used to form
    /// `harmony/community/{id_hex}/voting`.
    pub id_hex: String,
    /// Engine outbound → Zenoh `put`.
    pub publisher_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    /// Zenoh subscriber → engine inbound.
    pub subscriber_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
}

/// ZEB-270 Phase 3 Task 4.5: per-channel adapter request handed from
/// `ChannelLogRegistry::spawn` (lib.rs / runtime IPC) into the event
/// loop's Zenoh-session scope.
///
/// Same architectural rationale as `CommunityAdapterRequest`: the
/// channel-log engine is constructed from `start_node` (and from the
/// Phase 3 delta-consumer task), but the Zenoh `Session` lives
/// exclusively inside `event_loop::run`. Carrying the per-channel mpsc
/// halves through this struct lets the registry's `spawn` enqueue an
/// adapter binding without ever touching the session, and the event
/// loop's `select!` arm wires the halves to a Zenoh adapter against the
/// live session by calling `spawn_channel_log_zenoh_adapter`.
pub struct ChannelLogAdapterRequest {
    /// Hex-encoded community SpaceId (32 chars, lowercase) — used to
    /// form the per-channel events topic key
    /// `harmony/channels/{community_id_hex}/{channel_id_hex}/events`.
    pub community_id_hex: String,
    /// Hex-encoded ChannelId (32 chars, lowercase).
    pub channel_id_hex: String,
    /// Engine's outbound channel: bytes the engine writes here drain
    /// into Zenoh `put` on the per-channel events topic.
    pub publisher_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    /// Engine's inbound channel: bytes Zenoh receives on the per-
    /// channel events topic are forwarded here.
    pub subscriber_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    /// Engine's backfill query-request channel — drained by the
    /// adapter's queryable-driver task to issue `session.get` requests
    /// against the per-channel `since/**` queryable prefix.
    pub query_request_rx:
        tokio::sync::mpsc::Receiver<crate::community_channel_log_engine::BackfillQueryRequest>,
    /// Read-side closure invoked by the queryable task on each `since`
    /// query. Closes over an `Arc<ChannelLogEngine>` so the queryable
    /// can map (since, limit) to a vec of encrypted packets without
    /// holding a back-reference to the registry.
    #[allow(clippy::type_complexity)]
    pub read_for_query: Arc<
        dyn Fn(
                Option<crate::owner_state_types::Hlc>,
                usize,
            )
                -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<Vec<u8>>> + Send>>
            + Send
            + Sync
            + 'static,
    >,
    /// Per spec §10 + §8.4: emit `channel-backfill-progress` Tauri
    /// event from the query-request driver task every N events
    /// (`backfill_progress_event_interval`) and once at end. Receives
    /// (`fetched`, `total_estimate`); the adapter doesn't know the AppHandle
    /// directly (the registry constructs the closure with its `app:
    /// AppHandle<R>` captured), so this callback bridges the runtime
    /// type erasure.
    #[allow(clippy::type_complexity)]
    pub emit_backfill_progress: Arc<dyn Fn(u32, Option<u32>) + Send + Sync + 'static>,
    /// Spec §10: how many incoming reply packets to count between
    /// progress emissions. Default 16; tests can override via
    /// `ChannelLogEngineConfig.backfill_progress_event_interval`.
    pub backfill_progress_interval: usize,
    /// Per-engine default backfill limit applied when a
    /// `BackfillQueryRequest` carries `limit == 0`. Sourced from
    /// `ChannelLogEngineConfig.backfill_default_limit` at registry
    /// `spawn` time so per-community config overrides take effect
    /// (the previous shape hardcoded `CHANNEL_BACKFILL_DEFAULT_LIMIT`
    /// in the adapter, ignoring engine config). The hard cap
    /// `CHANNEL_BACKFILL_MAX_LIMIT` still applies on the adapter
    /// side as a server-side reply-storm bound.
    pub backfill_default_limit: usize,
    /// Closing flag for the adapter task. Independent from the
    /// engine's internal closing flag — they're flipped by separate
    /// paths:
    /// - `ChannelLogRegistry::stop` flips this bridge flag (unblocks
    ///   the adapter's pub/sub/qbl/qr task select arms within ~1s
    ///   closing-poll).
    /// - `ChannelLogEngine::shutdown` flips the engine's internal
    ///   flag (unblocks the engine's receive + flush loops).
    ///
    /// Both flips happen on `stop()`, but each bit is owned by its
    /// own teardown path and is freshly allocated at engine-construction
    /// time (see `ChannelLogRegistry::spawn`).
    pub closing: Arc<AtomicBool>,
}

/// A publish request sent from the Tauri command thread into the event loop.
pub struct PublishRequest {
    pub key_expr: String,
    pub payload: Vec<u8>,
    pub reply: oneshot::Sender<Result<(), String>>,
}

/// A content-fetch request sent from the Tauri command thread into the event loop.
pub struct FetchRequest {
    pub cid_hex: String,
    pub reply: oneshot::Sender<Result<Vec<u8>, String>>,
}

/// A content-ingest request: store local file bytes in the runtime's storage tier.
pub struct IngestRequest {
    pub cid_hex: String,
    pub data: Vec<u8>,
    pub reply: oneshot::Sender<Result<(), String>>,
}

/// Content-verb requests sent from Tauri commands into the event loop.
///
/// The event loop mutates the runtime's cache (pin/unpin) and snapshots
/// pinned state in response. Sidecar-only mutations (archive, replication
/// tier) are NOT routed through this channel — they run directly against
/// the `Arc<Mutex<ContentIndex>>` from the Tauri command handler.
pub enum ContentVerbRequest {
    Pin {
        cid: [u8; 32],
        reply: oneshot::Sender<Result<bool, String>>,
    },
    Unpin {
        cid: [u8; 32],
        reply: oneshot::Sender<Result<bool, String>>,
    },
    Burn {
        cid: [u8; 32],
        reply: oneshot::Sender<Result<bool, String>>,
    },
    /// Snapshot the set of currently-pinned CIDs in the runtime cache.
    /// Used by `list_content` to fill the `pinned` field per entry.
    PinnedSet {
        reply: oneshot::Sender<std::collections::HashSet<[u8; 32]>>,
    },
    /// ZEB-158 slice 1: read raw bytes for a CID out of the runtime
    /// cache. Used by `list_content(folder_cid=Some)` in src-tauri/src/lib.rs
    /// to parse a folder bundle's manifest without needing direct access
    /// to the `!Send` NodeRuntime.
    ///
    /// Returns `None` if the CID is not admitted in the cache. Callers
    /// surface "folder not in cache" diagnostics instead of errors so a
    /// legitimately-evicted folder is distinguishable from a malformed
    /// request.
    ReadBytes {
        cid: [u8; 32],
        reply: oneshot::Sender<Option<Vec<u8>>>,
    },
}

/// A follow/unfollow request sent from the Tauri command thread into the event loop.
pub enum FollowRequest {
    Follow { address: String },
    Unfollow { address: String },
}

/// Sub-D Phase 4 (ZEB-281): control messages for the profile-broadcast
/// subscriber pool. Each Subscribe declares a Zenoh subscriber for
/// `harmony/discovery/profile/{peer_addr_hex}/memberships`; Unsubscribe
/// aborts the task and drops the Zenoh subscriber.
///
/// The pool is keyed by `SubscriptionId` (allocated by NodeState via an
/// AtomicU64) — NOT by `OwnerAddr` — because multiple concurrent
/// ProfilePopovers may be open for the same peer.
pub enum ProfileBroadcastRequest {
    Subscribe {
        subscription_id: crate::profile_broadcast::SubscriptionId,
        peer_addr: crate::owner_state_types::OwnerAddr,
    },
    Unsubscribe {
        subscription_id: crate::profile_broadcast::SubscriptionId,
    },
}

/// ZEB-341 profile-card subscriber pool requests. Mirrors
/// `ProfileBroadcastRequest` but carries a raw `owner_id: [u8;16]` (the
/// card topic's owner) rather than an `OwnerAddr`, because the card topic
/// is keyed directly off the 16-byte owner id.
pub enum ProfileCardRequest {
    Subscribe {
        subscription_id: crate::profile_broadcast::SubscriptionId,
        owner_id: [u8; 16],
    },
    Unsubscribe {
        subscription_id: crate::profile_broadcast::SubscriptionId,
    },
}

/// Events bridged from spawned Zenoh tasks back to the main select loop.
enum ZenohEvent {
    Query {
        key_expr: String,
        payload: Vec<u8>,
    },
    ComputeQuery {
        key_expr: String,
        payload: Vec<u8>,
    },
    Subscription {
        key_expr: String,
        payload: Vec<u8>,
        source_zid: Option<String>,
    },
    FetchResponse {
        cid: [u8; 32],
        is_module: bool,
        result: Result<Vec<u8>, String>,
    },
}

/// ZEB-321 Phase 1: bundle of iroh-transport resources constructed at
/// `start_node` time and threaded into the event loop so the accept-loop
/// can be spawned alongside the rest of the long-lived background tasks.
/// The publisher is NOT in this bundle (see following paragraph).
///
/// Construction of the endpoint + link manager lives in `start_node`
/// (rather than inside `event_loop::run`) so the per-community
/// membership-delta consumer closure can capture the resolver and route
/// freshly-inserted `ReachabilityAnnounce` events into it without a
/// separate plumbing pass. The link manager is passed in pre-built so
/// the event loop owns only the `spawn_accept_loop` call.
///
/// ZEB-321 Phase 1 Task 9 update: the `ReachabilityPublisher` is NOT in
/// this bundle. The publisher's real `publish_fn` closure needs to
/// iterate joined communities + sign `ReachabilityAnnounce` events,
/// which requires the `CommunitySyncRegistry` + `PrivateIdentity` — both
/// constructed later in `start_node` than the iroh endpoint. lib.rs owns
/// the publisher's `Arc` + `JoinHandle` so it can build them once
/// registry + identity are ready (see start_node body in lib.rs). We
/// chose this "defer publisher construction" (Option A) over a runtime
/// `replace_publish_callback` swap (Option B) because it avoids interior
/// mutability and keeps the publisher's identity stable across its
/// entire lifetime — there's only ever one publisher per device session,
/// and the few-line delay until registry/identity are ready costs
/// nothing (the publisher's startup-publish would have hit a no-op
/// callback otherwise).
pub struct IrohRuntimeHandles {
    pub endpoint: std::sync::Arc<crate::iroh_endpoint::IrohEndpoint>,
    pub link_manager: std::sync::Arc<crate::zenoh_iroh_transport::IrohZenohLinkManager>,
}

/// Run the NodeRuntime event loop as a background task.
///
/// Sends `Ok(())` on `ready_tx` once UDP + Zenoh + startup actions are
/// all initialized, or `Err(msg)` if any startup step fails.
/// Returns when shutdown signal fires.
#[allow(clippy::too_many_arguments)] // pre-existing; tracked for refactor
pub async fn run<R: Runtime>(
    mut runtime: NodeRuntime<MemoryBookStore>,
    startup_actions: Vec<RuntimeAction>,
    app: AppHandle<R>,
    endpoint: Option<String>,
    ready_tx: oneshot::Sender<Result<(), String>>,
    mut shutdown: watch::Receiver<bool>,
    mut publish_rx: mpsc::Receiver<PublishRequest>,
    mut fetch_rx: mpsc::Receiver<FetchRequest>,
    mut ingest_rx: mpsc::Receiver<IngestRequest>,
    mut content_verb_rx: mpsc::Receiver<ContentVerbRequest>,
    cas_op_tx: mpsc::Sender<crate::content_store::CasOp>,
    mut cas_op_rx: mpsc::Receiver<crate::content_store::CasOp>,
    mut follow_rx: mpsc::Receiver<FollowRequest>,
    mut voice_rx: mpsc::Receiver<crate::voice::VoiceOutbound>,
    mut voice_channel_rx: mpsc::Receiver<crate::voice::VoiceChannelRequest>,
    // ZEB-352: outbound DM-call signaling relay receiver. The select! arm
    // publishes each `VoiceSignalRequest` to
    // `harmony/voice-signal/{callee_owner_hex}`.
    mut voice_signal_rx: mpsc::Receiver<crate::voice_signal::VoiceSignalRequest>,
    followed_set: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    vine_feed_cache: std::sync::Arc<std::sync::Mutex<crate::vine_feed_cache::VineFeedCache>>,
    mail_mgr: std::sync::Arc<std::sync::Mutex<crate::mail::MailManager>>,
    mail_sync: Option<Arc<crate::mail_sync::MailSync<R>>>,
    mut refresh_rx: mpsc::Receiver<crate::mail_sync::RefreshRequest>,
    mut pin_intent: std::collections::HashSet<[u8; 32]>,
    fetch_completion_tx: mpsc::Sender<[u8; 32]>,
    mut fetch_completion_rx: mpsc::Receiver<[u8; 32]>,
    pairing_in_tx: Option<mpsc::Sender<crate::pairing::types::PairingWireMessage>>,
    mut sync_handles: Option<SyncEngineHandles>,
    dm_outbox: Option<std::sync::Arc<tokio::sync::Mutex<crate::dm_outbox::DmOutbox>>>,
    dm_transport: Option<std::sync::Arc<dyn crate::dm_outbox::DmTransport>>,
    crdt_state: Option<std::sync::Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>>,
    // ZEB-227 Path B: outbound DM unicast receiver. None when no owner
    // identity is loaded (mirrors the dm_outbox/dm_transport/crdt_state
    // shape). The select! arm uses std::future::pending() to make the
    // None case effectively skipped without polling overhead. `mut` is
    // required because the arm calls `.as_mut()` on the Option.
    mut unicast_send_rx: Option<mpsc::Receiver<crate::dm_outbox::UnicastSendRequest>>,
    // ZEB-227 Path B Task 11: ContentStore handle for the
    // RuntimeAction::UnicastReceived interception block. handle_cidnotify_lifted
    // does a 500ms-timeout cas.get on the message_cid before
    // decrypt+inbox-write; without this handle the interception block
    // can't service inbound CidNotify packets. None when no owner identity
    // is loaded (same gating as dm_outbox/crdt_state).
    cas_handle: Option<std::sync::Arc<dyn crate::content_store::ContentStore>>,
    // ZEB-227 Path B Task 11: outbound DM unicast SENDER (clone of the
    // tx half of `unicast_send_rx`'s channel). The interception block
    // hands this to `DmOutbox.handle_unicast` so handle_cidnotify_lifted can
    // push DmAck fan-out requests back through the same channel that
    // RuntimeUnicastTransport uses for outbound CidNotify. Same channel,
    // both directions push; event_loop drains via unicast_send_rx for
    // both. None when no owner identity is loaded.
    unicast_send_tx: Option<mpsc::Sender<crate::dm_outbox::UnicastSendRequest>>,
    // ZEB-217 Sub-C Phase 2 Task 13: per-community state-CRDT Zenoh
    // adapter requests. `start_node` scans owner-state for joined
    // communities, spawns one engine per community via
    // `CommunitySyncRegistry`, and passes the matching channel halves
    // through this Vec so we can call
    // `spawn_community_state_zenoh_adapter` once the session is open.
    // Empty Vec when no owner identity is loaded or no communities
    // joined yet — Phase 3 IPC ships `create_community` /
    // `redeem_invite` which spawn additional engines at runtime
    // through the registry directly (those bypass this Vec).
    community_adapters: Vec<CommunityAdapterRequest>,
    // ZEB-217 Sub-C Phase 3 Task 9: on-demand `CommunityAdapterRequest`
    // receiver. The IPC `create_community` (and Phase 4
    // `redeem_invite`) construct a Request from a fresh
    // `spawn_engine` call's matching channel halves and dispatch it
    // here; the select arm below binds those halves to a new Zenoh
    // adapter against the live session. Drained one at a time —
    // Request order between IPC calls is preserved by mpsc, but
    // adapter spawn is fire-and-forget so two requests on the same
    // tick fan out concurrently rather than serializing.
    mut community_adapter_request_rx: mpsc::Receiver<CommunityAdapterRequest>,
    // ZEB-298+ZEB-312 PR 1: on-demand voting-log adapter request receiver.
    // `ensure_voting_engine_for` sends a `VotingLogAdapterRequest` here;
    // the select! arm below drains it and calls
    // `spawn_voting_log_zenoh_adapter` against the live session. Bounded
    // capacity 32 — same as `community_adapter_request_rx` (voting
    // engine creation is always user-triggered via IPC, low burst rate).
    mut voting_log_adapter_request_rx: mpsc::Receiver<VotingLogAdapterRequest>,
    // ZEB-262 Phase 4 Task 9: community sync registry. Threaded into
    // `handle_runtime_action_or_dispatch` so the new
    // `inbound_packet::try_dispatch_community` discriminant pre-fork
    // can route 0x10 community packets to
    // `community_invite::handle_unicast`. `None` until the owner
    // identity is loaded — same gating shape as `dm_outbox` /
    // `crdt_state`. The handler drops the packet (with a warn-log) if
    // the registry isn't set yet.
    community_registry: Option<std::sync::Arc<crate::community_state_sync::CommunitySyncRegistry>>,
    // ZEB-270 Phase 3 Task 4.5: per-channel Zenoh adapter request
    // receiver. `start_node` constructs the `ChannelLogRegistry` with
    // the matching `UnboundedSender` half; the registry's `spawn`
    // enqueues one request per (community, channel) pair. The select
    // arm below drains and binds each request to a Zenoh adapter
    // against the live session. Both boot-time `reconcile_from_state`
    // and runtime `Created` channel-config events flow through this
    // same channel. Unbounded because boot reconcile (which runs
    // BEFORE event_loop drains) may queue more requests than any
    // sensible bound — see `adapter_request_tx` doc on
    // `ChannelLogRegistryConfig` for the full rationale.
    mut channel_log_adapter_request_rx: mpsc::UnboundedReceiver<ChannelLogAdapterRequest>,
    // ZEB-218 Sub-D Phase 1: shared `LibraryDirectory` (aggregation + request
    // channel). `None` is allowed but currently always `Some` when the
    // event loop is started by production `start_node`. The consumer
    // task spawned below pulls `LibraryDirectoryRequest`s and declares
    // per-library subscribers.
    library_directory: Option<Arc<crate::library_directory::LibraryDirectory>>,
    // ZEB-218 Sub-D Phase 1: receiver paired with `LibraryDirectory.request_tx`.
    // Moved into the long-lived consumer task below. `None` when
    // `library_directory` is `None`. Unbounded — see
    // `library_directory::LibraryDirectory` doc; sized for the F1
    // startup-walk deadlock fix.
    library_request_rx: Option<
        mpsc::UnboundedReceiver<crate::library_directory::LibraryDirectoryRequest>,
    >,
    // ZEB-281 Sub-D Phase 4: profile-broadcast peer cache. `None` is
    // allowed but currently always `Some` when the event loop is started
    // by production `start_node`. Shared with the per-subscription
    // Zenoh subscriber tasks spawned by the consumer below.
    profile_broadcast_cache: Option<Arc<crate::profile_broadcast::ProfileBroadcastCache>>,
    // ZEB-281 Sub-D Phase 4: receiver paired with NodeState's
    // `profile_broadcast_request_tx`. IPC handlers send Subscribe /
    // Unsubscribe; the consumer below maintains a per-subscription
    // Zenoh subscriber pool with retry/backoff (matching the Phase 2
    // announce subscriber). `None` when `profile_broadcast_cache` is
    // `None`.
    profile_broadcast_request_rx: Option<mpsc::Receiver<ProfileBroadcastRequest>>,
    // ZEB-341: profile-card peer cache + request receiver. Mirrors the
    // profile-broadcast pair above. `None` when no owner identity is
    // loaded; always `Some` when started by production `start_node`.
    profile_card_cache: Option<Arc<crate::profile_card_broadcast::ProfileCardCache>>,
    profile_card_request_rx: Option<mpsc::Receiver<ProfileCardRequest>>,
    // Mint Phase 2 sync: channel pair bridging MintSyncEngine to Zenoh.
    // `None` when no owner identity is loaded.
    mut mint_sync_handles: Option<MintSyncHandles>,
    // ZEB-321 Phase 1 Task 8: bundle of iroh-transport resources built in
    // `start_node`. When `Some`, the event loop spawns the link-manager
    // accept loop + publisher driver as background tasks; when `None`
    // (test contexts that bypass `start_node`) the iroh subsystem stays
    // unwired and the resolver simply never receives updates — the rest
    // of the event loop is unaffected.
    iroh_handles: Option<IrohRuntimeHandles>,
) {
    // ── Startup: bind UDP, open Zenoh ────────────────────────────────
    // Each async step is raced against shutdown so stop_node can cancel
    // a slow or stuck zenoh::open without hanging on thread.join().
    macro_rules! cancellable {
        ($fut:expr, $msg:expr) => {
            tokio::select! {
                result = $fut => result,
                _ = shutdown.changed() => {
                    let e = format!("cancelled during {}", $msg);
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            }
        };
    }

    let udp = match cancellable!(
        UdpSocket::bind(format!("0.0.0.0:{RETICULUM_UDP_PORT}")),
        "UDP bind"
    ) {
        Ok(s) => s,
        Err(e) => {
            let e = format!("UDP bind on port {RETICULUM_UDP_PORT} failed: {e}");
            let _ = ready_tx.send(Err(e));
            return;
        }
    };
    if let Err(e) = udp.set_broadcast(true) {
        let e = format!("UDP set_broadcast failed: {e}");
        let _ = ready_tx.send(Err(e));
        return;
    }
    let broadcast_addr: SocketAddr = format!("255.255.255.255:{RETICULUM_UDP_PORT}")
        .parse()
        .expect("static broadcast addr");
    tracing::info!(port = RETICULUM_UDP_PORT, "UDP socket bound");

    let mut config = zenoh::Config::default();
    if let Some(ref ep) = endpoint {
        match serde_json::to_string(ep) {
            Ok(ep_json) => {
                if let Err(e) = config.insert_json5("connect/endpoints", &format!("[{ep_json}]")) {
                    let e = format!("zenoh config error: {e}");
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            }
            Err(e) => {
                let e = format!("endpoint serialize error: {e}");
                let _ = ready_tx.send(Err(e));
                return;
            }
        }
    }

    let session = match cancellable!(zenoh::open(config), "zenoh::open") {
        Ok(s) => s,
        Err(e) => {
            let e = format!("zenoh open failed: {e}");
            let _ = ready_tx.send(Err(e.clone()));
            let _ = app.emit(
                "zenoh-status",
                &crate::ZenohStatus {
                    status: "error".to_string(),
                    endpoint: None,
                    error: Some(e),
                },
            );
            return;
        }
    };
    tracing::info!("Zenoh session opened");

    // Own Zenoh session ID — attached to capacity publications so receivers
    // can determine hop distance by comparing against their peers_zid().
    let own_zid = session.zid().to_string();

    // Shared flag: set to true during intentional shutdown so spawned
    // subscriber/queryable tasks don't emit false session-lost errors.
    let closing = Arc::new(AtomicBool::new(false));

    // Channel from spawned Zenoh tasks → main select loop.
    let (zenoh_tx, mut zenoh_rx) = mpsc::channel::<ZenohEvent>(256);

    // ── ZEB-321 Phase 1 Task 8 (Task 9 + PR #157 round 4 updates): iroh ──
    // The accept-loop spawn AND the publisher spawn both happen in
    // `start_node` (lib.rs) — NOT here. lib.rs captures both JoinHandles
    // into `NodeState` so `clear_iroh_handles` can abort them on stop
    // (Greptile P1: accept loop previously detached and was never
    // aborted, leaving stale tasks across restart cycles). The event
    // loop's only responsibility for iroh is keeping the endpoint +
    // link_manager Arcs alive for its lifetime; we bind them to a
    // `_`-prefixed variable so they survive until event_loop exits but
    // the inner spawn side-effect lives upstream in lib.rs.
    let _iroh_handles_keepalive = iroh_handles;

    // ── Phase 3a: SyncEngine wire-up ────────────────────────────────────
    // The SyncEngine itself is constructed in start_node (lib.rs).
    // Here in event_loop we own the Zenoh adapter — declaring publisher
    // and subscriber on the state-root topic and forwarding bytes
    // between the SyncEngine's channels and Zenoh.
    if let Some(handles) = sync_handles.take() {
        let topic = format!("harmony/owner/{}/state-root-v1", handles.addr_hex);
        // Helper closure to surface adapter failures to the GUI as a
        // `state-root-sync-degraded` event so the user can see Phase 3a
        // sync isn't working — relying on log-only signals leaves the
        // failure invisible to anyone not tailing harmony's logs.
        // Engine itself remains alive: outbound publishes fail (engine
        // logs SyncError::TransportClosed) and inbound is gated off by
        // the engine's `inbound_closed` latch, so we operate in a
        // graceful publish-only / fully-degraded mode rather than
        // crashing the node.
        let emit_degraded = |reason: &str| {
            let _ = app.emit(
                "state-root-sync-degraded",
                serde_json::json!({
                    "reason": reason,
                    "topic": &topic,
                }),
            );
        };
        match zenoh::key_expr::KeyExpr::try_from(topic.clone()) {
            Ok(key_expr) => {
                // Outbound: drain SyncEngine publisher_tx → Zenoh put.
                let session_pub = session.clone();
                let key_pub = key_expr.clone();
                let mut outbound_rx = handles.outbound_rx;
                let closing_pub = Arc::clone(&closing);
                tokio::spawn(async move {
                    while let Some(bytes) = outbound_rx.recv().await {
                        if let Err(e) = session_pub.put(&key_pub, bytes).await {
                            if !closing_pub.load(Ordering::SeqCst) {
                                tracing::warn!(error = %e, "state-root publish failed");
                            }
                        }
                    }
                });

                // Inbound: Zenoh subscriber → SyncEngine subscriber_rx.
                match session.declare_subscriber(&key_expr).await {
                    Ok(sub) => {
                        let inbound_tx = handles.inbound_tx;
                        let closing_sub = Arc::clone(&closing);
                        let app_late = app.clone();
                        let topic_late = topic.clone();
                        tokio::spawn(async move {
                            // Two ways the loop ends:
                            //   1. `inbound_tx.send` fails — the engine
                            //      dropped its subscriber_rx, i.e. the
                            //      engine cleanly shut down. The engine
                            //      logs its own shutdown trace; we stay
                            //      silent here to avoid a spurious
                            //      "subscriber closed unexpectedly" on
                            //      every routine stop_node.
                            //   2. `sub.recv_async` returns Err — the
                            //      Zenoh session/subscriber died on us.
                            //      Warn AND emit the same degraded
                            //      event used at install-time so the
                            //      frontend can surface the failure
                            //      consistently regardless of WHEN it
                            //      happens. Skip both if the event
                            //      loop is already shutting down.
                            loop {
                                match sub.recv_async().await {
                                    Ok(sample) => {
                                        let bytes: Vec<u8> = sample.payload().to_bytes().to_vec();
                                        if inbound_tx.send(bytes).await.is_err() {
                                            break;
                                        }
                                    }
                                    Err(_) => {
                                        if !closing_sub.load(Ordering::SeqCst) {
                                            tracing::warn!(
                                                "state-root subscriber closed unexpectedly"
                                            );
                                            let _ = app_late.emit(
                                                "state-root-sync-degraded",
                                                serde_json::json!({
                                                    "reason": "subscriber_closed",
                                                    "topic": &topic_late,
                                                }),
                                            );
                                        }
                                        break;
                                    }
                                }
                            }
                        });
                    }
                    Err(e) => {
                        tracing::error!(
                            error = %e,
                            "failed to declare state-root subscriber"
                        );
                        emit_degraded("declare_subscriber_failed");
                        // Drop handles.inbound_tx by NOT spawning an
                        // inbound forwarder; engine's subscriber_rx
                        // hits None and latches `inbound_closed` so it
                        // continues in publish-only mode.
                    }
                }
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    %topic,
                    "state-root key_expr invalid; SyncEngine Zenoh adapter skipped"
                );
                emit_degraded("key_expr_invalid");
                // handles.outbound_rx and handles.inbound_tx drop at end
                // of this arm; engine sees both channels close.
            }
        }
    }

    // ── Mint Phase 2 sync: Zenoh adapter for mint-root-v1 topic ────────
    // Mirrors the owner-state `sync_handles` wiring above: outbound bytes
    // from MintSyncEngine's publish path → Zenoh put; inbound bytes from
    // Zenoh → MintSyncEngine's subscriber path. `None` when no owner
    // identity is loaded.
    if let Some(handles) = mint_sync_handles.take() {
        let topic = format!("harmony/owner/{}/mint-root-v1", handles.addr_hex);
        let emit_mint_degraded = |reason: &str| {
            let _ = app.emit(
                "mint-root-sync-degraded",
                serde_json::json!({
                    "reason": reason,
                    "topic": &topic,
                }),
            );
        };
        match zenoh::key_expr::KeyExpr::try_from(topic.clone()) {
            Ok(key_expr) => {
                // Outbound: drain MintSyncEngine publisher → Zenoh put.
                let session_pub = session.clone();
                let key_pub = key_expr.clone();
                let mut outbound_rx = handles.outbound_rx;
                let closing_pub = Arc::clone(&closing);
                tokio::spawn(async move {
                    while let Some(bytes) = outbound_rx.recv().await {
                        if let Err(e) = session_pub.put(&key_pub, bytes).await {
                            if !closing_pub.load(Ordering::SeqCst) {
                                tracing::warn!(error = %e, "mint-root publish failed");
                            }
                        }
                    }
                });

                // Inbound: Zenoh subscriber → MintSyncEngine subscriber_rx.
                // On transient recv_async errors, re-declare the subscriber
                // with exponential backoff (MAJOR 8). The only terminal
                // condition is `closing` becoming true (node shutdown) or
                // `inbound_tx.send` failing (engine dropped its receiver).
                match session.declare_subscriber(&key_expr).await {
                    Ok(sub) => {
                        let inbound_tx = handles.inbound_tx;
                        let closing_sub = Arc::clone(&closing);
                        let app_late = app.clone();
                        let topic_late = topic.clone();
                        let session_sub = session.clone();
                        let key_expr_sub = key_expr.clone();
                        tokio::spawn(async move {
                            let mut current_sub = sub;
                            let mut backoff_ms: u64 = 100;
                            'outer: loop {
                                loop {
                                    match current_sub.recv_async().await {
                                        Ok(sample) => {
                                            backoff_ms = 100; // reset on success
                                            let bytes: Vec<u8> =
                                                sample.payload().to_bytes().to_vec();
                                            if inbound_tx.send(bytes).await.is_err() {
                                                // Engine dropped its receiver — clean shutdown.
                                                break 'outer;
                                            }
                                        }
                                        Err(_) => {
                                            if closing_sub.load(Ordering::SeqCst) {
                                                break 'outer;
                                            }
                                            tracing::warn!(
                                                backoff_ms,
                                                "mint-root subscriber closed unexpectedly; \
                                                 will re-declare after backoff"
                                            );
                                            let _ = app_late.emit(
                                                "mint-root-sync-degraded",
                                                serde_json::json!({
                                                    "reason": "subscriber_closed",
                                                    "topic": &topic_late,
                                                }),
                                            );
                                            break; // break inner, retry outer
                                        }
                                    }
                                }
                                if closing_sub.load(Ordering::SeqCst) {
                                    break 'outer;
                                }
                                // Exponential backoff before re-declaring.
                                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                                backoff_ms = (backoff_ms * 2).min(30_000);
                                match session_sub.declare_subscriber(&key_expr_sub).await {
                                    Ok(new_sub) => {
                                        tracing::info!(
                                            "mint-root subscriber re-declared successfully"
                                        );
                                        current_sub = new_sub;
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            error = %e,
                                            backoff_ms,
                                            "mint-root subscriber re-declare failed; retrying"
                                        );
                                        // Don't reset backoff — keep backing off.
                                    }
                                }
                            }
                        });
                    }
                    Err(e) => {
                        tracing::error!(
                            error = %e,
                            "failed to declare mint-root subscriber"
                        );
                        emit_mint_degraded("declare_subscriber_failed");
                    }
                }
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    %topic,
                    "mint-root key_expr invalid; MintSyncEngine Zenoh adapter skipped"
                );
                emit_mint_degraded("key_expr_invalid");
            }
        }
    }

    // ── ZEB-217 Sub-C Phase 2: per-community state-CRDT Zenoh adapters ──
    // start_node spawned one engine per joined community via
    // `CommunitySyncRegistry` and handed us the matching channel halves
    // through `community_adapters`. Wire each to a Zenoh pub/sub on
    // `harmony/community/{id_hex}/state-root-v1` now that the session
    // is open and the `closing` flag exists. Each adapter runs as an
    // independent task — failure to bind one community's topic doesn't
    // affect any other.
    //
    // `spawn_community_state_zenoh_adapter` (shipped by Task 12) takes
    // `Arc<Session>` rather than the raw `Session`-clone shape used
    // by the owner-state adapter above, so wrap the session in Arc
    // once and bump the count per adapter. The owner-state adapter
    // continues to use `session.clone()` directly via Zenoh's
    // internal-Arc shape — both paths terminate at the same session
    // object.
    //
    // `session_arc` is constructed here unconditionally (even when
    // the boot-time community list is empty) so the select arm below
    // — Phase 3 Task 9's on-demand adapter request — has a live
    // `Arc<Session>` to clone for each `create_community` /
    // `redeem_invite` IPC. Cheap (one Arc bump) and avoids reaching
    // back into `session` from inside a long-running select! arm.
    let session_arc = Arc::new(session.clone());
    for req in community_adapters {
        spawn_community_state_zenoh_adapter(
            Arc::clone(&session_arc),
            req.id_hex,
            req.publisher_rx,
            req.subscriber_tx,
            Arc::clone(&closing),
        );
    }

    // ── ZEB-218 Sub-D Phase 1: library-directory subscription consumer ──
    // Mirrors the state-root subscriber pattern above — declare on
    // `LibraryDirectoryRequest::Subscribe`, drop the handle on
    // `Unsubscribe`. Each declared subscriber feeds samples into
    // `library_directory::process_sample` which decodes + verifies +
    // aggregates, then emits `library-directory-updated` on
    // non-Idempotent outcomes.
    if let (Some(library_directory), Some(library_request_rx)) =
        (library_directory, library_request_rx)
    {
        let library_directory_handle = library_directory.clone();
        // ZEB-279 Sub-D Phase 2: hold a second clone for the permanent
        // announce-topic subscriber spawned after this per-library
        // spawn (which consumes `library_directory_handle`).
        let library_directory_for_announce = library_directory.clone();
        let mut request_rx = library_request_rx;
        let session_for_libdir = Arc::clone(&session_arc);
        let app_for_libdir = app.clone();
        let closing_libdir = Arc::clone(&closing);
        tokio::spawn(async move {
            use std::collections::HashMap;
            let mut handles: HashMap<
                crate::owner_state_types::OwnerAddr,
                tokio::task::JoinHandle<()>,
            > = HashMap::new();
            while let Some(req) = request_rx.recv().await {
                match req {
                    crate::library_directory::LibraryDirectoryRequest::Subscribe(addr) => {
                        // F4 self-heal: prune any subscriber tasks that
                        // have already exited (e.g., zenoh recv_async
                        // returned Err). Without this sweep, a stale
                        // handle in the map prevents re-subscription
                        // until app restart.
                        handles.retain(|_, h| !h.is_finished());
                        if handles.contains_key(&addr) {
                            continue; // idempotent
                        }
                        let key_expr = format!(
                            "harmony/discovery/library/{}/communities",
                            hex::encode(addr.0)
                        );
                        let sub = match session_for_libdir.declare_subscriber(&key_expr).await {
                            Ok(s) => s,
                            Err(e) => {
                                tracing::warn!(
                                    ?addr,
                                    error = %e,
                                    "declare_subscriber failed for library"
                                );
                                continue;
                            }
                        };
                        let dir = Arc::clone(&library_directory_handle);
                        let app_for_task = app_for_libdir.clone();
                        let closing_task = Arc::clone(&closing_libdir);
                        // F2: capture the subscribed library addr — the
                        // topic owner — and pass it into process_sample
                        // so attribution can't be spoofed by a malicious
                        // library publishing entries under another
                        // library's listed_by.
                        let subscribed_addr = addr;
                        let handle = tokio::spawn(async move {
                            loop {
                                match sub.recv_async().await {
                                    Ok(sample) => {
                                        let bytes = sample.payload().to_bytes().to_vec();
                                        match dir.process_sample(subscribed_addr, bytes).await {
                                            Ok(result) => {
                                                // F6: emit on any non-idempotent state
                                                // change OR on cap-eviction (independent
                                                // of outcome's discriminant).
                                                let outcome_changed = !matches!(
                                                    result.outcome,
                                                    crate::library_directory::OnEntryOutcome::Idempotent
                                                );
                                                if outcome_changed || result.evicted.is_some() {
                                                    let community_id = match &result.outcome {
                                                        crate::library_directory::OnEntryOutcome::Inserted(c)
                                                        | crate::library_directory::OnEntryOutcome::Replaced(c)
                                                        | crate::library_directory::OnEntryOutcome::AccretedListedBy(c) => Some(*c),
                                                        crate::library_directory::OnEntryOutcome::Idempotent => None,
                                                    };
                                                    let _ = app_for_task.emit(
                                                        "library-directory-updated",
                                                        serde_json::json!({
                                                            "communityId": community_id.map(|c| hex::encode(c.0)),
                                                        }),
                                                    );
                                                }
                                            }
                                            Err(e) => {
                                                tracing::warn!(
                                                    error = ?e,
                                                    "library-directory entry rejected"
                                                );
                                            }
                                        }
                                    }
                                    Err(_) => {
                                        if !closing_task.load(Ordering::SeqCst) {
                                            tracing::warn!(
                                                ?addr,
                                                "library subscriber closed unexpectedly"
                                            );
                                        }
                                        break;
                                    }
                                }
                            }
                        });
                        handles.insert(addr, handle);
                    }
                    crate::library_directory::LibraryDirectoryRequest::Unsubscribe(addr) => {
                        if let Some(h) = handles.remove(&addr) {
                            h.abort();
                        }
                        let evicted = library_directory_handle.drop_library(&addr).await;
                        if !evicted.is_empty() {
                            let _ = app_for_libdir.emit(
                                "library-directory-updated",
                                serde_json::json!({ "communityId": null }),
                            );
                        }
                    }
                }
            }
        });

        // ZEB-279 Sub-D Phase 2: permanent announce-topic subscriber.
        // Single fixed-key subscription, lifetime = app lifetime — no
        // add/remove plumbing. Mirrors the per-library subscriber shape
        // above but without the request-channel (the announce key is
        // a fixed exact-match string; everyone listens to it always).
        {
            let dir = library_directory_for_announce;
            let session_for_announce = Arc::clone(&session_arc);
            let app_for_announce = app.clone();
            let closing_announce = Arc::clone(&closing);
            tokio::spawn(async move {
                let key_expr = "harmony/discovery/library/announce";
                // F4 (CodeAnt Major): outer retry loop. Previously
                // declare_subscriber failures and mid-session recv_async
                // errors permanently disabled auto-discovery; now we
                // exponentially back off and re-declare so a transient
                // transport hiccup doesn't kill discovery for the session.
                let mut backoff = std::time::Duration::from_secs(5);
                const MAX_BACKOFF: std::time::Duration = std::time::Duration::from_secs(60);
                loop {
                    if closing_announce.load(Ordering::SeqCst) {
                        break;
                    }
                    let sub = match session_for_announce.declare_subscriber(key_expr).await {
                        Ok(s) => {
                            // Reset backoff on each successful declare so a
                            // long-lived subscriber that briefly hiccups
                            // doesn't start from a 60s wait next time.
                            backoff = std::time::Duration::from_secs(5);
                            s
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                backoff_s = backoff.as_secs(),
                                "library announce declare_subscriber failed; retrying after backoff",
                            );
                            tokio::time::sleep(backoff).await;
                            backoff = std::cmp::min(backoff * 2, MAX_BACKOFF);
                            continue;
                        }
                    };
                    loop {
                        match sub.recv_async().await {
                            Ok(sample) => {
                                let bytes_view = sample.payload().to_bytes();
                                // F3 (CodeAnt Critical Security): drop oversized
                                // payloads BEFORE materializing into an owned
                                // Vec<u8>. The announce topic is global — any
                                // peer can publish, so an attacker could
                                // flood-DoS via attacker-sized frames otherwise.
                                if bytes_view.len()
                                    > crate::library_directory::MAX_ANNOUNCE_WIRE_BYTES
                                {
                                    tracing::warn!(
                                        size = bytes_view.len(),
                                        max = crate::library_directory::MAX_ANNOUNCE_WIRE_BYTES,
                                        "oversized library announce dropped"
                                    );
                                    continue;
                                }
                                let bytes = bytes_view.to_vec();
                                match dir.process_announce(bytes).await {
                                    Ok(result) => {
                                        let changed = matches!(
                                            result.outcome,
                                            crate::library_directory::AnnounceOutcome::Inserted(_)
                                                | crate::library_directory::AnnounceOutcome::Updated(_)
                                        );
                                        if changed || result.evicted.is_some() {
                                            let _ = app_for_announce.emit(
                                                "library-directory-updated",
                                                serde_json::json!({ "communityId": null }),
                                            );
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            error = ?e,
                                            "library announce rejected"
                                        );
                                    }
                                }
                            }
                            Err(_) => {
                                if !closing_announce.load(Ordering::SeqCst) {
                                    tracing::warn!(
                                        "library announce subscriber closed unexpectedly; reconnecting"
                                    );
                                }
                                break; // break inner loop → outer redeclares
                            }
                        }
                    }
                    if closing_announce.load(Ordering::SeqCst) {
                        break;
                    }
                    // Brief pause before re-declaring on mid-session
                    // recv_async failure (transport hiccup case).
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            });
        }
    }

    // ── ZEB-281 Sub-D Phase 4: profile-broadcast subscriber pool ─────
    // One Zenoh subscriber per (active) subscription_id, keyed off
    // ProfileBroadcastRequest::{Subscribe, Unsubscribe} from NodeState.
    // Same retry/backoff shape as the Phase 2 announce subscriber above
    // (5s initial backoff, max 60s). MAX_BROADCAST_WIRE_BYTES gates the
    // payload before we materialize an owned Vec<u8>; on decode +
    // verify success the per-subscription cache is updated and the
    // FLAT `profile-broadcast-received` event is emitted to the
    // frontend.
    if let (Some(profile_broadcast_cache), Some(profile_broadcast_request_rx)) =
        (profile_broadcast_cache, profile_broadcast_request_rx)
    {
        let session_for_profile = Arc::clone(&session_arc);
        let app_for_profile = app.clone();
        let closing_for_profile = Arc::clone(&closing);
        let cache_for_loop = Arc::clone(&profile_broadcast_cache);
        let mut request_rx = profile_broadcast_request_rx;
        tokio::spawn(async move {
            use std::collections::HashMap;
            let mut handles: HashMap<
                crate::profile_broadcast::SubscriptionId,
                tokio::task::JoinHandle<()>,
            > = HashMap::new();
            while let Some(req) = request_rx.recv().await {
                match req {
                    ProfileBroadcastRequest::Subscribe {
                        subscription_id,
                        peer_addr,
                    } => {
                        // Self-heal: prune any subscriber tasks that
                        // have already exited (same pattern as the
                        // library subscriber pool F4 fix).
                        handles.retain(|_, h| !h.is_finished());
                        if handles.contains_key(&subscription_id) {
                            tracing::warn!(
                                subscription_id,
                                "ProfileBroadcastRequest::Subscribe duplicate id — ignoring"
                            );
                            continue;
                        }
                        let key_expr = crate::profile_broadcast::broadcast_topic_for(&peer_addr);
                        let session = Arc::clone(&session_for_profile);
                        let app_for_task = app_for_profile.clone();
                        let closing_task = Arc::clone(&closing_for_profile);
                        let cache_for_task = Arc::clone(&cache_for_loop);
                        let handle = tokio::spawn(async move {
                            let mut backoff = std::time::Duration::from_secs(5);
                            const MAX_BACKOFF: std::time::Duration =
                                std::time::Duration::from_secs(60);
                            loop {
                                if closing_task.load(Ordering::SeqCst) {
                                    break;
                                }
                                let sub = match session.declare_subscriber(&key_expr).await {
                                    Ok(s) => {
                                        backoff = std::time::Duration::from_secs(5);
                                        s
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            error = %e,
                                            subscription_id,
                                            backoff_s = backoff.as_secs(),
                                            "profile broadcast declare_subscriber failed; \
                                             retrying after backoff"
                                        );
                                        tokio::time::sleep(backoff).await;
                                        backoff = std::cmp::min(backoff * 2, MAX_BACKOFF);
                                        continue;
                                    }
                                };
                                loop {
                                    match sub.recv_async().await {
                                        Ok(sample) => {
                                            let bytes_view = sample.payload().to_bytes();
                                            // Drop oversized payloads BEFORE
                                            // materializing into an owned
                                            // Vec<u8>.
                                            if bytes_view.len()
                                                > crate::profile_broadcast::MAX_BROADCAST_WIRE_BYTES
                                            {
                                                tracing::warn!(
                                                    size = bytes_view.len(),
                                                    max = crate::profile_broadcast::MAX_BROADCAST_WIRE_BYTES,
                                                    subscription_id,
                                                    "oversized profile broadcast dropped"
                                                );
                                                continue;
                                            }
                                            let bytes = bytes_view.to_vec();
                                            let broadcast: crate::profile_broadcast::ProfileMembershipBroadcast =
                                                match ciborium::from_reader(&bytes[..]) {
                                                    Ok(b) => b,
                                                    Err(e) => {
                                                        tracing::warn!(
                                                            error = ?e,
                                                            subscription_id,
                                                            "profile broadcast CBOR decode failed"
                                                        );
                                                        continue;
                                                    }
                                                };
                                            match cache_for_task
                                                .on_sample(subscription_id, broadcast)
                                                .await
                                            {
                                                Ok(outcome) => {
                                                    tracing::debug!(
                                                        ?outcome,
                                                        subscription_id,
                                                        "profile broadcast cached"
                                                    );
                                                    if let Some(info) = cache_for_task
                                                        .get_cached(subscription_id)
                                                        .await
                                                    {
                                                        // Spec §7: emit flat payload
                                                        // (subscriptionId + DiscoveredProfileInfo
                                                        // fields hoisted).
                                                        let _ = app_for_task.emit(
                                                            "profile-broadcast-received",
                                                            serde_json::json!({
                                                                "subscriptionId": subscription_id,
                                                                "ownerAddr": info.owner_addr,
                                                                "communityIds": info.community_ids,
                                                                "sharedAt": info.shared_at,
                                                            }),
                                                        );
                                                    }
                                                }
                                                Err(e) => {
                                                    tracing::warn!(
                                                        error = ?e,
                                                        subscription_id,
                                                        "profile broadcast rejected"
                                                    );
                                                }
                                            }
                                        }
                                        Err(_) => {
                                            if !closing_task.load(Ordering::SeqCst) {
                                                tracing::warn!(
                                                    subscription_id,
                                                    "profile broadcast subscriber closed; \
                                                     reconnecting"
                                                );
                                            }
                                            break;
                                        }
                                    }
                                }
                                if closing_task.load(Ordering::SeqCst) {
                                    break;
                                }
                                // Brief pause before re-declaring on
                                // mid-session recv_async failure.
                                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                            }
                        });
                        handles.insert(subscription_id, handle);
                    }
                    ProfileBroadcastRequest::Unsubscribe { subscription_id } => {
                        if let Some(h) = handles.remove(&subscription_id) {
                            h.abort();
                        }
                        cache_for_loop.drop_subscription(subscription_id).await;
                    }
                }
            }
        });
    }

    // ── ZEB-341: profile-card subscriber pool ────────────────────────
    // One Zenoh subscriber per (active) subscription_id, keyed off
    // ProfileCardRequest::{Subscribe, Unsubscribe} from NodeState. Same
    // retry/backoff shape as the profile-broadcast pool above (5s initial
    // backoff, max 60s). MAX_CARD_WIRE_BYTES gates the payload before we
    // materialize an owned Vec<u8>; on decode the card is verified (cert
    // model via `verify_card`) AND attribution-checked (verified owner ==
    // subscribed owner) before caching, then the FLAT
    // `member-card-received` event is emitted to the frontend.
    if let (Some(profile_card_cache), Some(profile_card_request_rx)) =
        (profile_card_cache, profile_card_request_rx)
    {
        let session_for_card = Arc::clone(&session_arc);
        let app_for_card = app.clone();
        let closing_for_card = Arc::clone(&closing);
        let cache_for_loop = Arc::clone(&profile_card_cache);
        let mut request_rx = profile_card_request_rx;
        tokio::spawn(async move {
            use std::collections::HashMap;
            let mut handles: HashMap<
                crate::profile_broadcast::SubscriptionId,
                tokio::task::JoinHandle<()>,
            > = HashMap::new();
            while let Some(req) = request_rx.recv().await {
                match req {
                    ProfileCardRequest::Subscribe {
                        subscription_id,
                        owner_id,
                    } => {
                        // Self-heal: prune any subscriber tasks that have
                        // already exited.
                        handles.retain(|_, h| !h.is_finished());
                        if handles.contains_key(&subscription_id) {
                            tracing::warn!(
                                subscription_id,
                                "ProfileCardRequest::Subscribe duplicate id — ignoring"
                            );
                            continue;
                        }
                        let key_expr = crate::profile_card_broadcast::card_topic_for(&owner_id);
                        let session = Arc::clone(&session_for_card);
                        let app_for_task = app_for_card.clone();
                        let closing_task = Arc::clone(&closing_for_card);
                        let cache_for_task = Arc::clone(&cache_for_loop);
                        let handle = tokio::spawn(async move {
                            let mut backoff = std::time::Duration::from_secs(5);
                            const MAX_BACKOFF: std::time::Duration =
                                std::time::Duration::from_secs(60);
                            loop {
                                if closing_task.load(Ordering::SeqCst) {
                                    break;
                                }
                                let sub = match session.declare_subscriber(&key_expr).await {
                                    Ok(s) => {
                                        backoff = std::time::Duration::from_secs(5);
                                        s
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            error = %e,
                                            subscription_id,
                                            backoff_s = backoff.as_secs(),
                                            "profile card declare_subscriber failed; \
                                             retrying after backoff"
                                        );
                                        tokio::time::sleep(backoff).await;
                                        backoff = std::cmp::min(backoff * 2, MAX_BACKOFF);
                                        continue;
                                    }
                                };
                                loop {
                                    match sub.recv_async().await {
                                        Ok(sample) => {
                                            let bytes_view = sample.payload().to_bytes();
                                            // Drop oversized payloads BEFORE
                                            // materializing into an owned
                                            // Vec<u8>.
                                            if bytes_view.len()
                                                > crate::profile_card_broadcast::MAX_CARD_WIRE_BYTES
                                            {
                                                tracing::warn!(
                                                    size = bytes_view.len(),
                                                    max = crate::profile_card_broadcast::MAX_CARD_WIRE_BYTES,
                                                    subscription_id,
                                                    "oversized profile card dropped"
                                                );
                                                continue;
                                            }
                                            let bytes = bytes_view.to_vec();
                                            let card: crate::profile_card_broadcast::ProfileCardBroadcast =
                                                match ciborium::from_reader(&bytes[..]) {
                                                    Ok(c) => c,
                                                    Err(e) => {
                                                        tracing::warn!(
                                                            error = ?e,
                                                            subscription_id,
                                                            "profile card CBOR decode failed"
                                                        );
                                                        continue;
                                                    }
                                                };
                                            // Cert model + signature.
                                            let verified_owner =
                                                match crate::profile_card_broadcast::verify_card(
                                                    &card,
                                                ) {
                                                    Ok(o) => o,
                                                    Err(e) => {
                                                        tracing::warn!(
                                                            error = ?e,
                                                            subscription_id,
                                                            "profile card verify failed"
                                                        );
                                                        continue;
                                                    }
                                                };
                                            // Attribution: bind the verified
                                            // owner to the SUBSCRIBED owner
                                            // (the topic's owner).
                                            if verified_owner != owner_id {
                                                tracing::warn!(
                                                    subscription_id,
                                                    "profile card attribution mismatch"
                                                );
                                                continue;
                                            }
                                            cache_for_task
                                                .insert_verified(subscription_id, &card)
                                                .await;
                                            if let Some(info) =
                                                cache_for_task.get_cached(subscription_id).await
                                            {
                                                // Flat payload (subscriptionId +
                                                // DiscoveredCardInfo fields hoisted).
                                                let _ = app_for_task.emit(
                                                    "member-card-received",
                                                    serde_json::json!({
                                                        "subscriptionId": subscription_id,
                                                        "ownerIdHex": info.owner_id_hex,
                                                        "displayName": info.display_name,
                                                        "statusText": info.status_text,
                                                        "avatarCid": info.avatar_cid,
                                                        // ZEB-345: include the
                                                        // long-form page root on the
                                                        // instant PUSH path too, so an
                                                        // open panel updates without
                                                        // waiting for the poll fallback
                                                        // (Cursor). DiscoveredCardInfo
                                                        // already carries it hex-encoded.
                                                        "profilePageRoot": info.profile_page_root,
                                                    }),
                                                );
                                            }
                                        }
                                        Err(_) => {
                                            if !closing_task.load(Ordering::SeqCst) {
                                                tracing::warn!(
                                                    subscription_id,
                                                    "profile card subscriber closed; \
                                                     reconnecting"
                                                );
                                            }
                                            break;
                                        }
                                    }
                                }
                                if closing_task.load(Ordering::SeqCst) {
                                    break;
                                }
                                // Brief pause before re-declaring on
                                // mid-session recv_async failure.
                                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                            }
                        });
                        handles.insert(subscription_id, handle);
                    }
                    ProfileCardRequest::Unsubscribe { subscription_id } => {
                        if let Some(h) = handles.remove(&subscription_id) {
                            h.abort();
                        }
                        cache_for_loop.drop_subscription(subscription_id).await;
                    }
                }
            }
        });
    }

    // ── ZEB-352: always-on DM-call signaling subscriber ──────────────
    // Subscribe to our OWN owner-scoped signaling topic so inbound
    // Invite/Accept/Decline/Cancel/End signals can be opened, verified,
    // and surfaced to the frontend as call events. Gated on the owner
    // identity being loaded at run() start (mirrors the dm_outbox /
    // crdt_state shape used by the DM machinery): we need (a) the
    // device-#2 signing key + self_owner from `dm_outbox`, and (b) the
    // `OwnerDeviceCache` (inside `crdt_state`) to resolve the caller's
    // identity_pub for signature verification. v1 requires the identity to
    // exist at startup; a node that adopts an identity later picks up the
    // subscription on the next start_node.
    //
    // The `JoinHandle` is bound to a `_`-prefixed run()-scope variable
    // (mirrors `_serve_handle` / `_iroh_handles_keepalive`) so the spawned
    // task survives for the lifetime of the event loop instead of being
    // dropped (and aborted) at the end of this block. It is explicitly aborted
    // in the shutdown drain below (like the media subscribers) rather than left
    // to terminate on the `closing` flag + a `recv_async` error.
    let voice_signal_sub_handle: Option<tokio::task::JoinHandle<()>> =
        if let (Some(dm_outbox), Some(crdt_state)) = (dm_outbox.as_ref(), crdt_state.as_ref()) {
            // Snapshot self owner + derive our X25519 private once, under the
            // outbox lock, before spawning the long-lived subscriber.
            let (self_owner_hex, self_x25519_priv) = {
                let g = dm_outbox.lock().await;
                let hex = hex::encode(g.self_owner.0);
                let x_priv = crate::dm_signing::ed25519_priv_to_x25519(&g.community_signing_key);
                (hex, x_priv)
            };
            let key_expr = format!("harmony/voice-signal/{self_owner_hex}");
            let session_for_signal = Arc::clone(&session_arc);
            let crdt_for_signal = Arc::clone(crdt_state);
            let app_for_signal = app.clone();
            let closing_for_signal = Arc::clone(&closing);
            let signal_handle = tokio::spawn(async move {
                let mut backoff = std::time::Duration::from_secs(5);
                const MAX_BACKOFF: std::time::Duration = std::time::Duration::from_secs(60);
                loop {
                    if closing_for_signal.load(Ordering::SeqCst) {
                        break;
                    }
                    let sub = match session_for_signal.declare_subscriber(&key_expr).await {
                        Ok(s) => {
                            backoff = std::time::Duration::from_secs(5);
                            s
                        }
                        Err(e) => {
                            tracing::warn!(
                                %key_expr,
                                error = %e,
                                backoff_s = backoff.as_secs(),
                                "voice signal declare_subscriber failed; retrying after backoff"
                            );
                            tokio::time::sleep(backoff).await;
                            backoff = std::cmp::min(backoff * 2, MAX_BACKOFF);
                            continue;
                        }
                    };
                    loop {
                        match sub.recv_async().await {
                            Ok(sample) => {
                                // The signaling topic is peer-writable, so bound
                                // the payload BEFORE allocating: a sealed signal
                                // is a few hundred bytes; cap well above that so
                                // one oversized sample can't force an unbounded
                                // heap allocation ahead of peek_caller's reject.
                                const MAX_VOICE_SIGNAL_BYTES: usize = 8 * 1024;
                                if sample.payload().len() > MAX_VOICE_SIGNAL_BYTES {
                                    tracing::warn!(
                                        size = sample.payload().len(),
                                        max = MAX_VOICE_SIGNAL_BYTES,
                                        "oversized voice signal dropped"
                                    );
                                    continue;
                                }
                                let sealed = sample.payload().to_bytes().to_vec();
                                // Open + decode the sealed box ONCE. The box is
                                // sealed to our X25519 key (identical for every
                                // candidate device), so we open here and only
                                // re-verify the Ed25519 signature per candidate
                                // below — never re-opening. The decoded caller is
                                // unverified until verify_decoded_signal binds the
                                // signature to a cached identity.
                                let signed = match crate::voice_signal::open_and_decode_unverified(
                                    &self_x25519_priv,
                                    &sealed,
                                ) {
                                    Ok(s) => s,
                                    Err(_) => continue,
                                };
                                let caller = signed.body.caller;
                                // Collect ALL cached identity pubs for the
                                // caller — Harmony is multi-device, so the
                                // caller may have signed with any enrolled
                                // device. Try each until one verifies.
                                let candidate_pubs: Vec<[u8; 64]> = {
                                    let g = crdt_for_signal.lock().await;
                                    g.owner_device_cache
                                        .devices
                                        .get(&caller)
                                        .map(|entry| {
                                            entry
                                                .device_identity_pubs
                                                .iter()
                                                .filter_map(|p| *p)
                                                .collect()
                                        })
                                        .unwrap_or_default()
                                };
                                let mut emitted = false;
                                for identity_pub in candidate_pubs {
                                    let Some(device_hash) =
                                        crate::dm_signing::derive_device_hash_from_identity_pub(
                                            &identity_pub,
                                        )
                                    else {
                                        continue;
                                    };
                                    if let Ok(signal) = crate::voice_signal::verify_decoded_signal(
                                        &signed,
                                        &identity_pub,
                                        device_hash,
                                    ) {
                                        if matches!(
                                            signal.kind,
                                            crate::voice_signal::VoiceSignalKind::Invite
                                        ) {
                                            // Resolve the shared DM space for this caller so the
                                            // frontend can pass spaceId to accept_call/decline_call.
                                            let space_hex = {
                                                let g = crdt_for_signal.lock().await;
                                                g.spaces.iter().find_map(|(sid, sp)| {
                                                    // Match the 1:1 DM space with this caller only.
                                                    // `members.len() == 2` excludes group-DM spaces
                                                    // (which also have a content_key and include the
                                                    // caller) — the send side likewise assumes a
                                                    // 2-member DM (resolve_dm_call_peer).
                                                    if sp.content_key.is_some()
                                                        && sp.members.len() == 2
                                                        && sp.members.contains(&signal.caller)
                                                    {
                                                        Some(hex::encode(sid.0))
                                                    } else {
                                                        None
                                                    }
                                                })
                                            };
                                            match space_hex {
                                                Some(ref sh) => {
                                                    emit_voice_signal_event(
                                                        &app_for_signal,
                                                        &signal,
                                                        Some(sh.as_str()),
                                                    );
                                                }
                                                None => {
                                                    // No shared DM space with the caller — callee
                                                    // cannot service this invite; drop silently.
                                                    tracing::debug!(
                                                        caller = %hex::encode(signal.caller.0),
                                                        "voice Invite dropped: no shared DM space"
                                                    );
                                                }
                                            }
                                        } else {
                                            emit_voice_signal_event(&app_for_signal, &signal, None);
                                        }
                                        emitted = true;
                                        break;
                                    }
                                }
                                let _ = emitted;
                            }
                            Err(_) => {
                                if !closing_for_signal.load(Ordering::SeqCst) {
                                    tracing::warn!(
                                        %key_expr,
                                        "voice signal subscriber closed; reconnecting"
                                    );
                                }
                                break;
                            }
                        }
                    }
                    if closing_for_signal.load(Ordering::SeqCst) {
                        break;
                    }
                    // Brief pause before re-declaring on mid-session failure.
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            });
            // Hand the handle back as the block's tail value so the run()-scope
            // binding keeps the task alive (vs. dropping it here, which aborts).
            Some(signal_handle)
        } else {
            None
        };

    // ── ZEB-343: content-serve queryable ─────────────────────────
    // Answer peer content GETs from the local StorageTier cache. The
    // lookup closure routes through CasOp::GetLocal so the read happens
    // on the event-loop-owned runtime (read-only; no recursive fetch).
    {
        let cas_op_tx_serve = cas_op_tx.clone();
        let serve_lookup = std::sync::Arc::new(move |cid: ContentId| {
            let tx = cas_op_tx_serve.clone();
            Box::pin(async move {
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                if tx
                    .send(crate::content_store::CasOp::GetLocal {
                        cid,
                        reply: reply_tx,
                    })
                    .await
                    .is_err()
                {
                    return None;
                }
                reply_rx.await.ok().flatten()
            })
                as std::pin::Pin<Box<dyn std::future::Future<Output = Option<Vec<u8>>> + Send>>
        });
        let _serve_handle = match spawn_content_serve_queryable(
            std::sync::Arc::clone(&session_arc),
            serve_lookup,
            std::sync::Arc::clone(&closing),
        )
        .await
        {
            Ok(handle) => handle,
            Err(e) => {
                // Never report ready with peer content-serving silently disabled
                // (CodeRabbit). Mirrors the other ready_tx failure paths above.
                let _ = ready_tx.send(Err(e));
                return;
            }
        };
    }

    // ── Process startup actions (declare queryables + subscribers) ────
    for action in startup_actions {
        dispatch_action(
            action,
            &session,
            &zenoh_tx,
            &udp,
            &broadcast_addr,
            &app,
            &closing,
            &own_zid,
        )
        .await;
    }

    // Subscribe to community channel messages for real-time messaging.
    dispatch_action(
        RuntimeAction::Subscribe {
            key_expr: "harmony/community/*/channels/*".to_string(),
        },
        &session,
        &zenoh_tx,
        &udp,
        &broadcast_addr,
        &app,
        &closing,
        &own_zid,
    )
    .await;

    // Subscribe to vine descriptors for the vine feed.
    dispatch_action(
        RuntimeAction::Subscribe {
            key_expr: "harmony/vines/*".to_string(),
        },
        &session,
        &zenoh_tx,
        &udp,
        &broadcast_addr,
        &app,
        &closing,
        &own_zid,
    )
    .await;

    // Subscribe to vine reactions (likes/unlikes).
    dispatch_action(
        RuntimeAction::Subscribe {
            key_expr: "harmony/vines/*/reactions/**".to_string(),
        },
        &session,
        &zenoh_tx,
        &udp,
        &broadcast_addr,
        &app,
        &closing,
        &own_zid,
    )
    .await;

    // Note: per-creator Zenoh subscriptions are not used yet because the
    // publish path (harmony/vines/{addr}) does not include /announce/.
    // Once harmony-node adopts the full keyspace protocol
    // (harmony/vines/{addr}/announce/{cid}), per-creator subscriptions can
    // be added here for write-side filtering. For now, the wildcard
    // subscription above catches all vines and we route by followed_set.

    // Subscribe to content availability announcements for the file manager.
    dispatch_action(
        RuntimeAction::Subscribe {
            key_expr: "harmony/announce/*".to_string(),
        },
        &session,
        &zenoh_tx,
        &udp,
        &broadcast_addr,
        &app,
        &closing,
        &own_zid,
    )
    .await;

    // Subscribe to LAN pairing wire messages (ZEB-197 v2 pairing) ONLY when
    // a pairing consumer (`pairing_in_tx`) was wired into this event loop.
    // PR #63 review: an unconditional subscribe paid the Zenoh subscription
    // cost (and exercised the ingress hot-path branch on every sample) for
    // nodes that don't even host the pairing state machine. Idle devices
    // still subscribe when the SM is wired — the SM's select! gate ensures
    // we don't ACT on inbound messages outside an active session, but we
    // need to be RECEIVING so the buffer is populated when a session starts.
    if pairing_in_tx.is_some() {
        dispatch_action(
            RuntimeAction::Subscribe {
                key_expr: crate::pairing::PAIRING_KEY_GLOB.to_string(),
            },
            &session,
            &zenoh_tx,
            &udp,
            &broadcast_addr,
            &app,
            &closing,
            &own_zid,
        )
        .await;
    }

    // Subscribe to inbound mail for this node's address, plus the /root
    // pointer that the Phase 2 MailSync walker consumes. Both keys are
    // hoisted to the loop scope so the emit_frontend_event filter can
    // dispatch exact-match by string comparison.
    //
    // Poison fallback: empty strings, guarded with `!key.is_empty()` in
    // the filter. Subscriptions are skipped rather than panicking —
    // mail functionality degrades but the rest of the node stays alive.
    let (own_mail_key, own_root_key) = match mail_mgr.lock() {
        Ok(g) => {
            let own_hex = g.owner_address_hex();
            drop(g);
            (
                format!("harmony/mail/v1/{own_hex}"),
                format!("harmony/mail/v1/{own_hex}/root"),
            )
        }
        Err(e) => {
            tracing::error!(error = %e, "mail_mgr mutex poisoned at startup; mail subs disabled");
            (String::new(), String::new())
        }
    };
    if !own_mail_key.is_empty() {
        dispatch_action(
            RuntimeAction::Subscribe {
                key_expr: own_mail_key.clone(),
            },
            &session,
            &zenoh_tx,
            &udp,
            &broadcast_addr,
            &app,
            &closing,
            &own_zid,
        )
        .await;
        dispatch_action(
            RuntimeAction::Subscribe {
                key_expr: own_root_key.clone(),
            },
            &session,
            &zenoh_tx,
            &udp,
            &broadcast_addr,
            &app,
            &closing,
            &own_zid,
        )
        .await;
    }

    // Signal the caller that startup fully succeeded — UDP bound, Zenoh
    // session open, all queryables and subscribers declared.
    let _ = ready_tx.send(Ok(()));

    // Phase 2: cold-start root query. Pulls current root via Zenoh `get` in
    // case the gateway last published before this client subscribed. 10s
    // budget — on failure or timeout, the normal publish-on-next-write
    // flow still delivers eventually.
    if let Some(ref sync) = mail_sync {
        if !own_root_key.is_empty() {
            let sync = Arc::clone(sync);
            let session_clone = session.clone();
            let key = own_root_key.clone();
            tokio::spawn(async move {
                match query_mail_root(&session_clone, &key, "startup").await {
                    Ok(Some(payload)) => sync.handle_startup_query_reply(Some(&payload)).await,
                    Ok(None) => {
                        tracing::warn!(
                            "startup root query: no responder — live push will catch up on next gateway publish"
                        );
                        sync.report_query_error(
                            "no gateway responded to startup query".to_string(),
                        );
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "startup root query failed");
                        sync.report_query_error(format!("startup query failed: {e}"));
                    }
                }
            });
        }
    }

    // ── Timer (250ms = 4 ticks/sec, same as harmony-node) ────────────
    let mut timer = tokio::time::interval(Duration::from_millis(250));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let start = std::time::Instant::now();

    let mut udp_buf = vec![0u8; 65535];

    // Directly connected Zenoh peers — refreshed every 20 timer ticks (~5s).
    // Used to derive hop distance: ZID in this set → hop 1, else → hop 2.
    // Eagerly populated so capacity updates arriving before the first refresh
    // aren't misclassified as hop 2.
    let mut direct_peer_zids: std::collections::HashSet<String> = session
        .info()
        .peers_zid()
        .await
        .map(|z| z.to_string())
        .collect();
    let mut peer_refresh_counter: u64 = 0;

    // Dynamic voice channel subscriptions — keyed by (community, channel).
    let mut voice_subs: std::collections::HashMap<
        (
            crate::owner_state_types::SpaceId,
            crate::community_membership::ChannelId,
        ),
        tokio::task::JoinHandle<()>,
    > = std::collections::HashMap::new();

    // ZEB-350: per-join channel key (seals/open media + beacons) and the node's
    // own device id (names the outbound topic segment). Keyed identically to
    // voice_subs so Join/Leave keep them in lockstep.
    let mut voice_keys: std::collections::HashMap<
        (
            crate::owner_state_types::SpaceId,
            crate::community_membership::ChannelId,
        ),
        std::sync::Arc<crate::community_channel_log::ChannelKey>,
    > = std::collections::HashMap::new();
    let mut voice_own_device: Option<String> = None; // hex of self ed25519 vk

    // ZEB-351 Voice V3: (community, channel) → the shared mute flag handed to
    // that channel's presence publisher. `set_voice_muted` →
    // `VoiceChannelRequest::SetMuted` flips this `Arc<AtomicBool>`; the publisher
    // reads it each heartbeat. Kept in lockstep with `voice_keys` on Join/Leave.
    let mut voice_mute_flags: std::collections::HashMap<
        (
            crate::owner_state_types::SpaceId,
            crate::community_membership::ChannelId,
        ),
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    > = std::collections::HashMap::new();

    // ZEB-351 Voice V3: (community, channel) → the monotone presence-beacon `seq`
    // counter SHARED between that channel's heartbeat publisher and the immediate
    // mute beacon fired on `SetMuted`. Both draw strictly-increasing `seq`s from
    // this one `Arc<AtomicU64>`, so an immediate beacon can never outrank later
    // heartbeats. Kept in lockstep with `voice_mute_flags` on Join/Leave.
    let mut voice_presence_seq: std::collections::HashMap<
        (
            crate::owner_state_types::SpaceId,
            crate::community_membership::ChannelId,
        ),
        std::sync::Arc<std::sync::atomic::AtomicU64>,
    > = std::collections::HashMap::new();

    // ZEB-352: DM call state — keyed by 16-byte call_id. No presence (2-party implicit).
    let mut dm_voice_subs: std::collections::HashMap<[u8; 16], tokio::task::JoinHandle<()>> =
        std::collections::HashMap::new();
    let mut dm_voice_keys: std::collections::HashMap<
        [u8; 16],
        std::sync::Arc<crate::community_channel_log::ChannelKey>,
    > = std::collections::HashMap::new();
    let mut dm_voice_mute_flags: std::collections::HashMap<
        [u8; 16],
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    > = std::collections::HashMap::new();

    // ZEB-350: presence layer. One shared roster map for the loop's
    // lifetime; per-join publisher/subscriber handles; a `voice_identity`
    // stash so Leave can build the `left` tombstone without re-resolving caps;
    // a monotonic clock (reusing `start`) for apply/sweep; and a 4 s sweep
    // tick that evicts silent (12 s) entries and re-emits affected rosters.
    let voice_presence_map = std::sync::Arc::new(tokio::sync::Mutex::new(
        crate::voice_presence::VoicePresenceMap::new(),
    ));
    let mut voice_presence_subs: std::collections::HashMap<
        (
            crate::owner_state_types::SpaceId,
            crate::community_membership::ChannelId,
        ),
        tokio::task::JoinHandle<()>,
    > = std::collections::HashMap::new();
    let mut voice_presence_pubs: std::collections::HashMap<
        (
            crate::owner_state_types::SpaceId,
            crate::community_membership::ChannelId,
        ),
        tokio::task::JoinHandle<()>,
    > = std::collections::HashMap::new();
    // (community, channel) → (self_owner, self_device, joined_hlc, signing_key)
    // — stashed on Join, read+removed on Leave to mint the presence tombstone
    // (which must be re-signed by device #2, so the key is carried here too).
    #[allow(clippy::type_complexity)]
    let mut voice_identity: std::collections::HashMap<
        (
            crate::owner_state_types::SpaceId,
            crate::community_membership::ChannelId,
        ),
        (
            crate::owner_state_types::OwnerAddr,
            [u8; 32],
            crate::owner_state_types::Hlc,
            std::sync::Arc<ed25519_dalek::SigningKey>,
        ),
    > = std::collections::HashMap::new();
    // Monotonic clock for apply/sweep (ms since the loop's `start` Instant,
    // matching the `now` math the UDP/timer arms already use).
    let voice_now_ms: std::sync::Arc<dyn Fn() -> u64 + Send + Sync> =
        std::sync::Arc::new(move || start.elapsed().as_millis() as u64);
    // 12 s TTL = 3 missed 4 s heartbeats.
    const VOICE_PRESENCE_TTL_MS: u64 = 12_000;
    let mut voice_sweep_tick = tokio::time::interval(Duration::from_secs(4));
    // The sweep arm below is `select!`-gated on `!voice_keys.is_empty()`, so the
    // tick is only polled while at least one voice channel is joined — a node
    // that never uses voice (the common case) pays zero periodic wakeups.
    // `Skip` prevents a catch-up burst of ticks when voice resumes after a long
    // idle gap during which the gated branch wasn't polled.
    voice_sweep_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // ZEB-227 PR #80 review fix: retry buffer for RuntimeActions whose
    // dispatch transiently failed because dm_outbox/crdt_state locks were
    // contended by an in-flight IPC handler. Today only
    // `RuntimeAction::UnicastReceived` requeues here — dropping that
    // packet on contention previously converted ordinary lock pressure
    // into delivery failures with no caller-visible recovery (Reticulum
    // is best-effort, so the upstream sender's CidNotify retransmit
    // takes ~retransmit_interval to drive a redelivery).
    //
    // Capacity 32 caps the very-degraded fan-out (e.g. event-loop
    // wedged behind a long-running IPC) so we don't unbounded-buffer.
    // On full we drop+warn, which is no worse than the prior behavior.
    // Each loop iteration drains AT MOST ONE queued action before
    // entering the select! again; this keeps other arms (timer, UDP,
    // shutdown) responsive even under a steady stream of contended
    // packets.
    let mut runtime_action_retry: std::collections::VecDeque<RuntimeAction> =
        std::collections::VecDeque::with_capacity(32);
    const RUNTIME_ACTION_RETRY_CAP: usize = 32;

    tracing::info!("event loop running");

    loop {
        let mut should_tick = false;

        tokio::select! {
            // ── UDP inbound ──────────────────────────────────────────
            // Intentionally does NOT set should_tick — matches harmony-node.
            // Packets are buffered and processed on the next 250ms timer tick.
            // This ensures tick_count and filter broadcast timers advance at
            // wall-clock rate regardless of packet arrival rate.
            result = udp.recv_from(&mut udp_buf) => {
                match result {
                    Ok((len, _addr)) => {
                        let now = start.elapsed().as_millis() as u64;
                        runtime.push_event(RuntimeEvent::InboundPacket {
                            interface_name: "udp0".to_string(),
                            raw: udp_buf[..len].to_vec(),
                            now,
                        });
                    }
                    Err(e) => {
                        tracing::warn!(err = %e, "UDP recv error");
                    }
                }
            }

            // ── 250ms timer tick ─────────────────────────────────────
            _ = timer.tick() => {
                let now = start.elapsed().as_millis() as u64;
                let unix_now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                runtime.push_event(RuntimeEvent::TimerTick { now, unix_now });
                should_tick = true;

                // ZEB-225 Sub-B Phase 2: drive the dm_outbox drain on every
                // tick. Skipped when no owner identity is loaded.
                //
                // ZEB-233: drain is now lock-lifted — Phase A (lock-held)
                // collects work, Phase B (unlocked) awaits transport.send,
                // Phase C (spawned, lock-held) records outcomes + emits
                // IPC events. Concurrent send_dm IPCs no longer block on
                // the slowest in-flight transport send. The lock-contention
                // try_lock skip behavior is preserved internally by
                // drain_lifted's Phase A.
                if let (Some(outbox), Some(transport), Some(state)) =
                    (dm_outbox.as_ref(), dm_transport.as_ref(), crdt_state.as_ref())
                {
                    let wall_now_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64;
                    crate::dm_outbox::drain_lifted(
                        std::sync::Arc::clone(outbox),
                        std::sync::Arc::clone(state),
                        transport.as_ref(),
                        wall_now_ms,
                        app.clone(),
                    )
                    .await;
                }

                // Refresh direct peer set every 20 timer ticks (~5 seconds).
                // Driven by timer only (not Zenoh events) to avoid excessive
                // peers_zid() calls under high message traffic.
                peer_refresh_counter += 1;
                if peer_refresh_counter.is_multiple_of(20) {
                    direct_peer_zids = session
                        .info()
                        .peers_zid()
                        .await
                        .map(|z| z.to_string())
                        .collect();
                }
            }

            // ── Zenoh events (from spawned tasks) ────────────────────
            Some(event) = zenoh_rx.recv() => {
                match event {
                    ZenohEvent::Query { key_expr, payload } => {
                        runtime.push_event(RuntimeEvent::QueryReceived {
                            query_id: 0,
                            key_expr,
                            payload,
                        });
                    }
                    ZenohEvent::ComputeQuery { key_expr, payload } => {
                        runtime.push_event(RuntimeEvent::ComputeQuery {
                            query_id: 0,
                            key_expr,
                            payload,
                        });
                    }
                    ZenohEvent::Subscription { key_expr, payload, source_zid } => {
                        // Pairing keys are routed to the pairing state machine
                        // (when present) and NOT forwarded to mail/vines/channels
                        // handlers. Pairing samples don't need to drive the
                        // runtime tick, so we `continue` the outer loop to skip
                        // `should_tick` for these.
                        // Hot-path: this branch fires on every Zenoh subscription
                        // sample (community updates, mail, voice, etc.), not just
                        // pairing. The starts_with target must be a `&'static str`
                        // — formatting `format!("{}/", PAIRING_KEY_PREFIX)` would
                        // heap-allocate a fresh `String` every event.
                        if key_expr.starts_with(crate::pairing::PAIRING_KEY_PREFIX_SLASH) {
                            // Note: oversized pairing payloads are dropped at the
                            // producer (the Zenoh subscriber callback for
                            // PAIRING_KEY_GLOB) before they enter zenoh_rx, so
                            // by the time we get here the size cap is guaranteed
                            // to hold. We don't re-check here — Cursor flagged
                            // the duplicate as dead code, and a stale defensive
                            // check is worse than none because it suggests the
                            // invariant is enforced where it isn't.
                            if let Some(tx) = pairing_in_tx.as_ref() {
                                match ciborium::from_reader::<crate::pairing::types::PairingWireMessage, _>(payload.as_slice()) {
                                    Ok(msg) => {
                                        // CRITICAL: must NOT await on a bounded channel here.
                                        // The pairing state machine intentionally does not poll
                                        // its receive end while idle (see state_machine.rs select!
                                        // guard). On an always-on subscription with no consumer,
                                        // `send().await` would block once the buffer fills (~64
                                        // messages of LAN pairing chatter from peer devices),
                                        // stalling the entire node event loop. Use try_send and
                                        // drop on Full — pairing tolerates loss (peers re-emit
                                        // Discover periodically; SAS verification surfaces any
                                        // mid-handshake drop as a state-machine timeout).
                                        if let Err(e) = tx.try_send(msg) {
                                            tracing::warn!(
                                                "pairing channel full or closed, dropping wire \
                                                 message on key {key_expr}: {e}"
                                            );
                                        }
                                    }
                                    Err(e) => tracing::warn!("invalid pairing wire message on key {key_expr}: {e}"),
                                }
                            }
                            continue;
                        }
                        let hop_distance = source_zid.as_ref().map(|zid| {
                            if direct_peer_zids.contains(zid) { 1u8 } else { 2u8 }
                        });
                        emit_frontend_event(
                            &app,
                            &key_expr,
                            &payload,
                            hop_distance,
                            &followed_set,
                            &vine_feed_cache,
                            &mail_mgr,
                            &own_mail_key,
                            &own_root_key,
                            mail_sync.as_ref(),
                        );
                        runtime.push_event(RuntimeEvent::SubscriptionMessage {
                            key_expr,
                            payload,
                        });
                    }
                    ZenohEvent::FetchResponse { cid, is_module, result } => {
                        if is_module {
                            runtime.push_event(RuntimeEvent::ModuleFetchResponse {
                                cid,
                                result,
                            });
                        } else {
                            runtime.push_event(RuntimeEvent::ContentFetchResponse {
                                cid,
                                result,
                            });
                        }
                    }
                }
                should_tick = true;
            }

            // ── Publish requests from Tauri commands ─────────────────
            Some(req) = publish_rx.recv() => {
                let result = session
                    .put(&req.key_expr, req.payload)
                    .await
                    .map_err(|e| format!("put failed: {e}"));
                let _ = req.reply.send(result);
            }

            // ── Content-fetch requests from Tauri commands ──────────
            Some(req) = fetch_rx.recv() => {
                let session = session.clone();
                let cid_hex = req.cid_hex;
                // ZEB-155: clone the completion sender so the spawned
                // task can notify the main loop after a successful fetch.
                let completion_tx = fetch_completion_tx.clone();
                // ZEB-159: clone cas_op_tx so the wrapped fetch_one can
                // admit each fetched CID's bytes to the StorageTier
                // cache synchronously (round-tripping through a
                // PutLocal reply oneshot per CID). Without admission
                // ordered before the completion signal, the ZEB-155
                // fetch-completion arm races the PutLocal arm and
                // walks a partial cache for freshly-fetched roots
                // (Cursor + Qodo R1).
                let cas_op_tx_for_fetch = cas_op_tx.clone();
                tokio::spawn(async move {
                    // Parse hex → 32-byte CID. Reply with an error if malformed.
                    let cid_bytes = match hex::decode(&cid_hex)
                        .ok()
                        .and_then(|b| <[u8; 32]>::try_from(b).ok())
                    {
                        Some(b) => b,
                        None => {
                            let _ = req.reply.send(Err(format!("invalid CID hex: {cid_hex}")));
                            return;
                        }
                    };
                    let root = ContentId::from_bytes(cid_bytes);

                    // Closure that does one Zenoh GET for a single CID.
                    let fetch_one = move |cid: ContentId| {
                        let session = session.clone();
                        async move {
                            let cid_hex = hex::encode(cid.to_bytes());
                            let prefix = cid_hex.get(1..2).unwrap_or("");
                            let key = format!("harmony/content/{prefix}/{cid_hex}");
                            fetch_via_zenoh(&session, &key).await
                        }
                    };
                    // ZEB-159: wrap fetch_one so each successful fetch
                    // also admits the bytes to the local cache. The
                    // wrapper sends CasOp::PutLocal { reply: Some(...) }
                    // per CID and awaits the reply, so by the time
                    // fetch_recursive returns Ok, every admission has
                    // been processed by the event-loop's PutLocal arm
                    // (which calls runtime.tick() before signaling).
                    // This synchronous round-trip is load-bearing for
                    // ordering: the fetch_completion_tx signal below
                    // depends on the cache being populated, so a
                    // fire-and-forget admit (as GetOrFetch uses at
                    // event_loop.rs:1625) would race the completion
                    // arm and walk a partial cache (Cursor + Qodo R1).
                    let fetch_one_with_admit =
                        wrap_fetch_one_with_admission(fetch_one, cas_op_tx_for_fetch);

                    let result = fetch_recursive(fetch_one_with_admit, root).await;
                    // ZEB-155: reply to the fetch caller FIRST so a full
                    // completion channel never delays the fetch reply.
                    // Then best-effort-notify via try_send. If the
                    // completion channel is full (rare — main loop drain
                    // is O(1) per select pass), we lose this chance to
                    // auto-repin; the next user action or next start_node
                    // reconverges. try_send also returns Err on closed,
                    // which is fine (event loop shutting down).
                    let is_ok = result.is_ok();
                    let _ = req.reply.send(result);
                    if is_ok {
                        let _ = completion_tx.try_send(cid_bytes);
                    }
                });
            }

            // ── Manual mail refresh from MailSync::refresh_now ──────
            Some(reply_tx) = refresh_rx.recv() => {
                if own_root_key.is_empty() {
                    let _ = reply_tx.send(Err("own_root_key unavailable".to_string()));
                } else {
                    let session_clone = session.clone();
                    let key = own_root_key.clone();
                    tokio::spawn(async move {
                        let result = query_mail_root(&session_clone, &key, "refresh").await;
                        let _ = reply_tx.send(result);
                    });
                }
            }

            // ── Content-ingest requests from Tauri commands ────────
            Some(req) = ingest_rx.recv() => {
                // Validate the CID hex decodes to exactly 32 bytes — this is
                // the only precondition for parse_subscription_event to route
                // the message into StorageTierEvent::PublishContent.
                let cid_ok = hex::decode(&req.cid_hex)
                    .ok()
                    .and_then(|b| <[u8; 32]>::try_from(b).ok())
                    .is_some();
                if !cid_ok {
                    let _ = req.reply.send(Err(format!("invalid CID hex: {}", req.cid_hex)));
                } else {
                    let key_expr = format!("harmony/content/publish/{}", req.cid_hex);
                    runtime.push_event(RuntimeEvent::SubscriptionMessage {
                        key_expr,
                        payload: req.data,
                    });
                    // Tick immediately so content is committed before replying.
                    for action in runtime.tick() {
                        handle_runtime_action_or_dispatch(
                            action, &session, &zenoh_tx, &udp,
                            &broadcast_addr, &app, &closing, &own_zid,
                            dm_outbox.as_ref(), crdt_state.as_ref(),
                            cas_handle.as_ref(), unicast_send_tx.as_ref(),
                            community_registry.as_ref(),
                            &mut runtime_action_retry, RUNTIME_ACTION_RETRY_CAP,
                        ).await;
                    }
                    let _ = req.reply.send(Ok(()));
                }
            }

            // ── Phase 3b: CAS operations from SyncEngine ────────────
            // PutLocal admits ciphertext to the local cache via the
            // existing StorageTier ingest path (parity with ingest_rx).
            // GetOrFetch checks cache; on miss spawns a Zenoh GET wrapped
            // in tokio::time::timeout and uses a second-mpsc-hop back
            // through cas_op_tx to admit fetched bytes before replying.
            // See spec §"Event loop handler" and §"Re-entry".
            Some(op) = cas_op_rx.recv() => {
                use crate::content_store::CasOp;
                match op {
                    CasOp::PutLocal { cid, blob, reply } => {
                        let cid_hex = hex::encode(cid.to_bytes());
                        let key_expr = format!("harmony/content/publish/{cid_hex}");
                        runtime.push_event(RuntimeEvent::SubscriptionMessage {
                            key_expr,
                            payload: blob,
                        });
                        for action in runtime.tick() {
                            handle_runtime_action_or_dispatch(
                                action, &session, &zenoh_tx, &udp,
                                &broadcast_addr, &app, &closing, &own_zid,
                                dm_outbox.as_ref(), crdt_state.as_ref(),
                                cas_handle.as_ref(), unicast_send_tx.as_ref(),
                                community_registry.as_ref(),
                                &mut runtime_action_retry, RUNTIME_ACTION_RETRY_CAP,
                            ).await;
                        }
                        // We do NOT inspect tick() actions for a "rejected"
                        // signal — StorageTier silently drops corrupted
                        // bytes (parity with ingest_rx pattern). A subsequent
                        // GetOrFetch on a corrupted CID hits a real cache
                        // miss and re-fetches over Zenoh, where harmony-
                        // content's transport-side hash check provides
                        // integrity. See plan §"Pre-flight: admit-rejection
                        // signal".
                        // Reply only if a Sender was provided — fire-and-forget
                        // callers (spawned-fetch admit hop) pass None.
                        if let Some(reply) = reply {
                            let _ = reply.send(Ok(()));
                        }
                    }
                    CasOp::GetLocal { cid, reply } => {
                        // Read-only: pull from the in-memory StorageTier cache
                        // without any network fetch. Mirrors the fast-path
                        // cache check in GetOrFetch (event_loop.rs:2018) but
                        // never spawns a Zenoh GET on a miss.
                        let bytes = runtime
                            .storage_tier()
                            .cache()
                            .get(&cid)
                            .map(|b| b.to_vec());
                        let _ = reply.send(bytes);
                    }
                    CasOp::GetOrFetch { cid, timeout, reply } => {
                        // 1. Cache check first (fast path).
                        if let Some(bytes) = runtime.storage_tier().cache().get(&cid).map(|b| b.to_vec()) {
                            let _ = reply.send(Ok(Some(bytes)));
                        } else {
                            // 2. Cache miss — spawn the Zenoh GET wrapped in
                            //    tokio::time::timeout. Spawning avoids holding
                            //    the select arm during the network I/O.
                            let cid_hex = hex::encode(cid.to_bytes());
                            // Always Some: cid.to_bytes() is [u8; 32], so cid_hex is
                            // exactly 64 chars. The unwrap_or("") fallback is
                            // defensive but unreachable in practice; the empty
                            // string would produce a malformed double-slash
                            // key, so no graceful-degradation guarantee.
                            let prefix = cid_hex.get(1..2).unwrap_or("").to_string();
                            let key = format!("harmony/content/{prefix}/{cid_hex}");
                            let session_clone = session.clone();
                            let cas_op_tx_for_admit = cas_op_tx.clone();
                            tokio::spawn(async move {
                                let fetch = fetch_via_zenoh(&session_clone, &key);
                                match tokio::time::timeout(timeout, fetch).await {
                                    Ok(Ok(bytes)) => {
                                        // ZEB-343 verify-on-fetch (spec §5.3):
                                        // mirror the wrap_fetch_one_with_admission
                                        // gate (T4). A Zenoh reply is untrusted —
                                        // verify hash==cid BEFORE admitting or
                                        // returning. On failure, do NOT admit and
                                        // reply Ok(None): treat a tampered reply as
                                        // a cache miss so the caller falls through
                                        // exactly as it would on a timeout (the
                                        // CRDT carries recovery).
                                        if !cid.verify_hash(&bytes) {
                                            tracing::warn!(
                                                cid = %cid_hex,
                                                "GetOrFetch: fetched bytes failed hash==cid; \
                                                 treating as cache miss (not admitting)"
                                            );
                                            let _ = reply.send(Ok(None));
                                            return;
                                        }
                                        // 3. Best-effort admit via try_send.
                                        //    We have the bytes for the caller
                                        //    regardless of whether caching
                                        //    succeeds — admit is fire-and-forget
                                        //    so network-fetch latency isn't
                                        //    blocked on local cache contention
                                        //    or event-loop progress. If the
                                        //    cas_op channel is full or closed,
                                        //    caching is skipped; the next
                                        //    GetOrFetch on this CID will
                                        //    re-fetch over the network.
                                        //    bytes.clone() is load-bearing —
                                        //    PutLocal.blob consumes the bytes,
                                        //    but the caller's reply still needs
                                        //    them.
                                        //    reply: None signals fire-and-forget
                                        //    intent — the PutLocal handler skips
                                        //    its reply.send when reply is None,
                                        //    avoiding wasted work on a dropped
                                        //    oneshot receiver.
                                        let _ = cas_op_tx_for_admit.try_send(crate::content_store::CasOp::PutLocal {
                                            cid,
                                            blob: bytes.clone(),
                                            reply: None,
                                        });
                                        let _ = reply.send(Ok(Some(bytes)));
                                    }
                                    Ok(Err(e)) => {
                                        let _ = reply.send(Err(crate::content_store::ContentStoreError::Io(
                                            format!("fetch '{key}': {e}"),
                                        )));
                                    }
                                    // Timeout → Ok(None) (CRDT carries recovery).
                                    Err(_) => {
                                        let _ = reply.send(Ok(None));
                                    }
                                }
                            });
                        }
                    }
                }
            }

            // ── ZEB-227 Path B: outbound DM unicast → NodeRuntime ────────────
            // RuntimeUnicastTransport (Task 6, dispatched per send_dm) pushes one
            // UnicastSendRequest per recipient device hash into this channel; we
            // forward each as a RuntimeEvent::SendUnicastToDevice into NodeRuntime,
            // which queues into pending_unicast_sends and resolves on next tick
            // against the path table (per ZEB-226's defer-then-drop semantics).
            //
            // The arm is gated by Optional rx — None is the inactive shape (no
            // owner identity loaded), and the std::future::pending() shim makes
            // the arm effectively skipped without polling overhead.
            Some(req) = async {
                match unicast_send_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                runtime.push_event(RuntimeEvent::SendUnicastToDevice {
                    destination_hash: req.destination_hash,
                    packet: req.packet,
                });
                should_tick = true;
            }

            // ── Content-verb requests (pin/unpin/burn/snapshot) ────
            Some(req) = content_verb_rx.recv() => {
                use harmony_content::cid::ContentId;
                match req {
                    ContentVerbRequest::Pin { cid, reply } => {
                        // ZEB-155: record intent in the event-loop cache so
                        // fetch-completion can auto-repin after a resurrect.
                        //
                        // This may contain CIDs not in the sidecar (e.g. a
                        // pin on a cached DM attachment for which no
                        // sidecar entry exists). That drift self-heals on
                        // the next start_node, which rebuilds pin_intent
                        // from the sidecar — sidecar remains authoritative.
                        pin_intent.insert(cid);
                        let root = ContentId::from_bytes(cid);
                        let all = collect_descendants(runtime.storage_tier().cache(), root);
                        let mut any_failed = false;
                        for id in all {
                            if !runtime.pin_content(id) {
                                any_failed = true;
                            }
                        }
                        let _ = reply.send(Ok(!any_failed));
                    }
                    ContentVerbRequest::Unpin { cid, reply } => {
                        // ZEB-155: clear intent so a later fetch doesn't re-pin.
                        pin_intent.remove(&cid);
                        let root = ContentId::from_bytes(cid);
                        let doomed = collect_descendants(runtime.storage_tier().cache(), root);

                        // ZEB-156: any CID reachable from a still-pinned root
                        // must stay pinned even when it sits in `doomed`. See
                        // `compute_keep_set` for the cross-cutting rationale.
                        let keep = compute_keep_set(
                            runtime.storage_tier().cache(),
                            &pin_intent,
                            doomed.len(),
                        );

                        for id in doomed {
                            if !keep.contains(&id) {
                                runtime.unpin_content(&id);
                            }
                        }
                        let _ = reply.send(Ok(true));
                    }
                    ContentVerbRequest::Burn { cid, reply } => {
                        // Burn on a RAM-only client cascades the runtime-side
                        // unpin; the sidecar-removal side of burn continues to
                        // happen in the Tauri command handler.
                        // ZEB-155: also drop any persisted intent (the Tauri
                        // command removes the sidecar entry, but this keeps
                        // the in-memory set consistent if the orders diverge).
                        pin_intent.remove(&cid);
                        let root = ContentId::from_bytes(cid);
                        let doomed = collect_descendants(runtime.storage_tier().cache(), root);

                        // ZEB-156: same keep-set logic as Unpin — burning one
                        // root must not destroy bytes another pinned root
                        // still relies on. See `compute_keep_set` for the
                        // shared-subtree case the Tauri OR-join misses.
                        let keep = compute_keep_set(
                            runtime.storage_tier().cache(),
                            &pin_intent,
                            doomed.len(),
                        );

                        for id in doomed {
                            if !keep.contains(&id) {
                                runtime.unpin_content(&id);
                                // Burn-specific: also evict the bytes from
                                // the cache immediately rather than waiting
                                // for W-TinyLFU pressure. This is the
                                // "no really, destroy this" semantic.
                                // `None` means the CID was not in the cache
                                // (e.g. already evicted, never admitted) —
                                // harmless.
                                let _ = runtime.remove_content(&id);
                            }
                        }
                        let _ = reply.send(Ok(true));
                    }
                    ContentVerbRequest::PinnedSet { reply } => {
                        let cache = runtime.storage_tier().cache();
                        let pinned: std::collections::HashSet<[u8; 32]> = cache
                            .iter_admitted()
                            .filter(|id| cache.is_pinned(id))
                            .map(|id| id.to_bytes())
                            .collect();
                        let _ = reply.send(pinned);
                    }
                    ContentVerbRequest::ReadBytes { cid, reply } => {
                        let id = ContentId::from_bytes(cid);
                        let bytes = runtime.storage_tier().cache().get(&id).map(|b| b.to_vec());
                        let _ = reply.send(bytes);
                    }
                }
            }

            // ── Fetch-completion replay hook (ZEB-155 + ZEB-159) ──
            // Spawned fetch tasks send on fetch_completion_tx after
            // fetch_recursive returns Ok. The spawned task admits every
            // fetched CID's bytes via synchronous CasOp::PutLocal hops
            // (ZEB-159) — each per-CID admission awaits its reply
            // oneshot before fetch_recursive proceeds, and the
            // CasOp::PutLocal handler ticks the runtime BEFORE sending
            // the reply, so by the time this arm runs, the bundle tree
            // is in the cache. If pin_intent contains the root, walk
            // all descendants currently in the cache and pin them.
            // This re-engages runtime-side eviction protection that
            // was lost when the previous node stopped and its
            // in-memory pinned-set went with it.
            Some(root_bytes) = fetch_completion_rx.recv() => {
                if pin_intent.contains(&root_bytes) {
                    let root = ContentId::from_bytes(root_bytes);
                    let all = collect_descendants(runtime.storage_tier().cache(), root);
                    for id in all {
                        runtime.pin_content(id);
                    }
                }
            }

            // Follow/unfollow updates are applied to followed_set directly
            // by the Tauri command handlers. When per-creator Zenoh
            // subscriptions are added (once the publish path includes
            // /announce/), the follow_rx channel will drive Subscribe/
            // Unsubscribe actions here.
            Some(_req) = follow_rx.recv() => {}

            // ── Voice frame relay (frontend → Zenoh) ────────────────
            // Await directly instead of spawning per-frame tasks — preserves
            // ordering and applies natural backpressure from Zenoh.
            Some(voice) = voice_rx.recv() => {
                match voice {
                    crate::voice::VoiceOutbound::Channel { community_id, channel_id, frame } => {
                        // ZEB-350: seal the whole frame (header + payload) under the
                        // per-(community, channel) ChannelKey, then publish to an
                        // own-device-named topic segment. Sender routing now lives in
                        // the topic, not the cleartext header.
                        if let Some(key) = voice_keys.get(&(community_id, channel_id)) {
                            let own = voice_own_device.as_deref().unwrap_or("self");
                            match crate::voice_crypto::encrypt_voice_packet(
                                key,
                                &community_id,
                                &channel_id,
                                crate::voice_crypto::VOICE_PACKET_AAD,
                                &frame,
                            ) {
                                Ok(sealed) => {
                                    let key_expr = format!(
                                        "harmony/voice/{}/{}/{}",
                                        hex::encode(community_id.0),
                                        hex::encode(channel_id.0),
                                        own,
                                    );
                                    if let Err(e) = session.put(&key_expr, sealed).await {
                                        tracing::warn!(%key_expr, err = %e, "voice publish failed");
                                    }
                                }
                                Err(e) => tracing::warn!(err = %e, "voice seal failed; dropping frame"),
                            }
                        }
                        // else: not joined to that (community, channel) — drop.
                    }
                    // ZEB-352: DM call outbound — seal under the per-call K_voice and
                    // publish to harmony/voice/dm/{callId}/{own}.
                    crate::voice::VoiceOutbound::Dm { call_id, frame } => {
                        // Server-side mute enforcement: SetDmCallMuted flips this
                        // flag; honor it here so a muted call never emits sealed
                        // audio even if the frontend gate misbehaves (defense in
                        // depth — the talk-gate also withholds frames). The flag
                        // starts `true` (start-muted, D10) until the user unmutes;
                        // a MISSING entry defaults to muted too, so a frame racing
                        // JoinDmCall can never leak audio (start-muted semantics).
                        let muted = dm_voice_mute_flags
                            .get(&call_id)
                            .map(|f| f.load(std::sync::atomic::Ordering::SeqCst))
                            .unwrap_or(true);
                        if muted {
                            continue;
                        }
                        if let Some(key) = dm_voice_keys.get(&call_id) {
                            let own = voice_own_device.as_deref().unwrap_or("self");
                            match crate::voice_crypto::encrypt_dm_voice_packet(
                                key, &call_id, crate::voice_crypto::VOICE_DM_PACKET_AAD, &frame,
                            ) {
                                Ok(sealed) => {
                                    let key_expr = format!("harmony/voice/dm/{}/{}", hex::encode(call_id), own);
                                    if let Err(e) = session.put(&key_expr, sealed).await {
                                        tracing::warn!(%key_expr, err = %e, "dm voice publish failed");
                                    }
                                }
                                Err(e) => tracing::warn!(err = %e, "dm voice seal failed; dropping frame"),
                            }
                        }
                    }
                }
            }

            // ── Voice channel join/leave ────────────────────────────
            Some(req) = voice_channel_rx.recv() => {
                match req {
                    crate::voice::VoiceChannelRequest::Join { community_id, channel_id, caps } => {
                        let sub_key = format!(
                            "harmony/voice/{}/{}/*",
                            hex::encode(community_id.0),
                            hex::encode(channel_id.0),
                        );
                        // Declare the media subscriber FIRST: all state mutation
                        // (cached key, own-device id, presence pub/sub) only
                        // happens after this succeeds, so a subscribe failure
                        // can't leave outbound sealing with a key that has no
                        // matching inbound task (split-brain). On failure we
                        // leave any prior state for this (community, channel)
                        // intact — old subscriber keeps running with the old key.
                        let key_for_sub = std::sync::Arc::clone(&caps.channel_key);
                        let (c_sub, ch_sub) = (community_id, channel_id);
                        let app_sub = app.clone();
                        let closing_sub = closing.clone();
                        // Captured for the retry loop: re-declare the subscriber
                        // off the shared session, and tag transport-lost/restored
                        // events so the frontend can filter by active channel.
                        let session_for_media = Arc::clone(&session_arc);
                        let sub_key_retry = sub_key.clone();
                        let community_hex = hex::encode(community_id.0);
                        let channel_hex = hex::encode(channel_id.0);
                        match session.declare_subscriber(&sub_key).await {
                            Ok(sub) => {
                                // The subscriber opens (decrypts) before emitting;
                                // an AEAD failure (non-member / stale epoch /
                                // tamper) is dropped silently.
                                //
                                // ZEB-353: on a transport drop the inner receive
                                // loop ends; instead of returning we re-declare
                                // the subscriber with exponential backoff (mirror
                                // the voice-signal sub), emitting transport-lost
                                // on drop and transport-restored on re-declare so
                                // the frontend can show "Reconnecting…".
                                let handle = tokio::spawn(async move {
                                    let mut sub = sub;
                                    let mut backoff = std::time::Duration::from_secs(5);
                                    const MAX_BACKOFF: std::time::Duration =
                                        std::time::Duration::from_secs(60);
                                    loop {
                                        // Track whether this subscription ever
                                        // carried a frame. Only a connection that
                                        // made real progress is allowed to reset the
                                        // backoff; one that declares OK but never
                                        // receives is flapping and must rate-limit.
                                        let mut made_progress = false;
                                        while let Ok(sample) = sub.recv_async().await {
                                            made_progress = true;
                                            if sample.payload().len() > crate::voice_crypto::MAX_VOICE_PACKET_BYTES {
                                                tracing::warn!(
                                                    len = sample.payload().len(),
                                                    max = crate::voice_crypto::MAX_VOICE_PACKET_BYTES,
                                                    "oversized voice packet dropped"
                                                );
                                                continue;
                                            }
                                            let sealed = sample.payload().to_bytes().to_vec();
                                            match crate::voice_crypto::decrypt_voice_packet(
                                                &key_for_sub,
                                                &c_sub,
                                                &ch_sub,
                                                crate::voice_crypto::VOICE_PACKET_AAD,
                                                &sealed,
                                            ) {
                                                Ok(frame) => {
                                                    let _ = app_sub.emit(
                                                        "voice-frame-received",
                                                        serde_json::json!({ "frameBytes": frame }),
                                                    );
                                                }
                                                Err(_) => { /* non-member / stale epoch / tamper → drop silently */ }
                                            }
                                        }
                                        // Inner receive loop ended on a transport
                                        // error. Stop if we're shutting down.
                                        if closing_sub.load(std::sync::atomic::Ordering::SeqCst) {
                                            break;
                                        }
                                        // Reset/grow the backoff based on real
                                        // progress, not mere re-subscription. A
                                        // connection that carried frames then dropped
                                        // is a healthy blip → reset to the initial 5s
                                        // and re-declare immediately. A connection
                                        // that declared OK but never received a frame
                                        // is flapping → sleep the current backoff and
                                        // grow it (5s → … → 60s cap) so the
                                        // lost/restored event rate stays bounded
                                        // instead of spinning. The `closing` re-check
                                        // inside the re-declare loop still owns
                                        // shutdown during the sleep.
                                        if made_progress {
                                            backoff = std::time::Duration::from_secs(5);
                                        } else {
                                            tokio::time::sleep(backoff).await;
                                            backoff = std::cmp::min(backoff * 2, MAX_BACKOFF);
                                        }
                                        tracing::warn!(
                                            key = %sub_key_retry,
                                            "voice subscriber closed unexpectedly; reconnecting"
                                        );
                                        let _ = app_sub.emit(
                                            "voice-transport-lost",
                                            serde_json::json!({
                                                "communityId": community_hex,
                                                "channelId": channel_hex,
                                            }),
                                        );
                                        // Re-declare the subscriber. Like the
                                        // voice-signal sub, try immediately on the
                                        // first attempt and only back off *after* a
                                        // failed re-declare, so a momentary blip
                                        // recovers without waiting out the backoff.
                                        loop {
                                            if closing_sub.load(std::sync::atomic::Ordering::SeqCst) {
                                                return;
                                            }
                                            match session_for_media
                                                .declare_subscriber(&sub_key_retry)
                                                .await
                                            {
                                                Ok(s) => {
                                                    // Backoff reset is owned by the
                                                    // made_progress logic above, not
                                                    // by mere re-subscription.
                                                    sub = s;
                                                    let _ = app_sub.emit(
                                                        "voice-transport-restored",
                                                        serde_json::json!({
                                                            "communityId": community_hex,
                                                            "channelId": channel_hex,
                                                        }),
                                                    );
                                                    break;
                                                }
                                                Err(e) => {
                                                    tracing::warn!(
                                                        key = %sub_key_retry,
                                                        error = %e,
                                                        backoff_s = backoff.as_secs(),
                                                        "voice subscriber re-declare failed; retrying"
                                                    );
                                                    tokio::time::sleep(backoff).await;
                                                    backoff =
                                                        std::cmp::min(backoff * 2, MAX_BACKOFF);
                                                }
                                            }
                                        }
                                    }
                                });
                                if let Some(old) = voice_subs.insert((community_id, channel_id), handle) {
                                    old.abort();
                                }
                                // Subscriber is live — now safe to cache the
                                // channel key (seals/opens media + beacons) and
                                // the node's own device id (names the outbound
                                // topic).
                                voice_keys.insert(
                                    (community_id, channel_id),
                                    std::sync::Arc::clone(&caps.channel_key),
                                );
                                if voice_own_device.is_none() {
                                    voice_own_device = Some(hex::encode(caps.self_device));
                                }
                                // ZEB-350: presence. Membership verification
                                // needs the community registry — guard the whole
                                // presence leg behind it (None = no verifier, skip).
                                if let Some(registry) = community_registry.clone() {
                                    let pres_topic = format!(
                                        "harmony/voice-presence/{}/{}",
                                        hex::encode(community_id.0),
                                        hex::encode(channel_id.0),
                                    );
                                    // Stash identity (+ signing key) so Leave can
                                    // mint and sign the tombstone.
                                    voice_identity.insert(
                                        (community_id, channel_id),
                                        (
                                            caps.self_owner,
                                            caps.self_device,
                                            caps.joined_hlc.clone(),
                                            std::sync::Arc::clone(&caps.signing_key),
                                        ),
                                    );
                                    let pres_sub = crate::voice_presence::spawn_voice_presence_subscriber(
                                        session.clone(),
                                        pres_topic.clone(),
                                        std::sync::Arc::clone(&caps.channel_key),
                                        community_id,
                                        channel_id,
                                        registry,
                                        std::sync::Arc::clone(&voice_presence_map),
                                        app.clone(),
                                        closing.clone(),
                                        std::sync::Arc::clone(&voice_now_ms),
                                    );
                                    // ZEB-351 Voice V3: the publisher now reads a
                                    // shared mute flag instead of hardcoding
                                    // `muted: true`. Start muted (V3 join flow is
                                    // start-muted); `SetMuted` flips it later.
                                    let mute_flag = std::sync::Arc::new(
                                        std::sync::atomic::AtomicBool::new(true),
                                    );
                                    voice_mute_flags.insert(
                                        (community_id, channel_id),
                                        std::sync::Arc::clone(&mute_flag),
                                    );
                                    // Shared monotone beacon `seq` source for both
                                    // the publisher loop and the `SetMuted`
                                    // immediate beacon (see voice_presence_seq).
                                    let seq_counter = std::sync::Arc::new(
                                        std::sync::atomic::AtomicU64::new(0),
                                    );
                                    voice_presence_seq.insert(
                                        (community_id, channel_id),
                                        std::sync::Arc::clone(&seq_counter),
                                    );
                                    let pubh = crate::voice_presence::spawn_voice_presence_publisher(
                                        session.clone(),
                                        pres_topic,
                                        std::sync::Arc::clone(&caps.channel_key),
                                        community_id,
                                        channel_id,
                                        std::sync::Arc::clone(&caps.signing_key),
                                        caps.self_owner,
                                        caps.self_device,
                                        caps.joined_hlc.clone(),
                                        mute_flag,
                                        seq_counter,
                                        Duration::from_secs(4),
                                        closing.clone(),
                                    );
                                    if let Some(h) = voice_presence_subs.insert((community_id, channel_id), pres_sub) {
                                        h.abort();
                                    }
                                    if let Some(h) = voice_presence_pubs.insert((community_id, channel_id), pubh) {
                                        h.abort();
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::error!(
                                    %sub_key,
                                    err = %e,
                                    "voice subscribe failed; join not established"
                                );
                            }
                        }
                    }
                    crate::voice::VoiceChannelRequest::Leave { community_id, channel_id } => {
                        // ZEB-350: stop the presence pub/sub, then send a
                        // best-effort `left` tombstone so peers drop us instantly
                        // (don't wait out the 12 s eviction).
                        if let Some(h) = voice_presence_pubs.remove(&(community_id, channel_id)) {
                            h.abort();
                        }
                        if let Some(h) = voice_presence_subs.remove(&(community_id, channel_id)) {
                            h.abort();
                        }
                        if let (Some(key), Some((owner, device, joined_hlc, signing_key))) = (
                            voice_keys.get(&(community_id, channel_id)),
                            voice_identity.remove(&(community_id, channel_id)),
                        ) {
                            if let Some(tombstone) = crate::voice_presence::build_presence_tombstone(
                                key,
                                &community_id,
                                &channel_id,
                                &signing_key,
                                owner,
                                device,
                                joined_hlc,
                            ) {
                                let pres_topic = format!(
                                    "harmony/voice-presence/{}/{}",
                                    hex::encode(community_id.0),
                                    hex::encode(channel_id.0),
                                );
                                if let Err(e) = session.put(&pres_topic, tombstone).await {
                                    tracing::warn!(%pres_topic, err = %e, "presence tombstone publish failed");
                                }
                            }
                        }
                        // ZEB-351 Voice V3: drop the shared mute flag in lockstep
                        // with the media key so a later `SetMuted` for a left
                        // channel is a no-op.
                        voice_mute_flags.remove(&(community_id, channel_id));
                        voice_presence_seq.remove(&(community_id, channel_id));
                        // Media leg: drop the cached key + abort the sub.
                        voice_keys.remove(&(community_id, channel_id));
                        if let Some(handle) = voice_subs.remove(&(community_id, channel_id)) {
                            handle.abort();
                        }
                        // Clear our local roster for this channel so the periodic
                        // sweep stops emitting `voice-presence-changed` for a
                        // channel we've left (final-review fix).
                        {
                            let mut g = voice_presence_map.lock().await;
                            g.remove_channel(&community_id, &channel_id);
                        }
                        // After remove_channel the roster for this channel is
                        // empty, so emit the now-empty roster once. Without
                        // this, a UI listening on `voice-presence-changed`
                        // keeps showing participants for the channel we left
                        // until some unrelated update fires (final-review fix).
                        let _ = app.emit(
                            "voice-presence-changed",
                            serde_json::json!({
                                "community": hex::encode(community_id.0),
                                "channel": hex::encode(channel_id.0),
                                "roster": Vec::<crate::voice_presence::RosterEntry>::new(),
                            }),
                        );
                    }
                    crate::voice::VoiceChannelRequest::SetMuted { community_id, channel_id, muted } => {
                        // ZEB-351 Voice V3: flip the shared mute flag the presence
                        // publisher reads each heartbeat. A no-op if the channel
                        // isn't joined (flag absent).
                        if let Some(flag) = voice_mute_flags.get(&(community_id, channel_id)) {
                            flag.store(muted, std::sync::atomic::Ordering::SeqCst);
                            // Immediate beacon so the roster reflects the new mute
                            // state without waiting out the next ≤4 s heartbeat.
                            // Best-effort: needs the channel key (voice_keys), the
                            // signing key + identity (voice_identity, stashed on
                            // Join), and the shared `seq` counter (voice_presence_seq).
                            // Drawing `seq` from that shared counter via `fetch_add`
                            // keeps it strictly between surrounding heartbeats — so
                            // this one-shot wins same-session freshness over OLDER
                            // beacons yet never outranks LATER heartbeats (the bug
                            // the old `u64::MAX - 1` sentinel caused). `u64::MAX`
                            // stays reserved for the leave tombstone.
                            if let (Some(key), Some((owner, device, joined_hlc, signing_key)), Some(seq_counter)) = (
                                voice_keys.get(&(community_id, channel_id)),
                                voice_identity.get(&(community_id, channel_id)),
                                voice_presence_seq.get(&(community_id, channel_id)),
                            ) {
                                let pres_topic = format!(
                                    "harmony/voice-presence/{}/{}",
                                    hex::encode(community_id.0),
                                    hex::encode(channel_id.0),
                                );
                                let seq = seq_counter
                                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                if let Err(e) = crate::voice_presence::publish_presence_once(
                                    &session,
                                    &pres_topic,
                                    key,
                                    &community_id,
                                    &channel_id,
                                    signing_key,
                                    *owner,
                                    *device,
                                    joined_hlc,
                                    seq,
                                    muted,
                                )
                                .await
                                {
                                    tracing::warn!(%pres_topic, err = ?e, "immediate mute beacon publish failed");
                                }
                            }
                        }
                    }
                    // ── ZEB-352: DM call lifecycle ────────────────────────
                    crate::voice::VoiceChannelRequest::JoinDmCall { call_id, caps } => {
                        let sub_key = format!("harmony/voice/dm/{}/*", hex::encode(call_id));
                        let key_for_sub = std::sync::Arc::clone(&caps.channel_key);
                        let app_sub = app.clone();
                        let closing_sub = closing.clone();
                        let call_hex = hex::encode(call_id);
                        let own_device_hex = hex::encode(caps.self_device);
                        if voice_own_device.is_none() {
                            voice_own_device = Some(own_device_hex.clone());
                        }
                        // Captured for the retry loop: re-declare the subscriber
                        // off the shared session, and tag transport-lost/restored
                        // events so the frontend can filter by active call.
                        let session_for_media = Arc::clone(&session_arc);
                        let sub_key_retry = sub_key.clone();
                        match session.declare_subscriber(&sub_key).await {
                            Ok(sub) => {
                                // ZEB-353: on a transport drop the inner receive
                                // loop ends; instead of returning we re-declare the
                                // subscriber with exponential backoff (mirror the
                                // voice-signal sub), emitting transport-lost on drop
                                // and transport-restored on re-declare so the
                                // frontend can show "Reconnecting…".
                                let handle = tokio::spawn(async move {
                                    let mut sub = sub;
                                    let mut backoff = std::time::Duration::from_secs(5);
                                    const MAX_BACKOFF: std::time::Duration =
                                        std::time::Duration::from_secs(60);
                                    loop {
                                        // Track whether this subscription ever
                                        // carried a frame. Only a connection that
                                        // made real progress is allowed to reset the
                                        // backoff; one that declares OK but never
                                        // receives is flapping and must rate-limit.
                                        let mut made_progress = false;
                                        while let Ok(sample) = sub.recv_async().await {
                                            made_progress = true;
                                            // Skip our own published frames: on a 2-party
                                            // DM the `.../*` sub also matches our own
                                            // {senderDevice} segment, and decrypting +
                                            // emitting them just to have the frontend drop
                                            // them by sender hash wastes CPU on the audio
                                            // hot path (self is ~half of DM traffic).
                                            if sample.key_expr().as_str().rsplit('/').next()
                                                == Some(own_device_hex.as_str())
                                            {
                                                continue;
                                            }
                                            if sample.payload().len() > crate::voice_crypto::MAX_VOICE_PACKET_BYTES {
                                                continue;
                                            }
                                            let sealed = sample.payload().to_bytes().to_vec();
                                            if let Ok(frame) = crate::voice_crypto::decrypt_dm_voice_packet(
                                                &key_for_sub,
                                                &call_id,
                                                crate::voice_crypto::VOICE_DM_PACKET_AAD,
                                                &sealed,
                                            ) {
                                                let _ = app_sub.emit(
                                                    "dm-voice-frame-received",
                                                    serde_json::json!({
                                                        "callId": call_hex,
                                                        "frameBytes": frame,
                                                    }),
                                                );
                                            }
                                        }
                                        // Inner receive loop ended on a transport
                                        // error. Stop if we're shutting down.
                                        if closing_sub.load(std::sync::atomic::Ordering::SeqCst) {
                                            break;
                                        }
                                        // Reset/grow the backoff based on real
                                        // progress, not mere re-subscription. A
                                        // connection that carried frames then dropped
                                        // is a healthy blip → reset to the initial 5s
                                        // and re-declare immediately. A connection
                                        // that declared OK but never received a frame
                                        // is flapping → sleep the current backoff and
                                        // grow it (5s → … → 60s cap) so the
                                        // lost/restored event rate stays bounded
                                        // instead of spinning. The `closing` re-check
                                        // inside the re-declare loop still owns
                                        // shutdown during the sleep.
                                        if made_progress {
                                            backoff = std::time::Duration::from_secs(5);
                                        } else {
                                            tokio::time::sleep(backoff).await;
                                            backoff = std::cmp::min(backoff * 2, MAX_BACKOFF);
                                        }
                                        tracing::warn!(
                                            key = %sub_key_retry,
                                            "dm voice subscriber closed unexpectedly; reconnecting"
                                        );
                                        let _ = app_sub.emit(
                                            "voice-transport-lost",
                                            serde_json::json!({ "callId": call_hex }),
                                        );
                                        // Re-declare the subscriber. Like the
                                        // voice-signal sub, try immediately on the
                                        // first attempt and only back off *after* a
                                        // failed re-declare, so a momentary blip
                                        // recovers without waiting out the backoff.
                                        loop {
                                            if closing_sub.load(std::sync::atomic::Ordering::SeqCst) {
                                                return;
                                            }
                                            match session_for_media
                                                .declare_subscriber(&sub_key_retry)
                                                .await
                                            {
                                                Ok(s) => {
                                                    // Backoff reset is owned by the
                                                    // made_progress logic above, not
                                                    // by mere re-subscription.
                                                    sub = s;
                                                    let _ = app_sub.emit(
                                                        "voice-transport-restored",
                                                        serde_json::json!({ "callId": call_hex }),
                                                    );
                                                    break;
                                                }
                                                Err(e) => {
                                                    tracing::warn!(
                                                        key = %sub_key_retry,
                                                        error = %e,
                                                        backoff_s = backoff.as_secs(),
                                                        "dm voice subscriber re-declare failed; retrying"
                                                    );
                                                    tokio::time::sleep(backoff).await;
                                                    backoff =
                                                        std::cmp::min(backoff * 2, MAX_BACKOFF);
                                                }
                                            }
                                        }
                                    }
                                });
                                dm_voice_keys.insert(call_id, caps.channel_key);
                                dm_voice_mute_flags.insert(
                                    call_id,
                                    std::sync::Arc::new(AtomicBool::new(true)),
                                );
                                // Abort any prior subscriber for this call_id —
                                // HashMap::insert returns the old handle, and
                                // merely dropping it detaches (doesn't stop) the
                                // task, which would double-emit remote frames on
                                // a rejoin until shutdown.
                                if let Some(old) = dm_voice_subs.insert(call_id, handle) {
                                    old.abort();
                                }
                            }
                            Err(e) => tracing::warn!(err = %e, "dm voice subscribe failed"),
                        }
                    }
                    crate::voice::VoiceChannelRequest::LeaveDmCall { call_id } => {
                        dm_voice_mute_flags.remove(&call_id);
                        dm_voice_keys.remove(&call_id);
                        if let Some(handle) = dm_voice_subs.remove(&call_id) {
                            handle.abort();
                        }
                    }
                    crate::voice::VoiceChannelRequest::SetDmCallMuted { call_id, muted } => {
                        if let Some(flag) = dm_voice_mute_flags.get(&call_id) {
                            flag.store(muted, std::sync::atomic::Ordering::SeqCst);
                        }
                    }
                }
            }

            // ── ZEB-352: outbound DM-call signaling relay ─────
            // Each resolved `VoiceSignalRequest` (sealed by the IPC seam)
            // is published to the callee's owner-scoped signaling topic.
            // Fire-and-forget: a publish failure is logged but doesn't
            // tear down the loop (the frontend retries via timeout).
            Some(req) = voice_signal_rx.recv() => {
                let key_expr = format!("harmony/voice-signal/{}", req.callee_owner_hex);
                if let Err(e) = session.put(&key_expr, req.sealed).await {
                    tracing::warn!(%key_expr, err = %e, "voice signal publish failed");
                }
            }

            // ── ZEB-350: presence roster sweep ───────────────
            // A dedicated 4 s interval tick (NOT a bare `sleep` arm, which
            // would reset every loop iteration and starve). Evicts entries
            // silent for >12 s, then re-emits the roster for each affected
            // (community, channel) exactly once. Gated on active voice channels
            // so a node with none joined never wakes here (Cursor review).
            _ = voice_sweep_tick.tick(), if !voice_keys.is_empty() => {
                let now = (voice_now_ms)();
                let evicted = {
                    let mut g = voice_presence_map.lock().await;
                    g.sweep(now, VOICE_PRESENCE_TTL_MS)
                };
                if !evicted.is_empty() {
                    // Dedup the (community, channel) keys; emit each once.
                    let mut touched: std::collections::HashSet<(
                        crate::owner_state_types::SpaceId,
                        crate::community_membership::ChannelId,
                    )> = std::collections::HashSet::new();
                    for ((c, ch), _owner, _device) in &evicted {
                        touched.insert((*c, *ch));
                    }
                    for (c, ch) in touched {
                        let roster = {
                            let g = voice_presence_map.lock().await;
                            g.roster(&c, &ch)
                        };
                        let _ = app.emit(
                            "voice-presence-changed",
                            serde_json::json!({
                                "community": hex::encode(c.0),
                                "channel": hex::encode(ch.0),
                                "roster": roster,
                            }),
                        );
                    }
                }
            }

            // ── ZEB-217 Sub-C Phase 3 Task 9: on-demand adapter ────
            // Drained when an IPC (`create_community`, Phase 4
            // `redeem_invite`) dispatches a fresh
            // `CommunityAdapterRequest`. Spawns the per-community
            // Zenoh adapter against the live `session_arc`. None on
            // recv() means stop_node took the matching sender — we
            // ignore (no break) so the loop continues toward the
            // shutdown arm below, which is the canonical exit.
            Some(req) = community_adapter_request_rx.recv() => {
                spawn_community_state_zenoh_adapter(
                    Arc::clone(&session_arc),
                    req.id_hex,
                    req.publisher_rx,
                    req.subscriber_tx,
                    Arc::clone(&closing),
                );
            }

            // ── ZEB-298+ZEB-312 PR 1: voting-log adapter bridge ──────
            // Drained whenever ensure_voting_engine_for enqueues an
            // adapter request. Spawns the per-community Zenoh adapter
            // against the live session_arc. Same closing-flag plumbing
            // as the state-root arm.
            Some(req) = voting_log_adapter_request_rx.recv() => {
                spawn_voting_log_zenoh_adapter(
                    Arc::clone(&session_arc),
                    req.id_hex,
                    req.publisher_rx,
                    req.subscriber_tx,
                    Arc::clone(&closing),
                );
            }

            // ── ZEB-270 Phase 3 Task 4.5: channel-log adapter bridge ──
            // Drained whenever `ChannelLogRegistry::spawn` enqueues an
            // adapter request. Spawns the per-channel Zenoh adapter
            // (publisher + subscriber + queryable + query-driver) against
            // the live `session_arc`. Per-channel `closing` flag in
            // `req.closing` is shared with the registry — `registry.stop`
            // flips it; the adapter task observes within ≤1s and exits.
            // Engine-level `closing` (held inside ChannelLogEngine itself)
            // is the engine's own flag and is independent.
            Some(req) = channel_log_adapter_request_rx.recv() => {
                let _handle = spawn_channel_log_zenoh_adapter(
                    Arc::clone(&session_arc),
                    req.community_id_hex,
                    req.channel_id_hex,
                    req.publisher_rx,
                    req.subscriber_tx,
                    req.query_request_rx,
                    req.read_for_query,
                    req.emit_backfill_progress,
                    req.backfill_progress_interval,
                    req.backfill_default_limit,
                    req.closing,
                );
                // JoinHandle dropped — adapter task is fire-and-forget.
                // The registry-held closing flag drives shutdown.
            }

            // ── Shutdown signal ──────────────────────────────────────
            _ = shutdown.changed() => {
                tracing::info!("shutdown signal received");
                break;
            }
        }

        if should_tick {
            let actions = runtime.tick();
            for action in actions {
                handle_runtime_action_or_dispatch(
                    action,
                    &session,
                    &zenoh_tx,
                    &udp,
                    &broadcast_addr,
                    &app,
                    &closing,
                    &own_zid,
                    dm_outbox.as_ref(),
                    crdt_state.as_ref(),
                    cas_handle.as_ref(),
                    unicast_send_tx.as_ref(),
                    community_registry.as_ref(),
                    &mut runtime_action_retry,
                    RUNTIME_ACTION_RETRY_CAP,
                )
                .await;
            }
        }

        // ZEB-227 PR #80 review fix: drain at most one queued
        // RuntimeAction per loop iteration. Processing one-at-a-time
        // means a steady stream of contended packets can't starve other
        // select! arms (timer, UDP, shutdown). When locks are still
        // contended on retry, `handle_runtime_action_or_dispatch`
        // re-pushes onto the buffer; we'll try again next iteration.
        if let Some(retry_action) = runtime_action_retry.pop_front() {
            handle_runtime_action_or_dispatch(
                retry_action,
                &session,
                &zenoh_tx,
                &udp,
                &broadcast_addr,
                &app,
                &closing,
                &own_zid,
                dm_outbox.as_ref(),
                crdt_state.as_ref(),
                cas_handle.as_ref(),
                unicast_send_tx.as_ref(),
                community_registry.as_ref(),
                &mut runtime_action_retry,
                RUNTIME_ACTION_RETRY_CAP,
            )
            .await;
        }
    }

    // Mark intentional shutdown so spawned tasks don't emit false errors.
    closing.store(true, Ordering::SeqCst);
    for (_, handle) in voice_subs.drain() {
        handle.abort();
    }
    // ZEB-350: abort the presence publisher/subscriber tasks too.
    for (_, handle) in voice_presence_subs.drain() {
        handle.abort();
    }
    for (_, handle) in voice_presence_pubs.drain() {
        handle.abort();
    }
    // ZEB-352: abort the DM-call media subscriber tasks too, so they don't run
    // detached past shutdown (emitting into a stale AppHandle or racing a
    // subsequent start_node restart that builds fresh state maps).
    for (_, handle) in dm_voice_subs.drain() {
        handle.abort();
    }
    // ZEB-352: abort the always-on voice-signal subscriber too, so it can't emit
    // signaling events into a stale AppHandle during the closing→session-close
    // window or race a subsequent start_node restart.
    if let Some(handle) = voice_signal_sub_handle {
        handle.abort();
    }
    let _ = session.close().await;
    tracing::info!("event loop stopped");
}

/// ZEB-352: map a verified inbound `VoiceSignal` to the matching frontend
/// event and emit it. The call state machine lives in the frontend; this
/// only translates the transport-level signal into a Tauri event with the
/// hex-encoded call-ID (and caller / decline reason where applicable).
fn emit_voice_signal_event<R: Runtime>(
    app: &AppHandle<R>,
    signal: &crate::voice_signal::VoiceSignal,
    space_id_hex: Option<&str>,
) {
    use crate::voice_signal::{DeclineReason, VoiceSignalKind};
    let call_hex = hex::encode(signal.call_id);
    match signal.kind {
        VoiceSignalKind::Invite => {
            let _ = app.emit(
                "incoming-call",
                serde_json::json!({
                    "callId": call_hex,
                    "callerOwner": hex::encode(signal.caller.0),
                    "spaceId": space_id_hex,
                }),
            );
        }
        VoiceSignalKind::Accept => {
            let _ = app.emit("call-accepted", serde_json::json!({ "callId": call_hex }));
        }
        VoiceSignalKind::Decline => {
            let reason = match signal.decline_reason {
                Some(DeclineReason::User) | None => "user",
                Some(DeclineReason::Busy) => "busy",
                Some(DeclineReason::Timeout) => "timeout",
            };
            let _ = app.emit(
                "call-declined",
                serde_json::json!({ "callId": call_hex, "reason": reason }),
            );
        }
        VoiceSignalKind::Cancel => {
            let _ = app.emit("call-canceled", serde_json::json!({ "callId": call_hex }));
        }
        VoiceSignalKind::End => {
            let _ = app.emit("call-ended", serde_json::json!({ "callId": call_hex }));
        }
    }
}

/// ZEB-227 Path B Task 11: handle a single `RuntimeAction`, peeling off
/// `RuntimeAction::UnicastReceived` for inbound DM dispatch through
/// `DmOutbox.handle_unicast`. Other variants fall through to the standard
/// `dispatch_action` platform-I/O path.
///
/// `dispatch_action` has a catch-all `_ => {}` arm that would silently
/// drop `UnicastReceived` — extracted here so all three `runtime.tick()`
/// loops route consistently without duplicating the interception block.
///
/// Lock acquisition uses `try_lock`: contention requeues the action via
/// `retry_buffer` so the next loop iteration retries once locks are free,
/// instead of dropping the packet. `.lock().await` here would re-introduce
/// the deadlock chain (send_dm IPC + cas_op processing both contend on
/// dm_outbox + crdt_state). The retry buffer keeps inbound DMs reliable
/// without the deadlock risk of awaiting on a contended lock.
///
/// `retry_buffer` is bounded: when full, the action is dropped+warned
/// (very-degraded case; means event-loop is wedged behind a long IPC and
/// >32 packets have queued — Reticulum CidNotify retransmit will redrive).
///
/// NOTE: a focused unit test for the requeue behavior is deferred — the
/// `handle_runtime_action_or_dispatch` helper requires an `AppHandle`,
/// `zenoh::Session`, `UdpSocket`, and several other handles to call,
/// which the existing event_loop test modules don't currently scaffold.
/// The fix-up is verified via type-checking of the requeue branch + the
/// dm_outbox-side tests for the channel-pressure path.
#[allow(clippy::too_many_arguments)]
async fn handle_runtime_action_or_dispatch<R: Runtime>(
    action: RuntimeAction,
    session: &zenoh::Session,
    zenoh_tx: &mpsc::Sender<ZenohEvent>,
    udp: &UdpSocket,
    broadcast_addr: &SocketAddr,
    app: &AppHandle<R>,
    closing: &Arc<AtomicBool>,
    own_zid: &str,
    dm_outbox: Option<&std::sync::Arc<tokio::sync::Mutex<crate::dm_outbox::DmOutbox>>>,
    crdt_state: Option<&std::sync::Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>>,
    cas_handle: Option<&std::sync::Arc<dyn crate::content_store::ContentStore>>,
    unicast_send_tx: Option<&mpsc::Sender<crate::dm_outbox::UnicastSendRequest>>,
    // ZEB-262 Phase 4 Task 9: registry handle for the community-packet
    // discriminant pre-fork (`inbound_packet::try_dispatch_community`).
    // None until owner identity is loaded; same gating as `dm_outbox`.
    community_registry: Option<&std::sync::Arc<crate::community_state_sync::CommunitySyncRegistry>>,
    retry_buffer: &mut std::collections::VecDeque<RuntimeAction>,
    retry_buffer_cap: usize,
) {
    if matches!(action, RuntimeAction::UnicastReceived { .. }) {
        // ZEB-262 Phase 4 Task 9: discriminant pre-fork. Peek
        // `packet[0]`; if `0x10` (community packet), route to
        // `inbound_packet::try_dispatch_community` and short-circuit.
        // Otherwise fall through to the existing DM dispatch
        // (preserves Path B 0x01-0x03 + unknown-discriminant logging).
        if let RuntimeAction::UnicastReceived { packet, .. } = &action {
            if packet.first() == Some(&0x10) {
                if let (Some(outbox), Some(state)) = (dm_outbox, crdt_state) {
                    crate::inbound_packet::try_dispatch_community(
                        community_registry,
                        outbox,
                        state,
                        packet,
                        Some(app),
                    )
                    .await;
                } else {
                    tracing::warn!(
                        "received community packet (0x10) but DM runtime not initialized (no owner identity?); dropping"
                    );
                }
                return;
            }
        }
        if let (Some(outbox), Some(state), Some(cas), Some(tx)) =
            (dm_outbox, crdt_state, cas_handle, unicast_send_tx)
        {
            // ZEB-241: pre-decode to detect CidNotify; spawn lifted handler
            // so the 500ms CAS fetch in Phase B doesn't hold the outbox +
            // state locks. Invite/Ack continue through the existing
            // try_lock + handle_unicast path (unchanged behavior).
            let packet_bytes = match &action {
                RuntimeAction::UnicastReceived { packet, .. } => packet.clone(),
                _ => unreachable!("matched UnicastReceived above"),
            };
            let wall_now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;

            match crate::dm_envelope::decode_packet(&packet_bytes) {
                Ok(crate::dm_envelope::DmPacket::CidNotify {
                    signed,
                    signature,
                    signed_bytes,
                }) => {
                    // Spawn lifted handler — fire-and-forget. Phase A
                    // locks are acquired inside the spawned task; the
                    // event_loop's select! returns immediately after
                    // spawn so the next inbound action can start.
                    let outbox_clone = std::sync::Arc::clone(outbox);
                    let state_clone = std::sync::Arc::clone(state);
                    let cas_clone = std::sync::Arc::clone(cas);
                    let tx_clone = tx.clone();
                    let app_clone = app.clone();
                    let signed_bytes_owned = signed_bytes.to_vec();
                    tokio::spawn(async move {
                        crate::dm_outbox::DmOutbox::handle_cidnotify_lifted(
                            outbox_clone,
                            state_clone,
                            cas_clone,
                            tx_clone,
                            app_clone,
                            signed,
                            signature,
                            signed_bytes_owned,
                            wall_now_ms,
                        )
                        .await;
                    });
                    return;
                }
                Ok(_) | Err(_) => {
                    // Invite / Ack / decode-failure: fall through to
                    // existing try_lock + handle_unicast path. The
                    // decode failure case re-decodes inside
                    // handle_unicast and tracing::warn!s accordingly,
                    // preserving prior error-handling behavior.
                }
            }

            let outbox_try = outbox.try_lock();
            let state_try = state.try_lock();
            match (outbox_try, state_try) {
                (Ok(mut outbox_g), Ok(mut state_g)) => {
                    let result = outbox_g
                        .handle_unicast(&mut state_g, cas.as_ref(), tx, packet_bytes, wall_now_ms)
                        .await;
                    // Drop locks before IPC emits.
                    drop(state_g);
                    drop(outbox_g);
                    match result {
                        Ok(outcome) => {
                            for rm in outcome.newly_received {
                                let _ = app.emit(
                                    "dm-received",
                                    serde_json::json!({
                                        "spaceId": hex::encode(rm.inbox_entry.space_id.0),
                                        "messageCid": hex::encode(rm.inbox_entry.message_cid.to_bytes()),
                                        "from": hex::encode(rm.inbox_entry.from.0),
                                        "receivedAt": rm.inbox_entry.received_at.wall_ms,
                                        "sentAt": rm.sent_at.wall_ms,
                                        "body": hex::encode(&rm.body),
                                        "mimeType": rm.mime_type,
                                    }),
                                );
                            }
                            for (space_id, message_cid, recipient) in outcome.newly_delivered {
                                let _ = app.emit(
                                    "dm-delivered",
                                    serde_json::json!({
                                        "spaceId": hex::encode(space_id.0),
                                        "messageCid": hex::encode(message_cid.to_bytes()),
                                        "recipientOwnerAddr": hex::encode(recipient.0),
                                    }),
                                );
                            }
                            for (space_id, message_cid) in outcome.newly_expired {
                                let _ = app.emit(
                                    "dm-expired",
                                    serde_json::json!({
                                        "spaceId": hex::encode(space_id.0),
                                        "messageCid": hex::encode(message_cid.to_bytes()),
                                    }),
                                );
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "handle_unicast dropped packet");
                        }
                    }
                }
                _ => {
                    // Locks contended — requeue this action so the next
                    // loop iteration retries once locks free up. Bounded:
                    // drop+warn when the retry buffer is full.
                    if retry_buffer.len() >= retry_buffer_cap {
                        tracing::warn!(
                            cap = retry_buffer_cap,
                            "UnicastReceived retry buffer full; dropping packet \
                             (event loop appears wedged behind contended IPC)"
                        );
                    } else {
                        tracing::debug!(
                            "handle_unicast deferred this tick (locks contended); requeued"
                        );
                        retry_buffer.push_back(action);
                    }
                }
            }
            return;
        }
        // No owner identity loaded — DM stack is uninitialized. Drop the
        // packet silently; harmony-node has the same behavior (see
        // event_loop.rs in harmony-node for the matching no-client-consumer
        // diagnostic).
        return;
    }
    dispatch_action(
        action,
        session,
        zenoh_tx,
        udp,
        broadcast_addr,
        app,
        closing,
        own_zid,
    )
    .await;
}

/// Dispatch a single RuntimeAction to the platform I/O layer.
#[allow(clippy::too_many_arguments)] // pre-existing; tracked for refactor
async fn dispatch_action<R: Runtime>(
    action: RuntimeAction,
    session: &zenoh::Session,
    zenoh_tx: &mpsc::Sender<ZenohEvent>,
    udp: &UdpSocket,
    broadcast_addr: &SocketAddr,
    app: &AppHandle<R>,
    closing: &Arc<AtomicBool>,
    own_zid: &str,
) {
    match action {
        // ── Network: Reticulum packet send ───────────────────────────
        RuntimeAction::SendOnInterface { raw, weight, .. } => {
            if let Some(w) = weight {
                if rand::random::<f32>() >= w {
                    return;
                }
            }
            let _ = udp.send_to(&raw, broadcast_addr).await;
        }

        // ── Zenoh: publish ───────────────────────────────────────────
        RuntimeAction::Publish { key_expr, payload } => {
            let session = session.clone();
            // Attach our ZenohId to capacity publications so receivers can
            // determine hop distance by comparing against their peers_zid().
            let zid_attachment = if key_expr.starts_with(crate::CAPACITY_PREFIX) {
                Some(own_zid.to_string())
            } else {
                None
            };
            tokio::spawn(async move {
                let mut builder = session.put(&key_expr, payload);
                if let Some(zid) = zid_attachment {
                    builder = builder.attachment(zid.as_bytes());
                }
                if let Err(e) = builder.await {
                    tracing::warn!(%key_expr, err = %e, "zenoh put failed");
                }
            });
        }

        // ── Zenoh: declare queryable ─────────────────────────────────
        RuntimeAction::DeclareQueryable { key_expr } => {
            let is_compute = key_expr.starts_with("harmony/compute/");
            let tx = zenoh_tx.clone();
            let app = app.clone();
            let closing = closing.clone();
            match session.declare_queryable(&key_expr).await {
                Ok(qbl) => {
                    tokio::spawn(async move {
                        while let Ok(query) = qbl.recv_async().await {
                            let qkey = query.key_expr().to_string();
                            let payload = query
                                .payload()
                                .map(|p| p.to_bytes().to_vec())
                                .unwrap_or_default();
                            let ev = if is_compute {
                                ZenohEvent::ComputeQuery {
                                    key_expr: qkey,
                                    payload,
                                }
                            } else {
                                ZenohEvent::Query {
                                    key_expr: qkey,
                                    payload,
                                }
                            };
                            if tx.send(ev).await.is_err() {
                                break;
                            }
                        }
                        // Only emit session-lost if this wasn't an intentional shutdown.
                        if !closing.load(Ordering::SeqCst) {
                            emit_session_lost(&app, "queryable closed unexpectedly");
                        }
                    });
                }
                Err(e) => {
                    tracing::error!(%key_expr, err = %e, "declare_queryable failed");
                }
            }
        }

        // ── Zenoh: subscribe ─────────────────────────────────────────
        RuntimeAction::Subscribe { key_expr } => {
            let tx = zenoh_tx.clone();
            let app = app.clone();
            let closing = closing.clone();
            match session.declare_subscriber(&key_expr).await {
                Ok(sub) => {
                    tokio::spawn(async move {
                        while let Ok(sample) = sub.recv_async().await {
                            let skey = sample.key_expr().to_string();
                            // PR #63 review (CodeRabbit): pairing-scope size
                            // cap MUST run BEFORE the heap allocation, not
                            // after. Earlier code did the check in the
                            // consumer (event-loop) path, by which point a
                            // hostile peer could fill the 256-slot zenoh_rx
                            // channel with oversized buffers. Doing the
                            // check on the bytes view skips both the .to_vec
                            // allocation and the channel queue when the
                            // payload is over-cap.
                            let bytes = sample.payload().to_bytes();
                            if skey.starts_with(crate::pairing::PAIRING_KEY_PREFIX_SLASH)
                                && bytes.len() > crate::pairing::MAX_PAIRING_WIRE_BYTES
                            {
                                tracing::warn!(
                                    "rejecting oversized pairing payload on {skey}: {} bytes > {}",
                                    bytes.len(),
                                    crate::pairing::MAX_PAIRING_WIRE_BYTES,
                                );
                                continue;
                            }
                            let payload = bytes.to_vec();
                            // Extract publisher's ZenohId from attachment (if present).
                            let source_zid = sample
                                .attachment()
                                .and_then(|a| String::from_utf8(a.to_bytes().to_vec()).ok());
                            if tx
                                .send(ZenohEvent::Subscription {
                                    key_expr: skey,
                                    payload,
                                    source_zid,
                                })
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        if !closing.load(Ordering::SeqCst) {
                            emit_session_lost(&app, "subscriber closed unexpectedly");
                        }
                    });
                }
                Err(e) => {
                    tracing::error!(%key_expr, err = %e, "declare_subscriber failed");
                }
            }
        }

        // ── Zenoh: fetch content by CID ──────────────────────────────
        RuntimeAction::FetchContent { cid } => {
            let cid_hex = hex::encode(cid);
            // Uses second hex nibble as shard prefix — matches harmony-zenoh fetch_key().
            let prefix = cid_hex.get(1..2).unwrap_or("");
            let key_expr = format!("harmony/content/{prefix}/{cid_hex}");
            let tx = zenoh_tx.clone();
            let session = session.clone();
            tokio::spawn(async move {
                let result = fetch_via_zenoh(&session, &key_expr).await;
                let _ = tx
                    .send(ZenohEvent::FetchResponse {
                        cid,
                        is_module: false,
                        result,
                    })
                    .await;
            });
        }

        RuntimeAction::FetchModule { cid } => {
            let cid_hex = hex::encode(cid);
            let prefix = cid_hex.get(1..2).unwrap_or("");
            let key_expr = format!("harmony/content/{prefix}/{cid_hex}");
            let tx = zenoh_tx.clone();
            let session = session.clone();
            tokio::spawn(async move {
                let result = fetch_via_zenoh(&session, &key_expr).await;
                let _ = tx
                    .send(ZenohEvent::FetchResponse {
                        cid,
                        is_module: true,
                        result,
                    })
                    .await;
            });
        }

        // ── SendReply: stub (same as harmony-node) ───────────────────
        RuntimeAction::SendReply { .. } => {
            tracing::trace!("SendReply not yet implemented in client");
        }

        // ── Actions not applicable to desktop client ─────────────────
        _ => {}
    }
}

/// Fetch content via Zenoh get() with a 30s timeout.
async fn fetch_via_zenoh(session: &zenoh::Session, key_expr: &str) -> Result<Vec<u8>, String> {
    let replies = session
        .get(key_expr)
        .await
        .map_err(|e| format!("zenoh get error: {e}"))?;

    let deadline = Duration::from_secs(30);
    tokio::time::timeout(deadline, async {
        while let Ok(reply) = replies.recv_async().await {
            match reply.result() {
                Ok(sample) => {
                    return Ok(sample.payload().to_bytes().to_vec());
                }
                Err(err) => {
                    let msg = String::from_utf8_lossy(&err.payload().to_bytes()).into_owned();
                    tracing::warn!(%key_expr, err = %msg, "zenoh fetch reply error");
                }
            }
        }
        Err(format!("no successful reply for '{key_expr}'"))
    })
    .await
    .unwrap_or_else(|_| Err(format!("fetch '{key_expr}' timed out after 30s")))
}

/// Query a mail root key with a 10-second budget.
///
/// Distinct from `fetch_via_zenoh` because the mail-root protocol treats an
/// empty reply as a valid sentinel ("no mail for this address yet") whereas
/// fetch_via_zenoh requires a successful non-empty reply. Returns:
/// - `Ok(Some(payload))` — at least one responder replied successfully. A
///   non-empty payload is the current root CID; an empty payload is the
///   explicit "no mail yet" sentinel from the gateway's queryable.
/// - `Ok(None)` — no responder replied at all. The caller surfaces this as
///   a failed query (e.g., no gateway with this queryable declared).
/// - `Err(msg)` — the `get` call itself failed, the 10s budget elapsed, or
///   every responder returned an error reply (no successful reply seen).
///
/// Multiple responders are tolerated via `ConsolidationMode::None`. A
/// non-empty success reply is preferred over the empty sentinel; either
/// success outcome is preferred over an error-only outcome.
///
/// Used by both the cold-start query and the manual refresh path. `op_label`
/// appears in the timeout message for log disambiguation ("startup" vs
/// "refresh").
async fn query_mail_root(
    session: &zenoh::Session,
    key: &str,
    op_label: &str,
) -> Result<Option<Vec<u8>>, String> {
    use zenoh::query::ConsolidationMode;

    let label = op_label.to_string();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let replies = session
            .get(key)
            .consolidation(ConsolidationMode::None)
            .await
            .map_err(|e| format!("get: {e}"))?;

        // Drain all replies. Track three outcomes so an all-errors
        // result doesn't silently collapse into "no responder":
        //   - non_empty: a real root CID (best — short-circuits)
        //   - saw_empty: gateway explicitly says "no mail"
        //   - reply_error: every reply that landed was an Err
        let mut non_empty: Option<Vec<u8>> = None;
        let mut saw_empty = false;
        let mut reply_error: Option<String> = None;
        while let Ok(reply) = replies.recv_async().await {
            match reply.result() {
                Ok(sample) => {
                    let bytes = sample.payload().to_bytes().to_vec();
                    if bytes.is_empty() {
                        saw_empty = true;
                    } else {
                        non_empty = Some(bytes);
                        break;
                    }
                }
                Err(err) => {
                    // Keep the first error message for the surfaced Err.
                    reply_error.get_or_insert_with(|| {
                        String::from_utf8_lossy(&err.payload().to_bytes()).into_owned()
                    });
                }
            }
        }
        if let Some(bytes) = non_empty {
            Ok(Some(bytes))
        } else if saw_empty {
            Ok(Some(Vec::new()))
        } else if let Some(err) = reply_error {
            Err(format!("{label} root query reply error: {err}"))
        } else {
            Ok(None)
        }
    })
    .await
    .map_err(|_| format!("{op_label} root query timed out (10s)"))
    .and_then(|r| r)
}

/// Emit zenoh-status error when a Zenoh session appears to have been lost.
fn emit_session_lost<R: Runtime>(app: &AppHandle<R>, reason: &str) {
    let _ = app.emit(
        "zenoh-status",
        &crate::ZenohStatus {
            status: "error".to_string(),
            endpoint: None,
            error: Some(format!("session lost: {reason}")),
        },
    );
}

use harmony_content::book::BookStore;
use harmony_content::bundle;
use harmony_content::cache::ContentStore;
use harmony_content::cid::{CidType, ContentId};

/// Walk every CID in the tree rooted at `cid`, reading bundle payloads from
/// the local content store. Returns root + every descendant in DFS order.
///
/// Bundle payloads not in the store are silently skipped — their subtrees
/// are unreachable and the caller's verb can't act on them anyway. A
/// malformed bundle payload is treated the same: log-worthy but not fatal.
pub(crate) fn collect_descendants<S: BookStore>(
    store: &ContentStore<S>,
    cid: ContentId,
) -> Vec<ContentId> {
    use harmony_content::cid::MAX_BUNDLE_DEPTH;

    let mut out = Vec::new();
    let mut stack: Vec<(ContentId, u8)> = vec![(cid, 0)];
    while let Some((id, depth)) = stack.pop() {
        if depth > MAX_BUNDLE_DEPTH {
            tracing::warn!(
                cid_depth = depth,
                max = MAX_BUNDLE_DEPTH,
                "collect_descendants aborting subtree past MAX_BUNDLE_DEPTH; data is corrupt"
            );
            continue;
        }
        out.push(id);
        if matches!(id.cid_type(), CidType::Bundle(_)) {
            if let Some(bytes) = store.get(&id) {
                match bundle::parse_bundle(bytes) {
                    Ok(children) => {
                        for child in children.iter().copied() {
                            stack.push((child, depth + 1));
                        }
                    }
                    Err(e) => tracing::warn!(
                        err = ?e,
                        "malformed bundle payload; subtree skipped"
                    ),
                }
            }
        }
    }
    out
}

/// Build the set of CIDs that must stay pinned because they are reachable
/// from one of `pin_intent`'s remaining roots.
///
/// ZEB-156: shared keep-set computation for `ContentVerbRequest::Unpin` and
/// `ContentVerbRequest::Burn`. The Tauri OR-join (`is_cid_pinned_by_any`)
/// only spots sibling-root sharing; transitive sharing — where an unrelated
/// sidecar entry's CID is a descendant of the verb's root — is invisible to
/// it. Walking remaining roots here closes that gap.
///
/// Capacity hint matches the doomed-set size, since the caller's CIDs of
/// interest are bounded by it (we only ever check `keep.contains(&id)` for
/// `id` drawn from `doomed`); over-allocation is harmless and saves a few
/// rehashes when the keep set is dense.
pub(crate) fn compute_keep_set<S: BookStore>(
    store: &ContentStore<S>,
    pin_intent: &std::collections::HashSet<[u8; 32]>,
    capacity_hint: usize,
) -> std::collections::HashSet<ContentId> {
    let mut keep: std::collections::HashSet<ContentId> =
        std::collections::HashSet::with_capacity(capacity_hint);
    for keep_root_bytes in pin_intent.iter() {
        let kr = ContentId::from_bytes(*keep_root_bytes);
        keep.extend(collect_descendants(store, kr));
    }
    keep
}

/// Fetch the bytes of a content tree by repeatedly calling `fetch_one` per
/// CID and concatenating leaf payloads in bundle-child order.
///
/// Iterative (not async-recursive) to avoid `Pin<Box<dyn Future>>` friction.
/// The order-preserving DFS is "push children in reverse, pop in child
/// order" — so for a bundle `[L1, L2, L3]` we emit bytes `L1 || L2 || L3`.
///
/// Depth-capped at `MAX_BUNDLE_DEPTH` for defensive safety — the write side
/// already enforces this, so legitimate trees never trip the guard.
///
/// Returns `Err` — rather than logging and skipping — on depth overflow or
/// a malformed bundle payload, in contrast to `collect_descendants`. Fetch
/// reassembly cannot produce a correct result with any subtree missing.
pub(crate) async fn fetch_recursive<F, Fut>(
    fetch_one: F,
    root: ContentId,
) -> Result<Vec<u8>, String>
where
    F: Fn(ContentId) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<u8>, String>>,
{
    use harmony_content::cid::MAX_BUNDLE_DEPTH;

    let mut out = Vec::new();
    let mut stack: Vec<(ContentId, u8)> = vec![(root, 0)];

    while let Some((cid, depth)) = stack.pop() {
        if depth > MAX_BUNDLE_DEPTH {
            return Err(format!(
                "bundle depth {depth} exceeds MAX_BUNDLE_DEPTH {MAX_BUNDLE_DEPTH}"
            ));
        }
        let bytes = fetch_one(cid).await?;
        if matches!(cid.cid_type(), CidType::Bundle(_)) {
            let children =
                bundle::parse_bundle(&bytes).map_err(|e| format!("malformed bundle: {e:?}"))?;
            for child in children.iter().rev() {
                stack.push((*child, depth + 1));
            }
        } else {
            out.extend_from_slice(&bytes);
        }
    }
    Ok(out)
}

/// ZEB-159: wraps a per-CID fetch closure so each successful fetch
/// admits the bytes to the local StorageTier cache via `cas_op_tx`
/// BEFORE returning to the caller. Each admission round-trips through
/// a `Some(reply)` oneshot so by the time `fetch_recursive` returns,
/// every fetched CID has been processed by the event-loop's
/// `CasOp::PutLocal` arm (which calls `runtime.tick()` before signaling
/// the reply). This is the load-bearing ordering for the
/// `fetch_completion_rx` cascade: without the synchronous round-trip,
/// the completion arm can race ahead of the PutLocal arm and walk a
/// partial cache (Cursor + Qodo R1, 2026-05-15).
///
/// Mirrors the GetOrFetch admit-hop pattern at `event_loop.rs:1625` in
/// shape, but differs in synchronization: GetOrFetch is fire-and-forget
/// because its caller has no downstream channel-ordered dependency on
/// admission completion. The fetch_rx path DOES — it signals
/// `fetch_completion_tx` after `fetch_recursive` returns — so the
/// admission must be ordered before the signal.
///
/// On `fetch_one` failure (Err), no admission is attempted for that
/// CID. On `cas_op_tx.send()` failure (event-loop shutting down), the
/// admission is skipped silently and the fetch still returns Ok —
/// admission is best-effort with respect to the cache, but ordered
/// with respect to the completion signal.
//
// `clippy::type_complexity` allow: the return type is intentionally
// explicit (`impl Fn(...) -> Pin<Box<dyn Future>> + Clone + Send +
// 'static`) because the wrapped closure must be `Send + 'static` to be
// captured into `tokio::spawn(async move { ... })` in the `fetch_rx`
// arm, and the returned future must be `Send` so the spawned task is
// `Send` (Tauri command futures require this). Factoring into a `type`
// alias would either (a) require a trait-alias nightly feature or
// (b) hide the load-bearing bounds behind a name and force readers to
// chase the alias to understand the contract.
#[allow(clippy::type_complexity)]
pub(crate) fn wrap_fetch_one_with_admission<F, Fut>(
    fetch_one: F,
    cas_op_tx: tokio::sync::mpsc::Sender<crate::content_store::CasOp>,
) -> impl Fn(
    ContentId,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>, String>> + Send>>
       + Clone
       + Send
       + 'static
where
    F: Fn(ContentId) -> Fut + Clone + Send + 'static,
    Fut: std::future::Future<Output = Result<Vec<u8>, String>> + Send + 'static,
{
    move |cid: ContentId| {
        let inner = fetch_one.clone();
        let cas_op_tx = cas_op_tx.clone();
        Box::pin(async move {
            let bytes = inner(cid).await?;
            // ZEB-343 verify-on-fetch (spec §5.3): the StorageTier cache admit
            // already verifies hash==cid, but the bytes RETURNED here go
            // straight to the caller (e.g. the avatar resolver) regardless of
            // admit success. Reject a tampered reply before it is returned OR
            // admitted, so a malicious server can never get its bytes rendered.
            if !cid.verify_hash(&bytes) {
                return Err(format!(
                    "fetched bytes for {} failed hash==CID verification",
                    hex::encode(cid.to_bytes())
                ));
            }
            // Synchronous round-trip through the event loop's PutLocal
            // arm. `bytes.clone()` is load-bearing: `CasOp::PutLocal.blob`
            // consumes the bytes, but the caller (and `fetch_recursive`'s
            // bundle parser) needs them too.
            //
            // `reply: Some(...)` + `reply_rx.await` is the fix for the
            // Cursor + Qodo R1 race: the PutLocal handler ticks the
            // runtime BEFORE sending the reply, so when reply_rx
            // resolves, the cache contains this CID. Without this fence,
            // the fetch_completion_rx arm in the event loop could be
            // picked by `select!` before the PutLocal arm processes our
            // admission, and `collect_descendants` would walk a partial
            // cache.
            //
            // Both awaits are bounded by `ADMISSION_TIMEOUT` (CodeRabbit
            // R2): a stalled event-loop arm or a saturated CAS channel
            // must not pin a successful fetch behind an unbounded wait.
            // On timeout OR `cas_op_tx.send()` failure (event loop
            // dropped during shutdown), skip the rest — admission is
            // best-effort with respect to the cache but ordered with
            // respect to the completion signal; if the event loop isn't
            // responding within 2s, the completion arm wouldn't be
            // running either, so ordering is moot.
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            let send_result = tokio::time::timeout(
                ADMISSION_TIMEOUT,
                cas_op_tx.send(crate::content_store::CasOp::PutLocal {
                    cid,
                    blob: bytes.clone(),
                    reply: Some(reply_tx),
                }),
            )
            .await;
            if matches!(send_result, Ok(Ok(()))) {
                // Discard the reply result. The cache may silently
                // reject under W-TinyLFU pressure; we don't propagate
                // that to the fetch caller (admission is best-effort,
                // not load-bearing for the fetch's own correctness).
                // A timeout here is equivalent to silent rejection.
                let _ = tokio::time::timeout(ADMISSION_TIMEOUT, reply_rx).await;
            }
            Ok(bytes)
        })
    }
}

/// Per-CID admission timeout in `wrap_fetch_one_with_admission`. Bounds
/// both `cas_op_tx.send()` and the reply-oneshot await so a stalled or
/// saturated event-loop CAS arm cannot pin a successful fetch behind an
/// unbounded wait. 2 seconds is generous for a local mpsc round-trip
/// (the PutLocal arm itself ticks the runtime — typically microseconds
/// — and sends the reply); a 2s timeout indicates real trouble
/// elsewhere, in which case best-effort skip-the-admit is the right
/// behavior. See CodeRabbit R2 on PR #125.
const ADMISSION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

#[cfg(test)]
mod descendants_tests {
    use super::collect_descendants;
    use harmony_content::book::BookStore;
    use harmony_content::book::MemoryBookStore;
    use harmony_content::bundle::BundleBuilder;
    use harmony_content::cache::ContentStore;
    use harmony_content::cid::{ContentFlags, ContentId};

    fn new_store() -> ContentStore<MemoryBookStore> {
        ContentStore::new(MemoryBookStore::new(), 1024)
    }

    #[test]
    fn returns_just_the_root_for_a_leaf() {
        let mut store = new_store();
        let leaf = store
            .insert_with_flags(b"hello", ContentFlags::default())
            .unwrap();

        let all = collect_descendants(&store, leaf);
        assert_eq!(all, vec![leaf]);
    }

    #[test]
    fn walks_a_flat_bundle() {
        let mut store = new_store();
        let a = store
            .insert_with_flags(b"aaa", ContentFlags::default())
            .unwrap();
        let b = store
            .insert_with_flags(b"bbb", ContentFlags::default())
            .unwrap();
        let c = store
            .insert_with_flags(b"ccc", ContentFlags::default())
            .unwrap();

        let mut builder = BundleBuilder::new();
        builder.add(a).add(b).add(c);
        let (payload, root) = builder.build_with_flags(ContentFlags::default()).unwrap();
        store.store(root, payload);

        let all = collect_descendants(&store, root);
        // Order is unspecified; compare as sets.
        use std::collections::HashSet;
        let got: HashSet<ContentId> = all.into_iter().collect();
        let expected: HashSet<ContentId> = [root, a, b, c].into_iter().collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn skips_subtrees_whose_bundle_payload_is_missing() {
        let mut store = new_store();
        let a = store
            .insert_with_flags(b"aaa", ContentFlags::default())
            .unwrap();
        let b = store
            .insert_with_flags(b"bbb", ContentFlags::default())
            .unwrap();

        let mut builder = BundleBuilder::new();
        builder.add(a).add(b);
        let (_payload, root) = builder.build_with_flags(ContentFlags::default()).unwrap();
        // Deliberately DO NOT store the bundle payload.

        let all = collect_descendants(&store, root);
        // Walker should still include the root itself; children are
        // unreachable and therefore silently skipped.
        assert_eq!(all, vec![root]);
    }
}

#[cfg(test)]
mod fetch_recursive_tests {
    use super::fetch_recursive;
    use harmony_content::bundle::BundleBuilder;
    use harmony_content::cid::{ContentFlags, ContentId};
    use std::collections::HashMap;

    #[tokio::test]
    async fn leaf_only_fetch_returns_single_payload() {
        let leaf = ContentId::for_book(b"hello", ContentFlags::default()).unwrap();
        let mut store: HashMap<ContentId, Vec<u8>> = HashMap::new();
        store.insert(leaf, b"hello".to_vec());

        let fetcher = move |cid: ContentId| {
            let bytes = store.get(&cid).cloned();
            std::future::ready(bytes.ok_or_else(|| format!("missing cid: {cid:?}")))
        };

        let got = fetch_recursive(fetcher, leaf).await.unwrap();
        assert_eq!(got, b"hello");
    }

    #[tokio::test]
    async fn bundle_fetch_concatenates_children_in_order() {
        let a_bytes = b"aaa".to_vec();
        let b_bytes = b"bbbb".to_vec();
        let c_bytes = b"ccccc".to_vec();
        let a = ContentId::for_book(&a_bytes, ContentFlags::default()).unwrap();
        let b = ContentId::for_book(&b_bytes, ContentFlags::default()).unwrap();
        let c = ContentId::for_book(&c_bytes, ContentFlags::default()).unwrap();

        let mut builder = BundleBuilder::new();
        builder.add(a).add(b).add(c);
        let (payload, root) = builder.build_with_flags(ContentFlags::default()).unwrap();

        let mut store: HashMap<ContentId, Vec<u8>> = HashMap::new();
        store.insert(a, a_bytes.clone());
        store.insert(b, b_bytes.clone());
        store.insert(c, c_bytes.clone());
        store.insert(root, payload);

        let fetcher = move |cid: ContentId| {
            let bytes = store.get(&cid).cloned();
            std::future::ready(bytes.ok_or_else(|| format!("missing cid: {cid:?}")))
        };

        let got = fetch_recursive(fetcher, root).await.unwrap();
        let mut expected = a_bytes;
        expected.extend_from_slice(&b_bytes);
        expected.extend_from_slice(&c_bytes);
        assert_eq!(got, expected);
    }

    #[tokio::test]
    async fn missing_leaf_propagates_error() {
        let a = ContentId::for_book(b"aaa", ContentFlags::default()).unwrap();
        let b = ContentId::for_book(b"bbb", ContentFlags::default()).unwrap();
        let mut builder = BundleBuilder::new();
        builder.add(a).add(b);
        let (payload, root) = builder.build_with_flags(ContentFlags::default()).unwrap();

        let mut store: HashMap<ContentId, Vec<u8>> = HashMap::new();
        // Deliberately omit `b`.
        store.insert(a, b"aaa".to_vec());
        store.insert(root, payload);

        let fetcher = move |cid: ContentId| {
            let bytes = store.get(&cid).cloned();
            std::future::ready(bytes.ok_or_else(|| format!("missing cid: {cid:?}")))
        };

        let err = fetch_recursive(fetcher, root).await.unwrap_err();
        assert!(err.contains("missing cid"), "got: {err}");
    }
}

#[cfg(test)]
mod fetch_one_wrapper_tests {
    use super::{fetch_recursive, wrap_fetch_one_with_admission};
    use crate::content_store::CasOp;
    use harmony_content::bundle::BundleBuilder;
    use harmony_content::cid::{ContentFlags, ContentId};
    use std::collections::HashMap;
    use tokio::sync::mpsc;

    /// Drain whatever's still queued in `cas_op_rx` after a test has
    /// finished its fetch_recursive call. Used by tests that expect
    /// ZERO admits (e.g. fetch failure path); the synchronous-admission
    /// tests use `responder_collect_admits` instead.
    ///
    /// R1 (Cursor + Qodo): admissions are now synchronous (reply
    /// `Some(...)`), so this helper accepts either reply variant. Tests
    /// that assert empty queues don't care about reply shape.
    fn drain_admits(rx: &mut mpsc::Receiver<CasOp>) -> Vec<(ContentId, Vec<u8>)> {
        let mut out = Vec::new();
        while let Ok(op) = rx.try_recv() {
            match op {
                CasOp::PutLocal { cid, blob, .. } => {
                    out.push((cid, blob));
                }
                CasOp::GetOrFetch { .. } => {
                    panic!("wrapper must not send GetOrFetch");
                }
                CasOp::GetLocal { .. } => {
                    panic!("wrapper must not send GetLocal");
                }
            }
        }
        out
    }

    /// Spawned-task helper: ACKs each PutLocal so the synchronous
    /// wrapper can proceed, and collects (cid, blob) per admission.
    /// The task exits when all senders are dropped (`recv()` returns
    /// None) — which happens when `fetch_recursive` consumes the
    /// wrapped closure and returns, releasing the last `cas_op_tx`.
    async fn responder_collect_admits(mut rx: mpsc::Receiver<CasOp>) -> Vec<(ContentId, Vec<u8>)> {
        let mut admits: Vec<(ContentId, Vec<u8>)> = Vec::new();
        while let Some(op) = rx.recv().await {
            match op {
                CasOp::PutLocal { cid, blob, reply } => {
                    admits.push((cid, blob));
                    if let Some(reply) = reply {
                        let _ = reply.send(Ok(()));
                    }
                }
                CasOp::GetOrFetch { .. } => {
                    panic!("wrapper must not send GetOrFetch");
                }
                CasOp::GetLocal { .. } => {
                    panic!("wrapper must not send GetLocal");
                }
            }
        }
        admits
    }

    #[tokio::test]
    async fn admits_each_fetched_cid_for_a_bundle_tree() {
        // Bundle tree: root → [a, b, c]
        let a_bytes = b"aaa".to_vec();
        let b_bytes = b"bbbb".to_vec();
        let c_bytes = b"ccccc".to_vec();
        let a = ContentId::for_book(&a_bytes, ContentFlags::default()).unwrap();
        let b = ContentId::for_book(&b_bytes, ContentFlags::default()).unwrap();
        let c = ContentId::for_book(&c_bytes, ContentFlags::default()).unwrap();

        let mut builder = BundleBuilder::new();
        builder.add(a).add(b).add(c);
        let (payload, root) = builder.build_with_flags(ContentFlags::default()).unwrap();

        let mut store: HashMap<ContentId, Vec<u8>> = HashMap::new();
        store.insert(a, a_bytes.clone());
        store.insert(b, b_bytes.clone());
        store.insert(c, c_bytes.clone());
        store.insert(root, payload.clone());

        let fetcher = move |cid: ContentId| {
            let bytes = store.get(&cid).cloned();
            std::future::ready(bytes.ok_or_else(|| format!("missing cid: {cid:?}")))
        };

        let (cas_op_tx, cas_op_rx) = mpsc::channel::<CasOp>(16);
        let wrapped = wrap_fetch_one_with_admission(fetcher, cas_op_tx);

        // R1 (Cursor + Qodo): the wrapper now uses synchronous
        // admission, so each per-CID call blocks awaiting a PutLocal
        // reply. Drive a responder concurrent with fetch_recursive
        // that ACKs each PutLocal and collects (cid, blob). The
        // responder finishes when fetch_recursive returns and the
        // wrapped closure is dropped, releasing the last cas_op_tx.
        let responder = tokio::spawn(responder_collect_admits(cas_op_rx));

        // Drive through fetch_recursive — every per-CID call goes through
        // the wrapper, so every successful fetch must produce a PutLocal.
        let got = fetch_recursive(wrapped, root).await.unwrap();
        let admits = responder.await.unwrap();

        // fetch_recursive's output is the concatenated leaves (existing
        // contract; we don't break it).
        let mut expected_concat = a_bytes.clone();
        expected_concat.extend_from_slice(&b_bytes);
        expected_concat.extend_from_slice(&c_bytes);
        assert_eq!(got, expected_concat);

        // Admission: every CID encountered (root bundle + 3 leaves).
        assert_eq!(admits.len(), 4, "expected 4 admissions, got {:?}", admits);

        // Each admission carries the correct bytes for its CID.
        let admit_map: HashMap<ContentId, Vec<u8>> = admits.into_iter().collect();
        assert_eq!(admit_map.get(&root), Some(&payload));
        assert_eq!(admit_map.get(&a), Some(&a_bytes));
        assert_eq!(admit_map.get(&b), Some(&b_bytes));
        assert_eq!(admit_map.get(&c), Some(&c_bytes));
    }

    #[tokio::test]
    async fn skips_admit_on_fetch_failure() {
        // fetch_one returns Err for the requested CID. Verify no
        // CasOp::PutLocal was sent.
        let cid = ContentId::for_book(b"missing", ContentFlags::default()).unwrap();
        let fetcher = |_cid: ContentId| {
            std::future::ready(Err::<Vec<u8>, String>(
                "synthetic fetch failure".to_string(),
            ))
        };

        let (cas_op_tx, mut cas_op_rx) = mpsc::channel::<CasOp>(4);
        let wrapped = wrap_fetch_one_with_admission(fetcher, cas_op_tx);

        let result = wrapped(cid).await;
        assert!(
            result.is_err(),
            "expected Err propagation; got {:?}",
            result
        );
        assert!(result.unwrap_err().contains("synthetic fetch failure"));

        // No admission should have been sent.
        let admits = drain_admits(&mut cas_op_rx);
        assert!(
            admits.is_empty(),
            "wrapper must not admit on fetch failure; got {:?}",
            admits
        );
    }

    #[tokio::test]
    async fn admit_failure_does_not_fail_fetch() {
        // cas_op channel is closed (receiver dropped). The wrapper's
        // synchronous send returns Err but the wrapper must NOT
        // propagate that — the caller still gets the fetched bytes.
        let bytes = b"payload".to_vec();
        let cid = ContentId::for_book(&bytes, ContentFlags::default()).unwrap();
        let bytes_for_fetcher = bytes.clone();
        let fetcher = move |_cid: ContentId| {
            let b = bytes_for_fetcher.clone();
            std::future::ready(Ok::<Vec<u8>, String>(b))
        };

        let (cas_op_tx, cas_op_rx) = mpsc::channel::<CasOp>(1);
        drop(cas_op_rx); // close the receiver — every send will Err.
        let wrapped = wrap_fetch_one_with_admission(fetcher, cas_op_tx);

        let result = wrapped(cid).await;
        assert!(
            result.is_ok(),
            "admission failure must not propagate to fetch caller; got {:?}",
            result
        );
        assert_eq!(result.unwrap(), bytes);
    }

    /// R2 (CodeRabbit): the wrapper bounds both `cas_op_tx.send` and
    /// `reply_rx.await` by `ADMISSION_TIMEOUT`. If the event-loop CAS
    /// arm is stalled (we simulate this by receiving the PutLocal but
    /// never replying), the wrapper must time out and still return
    /// Ok(bytes) — admission is best-effort, not load-bearing for the
    /// fetch caller. Uses `start_paused = true` so virtual time
    /// auto-advances when the runtime is idle, keeping wall-clock test
    /// time near-zero per the project's wall-clock-regression rule.
    #[tokio::test(start_paused = true)]
    async fn admission_timeout_does_not_fail_fetch() {
        let bytes = b"payload".to_vec();
        let cid = ContentId::for_book(&bytes, ContentFlags::default()).unwrap();
        let bytes_for_fetcher = bytes.clone();
        let fetcher = move |_cid: ContentId| {
            let b = bytes_for_fetcher.clone();
            std::future::ready(Ok::<Vec<u8>, String>(b))
        };

        let (cas_op_tx, mut cas_op_rx) = mpsc::channel::<CasOp>(1);
        let wrapped = wrap_fetch_one_with_admission(fetcher, cas_op_tx);

        // Receiver pulls the PutLocal but parks forever, holding the
        // PutLocal (and its reply_tx) in scope so the wrapper's
        // reply_rx never resolves naturally. The wrapper's
        // `tokio::time::timeout(ADMISSION_TIMEOUT, reply_rx)` must
        // fire and let the wrapper return Ok.
        let receiver = tokio::spawn(async move {
            let _op = cas_op_rx.recv().await.expect("expected a PutLocal");
            std::future::pending::<()>().await;
        });

        let result = wrapped(cid).await;
        receiver.abort();

        assert!(
            result.is_ok(),
            "admission timeout must not propagate to fetch caller; got {:?}",
            result
        );
        assert_eq!(result.unwrap(), bytes);
    }

    #[tokio::test]
    async fn wrap_rejects_bytes_that_fail_hash_eq_cid() {
        let real = b"the real avatar bytes";
        let cid = ContentId::for_book(real, ContentFlags::default()).unwrap();
        let (cas_op_tx, _cas_op_rx) = mpsc::channel::<CasOp>(8);
        let fetcher =
            move |_cid: ContentId| std::future::ready(Ok::<Vec<u8>, String>(b"TAMPERED".to_vec()));
        let wrapped = wrap_fetch_one_with_admission(fetcher, cas_op_tx);
        let result = wrapped(cid).await;
        assert!(
            result.is_err(),
            "tampered bytes must be rejected, not returned"
        );
        assert!(
            result.unwrap_err().contains("hash"),
            "error should mention the hash==CID verification failure"
        );
    }
}

#[cfg(test)]
mod content_verb_tests {
    use super::ContentVerbRequest;

    #[test]
    fn read_bytes_verb_variant_is_constructible() {
        let (reply_tx, _reply_rx) = tokio::sync::oneshot::channel::<Option<Vec<u8>>>();
        let req = ContentVerbRequest::ReadBytes {
            cid: [0x7Au8; 32],
            reply: reply_tx,
        };
        match req {
            ContentVerbRequest::ReadBytes { cid, .. } => {
                assert_eq!(cid, [0x7Au8; 32]);
            }
            _ => panic!("matched wrong variant"),
        }
    }
}

/// Bridge Zenoh subscription messages to Tauri frontend events.
#[allow(clippy::too_many_arguments)]
fn emit_frontend_event<R: Runtime>(
    app: &AppHandle<R>,
    key_expr: &str,
    payload: &[u8],
    hop_distance: Option<u8>,
    followed_set: &std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    vine_feed_cache: &std::sync::Arc<std::sync::Mutex<crate::vine_feed_cache::VineFeedCache>>,
    mail_mgr: &std::sync::Arc<std::sync::Mutex<crate::mail::MailManager>>,
    own_mail_key: &str,
    own_root_key: &str,
    mail_sync: Option<&Arc<crate::mail_sync::MailSync<R>>>,
) {
    if key_expr.starts_with("harmony/compute/capacity/") {
        if let Some(mut update) = crate::parse_capacity(key_expr, payload) {
            update.hop_distance = hop_distance;
            let _ = app.emit("capacity-update", &update);
        }
    } else if key_expr.starts_with("harmony/profile/") {
        if let Ok(profile) = serde_json::from_slice::<crate::ProfilePayload>(payload) {
            let _ = app.emit("profile-update", &profile);
        }
    } else if key_expr.starts_with("harmony/community/") {
        if let Ok(msg) = serde_json::from_slice::<crate::ChannelMessagePayload>(payload) {
            let _ = app.emit("message-received", &msg);
        }
    } else if key_expr.starts_with("harmony/vines/") {
        if key_expr.contains("/reactions/") {
            // ZEB-286: route reaction through the cache. Re-emit to the
            // frontend ONLY on Inserted or UpdatedNewer (stale/duplicate
            // re-arrivals are absorbed silently). The cache's per-LWW
            // dedupe replaces the previous naive every-sample emit.
            let outcome = match vine_feed_cache.lock() {
                Ok(mut cache) => cache.on_reaction_sample(key_expr, payload),
                Err(e) => {
                    tracing::error!(error = %e, "vine_feed_cache mutex poisoned; skipping reaction emit");
                    None
                }
            };
            if matches!(
                outcome,
                Some(
                    crate::vine_feed_cache::ReactionOutcome::Inserted
                        | crate::vine_feed_cache::ReactionOutcome::UpdatedNewer
                )
            ) {
                if let Ok(reaction) = serde_json::from_slice::<crate::VineReactionPayload>(payload)
                {
                    let _ = app.emit("vine-reaction-received", &reaction);
                }
            }
        } else {
            // ZEB-286: route descriptor through the cache. Source-tag
            // (Followed vs Discover) is decided by the cache once at
            // first insert; re-arrivals are absorbed. The cache returns
            // the ready-to-emit VineVideoDtoWithSource so we do not have
            // to re-parse + re-mutate JSON here.
            let now_ms = u64::try_from(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0),
            )
            .unwrap_or(u64::MAX);
            let outcome = match vine_feed_cache.lock() {
                Ok(mut cache) => match followed_set.lock() {
                    Ok(set) => cache.on_descriptor_sample(key_expr, payload, &set, now_ms),
                    Err(e) => {
                        tracing::error!(error = %e, "followed_set mutex poisoned; skipping descriptor emit");
                        None
                    }
                },
                Err(e) => {
                    tracing::error!(error = %e, "vine_feed_cache mutex poisoned; skipping descriptor emit");
                    None
                }
            };
            if let Some(crate::vine_feed_cache::DescriptorOutcome::Inserted { dto }) = outcome {
                let _ = app.emit("vine-received", &dto);
            }
        }
    } else if key_expr.starts_with("harmony/announce/") {
        if let Some(announcement) = crate::parse_content_announcement(key_expr, payload) {
            let _ = app.emit("content-announced", &announcement);
        }
    } else if key_expr.contains("/telemetry/") {
        if let Some(event) = crate::parse_telemetry(payload) {
            let _ = app.emit("telemetry-event", &event);
        }
    } else if !own_root_key.is_empty() && key_expr == own_root_key {
        // Phase 2: root CID push for this node's mailbox. Forward to
        // MailSync which re-walks the tree and registers header-only
        // entries for any new descendants. Spawn so the event loop
        // keeps pumping while the walker runs.
        if let Some(sync) = mail_sync {
            let sync = Arc::clone(sync);
            let payload = payload.to_vec();
            tokio::spawn(async move {
                sync.handle_root_push(&payload).await;
            });
        } else {
            tracing::debug!("got root push but mail_sync not initialized; ignoring");
        }
    } else if !own_mail_key.is_empty() && key_expr == own_mail_key {
        // Inbound mail delivery — store in MailManager and notify frontend.
        // NOTE: receive_message performs blocking disk I/O (blob write + index
        // persist) while holding the mutex. Acceptable for Phase 0 since mail
        // is infrequent. Phase 1 should offload to spawn_blocking or a
        // dedicated writer thread to avoid stalling the event loop under burst.
        //
        // Emit `mail-received` only on a fresh Insert. A Promoted outcome
        // means the walker already surfaced this row via register_header_only,
        // so re-emitting would duplicate the notification the user already saw.
        let mut mgr = match mail_mgr.lock() {
            Ok(g) => g,
            Err(e) => {
                tracing::error!(error = %e, "mail_mgr mutex poisoned");
                return;
            }
        };
        match mgr.receive_message(payload) {
            Ok(crate::mail::ReceiveOutcome::Inserted(entry)) => {
                let _ = app.emit("mail-received", &entry);
            }
            Ok(crate::mail::ReceiveOutcome::Promoted(_entry)) => {
                tracing::debug!(key_expr, "live push promoted Pending to Local (no emit)");
            }
            Err(e) => {
                tracing::debug!(key_expr, error = %e, "mail receive skipped");
            }
        }
    }
}

// ── ZEB-217 Sub-C Phase 2 Task 12: per-community state Zenoh adapter ──────
//
// Mirrors the owner-state adapter at lines 273-385 above, with the topic
// substituted for a per-community key expression and the Tauri AppHandle /
// `state-root-sync-degraded` emit removed. Per the Phase 2 design, transport
// degradation flows through the engine's `error_tx` channel; the registry's
// drain task (Task 13) converts those reports into the
// `community-state-sync-degraded` Tauri event. So this adapter logs+lets
// the channel close on transport failure and trusts the engine's
// `subscriber_channel_closed` degraded report to surface it.

/// Spawn a Zenoh publisher + subscriber for one community's state-root
/// topic (`harmony/community/{id_hex}/state-root-v1`).
///
/// Wires:
///   - `publisher_rx` (engine's outbound bytes) → `session.put(key, bytes)`
///   - Zenoh subscriber on the same key → `subscriber_tx` (engine's inbound)
///
/// `closing` is the event-loop-wide shutdown flag; when set, transport
/// errors are downgraded to silence so a clean `stop_node` doesn't spam
/// "publish failed" / "subscriber closed unexpectedly" warnings.
///
/// Returns a `JoinHandle<()>` so the registry / `start_node` can await
/// teardown if needed. Internally spawns two child tasks (publisher and
/// subscriber) and joins them before the outer handle resolves.
///
/// On failure to construct a `KeyExpr` from the topic string, the function
/// logs and returns a JoinHandle that resolves immediately — both
/// `publisher_rx` and `subscriber_tx` drop here, which the engine sees as
/// transport-closed (publish-only / fully-degraded mode).
pub fn spawn_community_state_zenoh_adapter(
    session: Arc<zenoh::Session>,
    community_id_hex: String,
    mut publisher_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    subscriber_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    closing: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    let topic = format!("harmony/community/{}/state-root-v1", community_id_hex);

    tokio::spawn(async move {
        let key_expr = match zenoh::key_expr::KeyExpr::try_from(topic.clone()) {
            Ok(k) => k,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    %topic,
                    "community state-root key_expr invalid; adapter skipped"
                );
                // publisher_rx and subscriber_tx drop on this arm's exit;
                // engine's transport sees both channels close and falls
                // into degraded mode.
                return;
            }
        };

        // Outbound: drain engine's publisher_rx → Zenoh put.
        let session_pub = Arc::clone(&session);
        let key_pub = key_expr.clone();
        let topic_pub = topic.clone();
        let closing_pub = Arc::clone(&closing);
        let pub_handle = tokio::spawn(async move {
            // Bounded-time shutdown: poll `closing` every second so a
            // node-stop event terminates the publisher within ~1s even
            // if no bytes are flowing on `publisher_rx`. Without this,
            // the outer JoinHandle this fn returns could only resolve
            // when the engine drops its publisher_tx — fine under the
            // documented teardown order (registry.shutdown_all first),
            // but easy for a future caller to misuse.
            loop {
                tokio::select! {
                    // Data-flow arm first: when both arms are ready
                    // (i.e., a byte is queued AND the 1s timer fires)
                    // the actual publish wins. With the previous arm
                    // order the biased eval would always pick the
                    // closing-check, delaying every collision-case
                    // publish by one loop iteration.
                    biased;
                    maybe = publisher_rx.recv() => {
                        let Some(bytes) = maybe else { break; };
                        if let Err(e) = session_pub.put(&key_pub, bytes).await {
                            if !closing_pub.load(Ordering::SeqCst) {
                                tracing::warn!(
                                    topic = %topic_pub,
                                    error = %e,
                                    "community state-root publish failed"
                                );
                            }
                        }
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                        if closing_pub.load(Ordering::SeqCst) { break; }
                    }
                }
            }
        });

        // Inbound: Zenoh subscriber → engine's subscriber_tx.
        let session_sub = session;
        let key_sub = key_expr;
        let topic_sub = topic;
        let closing_sub = Arc::clone(&closing);
        let sub_handle = tokio::spawn(async move {
            let sub = match session_sub.declare_subscriber(&key_sub).await {
                Ok(s) => s,
                Err(e) => {
                    if !closing_sub.load(Ordering::SeqCst) {
                        tracing::error!(
                            topic = %topic_sub,
                            error = %e,
                            "failed to declare community state-root subscriber"
                        );
                    }
                    // subscriber_tx drops on this arm's exit; engine's
                    // subscriber_rx hits None and latches inbound_closed,
                    // continuing in publish-only mode.
                    return;
                }
            };
            // Three ways the loop ends:
            //   1. `subscriber_tx.send` fails — engine cleanly shut down
            //      (registry tore the engine down). Stay silent so a
            //      routine community-leave / shutdown doesn't log.
            //   2. `sub.recv_async` returns Err — Zenoh session/subscriber
            //      died. Warn (gated on !closing) and exit; the engine's
            //      own subscriber_channel_closed degraded report covers
            //      surface-level visibility.
            //   3. `closing` flag flips — bounded-time shutdown, mirrors
            //      the publisher arm above.
            //   4. `subscriber_tx.closed()` resolves — the engine
            //      dropped its subscriber_rx (e.g., registry.stop_engine
            //      tore down a community while no inbound was flowing).
            //      Without this arm the loop stays blocked on
            //      `sub.recv_async` until the next sample arrives,
            //      leaving the JoinHandle unresolved indefinitely.
            loop {
                tokio::select! {
                    // Data-flow arm first (see publisher loop above
                    // for rationale). If `subscriber_tx.closed()`
                    // resolves on the same poll as an inbound sample,
                    // delivering the sample is harmless: the
                    // subsequent `subscriber_tx.send` returns Err and
                    // breaks the loop on the next iteration. Putting
                    // `closed()` first instead would silently discard
                    // that sample — contradicting the documented
                    // intent and masking edge-case message loss
                    // during teardown.
                    biased;
                    res = sub.recv_async() => {
                        match res {
                            Ok(sample) => {
                                let bytes: Vec<u8> = sample.payload().to_bytes().to_vec();
                                if subscriber_tx.send(bytes).await.is_err() {
                                    break;
                                }
                            }
                            Err(e) => {
                                if !closing_sub.load(Ordering::SeqCst) {
                                    tracing::warn!(
                                        topic = %topic_sub,
                                        error = %e,
                                        "community state-root subscriber closed unexpectedly"
                                    );
                                }
                                break;
                            }
                        }
                    }
                    _ = subscriber_tx.closed() => {
                        // Engine dropped subscriber_rx — nothing to
                        // forward to anymore. Silent exit; engine
                        // owns the shutdown trace if relevant.
                        break;
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                        if closing_sub.load(Ordering::SeqCst) { break; }
                    }
                }
            }
        });

        let _ = pub_handle.await;
        let _ = sub_handle.await;
    })
}

/// ZEB-298+ZEB-312 PR 1: per-community Zenoh adapter for the VotingLog
/// data plane. Structurally identical to
/// `spawn_community_state_zenoh_adapter` — pub/sub only (no queryable,
/// no backfill in PR 1). Topic: `harmony/community/{id_hex}/voting`.
///
/// The function is fire-and-forget: `event_loop::run`'s select! arm
/// calls it and drops the `JoinHandle`. The spawned task exits when
/// `closing` is set, when the engine drops its publisher_tx, or when
/// Zenoh closes the subscriber. Engine teardown is driven by the
/// `VotingLogEnginesMap` lock in stop_inner (same as state-root v1).
pub fn spawn_voting_log_zenoh_adapter(
    session: Arc<zenoh::Session>,
    community_id_hex: String,
    mut publisher_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    subscriber_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    closing: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    let topic = format!("harmony/community/{}/voting", community_id_hex);

    tokio::spawn(async move {
        let key_expr = match zenoh::key_expr::KeyExpr::try_from(topic.clone()) {
            Ok(k) => k,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    %topic,
                    "community voting-log key_expr invalid; adapter skipped"
                );
                // publisher_rx and subscriber_tx drop on this arm's exit;
                // engine's transport sees both channels close and falls
                // into degraded mode.
                return;
            }
        };

        // Outbound: drain engine's publisher_rx → Zenoh put.
        let session_pub = Arc::clone(&session);
        let key_pub = key_expr.clone();
        let topic_pub = topic.clone();
        let closing_pub = Arc::clone(&closing);
        let pub_handle = tokio::spawn(async move {
            // Bounded-time shutdown: poll `closing` every second so a
            // node-stop event terminates the publisher within ~1s even
            // if no bytes are flowing on `publisher_rx`. Without this,
            // the outer JoinHandle this fn returns could only resolve
            // when the engine drops its publisher_tx — fine under the
            // documented teardown order (registry.shutdown_all first),
            // but easy for a future caller to misuse.
            loop {
                tokio::select! {
                    // Data-flow arm first: when both arms are ready
                    // (i.e., a byte is queued AND the 1s timer fires)
                    // the actual publish wins. With the previous arm
                    // order the biased eval would always pick the
                    // closing-check, delaying every collision-case
                    // publish by one loop iteration.
                    biased;
                    maybe = publisher_rx.recv() => {
                        let Some(bytes) = maybe else { break; };
                        if let Err(e) = session_pub.put(&key_pub, bytes).await {
                            if !closing_pub.load(Ordering::SeqCst) {
                                tracing::warn!(
                                    topic = %topic_pub,
                                    error = %e,
                                    "community voting-log publish failed"
                                );
                            }
                        }
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                        if closing_pub.load(Ordering::SeqCst) { break; }
                    }
                }
            }
        });

        // Inbound: Zenoh subscriber → engine's subscriber_tx.
        let session_sub = session;
        let key_sub = key_expr;
        let topic_sub = topic;
        let closing_sub = Arc::clone(&closing);
        let sub_handle = tokio::spawn(async move {
            let sub = match session_sub.declare_subscriber(&key_sub).await {
                Ok(s) => s,
                Err(e) => {
                    if !closing_sub.load(Ordering::SeqCst) {
                        tracing::error!(
                            topic = %topic_sub,
                            error = %e,
                            "failed to declare community voting-log subscriber"
                        );
                    }
                    // subscriber_tx drops on this arm's exit; engine's
                    // subscriber_rx hits None and latches inbound_closed,
                    // continuing in publish-only mode.
                    return;
                }
            };
            // Three ways the loop ends:
            //   1. `subscriber_tx.send` fails — engine cleanly shut down
            //      (registry tore the engine down). Stay silent so a
            //      routine community-leave / shutdown doesn't log.
            //   2. `sub.recv_async` returns Err — Zenoh session/subscriber
            //      died. Warn (gated on !closing) and exit; the engine's
            //      own subscriber_channel_closed degraded report covers
            //      surface-level visibility.
            //   3. `closing` flag flips — bounded-time shutdown, mirrors
            //      the publisher arm above.
            //   4. `subscriber_tx.closed()` resolves — the engine
            //      dropped its subscriber_rx (e.g., registry.stop_engine
            //      tore down a community while no inbound was flowing).
            //      Without this arm the loop stays blocked on
            //      `sub.recv_async` until the next sample arrives,
            //      leaving the JoinHandle unresolved indefinitely.
            loop {
                tokio::select! {
                    // Data-flow arm first (see publisher loop above
                    // for rationale). If `subscriber_tx.closed()`
                    // resolves on the same poll as an inbound sample,
                    // delivering the sample is harmless: the
                    // subsequent `subscriber_tx.send` returns Err and
                    // breaks the loop on the next iteration. Putting
                    // `closed()` first instead would silently discard
                    // that sample — contradicting the documented
                    // intent and masking edge-case message loss
                    // during teardown.
                    biased;
                    res = sub.recv_async() => {
                        match res {
                            Ok(sample) => {
                                // Size cap: voting events are small CBOR envelopes
                                // (typically <2 KiB). Cap inbound payloads to prevent
                                // peer-controlled allocation attacks before we even
                                // decode for verification.
                                let payload_len = sample.payload().len();
                                const MAX_VOTING_PAYLOAD_BYTES: usize = 64 * 1024;
                                if payload_len > MAX_VOTING_PAYLOAD_BYTES {
                                    if !closing_sub.load(Ordering::SeqCst) {
                                        tracing::warn!(
                                            topic = %topic_sub,
                                            len = payload_len,
                                            max = MAX_VOTING_PAYLOAD_BYTES,
                                            "voting payload exceeds size cap; dropping"
                                        );
                                    }
                                    continue;
                                }
                                let bytes: Vec<u8> = sample.payload().to_bytes().to_vec();
                                if subscriber_tx.send(bytes).await.is_err() {
                                    break;
                                }
                            }
                            Err(e) => {
                                if !closing_sub.load(Ordering::SeqCst) {
                                    tracing::warn!(
                                        topic = %topic_sub,
                                        error = %e,
                                        "community voting-log subscriber closed unexpectedly"
                                    );
                                }
                                break;
                            }
                        }
                    }
                    _ = subscriber_tx.closed() => {
                        // Engine dropped subscriber_rx — nothing to
                        // forward to anymore. Silent exit; engine
                        // owns the shutdown trace if relevant.
                        break;
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                        if closing_sub.load(Ordering::SeqCst) { break; }
                    }
                }
            }
        });

        let _ = pub_handle.await;
        let _ = sub_handle.await;
    })
}

/// Per-(community, channel) Zenoh adapter for the ChannelLog data
/// plane (ZEB-270 / ZEB-248 Phase 3). Mirrors
/// `spawn_community_state_zenoh_adapter` in shape: spawns four
/// tokio tasks (publisher, subscriber, queryable, query-request
/// driver), all bound to the per-channel topics.
///
/// Topics:
/// - `harmony/channels/{cid_hex}/{ch_id_hex}/events` — live broadcast
/// - `harmony/channels/{cid_hex}/{ch_id_hex}/since/{hlc_hex}/{limit}` — queryable
///
/// The `read_for_query` callback is what the queryable handler uses
/// to fetch events for a backfill request — passed in to avoid
/// the engine ↔ adapter circular dep (per spec §8.1).
#[allow(clippy::too_many_arguments)] // Signature locked by spec §8 + plan Task 3.
pub fn spawn_channel_log_zenoh_adapter<F>(
    session: Arc<zenoh::Session>,
    community_id_hex: String,
    channel_id_hex: String,
    mut publisher_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    subscriber_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    mut query_request_rx: tokio::sync::mpsc::Receiver<
        crate::community_channel_log_engine::BackfillQueryRequest,
    >,
    read_for_query: Arc<F>,
    emit_backfill_progress: Arc<dyn Fn(u32, Option<u32>) + Send + Sync + 'static>,
    backfill_progress_interval: usize,
    backfill_default_limit: usize,
    closing: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()>
where
    // `?Sized` so callers can pass `Arc<dyn Fn(...) + Send + Sync>`
    // — the production bridge (ChannelLogAdapterRequest) carries the
    // closure as a trait object so it can be packed into an mpsc with
    // a uniform type. Concrete `Arc<F>` callers (the existing
    // event_loop unit tests) still compile under the relaxed bound.
    F: Fn(
            Option<crate::owner_state_types::Hlc>,
            usize,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<Vec<u8>>> + Send>>
        + Send
        + Sync
        + ?Sized
        + 'static,
{
    let events_topic = format!(
        "harmony/channels/{}/{}/events",
        community_id_hex, channel_id_hex
    );
    let queryable_prefix = format!(
        "harmony/channels/{}/{}/since/**",
        community_id_hex, channel_id_hex
    );

    tokio::spawn(async move {
        // Spawn-stop race fast path: if closing was flipped after the
        // request was queued but before this task started, exit
        // immediately without declaring Zenoh resources or holding the
        // read_for_query closure (which keeps the engine alive).
        if closing.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }

        let events_key = match zenoh::key_expr::KeyExpr::try_from(events_topic.clone()) {
            Ok(k) => k,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    %events_topic,
                    "channel-log events key_expr invalid; adapter skipped"
                );
                return;
            }
        };
        let queryable_key = match zenoh::key_expr::KeyExpr::try_from(queryable_prefix.clone()) {
            Ok(k) => k,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    %queryable_prefix,
                    "channel-log queryable key_expr invalid; adapter skipped"
                );
                return;
            }
        };

        // ── Publisher task ─────────────────────────────────────────
        let session_pub = Arc::clone(&session);
        let key_pub = events_key.clone();
        let topic_pub = events_topic.clone();
        let closing_pub = Arc::clone(&closing);
        let pub_handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    maybe = publisher_rx.recv() => {
                        let Some(bytes) = maybe else { break; };
                        if let Err(e) = session_pub.put(&key_pub, bytes).await {
                            if !closing_pub.load(Ordering::SeqCst) {
                                tracing::warn!(
                                    topic = %topic_pub,
                                    error = %e,
                                    "channel-log publish failed"
                                );
                            }
                        }
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                        if closing_pub.load(Ordering::SeqCst) { break; }
                    }
                }
            }
        });

        // ── Subscriber task ────────────────────────────────────────
        let session_sub = Arc::clone(&session);
        let key_sub = events_key.clone();
        let topic_sub = events_topic.clone();
        let subscriber_tx_sub = subscriber_tx.clone();
        let closing_sub = Arc::clone(&closing);
        let sub_handle = tokio::spawn(async move {
            let sub = match session_sub.declare_subscriber(&key_sub).await {
                Ok(s) => s,
                Err(e) => {
                    if !closing_sub.load(Ordering::SeqCst) {
                        tracing::error!(
                            topic = %topic_sub,
                            error = %e,
                            "failed to declare channel-log subscriber"
                        );
                    }
                    return;
                }
            };
            loop {
                tokio::select! {
                    biased;
                    res = sub.recv_async() => {
                        match res {
                            Ok(sample) => {
                                let bytes: Vec<u8> = sample.payload().to_bytes().to_vec();
                                if subscriber_tx_sub.send(bytes).await.is_err() {
                                    break;
                                }
                            }
                            Err(e) => {
                                if !closing_sub.load(Ordering::SeqCst) {
                                    tracing::warn!(
                                        topic = %topic_sub,
                                        error = %e,
                                        "channel-log subscriber closed unexpectedly"
                                    );
                                }
                                break;
                            }
                        }
                    }
                    _ = subscriber_tx_sub.closed() => break,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                        if closing_sub.load(Ordering::SeqCst) { break; }
                    }
                }
            }
        });

        // ── Queryable task ─────────────────────────────────────────
        let session_qbl = Arc::clone(&session);
        let key_qbl = queryable_key.clone();
        let prefix_qbl = queryable_prefix.clone();
        let read_for_query_qbl = Arc::clone(&read_for_query);
        let closing_qbl = Arc::clone(&closing);
        let backfill_default_limit_qbl = backfill_default_limit;
        let qbl_handle = tokio::spawn(async move {
            let qbl = match session_qbl.declare_queryable(&key_qbl).await {
                Ok(q) => q,
                Err(e) => {
                    if !closing_qbl.load(Ordering::SeqCst) {
                        tracing::error!(
                            prefix = %prefix_qbl,
                            error = %e,
                            "failed to declare channel-log queryable"
                        );
                    }
                    return;
                }
            };
            loop {
                tokio::select! {
                    biased;
                    res = qbl.recv_async() => {
                        let Ok(query) = res else { break; };
                        let qkey = query.key_expr().to_string();
                        // Reject malformed selectors outright instead of
                        // silently widening to a full backfill. A bad
                        // selector like `.../since/not_hlc/500` previously
                        // collapsed to `since=None` and served the entire
                        // log — broader result set than the requester
                        // asked for, and masked protocol bugs. Now we
                        // skip the reply (continue to next query) and
                        // log at debug.
                        let ParsedBackfillKey::Valid { since, limit: limit_raw } =
                            parse_channel_backfill_key(&qkey)
                        else {
                            tracing::debug!(%qkey, "ignoring malformed channel-log backfill selector");
                            continue;
                        };
                        // Clamp peer-controlled limit per spec §6.2 (hard
                        // cap 1000). limit=0 falls back to per-engine
                        // default sourced from
                        // `ChannelLogEngineConfig.backfill_default_limit`
                        // (also clamped to MAX so a misconfigured engine
                        // can't blow past the server-side reply-storm
                        // bound). Defense-in-depth: the qr-driver below
                        // applies the same clamp before the GET selector
                        // is built.
                        let limit = if limit_raw == 0 {
                            backfill_default_limit_qbl.min(CHANNEL_BACKFILL_MAX_LIMIT)
                        } else {
                            limit_raw.min(CHANNEL_BACKFILL_MAX_LIMIT)
                        };
                        let packets = (read_for_query_qbl)(since, limit).await;
                        for packet in packets {
                            if let Err(e) = query
                                .reply(query.key_expr(), packet)
                                .await
                            {
                                tracing::warn!(
                                    prefix = %prefix_qbl,
                                    error = %e,
                                    "channel-log queryable reply failed"
                                );
                                break;
                            }
                        }
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                        if closing_qbl.load(Ordering::SeqCst) { break; }
                    }
                }
            }
        });

        // ── Query-request driver ───────────────────────────────────
        let session_qr = Arc::clone(&session);
        let community_id_hex_qr = community_id_hex.clone();
        let channel_id_hex_qr = channel_id_hex.clone();
        let subscriber_tx_qr = subscriber_tx.clone();
        let closing_qr = Arc::clone(&closing);
        let emit_backfill_progress_qr = Arc::clone(&emit_backfill_progress);
        let backfill_default_limit_qr = backfill_default_limit;
        let qr_handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    maybe = query_request_rx.recv() => {
                        let Some(req) = maybe else { break; };
                        // Clamp our own request before encoding (defense
                        // in depth — also prevents a misbehaving local
                        // engine from issuing oversized requests). The
                        // per-engine `backfill_default_limit` (sourced
                        // from `ChannelLogEngineConfig` at registry
                        // spawn time, plumbed through
                        // `ChannelLogAdapterRequest`) replaces the
                        // previous hardcoded `CHANNEL_BACKFILL_DEFAULT_LIMIT`
                        // — config overrides now take effect. The MAX
                        // cap stays as the constant (server-side hard
                        // cap independent of engine config).
                        let limit = if req.limit == 0 {
                            backfill_default_limit_qr.min(CHANNEL_BACKFILL_MAX_LIMIT)
                        } else {
                            req.limit.min(CHANNEL_BACKFILL_MAX_LIMIT)
                        };
                        let since_hex = match &req.since {
                            Some(h) => format_hlc_hex(h),
                            None => "0".to_string(),
                        };
                        let key = format!(
                            "harmony/channels/{}/{}/since/{}/{}",
                            community_id_hex_qr, channel_id_hex_qr, since_hex, limit
                        );
                        // ConsolidationMode::None: backfill streams ALL
                        // per-event reply packets back from the queryable
                        // (spec §17.1: per-event packets, wire-identical
                        // to live broadcasts). Default consolidation
                        // (Auto → Latest) collapses to a single reply per
                        // source key, dropping every event but one.
                        // Mirrors the `mailbox_get_first_value` shape at
                        // `event_loop.rs:1903`.
                        let receiver = match session_qr
                            .get(&key)
                            .consolidation(zenoh::query::ConsolidationMode::None)
                            .await
                        {
                            Ok(r) => r,
                            Err(e) => {
                                if !closing_qr.load(Ordering::SeqCst) {
                                    tracing::warn!(
                                        %key,
                                        error = %e,
                                        "channel-log backfill query failed"
                                    );
                                }
                                continue;
                            }
                        };
                        let mut fetched: u32 = 0;
                        // Inner reply-drain loop with closing-poll arm.
                        // `recv_async()` blocks until the reply stream
                        // closes; if the peer hangs (partition / dropped
                        // session / silent peer) it can block forever.
                        // Wrap in `select!` so a flipped closing flag
                        // unblocks teardown within ~500ms instead of
                        // waiting on the outer 1s closing-poll AFTER the
                        // hung recv eventually returns. The 500ms inner
                        // poll is tighter than the outer 1s because
                        // backfill is user-triggered and stop() latency
                        // is a UX concern.
                        let drained_clean: bool = loop {
                            tokio::select! {
                                biased;
                                res = receiver.recv_async() => {
                                    let Ok(reply) = res else { break true; };
                                    if let Ok(sample) = reply.into_result() {
                                        let bytes: Vec<u8> = sample.payload().to_bytes().to_vec();
                                        if subscriber_tx_qr.send(bytes).await.is_err() {
                                            // subscriber_rx dropped (engine
                                            // teardown). No point serving more
                                            // backfill requests if we can't
                                            // deliver replies — exit the qr
                                            // task entirely so we don't loop
                                            // back, fire another session.get,
                                            // and spin until the 1s closing
                                            // poll catches up.
                                            return;
                                        }
                                        fetched = fetched.saturating_add(1);
                                        // Spec §10: emit channel-backfill-progress
                                        // every N replies. `total_estimate` is
                                        // `None` — we don't know the total until
                                        // the receiver closes (Zenoh streams
                                        // replies one-at-a-time).
                                        if backfill_progress_interval > 0
                                            && (fetched as usize)
                                                .is_multiple_of(backfill_progress_interval)
                                        {
                                            (emit_backfill_progress_qr)(fetched, None);
                                        }
                                    }
                                }
                                _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {
                                    if closing_qr.load(Ordering::SeqCst) {
                                        // Adapter is tearing down. Don't
                                        // emit a final progress tick
                                        // (consumer is going away) — exit
                                        // immediately and let the outer
                                        // closing-poll arm break the loop.
                                        break false;
                                    }
                                }
                            }
                        };
                        // Spec §10: emit a final progress tick at end-of-
                        // request. We always fire on `drained_clean`
                        // (including `fetched == 0`) so the UI can
                        // distinguish "backfill finished with zero
                        // results" from "backfill is still in flight" —
                        // a zero-result drain is otherwise invisible.
                        // `total_estimate = Some(fetched)` is the true
                        // total now that the reply stream has closed
                        // naturally; this lets the UI tell apart
                        // periodic mid-drain ticks (where total is
                        // unknown, `None`) from the terminal one.
                        // Skip on shutdown — the consumer is going away
                        // and a final tick after the closing flag
                        // flipped is racy noise.
                        if drained_clean {
                            (emit_backfill_progress_qr)(fetched, Some(fetched));
                        }
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                        if closing_qr.load(Ordering::SeqCst) { break; }
                    }
                }
            }
        });

        let _ = pub_handle.await;
        let _ = sub_handle.await;
        let _ = qbl_handle.await;
        let _ = qr_handle.await;
    })
}

/// ZEB-343: the peer-to-peer CAS serve primitive. Declares a single Zenoh
/// queryable on `harmony/content/*/**` and answers content GETs for PUBLIC
/// (unencrypted) CIDs held in the local store.
///
/// `lookup` is the local-store accessor (production wires it to a
/// `CasOp::GetLocal` round-trip; tests wire a HashMap) — passed in to avoid an
/// engine↔adapter circular dep, exactly like channel-log's `read_for_query`
/// (event_loop.rs:4073).
///
/// Serve gate: a CID is servable iff `!cid.flags().encrypted` (spec §5.2). This
/// is intrinsic to the CID (its header's leading bit) and holds per-chunk, so
/// no public-membership registry is needed. Encrypted CIDs get no reply.
///
/// Returned bytes are inherently integrity-safe: the local cache only admits
/// bytes that passed `hash==cid` (StorageTier::verify_cid), so anything `lookup`
/// returns already verifies. We still re-check `cid.verify_hash` before replying
/// as defense-in-depth (cheap; never serve corrupt bytes).
#[allow(clippy::type_complexity)]
pub async fn spawn_content_serve_queryable<F>(
    session: Arc<zenoh::Session>,
    lookup: Arc<F>,
    closing: Arc<AtomicBool>,
) -> Result<tokio::task::JoinHandle<()>, String>
where
    F: Fn(
            ContentId,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<Vec<u8>>> + Send>>
        + Send
        + Sync
        + ?Sized
        + 'static,
{
    let key_pattern = "harmony/content/*/**".to_string();

    // Declare the queryable SYNCHRONOUSLY (await before returning) so the caller
    // can sequence node-readiness AFTER we are actually able to serve. If the
    // declare were inside the detached spawn, run()'s later readiness signal
    // could win the race and the node would report ready before the queryable
    // exists. A declare failure is propagated as Err so startup does NOT report
    // healthy with peer content-serving silently disabled (the caller routes it
    // into ready_tx, matching the other startup-failure paths).
    if closing.load(Ordering::SeqCst) {
        return Ok(tokio::spawn(async {}));
    }
    let qbl = match session.declare_queryable(&key_pattern).await {
        Ok(q) => q,
        Err(e) => {
            return Err(format!("failed to declare content-serve queryable: {e}"));
        }
    };

    Ok(tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                res = qbl.recv_async() => {
                    let Ok(query) = res else { break; };
                    let qkey = query.key_expr().to_string();
                    let Some(cid) = parse_content_serve_cid(&qkey) else {
                        continue;
                    };
                    if cid.flags().encrypted {
                        continue;
                    }
                    let Some(bytes) = (lookup)(cid).await else {
                        continue;
                    };
                    if !cid.verify_hash(&bytes) {
                        tracing::warn!(%qkey, "content-serve: local bytes failed hash==cid; not serving");
                        continue;
                    }
                    if let Err(e) = query.reply(query.key_expr(), bytes).await {
                        tracing::warn!(%qkey, error = %e, "content-serve reply failed");
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                    if closing.load(Ordering::SeqCst) { break; }
                }
            }
        }
    }))
}

/// Parse a `harmony/content/{shard}/{cid_hex}` serve key into a ContentId.
/// Requires EXACTLY 4 slash-segments, a single-hex shard char, and a 64-hex
/// cid. Returns None for publish/transit/stats keys or any malformed selector.
fn parse_content_serve_cid(key: &str) -> Option<ContentId> {
    let segs: Vec<&str> = key.split('/').collect();
    if segs.len() != 4 || segs[0] != "harmony" || segs[1] != "content" {
        return None;
    }
    let shard = segs[2];
    if shard.len() != 1 || !shard.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let cid_hex = segs[3];
    if cid_hex.len() != 64 || !cid_hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    // Enforce the sharding invariant: the shard token MUST equal the CID's 2nd
    // hex nibble (how fetch_via_zenoh derives it). Reject a CID requested under a
    // mismatched shard so a peer can't address content off its canonical shard.
    if shard != &cid_hex[1..2] {
        return None;
    }
    let raw = hex::decode(cid_hex).ok()?;
    let arr: [u8; 32] = raw.try_into().ok()?;
    Some(ContentId::from_bytes(arr))
}

#[cfg(test)]
mod content_serve_parse_tests {
    use super::parse_content_serve_cid;

    const HEX64: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn valid_serve_key_parses() {
        // Shard must match the CID's 2nd hex nibble (HEX64[1..2] == "1").
        let key = format!("harmony/content/1/{HEX64}");
        assert!(parse_content_serve_cid(&key).is_some());
    }

    #[test]
    fn mismatched_shard_rejected() {
        // A valid CID requested under a shard != its 2nd hex nibble is rejected
        // (sharding invariant; Greptile P2).
        let key = format!("harmony/content/3/{HEX64}");
        assert!(parse_content_serve_cid(&key).is_none());
    }

    #[test]
    fn publish_key_rejected() {
        let key = format!("harmony/content/publish/{HEX64}");
        assert!(parse_content_serve_cid(&key).is_none());
    }

    #[test]
    fn short_cid_rejected() {
        // 63 hex chars (one short of 64)
        let cid63 = &HEX64[..63];
        let key = format!("harmony/content/3/{cid63}");
        assert!(parse_content_serve_cid(&key).is_none());
    }

    #[test]
    fn five_segment_key_rejected() {
        let key = format!("harmony/content/3/{HEX64}/extra");
        assert!(parse_content_serve_cid(&key).is_none());
    }
}

/// Per spec §6.2: default backfill limit when peer/local sends 0.
/// Used by the engine config (`ChannelLogEngineConfig.backfill_default_limit`)
/// as the production default; the adapter no longer references this
/// constant directly (it now uses the per-engine value plumbed through
/// `ChannelLogAdapterRequest.backfill_default_limit`). Kept for the
/// event-loop unit tests that still want a stable sentinel.
#[cfg(test)]
const CHANNEL_BACKFILL_DEFAULT_LIMIT: usize = 256;
/// Per spec §6.2 + §15: hard cap on backfill `limit` (peer-controlled
/// AND local-controlled). Bounds the reply storm on the queryable side
/// and prevents a misbehaving local engine from issuing oversized
/// requests on the driver side.
const CHANNEL_BACKFILL_MAX_LIMIT: usize = 1000;

/// Outcome of parsing a channel-log backfill selector key.
///
/// Distinguishes "valid selector with the explicit `0` sentinel (=
/// from earliest)" from "malformed selector". Previously both
/// collapsed to `since = None`, which silently widened a malformed
/// selector like `harmony/channels/.../since/not_hlc/500` into a
/// real full backfill — broader result set than intended and
/// masked protocol bugs in the requester. The queryable now skips
/// replying entirely on `Invalid`.
#[derive(Debug)]
enum ParsedBackfillKey {
    Valid {
        /// `None` means the explicit `"0"` sentinel — backfill from
        /// earliest. `Some(hlc)` means backfill strictly after this
        /// HLC.
        since: Option<crate::owner_state_types::Hlc>,
        /// Raw limit (still subject to the queryable's
        /// per-engine default + MAX clamp before use).
        limit: usize,
    },
    /// Selector didn't parse — wrong shape, missing segments, or
    /// non-`"0"` HLC field that didn't decode. Caller MUST skip
    /// replying.
    Invalid,
}

/// Parse `"harmony/channels/{cid}/{ch_id}/since/{hlc_hex}/{limit}"`.
///
/// Returns `ParsedBackfillKey::Valid` for well-formed selectors
/// (with `since = None` only when the HLC field is the explicit
/// `"0"` sentinel), or `ParsedBackfillKey::Invalid` when the
/// selector is malformed (wrong segment count, wrong literal at
/// index 4, or non-`"0"` HLC that fails to decode). A bad limit
/// integer falls back to `0`, which the caller's clamp converts to
/// the per-engine default — bad-limit isn't fatal, only bad-HLC
/// is, because the limit field has a safe default but the HLC
/// field directly determines the result-set boundary.
fn parse_channel_backfill_key(key: &str) -> ParsedBackfillKey {
    // Pattern is: harmony / channels / {cid} / {ch_id} / since / {hlc_hex} / {limit}
    //               0         1          2       3         4        5            6
    let parts: Vec<&str> = key.split('/').collect();
    if parts.len() < 7 || parts[4] != "since" {
        return ParsedBackfillKey::Invalid;
    }
    let hlc_hex = parts[5];
    let limit_str = parts.get(6).copied().unwrap_or("0");

    let since = if hlc_hex == "0" {
        None
    } else {
        match parse_hlc_hex(hlc_hex) {
            Some(hlc) => Some(hlc),
            None => return ParsedBackfillKey::Invalid,
        }
    };
    let limit = limit_str.parse::<usize>().unwrap_or(0);
    ParsedBackfillKey::Valid { since, limit }
}

fn parse_hlc_hex(hex_str: &str) -> Option<crate::owner_state_types::Hlc> {
    // wall_ms LE u64 (16 hex) || logical LE u32 (8 hex) || device_id_bytes (rest)
    if hex_str.len() < 24 {
        return None;
    }
    let wall_ms_bytes = hex::decode(&hex_str[0..16]).ok()?;
    let logical_bytes = hex::decode(&hex_str[16..24]).ok()?;
    let device_id_bytes = hex::decode(&hex_str[24..]).ok()?;
    let wall_ms = u64::from_le_bytes(wall_ms_bytes.try_into().ok()?);
    let logical = u32::from_le_bytes(logical_bytes.try_into().ok()?);
    let device_id = String::from_utf8(device_id_bytes).ok()?;
    Some(crate::owner_state_types::Hlc {
        wall_ms,
        logical,
        device_id,
    })
}

fn format_hlc_hex(hlc: &crate::owner_state_types::Hlc) -> String {
    let mut out = String::new();
    out.push_str(&hex::encode(hlc.wall_ms.to_le_bytes()));
    out.push_str(&hex::encode(hlc.logical.to_le_bytes()));
    out.push_str(&hex::encode(hlc.device_id.as_bytes()));
    out
}

#[cfg(test)]
mod channel_log_adapter_tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    /// Spawns the adapter, sends one packet via publisher, asserts
    /// the subscriber side receives it. Uses an in-memory Zenoh
    /// router so no real network is touched.
    ///
    /// Requires `multi_thread` flavor — Zenoh's runtime panics under
    /// the default current-thread scheduler.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn channel_log_adapter_publish_subscribe_round_trip() {
        let cfg = zenoh::Config::default();
        let session = Arc::new(zenoh::open(cfg).await.expect("zenoh open"));

        let (pub_tx, pub_rx) = mpsc::channel::<Vec<u8>>(8);
        let (sub_tx, mut sub_rx) = mpsc::channel::<Vec<u8>>(8);
        let (qreq_tx, qreq_rx) =
            mpsc::channel::<crate::community_channel_log_engine::BackfillQueryRequest>(2);

        let read_for_query = Arc::new(
            |_since: Option<crate::owner_state_types::Hlc>, _limit: usize| {
                Box::pin(async move { Vec::<Vec<u8>>::new() })
                    as std::pin::Pin<Box<dyn std::future::Future<Output = Vec<Vec<u8>>> + Send>>
            },
        );

        let closing = Arc::new(AtomicBool::new(false));
        // No-op progress callback for the publish/subscribe round-trip
        // unit test — no backfill query fires here, so the callback is
        // never invoked.
        let emit_progress: Arc<dyn Fn(u32, Option<u32>) + Send + Sync + 'static> =
            Arc::new(|_, _| {});
        let _adapter = spawn_channel_log_zenoh_adapter(
            Arc::clone(&session),
            "aabb".repeat(8),
            "ccdd".repeat(8),
            pub_rx,
            sub_tx,
            qreq_rx,
            read_for_query,
            emit_progress,
            16,
            CHANNEL_BACKFILL_DEFAULT_LIMIT,
            Arc::clone(&closing),
        );

        // Wait for the Zenoh subscriber to come online by round-tripping
        // a synthetic warmup packet. Replaces the prior fixed 250ms sleep
        // which was scheduler-dependent and flaked under load. We use a
        // distinct warmup byte sequence so leftover warmup deliveries
        // can't be mistaken for the real payload assertion below.
        let warmup_payload = b"__warmup__".to_vec();
        let warmup_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if std::time::Instant::now() >= warmup_deadline {
                panic!("subscriber didn't come online within 2s");
            }
            pub_tx
                .send(warmup_payload.clone())
                .await
                .expect("publish warmup");
            match tokio::time::timeout(std::time::Duration::from_millis(50), sub_rx.recv()).await {
                Ok(Some(received)) if received == warmup_payload => break,
                _ => {
                    // Subscriber not ready yet; retry.
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
            }
        }

        // Drain any extra warmup deliveries the subscriber may have
        // queued before it came online, so the real payload assertion
        // below is unambiguous.
        loop {
            match tokio::time::timeout(std::time::Duration::from_millis(20), sub_rx.recv()).await {
                Ok(Some(extra)) if extra == warmup_payload => continue,
                Ok(Some(other)) => {
                    panic!("unexpected non-warmup payload during drain: {:?}", other);
                }
                _ => break,
            }
        }

        let payload = b"channel-log-roundtrip".to_vec();
        pub_tx.send(payload.clone()).await.expect("publish send");

        let received = tokio::time::timeout(std::time::Duration::from_secs(2), sub_rx.recv())
            .await
            .expect("recv timeout")
            .expect("sub_rx open");
        assert_eq!(received, payload);

        closing.store(true, Ordering::SeqCst);
        // Keep qreq_tx alive until end so the query-request driver
        // doesn't latch the receiver-closed branch before closing
        // is observed.
        drop(qreq_tx);
    }

    #[test]
    fn parse_channel_backfill_key_round_trip_with_clamp() {
        // Format: harmony/channels/{cid}/{ch_id}/since/{hlc_hex}/{limit}
        let key = format!(
            "harmony/channels/{}/{}/since/0/9999999999",
            "aa".repeat(16),
            "bb".repeat(16)
        );
        let ParsedBackfillKey::Valid {
            since,
            limit: limit_raw,
        } = parse_channel_backfill_key(&key)
        else {
            panic!("expected ParsedBackfillKey::Valid for well-formed selector");
        };
        assert!(since.is_none(), "since=0 should parse to None");
        assert_eq!(limit_raw, 9_999_999_999_usize, "raw limit passes through");

        // Verify the clamp logic the queryable would apply:
        let limit = if limit_raw == 0 {
            CHANNEL_BACKFILL_DEFAULT_LIMIT
        } else {
            limit_raw.min(CHANNEL_BACKFILL_MAX_LIMIT)
        };
        assert_eq!(limit, CHANNEL_BACKFILL_MAX_LIMIT, "clamp caps at hard max");
    }

    #[test]
    fn parse_channel_backfill_key_zero_limit_uses_default_after_clamp() {
        let key = format!(
            "harmony/channels/{}/{}/since/0/0",
            "aa".repeat(16),
            "bb".repeat(16)
        );
        let ParsedBackfillKey::Valid {
            since: _,
            limit: limit_raw,
        } = parse_channel_backfill_key(&key)
        else {
            panic!("expected ParsedBackfillKey::Valid for well-formed selector");
        };
        assert_eq!(limit_raw, 0);

        let limit = if limit_raw == 0 {
            CHANNEL_BACKFILL_DEFAULT_LIMIT
        } else {
            limit_raw.min(CHANNEL_BACKFILL_MAX_LIMIT)
        };
        assert_eq!(limit, CHANNEL_BACKFILL_DEFAULT_LIMIT);
    }

    /// Round 3 (R3-1): a malformed HLC field MUST surface as
    /// `ParsedBackfillKey::Invalid`, not silently widen to a full
    /// backfill (the prior `(None, _)` collapse). The queryable
    /// handler skips replying on `Invalid` so a protocol-violating
    /// requester gets no data instead of getting more data than it
    /// asked for.
    #[test]
    fn parse_channel_backfill_key_rejects_malformed_hlc() {
        let key = format!(
            "harmony/channels/{}/{}/since/not_hlc/500",
            "aa".repeat(16),
            "bb".repeat(16)
        );
        match parse_channel_backfill_key(&key) {
            ParsedBackfillKey::Invalid => {}
            ParsedBackfillKey::Valid { .. } => {
                panic!("malformed HLC field must surface as Invalid, not silently widen to full backfill");
            }
        }
    }
}

/// ZEB-156 unit tests: verify the Unpin and Burn arms' keep-set cascade.
///
/// These tests replicate the production cascade bodies from
/// `ContentVerbRequest::Unpin` and `ContentVerbRequest::Burn` directly
/// against a `ContentStore` — the arms themselves can't easily be driven
/// without the full event-loop harness (`NodeRuntime` is `!Send` and
/// requires a dedicated OS thread; see `tests/content_index_integration.rs`
/// for the integration-level cascade coverage). Keeping the algorithm here
/// means the tests catch regressions in the keep-set computation in
/// isolation from the rest of the verb pipeline.
///
/// Both simulators call the production `compute_keep_set` helper directly,
/// so the keep-set logic itself is single-sourced; the simulators just
/// thread the post-keep loop (unpin vs unpin + remove) the way the verb
/// arms do. If the production arm bodies diverge from the simulators
/// (i.e. one of them adds another side effect inside the `!keep.contains`
/// branch), both must be updated together — this is documented at the top
/// of each `simulate_*_cascade` below.
#[cfg(test)]
mod pin_cascade_tests {
    use super::{collect_descendants, compute_keep_set};
    use harmony_content::book::{BookStore, MemoryBookStore};
    use harmony_content::bundle::BundleBuilder;
    use harmony_content::cache::ContentStore;
    use harmony_content::cid::{ContentFlags, ContentId};
    use std::collections::HashSet;

    /// Mirror of the `ContentVerbRequest::Unpin` arm's cascade body
    /// (event_loop.rs §"Content-verb requests"), refactored against a
    /// `&mut ContentStore` instead of `&mut NodeRuntime` so unit tests can
    /// drive it directly. KEEP IN SYNC with the arm — the per-id loop
    /// body MUST match (currently `cache.unpin(&id)` only; if the arm adds
    /// another effect, mirror it here too).
    fn simulate_unpin_cascade(
        cache: &mut ContentStore<MemoryBookStore>,
        pin_intent: &mut HashSet<[u8; 32]>,
        cid: [u8; 32],
    ) {
        pin_intent.remove(&cid);
        let root = ContentId::from_bytes(cid);
        let doomed = collect_descendants(cache, root);
        let keep = compute_keep_set(cache, pin_intent, doomed.len());
        for id in doomed {
            if !keep.contains(&id) {
                cache.unpin(&id);
            }
        }
    }

    /// Mirror of the `ContentVerbRequest::Burn` arm's cascade body
    /// (event_loop.rs §"Content-verb requests"), refactored against a
    /// `&mut ContentStore` instead of `&mut NodeRuntime`. KEEP IN SYNC
    /// with the arm — the per-id loop body MUST match: unpin AND
    /// `cache.remove`, both gated on `!keep.contains(&id)`. The arm's
    /// `runtime.remove_content` resolves to `cache.remove` via the
    /// `StorageTier::remove` → `<ContentStore as BookStore>::remove`
    /// delegation chain, so the observable cache effect is identical to
    /// `cache.remove` here.
    fn simulate_burn_cascade(
        cache: &mut ContentStore<MemoryBookStore>,
        pin_intent: &mut HashSet<[u8; 32]>,
        cid: [u8; 32],
    ) {
        pin_intent.remove(&cid);
        let root = ContentId::from_bytes(cid);
        let doomed = collect_descendants(cache, root);
        let keep = compute_keep_set(cache, pin_intent, doomed.len());
        for id in doomed {
            if !keep.contains(&id) {
                cache.unpin(&id);
                let _ = cache.remove(&id);
            }
        }
    }

    fn new_cache() -> ContentStore<MemoryBookStore> {
        ContentStore::new(MemoryBookStore::new(), 1024)
    }

    /// Pin a CID and all CIDs reachable from it, mirroring the
    /// `ContentVerbRequest::Pin` arm's cascade (event_loop.rs §"Content-verb
    /// requests"). Test-only helper — no keep-set logic since over-pinning
    /// is idempotent at the cache layer.
    fn cascade_pin(cache: &mut ContentStore<MemoryBookStore>, root: ContentId) {
        for id in collect_descendants(cache, root) {
            assert!(cache.pin(id), "pin quota exceeded in test fixture");
        }
    }

    /// Test 1 (spec): Unpin with a single root and no sharing — regression
    /// guard. All N+1 CIDs (root + descendants) must be unpinned after the
    /// cascade, matching pre-fix behavior when nothing remains in
    /// pin_intent.
    #[test]
    fn unpin_single_root_no_sharing_clears_full_subtree() {
        let mut cache = new_cache();
        let a = cache
            .insert_with_flags(b"leaf-a", ContentFlags::default())
            .unwrap();
        let b = cache
            .insert_with_flags(b"leaf-b", ContentFlags::default())
            .unwrap();
        let c = cache
            .insert_with_flags(b"leaf-c", ContentFlags::default())
            .unwrap();
        let mut builder = BundleBuilder::new();
        builder.add(a).add(b).add(c);
        let (payload, root) = builder.build_with_flags(ContentFlags::default()).unwrap();
        cache.store(root, payload);

        // Sanity: 4 distinct CIDs (root + 3 leaves).
        let descendants: HashSet<ContentId> =
            collect_descendants(&cache, root).into_iter().collect();
        assert_eq!(descendants.len(), 4);

        let mut pin_intent: HashSet<[u8; 32]> = HashSet::new();
        pin_intent.insert(root.to_bytes());
        cascade_pin(&mut cache, root);
        for cid in [root, a, b, c] {
            assert!(cache.is_pinned(&cid), "precondition: {cid:?} pinned");
        }

        simulate_unpin_cascade(&mut cache, &mut pin_intent, root.to_bytes());

        for cid in [root, a, b, c] {
            assert!(
                !cache.is_pinned(&cid),
                "{cid:?} must be unpinned after Unpin(root) with no other roots in pin_intent"
            );
        }
        assert!(pin_intent.is_empty(), "intent cleared");
    }

    /// Test 2 (spec): two roots sharing a leaf. Unpin one root, the shared
    /// leaf must stay pinned because it is reachable from the remaining
    /// root.
    #[test]
    fn unpin_with_shared_leaf_keeps_overlap_pinned() {
        let mut cache = new_cache();

        // Shared leaf `shared`. Plus a per-root leaf each, so the two
        // bundle roots have distinct CIDs (otherwise the test's "two
        // roots" precondition collapses to a single root).
        let shared = cache
            .insert_with_flags(b"shared-leaf", ContentFlags::default())
            .unwrap();
        let only_in_left = cache
            .insert_with_flags(b"left-only", ContentFlags::default())
            .unwrap();
        let only_in_right = cache
            .insert_with_flags(b"right-only", ContentFlags::default())
            .unwrap();

        let mut left_builder = BundleBuilder::new();
        left_builder.add(shared).add(only_in_left);
        let (left_payload, left_root) = left_builder
            .build_with_flags(ContentFlags::default())
            .unwrap();
        cache.store(left_root, left_payload);

        let mut right_builder = BundleBuilder::new();
        right_builder.add(shared).add(only_in_right);
        let (right_payload, right_root) = right_builder
            .build_with_flags(ContentFlags::default())
            .unwrap();
        cache.store(right_root, right_payload);

        assert_ne!(left_root, right_root, "two distinct bundle roots");

        // Pin both roots and their cascades.
        let mut pin_intent: HashSet<[u8; 32]> = HashSet::new();
        pin_intent.insert(left_root.to_bytes());
        pin_intent.insert(right_root.to_bytes());
        cascade_pin(&mut cache, left_root);
        cascade_pin(&mut cache, right_root);

        for cid in [shared, only_in_left, only_in_right, left_root, right_root] {
            assert!(cache.is_pinned(&cid), "precondition: {cid:?} pinned");
        }

        // Unpin the left root. The shared leaf is reachable from the
        // remaining right root, so it must stay pinned. `only_in_left`
        // is reachable only from the left root, so it must be unpinned.
        simulate_unpin_cascade(&mut cache, &mut pin_intent, left_root.to_bytes());

        assert!(
            !cache.is_pinned(&left_root),
            "left_root itself was the unpin target"
        );
        assert!(
            !cache.is_pinned(&only_in_left),
            "only_in_left has no other pinned root and must be unpinned"
        );
        assert!(
            cache.is_pinned(&shared),
            "shared leaf must stay pinned — it is in right_root's subtree (BUG GUARD: pre-ZEB-156, this would have been unpinned)"
        );
        assert!(
            cache.is_pinned(&right_root),
            "right_root is untouched and must stay pinned"
        );
        assert!(
            cache.is_pinned(&only_in_right),
            "only_in_right is still reachable from right_root"
        );
    }

    /// Test 3 (spec): two roots sharing a full subtree. `C = bundle(A, B)`
    /// where `A = bundle(a1, a2)`. Pin both A and C separately. Unpin C.
    ///
    /// Expected: C, B, and B's contents unpinned (B has no other pinned
    /// root); A and a1, a2 stay pinned because A is still in pin_intent
    /// and its cascade keeps a1 and a2 reachable.
    #[test]
    fn unpin_with_shared_subtree_keeps_overlap_pinned() {
        let mut cache = new_cache();

        // Leaves for A's subtree.
        let a1 = cache
            .insert_with_flags(b"a1-leaf", ContentFlags::default())
            .unwrap();
        let a2 = cache
            .insert_with_flags(b"a2-leaf", ContentFlags::default())
            .unwrap();
        let mut a_builder = BundleBuilder::new();
        a_builder.add(a1).add(a2);
        let (a_payload, cid_a) = a_builder.build_with_flags(ContentFlags::default()).unwrap();
        cache.store(cid_a, a_payload);

        // B's subtree is just leaves so the test stays small.
        let b1 = cache
            .insert_with_flags(b"b1-leaf", ContentFlags::default())
            .unwrap();
        let b2 = cache
            .insert_with_flags(b"b2-leaf", ContentFlags::default())
            .unwrap();
        let mut b_builder = BundleBuilder::new();
        b_builder.add(b1).add(b2);
        let (b_payload, cid_b) = b_builder.build_with_flags(ContentFlags::default()).unwrap();
        cache.store(cid_b, b_payload);

        // C = bundle(A, B).
        let mut c_builder = BundleBuilder::new();
        c_builder.add(cid_a).add(cid_b);
        let (c_payload, cid_c) = c_builder.build_with_flags(ContentFlags::default()).unwrap();
        cache.store(cid_c, c_payload);

        // Pin both A and C. Both go into pin_intent (mirror of sidecar
        // entries with pinned=true).
        let mut pin_intent: HashSet<[u8; 32]> = HashSet::new();
        pin_intent.insert(cid_a.to_bytes());
        pin_intent.insert(cid_c.to_bytes());
        cascade_pin(&mut cache, cid_a);
        cascade_pin(&mut cache, cid_c);

        for cid in [cid_a, cid_b, cid_c, a1, a2, b1, b2] {
            assert!(cache.is_pinned(&cid), "precondition: {cid:?} pinned");
        }

        // Unpin C. A's subtree (cid_a, a1, a2) must stay pinned because
        // pin_intent still contains cid_a and its cascade covers them.
        simulate_unpin_cascade(&mut cache, &mut pin_intent, cid_c.to_bytes());

        assert!(!cache.is_pinned(&cid_c), "cid_c unpinned (target)");
        assert!(
            !cache.is_pinned(&cid_b),
            "cid_b unpinned — reachable only from cid_c"
        );
        assert!(!cache.is_pinned(&b1), "b1 unpinned");
        assert!(!cache.is_pinned(&b2), "b2 unpinned");
        assert!(
            cache.is_pinned(&cid_a),
            "cid_a stays pinned — still in pin_intent (BUG GUARD: pre-ZEB-156, this would have been unpinned because it's a descendant of cid_c)"
        );
        assert!(
            cache.is_pinned(&a1),
            "a1 stays pinned — reachable from cid_a's keep-set walk"
        );
        assert!(
            cache.is_pinned(&a2),
            "a2 stays pinned — reachable from cid_a's keep-set walk"
        );
    }

    /// Test 4 (spec): Burn evicts from cache. Pin a synthetic root with
    /// several descendants, burn it. Every descendant must be both
    /// unpinned AND removed from the cache (`cache.get` returns `None`).
    ///
    /// `simulate_burn_cascade` mirrors the production `Burn` arm; this
    /// catches a regression where Burn forgets to call `cache.remove`
    /// (the bytes would linger in the cache until W-TinyLFU pressure
    /// kicked in, which is the pre-ZEB-156 behavior).
    #[test]
    fn burn_evicts_descendants_from_cache() {
        let mut cache = new_cache();
        let a = cache
            .insert_with_flags(b"burn-leaf-a", ContentFlags::default())
            .unwrap();
        let b = cache
            .insert_with_flags(b"burn-leaf-b", ContentFlags::default())
            .unwrap();
        let c = cache
            .insert_with_flags(b"burn-leaf-c", ContentFlags::default())
            .unwrap();
        let mut builder = BundleBuilder::new();
        builder.add(a).add(b).add(c);
        let (payload, root) = builder.build_with_flags(ContentFlags::default()).unwrap();
        cache.store(root, payload);

        let mut pin_intent: HashSet<[u8; 32]> = HashSet::new();
        pin_intent.insert(root.to_bytes());
        cascade_pin(&mut cache, root);

        // Sanity: everything is present and pinned before the burn.
        for cid in [root, a, b, c] {
            assert!(cache.is_pinned(&cid), "precondition: {cid:?} pinned");
            assert!(
                cache.get(&cid).is_some(),
                "precondition: {cid:?} present in cache"
            );
        }

        simulate_burn_cascade(&mut cache, &mut pin_intent, root.to_bytes());

        // Every descendant must be unpinned AND evicted.
        for cid in [root, a, b, c] {
            assert!(!cache.is_pinned(&cid), "{cid:?} unpinned after burn");
            assert!(
                cache.get(&cid).is_none(),
                "{cid:?} evicted from cache after burn (BUG GUARD: pre-ZEB-156, Burn relied on W-TinyLFU pressure and the bytes would still be reachable here)"
            );
        }
        assert!(pin_intent.is_empty(), "intent cleared");
    }

    /// Test 5 (spec): Burn respects the keep set. Same shared-subtree
    /// fixture as Test 3 (`C = bundle(A, B)` where `A = bundle(a1, a2)`,
    /// `B = bundle(b1, b2)`), with A and C pinned separately. Burn C.
    ///
    /// Expected: C unpinned + removed; A still pinned + still in cache;
    /// a1, a2 still in cache (A's keep-set walk covers them);
    /// B, b1, b2 unpinned + removed (B was C-only).
    #[test]
    fn burn_with_shared_subtree_keeps_overlap_in_cache() {
        let mut cache = new_cache();

        let a1 = cache
            .insert_with_flags(b"a1-leaf-burn", ContentFlags::default())
            .unwrap();
        let a2 = cache
            .insert_with_flags(b"a2-leaf-burn", ContentFlags::default())
            .unwrap();
        let mut a_builder = BundleBuilder::new();
        a_builder.add(a1).add(a2);
        let (a_payload, cid_a) = a_builder.build_with_flags(ContentFlags::default()).unwrap();
        cache.store(cid_a, a_payload);

        let b1 = cache
            .insert_with_flags(b"b1-leaf-burn", ContentFlags::default())
            .unwrap();
        let b2 = cache
            .insert_with_flags(b"b2-leaf-burn", ContentFlags::default())
            .unwrap();
        let mut b_builder = BundleBuilder::new();
        b_builder.add(b1).add(b2);
        let (b_payload, cid_b) = b_builder.build_with_flags(ContentFlags::default()).unwrap();
        cache.store(cid_b, b_payload);

        let mut c_builder = BundleBuilder::new();
        c_builder.add(cid_a).add(cid_b);
        let (c_payload, cid_c) = c_builder.build_with_flags(ContentFlags::default()).unwrap();
        cache.store(cid_c, c_payload);

        let mut pin_intent: HashSet<[u8; 32]> = HashSet::new();
        pin_intent.insert(cid_a.to_bytes());
        pin_intent.insert(cid_c.to_bytes());
        cascade_pin(&mut cache, cid_a);
        cascade_pin(&mut cache, cid_c);

        for cid in [cid_a, cid_b, cid_c, a1, a2, b1, b2] {
            assert!(cache.is_pinned(&cid), "precondition: {cid:?} pinned");
            assert!(cache.get(&cid).is_some(), "precondition: {cid:?} in cache");
        }

        simulate_burn_cascade(&mut cache, &mut pin_intent, cid_c.to_bytes());

        // cid_c is the burn target: unpinned + removed.
        assert!(!cache.is_pinned(&cid_c), "cid_c unpinned (burn target)");
        assert!(
            cache.get(&cid_c).is_none(),
            "cid_c removed from cache (burn target)"
        );

        // cid_a stays pinned because it's still in pin_intent; its
        // descendants must also stay in the cache.
        assert!(
            cache.is_pinned(&cid_a),
            "cid_a stays pinned — still in pin_intent (BUG GUARD: pre-ZEB-156, this would have been unpinned because it's a descendant of cid_c)"
        );
        assert!(
            cache.get(&cid_a).is_some(),
            "cid_a stays in cache — keep-set guard skipped cache.remove"
        );
        assert!(
            cache.is_pinned(&a1),
            "a1 stays pinned — reachable from cid_a's keep-set walk"
        );
        assert!(
            cache.is_pinned(&a2),
            "a2 stays pinned — reachable from cid_a's keep-set walk"
        );
        assert!(
            cache.get(&a1).is_some(),
            "a1 stays in cache — A still pins it"
        );
        assert!(
            cache.get(&a2).is_some(),
            "a2 stays in cache — A still pins it"
        );

        // B's subtree was reachable only from C — must be unpinned AND
        // removed.
        assert!(
            !cache.is_pinned(&cid_b),
            "cid_b unpinned — reachable only from cid_c"
        );
        assert!(!cache.is_pinned(&b1), "b1 unpinned");
        assert!(!cache.is_pinned(&b2), "b2 unpinned");
        assert!(
            cache.get(&cid_b).is_none(),
            "cid_b removed from cache (B was C-only)"
        );
        assert!(cache.get(&b1).is_none(), "b1 removed from cache");
        assert!(cache.get(&b2).is_none(), "b2 removed from cache");
    }

    /// Test 6 (spec): Empty `pin_intent` corner case. Pin a single root,
    /// burn it. After `pin_intent.remove(&cid)`, the set is empty so the
    /// keep set is empty. Every descendant gets unpinned AND evicted.
    ///
    /// This matches the pre-fix cascade end-state (no keep set means no
    /// guard), so the existing
    /// `chunked_ingest_pin_cascade_fetch_burn_roundtrip` integration test
    /// continues to pass. The unit test adds redundant unit-level
    /// coverage for the empty-`pin_intent` branch in the keep-set loop.
    #[test]
    fn burn_with_empty_pin_intent_evicts_everything() {
        let mut cache = new_cache();
        let leaf1 = cache
            .insert_with_flags(b"solo-leaf-1", ContentFlags::default())
            .unwrap();
        let leaf2 = cache
            .insert_with_flags(b"solo-leaf-2", ContentFlags::default())
            .unwrap();
        let mut builder = BundleBuilder::new();
        builder.add(leaf1).add(leaf2);
        let (payload, root) = builder.build_with_flags(ContentFlags::default()).unwrap();
        cache.store(root, payload);

        let mut pin_intent: HashSet<[u8; 32]> = HashSet::new();
        pin_intent.insert(root.to_bytes());
        cascade_pin(&mut cache, root);

        // The lone pinned root is exactly the one we're about to burn,
        // so `pin_intent` will be empty inside the cascade. Sanity-check
        // that precondition.
        assert_eq!(pin_intent.len(), 1);

        simulate_burn_cascade(&mut cache, &mut pin_intent, root.to_bytes());

        assert!(
            pin_intent.is_empty(),
            "intent must be empty after burning the only entry"
        );
        for cid in [root, leaf1, leaf2] {
            assert!(!cache.is_pinned(&cid), "{cid:?} unpinned");
            assert!(cache.get(&cid).is_none(), "{cid:?} evicted from cache");
        }
    }
}
