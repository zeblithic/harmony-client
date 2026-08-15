//! Simplified NodeRuntime event loop for the Tauri desktop client.
//!
//! Adapted from harmony-node's event_loop.rs, stripped down to:
//! - Zenoh session (pub/sub, queryables, content fetch)
//! - 250ms timer tick
//!
//! No disk/archive/S3 persistence, no inference.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use harmony_content::book::MemoryBookStore;
use harmony_runtime::{NodeRuntime, RuntimeAction, RuntimeEvent};
use tokio::sync::{mpsc, oneshot, watch};

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
    /// ZEB-707: sender for owner-state root-serve requests. The state-root
    /// queryable task forwards each inbound zenoh root query over this to the
    /// engine, which replies the current root wire so a butler that missed the
    /// live push can PULL it. Obtained from `SyncEngine::root_serve_tx()`.
    pub root_serve_tx: tokio::sync::mpsc::Sender<crate::fleet_sync::RootServeReq>,
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

/// ZEB-417 SP1: Notes fleet-sync Zenoh adapter handles. Mirrors
/// `MintSyncHandles` — constructed in `start_node` alongside the
/// `FleetSyncEngine<NotesDoc>`, consumed inside `event_loop::run` once the
/// Zenoh session is open.
pub struct NotesSyncHandles {
    /// Hex-encoded OWNER identity address — forms the notes topic key
    /// `harmony/owner/{addr_hex}/ds/notes-v1`.
    pub addr_hex: String,
    /// Encrypted bytes from the notes engine's publish path → Zenoh put.
    pub outbound_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    /// Encrypted bytes from Zenoh → the notes engine's subscribe path.
    pub inbound_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
}

/// ZEB-418 SP2 P1: butler dm-inbox fleet-sync Zenoh adapter handles.
/// Mirrors [`NotesSyncHandles`] — constructed in `start_node` alongside the
/// `FleetSyncEngine<DmInboxDoc>`, consumed inside `event_loop::run` once the
/// Zenoh session is open.
pub struct DmInboxSyncHandles {
    /// Hex-encoded OWNER identity address — forms the dm-inbox topic key
    /// `harmony/owner/{addr_hex}/ds/dm-inbox-v1`.
    pub addr_hex: String,
    /// Encrypted bytes from the dm-inbox engine's publish path → Zenoh put.
    pub outbound_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    /// Encrypted bytes from Zenoh → the dm-inbox engine's subscribe path.
    pub inbound_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
}

/// ZEB-418 SP2 P2: generic owner-dataset fleet-sync Zenoh adapter handles.
/// Same shape as [`DmInboxSyncHandles`] / [`NotesSyncHandles`] — constructed
/// in `start_node` alongside a `FleetSyncEngine`, consumed inside
/// `event_loop::run` once the Zenoh session is open. The topic is
/// `harmony/owner/{addr_hex}/ds/{dataset}` where `{dataset}` is supplied at
/// the consumption site (see [`P2SyncHandles`]).
pub struct DatasetSyncHandles {
    /// Hex-encoded OWNER identity address — forms the per-dataset topic key
    /// `harmony/owner/{addr_hex}/ds/{dataset}`.
    pub addr_hex: String,
    /// Encrypted bytes from the engine's publish path → Zenoh put.
    pub outbound_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    /// Encrypted bytes from Zenoh → the engine's subscribe path.
    pub inbound_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
}

/// ZEB-418 SP2 P2: the two P2 dataset adapter handle pairs, bundled so
/// `run(...)` grows ONE parameter instead of two. `None` when no owner
/// identity is loaded (and in test callers that bypass `start_node`).
pub struct P2SyncHandles {
    /// dm-outhold-v1 — sender-side outbound-hold blobs (spec D12).
    pub outhold: DatasetSyncHandles,
    /// fleet-net-v1 — per-device network info + pinned butler (spec §5–§6).
    pub fleet_net: DatasetSyncHandles,
}

/// ZEB-458 P4 Phase B: the two relay dataset adapter handle pairs, bundled so
/// `run(...)` grows ONE parameter instead of two (same rationale as
/// [`P2SyncHandles`]). Both datasets are fleet-scoped on the OWNER address —
/// `relay-hold-v1` replicates the relay's held blobs across the relay's own
/// fleet (D38); `relay-optin-v1` replicates the per-community opt-in across the
/// volunteer's fleet (D43). `None` when no owner identity is loaded (and in
/// test callers that bypass `start_node`).
pub struct RelaySyncHandles {
    /// relay-hold-v1 — the relay's held opaque blobs (D38).
    pub hold: DatasetSyncHandles,
    /// relay-optin-v1 — per-community relay opt-in flags (D43).
    pub optin: DatasetSyncHandles,
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
    /// ZEB-434 D1: queryable → engine serve requests (engine holds rx).
    /// Cost per served query: CRDT clone + CBOR encode + 2×AEAD + CAS
    /// put_serveable + fsync(replay). The mpsc capacity at the lib.rs
    /// call site (8) is the back-pressure bound; zenoh-facing rate
    /// limiting beyond that is future work.
    pub root_serve_tx: tokio::sync::mpsc::Sender<crate::community_state_sync::RootServeRequest>,
    /// ZEB-434 D3/D4: fetch driver → adapter query requests.
    pub fetch_request_rx: tokio::sync::mpsc::Receiver<CommunityRootFetchRequest>,
}

/// ZEB-434: one root-fetch query request from the per-community fetch
/// driver. The adapter executes the GET and reports the reply count.
pub struct CommunityRootFetchRequest {
    /// Reply-count report (fire-and-forget; drop = query aborted).
    pub report: tokio::sync::oneshot::Sender<usize>,
}

/// ZEB-718: closure the backfill responder calls to read the engine's
/// live voting events as plaintext `SignedVotingEvent` CBOR frames. The
/// adapter re-encrypts each frame under the **current** epoch before
/// replying, so it passes the requester's current-epoch cut.
pub type VotingBackfillReadFn = std::sync::Arc<
    dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<Vec<u8>>> + Send>>
        + Send
        + Sync,
>;

/// ZEB-718: closure the backfill requester calls with each decrypted
/// (current-epoch) plaintext `SignedVotingEvent` CBOR frame, to apply it
/// through the engine's coordinate-dedup backfill path.
pub type VotingBackfillApplyFn = std::sync::Arc<
    dyn Fn(Vec<u8>) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        + Send
        + Sync,
>;

/// ZEB-932: the plaintext RBSR protocol halves the voting adapter drives, as
/// type-erased closures over the (runtime-generic) engine — mirroring the
/// backfill closures. `initial` builds round 0; `respond` answers a request,
/// returning the reply plus the plaintext CBOR bodies for its `Have` keys (the
/// adapter seals the reply under `VOTING_RBSR_AAD` and encrypts the bodies under
/// `VOTING_TOPIC_AAD`, all under one epoch snapshot); `process_reply` returns the
/// next request, or `None` once converged (the requester's `Have` bodies having
/// already been applied via the backfill apply path).
#[derive(Clone)]
pub struct VotingRbsrHooks {
    pub initial: std::sync::Arc<
        dyn Fn() -> std::pin::Pin<
                Box<dyn std::future::Future<Output = crate::channel_rbsr::RbsrMessage> + Send>,
            > + Send
            + Sync,
    >,
    #[allow(clippy::type_complexity)]
    pub respond: std::sync::Arc<
        dyn Fn(
                crate::channel_rbsr::RbsrMessage,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<
                            Output = Option<(crate::channel_rbsr::RbsrMessage, Vec<Vec<u8>>)>,
                        > + Send,
                >,
            > + Send
            + Sync,
    >,
    #[allow(clippy::type_complexity)]
    pub process_reply: std::sync::Arc<
        dyn Fn(
                crate::channel_rbsr::RbsrMessage,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<Output = Option<crate::channel_rbsr::RbsrMessage>>
                        + Send,
                >,
            > + Send
            + Sync,
    >,
}

/// ZEB-298+ZEB-312 PR 1: per-community voting-log adapter request.
/// ZEB-718: gains a backfill queryable (responder) + a pull driver
/// (requester) alongside the pub/sub live path.
pub struct VotingLogAdapterRequest {
    /// Hex-encoded community SpaceId — used to form
    /// `harmony/community/{id_hex}/voting`.
    pub id_hex: String,
    /// ZEB-717: community SpaceId, for the epoch-key `Space` lookup in
    /// `crdt_state` when the adapter encrypts/decrypts voting packets.
    pub community_id: crate::owner_state_types::SpaceId,
    /// ZEB-717: live owner CRDT state. The adapter reads the community's
    /// current epoch key from here to encrypt outbound puts and to
    /// current-epoch-only decrypt inbound samples (wire = ciphertext,
    /// in-process = plaintext).
    pub crdt_state: std::sync::Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
    /// Engine outbound → Zenoh `put`.
    pub publisher_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    /// Zenoh subscriber → engine inbound.
    pub subscriber_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    /// ZEB-718: backfill responder — reads the engine's live events as
    /// plaintext frames (the adapter re-encrypts under the current epoch).
    pub read_for_backfill: VotingBackfillReadFn,
    /// ZEB-718: backfill requester — applies a decrypted current-epoch
    /// frame via the engine's coordinate-dedup path.
    pub apply_backfilled: VotingBackfillApplyFn,
    /// ZEB-718: periodic anti-entropy floor between backfill pulls.
    pub backfill_interval: std::time::Duration,
    /// ZEB-932: optional RBSR protocol halves. When `Some`, the adapter also
    /// spawns a `voting/rbsr` responder and drives RBSR-first catch-up (the
    /// full dump becomes a fallback + a periodic backstop). `None` → pure
    /// full-dump (pre-RBSR behavior).
    pub rbsr_hooks: Option<VotingRbsrHooks>,
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
                Option<Vec<u8>>,
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
    /// ZEB-593: engine-side RBSR closures (responder + requester halves), or
    /// `None` to run catch-up on the legacy `since/**` path only. When `Some`,
    /// the adapter declares a second `rbsr/**` queryable and tries RBSR before
    /// each watermark GET; when `None` (e.g. unit tests) RBSR is fully disabled.
    pub rbsr_hooks: Option<RbsrAdapterHooks>,
}

// ── ZEB-593 RBSR adapter hooks ──────────────────────────────────────────────
//
// The three engine-side closures the adapter needs to run RBSR catch-up,
// bundled so the whole feature threads as one value. Each closes over the same
// `Arc<ChannelLogEngine>` (the engine holds the channel key + reconcile source);
// the adapter only shuttles opaque sealed bytes and owns the Zenoh session.
//
// The boxed-future return types are public aliases so the engine-side closure
// constructors (registry) can annotate their return type without tripping
// `clippy::type_complexity`.
/// Boxed future returned by the RBSR responder closure.
pub type RbsrRespondFut =
    std::pin::Pin<Box<dyn std::future::Future<Output = Option<(Vec<u8>, Vec<Vec<u8>>)>> + Send>>;
/// Boxed future returned by the RBSR round-0 request closure.
pub type RbsrInitialFut = std::pin::Pin<Box<dyn std::future::Future<Output = Vec<u8>> + Send>>;
/// Boxed future returned by the RBSR ingest-and-advance closure.
pub type RbsrIngestFut = std::pin::Pin<Box<dyn std::future::Future<Output = RbsrStep> + Send>>;

type RbsrRespondClosure = dyn Fn(Vec<u8>) -> RbsrRespondFut + Send + Sync + 'static;
type RbsrInitialClosure = dyn Fn() -> RbsrInitialFut + Send + Sync + 'static;
type RbsrIngestClosure = dyn Fn(Vec<Vec<u8>>) -> RbsrIngestFut + Send + Sync + 'static;

/// Bundle of the engine-side RBSR closures threaded to the adapter.
pub struct RbsrAdapterHooks {
    /// Responder: open a sealed request, compute the reply, resolve + encrypt
    /// its `Have` events → `Some((sealed_reply, have_packets))`; `None` replies
    /// nothing (the requester then falls back).
    pub respond: Arc<RbsrRespondClosure>,
    /// Requester: seal this round-0 request over the local reconcile source.
    pub initial: Arc<RbsrInitialClosure>,
    /// Requester: ingest a round's reply frames and advance or converge.
    pub ingest: Arc<RbsrIngestClosure>,
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
    /// ZEB-344: optional assembled-byte ceiling enforced by `fetch_recursive`.
    /// `None` = unbounded (all callers except `fetch_avatar`).
    pub max_bytes: Option<usize>,
    /// ZEB-535: re-serve fetched (encrypted) artifact books. A fetcher that
    /// allowlists every CID it pulls can in turn serve those CIDs to other
    /// members; `false` for the avatar / content / profile-doc paths.
    pub serveable: bool,
}

/// A content-ingest request: store local file bytes in the runtime's storage tier.
pub struct IngestRequest {
    pub cid_hex: String,
    pub data: Vec<u8>,
    /// ZEB-535: allowlist this CID for member-to-member serve. `true` for an
    /// encrypted-artifact subtree (the sharer authorizes each chunk CID);
    /// `false` for the unencrypted avatar / file-vault ingest paths.
    pub serveable: bool,
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
    Follow {
        address: String,
    },
    Unfollow {
        address: String,
    },
    /// ZEB-671: publish the owner's freshly signed follow list on
    /// `harmony/vines/{owner}/follows`. Built + signed on the command
    /// side (which owns `FollowManager` + the owner identity); the
    /// event loop only performs the Zenoh put and logs failures.
    PublishFollowList {
        owner: String,
        payload: Vec<u8>,
    },
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

/// ZEB-537 community-presence subscriber/publisher pool requests. Mirrors
/// `ProfileCardRequest` but keyed by the 16-byte `community_id` (presence is
/// community-scoped). IPC handlers send `Subscribe` when the user opens a
/// community and `Unsubscribe` when they leave it; the event loop owns the
/// per-community (publisher, subscriber) task pair.
pub enum CommunityPresenceRequest {
    Subscribe { community_id: [u8; 16] },
    Unsubscribe { community_id: [u8; 16] },
}

/// ZEB-815 community address-book task-pool requests. Sibling of
/// `CommunityPresenceRequest` — sent from the SAME IPC sites, keyed the same
/// way — because the address book's lifetime is exactly a community
/// subscription's: it syncs while the node is in the community and stops when
/// it leaves.
pub enum AddressBookRequest {
    Subscribe { community_id: [u8; 16] },
    Unsubscribe { community_id: [u8; 16] },
}

/// ZEB-815: names for the four per-community address-book tasks, in the order
/// the pool spawns and stores them. Used only to say WHICH task died when a
/// group is reaped — keep in step with the `vec![..]` at the insert site.
const ADDRBOOK_TASK_LABELS: [&str; 4] = [
    "snapshot-queryable",
    "record-subscriber",
    "snapshot-requester",
    "sidecar-persist",
];

/// ZEB-815: everything the address-book pool needs that isn't already a
/// parameter of [`run`]. Bundled (like [`IrohRuntimeHandles`]) rather than
/// threaded as five more positional arguments, and `Option` at the call site
/// for the same reason the presence pool is: test callers that bypass
/// `start_node` leave the pool unwired, and the book simply never syncs.
pub struct AddressBookRuntime {
    /// Paired with NodeState's `addrbook_request_tx`.
    pub request_rx: mpsc::Receiver<AddressBookRequest>,
    /// The node-wide store, shared with the publisher (Task 6) and the IPC
    /// readers (Task 7) — every community's rows live in this one book.
    pub book: Arc<crate::community_address_book::CommunityAddressBook>,
    /// Ingest fan-out targets — the exact resolvers the CRDT membership-delta
    /// hook fed before ZEB-815 (spec §4), so dial + deposit consumers are
    /// unchanged by the move.
    pub reachability_resolver: Arc<crate::reachability_resolver::ReachabilityResolver>,
    pub community_relay_resolver: Arc<crate::community_relay_resolver::CommunityRelayResolver>,
    /// Sidecar root — the same `identity_dir` `CommunitySyncRegistry` derives
    /// `communities/{hex}/crdt.cbor` from, so `addrbook.cbor` lands beside it.
    pub identity_dir: std::path::PathBuf,
    /// ingest → sidecar-persist wakeups. Threaded in (not minted per
    /// `Subscribe`) because the publisher closures in `start_node` are the
    /// THIRD producer: a locally-published row upserts into the book from
    /// outside this pool entirely, and a pool-local `Notify` would leave that
    /// change unpersisted until a peer echoed it back.
    pub dirty_hub: crate::address_book_sync::AddrbookDirtyHub,
    /// ZEB-815: UI-signal seam for remote reachability additions — the
    /// Network-Health notify + `connectivity-reachability-changed` emit the
    /// `ReachabilityAnnounce` delta arm fired before routing data moved off the
    /// membership CRDT. `None` leaves both signals unfired, which is correct for
    /// callers with no UI stack (unit tests, and any construction that bypasses
    /// `start_node`).
    pub ingest_observer: Option<Arc<dyn crate::address_book_sync::AddrbookIngestObserver>>,
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
/// (rather than inside `event_loop::run`) so the resolver's feeders can
/// capture it without a separate plumbing pass: the address-book ingest
/// path adds records (ZEB-815) and the per-community membership-delta
/// consumer closure evicts on Leave/Kick. The link manager is passed in
/// pre-built so the event loop owns only the `spawn_accept_loop` call.
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

/// ZEB-358: build + emit the `voice-moderation-changed` overlay for
/// (community, channel). Resolves power levels from materialized membership;
/// lists currently-enforced muted/kicked owners; flags the local node's own
/// state. Best-effort (a failed emit is logged by Tauri, not surfaced).
#[allow(clippy::too_many_arguments)]
async fn emit_moderation_changed(
    app: &std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>,
    registry: &std::sync::Arc<crate::community_state_sync::CommunitySyncRegistry>,
    presence_map: &std::sync::Arc<tokio::sync::Mutex<crate::voice_presence::VoicePresenceMap>>,
    moderation_map: &std::sync::Arc<tokio::sync::Mutex<crate::voice_moderation::ActiveModeration>>,
    community: crate::owner_state_types::SpaceId,
    channel: crate::community_membership::ChannelId,
    self_owner: crate::owner_state_types::OwnerAddr,
    now_ms: u64,
) {
    let snap = {
        let g = moderation_map.lock().await;
        g.snapshot(&community, &channel, now_ms)
    };
    // Resolve power levels (mirror beacon_signer_is_member's resolution).
    let materialized = match registry.engine_arc(&community).await {
        Some(engine) => {
            let admin = engine.admin_addr();
            let state = engine.state();
            let guard = state.lock().await;
            Some(guard.materialized(admin))
        }
        None => None,
    };
    let roster_owners: Vec<[u8; 16]> = {
        let g = presence_map.lock().await;
        g.roster(&community, &channel)
            .into_iter()
            .map(|e| e.owner)
            .collect()
    };
    let power_of = |owner: &[u8; 16]| -> u8 {
        materialized
            .as_ref()
            .and_then(|m| {
                m.power_levels
                    .get(&crate::owner_state_types::OwnerAddr(*owner))
                    .copied()
            })
            .unwrap_or(0)
    };
    let mut powers = serde_json::Map::new();
    for o in roster_owners.iter().chain(std::iter::once(&self_owner.0)) {
        powers.insert(hex::encode(o), serde_json::json!(power_of(o)));
    }
    crate::node_event_sink::emit_ser(
        app.as_ref(),
        "voice-moderation-changed",
        &serde_json::json!({
            "community": hex::encode(community.0),
            "channel": hex::encode(channel.0),
            "mutedOwners": snap.muted.iter().map(hex::encode).collect::<Vec<_>>(),
            "kickedOwners": snap.kicked.iter().map(hex::encode).collect::<Vec<_>>(),
            // ZEB-612: owners holding an unexpired invite-to-speak, plus
            // whether WE are invited (drives the "Unmute?" banner).
            "invitedOwners": snap.invited.iter().map(hex::encode).collect::<Vec<_>>(),
            "powers": powers,
            "selfPower": power_of(&self_owner.0),
            "selfModMuted": snap.muted.contains(&self_owner.0),
            "selfKicked": snap.kicked.contains(&self_owner.0),
            "selfInvited": snap.invited.contains(&self_owner.0),
        }),
    );
}

/// ZEB-418 SP2 P2: wire one [`DatasetSyncHandles`] pair to a Zenoh pub/sub
/// on `harmony/owner/{addr_hex}/ds/{dataset}`. Parameterized copy of the
/// dm-inbox consumption block inside `run` (same outbound drain → put loop,
/// same backoff-resubscribe inbound loop, same degraded-event emits) —
/// extracted because P2 wires TWO datasets (dm-outhold-v1 + fleet-net-v1)
/// with identical plumbing.
///
/// `dataset` is the topic suffix (e.g. `"dm-outhold-v1"`); `degraded_event`
/// is the Tauri event emitted on adapter degradation (e.g.
/// `"dm-outhold-sync-degraded"`).
///
/// `max_payload_bytes` bounds each inbound sample BEFORE it is copied into
/// an owned Vec — these are peer-fed topics, so an unbounded copy would be
/// attacker-driven allocation (PR #222 round 1). Oversize samples are
/// dropped with a warn and the loop continues as if nothing arrived.
async fn spawn_dataset_sync_zenoh_adapter(
    session: &zenoh::Session,
    app: &std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>,
    closing: &Arc<AtomicBool>,
    handles: DatasetSyncHandles,
    dataset: &'static str,
    degraded_event: &'static str,
    max_payload_bytes: usize,
) {
    let topic = format!("harmony/owner/{}/ds/{}", handles.addr_hex, dataset);
    let emit_degraded = |reason: &str| {
        crate::node_event_sink::emit_ser(
            app.as_ref(),
            degraded_event,
            &serde_json::json!({
                "reason": reason,
                "topic": &topic,
            }),
        );
    };
    match zenoh::key_expr::KeyExpr::try_from(topic.clone()) {
        Ok(key_expr) => {
            // Outbound: drain engine publisher → Zenoh put.
            let session_pub = session.clone();
            let key_pub = key_expr.clone();
            let mut outbound_rx = handles.outbound_rx;
            let closing_pub = Arc::clone(closing);
            tokio::spawn(async move {
                while let Some(bytes) = outbound_rx.recv().await {
                    if let Err(e) = session_pub.put(&key_pub, bytes).await {
                        if !closing_pub.load(Ordering::SeqCst) {
                            tracing::warn!(error = %e, dataset, "dataset-root publish failed");
                        }
                    }
                }
            });

            // Inbound: Zenoh subscriber → engine subscriber_rx.
            // On transient recv_async errors, re-declare the subscriber
            // with exponential backoff. The only terminal condition is
            // `closing` becoming true (node shutdown) or `inbound_tx.send`
            // failing (engine dropped its receiver).
            match session.declare_subscriber(&key_expr).await {
                Ok(sub) => {
                    let inbound_tx = handles.inbound_tx;
                    let closing_sub = Arc::clone(closing);
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
                                                          // Size-gate BEFORE the owned copy:
                                                          // peer-fed topic — an unbounded
                                                          // to_vec() would be attacker-driven
                                                          // allocation. Drop and move on (a
                                                          // non-event, not a subscriber error).
                                        let len = sample.payload().len();
                                        if len > max_payload_bytes {
                                            tracing::warn!(
                                                dataset,
                                                len,
                                                max = max_payload_bytes,
                                                "dataset payload exceeds size cap; \
                                                 dropping sample"
                                            );
                                            continue;
                                        }
                                        let bytes: Vec<u8> = sample.payload().to_bytes().to_vec();
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
                                            dataset,
                                            "dataset-root subscriber closed unexpectedly; \
                                             will re-declare after backoff"
                                        );
                                        crate::node_event_sink::emit_ser(
                                            app_late.as_ref(),
                                            degraded_event,
                                            &serde_json::json!({
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
                                        dataset,
                                        "dataset-root subscriber re-declared successfully"
                                    );
                                    current_sub = new_sub;
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        error = %e,
                                        backoff_ms,
                                        dataset,
                                        "dataset-root subscriber re-declare failed; retrying"
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
                        dataset,
                        "failed to declare dataset-root subscriber"
                    );
                    emit_degraded("declare_subscriber_failed");
                }
            }
        }
        Err(e) => {
            tracing::error!(
                error = %e,
                %topic,
                dataset,
                "dataset-root key_expr invalid; FleetSyncEngine Zenoh adapter skipped"
            );
            emit_degraded("key_expr_invalid");
        }
    }
}

/// ZEB-620: build the zenoh `connect/endpoints` locator list. Unlike ZEB-368
/// (which merged every resolver-known iroh peer into the static connect set),
/// this carries ONLY the optional LAN/Reticulum connect endpoint — boot peers
/// now enter the reconnect supervisor as `NewPeer` kicks (see
/// [`crate::iroh_zenoh_registration::seed_boot_peers_into_supervisor`]). It takes
/// no resolver, so a known peer structurally cannot leak an `iroh/` locator into
/// zenoh's static connect set. Each entry is JSON-quoted for `insert_json5`.
fn build_connect_endpoints(endpoint: Option<&str>) -> Result<Vec<String>, String> {
    let mut connect_eps: Vec<String> = Vec::new();
    if let Some(ep) = endpoint {
        // Already-quoted JSON string form (mirrors the prior inline build).
        let ep_json =
            serde_json::to_string(ep).map_err(|e| format!("endpoint serialize error: {e}"))?;
        connect_eps.push(ep_json);
    }
    Ok(connect_eps)
}

/// ZEB-627: generation-keyed zid→node cache for the zenoh transport-events
/// listener. Values are `Option<[u8; 32]>` — `None` tombstones a zid unknown
/// at this generation, so repeated events from a non-peer session don't pay an
/// O(active_peers) rebuild each (the pre-ZEB-627 behavior). Any resolver
/// change (generation bump) clears the cache wholesale, covering BOTH stale
/// directions: a hit for an evicted/reassigned peer (stale-positive kicks for
/// departed nodes) and a tombstone hiding a newly learned peer.
struct ZidNodeCache {
    map: std::collections::HashMap<String, Option<[u8; 32]>>,
    seen_gen: Option<u64>,
}

impl ZidNodeCache {
    fn new() -> Self {
        Self {
            map: std::collections::HashMap::new(),
            seen_gen: None,
        }
    }

    /// Resolve `zid`. `current_gen` must be read from the resolver BEFORE the
    /// rebuild closure would run — a concurrent mutation mid-rebuild then
    /// forces another clear on the next event (conservative, never stale).
    fn lookup(
        &mut self,
        zid: &str,
        current_gen: u64,
        rebuild: impl FnOnce() -> std::collections::HashMap<String, [u8; 32]>,
    ) -> Option<[u8; 32]> {
        if self.seen_gen != Some(current_gen) {
            self.map.clear();
            self.seen_gen = Some(current_gen);
        }
        if let Some(cached) = self.map.get(zid) {
            return *cached;
        }
        // EXTEND, don't replace: at a stable generation the resolver view is
        // immutable (any add/remove bumps the generation), so previously
        // cached entries AND tombstones for OTHER zids are still valid —
        // replacing the map would discard those tombstones and re-pay the
        // O(active_peers) rebuild for every interleaved non-peer session
        // (final-review finding, 2026-07-04).
        self.map
            .extend(rebuild().into_iter().map(|(k, v)| (k, Some(v))));
        let hit = self.map.get(zid).copied().flatten();
        if hit.is_none() {
            self.map.insert(zid.to_string(), None); // tombstone
        }
        hit
    }
}

#[cfg(test)]
mod zid_node_cache_tests {
    use super::ZidNodeCache;
    use std::cell::Cell;
    use std::collections::HashMap;

    fn view(entries: &[(&str, [u8; 32])]) -> HashMap<String, [u8; 32]> {
        entries.iter().map(|(z, n)| (z.to_string(), *n)).collect()
    }

    #[test]
    fn hit_does_not_rebuild() {
        let mut c = ZidNodeCache::new();
        let rebuilds = Cell::new(0);
        let a = [1u8; 32];
        assert_eq!(
            c.lookup("z1", 0, || {
                rebuilds.set(rebuilds.get() + 1);
                view(&[("z1", a)])
            }),
            Some(a)
        );
        assert_eq!(
            c.lookup("z1", 0, || {
                rebuilds.set(rebuilds.get() + 1);
                view(&[("z1", a)])
            }),
            Some(a)
        );
        assert_eq!(rebuilds.get(), 1, "same-generation hit skips the rebuild");
    }

    #[test]
    fn tombstone_prevents_rebuild_per_event() {
        let mut c = ZidNodeCache::new();
        let rebuilds = Cell::new(0);
        for _ in 0..3 {
            assert_eq!(
                c.lookup("ghost", 7, || {
                    rebuilds.set(rebuilds.get() + 1);
                    view(&[])
                }),
                None
            );
        }
        assert_eq!(
            rebuilds.get(),
            1,
            "unknown zid rebuilds once per generation, then tombstones"
        );
    }

    #[test]
    fn interleaved_ghosts_keep_each_others_tombstones() {
        // Final-review finding (2026-07-04): a rebuild must EXTEND the map,
        // not replace it — replacement dropped same-generation tombstones for
        // other zids, so two interleaved non-peer sessions re-paid the full
        // rebuild on every event.
        let mut c = ZidNodeCache::new();
        let rebuilds = Cell::new(0);
        let mut probe = |zid: &str| {
            c.lookup(zid, 7, || {
                rebuilds.set(rebuilds.get() + 1);
                view(&[])
            })
        };
        for _ in 0..3 {
            assert_eq!(probe("ghost-a"), None);
            assert_eq!(probe("ghost-b"), None);
        }
        assert_eq!(
            rebuilds.get(),
            2,
            "one rebuild per distinct ghost per generation, not per event"
        );
    }

    #[test]
    fn generation_bump_clears_stale_positive() {
        let mut c = ZidNodeCache::new();
        let a = [1u8; 32];
        assert_eq!(c.lookup("z1", 0, || view(&[("z1", a)])), Some(a));
        // Resolver evicted z1's peer → generation bumped, view empty.
        assert_eq!(
            c.lookup("z1", 1, || view(&[])),
            None,
            "stale-positive entry does not survive a generation bump"
        );
    }

    #[test]
    fn generation_bump_reveals_new_peer_behind_tombstone() {
        let mut c = ZidNodeCache::new();
        let b = [2u8; 32];
        assert_eq!(c.lookup("z2", 0, || view(&[])), None); // tombstoned
        assert_eq!(
            c.lookup("z2", 1, || view(&[("z2", b)])),
            Some(b),
            "stale-negative tombstone does not survive a generation bump"
        );
    }
}

/// ZEB-928 (R4): membership-delta controller for the bounded-degree admission oracle.
///
/// Polls each joined community's O(1) [`materialized_version`] and, only when one
/// advances (or a community is left), recomputes the union of chosen ring neighbors
/// (`community_topology`) into the admitted device-key set and publishes it, then sweeps
/// so the filtered re-arm dials exactly the admitted, already-resolved neighbors. Rosters
/// for unchanged communities stay cached, so the O(N log N) recompute is delta-gated, not
/// per-tick. Spawned once at session open in router mode with an owner identity present.
///
/// [`materialized_version`]: crate::community_state_crdt::CommunityState::materialized_version
async fn run_admission_controller(
    registry: std::sync::Arc<crate::community_state_sync::CommunitySyncRegistry>,
    self_vk: [u8; 32],
    oracle: std::sync::Arc<crate::admission_oracle::AdmissionOracle>,
    supervisor: crate::reconnect_supervisor::SupervisorHandle,
) {
    use std::collections::{BTreeMap, BTreeSet};
    // Per-community (last-seen materialized version, active enrolled device keys). The
    // admitted set is the union over ALL joined communities, so unchanged communities
    // stay cached and only version-advanced ones re-read `materialized()`.
    let mut cache: BTreeMap<crate::owner_state_types::SpaceId, (u64, BTreeSet<[u8; 32]>)> =
        BTreeMap::new();
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(2));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let ids = registry.spawned_community_ids().await;
        let id_set: BTreeSet<crate::owner_state_types::SpaceId> = ids.iter().copied().collect();
        let mut changed = false;
        let before = cache.len();
        cache.retain(|k, _| id_set.contains(k));
        if cache.len() != before {
            changed = true; // left one or more communities → the union shrank
        }
        for id in &ids {
            let Some(engine) = registry.engine_arc(id).await else {
                continue;
            };
            let state = engine.state();
            let g = state.lock().await;
            let ver = g.materialized_version();
            if cache.get(id).map(|(v, _)| *v) == Some(ver) {
                continue; // unchanged since the last recompute
            }
            let mat = g.materialized(engine.admin_addr());
            drop(g);
            let devices: BTreeSet<[u8; 32]> =
                crate::community_gateway_dial_driver::enrolled_keys_from_members(&mat.members)
                    .into_iter()
                    .collect();
            cache.insert(*id, (ver, devices));
            changed = true;
        }
        if changed {
            let communities: Vec<(BTreeSet<[u8; 32]>, Vec<u8>)> = cache
                .iter()
                .map(|(id, (_, devs))| (devs.clone(), id.0.to_vec()))
                .collect();
            oracle.publish_admitted(crate::admission_oracle::compute_admitted(
                &communities,
                &self_vk,
            ));
            // Re-arm: the oracle-filtered sweep dials exactly the admitted, already-
            // resolved neighbors and drops the rest.
            supervisor.kick_sweep();
        }
    }
}

/// Run the NodeRuntime event loop as a background task.
///
/// Sends `Ok(())` on `ready_tx` once UDP + Zenoh + startup actions are
/// all initialized, or `Err(msg)` if any startup step fails.
/// Returns when shutdown signal fires.
#[allow(clippy::too_many_arguments)] // pre-existing; tracked for refactor
pub async fn run(
    mut runtime: NodeRuntime<MemoryBookStore>,
    startup_actions: Vec<RuntimeAction>,
    app: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>,
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
    mail_sync: Option<Arc<crate::mail_sync::MailSync>>,
    mut refresh_rx: mpsc::Receiver<crate::mail_sync::RefreshRequest>,
    mut pin_intent: std::collections::HashSet<[u8; 32]>,
    fetch_completion_tx: mpsc::Sender<[u8; 32]>,
    mut fetch_completion_rx: mpsc::Receiver<[u8; 32]>,
    pairing_in_tx: Option<mpsc::Sender<crate::pairing::types::PairingWireMessage>>,
    mut sync_handles: Option<SyncEngineHandles>,
    dm_outbox: Option<std::sync::Arc<tokio::sync::Mutex<crate::dm_outbox::DmOutbox>>>,
    dm_transport: Option<std::sync::Arc<dyn crate::dm_outbox::DmTransport>>,
    crdt_state: Option<std::sync::Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>>,
    // ZEB-703: owner-state SyncEngine handle for the drain tick. The drain's
    // Phase C / deposit-rung CRDT mutations must notify_dirty this engine or
    // they are never persisted at runtime nor replicated (same gating shape
    // as `dm_outbox` / `crdt_state`: `None` until an owner is loaded).
    owner_sync_engine: Option<std::sync::Arc<crate::owner_state_sync::SyncEngine>>,
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
    // ZEB-262 Phase 4 Task 9: community sync registry. Used for community
    // CRDT sync and adapter spawning. `None` until the owner identity is
    // loaded — same gating shape as `dm_outbox` / `crdt_state`.
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
    // ZEB-884: our own owner-card publisher, so a queryable on our card topic can
    // answer a late subscriber's query-on-subscribe GET with the cached signed
    // bytes (no re-sign). `None` when no owner identity is loaded.
    profile_card_publisher: Option<Arc<crate::profile_card_broadcast::ProfileCardPublisher>>,
    // ZEB-537: community-presence request receiver (paired with NodeState's
    // `community_presence_request_tx`) + the shared roster map (shared with
    // NodeState so IPC reads + the loop's subscriber/sweeper write the same
    // source of truth). `community_presence_request_rx` is `None` in test
    // callers that bypass `start_node`; the map is always provided.
    community_presence_request_rx: Option<mpsc::Receiver<CommunityPresenceRequest>>,
    community_presence_map: std::sync::Arc<
        tokio::sync::Mutex<crate::community_presence::CommunityPresenceMap>,
    >,
    // ZEB-815: address-book pool inputs (request rx + store + the two resolver
    // fan-out targets + the sidecar root). `None` in test callers that bypass
    // `start_node`; the pool is then not spawned and the presence subscribers
    // get a resync handle nobody consumes.
    addrbook_runtime: Option<AddressBookRuntime>,
    // Mint Phase 2 sync: channel pair bridging MintSyncEngine to Zenoh.
    // `None` when no owner identity is loaded.
    mut mint_sync_handles: Option<MintSyncHandles>,
    // ZEB-417 SP1: channel pair bridging the Notes FleetSyncEngine to Zenoh
    // on `harmony/owner/{addr_hex}/ds/notes-v1`. `None` when no owner identity
    // is loaded (and in test callers that bypass `start_node`).
    mut notes_sync_handles: Option<NotesSyncHandles>,
    // ZEB-418 SP2 P1: channel pair bridging the dm-inbox FleetSyncEngine to
    // Zenoh on `harmony/owner/{addr_hex}/ds/dm-inbox-v1`. `None` when no
    // owner identity is loaded (and in test callers that bypass `start_node`).
    mut dm_inbox_sync_handles: Option<DmInboxSyncHandles>,
    // ZEB-418 SP2 P2: the two P2 dataset channel pairs (dm-outhold-v1 +
    // fleet-net-v1), bridged to Zenoh on
    // `harmony/owner/{addr_hex}/ds/{dataset}`. `None` when no owner
    // identity is loaded (and in test callers that bypass `start_node`).
    mut p2_sync_handles: Option<P2SyncHandles>,
    // ZEB-458 P4 Phase B: the two relay dataset channel pairs (relay-hold-v1 +
    // relay-optin-v1), bridged to Zenoh on
    // `harmony/owner/{addr_hex}/ds/{dataset}`. `None` when no owner identity is
    // loaded (and in test callers that bypass `start_node`).
    mut relay_sync_handles: Option<RelaySyncHandles>,
    // ZEB-668 S1: the owner-trust dataset channel pair (owner-trust-v1 —
    // harmony-owner enrollments/vouching/revocations/liveness), bridged to
    // Zenoh on `harmony/owner/{addr_hex}/ds/owner-trust-v1`. `None` when no
    // owner identity is loaded (and in test callers that bypass `start_node`).
    mut trust_sync_handles: Option<DatasetSyncHandles>,
    // ZEB-677 S3: the quorum co-sign request dataset channel pair
    // (owner-quorum-req-v1) — pending quorum revocation requests across
    // the owner's fleet. `None` when no owner identity is loaded.
    mut quorum_sync_handles: Option<DatasetSyncHandles>,
    // ZEB-668 S5: the fleet-keys carrier dataset channel pair
    // (fleet-keys-v1) — epoch bump distribution across the owner's fleet.
    // `None` when no owner identity is loaded.
    mut fleet_keys_sync_handles: Option<DatasetSyncHandles>,
    // ZEB-495 (ZEB-340 Part 2): channel pair bridging the
    // community-device-intro FleetSyncEngine to Zenoh on
    // `harmony/owner/{addr_hex}/ds/community-device-intro-v1`. `None` when no
    // owner identity is loaded (and in test callers that bypass `start_node`).
    mut community_device_intro_sync_handles: Option<DatasetSyncHandles>,
    // ZEB-321 Phase 1 Task 8: bundle of iroh-transport resources built in
    // `start_node`. When `Some`, the event loop spawns the link-manager
    // accept loop + publisher driver as background tasks; when `None`
    // (test contexts that bypass `start_node`) the iroh subsystem stays
    // unwired and the resolver simply never receives updates — the rest
    // of the event loop is unaffected.
    iroh_handles: Option<IrohRuntimeHandles>,
    // ZEB-373: shared dynamic-dial telemetry. When `Some` (alongside
    // `iroh_handles`), the event loop installs the resolver's dial-hint
    // sender and spawns the dial driver to dial newly-learned peers via the
    // live zenoh `Runtime`. `None` in test contexts that bypass `start_node`.
    dial_telemetry: Option<std::sync::Arc<crate::network_health::DialTelemetry>>,
    // ZEB-395: shared serve-allowlist. The same handle is attached to the
    // production RuntimeContentStore (so publish_root_now's put_serveable
    // registers community-root CIDs) and consulted by the content-serve
    // queryable below. Empty for any caller that doesn't publish community roots.
    serve_allowlist: crate::content_store::CommunityServeAllowlist,
    // ZEB-418 P2 Task 7 (D16): routing-record re-publish trigger. The 250ms
    // timer arm invokes it every BUTLER_SET_REFRESH_MS (~half the butler-set
    // freshness window) so the published `bs_at` never lapses while the
    // device is up. The closure is sync (it spawns its own async work);
    // production passes the lib.rs closure that re-stamps the fleet-net
    // self-row and re-registers the active pkarr publications. `None` in
    // test callers that bypass `start_node` (and when no owner identity is
    // loaded).
    routing_republish: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
    // ZEB-434 D6: transport-epoch watch SENDER. The 5s peer-refresh arm
    // bumps the value whenever a never-before-seen zenoh session id
    // appears (zids are per-session, so a rebooted peer always presents
    // a fresh one → "new zid" = peer arrival/recovery). Per-community
    // root-fetch drivers (and the Task 7/9 channel-log / mail-root
    // subscribers) hold receiver clones and re-arm their satisfied
    // latches on each bump. Test callers that don't exercise the
    // catch-up path pass `tokio::sync::watch::channel(0u64).0` (a
    // sender with no receivers — send_modify is then a no-op).
    transport_epoch_tx: tokio::sync::watch::Sender<u64>,
    // ZEB-702 T3 (Component B): `Arc<dyn RepublishDirty>` over every
    // owner-scoped dataset engine — all 12: owner-state, fleet-net, dm-inbox,
    // dm-outhold, owner-trust, fleet-keys, owner-quorum-req,
    // community-device-intro, mint, notes, relay-hold, relay-optin (the last
    // two added by T3b). The listener spawned below
    // subscribes to `transport_epoch_tx` and nudges each on the transport
    // up-edge so a link that forms after the last publish still receives the
    // current root. Empty in test callers that bypass `start_node` and when no
    // owner identity is loaded — the listener is then not spawned.
    republish_on_epoch: Vec<Arc<dyn crate::fleet_sync::RepublishDirty>>,
    // ZEB-599 Direction 1: presence-driven full-reconcile sender. Cloned into
    // each community presence subscriber; bumped when a new roster device
    // (potential holder) appears so channel-log backfill drivers re-arm with a
    // FULL reconcile within the cooldown instead of waiting the ~1h floor. Test
    // callers that don't exercise presence pass a receiver-less sender (bump is
    // then a no-op), same as `transport_epoch_tx`.
    presence_resync_tx: tokio::sync::watch::Sender<u64>,
    // ZEB-618: restart-aware anti-entropy floor for the mail-root fetch
    // (ZEB-584 parity). `Some((interval_ms, persist))` makes the mail-root
    // driver's periodic full-refetch survive restarts: the interval is a
    // SINGLE jittered draw (built by `start_node_inner` where `app_data_dir`
    // is in scope) used for BOTH the persisted first-deadline computation and
    // the driver's interval arg — never two different draws. `persist` seeds
    // the first fire from `<data>/mail/backfill_state.cbor` and re-persists on
    // each fire. `None` (every test caller and any node without a mail dir)
    // keeps the legacy interval-from-spawn floor with a fresh per-spawn draw.
    mail_resync: Option<(u64, crate::channel_backfill::ResyncPersist)>,
    // ZEB-621: delta-gated address-change fan-out hub. `Some` when built by
    // production `start_node` (owner identity loaded). The reconnect-supervisor
    // block below installs the sweep hook onto it once the `SupervisorHandle`
    // exists, so a real self-address change kicks a presence sweep. `None` in
    // test callers that bypass `start_node` (the fan-out simply never sweeps).
    addr_change_fanout: Option<std::sync::Arc<crate::addr_change_fanout::AddrChangeFanout>>,
    // ZEB-612 S3: per-CID distinct announcing sessions, written by the
    // `harmony/announce/*` subscription arm below, swept + refreshed by the
    // re-announce tick, read by list_content/list_root for `replicaCount`.
    // Test callers that don't exercise announcements pass a fresh Arc.
    observed_holders: std::sync::Arc<std::sync::Mutex<crate::observed_holders::ObservedHolders>>,
    // ZEB-612 S3: the disk-backed content index, used by the re-announce
    // tick to rebuild this node's announceable set (Public, non-archived).
    // Same Arc as NodeState.content_index in production; test callers that
    // don't exercise re-announcement pass a fresh empty index.
    content_index: std::sync::Arc<std::sync::Mutex<crate::content_index::ContentIndex>>,
    // ZEB-669 S2: verified remote buddy records, written by the
    // `harmony/storage/*` subscription arm, read by the auto-pin engine
    // tick and the buddy IPCs. Test callers pass a fresh store.
    storage_records: std::sync::Arc<std::sync::Mutex<crate::storage_records::StorageRecordStore>>,
    // ZEB-669 S2: refcounted hosting ledger (engine writes, IPCs read).
    storage_ledger: std::sync::Arc<std::sync::Mutex<crate::storage_ledger::StorageLedger>>,
    // ZEB-669 S2: local buddy settings (budget/pledges), read by the
    // engine tick to derive pacts and enforce the shared budget.
    storage_settings: std::sync::Arc<std::sync::Mutex<crate::storage_settings::StorageSettings>>,
    // ZEB-669 S2: the local OWNER address (hex address_hash) — the
    // planner's `me` for pact derivation. Empty when no identity is
    // loaded; the engine tick no-ops then. Distinct from `own_zid`,
    // which is the transport-session id (announces stay anonymous).
    own_owner_addr: String,
    // ZEB-679: shared revoked-device projection consulted by storage-
    // record v2 admission (same handle the DM/friend/PEX cutoffs use).
    // Test callers pass `RevokedDeviceProjection::new()`.
    revoked_projection: crate::revoked_device_projection::RevokedDeviceProjection,
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

    // ZEB-620: the reconnect supervisor handle, built + spawned after `zenoh::open`
    // (it needs the live Runtime for its dialer) and threaded to the presence
    // subscribers below (presence-sweep kicks). Declared here so its lifetime spans
    // the whole event loop. Replaces ZEB-373's dial-hint mpsc install: the resolver
    // now kicks the supervisor directly (`set_supervisor`, installed post-open).
    let mut reconnect_supervisor: Option<crate::reconnect_supervisor::SupervisorHandle> = None;
    // The supervisor loop never exits on its own; its JoinHandle is kept so the
    // shutdown drain can abort it (otherwise it would keep dialing peers after
    // `run()` returns — CodeRabbit, PR #392).
    let mut reconnect_supervisor_task: Option<tokio::task::JoinHandle<()>> = None;
    // ZEB-928: the R4 admission controller loops forever like the supervisor; capture its
    // handle so the shutdown drain aborts it too (CodeRabbit, PR #674).
    let mut admission_controller_task: Option<tokio::task::JoinHandle<()>> = None;

    let mut config = zenoh::Config::default();
    // ZEB-809: LAN scouting (multicast AND gossip) is OFF by default.
    //
    // `zenoh::Config::default()` leaves both ENABLED, which makes this session
    // peer with every zenoh node reachable on the LAN. That is a second ingress
    // into the very session that carries our data plane, and it bypasses the
    // entire peer-acquisition design: no routing record, no membership check, no
    // dial policy, no reconnect supervision. Peers are supposed to arrive via
    // pkarr routing record → ZEB-373 iroh dial, and only that way.
    //
    // Measured on a four-node fleet (ZEB-803 investigation, 2026-07-26): 361
    // transport-session events in 90 minutes, 359 of them distinct foreign zids,
    // plus repeated connect attempts to scouted IPv6 locators the host has no
    // route to. Our own peers use `deterministic_zid_hex`, so none of those 359
    // were ours.
    //
    // We had already diagnosed this and fixed it for tests only: see
    // `hermetic_zenoh_config` below, whose doc describes exactly this pathology
    // and disables both knobs. Production disabled neither by default, and only
    // multicast under an env var — half a switch, opt-out.
    //
    // Safe because the dial genuinely works without scouting: the e2e probe
    // `s5c_clean_dial_only_card_propagation_probe` converges two clean
    // co-located nodes over the iroh dial alone. Cross-WAN peers never had
    // multicast in the first place, so this makes LAN behaviour match the path
    // that already had to work everywhere else — and stops the LAN masking a
    // broken dial (`zenoh-LAN-masks-iroh`).
    //
    // Opt-IN, not opt-out: set HARMONY_ZENOH_ENABLE_LAN_SCOUTING=1 to restore
    // stock zenoh discovery. Inverted deliberately — the old
    // HARMONY_ZENOH_DISABLE_MULTICAST made the safe configuration the one you
    // had to remember to ask for.
    //
    // The opt-in is strict: the value must be exactly "1" (after trimming
    // whitespace). "true", "yes", "false", or a typo all read as UNSET — for a
    // flag whose enable direction widens the attack surface, a mis-set value
    // must fail toward the safe default, never toward open (PR #558 review).
    if std::env::var("HARMONY_ZENOH_ENABLE_LAN_SCOUTING")
        .map(|v| v.trim() == "1")
        .unwrap_or(false)
    {
        // Set both knobs explicitly rather than riding zenoh's defaults: the
        // opt-in contract is "scouting ON", and it should survive a future
        // zenoh release flipping its default posture (PR #558 review).
        for knob in ["scouting/multicast/enabled", "scouting/gossip/enabled"] {
            if let Err(e) = config.insert_json5(knob, "true") {
                let e = format!("zenoh config error (enable {knob}): {e}");
                let _ = ready_tx.send(Err(e));
                return;
            }
        }
        tracing::warn!(
            "HARMONY_ZENOH_ENABLE_LAN_SCOUTING=1: zenoh multicast + gossip scouting ENABLED. \
             This session will peer with any zenoh node on the LAN, outside routing-record / \
             dial-policy control (ZEB-809)."
        );
    } else {
        for knob in ["scouting/multicast/enabled", "scouting/gossip/enabled"] {
            if let Err(e) = config.insert_json5(knob, "false") {
                let e = format!("zenoh config error (disable {knob}): {e}");
                let _ = ready_tx.send(Err(e));
                return;
            }
        }
        tracing::info!(
            "ZEB-809: zenoh LAN scouting disabled (multicast + gossip); peers arrive via \
             routing record + iroh dial only"
        );
    }
    // ZEB-390: give this session a DETERMINISTIC zenoh id derived from our own
    // iroh node-id, so a peer dialing us via the dynamic dial driver can compute
    // the SAME zid and `connect_peer`'s post-handshake transport lookup actually
    // matches. Previously every node took a random per-boot zid, so the dialer's
    // `connect_peer(ZenohIdProto::rand(), ...)` could never find the established
    // transport and ALWAYS reported failure → no Zenoh sync after a join. Must be
    // set BEFORE `zenoh::open`. See `iroh_dial_driver::deterministic_zid_hex`.
    if let Some(ref ih) = iroh_handles {
        let zid_hex =
            crate::iroh_dial_driver::deterministic_zid_hex(ih.endpoint.node_id().as_bytes());
        if let Err(e) = config.insert_json5("id", &format!("\"{zid_hex}\"")) {
            let e = format!("zenoh config error (id): {e}");
            let _ = ready_tx.send(Err(e));
            return;
        }
    }
    // ZEB-912: session mode (default "peer"; HARMONY_ZENOH_MODE=router opts into
    // zenoh's router hat, the only one with linkstate multi-hop data routing in
    // 1.9.0). Set alongside a `timestamping/enabled` pin: the timestamping
    // default is mode-dependent (router=true, peer=false) and would silently
    // start HLC-stamping every data message on a router-mode node — pinning
    // false in BOTH modes keeps the wire identical to today regardless of mode.
    let zenoh_mode = zenoh_session_mode();
    // Logged unconditionally so a test (or operator) has POSITIVE evidence of
    // the mode that actually applied — e2e s14 asserts this exact line per
    // node, so a knob regression fails as "mode never engaged" instead of
    // masquerading as a transport timeout (CodeRabbit #671).
    tracing::info!("ZEB-912: zenoh session mode: {zenoh_mode}");
    if let Err(e) = config.insert_json5("mode", &format!("\"{zenoh_mode}\"")) {
        let e = format!("zenoh config error (mode): {e}");
        let _ = ready_tx.send(Err(e));
        return;
    }
    if let Err(e) = config.insert_json5("timestamping/enabled", "false") {
        let e = format!("zenoh config error (timestamping/enabled): {e}");
        let _ = ready_tx.send(Err(e));
        return;
    }
    // ZEB-912: zenoh's default listen endpoint is ALSO mode-dependent — peer
    // binds ephemeral `tcp/[::]:0`, router binds the FIXED zenohd port
    // `tcp/[::]:7447`. Co-located router-mode nodes collide on it (e2e s14:
    // second node's `zenoh::open` died "Address already in use"), and the TCP
    // listener is vestigial for harmony anyway (links ride iroh; scouting is
    // off; connect/endpoints is empty — nothing ever dials it). Normalize the
    // router default to the same ephemeral bind peer mode uses, BEFORE the
    // listen-endpoint merge below reads the value back. Gated on router mode so
    // peer-mode config stays byte-identical to today.
    if zenoh_mode == "router" {
        if let Err(e) = config.insert_json5(
            "listen/endpoints",
            r#"{"router":["tcp/[::]:0"],"peer":["tcp/[::]:0"]}"#,
        ) {
            let e = format!("zenoh config error (listen/endpoints router-default): {e}");
            let _ = ready_tx.send(Err(e));
            return;
        }
    }

    // ZEB-620: the LAN/Reticulum connect endpoint is the ONLY thing seeded into
    // zenoh's static `connect/endpoints`. ZEB-368 also injected every
    // resolver-known iroh peer here (`iroh_connect_locators`); that static seed is
    // retired — boot peers now enter the reconnect supervisor as `NewPeer` kicks
    // after `zenoh::open` (see `seed_boot_peers_into_supervisor` below), so a peer
    // whose first dial fails or later drops is reconnected indefinitely rather than
    // dialed once at boot. `merge_iroh_listen_endpoints` (the LISTEN side) is
    // unchanged.
    let connect_eps = match build_connect_endpoints(endpoint.as_deref()) {
        Ok(v) => v,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };
    if !connect_eps.is_empty() {
        let arr = format!("[{}]", connect_eps.join(","));
        if let Err(e) = config.insert_json5("connect/endpoints", &arr) {
            let e = format!("zenoh config error (connect/endpoints): {e}");
            let _ = ready_tx.send(Err(e));
            return;
        }
    }

    // ZEB-368: listen on our own iroh locator so Zenoh creates the iroh manager via
    // the factory (→ starts the inbound forwarder, registers harmony's manager) at
    // open even on inbound-only / no-known-peer nodes. new_listener is a no-op that
    // returns the locator (harmony's spawn_accept_loop already owns the accept loop).
    if let Some(ref ih) = iroh_handles {
        // MERGE iroh into the listen set: insert_json5 overwrites (no merge), so we
        // read listen/endpoints back and APPEND our locator, preserving every existing
        // listener (default peer `tcp/[::]:0`, the router listener, any other path's).
        // Overwriting iroh-only would silently drop the LAN zenoh transport (CodeRabbit
        // + Qodo, PR#188).
        let self_loc =
            crate::iroh_zenoh_registration::iroh_listen_locator(ih.endpoint.node_id().as_bytes());
        let current = config.get_json("listen/endpoints").ok();
        let eps = crate::iroh_zenoh_registration::merge_iroh_listen_endpoints(
            current.as_deref(),
            &self_loc,
            zenoh_mode,
        );
        if let Err(e) = config.insert_json5("listen/endpoints", &eps) {
            let e = format!("zenoh config error (listen/endpoints): {e}");
            let _ = ready_tx.send(Err(e));
            return;
        }
    }

    // ZEB-616 Component C: bound how long a silently-dead path's zenoh face
    // can linger when no reconnect arrives to trigger the accept-loop teardown
    // (zenoh_iroh_transport.rs, Component A). GATED on iroh being enabled:
    // this tuning targets the iroh stale-face case, and zenoh's
    // `transport/link/tx` lease is transport-GLOBAL (not per-link-kind), so
    // applying it only on iroh-enabled runs keeps the blast radius off
    // pure-non-iroh runs — matching the config-scope discipline of PR#188/#268
    // (which preserved other listeners in this same block). Within an
    // iroh-enabled run the shortened lease also covers any coexisting LAN/TCP
    // links, which is benign (keep_alive probes every 1s ≪ the 4s lease keep a
    // healthy link alive) and consistent with faster mesh convergence. In
    // zenoh 1.9.0 `keep_alive` is the number of keep-alive probes per lease
    // (probe interval = lease / keep_alive): lease 4000ms with keep_alive 4 →
    // a probe every 1s, a dead path's face reaped within ~4s (vs the ~10s
    // default lease). keep_alive=4 matches the current default but is set
    // explicitly so the probe cadence is pinned against a future default
    // change.
    if iroh_handles.is_some() {
        if let Err(e) = config.insert_json5("transport/link/tx/lease", "4000") {
            let e = format!("zenoh config error (tx/lease): {e}");
            let _ = ready_tx.send(Err(e));
            return;
        }
        if let Err(e) = config.insert_json5("transport/link/tx/keep_alive", "4") {
            let e = format!("zenoh config error (tx/keep_alive): {e}");
            let _ = ready_tx.send(Err(e));
            return;
        }
    }

    let (zenoh_runtime, session) =
        match cancellable!(open_session_with_runtime(config), "zenoh::open") {
            Ok(pair) => pair,
            Err(e) => {
                let e = format!("zenoh open failed: {e}");
                let _ = ready_tx.send(Err(e.clone()));
                crate::node_event_sink::emit_ser(
                    app.as_ref(),
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

    // ZEB-620: the reconnect supervisor OWNS all dialing. Build one supervisor per
    // node session, wire it as the single dial authority, and spawn its loop:
    //  - resolver `set_supervisor`: first-learn → `NewPeer`, record change →
    //    `RecordChanged` kicks (ZEB-620 Task 4);
    //  - link-manager `set_reconnect_handle`: registry drop-watcher → `Dropped`
    //    kick + successful-swap `mark_connected` (Task 3);
    //  - boot seeding: every resolver-known peer enters as a `NewPeer` kick
    //    (recency-ordered) instead of ZEB-368's static `connect/endpoints` seed;
    //  - zenoh transport-events listener: a transport `Delete` → `Dropped` kick,
    //    a non-fatal secondary drop source behind the registry watchers.
    // ZEB-622: peer-liveness state machine. Constructed unconditionally (it is
    // useful even if the supervisor block's install-order gate trips below) and
    // wired to the SAME transport epoch as the zid-poll gate, so a registry or
    // zenoh transport up-edge re-arms the backfill latches exactly like a new
    // zid does. `set_transport_epoch_tx` is install-once; the resolver + link-
    // manager installs mirror the supervisor wiring (the resolver clone shares
    // state via its inner `Arc`). Order between the installs and the epoch-tx
    // wiring is irrelevant — each is an independent install-once seam.
    let liveness = crate::peer_liveness::LivenessHandle::new();
    liveness.set_transport_epoch_tx(transport_epoch_tx.clone());
    if let Some(ref ih) = iroh_handles {
        if ih
            .link_manager
            .set_liveness_handle(liveness.clone())
            .is_err()
        {
            tracing::error!("ZEB-622: liveness handle already installed; keeping the existing one");
        }
        ih.link_manager.resolver().set_liveness(liveness.clone());
    }

    if let (Some(ref ih), Some(ref telemetry)) = (&iroh_handles, &dial_telemetry) {
        use crate::reconnect_supervisor::{
            run_reconnect_supervisor, SupervisorConfig, SupervisorHandle,
        };

        let self_nid = *ih.endpoint.node_id().as_bytes();
        let handle = SupervisorHandle::new();
        let resolver = ih.link_manager.resolver();

        // Install on the transport FIRST — the only fallible install. Installing
        // on the resolver before knowing the transport accepted would, on
        // failure, split the producers: resolver/presence kicks feeding a fresh
        // loop whose `Dropped` events the drop-watchers never deliver (they keep
        // kicking the previously-installed handle). One `run()` per link manager
        // makes the failure unreachable in practice; skipping supervisor startup
        // keeps even that path consistent (CodeRabbit, PR #392).
        if ih
            .link_manager
            .set_reconnect_handle(handle.clone())
            .is_err()
        {
            tracing::error!(
                "ZEB-620: a reconnect handle is already installed on this link \
                 manager; skipping reconnect-supervisor startup to avoid split \
                 producers"
            );
        } else {
            resolver.set_supervisor(handle.clone());

            // ZEB-928 (R4): install the bounded-degree admission oracle on both the
            // supervisor (reads it at kick / do_sweep / parole) and the resolver (feeds
            // it verified node_id→enrolled_key bindings). Router mode enables filtering;
            // peer mode installs a disabled oracle that admits everything, so behavior is
            // unchanged. Spawn the membership-delta controller only when filtering is on
            // AND an owner identity is present (→ a self device key + a registry).
            let admission_oracle = std::sync::Arc::new(
                crate::admission_oracle::AdmissionOracle::new(zenoh_session_mode() == "router"),
            );
            handle.set_admission_oracle(std::sync::Arc::clone(&admission_oracle));
            resolver.set_admission_oracle(std::sync::Arc::clone(&admission_oracle));
            if admission_oracle.enabled() {
                if let (Some(registry), Some(dm_outbox)) =
                    (community_registry.clone(), dm_outbox.as_ref())
                {
                    let self_vk = {
                        let g = dm_outbox.lock().await;
                        g.community_signing_key.verifying_key().to_bytes()
                    };
                    admission_controller_task = Some(tokio::spawn(run_admission_controller(
                        registry,
                        self_vk,
                        std::sync::Arc::clone(&admission_oracle),
                        handle.clone(),
                    )));
                }
            }

            // ZEB-621: install the supervisor sweep hook onto the address-change
            // fan-out — the `SupervisorHandle` exists only inside this block. A
            // real self-address change then kicks a presence sweep (re-arming
            // every known non-connected peer) instead of waiting on the next idle
            // cycle. Install-once; absent in test callers (fan-out is `None`).
            if let Some(ref fanout) = addr_change_fanout {
                let sweep_handle = handle.clone();
                fanout.set_supervisor_sweep(Box::new(move || sweep_handle.kick_sweep()));
            }

            let dialer = std::sync::Arc::new(crate::iroh_dial_driver::RuntimePeerDialer::new(
                zenoh_runtime.clone(),
            ));
            reconnect_supervisor_task = Some(tokio::spawn(run_reconnect_supervisor(
                handle.clone(),
                dialer,
                std::sync::Arc::new(resolver.clone()),
                std::sync::Arc::clone(telemetry),
                self_nid,
                SupervisorConfig::default(),
            )));

            // Boot seed: every peer the resolver already knows enters the supervisor
            // as a `NewPeer` kick (recency-ordered), so a peer whose first dial fails
            // or later drops is reconnected indefinitely — not dialed once at boot.
            let seeded = crate::iroh_zenoh_registration::seed_boot_peers_into_supervisor(
                &resolver, &self_nid, &handle,
            );
            if !seeded.is_empty() {
                tracing::info!(
                    "ZEB-620: seeded {} boot peer(s) into the reconnect supervisor",
                    seeded.len()
                );
            }

            // Zenoh transport-events listener: a transport `Delete` maps the peer's
            // zid back to its iroh node-id (via the resolver + the deterministic
            // zid derivation) and kicks `Dropped`. The registry drop-watchers are the
            // primary drop source; this is a non-fatal secondary that also catches a
            // face zenoh reaps without a registry eviction. Listener-declare failure
            // is warned once and the task exits (reconnect still works via the
            // watchers).
            let listener_session = session.clone();
            let listener_resolver = resolver.clone();
            let listener_handle = handle.clone();
            let listener_liveness = liveness.clone();
            tokio::spawn(async move {
                use zenoh::sample::SampleKind;
                let listener = match listener_session.info().transport_events_listener().await {
                    Ok(l) => l,
                    Err(e) => {
                        tracing::warn!(
                            "ZEB-620: zenoh transport-events listener unavailable ({e}); \
                         relying on registry drop-watchers for reconnect kicks"
                        );
                        return;
                    }
                };
                // ZEB-627: generation-keyed zid→node cache (see ZidNodeCache).
                // Replaces the former miss-only rebuild map, whose hits were
                // never revalidated (stale-positive kicks for departed peers)
                // and whose unknown-zid misses rebuilt O(active_peers) on
                // every event (no negative cache).
                let mut zid_cache = ZidNodeCache::new();
                while let Ok(event) = listener.recv_async().await {
                    // Resolve the peer's zid → iroh node-id ONCE via the cache,
                    // then dispatch on the event kind. `SampleKind` is a closed
                    // two-variant enum, so Put/Delete are exhaustive (no `_` arm
                    // — one would be an unreachable-pattern error under -D warnings).
                    let zid = event.transport().zid().to_string();
                    let current_gen = listener_resolver.generation();
                    let node_id = zid_cache.lookup(&zid, current_gen, || {
                        // ZEB-702 T3: enumerate the DIAL view (`list_dialable_peers`),
                        // which INCLUDES the fleet slot, not the butler/diagnostics
                        // view (`list_active_peers`, backed by `durable_preferred`,
                        // which excludes it). A fleet-only sibling's transport Delete
                        // now maps back to its node-id so the secondary Dropped
                        // reconnect kick / liveness down-edge fires for it too — the
                        // registry drop-watchers remain the primary source.
                        listener_resolver
                            .list_dialable_peers()
                            .into_iter()
                            .map(|(_owner, entry)| {
                                let node_id = entry.payload.iroh_node_id;
                                (
                                    crate::iroh_dial_driver::deterministic_zid_hex(&node_id),
                                    node_id,
                                )
                            })
                            .collect()
                    });
                    match event.kind() {
                        // Down-edge: kick the reconnect supervisor (ZEB-620) AND
                        // raise an external liveness down-edge (ZEB-622) so a
                        // conn-less (zenoh-only) slot leaves Degraded — a
                        // registry-backed slot is untouched (its Disconnected
                        // edge is owned by that conn's drop watcher).
                        SampleKind::Delete => match node_id {
                            Some(n) => {
                                listener_handle.kick(
                                    n,
                                    crate::reconnect_supervisor::ReconnectTrigger::Dropped,
                                );
                                listener_liveness.on_transport_down_external(n);
                            }
                            None => tracing::debug!(
                        "ZEB-620: transport Delete for zid {zid} not a resolver-known iroh peer; \
                         no reconnect kick"
                    ),
                        },
                        // Up-edge (ZEB-622): raise an external liveness up-edge
                        // (acts only if the peer is absent/Disconnected — never
                        // clobbers a conn-backed registry state).
                        SampleKind::Put => match node_id {
                            Some(n) => listener_liveness.on_transport_up_external(n),
                            None => tracing::debug!(
                        "ZEB-622: transport Put for zid {zid} not a resolver-known iroh peer; \
                         no liveness up-edge"
                    ),
                        },
                    }
                }
                tracing::debug!(
                    "ZEB-620: zenoh transport-events listener stopped (session closed)"
                );
            });

            reconnect_supervisor = Some(handle);
        }
    }

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
            crate::node_event_sink::emit_ser(
                app.as_ref(),
                "state-root-sync-degraded",
                &serde_json::json!({
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

                // ZEB-707 (server side of the pull): a queryable on the
                // state-root topic. A butler that missed the live push GETs this
                // key; we forward each query to the engine over `root_serve_tx`
                // and reply the current root wire, which the butler runs through
                // its normal inbound decrypt/merge path. Mirrors the community
                // state-root queryable (`spawn_community_state_zenoh_adapter`).
                let session_qbl = session.clone();
                let key_qbl = key_expr.clone();
                let topic_qbl = topic.clone();
                let closing_qbl = Arc::clone(&closing);
                let app_qbl = app.clone();
                let root_serve_tx_qbl = handles.root_serve_tx;
                tokio::spawn(async move {
                    let qbl = match session_qbl.declare_queryable(&key_qbl).await {
                        Ok(q) => q,
                        Err(e) => {
                            if !closing_qbl.load(Ordering::SeqCst) {
                                tracing::error!(topic = %topic_qbl, error = %e,
                                    "failed to declare owner-state root queryable");
                                // Surface the serving failure to the GUI like the
                                // subscriber path, so a dead pull-serve side is
                                // visible rather than silent (CodeRabbit). This
                                // queryable structure mirrors the production-proven
                                // community state-root queryable; adding
                                // re-declaration recovery to both is a codebase-wide
                                // follow-up, not owner-state-specific.
                                crate::node_event_sink::emit_ser(
                                    app_qbl.as_ref(),
                                    "state-root-sync-degraded",
                                    &serde_json::json!({
                                        "reason": "declare_queryable_failed",
                                        "topic": &topic_qbl,
                                    }),
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
                                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                                if root_serve_tx_qbl.send(reply_tx).await.is_err() {
                                    break; // engine gone
                                }
                                // ZEB-437: bounded wait — same rationale as the
                                // community state-root queryable. A mid-encode
                                // stop_node must not pin this queryable (and
                                // thus adapter teardown) past the ~1s closing
                                // SLA. `None` = reply abandoned; on a
                                // closing-abandon break now.
                                match recv_root_reply_bounded(reply_rx, &closing_qbl, &topic_qbl).await {
                                    Some(wire) => {
                                        if let Err(e) = query.reply(query.key_expr(), wire).await {
                                            tracing::warn!(topic = %topic_qbl, error = %e,
                                                "owner-state root queryable reply failed");
                                        }
                                    }
                                    None => {
                                        if closing_qbl.load(Ordering::SeqCst) {
                                            break;
                                        }
                                    }
                                }
                            }
                            _ = tokio::time::sleep(Duration::from_secs(1)) => {
                                if closing_qbl.load(Ordering::SeqCst) { break; }
                            }
                        }
                    }
                });

                // ZEB-707 (receiver side of the pull): a GET-driver task fed by
                // a `run_root_fetch_driver`. The driver re-arms on transport-epoch
                // bumps (a fleet peer (re)connecting), the presence kick, and an
                // ~hourly anti-entropy floor; each arm sends one request, and this
                // task runs the zenoh GET and pipes every reply into the engine's
                // inbound path — so a butler PULLs the primary's root when the
                // live push never fired (D3 Mode B). Mirrors the community
                // `rf_handle` GET driver + the mail-root driver spawn. The fetch
                // request/report channel is local: both ends live in this scope.
                let (fetch_request_tx, mut fetch_request_rx) =
                    tokio::sync::mpsc::channel::<CommunityRootFetchRequest>(8);
                let session_rf = session.clone();
                let key_rf = topic.clone();
                // Clone BEFORE the subscriber task below moves `handles.inbound_tx`.
                let subscriber_tx_rf = handles.inbound_tx.clone();
                let closing_rf = Arc::clone(&closing);
                tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            biased;
                            maybe = fetch_request_rx.recv() => {
                                let Some(req) = maybe else { break; };
                                // NB: this GET intentionally uses the default
                                // `Locality::Any`, NOT `Remote`. A paired butler
                                // is often co-located with its primary (same home
                                // LAN — the D3 topology), and `Locality::Remote`
                                // was observed live to exclude the co-located
                                // primary's queryable, not just this node's own
                                // self-reply — breaking the pull entirely (D3 red).
                                // The self-reply that `Remote` would suppress is
                                // benign: a reachable primary answers the SAME GET
                                // (the pull's purpose), so the latch is satisfied by
                                // a real root; the only case a lone self-reply
                                // satisfies it early is when the primary didn't
                                // answer — where no pull could have succeeded anyway,
                                // and the epoch/floor re-arm retries. Its own root
                                // is echo-suppressed on the inbound side regardless.
                                let receiver = match session_rf
                                    .get(&key_rf)
                                    .consolidation(zenoh::query::ConsolidationMode::None)
                                    .timeout(Duration::from_secs(10))
                                    .await
                                {
                                    Ok(r) => r,
                                    Err(e) => {
                                        if !closing_rf.load(Ordering::SeqCst) {
                                            tracing::warn!(key = %key_rf, error = %e,
                                                "owner-state root fetch query failed");
                                        }
                                        continue; // report drops → driver maps to NoReply
                                    }
                                };
                                let mut replies: usize = 0;
                                // ZEB-812: never await subscriber_tx inside
                                // the reply-drain arm — that holds zenoh's
                                // reply channel hostage on engine
                                // backpressure and can park zenoh's net
                                // thread (see `reply_spill` module doc).
                                let mut spill = crate::reply_spill::ReplySpill::new(
                                    subscriber_tx_rf.clone(),
                                    ROOT_FETCH_SPILL_MAX,
                                );
                                let drained_clean: bool = loop {
                                    tokio::select! {
                                        biased;
                                        res = receiver.recv_async() => {
                                            let Ok(reply) = res else { break true; };
                                            if let Ok(sample) = reply.into_result() {
                                                // ZEB-707: bound the reply before
                                                // materializing it — a matching peer
                                                // controls this payload, and the
                                                // engine only validates it after the
                                                // copy (CodeRabbit). A real root wire
                                                // is a few KB; skip anything wildly
                                                // larger without allocating or
                                                // counting it.
                                                if sample.payload().len()
                                                    > crate::fleet_sync::MAX_ROOT_WIRE_BYTES
                                                {
                                                    tracing::warn!(key = %key_rf,
                                                        len = sample.payload().len(),
                                                        "owner-state root reply exceeds wire cap; skipping");
                                                    continue;
                                                }
                                                let bytes: Vec<u8> =
                                                    sample.payload().to_bytes().to_vec();
                                                match spill.accept(bytes) {
                                                    crate::reply_spill::AcceptOutcome::Accepted => {
                                                        replies = replies.saturating_add(1);
                                                    }
                                                    crate::reply_spill::AcceptOutcome::DroppedFull => {}
                                                    crate::reply_spill::AcceptOutcome::ConsumerGone => {
                                                        return; // engine teardown
                                                    }
                                                }
                                            }
                                        }
                                        _ = tokio::time::sleep(Duration::from_millis(500)) => {
                                            if closing_rf.load(Ordering::SeqCst) { break false; }
                                        }
                                    }
                                };
                                if spill.dropped() > 0 {
                                    tracing::warn!(key = %key_rf, dropped = spill.dropped(),
                                        "owner-state root fetch: reply storm exceeded spill cap; \
                                         overflow dropped (next reconcile re-fetches)");
                                }
                                // ZEB-816: peak buffer depth against the cap,
                                // so a clean drain (dropped == 0) still says
                                // whether the spill ran near the ceiling or
                                // was never exercised. Captured before flush()
                                // consumes the spill.
                                tracing::debug!(
                                    target: "harmony_channel",
                                    key = %key_rf,
                                    replies,
                                    spill_peak = spill.peak(),
                                    spill_cap = ROOT_FETCH_SPILL_MAX,
                                    "owner-state root fetch: reply drain complete"
                                );
                                // ZEB-812: post-drain delivery; report only
                                // once the page has landed (or never, on
                                // shutdown/teardown — matching the old
                                // no-report semantics of those paths).
                                let flushed_clean = drained_clean
                                    && match spill.flush(&closing_rf).await {
                                        crate::reply_spill::FlushOutcome::Flushed => true,
                                        crate::reply_spill::FlushOutcome::ConsumerGone => return,
                                        crate::reply_spill::FlushOutcome::ShutdownAbandoned => {
                                            false
                                        }
                                    };
                                if flushed_clean {
                                    let _ = req.report.send(replies);
                                }
                            }
                            _ = tokio::time::sleep(Duration::from_secs(1)) => {
                                if closing_rf.load(Ordering::SeqCst) { break; }
                            }
                        }
                    }
                });

                // Shutdown bridge: flip the driver's watch when `closing` flips
                // (1s poll — mirrors the mail-root driver and the adapter tasks).
                let (owner_root_shutdown_tx, owner_root_shutdown_rx) =
                    tokio::sync::watch::channel(false);
                {
                    let closing_drv = Arc::clone(&closing);
                    tokio::spawn(async move {
                        loop {
                            tokio::time::sleep(Duration::from_secs(1)).await;
                            if closing_drv.load(Ordering::SeqCst) {
                                let _ = owner_root_shutdown_tx.send(true);
                                return;
                            }
                        }
                    });
                }
                let request_root = move || {
                    let fetch_tx = fetch_request_tx.clone();
                    async move {
                        let (report_tx, report_rx) = tokio::sync::oneshot::channel();
                        if fetch_tx
                            .send(CommunityRootFetchRequest { report: report_tx })
                            .await
                            .is_err()
                        {
                            return crate::channel_backfill::RootFetch::EngineGone;
                        }
                        match report_rx.await {
                            Ok(n) if n > 0 => crate::channel_backfill::RootFetch::Answered,
                            Ok(_) | Err(_) => crate::channel_backfill::RootFetch::NoReply,
                        }
                    }
                };
                tokio::spawn(crate::channel_backfill::run_root_fetch_driver(
                    crate::channel_backfill::RootFetchLatch::new(),
                    request_root,
                    owner_root_shutdown_rx,
                    // event_loop holds the epoch/presence SENDERS; derive receivers.
                    // `.subscribe()` is legal here — this precedes the presence
                    // sender's move into the presence task.
                    Some(transport_epoch_tx.subscribe()),
                    Some(presence_resync_tx.subscribe()),
                    // ZEB-425 anti-entropy floor: re-arm ~hourly (jittered) even
                    // with no epoch bump. Not restart-aware — the epoch + presence
                    // kicks cover reconnect; a persisted floor is a future add.
                    Some(crate::channel_backfill::periodic_resync_interval_ms()),
                    || {
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0)
                    },
                    None,
                ));

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
                                            crate::node_event_sink::emit_ser(
                                                app_late.as_ref(),
                                                "state-root-sync-degraded",
                                                &serde_json::json!({
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
            crate::node_event_sink::emit_ser(
                app.as_ref(),
                "mint-root-sync-degraded",
                &serde_json::json!({
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
                                            crate::node_event_sink::emit_ser(
                                                app_late.as_ref(),
                                                "mint-root-sync-degraded",
                                                &serde_json::json!({
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

    // ── ZEB-417 SP1: Notes fleet-sync Zenoh adapter for ds/notes-v1 topic ─
    // Mirrors the mint-root wiring above (including the superior
    // backoff-resubscribe inbound loop): outbound bytes from the Notes
    // FleetSyncEngine's publish path → Zenoh put; inbound bytes from Zenoh →
    // the engine's subscriber path. `None` when no owner identity is loaded.
    if let Some(handles) = notes_sync_handles.take() {
        let topic = format!("harmony/owner/{}/ds/notes-v1", handles.addr_hex);
        let emit_notes_degraded = |reason: &str| {
            crate::node_event_sink::emit_ser(
                app.as_ref(),
                "notes-sync-degraded",
                &serde_json::json!({
                    "reason": reason,
                    "topic": &topic,
                }),
            );
        };
        match zenoh::key_expr::KeyExpr::try_from(topic.clone()) {
            Ok(key_expr) => {
                // Outbound: drain Notes engine publisher → Zenoh put.
                let session_pub = session.clone();
                let key_pub = key_expr.clone();
                let mut outbound_rx = handles.outbound_rx;
                let closing_pub = Arc::clone(&closing);
                tokio::spawn(async move {
                    while let Some(bytes) = outbound_rx.recv().await {
                        if let Err(e) = session_pub.put(&key_pub, bytes).await {
                            if !closing_pub.load(Ordering::SeqCst) {
                                tracing::warn!(error = %e, "notes-root publish failed");
                            }
                        }
                    }
                });

                // Inbound: Zenoh subscriber → Notes engine subscriber_rx.
                // On transient recv_async errors, re-declare the subscriber
                // with exponential backoff. The only terminal condition is
                // `closing` becoming true (node shutdown) or `inbound_tx.send`
                // failing (engine dropped its receiver).
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
                                                "notes-root subscriber closed unexpectedly; \
                                                 will re-declare after backoff"
                                            );
                                            crate::node_event_sink::emit_ser(
                                                app_late.as_ref(),
                                                "notes-sync-degraded",
                                                &serde_json::json!({
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
                                            "notes-root subscriber re-declared successfully"
                                        );
                                        current_sub = new_sub;
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            error = %e,
                                            backoff_ms,
                                            "notes-root subscriber re-declare failed; retrying"
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
                            "failed to declare notes-root subscriber"
                        );
                        emit_notes_degraded("declare_subscriber_failed");
                    }
                }
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    %topic,
                    "notes-root key_expr invalid; Notes FleetSyncEngine Zenoh adapter skipped"
                );
                emit_notes_degraded("key_expr_invalid");
            }
        }
    }

    // ── ZEB-418 SP2 P1 / ZEB-423: dm-inbox fleet-sync Zenoh adapter ─
    // Behaviourally identical to the P2 datasets below, so it now routes
    // through the shared `spawn_dataset_sync_zenoh_adapter` helper instead of
    // an open-coded copy of the same outbound-drain + backoff-resubscribe
    // inbound loop. The migration's payoff (ZEB-423) is the helper's
    // `max_payload_bytes` size gate: this peer-fed topic previously copied
    // every inbound sample into an owned `Vec<u8>` with no cap, so an oversize
    // frame was attacker-driven allocation before the FleetSync engine could
    // reject it. `None` when no owner identity is loaded.
    if let Some(handles) = dm_inbox_sync_handles.take() {
        spawn_dataset_sync_zenoh_adapter(
            &session,
            &app,
            &closing,
            DatasetSyncHandles {
                addr_hex: handles.addr_hex,
                outbound_rx: handles.outbound_rx,
                inbound_tx: handles.inbound_tx,
            },
            "dm-inbox-v1",
            "dm-inbox-sync-degraded",
            crate::butler_deposit::DM_INBOX_DATASET_MAX_BYTES,
        )
        .await;
    }

    // ── ZEB-418 SP2 P2: dm-outhold + fleet-net fleet-sync Zenoh adapters ─
    // Same plumbing as the dm-inbox block above (extracted into
    // `spawn_dataset_sync_zenoh_adapter` because P2 wires two datasets):
    // outbound bytes from each FleetSyncEngine's publish path → Zenoh put;
    // inbound bytes from Zenoh → the engine's subscriber path. `None` when
    // no owner identity is loaded.
    if let Some(p2) = p2_sync_handles.take() {
        spawn_dataset_sync_zenoh_adapter(
            &session,
            &app,
            &closing,
            p2.outhold,
            "dm-outhold-v1",
            "dm-outhold-sync-degraded",
            crate::dm_outhold::DM_OUTHOLD_DATASET_MAX_BYTES,
        )
        .await;
        spawn_dataset_sync_zenoh_adapter(
            &session,
            &app,
            &closing,
            p2.fleet_net,
            "fleet-net-v1",
            "fleet-net-sync-degraded",
            crate::fleet_net::FLEET_NET_DATASET_MAX_BYTES,
        )
        .await;
    }

    // ── ZEB-668 S1: owner-trust fleet-sync Zenoh adapter ─────────────────
    // Same plumbing as the datasets above. Replicates the harmony-owner
    // trust CRDT (enrollments / vouching / revocations / liveness) across
    // the owner's own fleet so a revocation issued on one device reaches
    // its siblings. `None` when no owner identity is loaded.
    if let Some(trust) = trust_sync_handles.take() {
        spawn_dataset_sync_zenoh_adapter(
            &session,
            &app,
            &closing,
            trust,
            crate::owner_trust_sync::OWNER_TRUST_DATASET,
            "owner-trust-sync-degraded",
            crate::owner_trust_sync::OWNER_TRUST_DATASET_MAX_BYTES,
        )
        .await;
    }

    // ── ZEB-677 S3: owner-quorum-req fleet-sync Zenoh adapter ────────────
    // Same plumbing as the datasets above. Replicates pending quorum
    // co-sign requests (revocation ceremony) across the owner's own fleet
    // so a sibling can approve and the initiator can assemble the cert.
    // `None` when no owner identity is loaded.
    if let Some(quorum) = quorum_sync_handles.take() {
        spawn_dataset_sync_zenoh_adapter(
            &session,
            &app,
            &closing,
            quorum,
            crate::owner_quorum_sync::OWNER_QUORUM_DATASET,
            "owner-quorum-sync-degraded",
            crate::owner_quorum_sync::OWNER_QUORUM_DATASET_MAX_BYTES,
        )
        .await;
    }

    // ── ZEB-668 S5: fleet-keys carrier Zenoh adapter ─────────────────────
    // Distributes the master-signed epoch doc (sealed per-device material)
    // across the owner's fleet. Keyed by the pinned epoch-0 KeyTree inside
    // the engine; this adapter is plumbing only.
    if let Some(fkeys) = fleet_keys_sync_handles.take() {
        spawn_dataset_sync_zenoh_adapter(
            &session,
            &app,
            &closing,
            fkeys,
            crate::fleet_key_epoch::FLEET_KEYS_DATASET,
            "fleet-keys-sync-degraded",
            crate::fleet_key_epoch::FLEET_KEYS_DATASET_MAX_BYTES,
        )
        .await;
    }

    // ── ZEB-458 P4 Phase B: relay-hold + relay-optin fleet-sync adapters ─
    // Same plumbing as the P2 datasets above — `relay-hold-v1` replicates the
    // relay's held opaque blobs across the relay's own fleet (D38);
    // `relay-optin-v1` replicates the per-community opt-in across the
    // volunteer's fleet (D43). Both are keyed off the OWNER address hex
    // (`handles.addr_hex`, set to `owner_addr_hex` at the start_node call site)
    // and the per-dataset lookup tag, forming
    // `harmony/owner/{addr_hex}/ds/{dataset}`. `None` when no owner identity is
    // loaded.
    if let Some(relay) = relay_sync_handles.take() {
        spawn_dataset_sync_zenoh_adapter(
            &session,
            &app,
            &closing,
            relay.hold,
            "relay-hold-v1",
            "relay-hold-sync-degraded",
            crate::community_relay::RELAY_HOLD_DATASET_MAX_BYTES,
        )
        .await;
        spawn_dataset_sync_zenoh_adapter(
            &session,
            &app,
            &closing,
            relay.optin,
            "relay-optin-v1",
            "relay-optin-sync-degraded",
            crate::community_relay::RELAY_OPTIN_DATASET_MAX_BYTES,
        )
        .await;
    }

    // ── ZEB-495 (ZEB-340 Part 2): community-device-intro fleet-sync adapter ─
    // Same plumbing as the P2 / relay datasets above — replicates deposited
    // `DeviceAnnounce` intros across the owner's fleet so an enrolled sibling
    // can relay a second device's self-introduction into the community engine
    // (Option A). Keyed off the OWNER address hex, forming
    // `harmony/owner/{addr_hex}/ds/community-device-intro-v1`. `None` when no
    // owner identity is loaded.
    if let Some(handles) = community_device_intro_sync_handles.take() {
        spawn_dataset_sync_zenoh_adapter(
            &session,
            &app,
            &closing,
            handles,
            "community-device-intro-v1",
            "community-device-intro-sync-degraded",
            crate::community_device_intro_ingest::COMMUNITY_DEVICE_INTRO_DATASET_MAX_BYTES,
        )
        .await;
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
            req.root_serve_tx,
            req.fetch_request_rx,
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
                                                    crate::node_event_sink::emit_ser(
                                                        app_for_task.as_ref(),
                                                        "library-directory-updated",
                                                        &serde_json::json!({
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
                            crate::node_event_sink::emit_ser(
                                app_for_libdir.as_ref(),
                                "library-directory-updated",
                                &serde_json::json!({ "communityId": null }),
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
                                            crate::node_event_sink::emit_ser(
                                                app_for_announce.as_ref(),
                                                "library-directory-updated",
                                                &serde_json::json!({ "communityId": null }),
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
                                                .on_sample(
                                                    subscription_id,
                                                    broadcast,
                                                    crate::iroh_friend_acceptor::wall_now_secs(),
                                                )
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
                                                        crate::node_event_sink::emit_ser(
                                                            app_for_task.as_ref(),
                                                            "profile-broadcast-received",
                                                            &serde_json::json!({
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
                                // ZEB-884: query-on-subscribe fast path. A plain
                                // declare_subscriber only receives FUTURE PUTs, so a
                                // late joiner would wait up to the 600s publisher
                                // refresh for a name. Fire a one-shot GET against the
                                // peer's publisher-side queryable to fetch the current
                                // card. Run it in a DETACHED task so it NEVER blocks the
                                // live recv loop below (or a shutdown) for up to the GET
                                // budget — the recv loop starts immediately and both
                                // paths feed the same cache (newer-HLC-wins). Local-
                                // drain into an owned Vec ONLY — never forward a Reply
                                // into a bounded channel (ZEB-803/812 reply-drain
                                // wedge). A peer with no queryable / no card yields
                                // nothing; we fall through to live PUTs.
                                {
                                    let session_q = Arc::clone(&session);
                                    let key_q = key_expr.clone();
                                    let cache_q = Arc::clone(&cache_for_task);
                                    let app_q = app_for_task.clone();
                                    let closing_q = Arc::clone(&closing_task);
                                    tokio::spawn(async move {
                                        if closing_q.load(Ordering::SeqCst) {
                                            return;
                                        }
                                        let replies = match session_q.get(&key_q).await {
                                            Ok(r) => r,
                                            Err(e) => {
                                                tracing::debug!(
                                                    error = %e,
                                                    subscription_id,
                                                    "profile card query-on-subscribe get failed; relying on PUTs"
                                                );
                                                return;
                                            }
                                        };
                                        let fetched = tokio::time::timeout(
                                            std::time::Duration::from_secs(8),
                                            async {
                                                while let Ok(reply) = replies.recv_async().await {
                                                    match reply.result() {
                                                        Ok(sample) => {
                                                            let view = sample.payload().to_bytes();
                                                            if view.len()
                                                                <= crate::profile_card_broadcast::MAX_CARD_WIRE_BYTES
                                                            {
                                                                return Some(view.to_vec());
                                                            }
                                                            tracing::warn!(
                                                                size = view.len(),
                                                                max = crate::profile_card_broadcast::MAX_CARD_WIRE_BYTES,
                                                                subscription_id,
                                                                "oversized profile card query reply dropped"
                                                            );
                                                        }
                                                        Err(err) => {
                                                            let msg = String::from_utf8_lossy(
                                                                &err.payload().to_bytes(),
                                                            )
                                                            .into_owned();
                                                            tracing::debug!(
                                                                subscription_id,
                                                                err = %msg,
                                                                "profile card query reply error"
                                                            );
                                                        }
                                                    }
                                                }
                                                None
                                            },
                                        )
                                        .await
                                        .ok()
                                        .flatten();
                                        if let Some(bytes) = fetched {
                                            ingest_card_bytes(
                                                &bytes,
                                                subscription_id,
                                                owner_id,
                                                cache_q.as_ref(),
                                                app_q.as_ref(),
                                            )
                                            .await;
                                        }
                                    });
                                }
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
                                            // ZEB-884: one shared pipeline for the
                                            // live-PUT arm and the query-on-subscribe
                                            // fast path (decode/verify/attribute/
                                            // cache/emit).
                                            ingest_card_bytes(
                                                &bytes,
                                                subscription_id,
                                                owner_id,
                                                cache_for_task.as_ref(),
                                                app_for_task.as_ref(),
                                            )
                                            .await;
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

    // ── ZEB-884: self-card queryable ─────────────────────────────────
    // Answer a peer's query-on-subscribe GET on OUR card topic with our cached
    // signed card bytes, so a late subscriber resolves our name in <1s instead of
    // waiting up to the 600s publisher refresh. Gated on our own owner identity
    // (self_owner from dm_outbox) + the card publisher (its cached `latest`).
    // Fire-and-forget, mirroring the owner-state root queryable: it self-
    // terminates on the `closing` flag / session close. `query.reply` back-
    // pressures only this responder — never an engine channel — so it is not
    // subject to the reply-drain wedge.
    if let (Some(dm_outbox_qbl), Some(card_publisher_qbl)) =
        (dm_outbox.as_ref(), profile_card_publisher.as_ref())
    {
        let self_owner = { dm_outbox_qbl.lock().await.self_owner };
        let key_expr = crate::profile_card_broadcast::card_topic_for(&self_owner.0);
        let latest = card_publisher_qbl.latest_handle();
        let session_for_qbl = Arc::clone(&session_arc);
        let closing_for_qbl = Arc::clone(&closing);
        tokio::spawn(async move {
            let qbl = match session_for_qbl.declare_queryable(&key_expr).await {
                Ok(q) => q,
                Err(e) => {
                    if !closing_for_qbl.load(Ordering::SeqCst) {
                        tracing::warn!(error = %e, "failed to declare self-card queryable");
                    }
                    return;
                }
            };
            loop {
                tokio::select! {
                    biased;
                    res = qbl.recv_async() => {
                        let Ok(query) = res else { break; };
                        // Snapshot the cached signed card (no re-sign). `None` =
                        // we have not published a card yet -> answer nothing, so
                        // the querying peer falls through to live PUTs.
                        let snapshot = latest.lock().await.clone();
                        if let Some((_topic, bytes)) = snapshot {
                            if let Err(e) = query.reply(query.key_expr(), bytes).await {
                                tracing::warn!(error = %e, "self-card queryable reply failed");
                            }
                        }
                    }
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {
                        if closing_for_qbl.load(Ordering::SeqCst) {
                            break;
                        }
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
    let voice_signal_sub_handle: Option<tokio::task::JoinHandle<()>> = if let (
        Some(dm_outbox),
        Some(crdt_state),
    ) =
        (dm_outbox.as_ref(), crdt_state.as_ref())
    {
        // Snapshot self owner + derive our X25519 private once, under the
        // outbox lock, before spawning the long-lived subscriber.
        let (self_owner_hex, self_owner, self_x25519_priv) = {
            let g = dm_outbox.lock().await;
            let hex = hex::encode(g.self_owner.0);
            let x_priv = crate::dm_signing::ed25519_priv_to_x25519(&g.community_signing_key);
            (hex, g.self_owner, x_priv)
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
                                    if let Some(sp) = signal.space_id {
                                        // ZEB-360 group path: the signal names its
                                        // space directly — no 2-member scan. Verify the
                                        // space exists, is a GroupDm, the caller is a
                                        // member, and it carries a content_key. Then
                                        // route by kind. For a Decline, `signal.caller`
                                        // is the decliner (responder), so requiring the
                                        // caller to be a group member is correct.
                                        let ok = {
                                            let g = crdt_for_signal.lock().await;
                                            g.spaces.get(&sp).is_some_and(|s| {
                                                s.kind
                                                    == crate::owner_state_types::SpaceKind::GroupDm
                                                    && s.content_key.is_some()
                                                    && s.members.contains(&signal.caller)
                                                    // The LOCAL owner must also still be a
                                                    // current member — a device holding the
                                                    // space in CRDT but no longer in `members`
                                                    // would otherwise surface a ring it can't
                                                    // join (join_group_call /
                                                    // resolve_group_call_members reject it).
                                                    && s.members.contains(&self_owner)
                                            })
                                        };
                                        if ok {
                                            emit_group_voice_signal_event(
                                                &app_for_signal,
                                                &signal,
                                                &hex::encode(sp.0),
                                            );
                                        } else {
                                            tracing::debug!(
                                                    "group voice signal dropped: space invalid / caller not a member"
                                                );
                                        }
                                    } else if matches!(
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
            serve_allowlist.clone(),
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
        dispatch_action(action, &session, &zenoh_tx, &app, &closing, &own_zid).await;
    }

    // Subscribe to community channel messages for real-time messaging.
    dispatch_action(
        RuntimeAction::Subscribe {
            key_expr: "harmony/community/*/channels/*".to_string(),
        },
        &session,
        &zenoh_tx,
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
        &app,
        &closing,
        &own_zid,
    )
    .await;

    // Subscribe to vine reactions (likes/unlikes). Exactly `*/*` — the
    // canonical key is `…/reactions/{vine_id}/{reactor}`, and `**` would
    // deliver arbitrarily deep non-canonical keys to the verify path
    // (wasted signature work; Qodo PR #446 round 1).
    dispatch_action(
        RuntimeAction::Subscribe {
            key_expr: "harmony/vines/*/reactions/*/*".to_string(),
        },
        &session,
        &zenoh_tx,
        &app,
        &closing,
        &own_zid,
    )
    .await;

    // ZEB-670: subscribe to vine tombstones (creator-signed deletes).
    // Own key space because the descriptor subscription `harmony/vines/*`
    // is single-segment and cannot match the deeper tombstone keys.
    dispatch_action(
        RuntimeAction::Subscribe {
            key_expr: "harmony/vines/*/tombstones/*".to_string(),
        },
        &session,
        &zenoh_tx,
        &app,
        &closing,
        &own_zid,
    )
    .await;

    // ZEB-671: subscribe to published follow lists (Discover graph
    // edges). Exactly one owner segment — deeper keys are non-canonical
    // and the ingest path rejects shape mismatches anyway.
    dispatch_action(
        RuntimeAction::Subscribe {
            key_expr: "harmony/vines/*/follows".to_string(),
        },
        &session,
        &zenoh_tx,
        &app,
        &closing,
        &own_zid,
    )
    .await;

    // ZEB-678 S2: subscribe to per-feed authority records (owner-anchoring +
    // revocation). Own key space — `harmony/vines/*` is single-segment and
    // cannot match the deeper `{N}/authority` keys.
    dispatch_action(
        RuntimeAction::Subscribe {
            key_expr: "harmony/vines/*/authority".to_string(),
        },
        &session,
        &zenoh_tx,
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
            key_expr: format!("{}*", crate::ANNOUNCE_PREFIX),
        },
        &session,
        &zenoh_tx,
        &app,
        &closing,
        &own_zid,
    )
    .await;

    // ZEB-669 S2: storage-buddy record topics (signed pledge lists,
    // backup sets, hosting reports). One subscription per record kind —
    // `harmony/storage/*/*` would also match future non-record topics.
    for kind in ["pledges", "backup-set", "hosting"] {
        dispatch_action(
            RuntimeAction::Subscribe {
                key_expr: format!("{}*/{kind}", crate::STORAGE_RECORD_PREFIX),
            },
            &session,
            &zenoh_tx,
            &app,
            &closing,
            &own_zid,
        )
        .await;
    }

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
            &app,
            &closing,
            &own_zid,
        )
        .await;
    }

    // Signal the caller that startup fully succeeded — UDP bound, Zenoh
    // session open, all queryables and subscribers declared.
    let _ = ready_tx.send(Ok(()));

    // ZEB-702 T3 (Component B): spawn the transport-epoch republish listener.
    // The SENDER (`transport_epoch_tx`) lives for run()'s whole lifetime, so
    // this subscriber only exits when the event loop does (sender drop → the
    // loop's `changed()` returns Err). Subscribing HERE — before the select
    // loop that bumps the epoch (~5s peer-refresh arm) — guarantees no up-edge
    // is missed. Skipped when there are no engines (test callers / no owner
    // identity) so we don't spawn an idle task.
    if !republish_on_epoch.is_empty() {
        tokio::spawn(run_epoch_republish(
            transport_epoch_tx.subscribe(),
            republish_on_epoch,
        ));
    }

    // Phase 2: cold-start root query. Pulls current root via Zenoh `get` in
    // case the gateway last published before this client subscribed. ZEB-434
    // D9: latch-driven retries (30s base doubling to a 600s cap) replace the
    // old one-shot, which raced link establishment — a slow-linking boot no
    // longer misses until the gateway's next publish. The empty-vs-none
    // discrimination maps to Answered-vs-NoReply: an empty payload is the
    // gateway's valid "no mail yet" sentinel (satisfies the latch), while
    // zero responders / query failure back off and retry. Once satisfied the
    // driver parks and re-arms on transport-epoch bumps, so a recovered link
    // re-queries (with the EPOCH_REARM_COOLDOWN_MS cooldown).
    if let Some(ref sync) = mail_sync {
        if !own_root_key.is_empty() {
            let session_mail = session.clone();
            let key_mail = own_root_key.clone();
            let sync_mail = Arc::clone(sync);
            // event_loop holds the watch SENDER (the 5s peer-refresh arm
            // does the bumping); internal consumers derive receivers.
            let epoch_rx_mail = Some(transport_epoch_tx.subscribe());
            // Shutdown bridge: flip the watch when the loop's closing flag
            // flips (1s poll — mirrors the adapter tasks' closing-poll
            // discipline).
            let (mail_shutdown_tx, mail_shutdown_rx) = tokio::sync::watch::channel(false);
            {
                let closing_mail = Arc::clone(&closing);
                tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        if closing_mail.load(Ordering::SeqCst) {
                            let _ = mail_shutdown_tx.send(true);
                            return;
                        }
                    }
                });
            }
            // ZEB-443: the retry driver re-invokes this closure with backoff
            // (30s base, doubling to a 600s cap) for as long as it is running.
            //
            // ZEB-805: this comment used to say "forever". It is not forever,
            // and the difference matters to whoever reads a log that stops. The
            // driver exits on `shutdown_rx`, on its epoch/shutdown watch SENDER
            // dropping, and on the adapter being permanently gone — see
            // `run_root_fetch_driver`'s contract in channel_backfill.rs. During
            // the ZEB-805 incident this driver made 11 attempts and then stopped
            // while still unsatisfied, ~10s after zenoh logged "sending on a
            // closed channel"; that is consistent with one of those exits, but
            // the live state was destroyed by the restart, so WHICH is not
            // established. Do not read "retrying with backoff" as a guarantee
            // that retries are still happening.
            //
            // UI error spam is prevented inside
            // `report_query_error` itself — identical-message reports
            // while already in `Error` are suppressed, and ANY
            // transition away from `Error` (startup reply, successful
            // manual refresh) re-arms reporting. Deduping here with a
            // closure-side latch instead would desync from refresh_now:
            // a successful manual refresh would clear the UI to idle
            // while the latch stayed set, silencing an ongoing outage.
            let request_root = move || {
                let session = session_mail.clone();
                let key = key_mail.clone();
                let sync = Arc::clone(&sync_mail);
                async move {
                    let result = query_mail_root(&session, &key, "startup").await;
                    match &result {
                        Ok(Some(payload)) => {
                            // Empty payload routes through the same handler:
                            // it clears any prior error to idle ("no mail
                            // yet"). Re-entry on a later epoch-bump re-query
                            // is safe — start_or_queue_walk single-flights
                            // and dedups repeated roots.
                            Arc::clone(&sync)
                                .handle_startup_query_reply(Some(payload.as_slice()))
                                .await;
                        }
                        Ok(None) => {
                            // ZEB-805: this line used to append "live push also
                            // catches up on next gateway publish". Both that
                            // clause and the retry promise were false during the
                            // incident — the retries stopped and live push did
                            // not catch up — and an operator who read it stopped
                            // investigating. State only what just happened; a
                            // log line must not vouch for a fallback it cannot
                            // observe.
                            tracing::info!("startup root query: no responder");
                            sync.report_query_error(
                                "no gateway responded to startup query".to_string(),
                            );
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "startup root query failed; retrying with backoff");
                            sync.report_query_error(format!("startup query failed: {e}"));
                        }
                    }
                    map_mail_root_outcome(&result)
                }
            };
            // ZEB-618: use the caller-built persist pair when present. Its
            // `interval_ms` is a SINGLE jittered draw shared with the persisted
            // first-deadline (see `mail_resync` param) — so the deadline and the
            // driver arg can never diverge. The `None` (test / no-mail-dir) path
            // keeps the legacy ZEB-425 floor with a fresh per-spawn jitter draw.
            let (mail_interval_ms, mail_persist) = match &mail_resync {
                Some((ms, p)) => (Some(*ms), Some(p.clone())),
                None => (
                    Some(crate::channel_backfill::periodic_resync_interval_ms()),
                    None,
                ),
            };
            tokio::spawn(crate::channel_backfill::run_root_fetch_driver(
                crate::channel_backfill::RootFetchLatch::new(),
                request_root,
                mail_shutdown_rx,
                epoch_rx_mail,
                // ZEB-618: presence kick — same watch the channel-log drivers
                // get. `subscribe()` here is legal because this spawn precedes
                // the sender's move into the presence task (~:3030).
                Some(presence_resync_tx.subscribe()),
                // ZEB-425 anti-entropy floor: re-arm the mail-root fetch
                // ~hourly even with no epoch bump (router-only gateways / late
                // queryables / same-zid reconnects). ZEB-618: restart-aware
                // when the persist pair is wired.
                mail_interval_ms,
                || {
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0)
                },
                mail_persist,
            ));
        }
    }

    // ── Timer (250ms = 4 ticks/sec, same as harmony-node) ────────────
    let mut timer = tokio::time::interval(Duration::from_millis(250));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let start = std::time::Instant::now();

    // Directly connected Zenoh peers — refreshed every 20 timer ticks (~5s).
    // Used to derive hop distance: ZID in this set → hop 1, else → hop 2.
    // Eagerly populated so capacity updates arriving before the first refresh
    // aren't misclassified as hop 2.
    let mut direct_peer_zids: std::collections::HashSet<String> = direct_link_zids(&session).await;
    // ZEB-622: previous zid-poll snapshot, kept SEPARATE from the overwrite-
    // style `direct_peer_zids` above (whose hop-distance consumers need the
    // current-snapshot semantics). SEEDED from the same boot-time snapshot
    // WITHOUT bumping the transport epoch: boot-time peers are not "recovered"
    // — the per-community spawn-time latch query already covers them, and a
    // bump here would just burn the fetch drivers' first cooldown window for no
    // benefit. `detect_up_edges` REPLACES this each poll, so a same-zid
    // reconnect after a drop re-fires (unlike the old accumulating seen-set).
    let mut transport_prev_zids: std::collections::HashSet<String> = direct_peer_zids.clone();
    let mut peer_refresh_counter: u64 = 0;

    // ZEB-418 P2 Task 7 (D16): periodic routing-record re-publish, counted
    // in 250ms timer ticks (same pattern as `peer_refresh_counter`).
    // BUTLER_SET_REFRESH_MS / 250 = one fire per ~half freshness window;
    // the multiple-of pattern means the FIRST fire lands a full interval
    // after boot — correct, since start_node's publisher registrations
    // already published a fresh blob.
    const ROUTING_REPUBLISH_TICKS: u64 = crate::butler_deposit::BUTLER_SET_REFRESH_MS / 250;
    let mut routing_republish_counter: u64 = 0;

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

    // ZEB-612 Town Hall: (community, channel) → the shared raised-hand cell
    // handed to that channel's presence publisher. 0 = lowered; otherwise the
    // wall-clock ms of the FIRST raise (stable queue position — see
    // `update_hand_cell`). `set_voice_hand` → `VoiceChannelRequest::SetHand`
    // updates it; the publisher republishes it each heartbeat. Kept in
    // lockstep with `voice_mute_flags` on Join/Leave.
    let mut voice_hand_flags: std::collections::HashMap<
        (
            crate::owner_state_types::SpaceId,
            crate::community_membership::ChannelId,
        ),
        std::sync::Arc<std::sync::atomic::AtomicU64>,
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

    // ── ZEB-360: group-DM voice presence bookkeeping ───────────────────────
    // Group presence is space-scoped (topic harmony/voice-presence/group-dm/
    // {spaceIdHex}, sealed under derive_groupdm_presence_key). The ROSTER lives
    // in its OWN `groupdm_presence_map` (declared below), keyed by
    // (SpaceId(space_id), ChannelId(call_id)); a dedicated map (rather than the
    // shared community `voice_presence_map`) lets the eviction sweep TTL-evict
    // crashed group participants without disturbing community sweep semantics.
    //
    // One read-subscriber per watched space (WatchGroupCall; banner + in-call
    // roster both consume it). Keyed by raw 16-byte space_id.
    let mut groupdm_presence_subs: std::collections::HashMap<
        [u8; 16],
        tokio::task::JoinHandle<()>,
    > = std::collections::HashMap::new();
    // One beacon publisher per active call we're in, keyed by (space_id, call_id).
    let mut groupdm_presence_pubs: std::collections::HashMap<
        ([u8; 16], [u8; 16]),
        tokio::task::JoinHandle<()>,
    > = std::collections::HashMap::new();
    // The shared mute flag handed to each call's publisher; SetGroupCallMuted
    // flips it (next beacon reflects it). Keyed by (space_id, call_id).
    let mut groupdm_presence_mute: std::collections::HashMap<
        ([u8; 16], [u8; 16]),
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    > = std::collections::HashMap::new();
    // Caps needed to mint+sign the `left` tombstone at StopGroupPresence time
    // (topic + presence_key + signing identity), stashed alongside the publisher.
    // Keyed by (space_id, call_id).
    #[allow(clippy::type_complexity)]
    let mut groupdm_presence_caps: std::collections::HashMap<
        ([u8; 16], [u8; 16]),
        (
            String,                                                   // topic
            std::sync::Arc<crate::community_channel_log::ChannelKey>, // presence_key
            std::sync::Arc<ed25519_dalek::SigningKey>,                // signing_key
            crate::owner_state_types::OwnerAddr,                      // self_owner
            [u8; 32],                                                 // self_device
            crate::owner_state_types::Hlc,                            // joined_hlc
        ),
    > = std::collections::HashMap::new();
    // ZEB-360 crash-eviction parity: group presence gets its OWN roster map (not
    // the shared community `voice_presence_map`). The eviction sweep below sweeps
    // BOTH maps with the same TTL, so a crashed group-call participant who never
    // published a `left` tombstone TTL-evicts as a ghost row, exactly like
    // community voice. Keyed by (SpaceId(space_id), ChannelId(call_id)).
    let groupdm_presence_map = std::sync::Arc::new(tokio::sync::Mutex::new(
        crate::voice_presence::VoicePresenceMap::new(),
    ));

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

    // ── ZEB-612 S3: content re-announce + holder sweep ────────────────
    // No upstream re-announcement exists (announces fire only on
    // store/publish — harmony-content storage_tier), so a staleness-pruned
    // holder map would decay to empty. This node refreshes its own
    // announceable content every REANNOUNCE_INTERVAL_MS; receivers sweep
    // holders at 3× (HOLDER_STALE_MS). O(library) tiny publishes per
    // interval — acceptable at current scale; real hosting accounting is
    // ZEB-669.
    let mut reannounce_tick = tokio::time::interval(Duration::from_millis(
        crate::observed_holders::REANNOUNCE_INTERVAL_MS,
    ));
    reannounce_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // ZEB-669 S2: auto-pin engine tick + fetch-completion channel.
    let mut buddy_sync_tick = tokio::time::interval(Duration::from_millis(BUDDY_SYNC_INTERVAL_MS));
    buddy_sync_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let (buddy_fetch_tx, mut buddy_fetch_rx) = mpsc::channel::<BuddyFetchResult>(16);
    let mut buddy_engine = BuddyEngineState::default();

    // ── ZEB-358 voice moderation ──────────────────────────────────
    // Receiver-side enforcement state shared with the per-channel control
    // subscriber tasks (apply on receipt) and the loop (issue + sweep).
    let voice_moderation_map = std::sync::Arc::new(tokio::sync::Mutex::new(
        crate::voice_moderation::ActiveModeration::default(),
    ));
    let mut voice_control_subs: std::collections::HashMap<
        (
            crate::owner_state_types::SpaceId,
            crate::community_membership::ChannelId,
        ),
        tokio::task::JoinHandle<()>,
    > = std::collections::HashMap::new();
    // ZEB-358 (Cursor HIGH): per-channel "this node is kicked" flag, SHARED with
    // that channel's presence publisher (gates beacon publishing) and its control
    // sub (sets it from the moderation map after each apply). Kept in lockstep
    // with `voice_control_subs` on Join/Leave.
    let mut voice_self_kicked_flags: std::collections::HashMap<
        (
            crate::owner_state_types::SpaceId,
            crate::community_membership::ChannelId,
        ),
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    > = std::collections::HashMap::new();
    // ZEB-358 (Qodo perf): true iff ANY moderation is currently enforced in ANY
    // channel. The audio media subscriber reads this to skip the per-frame
    // presence+moderation `Mutex` locks when no moderation is active (the common
    // case). Updated after every apply (control sub + Moderate) and every sweep.
    let voice_moderation_active = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Directives THIS node issued and is re-asserting until `stop_after_ms`.
    // Key: (community, channel, target_owner, class). Main-loop owned. The
    // ModClass key keeps one live directive per enforcement class (ZEB-612:
    // mute / kick / invite) so an invite never displaces a kick re-assert.
    #[allow(clippy::type_complexity)]
    let mut voice_issuer_directives: std::collections::HashMap<
        (
            crate::owner_state_types::SpaceId,
            crate::community_membership::ChannelId,
            [u8; 16],
            crate::voice_moderation::ModClass,
        ),
        (crate::voice_moderation::SignedVoiceModerationDirective, u64),
    > = std::collections::HashMap::new();
    // Per-channel monotone seq for moderation directive LWW tiebreak.
    let mut voice_moderation_seq: std::collections::HashMap<
        (
            crate::owner_state_types::SpaceId,
            crate::community_membership::ChannelId,
        ),
        u64,
    > = std::collections::HashMap::new();

    // ZEB-815: per-community address-book resync handles. Created HERE, above
    // both pools, because the producer (presence subscriber) and the consumer
    // (addrbook snapshot requester) are spawned by two independent tasks
    // draining two independent channels — either can reach a given community
    // first. The hub hands both sides the same `Notify` whichever order they
    // arrive in. Unconditional (it is a map behind an Arc): when the addrbook
    // pool is unwired, a presence-side fire simply has no consumer.
    let addrbook_resync_hub = crate::address_book_sync::AddrbookResyncHub::new();

    // ── ZEB-537: community-presence pool + global sweeper ─────────────
    // One (publisher, subscriber) task pair per subscribed community, keyed off
    // CommunityPresenceRequest::{Subscribe, Unsubscribe} from NodeState (mirrors
    // the ZEB-341 profile-card pool above). The publisher heartbeats our own
    // device-#2-signed beacon; the subscriber opens/verifies peers' beacons into
    // the shared roster map. A single global sweeper task TTL-evicts stale
    // entries across every community and re-emits the affected rosters.
    //
    // Gated on the community registry + an owner identity (dm_outbox) being
    // present — both required to derive presence keys and to mint our own
    // beacons. When either is absent (test callers bypassing `start_node`) the
    // pool stays unwired, exactly like the voice/profile-card pools.
    if let (Some(mut request_rx), Some(registry), Some(dm_outbox)) = (
        community_presence_request_rx,
        community_registry.clone(),
        dm_outbox.as_ref(),
    ) {
        // Self identity for our OWN presence beacons. `self_device` MUST be the
        // enrolled device key #2 (== community_signing_key.verifying_key()) or
        // `beacon_signer_is_member` rejects our own beacons; `signing_key` signs
        // with that same key. (See lib.rs:5138.) Read once under the outbox
        // (tokio) lock before spawning the long-lived task.
        let (self_owner, signing_key, self_device) = {
            let g = dm_outbox.lock().await;
            let sk = std::sync::Arc::clone(&g.community_signing_key);
            let dev = sk.verifying_key().to_bytes();
            (g.self_owner, sk, dev)
        };

        // ── presence-request task: owns the rx + the per-community handles ──
        {
            let session_for_presence = Arc::clone(&session_arc);
            let registry_for_presence = Arc::clone(&registry);
            let app_for_presence = app.clone();
            let closing_for_presence = Arc::clone(&closing);
            let map_for_presence = std::sync::Arc::clone(&community_presence_map);
            let now_ms_for_presence = std::sync::Arc::clone(&voice_now_ms);
            // ZEB-599 Direction 1: presence-driven full-reconcile sender —
            // moved into the presence task, cloned per subscriber below.
            let presence_resync_tx_for_presence = presence_resync_tx;
            // ZEB-620 Task 5: reconnect-supervisor handle threaded the same way, so
            // a roster device-set change also kicks a presence sweep (re-arm all
            // non-connected peers). `None` on iroh-disabled runs.
            let supervisor_for_presence = reconnect_supervisor.clone();
            // ZEB-815: the per-community handle each subscriber fires on a
            // roster device-set change, waking that community's snapshot
            // requester (spec §2: snapshot on presence roster change).
            let addrbook_hub_for_presence = addrbook_resync_hub.clone();
            // ZEB-919: owner-state handle so presence seals/opens under the
            // LIVE epoch key (None only pre-owner-load, which cannot reach a
            // Subscribe — degraded spawn-key mode is for tests).
            let crdt_state_for_presence = crdt_state.clone();
            tokio::spawn(async move {
                use std::collections::HashMap;
                let mut handles: HashMap<
                    [u8; 16],
                    (tokio::task::JoinHandle<()>, tokio::task::JoinHandle<()>),
                > = HashMap::new();
                // ZEB-537: monotonic per-subscribe session counter. This task
                // processes Subscribes sequentially, so a strictly-increasing
                // `logical` makes each new subscribe's `started_hlc` strictly
                // newer than the previous one — even within the same `wall_ms`.
                // Without it, a rapid unsubscribe→resubscribe inside one ms
                // produces an identical `(started_hlc, seq=0)` prefix and peers
                // reject the new session's beacons as stale until TTL.
                let mut session_logical: u32 = 0;
                // ZEB-600: node-global presence-visibility handle, pulled once
                // from the shared map. Each publisher gates its beacon on this
                // Arc, so a live set_presence_visibility flip suppresses beacons
                // on the next tick without re-spawning the pool.
                let presence_visible = map_for_presence.lock().await.visible_handle();
                while let Some(req) = request_rx.recv().await {
                    match req {
                        CommunityPresenceRequest::Subscribe { community_id } => {
                            // Self-heal: a pair is healthy only if BOTH tasks are
                            // alive. If either has exited, abort the survivor and
                            // drop the entry so the Subscribe below can restart
                            // both cleanly.
                            handles.retain(|_, (p, s)| {
                                let alive = !p.is_finished() && !s.is_finished();
                                if !alive {
                                    p.abort();
                                    s.abort();
                                }
                                alive
                            });
                            if handles.contains_key(&community_id) {
                                tracing::warn!(
                                    community_id = %hex::encode(community_id),
                                    "CommunityPresenceRequest::Subscribe duplicate — ignoring"
                                );
                                continue;
                            }
                            let topic =
                                format!("harmony/presence/{}/beacons", hex::encode(community_id));
                            let community = crate::owner_state_types::SpaceId(community_id);
                            // Fresh session HLC (seq restarts at 0 on every
                            // (re)subscribe; the map's freshness rules treat a
                            // strictly-newer started_hlc as a new session).
                            // Bump `logical` per subscribe so the HLC is
                            // strictly-increasing even when `wall_ms` is
                            // unchanged across a rapid resubscribe.
                            session_logical = session_logical.wrapping_add(1);
                            let started_hlc = crate::owner_state_types::Hlc {
                                wall_ms: crate::iroh_friend_acceptor::wall_now_ms(),
                                logical: session_logical,
                                device_id: hex::encode(self_device),
                            };
                            let seq_counter =
                                std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
                            let pub_handle =
                                crate::community_presence::spawn_community_presence_publisher(
                                    (*session_for_presence).clone(),
                                    topic.clone(),
                                    Arc::clone(&registry_for_presence),
                                    community,
                                    std::sync::Arc::clone(&signing_key),
                                    self_owner,
                                    self_device,
                                    started_hlc,
                                    seq_counter,
                                    Duration::from_millis(
                                        crate::community_presence::BEACON_INTERVAL_MS,
                                    ),
                                    Arc::clone(&presence_visible),
                                    Arc::clone(&closing_for_presence),
                                    // ZEB-919: live epoch-key seal.
                                    crdt_state_for_presence.clone(),
                                );
                            let sub_handle =
                                crate::community_presence::spawn_community_presence_subscriber(
                                    (*session_for_presence).clone(),
                                    topic,
                                    Arc::clone(&registry_for_presence),
                                    community,
                                    std::sync::Arc::clone(&map_for_presence),
                                    app_for_presence.clone(),
                                    Arc::clone(&closing_for_presence),
                                    std::sync::Arc::clone(&now_ms_for_presence),
                                    // ZEB-599 Direction 1: kick channel-log
                                    // backfill drivers into a full reconcile
                                    // when this community's roster gains a
                                    // device (a new potential holder).
                                    presence_resync_tx_for_presence.clone(),
                                    // ZEB-620 Task 5: same roster edge kicks the
                                    // reconnect supervisor into a presence sweep.
                                    supervisor_for_presence.clone(),
                                    // ZEB-815: and wakes this community's
                                    // address-book snapshot requester.
                                    Some(addrbook_hub_for_presence.handle(community_id)),
                                    // ZEB-919: live epoch-key open candidates.
                                    crdt_state_for_presence.clone(),
                                );
                            handles.insert(community_id, (pub_handle, sub_handle));
                            // Emit an INITIAL empty roster so the UI has a
                            // baseline immediately (peers populate it as their
                            // beacons arrive).
                            crate::node_event_sink::emit_ser(
                                app_for_presence.as_ref(),
                                "presence-updated",
                                &crate::community_presence::PresenceUpdatedPayload::new(
                                    community_id,
                                    &[],
                                ),
                            );
                        }
                        CommunityPresenceRequest::Unsubscribe { community_id } => {
                            if let Some((p, s)) = handles.remove(&community_id) {
                                p.abort();
                                s.abort();
                            }
                            map_for_presence
                                .lock()
                                .await
                                .remove_community(&crate::owner_state_types::SpaceId(community_id));
                            crate::node_event_sink::emit_ser(
                                app_for_presence.as_ref(),
                                "presence-updated",
                                &crate::community_presence::PresenceUpdatedPayload::new(
                                    community_id,
                                    &[],
                                ),
                            );
                        }
                    }
                }
                // Request channel closed: abort any still-running per-community
                // pub/sub pairs (they otherwise rely solely on `closing`, which
                // may not be set on this path).
                for (_cid, (p, s)) in handles.drain() {
                    p.abort();
                    s.abort();
                }
            });
        }

        // ── global TTL sweeper: evict stale devices + re-emit rosters ──
        {
            let app_for_sweep = app.clone();
            let closing_for_sweep = Arc::clone(&closing);
            let map_for_sweep = std::sync::Arc::clone(&community_presence_map);
            let now_ms_for_sweep = std::sync::Arc::clone(&voice_now_ms);
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_millis(
                    crate::community_presence::BEACON_INTERVAL_MS,
                ));
                loop {
                    tick.tick().await;
                    if closing_for_sweep.load(Ordering::SeqCst) {
                        break;
                    }
                    let evicted = {
                        let mut g = map_for_sweep.lock().await;
                        g.sweep((now_ms_for_sweep)(), crate::community_presence::STALE_MS)
                    };
                    if evicted.is_empty() {
                        continue;
                    }
                    // Distinct communities whose roster changed this sweep.
                    let mut communities: Vec<crate::owner_state_types::SpaceId> =
                        evicted.into_iter().map(|(c, _, _)| c).collect();
                    communities.sort();
                    communities.dedup();
                    for community in communities {
                        let members = {
                            let g = map_for_sweep.lock().await;
                            g.online_owners(&community)
                        };
                        crate::node_event_sink::emit_ser(
                            app_for_sweep.as_ref(),
                            "presence-updated",
                            &crate::community_presence::PresenceUpdatedPayload::new(
                                community.0,
                                &members,
                            ),
                        );
                    }
                }
            });
        }
    }

    // ── ZEB-815: community address-book task pool ─────────────────────
    // Four tasks per subscribed community (live subscriber, snapshot
    // queryable, snapshot requester, sidecar persist), keyed off
    // AddressBookRequest::{Subscribe, Unsubscribe} — the sibling requests the
    // presence IPC sites send. Mirrors the ZEB-537 presence pool above: one
    // task owns the rx plus the per-community handles, with the same
    // self-healing `retain` and the same abort-on-unsubscribe/abort-on-close
    // shutdown. Gated on the registry (the seal key derives from the community
    // membership key) exactly like presence.
    if let (Some(addrbook), Some(registry)) = (addrbook_runtime, community_registry.clone()) {
        let session_for_addrbook = Arc::clone(&session_arc);
        let hub_for_addrbook = addrbook_resync_hub.clone();
        // ZEB-919: owner-state handle so seal/open track the LIVE epoch key
        // (None only pre-owner-load, which cannot reach a Subscribe —
        // degraded spawn-key mode is for tests).
        let crdt_state_for_addrbook = crdt_state.clone();
        let AddressBookRuntime {
            mut request_rx,
            book,
            reachability_resolver,
            community_relay_resolver,
            identity_dir,
            dirty_hub,
            ingest_observer,
        } = addrbook;
        tokio::spawn(async move {
            use std::collections::HashMap;
            // Vec (not a tuple) because all four handles are interchangeable
            // to the pool: it only ever aborts them together.
            let mut handles: HashMap<[u8; 16], Vec<tokio::task::JoinHandle<()>>> = HashMap::new();
            while let Some(req) = request_rx.recv().await {
                match req {
                    AddressBookRequest::Subscribe { community_id } => {
                        // Self-heal: a group is healthy only if EVERY task is
                        // alive. If any exited (e.g. a failed zenoh declare),
                        // abort the survivors and drop the entry so this
                        // Subscribe restarts all four cleanly.
                        //
                        // This sweeps EVERY community, not just the one being
                        // subscribed, and only the subscribed one is respawned
                        // below — so a reaped group is a community that stops
                        // syncing until it is re-subscribed. Log which task
                        // died; without it the reap is invisible.
                        handles.retain(|cid, hs| {
                            debug_assert_eq!(
                                hs.len(),
                                ADDRBOOK_TASK_LABELS.len(),
                                "addrbook task labels must stay in step with the spawned group"
                            );
                            let finished: Vec<&str> = hs
                                .iter()
                                .enumerate()
                                .filter(|(_, h)| h.is_finished())
                                // `get` not `[i]`: a panic here would take down
                                // the whole pool, and a stale label is a strictly
                                // better failure than that.
                                .map(|(i, _)| ADDRBOOK_TASK_LABELS.get(i).copied().unwrap_or("?"))
                                .collect();
                            if finished.is_empty() {
                                return true;
                            }
                            tracing::warn!(
                                community_id = %hex::encode(cid),
                                finished = %finished.join(","),
                                "addrbook task group reaped: a task exited, so its \
                                 survivors are aborted — this community stops syncing \
                                 its address book until it is re-subscribed"
                            );
                            for h in hs.iter() {
                                h.abort();
                            }
                            false
                        });
                        if handles.contains_key(&community_id) {
                            tracing::warn!(
                                community_id = %hex::encode(community_id),
                                "AddressBookRequest::Subscribe duplicate — ignoring"
                            );
                            continue;
                        }
                        let community = crate::owner_state_types::SpaceId(community_id);
                        // `dirty`: ingest (subscriber + requester + the
                        // publisher closures' OWN rows) → persist.
                        // `resync`: presence roster change → requester. Both
                        // come from hubs whose other side may already hold the
                        // handle — the presence pool for `resync`, the
                        // publishers for `dirty` (they can publish a local row
                        // before this Subscribe is drained).
                        let dirty = dirty_hub.handle(community_id);
                        let resync = hub_for_addrbook.handle(community_id);
                        // Queryable FIRST, and awaited: a peer reacting to our
                        // arrival must not query us before we can serve.
                        let queryable_handle =
                            crate::address_book_sync::spawn_addrbook_snapshot_queryable(
                                (*session_for_addrbook).clone(),
                                Arc::clone(&registry),
                                std::sync::Arc::clone(&book),
                                community,
                                // ZEB-919: live epoch-key seal.
                                crdt_state_for_addrbook.clone(),
                            )
                            .await;
                        let subscriber_handle = crate::address_book_sync::spawn_addrbook_subscriber(
                            (*session_for_addrbook).clone(),
                            Arc::clone(&registry),
                            std::sync::Arc::clone(&book),
                            std::sync::Arc::clone(&reachability_resolver),
                            std::sync::Arc::clone(&community_relay_resolver),
                            community,
                            std::sync::Arc::clone(&dirty),
                            ingest_observer.clone(),
                            // ZEB-919: live epoch-key open candidates.
                            crdt_state_for_addrbook.clone(),
                        );
                        let requester_handle =
                            crate::address_book_sync::spawn_addrbook_snapshot_requester(
                                (*session_for_addrbook).clone(),
                                Arc::clone(&registry),
                                std::sync::Arc::clone(&book),
                                std::sync::Arc::clone(&reachability_resolver),
                                std::sync::Arc::clone(&community_relay_resolver),
                                community,
                                resync,
                                std::sync::Arc::clone(&dirty),
                                ingest_observer.clone(),
                                // ZEB-919: live epoch-key open candidates.
                                crdt_state_for_addrbook.clone(),
                            );
                        let persist_handle = crate::address_book_sync::spawn_addrbook_persist_task(
                            std::sync::Arc::clone(&book),
                            crate::community_address_book::addrbook_path(&identity_dir, &community),
                            community,
                            dirty,
                        );
                        handles.insert(
                            community_id,
                            vec![
                                queryable_handle,
                                subscriber_handle,
                                requester_handle,
                                persist_handle,
                            ],
                        );
                    }
                    AddressBookRequest::Unsubscribe { community_id } => {
                        if let Some(hs) = handles.remove(&community_id) {
                            for h in hs {
                                h.abort();
                            }
                        }
                        // The resync handle stays in the hub (see its doc: a
                        // resubscribe must not re-mint one half of the pair),
                        // and so do the book's ROWS — leaving a community is a
                        // distinct eviction path (spec §5, Task 7).
                    }
                }
            }
            // Request channel closed (stop_node dropped the sender): abort any
            // still-running groups, same terminal drain as the presence pool.
            for (_cid, hs) in handles.drain() {
                for h in hs {
                    h.abort();
                }
            }
        });
    }

    tracing::info!("event loop running");

    loop {
        let mut should_tick = false;

        tokio::select! {
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
                        // ZEB-703: Phase C mutations persist via notify_dirty.
                        owner_sync_engine.clone(),
                    )
                    .await;
                }

                // Refresh direct peer set every 20 timer ticks (~5 seconds).
                // Driven by timer only (not Zenoh events) to avoid excessive
                // session-info calls under high message traffic.
                peer_refresh_counter += 1;
                if peer_refresh_counter.is_multiple_of(20) {
                    let refreshed: Vec<String> =
                        direct_link_zids(&session).await.into_iter().collect();
                    // ZEB-622: any up-edge (a zid absent last poll, present now)
                    // bumps the transport epoch — community root-fetch /
                    // channel-backfill / mail-root latches re-arm (their drivers
                    // subscribe). A same-zid reconnect after a drop now re-fires.
                    if detect_up_edges(&mut transport_prev_zids, refreshed) {
                        transport_epoch_tx.send_modify(|e| *e = e.wrapping_add(1));
                    }
                    // `detect_up_edges` moved `refreshed` into `transport_prev_zids`
                    // (now the fresh snapshot); mirror it into the hop-distance set,
                    // which tracks the same snapshot.
                    direct_peer_zids = transport_prev_zids.clone();
                }

                // ZEB-418 P2 Task 7 (D16): periodic routing-record
                // re-publish so the advertised butler set's `bs_at` never
                // lapses while the device is up. The closure is sync and
                // spawns its own async work — the timer arm never blocks.
                if let Some(ref republish) = routing_republish {
                    routing_republish_counter += 1;
                    if routing_republish_counter.is_multiple_of(ROUTING_REPUBLISH_TICKS) {
                        republish();
                    }
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
                        note_announce_sample(
                            &observed_holders,
                            &key_expr,
                            &payload,
                            source_zid.as_deref(),
                            &own_zid,
                            || start.elapsed().as_millis() as u64,
                        );
                        // ZEB-669 S2: signed buddy records route to the
                        // record store; nothing else consumes them.
                        // Hosting receipt stamps use WALL-clock ms (the
                        // IPC layer computes report ages against the same
                        // clock — loop-relative time isn't visible there).
                        if key_expr.starts_with(crate::STORAGE_RECORD_PREFIX) {
                            if note_storage_record_sample(
                                &storage_records,
                                &key_expr,
                                &payload,
                                &revoked_projection,
                                crate::wall_clock_ms,
                            ) {
                                crate::node_event_sink::emit_ser(
                                    app.as_ref(),
                                    "storage-buddies-updated",
                                    &serde_json::Value::Null,
                                );
                            }
                            continue;
                        }
                        let hop_distance = source_zid.as_ref().map(|zid| {
                            if direct_peer_zids.contains(zid) { 1u8 } else { 2u8 }
                        });
                        let vine_evict_cid = emit_frontend_event(
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
                        // ZEB-670: a vine tombstone freed the last live
                        // reference to its content — burn the blob like
                        // ContentVerbRequest::Burn, EXCEPT a deliberate
                        // local pin outlives a remote tombstone (skip
                        // entirely; never touch pin_intent).
                        if let Some(cid_hex) = vine_evict_cid {
                            match hex::decode(&cid_hex)
                                .ok()
                                .and_then(|v| <[u8; 32]>::try_from(v).ok())
                            {
                                Some(cid_bytes)
                                    if pin_intent.contains(&cid_bytes)
                                        || buddy_engine.buddy_pins.contains(&cid_bytes) =>
                                {
                                    tracing::info!(
                                        cid = %cid_hex,
                                        "vine tombstone: content locally or buddy-pinned; keeping bytes"
                                    );
                                }
                                Some(cid_bytes) => {
                                    let root = ContentId::from_bytes(cid_bytes);
                                    let doomed =
                                        collect_descendants(runtime.storage_tier().cache(), root);
                                    let protective: std::collections::HashSet<[u8; 32]> =
                                        pin_intent
                                            .union(&buddy_engine.buddy_pins)
                                            .copied()
                                            .collect();
                                    let keep = compute_keep_set(
                                        runtime.storage_tier().cache(),
                                        &protective,
                                        doomed.len(),
                                    );
                                    for id in doomed {
                                        if !keep.contains(&id) {
                                            runtime.unpin_content(&id);
                                            let _ = runtime.remove_content(&id);
                                        }
                                    }
                                }
                                None => tracing::warn!(
                                    cid = %cid_hex,
                                    "vine tombstone: evict CID is not 32-byte hex; skipping evict"
                                ),
                            }
                        }
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
                let max_bytes = req.max_bytes;
                // ZEB-535: re-serve fetched (encrypted) artifact books — when set,
                // every CID admitted during this fetch is allowlisted so this node
                // can serve the artifact to other members.
                let serveable = req.serveable;
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
                            // ZEB-409: bound a single leaf's payload (avatar path
                            // passes Some(AVATAR_MAX_BYTES); other callers None).
                            fetch_via_zenoh(&session, &key, max_bytes).await
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
                        wrap_fetch_one_with_admission(fetch_one, cas_op_tx_for_fetch, serveable);

                    let result = fetch_recursive(fetch_one_with_admit, root, max_bytes).await;
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
                // Parse the CID hex to exactly 32 bytes — this is the only
                // precondition for parse_subscription_event to route the message
                // into StorageTierEvent::PublishContent. Capturing the parsed
                // ContentId here lets the serveable allowlist below reuse it
                // (no redundant re-decode, no unreachable failure branch).
                let parsed_cid = hex::decode(&req.cid_hex)
                    .ok()
                    .and_then(|b| <[u8; 32]>::try_from(b).ok())
                    .map(ContentId::from_bytes);
                match parsed_cid {
                    None => {
                        let _ = req.reply.send(Err(format!("invalid CID hex: {}", req.cid_hex)));
                    }
                    Some(cid) => {
                        // ZEB-535: a serveable ingest allowlists its CID for
                        // member-to-member serving. Confirm the bytes actually hash
                        // to that CID FIRST — junk bytes are silently dropped by
                        // StorageTier, but allowlisting them anyway would poison the
                        // encrypted-CID serve allowlist with un-servable entries.
                        if req.serveable && !cid.verify_hash(&req.data) {
                            let _ = req.reply.send(Err(format!(
                                "serveable ingest CID hash mismatch: {}",
                                req.cid_hex
                            )));
                            continue;
                        }
                        let key_expr = format!("harmony/content/publish/{}", req.cid_hex);
                        runtime.push_event(RuntimeEvent::SubscriptionMessage {
                            key_expr,
                            payload: req.data,
                        });
                        // Tick immediately so content is committed before replying.
                        for action in runtime.tick() {
                            dispatch_action(action, &session, &zenoh_tx, &app, &closing, &own_zid)
                                .await;
                        }
                        // ZEB-535: allowlist this CID for member-to-member serving so a
                        // chunked encrypted artifact's CIDs are servable (the serve gate
                        // refuses encrypted CIDs that aren't allowlisted). Hash-verified
                        // above for serveable requests.
                        if req.serveable {
                            serve_allowlist.allow(cid);
                        }
                        let _ = req.reply.send(Ok(()));
                    }
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
                    CasOp::PutLocal { cid, blob, serveable, reply } => {
                        let cid_hex = hex::encode(cid.to_bytes());
                        // ZEB-535: a serveable PutLocal allowlists its CID for
                        // re-serving. Confirm the bytes hash to the CID FIRST so junk
                        // bytes can't poison the encrypted-CID serve allowlist (the
                        // StorageTier ingest below silently drops corrupted bytes).
                        if serveable && !cid.verify_hash(&blob) {
                            tracing::warn!(cid=%cid_hex, "serveable PutLocal bytes failed hash==cid; not allowlisting");
                            if let Some(reply) = reply {
                                let _ = reply.send(Err(crate::content_store::ContentStoreError::Io(
                                    format!("serveable PutLocal CID hash mismatch: {cid_hex}"),
                                )));
                            }
                            continue;
                        }
                        let key_expr = format!("harmony/content/publish/{cid_hex}");
                        runtime.push_event(RuntimeEvent::SubscriptionMessage {
                            key_expr,
                            payload: blob,
                        });
                        for action in runtime.tick() {
                            dispatch_action(action, &session, &zenoh_tx, &app, &closing, &own_zid)
                                .await;
                        }
                        // ZEB-535: allowlist a serveable CID so a fetcher that
                        // re-admits an encrypted artifact book can in turn serve it
                        // to other members (the fetch-admission wrapper sets this).
                        if serveable {
                            serve_allowlist.allow(cid);
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
                    CasOp::AllowServeSubtree { root, reply } => {
                        // ZEB-539: allowlist a validated artifact's whole local CID
                        // subtree for member-to-member re-serve.
                        //
                        // Qodo (High): do NOT walk the DAG + allowlist inline in
                        // the `select!` arm — a deep tree would delay every other
                        // CAS/network event. The `ContentStore` is not
                        // `Clone + Send + 'static`, so it can't be moved into a
                        // `spawn_blocking`; instead spawn an async task that walks
                        // via read-only `CasOp::GetLocal` round-trips (which never
                        // trigger a network fetch) and allowlists off-loop. The
                        // arm returns immediately. `serve_allowlist` is `Clone`
                        // (shared handle); the walk reuses
                        // `collect_descendants_via_cas` (no duplicated walk).
                        let cas_op_tx_for_walk = cas_op_tx.clone();
                        let serve_allowlist_for_walk = serve_allowlist.clone();
                        tokio::spawn(async move {
                            let result =
                                collect_descendants_via_cas(&cas_op_tx_for_walk, root)
                                    .await
                                    .map(|all| {
                                        // `collect_descendants_via_cas` dedups
                                        // during traversal, so `all` is already
                                        // unique — `len()` is the true count and
                                        // no post-hoc HashSet is needed. `allow`
                                        // is idempotent regardless.
                                        let count = all.len();
                                        for cid in all {
                                            serve_allowlist_for_walk.allow(cid);
                                        }
                                        count
                                    });
                            let _ = reply.send(result);
                        });
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
                                let fetch = fetch_via_zenoh(&session_clone, &key, None);
                                match tokio::time::timeout(timeout, fetch).await {
                                    Ok(Ok(bytes)) => {
                                        // ZEB-343 verify-on-fetch (spec §5.3):
                                        // mirror the wrap_fetch_one_with_admission
                                        // gate (T4). A Zenoh reply is untrusted —
                                        // verify hash==cid BEFORE admitting or
                                        // returning. On failure, do NOT admit and
                                        // reply Ok(None): treat a tampered reply as
                                        // a cache miss so the caller falls through
                                        // exactly as it would on a timeout.
                                        //
                                        // ZEB-805: "as it would on a timeout" is
                                        // still the right shape, but that used to
                                        // be justified with "the CRDT carries
                                        // recovery" — which was false. What makes
                                        // this safe is the caller's BOUNDED RETRY,
                                        // not eventual consistency. A retry against
                                        // a persistently tampering holder still
                                        // exhausts and drops, which is correct.
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
                                            // ZEB-535: GetOrFetch (community-root /
                                            // CRDT sync) does not re-serve via this
                                            // hop; the put_serveable path allowlists
                                            // community roots explicitly.
                                            serveable: false,
                                            reply: None,
                                        });
                                        let _ = reply.send(Ok(Some(bytes)));
                                    }
                                    Ok(Err(e)) => {
                                        let _ = reply.send(Err(crate::content_store::ContentStoreError::Io(
                                            format!("fetch '{key}': {e}"),
                                        )));
                                    }
                                    // Timeout → Ok(None).
                                    //
                                    // ZEB-805: this used to read "(CRDT carries
                                    // recovery)". It does not. The recovery that
                                    // claim named — the next state-root from any
                                    // peer — is a LARGER blob fetched under the
                                    // SAME budget. Attempts are independent; their
                                    // difficulty is not, because state only grows,
                                    // so that recovery path is anti-correlated
                                    // with the failure it is supposed to repair —
                                    // and a transient miss became a
                                    // 90-minute silent partition. Ok(None) means
                                    // "not fetchable within the caller's budget";
                                    // deciding what to do about that belongs to
                                    // the caller, which now retries under bound.
                                    Err(_) => {
                                        let _ = reply.send(Ok(None));
                                    }
                                }
                            });
                        }
                    }
                }
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
                        // ZEB-669 S2 (PR #449 review): a buddy pact may
                        // still pin this root — clear only the USER's
                        // intent and leave the physical pin to the
                        // engine's ownership.
                        if buddy_engine.buddy_pins.contains(&cid) {
                            let _ = reply.send(Ok(true));
                        } else {
                            let root = ContentId::from_bytes(cid);
                            let doomed =
                                collect_descendants(runtime.storage_tier().cache(), root);

                            // ZEB-156: any CID reachable from a still-pinned root
                            // must stay pinned even when it sits in `doomed`. See
                            // `compute_keep_set` for the cross-cutting rationale.
                            // ZEB-669 S2: buddy roots protect their subtrees too.
                            let protective: std::collections::HashSet<[u8; 32]> = pin_intent
                                .union(&buddy_engine.buddy_pins)
                                .copied()
                                .collect();
                            let keep = compute_keep_set(
                                runtime.storage_tier().cache(),
                                &protective,
                                doomed.len(),
                            );

                            for id in doomed {
                                if !keep.contains(&id) {
                                    runtime.unpin_content(&id);
                                }
                            }
                            let _ = reply.send(Ok(true));
                        }
                    }
                    ContentVerbRequest::Burn { cid, reply } => {
                        // Burn on a RAM-only client cascades the runtime-side
                        // unpin; the sidecar-removal side of burn continues to
                        // happen in the Tauri command handler.
                        // ZEB-155: also drop any persisted intent (the Tauri
                        // command removes the sidecar entry, but this keeps
                        // the in-memory set consistent if the orders diverge).
                        pin_intent.remove(&cid);
                        // ZEB-669 S2 (PR #449 review): burn is the user
                        // explicitly destroying local bytes — it overrides
                        // buddy ownership. Drop the ledger claim too so
                        // hosting reports stay honest; the engine refetches
                        // next tick if the pact still wants it.
                        if buddy_engine.buddy_pins.remove(&cid) {
                            storage_ledger
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .drop_cid_everywhere(&hex::encode(cid));
                        }
                        let root = ContentId::from_bytes(cid);
                        let doomed = collect_descendants(runtime.storage_tier().cache(), root);

                        // ZEB-156: same keep-set logic as Unpin — burning one
                        // root must not destroy bytes another pinned root
                        // still relies on. See `compute_keep_set` for the
                        // shared-subtree case the Tauri OR-join misses.
                        let protective: std::collections::HashSet<[u8; 32]> = pin_intent
                            .union(&buddy_engine.buddy_pins)
                            .copied()
                            .collect();
                        let keep = compute_keep_set(
                            runtime.storage_tier().cache(),
                            &protective,
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
            // by the Tauri command handlers. ZEB-811 Task 8: those same
            // handlers (`follow_vine_creator_impl` / `unfollow_vine_creator_impl`,
            // lib.rs) also `notify_one()` the vine-pull driver's wake handle
            // there — this arm stays a no-op. When per-creator Zenoh
            // subscriptions are added (once the publish path includes
            // /announce/), the follow_rx channel will drive Subscribe/
            // Unsubscribe actions here.
            Some(req) = follow_rx.recv() => {
                match req {
                    FollowRequest::Follow { .. } | FollowRequest::Unfollow { .. } => {}
                    // ZEB-671: wire publication of the signed follow list.
                    FollowRequest::PublishFollowList { owner, payload } => {
                        let key = format!("harmony/vines/{owner}/follows");
                        if let Err(e) = session.put(&key, payload).await {
                            tracing::error!(error = %e, key, "follow-list publish failed");
                            // Same degraded-sync signal the other publish
                            // adapters emit (CodeRabbit PR #447): the UI
                            // can surface that Discover-graph propagation
                            // is impaired instead of failing silently.
                            crate::node_event_sink::emit_ser(
                                app.as_ref(),
                                "follow-list-sync-degraded",
                                &serde_json::json!({ "reason": "publish_failed", "key": key }),
                            );
                        }
                    }
                }
            }

            // ── Voice frame relay (frontend → Zenoh) ────────────────
            // Await directly instead of spawning per-frame tasks — preserves
            // ordering and applies natural backpressure from Zenoh.
            Some(voice) = voice_rx.recv() => {
                match voice {
                    crate::voice::VoiceOutbound::Channel { community_id, channel_id, frame } => {
                        // ZEB-362: seal the frame AND sign it with this device's
                        // signing key (the same device-#2 key that signs presence
                        // beacons, stashed in `voice_identity` at Join), then
                        // publish to the own-device-named topic. A receiver now
                        // authenticates the sender from the verified presence map
                        // instead of trusting the (sender-controlled) topic suffix.
                        if let (Some(key), Some(identity)) = (
                            voice_keys.get(&(community_id, channel_id)),
                            voice_identity.get(&(community_id, channel_id)),
                        ) {
                            let own = voice_own_device.as_deref().unwrap_or("self");
                            match crate::voice_crypto::seal_and_sign_voice_packet(
                                key,
                                &identity.3,
                                &community_id,
                                &channel_id,
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
                                Err(e) => tracing::warn!(err = %e, "voice seal+sign failed; dropping frame"),
                            }
                        }
                        // else: not joined / no signing identity for that
                        // (community, channel) — drop.
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
                        let own_device_hex_sub = hex::encode(caps.self_device);
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
                        // ZEB-358: media-drop — resolve each sender's owner via the
                        // presence map and drop the frame if that owner is currently
                        // muted/kicked in this channel.
                        let mod_map_media = std::sync::Arc::clone(&voice_moderation_map);
                        let presence_map_media = std::sync::Arc::clone(&voice_presence_map);
                        let voice_now_ms_media = std::sync::Arc::clone(&voice_now_ms);
                        // ZEB-358 (Qodo perf): skip the per-frame presence+moderation
                        // lookups entirely while no moderation is active.
                        let mod_active_media = std::sync::Arc::clone(&voice_moderation_active);
                        match session.declare_subscriber(&sub_key).await {
                            Ok(sub) => {
                                // The subscriber opens (decrypts) before emitting;
                                // an AEAD failure (non-member / stale epoch /
                                // tamper) is dropped silently.
                                //
                                // ZEB-353/355: on a transport drop the shared
                                // driver (voice_reconnect.rs) re-declares the
                                // subscriber with progress-aware backoff,
                                // emitting transport-lost on drop and
                                // transport-restored on re-declare so the
                                // frontend can show "Reconnecting…".
                                let app_frame = app_sub.clone();
                                let on_frame = async move |sample: zenoh::sample::Sample| {
                                    // ZEB-362 (Qodo perf): the `.../*` subscription also
                                    // delivers our OWN published frames; skip them before
                                    // the per-frame verify/open work (mirrors the DM
                                    // subscriber's self-skip). We never play our own mic
                                    // back.
                                    if sample.key_expr().as_str().rsplit('/').next()
                                        == Some(own_device_hex_sub.as_str())
                                    {
                                        return;
                                    }
                                    if sample.payload().len() > crate::voice_crypto::MAX_VOICE_PACKET_BYTES {
                                        tracing::warn!(
                                            len = sample.payload().len(),
                                            max = crate::voice_crypto::MAX_VOICE_PACKET_BYTES,
                                            "oversized voice packet dropped"
                                        );
                                        return;
                                    }
                                    // ZEB-362: authenticate the sender of EVERY frame
                                    // (always-verify, fail-closed). The topic suffix is
                                    // now an untrusted hint; the detached Ed25519
                                    // signature, checked against the device VK we trust
                                    // from the verified presence roster, binds the frame
                                    // to its owner.
                                    //
                                    // (1) parse the claimed device VK from the suffix.
                                    let dev = match sample
                                        .key_expr()
                                        .as_str()
                                        .rsplit('/')
                                        .next()
                                        .and_then(|h| hex::decode(h).ok())
                                        .and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok())
                                    {
                                        Some(d) => d,
                                        None => return, // not 32-byte hex → drop
                                    };
                                    // (2) device → verified owner from the signed presence
                                    //     roster. Unknown device → drop (fail-closed). The
                                    //     start-muted invariant (D10) means a transmitting
                                    //     device has already announced presence, so this
                                    //     costs no real audio.
                                    let owner = {
                                        let g = presence_map_media.lock().await;
                                        g.owner_for_device(&c_sub, &ch_sub, &dev)
                                    };
                                    let owner = match owner {
                                        Some(o) => o,
                                        None => return,
                                    };
                                    let sealed = sample.payload().to_bytes().to_vec();
                                    // (3) verify the per-frame signature against the device.
                                    if crate::voice_crypto::verify_voice_frame_sig(
                                        &dev, &c_sub, &ch_sub, &sealed,
                                    )
                                    .is_err()
                                    {
                                        return; // forged / spoofed-suffix / corrupt → drop
                                    }
                                    // (4) moderation drop on the now-AUTHENTICATED owner,
                                    //     gated for the hot path (no extra locks while no
                                    //     moderation is active — Qodo perf).
                                    if mod_active_media
                                        .load(std::sync::atomic::Ordering::Relaxed)
                                    {
                                        let now = (voice_now_ms_media)();
                                        let g = mod_map_media.lock().await;
                                        if g.is_muted(&c_sub, &ch_sub, &owner, now)
                                            || g.is_kicked(&c_sub, &ch_sub, &owner, now)
                                        {
                                            return; // moderated sender — un-spoofable now
                                        }
                                    }
                                    // (5) open the packet (AAD binds the device VK).
                                    let frame = match crate::voice_crypto::open_voice_packet_unchecked(
                                        &key_for_sub,
                                        &dev,
                                        &c_sub,
                                        &ch_sub,
                                        &sealed,
                                    ) {
                                        Ok(f) => f,
                                        Err(_) => return, // wrong key / stale / tamper → drop
                                    };
                                    // (6) attribution integrity: the cleartext header's
                                    //     senderHash (VK[0..16], bytes 7..23) must match the
                                    //     authenticated device, so a member can't sign their
                                    //     own frame but mislabel the audio as someone else.
                                    if frame.len() < 23 || frame[7..23] != dev[..16] {
                                        return;
                                    }
                                    crate::node_event_sink::emit_ser(app_frame.as_ref(), "voice-frame-received", &serde_json::json!({ "frameBytes": frame }));
                                };
                                let app_lost = app_sub.clone();
                                let lost_community = community_hex.clone();
                                let lost_channel = channel_hex.clone();
                                let on_lost = move || {
                                    crate::node_event_sink::emit_ser(app_lost.as_ref(), "voice-transport-lost", &serde_json::json!({
                                            "communityId": lost_community,
                                            "channelId": lost_channel,
                                        }));
                                };
                                let on_restored = move || {
                                    crate::node_event_sink::emit_ser(app_sub.as_ref(), "voice-transport-restored", &serde_json::json!({
                                            "communityId": community_hex,
                                            "channelId": channel_hex,
                                        }));
                                };
                                let handle =
                                    tokio::spawn(crate::voice_reconnect::run_media_subscriber(
                                        crate::voice_reconnect::MediaSubscriberCtx {
                                            session: session_for_media,
                                            sub_key: sub_key_retry,
                                            label: "voice",
                                            closing: closing_sub,
                                        },
                                        sub,
                                        on_frame,
                                        on_lost,
                                        on_restored,
                                    ));
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
                                        registry.clone(),
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
                                    // ZEB-612: shared raised-hand cell (0 = lowered;
                                    // else wall-clock ms of the first raise).
                                    // `SetHand` updates it; each heartbeat
                                    // republishes it.
                                    let hand_flag = std::sync::Arc::new(
                                        std::sync::atomic::AtomicU64::new(0),
                                    );
                                    voice_hand_flags.insert(
                                        (community_id, channel_id),
                                        std::sync::Arc::clone(&hand_flag),
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
                                    // ZEB-358: per-channel self-kicked flag gating the
                                    // presence publisher (a kicked owner stops beaconing
                                    // so peers presence-evict us). Set by the control sub
                                    // + sweep tick from the moderation map.
                                    let self_kicked_flag = std::sync::Arc::new(
                                        std::sync::atomic::AtomicBool::new(false),
                                    );
                                    voice_self_kicked_flags.insert(
                                        (community_id, channel_id),
                                        std::sync::Arc::clone(&self_kicked_flag),
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
                                        hand_flag,
                                        std::sync::Arc::clone(&self_kicked_flag),
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
                                    // ZEB-358: control subscriber — open + verify +
                                    // authorize each sealed directive, then apply to
                                    // the shared moderation map and re-emit the
                                    // overlay. Mirrors the media sub's reconnect
                                    // backoff (5s → 60s cap; reset only on progress).
                                    let control_topic = format!(
                                        "harmony/voice-control/{}/{}",
                                        hex::encode(community_id.0),
                                        hex::encode(channel_id.0),
                                    );
                                    let ctrl_session = Arc::clone(&session_arc);
                                    let ctrl_key = std::sync::Arc::clone(&caps.channel_key);
                                    let ctrl_registry = community_registry
                                        .clone()
                                        .expect("registry present in this branch");
                                    let ctrl_mod_map =
                                        std::sync::Arc::clone(&voice_moderation_map);
                                    let ctrl_presence_map =
                                        std::sync::Arc::clone(&voice_presence_map);
                                    let ctrl_app = app.clone();
                                    let ctrl_closing = closing.clone();
                                    let ctrl_now_ms = std::sync::Arc::clone(&voice_now_ms);
                                    let ctrl_self_owner = caps.self_owner;
                                    let ctrl_community = community_id;
                                    let ctrl_channel = channel_id;
                                    // ZEB-358: the control sub updates the self-kicked
                                    // flag (gates presence publishing) and the global
                                    // moderation-active flag (gates the audio hot path)
                                    // from the moderation map after each apply.
                                    let ctrl_self_kicked =
                                        std::sync::Arc::clone(&self_kicked_flag);
                                    let ctrl_mod_active =
                                        std::sync::Arc::clone(&voice_moderation_active);
                                    let handle = tokio::spawn(async move {
                                        // ZEB-355: backoff arithmetic shared with the
                                        // media loops (voice_reconnect.rs); this loop
                                        // keeps its declare-at-top shape (no UI events).
                                        let mut backoff =
                                            crate::voice_reconnect::ProgressBackoff::new();
                                        loop {
                                            let sub = match ctrl_session
                                                .declare_subscriber(&control_topic)
                                                .await
                                            {
                                                Ok(s) => s,
                                                Err(e) => {
                                                    if ctrl_closing.load(
                                                        std::sync::atomic::Ordering::SeqCst,
                                                    ) {
                                                        return;
                                                    }
                                                    let delay = backoff.on_redeclare_failure();
                                                    tracing::warn!(
                                                        %control_topic,
                                                        err = %e,
                                                        backoff_s = delay.as_secs(),
                                                        "voice control subscribe failed; retrying"
                                                    );
                                                    tokio::time::sleep(delay).await;
                                                    continue;
                                                }
                                            };
                                            // Track real progress so a flapping
                                            // sub (declares OK, never receives)
                                            // rate-limits instead of spinning.
                                            let mut made_progress = false;
                                            while let Ok(sample) = sub.recv_async().await {
                                                made_progress = true;
                                                if sample.payload().len()
                                                    > crate::voice_crypto::MAX_VOICE_PACKET_BYTES
                                                {
                                                    continue;
                                                }
                                                let packet =
                                                    sample.payload().to_bytes().to_vec();
                                                let Some(signed) =
                                                    crate::voice_moderation::open_directive(
                                                        &ctrl_key,
                                                        &ctrl_community,
                                                        &ctrl_channel,
                                                        &packet,
                                                    )
                                                else {
                                                    continue;
                                                };
                                                // `directive_signer_is_authorized`
                                                // verifies the device-#2 signature
                                                // (via `verify_directive_authority`)
                                                // before the membership + power
                                                // gates, so an explicit
                                                // `verify_directive_sig` here would
                                                // be redundant (Qodo).
                                                if crate::voice_moderation::directive_signer_is_authorized(
                                                    &ctrl_registry,
                                                    &ctrl_community,
                                                    &signed,
                                                )
                                                .await
                                                .is_err()
                                                {
                                                    continue;
                                                }
                                                let now = (ctrl_now_ms)();
                                                let changed = {
                                                    let mut g = ctrl_mod_map.lock().await;
                                                    // ZEB-853 C2: forward-skew reject at the
                                                    // ingest boundary. `now` above is the
                                                    // MONOTONIC liveness clock (voice_now_ms =
                                                    // start.elapsed) — unusable against the
                                                    // attacker-controlled wall stamp — so the
                                                    // gate reads the receiver's own WALL clock
                                                    // (receiver_now_ms(); None ⇒ apply-all).
                                                    // Drops a directive whose issued_hlc.wall_ms
                                                    // is >5 min ahead, which would otherwise win
                                                    // the strictly_newer LWW forever and freeze
                                                    // the slot (FAIL-OPEN).
                                                    g.apply_gated(
                                                        &ctrl_community,
                                                        &ctrl_channel,
                                                        &signed.directive,
                                                        now,
                                                        crate::voice_moderation::ENFORCE_TTL_MS,
                                                        crate::clock_trust::receiver_now_ms(),
                                                    )
                                                };
                                                // ZEB-358: refresh the self-kicked flag
                                                // (gates our presence publisher) and the
                                                // global moderation-active flag (gates the
                                                // audio hot path) from the just-applied map.
                                                {
                                                    let g = ctrl_mod_map.lock().await;
                                                    ctrl_self_kicked.store(
                                                        g.is_kicked(
                                                            &ctrl_community,
                                                            &ctrl_channel,
                                                            &ctrl_self_owner.0,
                                                            now,
                                                        ),
                                                        std::sync::atomic::Ordering::Relaxed,
                                                    );
                                                    ctrl_mod_active.store(
                                                        g.any_enforced(now),
                                                        std::sync::atomic::Ordering::Relaxed,
                                                    );
                                                }
                                                if changed {
                                                    emit_moderation_changed(
                                                        &ctrl_app,
                                                        &ctrl_registry,
                                                        &ctrl_presence_map,
                                                        &ctrl_mod_map,
                                                        ctrl_community,
                                                        ctrl_channel,
                                                        ctrl_self_owner,
                                                        now,
                                                    )
                                                    .await;
                                                }
                                            }
                                            // Inner receive loop ended on a
                                            // transport error.
                                            if ctrl_closing
                                                .load(std::sync::atomic::Ordering::SeqCst)
                                            {
                                                break;
                                            }
                                            tracing::warn!(
                                                %control_topic,
                                                "voice control subscriber closed unexpectedly; reconnecting"
                                            );
                                            if let Some(delay) = backoff.on_drop(made_progress) {
                                                tokio::time::sleep(delay).await;
                                            }
                                        }
                                    });
                                    if let Some(old) = voice_control_subs
                                        .insert((community_id, channel_id), handle)
                                    {
                                        old.abort();
                                    }
                                    // Emit the moderation overlay once on join so the
                                    // frontend gets `selfPower` immediately — otherwise a
                                    // moderator joining a quiet channel (no active
                                    // directives) would see no Mute/Remove controls until
                                    // the next moderation apply/sweep (Cursor: "overlay
                                    // missing on join").
                                    emit_moderation_changed(
                                        &app,
                                        &registry,
                                        &voice_presence_map,
                                        &voice_moderation_map,
                                        community_id,
                                        channel_id,
                                        caps.self_owner,
                                        (voice_now_ms)(),
                                    )
                                    .await;
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
                        // ZEB-358: tear down the moderation control sub + drop the
                        // channel's enforcement state, any issuer directives we were
                        // re-asserting, and the per-channel directive seq.
                        if let Some(h) = voice_control_subs.remove(&(community_id, channel_id)) {
                            h.abort();
                        }
                        voice_self_kicked_flags.remove(&(community_id, channel_id));
                        // Drop this channel's enforcement state AND recompute the
                        // global moderation-active flag in the same lock — the
                        // channel we left may have held the only enforced
                        // directives, so without this the atomic could stay true
                        // and force needless per-frame lookups on later joins
                        // (Cursor: "Leave skips moderation flag refresh").
                        let mod_active_after_leave = {
                            let mut g = voice_moderation_map.lock().await;
                            g.remove_channel(&community_id, &channel_id);
                            g.any_enforced((voice_now_ms)())
                        };
                        voice_moderation_active
                            .store(mod_active_after_leave, std::sync::atomic::Ordering::Relaxed);
                        voice_issuer_directives.retain(|(c, ch, _t, _m), _| {
                            !(*c == community_id && *ch == channel_id)
                        });
                        voice_moderation_seq.remove(&(community_id, channel_id));
                        // ZEB-351 Voice V3: drop the shared mute flag in lockstep
                        // with the media key so a later `SetMuted` for a left
                        // channel is a no-op. ZEB-612: ditto the hand cell — a
                        // rejoin starts hand-lowered.
                        voice_mute_flags.remove(&(community_id, channel_id));
                        voice_hand_flags.remove(&(community_id, channel_id));
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
                        crate::node_event_sink::emit_ser(app.as_ref(), "voice-presence-changed", &serde_json::json!({
                                "community": hex::encode(community_id.0),
                                "channel": hex::encode(channel_id.0),
                                "roster": Vec::<crate::voice_presence::RosterEntry>::new(),
                            }));
                        // ZEB-358: likewise clear the moderation overlay for the
                        // channel we left, so the UI never shows a stale mute/kick
                        // banner after leaving (mirrors the empty-roster emit above).
                        crate::node_event_sink::emit_ser(app.as_ref(), "voice-moderation-changed", &serde_json::json!({
                                "community": hex::encode(community_id.0),
                                "channel": hex::encode(channel_id.0),
                                "mutedOwners": Vec::<String>::new(),
                                "kickedOwners": Vec::<String>::new(),
                                // ZEB-612: keep the reset payload's shape in
                                // lockstep with emit_moderation_changed so a
                                // uniform consumer never sees the invite keys
                                // vanish (CodeRabbit Major, PR #442).
                                "invitedOwners": Vec::<String>::new(),
                                "powers": serde_json::Map::new(),
                                "selfPower": 0,
                                "selfModMuted": false,
                                "selfKicked": false,
                                "selfInvited": false,
                            }));
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
                                // ZEB-612: carry the CURRENT hand state so an
                                // immediate mute beacon never silently lowers a
                                // raised hand (the beacon is whole-state).
                                let hand = voice_hand_flags
                                    .get(&(community_id, channel_id))
                                    .map(|f| f.load(std::sync::atomic::Ordering::SeqCst))
                                    .and_then(|hr| (hr != 0).then_some(hr));
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
                                    hand,
                                )
                                .await
                                {
                                    tracing::warn!(%pres_topic, err = ?e, "immediate mute beacon publish failed");
                                }
                            }
                        }
                    }
                    crate::voice::VoiceChannelRequest::SetHand { community_id, channel_id, raised_at } => {
                        // ZEB-612 Town Hall: update the shared hand cell the
                        // presence publisher reads each heartbeat. A no-op if
                        // the channel isn't joined (cell absent). The cell
                        // keeps the FIRST raise's stamp across repeat raises
                        // (stable queue position); lower always resets.
                        if let Some(flag) = voice_hand_flags.get(&(community_id, channel_id)) {
                            let hand =
                                crate::voice_presence::update_hand_cell(flag, raised_at);
                            // Immediate beacon so the queue reflects the hand
                            // without waiting out the next ≤4 s heartbeat
                            // (mirrors the SetMuted arm; carries the CURRENT
                            // mute state — the beacon is whole-state).
                            if let (Some(key), Some((owner, device, joined_hlc, signing_key)), Some(seq_counter), Some(mute_flag)) = (
                                voice_keys.get(&(community_id, channel_id)),
                                voice_identity.get(&(community_id, channel_id)),
                                voice_presence_seq.get(&(community_id, channel_id)),
                                voice_mute_flags.get(&(community_id, channel_id)),
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
                                    mute_flag.load(std::sync::atomic::Ordering::SeqCst),
                                    hand,
                                )
                                .await
                                {
                                    tracing::warn!(%pres_topic, err = ?e, "immediate hand beacon publish failed");
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
                                // ZEB-353/355: on a transport drop the shared
                                // driver (voice_reconnect.rs) re-declares the
                                // subscriber with progress-aware backoff,
                                // emitting transport-lost on drop and
                                // transport-restored on re-declare so the
                                // frontend can show "Reconnecting…".
                                let app_frame = app_sub.clone();
                                let frame_call_hex = call_hex.clone();
                                let on_frame = async move |sample: zenoh::sample::Sample| {
                                    // Skip our own published frames: on a 2-party
                                    // DM the `.../*` sub also matches our own
                                    // {senderDevice} segment, and decrypting +
                                    // emitting them just to have the frontend drop
                                    // them by sender hash wastes CPU on the audio
                                    // hot path (self is ~half of DM traffic).
                                    if sample.key_expr().as_str().rsplit('/').next()
                                        == Some(own_device_hex.as_str())
                                    {
                                        return;
                                    }
                                    if sample.payload().len()
                                        > crate::voice_crypto::MAX_VOICE_PACKET_BYTES
                                    {
                                        return;
                                    }
                                    let sealed = sample.payload().to_bytes().to_vec();
                                    if let Ok(frame) = crate::voice_crypto::decrypt_dm_voice_packet(
                                        &key_for_sub,
                                        &call_id,
                                        crate::voice_crypto::VOICE_DM_PACKET_AAD,
                                        &sealed,
                                    ) {
                                        crate::node_event_sink::emit_ser(app_frame.as_ref(), "dm-voice-frame-received", &serde_json::json!({
                                                "callId": frame_call_hex,
                                                "frameBytes": frame,
                                            }));
                                    }
                                };
                                let app_lost = app_sub.clone();
                                let lost_call_hex = call_hex.clone();
                                let on_lost = move || {
                                    crate::node_event_sink::emit_ser(app_lost.as_ref(), "voice-transport-lost", &serde_json::json!({ "callId": lost_call_hex }));
                                };
                                let restored_call_hex = call_hex.clone();
                                let on_restored = move || {
                                    crate::node_event_sink::emit_ser(app_sub.as_ref(), "voice-transport-restored", &serde_json::json!({ "callId": restored_call_hex }));
                                };
                                let handle =
                                    tokio::spawn(crate::voice_reconnect::run_media_subscriber(
                                        crate::voice_reconnect::MediaSubscriberCtx {
                                            session: session_for_media,
                                            sub_key: sub_key_retry,
                                            label: "dm voice",
                                            closing: closing_sub,
                                        },
                                        sub,
                                        on_frame,
                                        on_lost,
                                        on_restored,
                                    ));
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
                    // ZEB-358: a moderator issues a directive. Sign + seal under
                    // the channel key, publish to the control topic, loopback-apply
                    // locally, and track for re-assert/revoke in the 4 s tick.
                    crate::voice::VoiceChannelRequest::Moderate {
                        community_id,
                        channel_id,
                        target_owner,
                        action,
                        duration_ms,
                        issued_hlc,
                    } => {
                        // Resolve the active-channel caps (mirrors the SetMuted arm).
                        if let (Some(key), Some((self_owner, self_device, _hlc, signing_key)), Some(registry)) = (
                            voice_keys.get(&(community_id, channel_id)),
                            voice_identity.get(&(community_id, channel_id)),
                            community_registry.clone(),
                        ) {
                            let seq = {
                                let c = voice_moderation_seq
                                    .entry((community_id, channel_id))
                                    .or_insert(0);
                                *c += 1;
                                *c
                            };
                            let directive = crate::voice_moderation::VoiceModerationDirective {
                                actor_owner: self_owner.0,
                                actor_device: *self_device,
                                target_owner: target_owner.0,
                                action,
                                issued_hlc,
                                seq,
                            };
                            match crate::voice_moderation::sign_directive(directive, signing_key) {
                                Ok(signed) => {
                                    // Local authority pre-check (re-verified by all
                                    // receivers; this is just issuer-side UX).
                                    if crate::voice_moderation::directive_signer_is_authorized(
                                        &registry,
                                        &community_id,
                                        &signed,
                                    )
                                    .await
                                    .is_ok()
                                    {
                                        let control_topic = format!(
                                            "harmony/voice-control/{}/{}",
                                            hex::encode(community_id.0),
                                            hex::encode(channel_id.0),
                                        );
                                        match crate::voice_moderation::seal_directive(
                                            key,
                                            &community_id,
                                            &channel_id,
                                            &signed,
                                        ) {
                                            // Seal failed: do NOT enforce locally or
                                            // register a re-assert — peers would never
                                            // get a valid directive, so the issuer must
                                            // not show phantom enforcement (Cursor).
                                            Err(e) => tracing::warn!(
                                                err = ?e,
                                                "moderation directive seal failed; not applied"
                                            ),
                                            Ok(sealed) => {
                                            // A publish failure here is transient — the
                                            // 4 s re-assert retries — so we still enforce
                                            // locally + register the directive.
                                            if let Err(e) = session.put(&control_topic, sealed).await {
                                                tracing::warn!(%control_topic, err = %e, "moderation directive publish failed");
                                            }
                                            let now = (voice_now_ms)();
                                        // ZEB-612: an invite's default window is
                                        // INVITE_TTL_MS (~2 min), not the 5 min
                                        // punitive default.
                                        let stop_after = now
                                            + duration_ms.unwrap_or(match action {
                                                crate::voice_moderation::ModAction::InviteToSpeak => {
                                                    crate::voice_moderation::INVITE_TTL_MS
                                                }
                                                _ => crate::voice_moderation::DEFAULT_MODERATION_MS,
                                            });
                                        let idkey = (
                                            community_id,
                                            channel_id,
                                            target_owner.0,
                                            action.class(),
                                        );
                                        if action.enforces() {
                                            voice_issuer_directives
                                                .insert(idkey, (signed.clone(), stop_after));
                                        } else {
                                            // Revoke: re-assert briefly for reliable
                                            // delivery, then drop the positive entry.
                                            voice_issuer_directives.insert(
                                                idkey,
                                                (
                                                    signed.clone(),
                                                    now + crate::voice_moderation::ENFORCE_TTL_MS,
                                                ),
                                            );
                                        }
                                        // Loopback apply + emit.
                                        let changed = {
                                            let mut g = voice_moderation_map.lock().await;
                                            g.apply(
                                                &community_id,
                                                &channel_id,
                                                &signed.directive,
                                                now,
                                                crate::voice_moderation::ENFORCE_TTL_MS,
                                            )
                                        };
                                        // ZEB-358 (Qodo perf): refresh the hot-path
                                        // moderation-active flag after the issuer apply.
                                        voice_moderation_active.store(
                                            {
                                                let g = voice_moderation_map.lock().await;
                                                g.any_enforced(now)
                                            },
                                            std::sync::atomic::Ordering::Relaxed,
                                        );
                                        if changed {
                                            emit_moderation_changed(
                                                &app,
                                                &registry,
                                                &voice_presence_map,
                                                &voice_moderation_map,
                                                community_id,
                                                channel_id,
                                                *self_owner,
                                                now,
                                            )
                                            .await;
                                        }
                                            }
                                        }
                                    } else {
                                        tracing::warn!(
                                            "local moderation pre-check failed: insufficient power or not a member"
                                        );
                                    }
                                }
                                Err(e) => tracing::warn!(err = ?e, "moderation directive sign failed"),
                            }
                        } else {
                            // Not joined to this (community, channel): no caps to
                            // sign/seal with, so the directive can't be issued. Log
                            // rather than drop silently.
                            tracing::warn!(
                                "moderate request for a channel we are not joined to; dropped"
                            );
                        }
                    }
                    // ── ZEB-360: group-DM voice presence ──────────────────────
                    // Space-scoped presence on topic
                    // harmony/voice-presence/group-dm/{spaceIdHex}, sealed under the
                    // group-DM presence key. The ROSTER lives in the dedicated
                    // `groupdm_presence_map` keyed by (SpaceId(space_id),
                    // ChannelId(call_id)); media reuses the 1:1 DM path verbatim
                    // (JoinDmCall), so nothing media-related lives here.
                    crate::voice::VoiceChannelRequest::WatchGroupCall {
                        space_id,
                        presence_key,
                    } => {
                        // Read-only roster subscriber for the banner + in-call view.
                        // Idempotent: a second watch for the same space is a no-op
                        // (keep the running sub). Needs the CRDT to verify membership.
                        if let std::collections::hash_map::Entry::Vacant(slot) =
                            groupdm_presence_subs.entry(space_id)
                        {
                            if let Some(crdt) = crdt_state.as_ref() {
                                let topic = format!(
                                    "harmony/voice-presence/group-dm/{}",
                                    hex::encode(space_id)
                                );
                                let sub =
                                    crate::voice_presence::spawn_groupdm_presence_subscriber(
                                        session.clone(),
                                        topic,
                                        presence_key,
                                        crate::owner_state_types::SpaceId(space_id),
                                        std::sync::Arc::clone(crdt),
                                        std::sync::Arc::clone(&groupdm_presence_map),
                                        app.clone(),
                                        closing.clone(),
                                        std::sync::Arc::clone(&voice_now_ms),
                                    );
                                slot.insert(sub);
                            } else {
                                tracing::warn!(
                                    "WatchGroupCall before CRDT state loaded; dropping"
                                );
                            }
                        }
                    }
                    crate::voice::VoiceChannelRequest::UnwatchGroupCall { space_id } => {
                        // Only tear down the read subscriber if NO publisher is active
                        // for this space (an in-call member keeps the roster live).
                        let publisher_active = groupdm_presence_pubs
                            .keys()
                            .any(|(sp, _call)| *sp == space_id);
                        if !publisher_active {
                            if let Some(h) = groupdm_presence_subs.remove(&space_id) {
                                h.abort();
                            }
                            // Drop ALL of this space's roster rows — including
                            // subscriber-discovered `(space, call)` rows we never
                            // published into (those `groupdm_presence_caps` can't
                            // reach, and which would otherwise survive until the TTL
                            // sweep with no subscriber left to refresh/evict them).
                            {
                                let mut g = groupdm_presence_map.lock().await;
                                g.remove_space(&crate::owner_state_types::SpaceId(space_id));
                            }
                        }
                    }
                    crate::voice::VoiceChannelRequest::StartGroupPresence {
                        space_id,
                        call_id,
                        presence_key,
                        caps,
                    } => {
                        let topic = format!(
                            "harmony/voice-presence/group-dm/{}",
                            hex::encode(space_id)
                        );
                        // Ensure the read subscriber is running (reuse the
                        // WatchGroupCall setup if absent) so the in-call roster is live.
                        if let std::collections::hash_map::Entry::Vacant(slot) =
                            groupdm_presence_subs.entry(space_id)
                        {
                            if let Some(crdt) = crdt_state.as_ref() {
                                let sub =
                                    crate::voice_presence::spawn_groupdm_presence_subscriber(
                                        session.clone(),
                                        topic.clone(),
                                        std::sync::Arc::clone(&presence_key),
                                        crate::owner_state_types::SpaceId(space_id),
                                        std::sync::Arc::clone(crdt),
                                        std::sync::Arc::clone(&groupdm_presence_map),
                                        app.clone(),
                                        closing.clone(),
                                        std::sync::Arc::clone(&voice_now_ms),
                                    );
                                slot.insert(sub);
                            } else {
                                tracing::warn!(
                                    "StartGroupPresence before CRDT state loaded; \
                                     publishing without a local roster subscriber"
                                );
                            }
                        }
                        // Start muted (D7); SetGroupCallMuted flips it later. The
                        // publisher and any immediate beacon share one monotone seq.
                        let muted = std::sync::Arc::new(
                            std::sync::atomic::AtomicBool::new(true),
                        );
                        let seq_counter = std::sync::Arc::new(
                            std::sync::atomic::AtomicU64::new(0),
                        );
                        groupdm_presence_mute.insert(
                            (space_id, call_id),
                            std::sync::Arc::clone(&muted),
                        );
                        let pubh = crate::voice_presence::spawn_groupdm_presence_publisher(
                            session.clone(),
                            topic.clone(),
                            std::sync::Arc::clone(&presence_key),
                            crate::owner_state_types::SpaceId(space_id),
                            call_id,
                            std::sync::Arc::clone(&caps.signing_key),
                            caps.self_owner,
                            caps.self_device,
                            caps.joined_hlc.clone(),
                            muted,
                            seq_counter,
                            Duration::from_secs(4),
                            closing.clone(),
                        );
                        // Stash the tombstone caps so StopGroupPresence can mint the
                        // `left` beacon without re-resolving identity.
                        groupdm_presence_caps.insert(
                            (space_id, call_id),
                            (
                                topic,
                                presence_key,
                                std::sync::Arc::clone(&caps.signing_key),
                                caps.self_owner,
                                caps.self_device,
                                caps.joined_hlc,
                            ),
                        );
                        if let Some(old) =
                            groupdm_presence_pubs.insert((space_id, call_id), pubh)
                        {
                            old.abort();
                        }
                    }
                    crate::voice::VoiceChannelRequest::StopGroupPresence {
                        space_id,
                        call_id,
                    } => {
                        // Abort the heartbeat publisher BEFORE publishing the
                        // `left` tombstone: otherwise a heartbeat tick can race the
                        // tombstone and re-add this device to peer rosters until the
                        // TTL sweep. Leave the read subscriber running (the DM view
                        // may still be watching; UnwatchGroupCall stops it on
                        // unmount).
                        if let Some((
                            topic,
                            presence_key,
                            signing_key,
                            self_owner,
                            self_device,
                            joined_hlc,
                        )) = groupdm_presence_caps.remove(&(space_id, call_id))
                        {
                            if let Some(h) = groupdm_presence_pubs.remove(&(space_id, call_id)) {
                                h.abort();
                            }
                            crate::voice_presence::publish_groupdm_leave_tombstone(
                                &session,
                                &topic,
                                &presence_key,
                                &crate::owner_state_types::SpaceId(space_id),
                                call_id,
                                &signing_key,
                                self_owner,
                                self_device,
                                &joined_hlc,
                            )
                            .await;
                        } else if let Some(h) = groupdm_presence_pubs.remove(&(space_id, call_id)) {
                            // No caps stashed (shouldn't happen) — still abort the
                            // publisher so it can't keep advertising presence.
                            h.abort();
                        }
                        groupdm_presence_mute.remove(&(space_id, call_id));
                    }
                    crate::voice::VoiceChannelRequest::SetGroupCallMuted {
                        space_id,
                        call_id,
                        muted,
                    } => {
                        // Flip the presence beacon's mute bit (next beacon reflects
                        // it). Media mute is handled separately via SetDmCallMuted
                        // (Task 8) — do NOT touch media here. No-op if not in this call.
                        if let Some(flag) = groupdm_presence_mute.get(&(space_id, call_id)) {
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
            // ZEB-360: also fires while any group-DM presence subscriber is live,
            // so a crashed group-call participant's ghost row TTL-evicts too.
            // ── ZEB-612 S3: holder sweep + own-content re-announce ────
            _ = reannounce_tick.tick() => {
                let now = start.elapsed().as_millis() as u64;
                // Poison-resilient locks: both maps are best-effort caches;
                // a panic elsewhere must not turn every later tick into a
                // repeat panic of the central event loop.
                observed_holders
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .sweep(now, crate::observed_holders::HOLDER_STALE_MS);
                // Collect under the lock, publish after dropping it — a
                // std Mutex guard must never be held across an await.
                let announcements = {
                    let idx = content_index
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    crate::collect_reannouncements(&idx)
                };
                for (key_expr, payload) in announcements {
                    dispatch_action(
                        RuntimeAction::Publish { key_expr, payload },
                        &session,
                        &zenoh_tx,
                        &app,
                        &closing,
                        &own_zid,
                    )
                    .await;
                }
            }
            // ── ZEB-669 S2: auto-pin engine tick ────────────────────
            _ = buddy_sync_tick.tick(), if !own_owner_addr.is_empty() => {
                let now_ms = start.elapsed().as_millis() as u64;
                // (1) Boot honesty sweep, once: the cache is RAM-only, so
                // ledger claims that didn't survive the restart are dropped
                // — hosting reports must only ever claim actually-held
                // bytes. Wanted cids re-enter via the normal fetch path.
                if !buddy_engine.booted {
                    buddy_engine.booted = true;
                    let admitted: std::collections::HashSet<[u8; 32]> = runtime
                        .storage_tier()
                        .cache()
                        .iter_admitted()
                        .map(|id| id.to_bytes())
                        .collect();
                    let mut ledger = storage_ledger
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    for cid_hex in ledger.distinct_cids() {
                        let cid_bytes = hex::decode(&cid_hex)
                            .ok()
                            .and_then(|v| <[u8; 32]>::try_from(v).ok());
                        match cid_bytes {
                            Some(b) if admitted.contains(&b) => {
                                // Present (shouldn't happen on a RAM-only
                                // cache, but stay correct if persistence
                                // ever lands): re-establish the physical
                                // pin + buddy ownership.
                                buddy_engine.buddy_pins.insert(b);
                                let root = ContentId::from_bytes(b);
                                for id in
                                    collect_descendants(runtime.storage_tier().cache(), root)
                                {
                                    runtime.pin_content(id);
                                }
                            }
                            _ => {
                                tracing::info!(
                                    cid = %cid_hex,
                                    "buddy ledger: cid absent from cache after restart; dropping claim (will refetch)"
                                );
                                ledger.drop_cid_everywhere(&cid_hex);
                            }
                        }
                    }
                }
                // (2) Hosting-report staleness sweep (wall clock — receipt
                // stamps are wall ms, see note_storage_record_sample) +
                // ZEB-923 record-TTL decay + ZEB-679 R1 retroactive
                // revocation purge: records admitted before the projection
                // learned their signer's revocation must stop driving the
                // planner.
                {
                    let mut records = storage_records
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    records.sweep_hosting(crate::wall_clock_ms());
                    // ZEB-923: decay pledge/backup records not renewed
                    // within the TTL; the planner in step (3) observes the
                    // decay in this same tick and releases the pins.
                    if records.sweep_stale_pledges_and_backups(crate::wall_clock_ms()) {
                        tracing::info!("storage records: stale pledge/backup records decayed");
                        crate::node_event_sink::emit_ser(
                            app.as_ref(),
                            "storage-buddies-updated",
                            &serde_json::Value::Null,
                        );
                    }
                    if records.purge_revoked(&revoked_projection) {
                        tracing::info!("storage records purged for revoked signer(s)");
                    }
                }
                // (3) Plan under short locks (guards dropped before any
                // await; the plan is applied mechanically below).
                let plan = {
                    let records = storage_records
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let ledger = storage_ledger
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let settings = storage_settings
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let backoff = &buddy_engine.backoff;
                    crate::buddy_pin_planner::plan(
                        &own_owner_addr,
                        &settings.my_pledges,
                        &records,
                        &ledger,
                        settings.shared_budget_bytes,
                        &buddy_engine.inflight,
                        &|cid| backoff.get(cid).is_some_and(|(_, at)| *at > now_ms),
                    )
                };
                // (4) Releases + evictions + attributions (ledger first,
                // physical unpins after the guard drops).
                let mut contribution_changed = false;
                let mut to_unpin: Vec<String> = Vec::new();
                {
                    let mut ledger = storage_ledger
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    for buddy in &plan.release_buddies {
                        to_unpin.extend(ledger.release_buddy(buddy));
                        contribution_changed = true;
                    }
                    for (buddy, cid_hex) in &plan.release {
                        if matches!(
                            ledger.release(buddy, cid_hex),
                            crate::storage_ledger::ReleaseOutcome::LastReference
                        ) {
                            to_unpin.push(cid_hex.clone());
                        }
                        contribution_changed = true;
                    }
                    if let Some(target) = plan.evict_to {
                        to_unpin.extend(ledger.evict_newest_first(target));
                        contribution_changed = true;
                    }
                    for (buddy, cid_hex, size) in &plan.attribute_only {
                        if ledger.record_pin(buddy, cid_hex, *size, crate::wall_clock_ms()) {
                            contribution_changed = true;
                        }
                    }
                    // PR #449 review (CodeRabbit): an attribute_only above
                    // can re-reference a cid that a release just queued for
                    // unpinning (release for pact A + attribution for pact
                    // B in one plan). Physically unpin only what the FINAL
                    // ledger state no longer claims anywhere.
                    to_unpin.retain(|cid_hex| ledger.held_anywhere(cid_hex).is_none());
                }
                for cid_hex in to_unpin {
                    let Some(cid_bytes) = hex::decode(&cid_hex)
                        .ok()
                        .and_then(|v| <[u8; 32]>::try_from(v).ok())
                    else {
                        continue;
                    };
                    // Release BUDDY ownership only. If the user manually
                    // pinned the same root, the physical pin stays (PR
                    // #449 review, CodeRabbit) — pin_intent is theirs.
                    buddy_engine.buddy_pins.remove(&cid_bytes);
                    if pin_intent.contains(&cid_bytes) {
                        continue;
                    }
                    let root = ContentId::from_bytes(cid_bytes);
                    let doomed = collect_descendants(runtime.storage_tier().cache(), root);
                    let protective: std::collections::HashSet<[u8; 32]> = pin_intent
                        .union(&buddy_engine.buddy_pins)
                        .copied()
                        .collect();
                    let keep = compute_keep_set(
                        runtime.storage_tier().cache(),
                        &protective,
                        doomed.len(),
                    );
                    for id in doomed {
                        if !keep.contains(&id) {
                            runtime.unpin_content(&id);
                        }
                    }
                }
                // (5) Spawn fetches, bounded; reservations are already in
                // the plan's arithmetic and recorded here before spawn.
                let capacity =
                    BUDDY_FETCH_MAX_INFLIGHT.saturating_sub(buddy_engine.inflight.len());
                if capacity > 0 && !plan.fetch.is_empty() {
                    // Defensive per-fetch byte ceiling: global headroom at
                    // reservation time (authoritative check happens at
                    // completion with ACTUAL sizes).
                    let mut headroom = {
                        let ledger = storage_ledger
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        let settings = storage_settings
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        let reserved: u64 =
                            buddy_engine.inflight.values().map(|f| f.claimed).sum();
                        settings
                            .shared_budget_bytes
                            .saturating_sub(ledger.distinct_pinned_bytes())
                            .saturating_sub(reserved)
                    };
                    for cand in plan.fetch.into_iter().take(capacity) {
                        let Some(cid_bytes) = hex::decode(&cand.cid)
                            .ok()
                            .and_then(|v| <[u8; 32]>::try_from(v).ok())
                        else {
                            continue; // ingest-validated; defensive only
                        };
                        // PR #449 review (Qodo): cap each fetch near its
                        // CLAIMED size (2× slack for bundle overhead /
                        // benign drift, 64 KiB floor), still bounded by
                        // global headroom — an under-claiming record must
                        // not burn the whole remaining budget on one
                        // download that gets dropped at completion.
                        let per_fetch_cap = cand
                            .claimed
                            .saturating_mul(2)
                            .max(64 * 1024)
                            .min(headroom);
                        let max = usize::try_from(per_fetch_cap).unwrap_or(usize::MAX);
                        headroom = headroom.saturating_sub(cand.claimed);
                        buddy_engine.inflight.insert(
                            cand.cid.clone(),
                            crate::buddy_pin_planner::InflightFetch {
                                buddy: cand.buddy.clone(),
                                claimed: cand.claimed,
                            },
                        );
                        let session = session.clone();
                        let cas_op_tx_for_buddy = cas_op_tx.clone();
                        let tx = buddy_fetch_tx.clone();
                        tokio::spawn(async move {
                            let fetch_one = move |cid: ContentId| {
                                let session = session.clone();
                                async move {
                                    let cid_hex = hex::encode(cid.to_bytes());
                                    let prefix = cid_hex.get(1..2).unwrap_or("");
                                    let key = format!("harmony/content/{prefix}/{cid_hex}");
                                    fetch_via_zenoh(&session, &key, Some(max)).await
                                }
                            };
                            // serveable: false — public durables serve
                            // freely regardless (content_cid_servable
                            // checks only the encrypted bit).
                            let fetch = wrap_fetch_one_with_admission(
                                fetch_one,
                                cas_op_tx_for_buddy,
                                false,
                            );
                            let result =
                                fetch_recursive(fetch, ContentId::from_bytes(cid_bytes), Some(max))
                                    .await
                                    .map(|bytes| bytes.len() as u64);
                            let _ = tx
                                .send(BuddyFetchResult {
                                    buddy: cand.buddy,
                                    cid: cand.cid,
                                    cid_bytes,
                                    result,
                                })
                                .await;
                        });
                    }
                }
                if contribution_changed {
                    crate::node_event_sink::emit_ser(
                        app.as_ref(),
                        "contribution-updated",
                        &serde_json::Value::Null,
                    );
                }
            }

            // ── ZEB-669 S2: buddy fetch completions ─────────────────
            Some(done) = buddy_fetch_rx.recv() => {
                buddy_engine.inflight.remove(&done.cid);
                let now_ms = start.elapsed().as_millis() as u64;
                match done.result {
                    Err(e) => {
                        let attempts = buddy_engine
                            .backoff
                            .get(&done.cid)
                            .map(|(a, _)| *a)
                            .unwrap_or(0)
                            + 1;
                        let delay = BUDDY_FETCH_BACKOFF_BASE_MS
                            .checked_shl(attempts.min(7) - 1)
                            .unwrap_or(u64::MAX)
                            .min(BUDDY_FETCH_BACKOFF_MAX_MS);
                        buddy_engine
                            .backoff
                            .insert(done.cid.clone(), (attempts, now_ms + delay));
                        tracing::debug!(cid = %done.cid, error = %e, attempts, "buddy fetch failed; backing off");
                    }
                    Ok(actual) => {
                        // Serialized reconcile at ACTUAL size, revalidating
                        // everything that may have moved while the fetch
                        // ran (PR #449 review, CodeRabbit): the pact must
                        // still be mutual, the entry still wanted, the
                        // per-buddy slice must fit the ACTUAL (claims are
                        // hints), and the shared budget must fit. On any
                        // failure the content stays admitted-unpinned
                        // (evictable) and never enters the ledger — the
                        // honesty rule. Backoff prevents replan thrash
                        // when claimed < actual would re-propose it.
                        let admit_err: Option<&'static str> = {
                            let records = storage_records
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            let ledger = storage_ledger
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            let settings = storage_settings
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            let pledge = settings.my_pledges.get(&done.buddy).copied();
                            let still_mutual = pledge.is_some()
                                && records
                                    .pledge_list(&done.buddy)
                                    .is_some_and(|r| {
                                        r.pledges.iter().any(|p| p.to == own_owner_addr)
                                    });
                            let still_wanted = records
                                .backup_set(&done.buddy)
                                .is_some_and(|s| s.entries.iter().any(|e| e.cid == done.cid));
                            let slice_fits = pledge.is_some_and(|p| {
                                ledger
                                    .bytes_for_buddy(&done.buddy)
                                    .saturating_add(actual)
                                    <= p
                            });
                            let reserved: u64 =
                                buddy_engine.inflight.values().map(|f| f.claimed).sum();
                            let budget_fits = ledger
                                .distinct_pinned_bytes()
                                .saturating_add(reserved)
                                .saturating_add(actual)
                                <= settings.shared_budget_bytes;
                            if !still_mutual {
                                Some("pact withdrawn while fetching")
                            } else if !still_wanted {
                                Some("entry removed from backup set while fetching")
                            } else if !slice_fits {
                                Some("actual size exceeds the per-buddy pledge slice")
                            } else if !budget_fits {
                                Some("actual size exceeds remaining shared budget")
                            } else {
                                None
                            }
                        };
                        if let Some(reason) = admit_err {
                            tracing::info!(
                                cid = %done.cid, actual, reason,
                                "buddy fetch completed but not admitted; skipping pin"
                            );
                            buddy_engine.backoff.insert(
                                done.cid.clone(),
                                (1, now_ms + BUDDY_FETCH_BACKOFF_BASE_MS),
                            );
                        } else {
                            // Mirror the ContentVerbRequest::Pin arm, but
                            // record BUDDY ownership (never pin_intent —
                            // that set is the user's manual intent).
                            buddy_engine.buddy_pins.insert(done.cid_bytes);
                            let root = ContentId::from_bytes(done.cid_bytes);
                            let all =
                                collect_descendants(runtime.storage_tier().cache(), root);
                            let mut any_failed = false;
                            for id in all {
                                if !runtime.pin_content(id) {
                                    any_failed = true;
                                }
                            }
                            if any_failed {
                                // Pin-count quota exhausted: undo (keep-set
                                // protected) and back off — the pact reads
                                // Catching up rather than half-pinning.
                                buddy_engine.buddy_pins.remove(&done.cid_bytes);
                                let doomed =
                                    collect_descendants(runtime.storage_tier().cache(), root);
                                let protective: std::collections::HashSet<[u8; 32]> =
                                    pin_intent
                                        .union(&buddy_engine.buddy_pins)
                                        .copied()
                                        .collect();
                                let keep = compute_keep_set(
                                    runtime.storage_tier().cache(),
                                    &protective,
                                    doomed.len(),
                                );
                                for id in doomed {
                                    // keep-set is built from the union, so
                                    // a manually-pinned root's subtree is
                                    // already protected.
                                    if !keep.contains(&id) {
                                        runtime.unpin_content(&id);
                                    }
                                }
                                buddy_engine.backoff.insert(
                                    done.cid.clone(),
                                    (1, now_ms + BUDDY_FETCH_BACKOFF_BASE_MS),
                                );
                                tracing::warn!(
                                    cid = %done.cid,
                                    "buddy pin failed (pin quota exhausted); backing off"
                                );
                            } else {
                                buddy_engine.backoff.remove(&done.cid);
                                let changed = storage_ledger
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .record_pin(
                                        &done.buddy,
                                        &done.cid,
                                        actual,
                                        crate::wall_clock_ms(),
                                    );
                                if changed {
                                    crate::node_event_sink::emit_ser(
                                        app.as_ref(),
                                        "contribution-updated",
                                        &serde_json::Value::Null,
                                    );
                                }
                            }
                        }
                    }
                }
            }

            _ = voice_sweep_tick.tick(),
                if !voice_keys.is_empty() || !groupdm_presence_subs.is_empty() => {
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
                        crate::node_event_sink::emit_ser(app.as_ref(), "voice-presence-changed", &serde_json::json!({
                                "community": hex::encode(c.0),
                                "channel": hex::encode(ch.0),
                                "roster": roster,
                            }));
                    }
                }
                // ── ZEB-360: group-DM presence crash-eviction ─────────────
                // Sweep the dedicated group map with the SAME `now` + TTL, then
                // re-emit each affected (space, call) roster as a group event so
                // a crashed participant (no `left` tombstone) drops to ghost like
                // community voice. Graceful leaves already publish a tombstone.
                let swept_group = {
                    let mut g = groupdm_presence_map.lock().await;
                    g.sweep(now, VOICE_PRESENCE_TTL_MS)
                };
                if !swept_group.is_empty() {
                    // Dedup the (space, call) keys; emit each once.
                    let mut seen: std::collections::HashSet<(
                        crate::owner_state_types::SpaceId,
                        crate::community_membership::ChannelId,
                    )> = std::collections::HashSet::new();
                    for ((sp, call), _owner, _device) in &swept_group {
                        seen.insert((*sp, *call));
                    }
                    for (sp, call) in seen {
                        let roster = {
                            let g = groupdm_presence_map.lock().await;
                            g.roster(&sp, &call)
                        };
                        crate::node_event_sink::emit_ser(app.as_ref(), "group-call-presence-changed", &serde_json::json!({
                                "spaceId": hex::encode(sp.0),
                                "callId": hex::encode(call.0),
                                "roster": roster,
                            }));
                    }
                }
                // ZEB-358: lapse moderation directives past TTL, re-emit overlay.
                let now2 = (voice_now_ms)();
                let mod_changed = {
                    let mut g = voice_moderation_map.lock().await;
                    g.sweep(now2)
                };
                // ZEB-358 (Qodo perf): refresh the hot-path moderation-active flag
                // after the sweep (a lapse can flip the last enforcement off).
                voice_moderation_active.store(
                    {
                        let g = voice_moderation_map.lock().await;
                        g.any_enforced(now2)
                    },
                    std::sync::atomic::Ordering::Relaxed,
                );
                if let Some(registry) = community_registry.clone() {
                    for (c, ch) in mod_changed {
                        // self_owner for this channel from voice_identity.
                        if let Some((self_owner, _d, _h, _k)) =
                            voice_identity.get(&(c, ch))
                        {
                            // ZEB-358 (Cursor HIGH): a sweep that lapses our own
                            // kick must clear the self-kicked flag so the presence
                            // publisher resumes beaconing.
                            if let Some(flag) = voice_self_kicked_flags.get(&(c, ch)) {
                                let kicked = {
                                    let g = voice_moderation_map.lock().await;
                                    g.is_kicked(&c, &ch, &self_owner.0, now2)
                                };
                                flag.store(kicked, std::sync::atomic::Ordering::Relaxed);
                            }
                            emit_moderation_changed(
                                &app,
                                &registry,
                                &voice_presence_map,
                                &voice_moderation_map,
                                c,
                                ch,
                                *self_owner,
                                now2,
                            )
                            .await;
                        }
                    }
                    // Re-assert issuer directives still within their window; drop
                    // expired ones.
                    let mut expired: Vec<_> = Vec::new();
                    for (idkey, (signed, stop_after)) in voice_issuer_directives.iter() {
                        if now2 >= *stop_after {
                            expired.push(*idkey);
                            continue;
                        }
                        let (c, ch, _t, _m) = idkey;
                        if let Some(key) = voice_keys.get(&(*c, *ch)) {
                            if let Ok(sealed) = crate::voice_moderation::seal_directive(
                                key, c, ch, signed,
                            ) {
                                let topic = format!(
                                    "harmony/voice-control/{}/{}",
                                    hex::encode(c.0),
                                    hex::encode(ch.0),
                                );
                                let _ = session.put(&topic, sealed).await;
                            }
                        }
                    }
                    for k in expired {
                        voice_issuer_directives.remove(&k);
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
                    req.root_serve_tx,
                    req.fetch_request_rx,
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
                    req.community_id,
                    req.crdt_state,
                    req.publisher_rx,
                    req.subscriber_tx,
                    req.read_for_backfill,
                    req.apply_backfilled,
                    req.backfill_interval,
                    req.rbsr_hooks,
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
                    req.rbsr_hooks,
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
                dispatch_action(action, &session, &zenoh_tx, &app, &closing, &own_zid).await;
            }
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
    // ZEB-358: abort the per-channel moderation control subscriber tasks too, so
    // they don't run detached past shutdown (CodeRabbit).
    for (_, handle) in voice_control_subs.drain() {
        handle.abort();
    }
    // ZEB-352: abort the DM-call media subscriber tasks too, so they don't run
    // detached past shutdown (emitting into a stale AppHandle or racing a
    // subsequent start_node restart that builds fresh state maps).
    for (_, handle) in dm_voice_subs.drain() {
        handle.abort();
    }
    // ZEB-360: abort the group-DM presence subscriber/publisher tasks too, for the
    // same reason — the group subscriber can emit `group-call-presence-changed`
    // into a stale AppHandle in the closing→session-close window, and a leftover
    // publisher would keep putting beacons past shutdown.
    for (_, handle) in groupdm_presence_subs.drain() {
        handle.abort();
    }
    for (_, handle) in groupdm_presence_pubs.drain() {
        handle.abort();
    }
    // ZEB-352: abort the always-on voice-signal subscriber too, so it can't emit
    // signaling events into a stale AppHandle during the closing→session-close
    // window or race a subsequent start_node restart.
    if let Some(handle) = voice_signal_sub_handle {
        handle.abort();
    }
    // ZEB-620: abort the reconnect-supervisor loop too — it never exits on its
    // own, and left running it would keep dialing peers (and holding the zenoh
    // runtime) after the event loop returns.
    if let Some(handle) = reconnect_supervisor_task {
        handle.abort();
    }
    // ZEB-928: stop the R4 admission controller's infinite poll loop too (CodeRabbit, PR #674).
    if let Some(handle) = admission_controller_task {
        handle.abort();
    }
    let _ = session.close().await;
    // ZEB-468: `session.close()` does NOT close an *adopted* runtime. We build the
    // session via `zenoh::session::init(DynamicRuntime)` (open_session_with_runtime),
    // so its `static_runtime` is None and `Session::close_inner` only sends a session
    // face-close — it never calls the Runtime's `manager.close()`. Across a restart
    // (mint, or app quit) that leaks every transport + TCP listener:
    //   • the peer keeps our old (identity-stable, ZEB-390) zid's face → the restarted
    //     session's re-declarations are rejected "Resource remapped. Remapping
    //     unsupported!" → the Zenoh pub/sub mesh never re-forms (cards don't route);
    //   • the orphaned TCP accept loop hot-spins on a shutting-down Tokio runtime
    //     ("…being shutdown…"), burning CPU until the process exits.
    // Close the runtime ourselves: `Runtime::close()` runs `terminate_all_async()`
    // (cancels the accept loops) + `manager.close()` (closes transports → the peer
    // drops our face). Bounded by a timeout so a stalled transport-close can never
    // wedge a mint/restart; on timeout we proceed (the thread runtime drops anyway —
    // strictly better than today's no-close).
    const ZENOH_RUNTIME_CLOSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
    match tokio::time::timeout(ZENOH_RUNTIME_CLOSE_TIMEOUT, zenoh_runtime.close()).await {
        Ok(Ok(())) => tracing::info!("zenoh runtime closed"),
        Ok(Err(e)) => tracing::warn!("zenoh runtime close error: {e}"),
        Err(_) => tracing::warn!(
            "zenoh runtime close timed out after {ZENOH_RUNTIME_CLOSE_TIMEOUT:?}; \
             proceeding with teardown"
        ),
    }
    tracing::info!("event loop stopped");
}

/// ZEB-352: map a verified inbound `VoiceSignal` to the matching frontend
/// event and emit it. The call state machine lives in the frontend; this
/// only translates the transport-level signal into a Tauri event with the
/// hex-encoded call-ID (and caller / decline reason where applicable).
fn emit_voice_signal_event(
    app: &std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>,
    signal: &crate::voice_signal::VoiceSignal,
    space_id_hex: Option<&str>,
) {
    use crate::voice_signal::{DeclineReason, VoiceSignalKind};
    let call_hex = hex::encode(signal.call_id);
    match signal.kind {
        VoiceSignalKind::Invite => {
            crate::node_event_sink::emit_ser(
                app.as_ref(),
                "incoming-call",
                &serde_json::json!({
                    "callId": call_hex,
                    "callerOwner": hex::encode(signal.caller.0),
                    "spaceId": space_id_hex,
                }),
            );
        }
        VoiceSignalKind::Accept => {
            crate::node_event_sink::emit_ser(
                app.as_ref(),
                "call-accepted",
                &serde_json::json!({ "callId": call_hex }),
            );
        }
        VoiceSignalKind::Decline => {
            let reason = match signal.decline_reason {
                Some(DeclineReason::User) | None => "user",
                Some(DeclineReason::Busy) => "busy",
                Some(DeclineReason::Timeout) => "timeout",
            };
            crate::node_event_sink::emit_ser(
                app.as_ref(),
                "call-declined",
                &serde_json::json!({ "callId": call_hex, "reason": reason }),
            );
        }
        VoiceSignalKind::Cancel => {
            crate::node_event_sink::emit_ser(
                app.as_ref(),
                "call-canceled",
                &serde_json::json!({ "callId": call_hex }),
            );
        }
        VoiceSignalKind::End => {
            crate::node_event_sink::emit_ser(
                app.as_ref(),
                "call-ended",
                &serde_json::json!({ "callId": call_hex }),
            );
        }
    }
}

/// ZEB-360: emit the frontend event for a verified inbound *group* voice signal.
/// Only `Invite`/`Decline` carry to the UI (group calls follow a drop-in model:
/// no Accept/Cancel/End signals are sent). `space_id_hex` names the GroupDm space
/// so the banner/roster can scope correctly. Mirrors [`emit_voice_signal_event`].
fn emit_group_voice_signal_event(
    app: &std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>,
    signal: &crate::voice_signal::VoiceSignal,
    space_id_hex: &str,
) {
    use crate::voice_signal::VoiceSignalKind;
    let call_hex = hex::encode(signal.call_id);
    match signal.kind {
        VoiceSignalKind::Invite => {
            crate::node_event_sink::emit_ser(
                app.as_ref(),
                "incoming-group-call",
                &serde_json::json!({
                    "callId": call_hex,
                    "callerOwner": hex::encode(signal.caller.0),
                    "spaceId": space_id_hex,
                }),
            );
        }
        VoiceSignalKind::Decline => {
            // `caller` on a Decline is the decliner (responder).
            crate::node_event_sink::emit_ser(
                app.as_ref(),
                "group-call-declined",
                &serde_json::json!({
                    "callId": call_hex,
                    "spaceId": space_id_hex,
                    "owner": hex::encode(signal.caller.0),
                }),
            );
        }
        // Drop-in model: no group Accept/Cancel/End signals are sent.
        _ => {}
    }
}

/// Which publishes carry our ZenohId as a sample attachment. Capacity
/// beacons need it for hop-distance inference; content announcements
/// (ZEB-669 slice 1) need it so receivers can attribute the announcing
/// session — `ObservedHolders` reads the attachment, and without it the
/// ×N "copies seen" counter never counts real peers. The zid is a
/// transport-session id, not an owner identity: announcements stay
/// anonymous (ZEB-669 §0.2 hybrid attribution).
fn publish_attaches_zid(key_expr: &str) -> bool {
    key_expr.starts_with(crate::CAPACITY_PREFIX) || key_expr.starts_with(crate::ANNOUNCE_PREFIX)
}

/// ZEB-612 S3 receive path, extracted for testability (ZEB-669 S1):
/// record a distinct announcing session per CID. Own announcements loop
/// back on the local session — exclude `own_zid` so `replicaCount = 1
/// (self) + peers` doesn't double-count. Samples without source info
/// can't be attributed and are skipped (the count is an observed lower
/// bound); announce publishes attach the zid (`publish_attaches_zid`),
/// so real peer announces are attributable from this build onward.
/// `now_ms` is lazy (PR #448 review, Qodo): the receive loop is a hot
/// path and every non-announce sample would otherwise pay an
/// `Instant::elapsed` it never uses — the clock is read only when a
/// sample actually lands in the holder map.
fn note_announce_sample(
    observed_holders: &Arc<std::sync::Mutex<crate::observed_holders::ObservedHolders>>,
    key_expr: &str,
    payload: &[u8],
    source_zid: Option<&str>,
    own_zid: &str,
    now_ms: impl FnOnce() -> u64,
) {
    if !key_expr.starts_with(crate::ANNOUNCE_PREFIX) {
        return;
    }
    if let (Some(zid), Some(a)) = (
        source_zid,
        crate::parse_content_announcement(key_expr, payload),
    ) {
        if zid != own_zid {
            // Poison-resilient: the holder map is a best-effort cache —
            // keep serving it rather than re-panicking the loop.
            observed_holders
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .note(&a.cid, zid, now_ms());
        }
    }
}

/// ZEB-669 S2: auto-pin engine cadence (reconcile pacts every 30 s).
pub(crate) const BUDDY_SYNC_INTERVAL_MS: u64 = 30_000;
/// Bounded concurrent buddy fetches — network fetches are spawned, so
/// this caps memory/bandwidth, not loop latency.
const BUDDY_FETCH_MAX_INFLIGHT: usize = 4;
/// Exponential fetch backoff: base × 2^attempts, capped at one hour.
const BUDDY_FETCH_BACKOFF_BASE_MS: u64 = 60_000;
const BUDDY_FETCH_BACKOFF_MAX_MS: u64 = 3_600_000;

/// Outcome of one spawned buddy fetch task.
struct BuddyFetchResult {
    buddy: String,
    cid: String,
    cid_bytes: [u8; 32],
    /// Ok(actual assembled bytes) — the value the ledger records.
    result: Result<u64, String>,
}

/// Loop-local auto-pin engine state. Living inside the single-threaded
/// select loop is what makes budget admission SERIALIZED (spec §3, PR
/// #448 review): reservations are taken and reconciled on one thread,
/// so concurrent fetches can never observe stale remaining-budget.
#[derive(Default)]
struct BuddyEngineState {
    /// cid-hex → reservation, while a fetch task runs.
    inflight: std::collections::HashMap<String, crate::buddy_pin_planner::InflightFetch>,
    /// cid-hex → (attempts, loop-relative retry-at ms).
    backoff: std::collections::HashMap<String, (u32, u64)>,
    /// Roots pinned ON BEHALF OF BUDDIES — kept separate from the
    /// manual `pin_intent` (PR #449 review, CodeRabbit): a buddy release
    /// must never erase the user's own pin intent, and a manual unpin
    /// must never physically unpin a root the ledger still claims.
    /// Physical unpin happens only when NEITHER set holds the root.
    buddy_pins: std::collections::HashSet<[u8; 32]>,
    /// First-tick boot honesty sweep done?
    booted: bool,
}

/// ZEB-669 S2: route a `harmony/storage/*` sample into the record store.
/// Returns true when the store changed (`Inserted | UpdatedNewer`) so the
/// caller emits `storage-buddies-updated` only on real change. Rejected
/// records log at debug (zero state effect, like follow lists); non-store
/// keys return false without taking the lock. `now_ms` is lazy — only a
/// hosting sample (which stamps a receipt clock) reads it.
fn note_storage_record_sample(
    storage_records: &Arc<std::sync::Mutex<crate::storage_records::StorageRecordStore>>,
    key_expr: &str,
    payload: &[u8],
    revoked: &crate::revoked_device_projection::RevokedDeviceProjection,
    now_ms: impl FnOnce() -> u64,
) -> bool {
    use crate::storage_records::RecordOutcome;
    let Some(rest) = key_expr.strip_prefix(crate::STORAGE_RECORD_PREFIX) else {
        return false;
    };
    let Some((_owner, kind)) = rest.split_once('/') else {
        return false;
    };
    if !matches!(kind, "pledges" | "backup-set" | "hosting") {
        return false;
    }
    let mut store = storage_records
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Wall clock evaluated once we KNOW this is a storage record (the
    // lazy closure keeps non-storage keys clock-free); all three
    // families need it now — cert expiry + pin stamping (ZEB-679), and
    // the hosting receipt clock.
    let now = now_ms();
    let outcome = match kind {
        "pledges" => store.on_pledge_list_sample(key_expr, payload, revoked, now),
        "backup-set" => store.on_backup_set_sample(key_expr, payload, revoked, now),
        "hosting" => store.on_hosting_report_sample(key_expr, payload, revoked, now),
        _ => return false,
    };
    match &outcome {
        RecordOutcome::Rejected(reason) => {
            tracing::debug!(key = %key_expr, %reason, "storage record rejected");
        }
        // ZEB-869: a valid newcomer dropped by freeze-when-full is a
        // benign, self-healing no-op (like an honest skewed-clock reject,
        // ZEB-855) — log at debug so the silent drop is diagnosable, but
        // do NOT emit `storage-buddies-updated` (`.changed()` is false).
        RecordOutcome::IgnoredAtCap => {
            tracing::debug!(
                key = %key_expr,
                cap = crate::storage_records::MAX_TRACKED_OWNERS,
                "storage record dropped at cap: family full, newcomer self-evicted (freeze-when-full)"
            );
        }
        _ => {}
    }
    outcome.changed()
}

/// Dispatch a single RuntimeAction to the platform I/O layer.
async fn dispatch_action(
    action: RuntimeAction,
    session: &zenoh::Session,
    zenoh_tx: &mpsc::Sender<ZenohEvent>,
    app: &std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>,
    closing: &Arc<AtomicBool>,
    own_zid: &str,
) {
    match action {
        // ── Zenoh: publish ───────────────────────────────────────────
        RuntimeAction::Publish { key_expr, payload } => {
            let session = session.clone();
            // Attach our ZenohId where receivers attribute the publisher:
            // capacity beacons (hop distance) and content announcements
            // (observed holders). See `publish_attaches_zid`.
            let zid_attachment = if publish_attaches_zid(&key_expr) {
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
                let result = fetch_via_zenoh(&session, &key_expr, None).await;
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
                let result = fetch_via_zenoh(&session, &key_expr, None).await;
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

/// ZEB-409: report whether a single leaf's payload exceeds the per-fetch
/// ceiling, returning `Some(cap)` when it does (for the error message) and
/// `None` otherwise. `max_bytes == None` is unbounded. Pure so the threshold is
/// unit-testable without a Zenoh session — same philosophy as the frontend's
/// pure `assertDecodedDimsOk`. Allows `len == cap` (rejects only strictly over),
/// mirroring `fetch_recursive`'s assembled-total check.
fn leaf_cap_exceeded(payload_len: usize, max_bytes: Option<usize>) -> Option<usize> {
    match max_bytes {
        Some(cap) if payload_len > cap => Some(cap),
        _ => None,
    }
}

/// ZEB-884: decode → verify → attribute → cache → emit one received owner card.
/// Shared by the live-PUT subscriber arm and the query-on-subscribe fast path so
/// both converge through ONE verify/attribution/cache/emit implementation.
/// `bytes` is a single card payload; oversized / undecodable / unverifiable /
/// misattributed inputs are dropped (logged), never cached. The oversize guard is
/// duplicated here (the PUT arm also pre-checks the un-materialized view) so this
/// is self-contained and safe for any caller.
async fn ingest_card_bytes(
    bytes: &[u8],
    subscription_id: crate::profile_broadcast::SubscriptionId,
    owner_id: [u8; 16],
    cache: &crate::profile_card_broadcast::ProfileCardCache,
    app: &dyn crate::node_event_sink::NodeEventSink,
) {
    if bytes.len() > crate::profile_card_broadcast::MAX_CARD_WIRE_BYTES {
        tracing::warn!(
            size = bytes.len(),
            max = crate::profile_card_broadcast::MAX_CARD_WIRE_BYTES,
            subscription_id,
            "oversized profile card dropped"
        );
        return;
    }
    let card: crate::profile_card_broadcast::ProfileCardBroadcast =
        match ciborium::from_reader(bytes) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = ?e, subscription_id, "profile card CBOR decode failed");
                return;
            }
        };
    // Cert model + signature.
    let verified_owner = match crate::profile_card_broadcast::verify_card(
        &card,
        crate::iroh_friend_acceptor::wall_now_secs(),
    ) {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(error = ?e, subscription_id, "profile card verify failed");
            return;
        }
    };
    // Attribution: bind the verified owner to the SUBSCRIBED owner (the topic's owner).
    if verified_owner != owner_id {
        tracing::warn!(subscription_id, "profile card attribution mismatch");
        return;
    }
    cache.insert_verified(subscription_id, &card).await;
    if let Some(info) = cache.get_cached(subscription_id).await {
        // Flat payload (subscriptionId + DiscoveredCardInfo fields hoisted).
        crate::node_event_sink::emit_ser(
            app,
            "member-card-received",
            &serde_json::json!({
                "subscriptionId": subscription_id,
                "ownerIdHex": info.owner_id_hex,
                "displayName": info.display_name,
                "statusText": info.status_text,
                "avatarCid": info.avatar_cid,
                "profilePageRoot": info.profile_page_root,
            }),
        );
    }
}

#[cfg(test)]
mod ingest_card_bytes_tests {
    use super::*;

    fn signed_card(tag: u8, name: &str) -> ([u8; 16], Vec<u8>) {
        let owner = crate::community_membership::mint_test_owner(tag);
        let card = crate::profile_card_broadcast::sign_card(
            &owner.device_key,
            owner.owner.0,
            name.into(),
            String::new(),
            None,
            None,
            owner.cert.clone(),
            crate::owner_state_types::Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "d".into(),
            },
        )
        .unwrap();
        let bytes = crate::owner_state_crypto::canonical_cbor_encode(&card).unwrap();
        (owner.owner.0, bytes)
    }

    /// A valid card on a matching subscription is cached and emits
    /// `member-card-received` — the shared pipeline the query path also uses.
    #[tokio::test]
    async fn valid_card_is_cached_and_emitted() {
        let (owner_id, bytes) = signed_card(0x90, "Ada");
        let cache = crate::profile_card_broadcast::ProfileCardCache::default();
        let sink = crate::node_event_sink::RecordingSink::new();
        cache.register(7, owner_id).await;
        ingest_card_bytes(&bytes, 7, owner_id, &cache, &sink).await;
        let info = cache.get_cached(7).await.expect("card cached after ingest");
        assert_eq!(info.display_name, "Ada");
        assert!(
            sink.frames()
                .iter()
                .any(|(e, p)| e == "member-card-received" && p["displayName"] == "Ada"),
            "member-card-received emitted with the resolved name"
        );
    }

    /// A card whose verified owner != the subscribed owner is dropped (never
    /// cached, no event) — the attribution guard, shared by both ingest paths.
    #[tokio::test]
    async fn attribution_mismatch_is_rejected() {
        let (owner_a, bytes_a) = signed_card(0x91, "Ada");
        let (owner_b, _bytes_b) = signed_card(0x92, "Bo");
        assert_ne!(owner_a, owner_b);
        let cache = crate::profile_card_broadcast::ProfileCardCache::default();
        let sink = crate::node_event_sink::RecordingSink::new();
        cache.register(8, owner_b).await;
        ingest_card_bytes(&bytes_a, 8, owner_b, &cache, &sink).await;
        assert!(cache.get_cached(8).await.is_none());
        assert!(sink.frames().is_empty());
    }

    /// An oversized payload is dropped before any decode/verify work.
    #[tokio::test]
    async fn oversized_is_dropped_before_decode() {
        let cache = crate::profile_card_broadcast::ProfileCardCache::default();
        let sink = crate::node_event_sink::RecordingSink::new();
        let big = vec![0u8; crate::profile_card_broadcast::MAX_CARD_WIRE_BYTES + 1];
        cache.register(9, [0u8; 16]).await;
        ingest_card_bytes(&big, 9, [0u8; 16], &cache, &sink).await;
        assert!(cache.get_cached(9).await.is_none());
        assert!(sink.frames().is_empty());
    }
}

/// Fetch content via Zenoh get() with a 30s timeout.
///
/// ZEB-409: `max_bytes` bounds a single leaf's payload. When set, an oversized
/// leaf is rejected from its declared `ZBytes::len()` BEFORE the contiguous
/// `.to_vec()` copy (and before `fetch_recursive`'s assembled-total extend), so
/// a hostile peer serving one large content-addressed leaf can't force the extra
/// Rust-side materialization. `None` = unbounded (every caller except the avatar
/// content-fetch path).
async fn fetch_via_zenoh(
    session: &zenoh::Session,
    key_expr: &str,
    max_bytes: Option<usize>,
) -> Result<Vec<u8>, String> {
    let replies = session
        .get(key_expr)
        .await
        .map_err(|e| format!("zenoh get error: {e}"))?;

    let deadline = Duration::from_secs(30);
    tokio::time::timeout(deadline, async {
        while let Ok(reply) = replies.recv_async().await {
            match reply.result() {
                Ok(sample) => {
                    let payload = sample.payload();
                    let len = payload.len();
                    if let Some(cap) = leaf_cap_exceeded(len, max_bytes) {
                        return Err(format!(
                            "leaf '{key_expr}' payload {len} exceeds max_bytes cap {cap}"
                        ));
                    }
                    return Ok(payload.to_bytes().to_vec());
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
fn emit_session_lost(
    app: &std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>,
    reason: &str,
) {
    crate::node_event_sink::emit_ser(
        app.as_ref(),
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

/// Async sibling of [`collect_descendants`] for callers that don't hold a
/// `&ContentStore` (the event loop owns it exclusively): walk the local DAG
/// rooted at `root` by reading interior *bundle* nodes via read-only
/// `CasOp::GetLocal` round-trips on `cas_op_tx`. Returns root + every
/// descendant in DFS order, or `Err` if the root itself is missing locally.
///
/// `GetLocal` never triggers a network fetch (spec: it answers from the
/// in-memory cache only), so this is safe to spawn off the event-loop
/// `select!` arm — it can't recurse into a fetch or invert the serve
/// relationship. Used by the `AllowServeSubtree` handler so the walk +
/// allowlist run in a spawned task instead of inline in the loop. The
/// `ContentStore` is not `Clone + Send + 'static`, so it can't be moved into
/// a `spawn_blocking`; this channel-round-trip walk is the off-loop path.
///
/// Mirrors `collect_descendants`'s DFS/depth-guard/bundle-parse, with two
/// adaptations for the channel-round-trip node source: the fetched root payload
/// is threaded into the walk so a Bundle root isn't fetched twice, and CIDs are
/// deduplicated *during* traversal (each node here is a GetLocal round-trip, so
/// revisiting shared subtrees is far costlier than in the sync `&store.get`
/// version). Returns a duplicate-free CID list.
async fn collect_descendants_via_cas(
    cas_op_tx: &tokio::sync::mpsc::Sender<crate::content_store::CasOp>,
    root: ContentId,
) -> Result<Vec<ContentId>, crate::content_store::ContentStoreError> {
    use crate::content_store::{CasOp, ContentStoreError};
    use harmony_content::cid::MAX_BUNDLE_DEPTH;

    // Read-only local-cache lookup via the event loop. `None` => not local.
    async fn get_local(
        tx: &tokio::sync::mpsc::Sender<CasOp>,
        cid: ContentId,
    ) -> Result<Option<Vec<u8>>, ContentStoreError> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        tx.send(CasOp::GetLocal { cid, reply })
            .await
            .map_err(|_| ContentStoreError::Io("cas_op channel closed".to_string()))?;
        rx.await
            .map_err(|_| ContentStoreError::Io("GetLocal reply dropped".to_string()))
    }

    // Root-missing invariant: refuse rather than allowlist a partial tree.
    // Keep the fetched root bytes in hand so a Bundle root isn't fetched a
    // second time when it is popped below (Greptile P2: a multi-chunk artifact
    // would otherwise pay an extra GetLocal round-trip on every allowlisting).
    let root_bytes = get_local(cas_op_tx, root).await?.ok_or_else(|| {
        ContentStoreError::Io(format!(
            "root {} missing locally; cannot authorize subtree",
            hex::encode(root.to_bytes())
        ))
    })?;

    let mut out = Vec::new();
    // Traversal-time dedup (CodeRabbit, Major): a crafted bundle DAG can aim
    // many edges at the same shared subtree. Skipping already-seen CIDs *before*
    // fetching/parsing their children bounds the GetLocal round-trips + bundle
    // parsing to O(unique nodes) instead of O(edges), and also backstops cycles.
    // Because every CID pushed to `out` is unique, the caller needs no post-hoc
    // dedup pass.
    let mut seen: std::collections::HashSet<ContentId> = std::collections::HashSet::new();
    // The stack carries optional bytes so the root's already-fetched payload is
    // reused; interior nodes carry `None` and are fetched on demand.
    let mut stack: Vec<(ContentId, Option<Vec<u8>>, u8)> = vec![(root, Some(root_bytes), 0)];
    while let Some((id, maybe_bytes, depth)) = stack.pop() {
        if depth > MAX_BUNDLE_DEPTH {
            tracing::warn!(
                cid_depth = depth,
                max = MAX_BUNDLE_DEPTH,
                "collect_descendants_via_cas aborting subtree past MAX_BUNDLE_DEPTH; data is corrupt"
            );
            continue;
        }
        if !seen.insert(id) {
            continue;
        }
        out.push(id);
        if matches!(id.cid_type(), CidType::Bundle(_)) {
            // Reuse the root bytes already in hand; fetch interior bundle nodes.
            let bytes_opt = match maybe_bytes {
                Some(b) => Some(b),
                None => get_local(cas_op_tx, id).await?,
            };
            if let Some(bytes) = bytes_opt {
                match bundle::parse_bundle(&bytes) {
                    Ok(children) => {
                        for child in children.iter().copied() {
                            stack.push((child, None, depth + 1));
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
    Ok(out)
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
    max_bytes: Option<usize>,
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
            // ZEB-344: bound the assembled size so an oversized avatar_cid
            // can't force an unbounded download. ≤ cap + one chunk (a single
            // chunk is bounded by ChunkerConfig::DEFAULT). None = unbounded.
            if let Some(cap) = max_bytes {
                if out.len() > cap {
                    return Err(format!(
                        "content exceeds max_bytes cap: {} > {cap}",
                        out.len()
                    ));
                }
            }
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
    // ZEB-535: when true, each admitted CID is allowlisted for member-to-member
    // serving (re-serve encrypted artifact books). `false` for avatar/content.
    serveable: bool,
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
                    serveable,
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

/// ZEB-912: all directly-linked zids, regardless of the REMOTE session's mode.
/// zenoh's session info partitions direct links by remote whatami (`peers_zid`
/// vs `routers_zid` — an all-router mesh under HARMONY_ZENOH_MODE=router
/// reports every link in `routers_zid`, probe-verified in the R3 spike doc).
/// Hop-distance classification and ZEB-622 up-edge detection care about
/// "directly linked", not the remote's mode — reading only `peers_zid` would
/// silently blind both on router-mode runs.
async fn direct_link_zids(session: &zenoh::Session) -> std::collections::HashSet<String> {
    let info = session.info();
    let mut set: std::collections::HashSet<String> =
        info.peers_zid().await.map(|z| z.to_string()).collect();
    set.extend(info.routers_zid().await.map(|z| z.to_string()));
    set
}

/// ZEB-622: up-edge detector over zid-poll snapshots. Replaces the accumulating
/// seen-zid set (which never forgot, so a same-zid reconnect never re-armed the
/// backfill epoch). An up-edge = a zid present now that was absent in the
/// previous snapshot; `prev` is REPLACED by the current snapshot each call, so
/// a flap longer than one poll interval (~5s) re-fires. Sub-interval LAN flaps
/// can be missed here — iroh peers are covered event-driven by peer_liveness.
fn detect_up_edges(prev: &mut std::collections::HashSet<String>, current: Vec<String>) -> bool {
    // Borrow to compute the up-edge, THEN move `current` into `prev` — no
    // per-poll string clones (the old `current.iter().cloned().collect()`
    // allocated a fresh copy of every zid each ~5s poll).
    let any_new = current.iter().any(|z| !prev.contains(z));
    *prev = current.into_iter().collect();
    any_new
}

/// ZEB-702 T3 (Component B): re-offer every owner-scoped dataset root on a
/// transport up-edge. Zenoh `put`s are fire-and-forget and the sync engines
/// publish only on local dirty (debounced) or explicit flush — so a link that
/// forms AFTER the last publish carries nothing until the next local mutation
/// (the D3 300 s roster stall: a cert-only butler's `friend_graph` never
/// converges). This listener nudges each engine's `notify_dirty` when the
/// transport epoch advances, re-offering the CURRENT root — byte-identical
/// content, idempotent on receivers (LWW/HLC merge), coalesced by the engines'
/// own debounce.
///
/// `rx.changed()` wakes ONLY on values written after `rx` was created — the
/// initial subscribe value is NOT a change, so we never re-offer on a spurious
/// boot edge (the boot flushes already cover the first publish). It returns
/// `Err` exactly when every `Sender` has dropped (the event loop exiting), which
/// ends the task cleanly — no separate shutdown signal needed.
pub(crate) async fn run_epoch_republish(
    mut rx: watch::Receiver<u64>,
    engines: Vec<Arc<dyn crate::fleet_sync::RepublishDirty>>,
) {
    while rx.changed().await.is_ok() {
        for engine in &engines {
            engine.republish_dirty();
        }
    }
}

/// ZEB-434 D9: classify a mail-root query result for the retry latch.
/// An empty-payload reply is a VALID answer (the "no mail yet" sentinel —
/// see [`query_mail_root`]); only zero-responders / query failure retries.
fn map_mail_root_outcome(
    result: &Result<Option<Vec<u8>>, String>,
) -> crate::channel_backfill::RootFetch {
    match result {
        Ok(Some(_)) => crate::channel_backfill::RootFetch::Answered,
        Ok(None) | Err(_) => crate::channel_backfill::RootFetch::NoReply,
    }
}

#[cfg(test)]
mod mail_root_outcome_tests {
    use super::map_mail_root_outcome;
    use crate::channel_backfill::RootFetch;

    /// ZEB-434 D9: an EMPTY reply payload is the gateway's valid "no mail
    /// yet" sentinel — it must satisfy the retry latch (Answered) exactly
    /// like a real root CID. Only zero-responders / query failure retries.
    #[test]
    fn mail_root_outcome_mapping_discriminates_empty_from_none() {
        // Empty payload = valid "no mail yet" sentinel → Answered.
        assert_eq!(
            map_mail_root_outcome(&Ok(Some(vec![]))),
            RootFetch::Answered
        );
        // Real root payload → Answered.
        assert_eq!(
            map_mail_root_outcome(&Ok(Some(vec![1, 2, 3]))),
            RootFetch::Answered
        );
        // Zero responders → retry.
        assert_eq!(map_mail_root_outcome(&Ok(None)), RootFetch::NoReply);
        // Query failure → retry.
        assert_eq!(
            map_mail_root_outcome(&Err("boom".to_string())),
            RootFetch::NoReply
        );
    }
}

#[cfg(test)]
mod transport_epoch_tests {
    use super::detect_up_edges;

    /// A never-before-seen zid in the current snapshot is an up-edge.
    #[test]
    fn detect_up_edges_new_zid_fires() {
        let mut prev: std::collections::HashSet<String> = ["a".to_string()].into_iter().collect();
        assert!(detect_up_edges(
            &mut prev,
            vec!["a".to_string(), "b".to_string()]
        ));
    }

    /// An identical snapshot re-fires nothing.
    #[test]
    fn detect_up_edges_unchanged_no_fire() {
        let mut prev: std::collections::HashSet<String> =
            ["a".to_string(), "b".to_string()].into_iter().collect();
        assert!(!detect_up_edges(
            &mut prev,
            vec!["a".to_string(), "b".to_string()]
        ));
    }

    /// The regression the accumulating gate failed: a zid that drops out of the
    /// snapshot and RETURNS a poll later re-fires, because `prev` is REPLACED by
    /// each snapshot. The old seen-set never forgot, so a same-zid reconnect was
    /// silently swallowed and the backfill epoch never re-armed.
    #[test]
    fn detect_up_edges_drop_then_return_refires() {
        let mut prev: std::collections::HashSet<String> = ["a".to_string()].into_iter().collect();
        // "a" drops out → not an up-edge, and `prev` becomes empty.
        assert!(!detect_up_edges(&mut prev, vec![]));
        // "a" returns → up-edge (absent in the previous snapshot).
        assert!(detect_up_edges(&mut prev, vec!["a".to_string()]));
    }

    /// A snapshot that simultaneously loses one zid and gains another still
    /// fires on the newcomer (and then quiesces on the unchanged repeat).
    #[test]
    fn detect_up_edges_simultaneous_add_remove_fires() {
        let mut prev: std::collections::HashSet<String> = ["a".to_string()].into_iter().collect();
        assert!(detect_up_edges(&mut prev, vec!["b".to_string()]));
        assert!(!detect_up_edges(&mut prev, vec!["b".to_string()]));
    }
}

#[cfg(test)]
mod epoch_republish_tests {
    //! ZEB-702 T3 (Component B): the transport-epoch republish listener
    //! (`run_epoch_republish`) re-offers every owner-scoped dataset root on a
    //! transport up-edge. These tests drive the extracted loop body with fake
    //! engines + a real watch channel — the exact shape production wires.
    use super::run_epoch_republish;
    use crate::fleet_sync::RepublishDirty;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    /// Fake dataset engine: counts `republish_dirty` calls. The real engines'
    /// `republish_dirty` is exactly `notify_dirty` — a synchronous, non-blocking
    /// schedule — so a call-count fake faithfully models the seam the listener
    /// drives (the debounced publish itself is covered by the T2 engine tests).
    #[derive(Default)]
    struct CountingEngine {
        calls: AtomicUsize,
    }
    impl RepublishDirty for CountingEngine {
        fn republish_dirty(&self) {
            self.calls.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Build N counting engines; return the concrete handles (to read counters)
    /// and the `Arc<dyn RepublishDirty>` bundle the listener consumes.
    fn engines(n: usize) -> (Vec<Arc<CountingEngine>>, Vec<Arc<dyn RepublishDirty>>) {
        let concrete: Vec<Arc<CountingEngine>> = (0..n)
            .map(|_| Arc::new(CountingEngine::default()))
            .collect();
        let dyn_engines: Vec<Arc<dyn RepublishDirty>> = concrete
            .iter()
            .map(|e| e.clone() as Arc<dyn RepublishDirty>)
            .collect();
        (concrete, dyn_engines)
    }

    /// (a) One post-subscribe bump nudges every engine exactly once. The
    /// `send_modify` then `drop(tx)` sequence is deterministic: the receiver's
    /// first `changed()` sees the version advance (Ok → one fan-out), the second
    /// sees the sender gone with no further change (Err → clean exit), so each
    /// engine is nudged exactly once regardless of scheduler timing.
    #[tokio::test(start_paused = true)]
    async fn one_bump_nudges_each_engine_once() {
        let (tx, rx) = tokio::sync::watch::channel(0u64);
        let (concrete, dyn_engines) = engines(3);
        let task = tokio::spawn(run_epoch_republish(rx, dyn_engines));
        tx.send_modify(|e| *e = e.wrapping_add(1));
        drop(tx);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("listener must exit when the sender drops")
            .expect("listener task must not panic");
        for e in &concrete {
            assert_eq!(e.calls.load(Ordering::SeqCst), 1, "one bump = one nudge");
        }
    }

    /// (b) No bump → zero nudges. The initial subscribe value is NOT a change
    /// (`changed()` only wakes on post-subscribe writes), so a listener that
    /// starts and then sees the sender drop must never have fired — this pins
    /// the "don't re-offer on the initial value" contract.
    #[tokio::test(start_paused = true)]
    async fn no_bump_no_nudge() {
        let (tx, rx) = tokio::sync::watch::channel(0u64);
        let (concrete, dyn_engines) = engines(2);
        let task = tokio::spawn(run_epoch_republish(rx, dyn_engines));
        // Drop the sender WITHOUT any send_modify — the current value 0 is the
        // subscribe baseline, not a change.
        drop(tx);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("listener must exit when the sender drops")
            .expect("listener task must not panic");
        for e in &concrete {
            assert_eq!(
                e.calls.load(Ordering::SeqCst),
                0,
                "no bump must produce no nudge (initial value is not a change)"
            );
        }
    }

    /// (c) Two rapid bumps → each engine nudged at least once and at most twice.
    /// watch coalescing is allowed: if the listener observes both versions in one
    /// `changed()` wake it fires once; if it wakes between them it fires twice.
    /// We assert the BOUND, not an exact count.
    #[tokio::test(start_paused = true)]
    async fn two_rapid_bumps_bounded() {
        let (tx, rx) = tokio::sync::watch::channel(0u64);
        let (concrete, dyn_engines) = engines(2);
        let task = tokio::spawn(run_epoch_republish(rx, dyn_engines));
        tx.send_modify(|e| *e = e.wrapping_add(1));
        // Give the listener a chance (but no guarantee) to observe the first bump
        // before the second lands — exercises both the coalesced and the
        // observed-separately paths across runs.
        tokio::task::yield_now().await;
        tx.send_modify(|e| *e = e.wrapping_add(1));
        drop(tx);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("listener must exit when the sender drops")
            .expect("listener task must not panic");
        for e in &concrete {
            let calls = e.calls.load(Ordering::SeqCst);
            assert!(
                (1..=2).contains(&calls),
                "two bumps must nudge 1..=2 times (coalescing), got {calls}"
            );
        }
    }

    /// (d) Sender dropped → the task exits (no hang). Paused time makes the
    /// timeout deterministic: if the loop failed to exit on sender-drop the
    /// virtual clock would auto-advance to the deadline and this would Err.
    #[tokio::test(start_paused = true)]
    async fn sender_drop_exits_task() {
        let (tx, rx) = tokio::sync::watch::channel(0u64);
        let (_concrete, dyn_engines) = engines(1);
        let task = tokio::spawn(run_epoch_republish(rx, dyn_engines));
        drop(tx);
        tokio::time::timeout(Duration::from_secs(30), task)
            .await
            .expect("listener must exit promptly when the sender drops")
            .expect("listener task must not panic");
    }
}

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
mod zeb_409_leaf_cap_tests {
    use super::leaf_cap_exceeded;

    #[test]
    fn none_is_unbounded() {
        // No cap: even a huge leaf is accepted (pre-ZEB-409 behavior for the
        // non-avatar callers that pass `None`).
        assert_eq!(leaf_cap_exceeded(10_000_000, None), None);
        assert_eq!(leaf_cap_exceeded(0, None), None);
    }

    #[test]
    fn under_and_at_cap_are_accepted() {
        // Strictly-under and exactly-at-cap are allowed — mirrors
        // fetch_recursive's assembled-total `out.len() > cap` boundary.
        assert_eq!(leaf_cap_exceeded(100, Some(512)), None);
        assert_eq!(leaf_cap_exceeded(512, Some(512)), None);
    }

    #[test]
    fn over_cap_reports_the_cap() {
        // One byte over → rejected, and the cap is surfaced for the error.
        assert_eq!(leaf_cap_exceeded(513, Some(512)), Some(512));
        // A 600KiB leaf under the 512KiB avatar cap is rejected pre-`to_vec`.
        const AVATAR_CAP: usize = 512 * 1024;
        assert_eq!(
            leaf_cap_exceeded(600 * 1024, Some(AVATAR_CAP)),
            Some(AVATAR_CAP)
        );
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

        let got = fetch_recursive(fetcher, leaf, None).await.unwrap();
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

        let got = fetch_recursive(fetcher, root, None).await.unwrap();
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

        let err = fetch_recursive(fetcher, root, None).await.unwrap_err();
        assert!(err.contains("missing cid"), "got: {err}");
    }

    #[tokio::test]
    async fn max_bytes_cap_rejects_oversized_assembly() {
        // a(3)+b(4)+c(5) = 12 bytes assembled, fetched in order a,b,c.
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
        store.insert(a, a_bytes);
        store.insert(b, b_bytes);
        store.insert(c, c_bytes);
        store.insert(root, payload);
        let fetcher = move |cid: ContentId| {
            let bytes = store.get(&cid).cloned();
            std::future::ready(bytes.ok_or_else(|| format!("missing cid: {cid:?}")))
        };

        // cap=5 → rejected once a(3)+b(4)=7 > 5.
        let err = fetch_recursive(fetcher.clone(), root, Some(5))
            .await
            .unwrap_err();
        assert!(err.contains("exceeds"), "got: {err}");
        // cap=12 (exactly the total) → accepted.
        let got = fetch_recursive(fetcher.clone(), root, Some(12))
            .await
            .unwrap();
        assert_eq!(got.len(), 12);
        // None → unbounded, accepted.
        let got = fetch_recursive(fetcher, root, None).await.unwrap();
        assert_eq!(got.len(), 12);
    }
}

#[cfg(test)]
mod allow_serve_subtree_tests {
    /// ZEB-539: covers the two halves of the `AllowServeSubtree` handler.
    /// `allowlist_flips_serve_gate_for_encrypted_cid` proves the allowlist's
    /// allow/contains drive `content_cid_servable` false→true for an encrypted
    /// CID. `walker_*` exercise the off-loop `collect_descendants_via_cas`
    /// contract directly over an mpsc-backed `GetLocal` responder — full subtree
    /// collection with traversal-time dedup, and the missing-root error — without
    /// booting `event_loop::run`. The end-to-end CasOp-flips-the-gate path is
    /// additionally covered by PR2's two-node integration test (T2).
    #[test]
    fn allowlist_flips_serve_gate_for_encrypted_cid() {
        use crate::content_store::CommunityServeAllowlist;
        use harmony_content::cid::{ContentFlags, ContentId};
        let cid = ContentId::for_book(
            b"encrypted-artifact-root",
            ContentFlags {
                encrypted: true,
                ..Default::default()
            },
        )
        .unwrap();
        let allow = CommunityServeAllowlist::new();
        assert!(
            !super::content_cid_servable(&cid, &allow),
            "encrypted CID not servable before allowlisting"
        );
        allow.allow(cid);
        assert!(
            super::content_cid_servable(&cid, &allow),
            "encrypted CID servable after allowlisting (the AllowServeSubtree effect)"
        );
    }

    /// The off-loop walker collects root + every descendant over a `GetLocal`
    /// responder, and returns each CID exactly once even when a leaf is reachable
    /// via two paths (traversal-time dedup). DAG: root → [sub, a]; sub → [a, b],
    /// so `a` is shared between the root and the inner bundle.
    #[tokio::test]
    async fn walker_collects_subtree_and_dedups_shared_leaves() {
        use super::collect_descendants_via_cas;
        use crate::content_store::CasOp;
        use harmony_content::bundle::BundleBuilder;
        use harmony_content::cid::{ContentFlags, ContentId};
        use std::collections::{HashMap, HashSet};
        use tokio::sync::mpsc;

        let a_bytes = b"aaa".to_vec();
        let b_bytes = b"bbbb".to_vec();
        let a = ContentId::for_book(&a_bytes, ContentFlags::default()).unwrap();
        let b = ContentId::for_book(&b_bytes, ContentFlags::default()).unwrap();

        let mut sub_builder = BundleBuilder::new();
        sub_builder.add(a).add(b);
        let (sub_payload, sub) = sub_builder
            .build_with_flags(ContentFlags::default())
            .unwrap();

        // Root references `a` directly AND via `sub` → `a` is reached twice.
        let mut root_builder = BundleBuilder::new();
        root_builder.add(sub).add(a);
        let (root_payload, root) = root_builder
            .build_with_flags(ContentFlags::default())
            .unwrap();

        let mut store: HashMap<ContentId, Vec<u8>> = HashMap::new();
        store.insert(a, a_bytes);
        store.insert(b, b_bytes);
        store.insert(sub, sub_payload);
        store.insert(root, root_payload);

        let (cas_op_tx, mut cas_op_rx) = mpsc::channel::<CasOp>(16);
        // Responder answers GetLocal from the store; the walker sends nothing else.
        let responder = tokio::spawn(async move {
            while let Some(op) = cas_op_rx.recv().await {
                match op {
                    CasOp::GetLocal { cid, reply } => {
                        let _ = reply.send(store.get(&cid).cloned());
                    }
                    _ => panic!("walker must only send GetLocal"),
                }
            }
        });

        let got = collect_descendants_via_cas(&cas_op_tx, root)
            .await
            .expect("walk succeeds when root is present");

        let unique: HashSet<ContentId> = got.iter().copied().collect();
        assert_eq!(unique.len(), got.len(), "walker returned duplicate CIDs");
        assert_eq!(
            unique,
            [root, sub, a, b].into_iter().collect::<HashSet<_>>(),
            "walk must yield root + all descendants exactly once"
        );

        drop(cas_op_tx);
        responder.await.unwrap();
    }

    /// A missing root is a hard error (refuse rather than allowlist a partial
    /// tree): the responder reports every CID absent, so the upfront root fetch
    /// returns `None`.
    #[tokio::test]
    async fn walker_errors_when_root_missing_locally() {
        use super::collect_descendants_via_cas;
        use crate::content_store::CasOp;
        use harmony_content::cid::{ContentFlags, ContentId};
        use tokio::sync::mpsc;

        let root = ContentId::for_book(b"absent-root", ContentFlags::default()).unwrap();
        let (cas_op_tx, mut cas_op_rx) = mpsc::channel::<CasOp>(16);
        let responder = tokio::spawn(async move {
            while let Some(op) = cas_op_rx.recv().await {
                if let CasOp::GetLocal { reply, .. } = op {
                    let _ = reply.send(None);
                }
            }
        });

        let err = collect_descendants_via_cas(&cas_op_tx, root)
            .await
            .expect_err("missing root must error");
        let msg = format!("{err:?}");
        assert!(msg.contains("missing locally"), "got: {msg}");

        drop(cas_op_tx);
        responder.await.unwrap();
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
                CasOp::AllowServeSubtree { .. } => {
                    panic!("wrapper must not send AllowServeSubtree");
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
    async fn responder_collect_admits(rx: mpsc::Receiver<CasOp>) -> Vec<(ContentId, Vec<u8>)> {
        let (admits, _serveable) = responder_collect_admits_with_serveable(rx).await;
        admits
    }

    /// Like `responder_collect_admits` but also captures each admit's
    /// `serveable` flag (ZEB-535), so a test can assert the flag propagated
    /// from the wrapper's `serveable` param into every `CasOp::PutLocal`.
    async fn responder_collect_admits_with_serveable(
        mut rx: mpsc::Receiver<CasOp>,
    ) -> (Vec<(ContentId, Vec<u8>)>, Vec<bool>) {
        let mut admits: Vec<(ContentId, Vec<u8>)> = Vec::new();
        let mut serveables: Vec<bool> = Vec::new();
        while let Some(op) = rx.recv().await {
            match op {
                CasOp::PutLocal {
                    cid,
                    blob,
                    serveable,
                    reply,
                } => {
                    admits.push((cid, blob));
                    serveables.push(serveable);
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
                CasOp::AllowServeSubtree { .. } => {
                    panic!("wrapper must not send AllowServeSubtree");
                }
            }
        }
        (admits, serveables)
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
        let wrapped = wrap_fetch_one_with_admission(fetcher, cas_op_tx, false);

        // R1 (Cursor + Qodo): the wrapper now uses synchronous
        // admission, so each per-CID call blocks awaiting a PutLocal
        // reply. Drive a responder concurrent with fetch_recursive
        // that ACKs each PutLocal and collects (cid, blob). The
        // responder finishes when fetch_recursive returns and the
        // wrapped closure is dropped, releasing the last cas_op_tx.
        let responder = tokio::spawn(responder_collect_admits(cas_op_rx));

        // Drive through fetch_recursive — every per-CID call goes through
        // the wrapper, so every successful fetch must produce a PutLocal.
        let got = fetch_recursive(wrapped, root, None).await.unwrap();
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

    /// ZEB-535: the wrapper's `serveable` param must propagate onto EVERY
    /// `CasOp::PutLocal` it emits, so a fetcher pulling a chunked encrypted
    /// artifact re-serves every CID it admits (not just the root).
    #[tokio::test]
    async fn serveable_flag_propagates_to_every_admit() {
        let a_bytes = b"aaa".to_vec();
        let b_bytes = b"bbbb".to_vec();
        let a = ContentId::for_book(&a_bytes, ContentFlags::default()).unwrap();
        let b = ContentId::for_book(&b_bytes, ContentFlags::default()).unwrap();

        let mut builder = BundleBuilder::new();
        builder.add(a).add(b);
        let (payload, root) = builder.build_with_flags(ContentFlags::default()).unwrap();

        let mut store: HashMap<ContentId, Vec<u8>> = HashMap::new();
        store.insert(a, a_bytes.clone());
        store.insert(b, b_bytes.clone());
        store.insert(root, payload.clone());

        let fetcher = move |cid: ContentId| {
            let bytes = store.get(&cid).cloned();
            std::future::ready(bytes.ok_or_else(|| format!("missing cid: {cid:?}")))
        };

        let (cas_op_tx, cas_op_rx) = mpsc::channel::<CasOp>(16);
        // serveable: true — every admit must carry it.
        let wrapped = wrap_fetch_one_with_admission(fetcher, cas_op_tx, true);
        let responder = tokio::spawn(responder_collect_admits_with_serveable(cas_op_rx));

        let _ = fetch_recursive(wrapped, root, None).await.unwrap();
        let (admits, serveables) = responder.await.unwrap();

        assert_eq!(admits.len(), 3, "root bundle + 2 leaves");
        assert_eq!(serveables.len(), 3);
        assert!(
            serveables.iter().all(|s| *s),
            "every admit must carry serveable=true; got {serveables:?}"
        );
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
        let wrapped = wrap_fetch_one_with_admission(fetcher, cas_op_tx, false);

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
        let wrapped = wrap_fetch_one_with_admission(fetcher, cas_op_tx, false);

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
        let wrapped = wrap_fetch_one_with_admission(fetcher, cas_op_tx, false);

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
        let wrapped = wrap_fetch_one_with_admission(fetcher, cas_op_tx, false);
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
fn emit_frontend_event(
    app: &std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>,
    key_expr: &str,
    payload: &[u8],
    hop_distance: Option<u8>,
    followed_set: &std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    vine_feed_cache: &std::sync::Arc<std::sync::Mutex<crate::vine_feed_cache::VineFeedCache>>,
    mail_mgr: &std::sync::Arc<std::sync::Mutex<crate::mail::MailManager>>,
    own_mail_key: &str,
    own_root_key: &str,
    mail_sync: Option<&Arc<crate::mail_sync::MailSync>>,
) -> Option<String> {
    if key_expr.starts_with("harmony/compute/capacity/") {
        if let Some(mut update) = crate::parse_capacity(key_expr, payload) {
            update.hop_distance = hop_distance;
            crate::node_event_sink::emit_ser(app.as_ref(), "capacity-update", &update);
        }
    } else if key_expr.starts_with("harmony/profile/") {
        if let Ok(profile) = serde_json::from_slice::<crate::ProfilePayload>(payload) {
            crate::node_event_sink::emit_ser(app.as_ref(), "profile-update", &profile);
        }
    } else if key_expr.starts_with("harmony/community/") {
        if let Ok(msg) = serde_json::from_slice::<crate::ChannelMessagePayload>(payload) {
            crate::node_event_sink::emit_ser(app.as_ref(), "message-received", &msg);
        }
    } else if key_expr.starts_with("harmony/vines/") {
        // ZEB-678 S2: verifier-controlled wall clock for the authority-aware
        // ingest paths (authority enrollment expiry; reaction enrollment). One
        // computation shared by the authority / reaction / descriptor arms.
        let now_ms = u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0),
        )
        .unwrap_or(u64::MAX);
        let now_secs = now_ms / 1000;
        if key_expr.contains("/tombstones/") {
            // ZEB-670: creator-signed delete. Returns the CID to evict
            // when the tombstone freed the last live reference — the
            // caller owns `runtime` + `pin_intent` and performs the burn.
            return handle_vine_tombstone_sample(app, key_expr, payload, vine_feed_cache);
        }
        if key_expr.ends_with("/follows") {
            // ZEB-671: verified follow list → cache (LWW). A real graph
            // change (Inserted/UpdatedNewer) is announced to the frontend
            // so it refetches degree/provenance annotations; stale
            // re-arrivals and rejected records are absorbed silently.
            // `reach_changed` gates the emit: an admitted list from an
            // owner outside the viewer's graph does not change any
            // degree, and the frontend should not refetch for it.
            let reach_changed = match vine_feed_cache.lock() {
                Ok(mut cache) => match cache.on_follow_list_sample(key_expr, payload) {
                    crate::vine_feed_cache::FollowListOutcome::Inserted
                    | crate::vine_feed_cache::FollowListOutcome::UpdatedNewer => {
                        cache.recompute_reach()
                    }
                    crate::vine_feed_cache::FollowListOutcome::Rejected(reason) => {
                        tracing::debug!(key_expr, reason, "follow-list sample rejected");
                        false
                    }
                    crate::vine_feed_cache::FollowListOutcome::IgnoredOlder => false,
                },
                Err(e) => {
                    tracing::error!(error = %e, "vine_feed_cache mutex poisoned; skipping follow-list ingest");
                    false
                }
            };
            if reach_changed {
                crate::node_event_sink::emit_ser(
                    app.as_ref(),
                    "vine-graph-updated",
                    &serde_json::Value::Null,
                );
            }
            return None;
        }
        if key_expr.ends_with("/authority") {
            // ZEB-678 S2: feed authority record → cache (owner-anchoring +
            // revocation). Verifier-controlled clock; ingest never degrades
            // existing state and drives no frontend view, so no emit.
            match vine_feed_cache.lock() {
                Ok(mut cache) => cache.on_authority_sample(key_expr, payload, now_secs),
                Err(e) => {
                    tracing::error!(error = %e, "vine_feed_cache mutex poisoned; skipping authority ingest");
                }
            }
            return None;
        }
        if key_expr.contains("/reactions/") {
            // ZEB-286: route reaction through the cache. Re-emit to the
            // frontend ONLY on Inserted or UpdatedNewer (stale/duplicate
            // re-arrivals are absorbed silently). The cache's per-LWW
            // dedupe replaces the previous naive every-sample emit.
            let outcome = match vine_feed_cache.lock() {
                Ok(mut cache) => cache.on_reaction_sample(key_expr, payload, now_secs),
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
                    crate::node_event_sink::emit_ser(
                        app.as_ref(),
                        "vine-reaction-received",
                        &reaction,
                    );
                }
            }
        } else {
            // ZEB-286: route descriptor through the cache. Source-tag
            // (Followed vs Discover) is decided by the cache once at
            // first insert; re-arrivals are absorbed. The cache returns
            // the ready-to-emit VineVideoDtoWithSource so we do not have
            // to re-parse + re-mutate JSON here. `now_ms` is the block-hoisted
            // clock (ZEB-678 S2).
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
                crate::node_event_sink::emit_ser(app.as_ref(), "vine-received", &dto);
            }
        }
    } else if key_expr.starts_with("harmony/announce/") {
        if let Some(announcement) = crate::parse_content_announcement(key_expr, payload) {
            crate::node_event_sink::emit_ser(app.as_ref(), "content-announced", &announcement);
        }
    } else if key_expr.contains("/telemetry/") {
        if let Some(event) = crate::parse_telemetry(payload) {
            crate::node_event_sink::emit_ser(app.as_ref(), "telemetry-event", &event);
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
                return None;
            }
        };
        match mgr.receive_message(payload) {
            Ok(crate::mail::ReceiveOutcome::Inserted(entry)) => {
                crate::node_event_sink::emit_ser(app.as_ref(), "mail-received", &entry);
            }
            Ok(crate::mail::ReceiveOutcome::Promoted(_entry)) => {
                tracing::debug!(key_expr, "live push promoted Pending to Local (no emit)");
            }
            Err(e) => {
                tracing::debug!(key_expr, error = %e, "mail receive skipped");
            }
        }
    }
    None
}

/// ZEB-670: route a vine-tombstone sample through the cache; emit
/// `vine-removed` when a cached descriptor was actually removed.
/// Verification (signature, topic binding, ownership) happens inside
/// `on_tombstone_sample`. Returns the hex CID to evict when the
/// tombstone freed the last live reference to its content — the caller
/// owns `runtime` + `pin_intent` and performs the (pin-guarded) burn.
fn handle_vine_tombstone_sample(
    app: &std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>,
    key_expr: &str,
    payload: &[u8],
    vine_feed_cache: &std::sync::Arc<std::sync::Mutex<crate::vine_feed_cache::VineFeedCache>>,
) -> Option<String> {
    let outcome = match vine_feed_cache.lock() {
        Ok(mut cache) => cache.on_tombstone_sample(key_expr, payload),
        Err(e) => {
            tracing::error!(error = %e, "vine_feed_cache mutex poisoned; skipping tombstone");
            None
        }
    };
    match outcome {
        Some(crate::vine_feed_cache::TombstoneOutcome::Applied { removed, evict_cid }) => {
            // Emitted for every fresh apply — a subscriber holding only
            // reshares (no original cached) still needs the event to mark
            // its stubs (CodeRabbit PR #445 round 1).
            crate::node_event_sink::emit_ser(app.as_ref(), "vine-removed", &removed);
            evict_cid
        }
        Some(crate::vine_feed_cache::TombstoneOutcome::Rejected(reason)) => {
            tracing::debug!(key_expr, reason, "vine tombstone rejected");
            None
        }
        Some(crate::vine_feed_cache::TombstoneOutcome::AlreadyApplied) | None => None,
    }
}

#[cfg(test)]
mod vine_tombstone_routing_tests {
    use super::handle_vine_tombstone_sample;
    use crate::node_event_sink::{NodeEventSink, RecordingSink};
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

    struct Fixture {
        recording: Arc<RecordingSink>,
        sink: Arc<dyn NodeEventSink>,
        cache: Arc<Mutex<crate::vine_feed_cache::VineFeedCache>>,
        id: harmony_identity::PrivateIdentity,
        addr: String,
    }

    fn fixture() -> Fixture {
        let recording = RecordingSink::new();
        let sink: Arc<dyn NodeEventSink> = Arc::new(recording.clone());
        let cache = Arc::new(Mutex::new(crate::vine_feed_cache::VineFeedCache::new()));
        let id = harmony_identity::PrivateIdentity::generate(&mut rand::rngs::OsRng);
        let addr = hex::encode(id.public_identity().address_hash);
        Fixture {
            recording,
            sink,
            cache,
            id,
            addr,
        }
    }

    fn insert_descriptor(
        cache: &Arc<Mutex<crate::vine_feed_cache::VineFeedCache>>,
        signer: &harmony_identity::PrivateIdentity,
        vine_id: &str,
        cid: &str,
    ) {
        // ZEB-673: cache admission is strict — the seed must be
        // creator-signed and arrive on the signer's own topic.
        let addr = hex::encode(signer.public_identity().address_hash);
        let mut d = crate::VineDescriptorPayload {
            id: vine_id.into(),
            creator_address: addr.clone(),
            creator_name: "Creator".into(),
            created_at: 1_700_000_000,
            video_cid: cid.into(),
            title: None,
            reshare_of: None,
            original_creator_address: None,
            original_creator_name: None,
            identity_pub: None,
            sig: None,
            device_sig: None,
        };
        crate::vine_signing::sign_descriptor(signer, &mut d);
        let outcome = cache.lock().unwrap().on_descriptor_sample(
            &format!("harmony/vines/{addr}"),
            &serde_json::to_vec(&d).unwrap(),
            &HashSet::new(),
            1_700_000_000_000, // ms; now_secs matches the descriptor's created_at (1_700_000_000)
        );
        assert!(matches!(
            outcome,
            Some(crate::vine_feed_cache::DescriptorOutcome::Inserted { .. })
        ));
    }

    fn signed_tombstone_bytes(
        id: &harmony_identity::PrivateIdentity,
        vine_id: &str,
        video_cid: &str,
        addr: &str,
    ) -> Vec<u8> {
        let t = crate::vine_tombstone::sign_tombstone(
            id,
            vine_id.into(),
            video_cid.into(),
            addr.into(),
            1_900_000_000,
        );
        serde_json::to_vec(&t).unwrap()
    }

    #[test]
    fn applied_tombstone_emits_vine_removed_once_and_returns_evict_cid() {
        let Fixture {
            recording,
            sink,
            cache,
            id,
            addr,
        } = fixture();
        insert_descriptor(&cache, &id, "vine-1", "aa".repeat(32).as_str());

        let evict = handle_vine_tombstone_sample(
            &sink,
            &format!("harmony/vines/{addr}/tombstones/vine-1"),
            &signed_tombstone_bytes(&id, "vine-1", &"aa".repeat(32), &addr),
            &cache,
        );

        assert_eq!(evict.as_deref(), Some("aa".repeat(32).as_str()));
        let frames = recording.frames();
        assert_eq!(frames.len(), 1, "exactly one emit, got {frames:?}");
        let (event, payload) = &frames[0];
        assert_eq!(event, "vine-removed");
        assert_eq!(payload["vineId"], "vine-1");
        assert_eq!(payload["videoCid"], "aa".repeat(32));
        assert_eq!(payload["creatorAddress"], addr);
    }

    #[test]
    fn rejected_tombstone_emits_nothing() {
        let Fixture {
            recording,
            sink,
            cache,
            id,
            addr,
        } = fixture();
        insert_descriptor(&cache, &id, "vine-1", "cid-aaa");

        let mut t: crate::vine_tombstone::VineTombstonePayload =
            serde_json::from_slice(&signed_tombstone_bytes(&id, "vine-1", "cid-aaa", &addr))
                .unwrap();
        t.sig = hex::encode([0u8; 64]);

        let evict = handle_vine_tombstone_sample(
            &sink,
            &format!("harmony/vines/{addr}/tombstones/vine-1"),
            &serde_json::to_vec(&t).unwrap(),
            &cache,
        );

        assert_eq!(evict, None);
        assert!(recording.frames().is_empty());
        assert_eq!(cache.lock().unwrap().len_descriptors(), 1);
    }

    #[test]
    fn pre_arrival_tombstone_emits_removal_and_evict_but_only_once() {
        let Fixture {
            recording,
            sink,
            cache,
            id,
            addr,
        } = fixture();

        let evict = handle_vine_tombstone_sample(
            &sink,
            &format!("harmony/vines/{addr}/tombstones/vine-9"),
            &signed_tombstone_bytes(&id, "vine-9", "cid-zzz", &addr),
            &cache,
        );

        // No descriptor was cached, but the event still fires (a
        // reshare-only subscriber needs it for stub-marking) and the
        // evict candidate is reported (evicting never-held bytes is a
        // no-op). CodeRabbit PR #445 round 1.
        assert_eq!(evict.as_deref(), Some("cid-zzz"));
        let frames = recording.frames();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].0, "vine-removed");
        assert_eq!(frames[0].1["vineId"], "vine-9");

        // Idempotence: re-delivery neither re-emits nor re-evicts.
        let again = handle_vine_tombstone_sample(
            &sink,
            &format!("harmony/vines/{addr}/tombstones/vine-9"),
            &signed_tombstone_bytes(&id, "vine-9", "cid-zzz", &addr),
            &cache,
        );
        assert_eq!(again, None);
        assert_eq!(recording.frames().len(), 1);
    }
}

/// ZEB-437: bounded wait for a state-root queryable's engine reply.
///
/// A state-root queryable forwards each inbound query to the engine's
/// single-writer task and awaits a freshly-encoded root packet over
/// `reply_rx`. The engine's `select!` commits fully to that serve arm while it
/// encodes (CRDT clone + CBOR + AEAD + CAS pin + replay fsync; 500ms–2s on a
/// degraded path), so it cannot observe `stop_node` until the encode finishes.
/// A queryable parked on a bare `reply_rx.await` is therefore pinned for that
/// whole duration — and because the adapter's outer task joins the queryable
/// sub-task, that pins adapter teardown too, beyond the ~1s closing-poll SLA
/// the publisher, subscriber, and root-fetch sub-tasks honor.
///
/// This races the engine's oneshot against a repeating 500ms closing-poll
/// (mirroring the sibling root-fetch drain loop below) so a node-stop unblocks
/// teardown within one tick, plus an overall ~5s cap so a wedged (non-closing)
/// engine can't pin the queryable indefinitely. `Some(packet)` is the servable
/// wire; `None` means the reply was abandoned — `closing` flipped (silent,
/// routine shutdown), the cap elapsed (warned, since a non-closing wedged
/// engine is a real fault), the engine dropped the oneshot, or the engine
/// reported an encode error — and in every `None` case the caller withholds the
/// zenoh reply so the querier's latch backs off and retries (possibly against
/// another responder).
/// The biased reply arm means the normal fast path pays no tick.
async fn recv_root_reply_bounded<E>(
    mut reply_rx: tokio::sync::oneshot::Receiver<Result<Vec<u8>, E>>,
    closing: &AtomicBool,
    topic: &str,
) -> Option<Vec<u8>> {
    // Closing-poll cadence; matches the root-fetch drain loop below.
    const POLL: Duration = Duration::from_millis(500);
    // Overall cap for a wedged, non-closing engine: 10 × 500ms = 5s.
    const MAX_TICKS: u32 = 10;
    let mut ticks: u32 = 0;
    loop {
        tokio::select! {
            biased;
            // Ok(Ok(packet)) → servable; RecvError (engine gone) and an
            // engine-side encode Err both collapse to None, exactly as the
            // former `if let Ok(Ok(_))` withheld the reply.
            r = &mut reply_rx => return r.ok().and_then(|inner| inner.ok()),
            _ = tokio::time::sleep(POLL) => {
                ticks += 1;
                // A `closing`-triggered abandon is routine (every stop_node)
                // and stays silent — logging it would spam a warning on every
                // shutdown. The cap firing while NOT closing is a genuinely
                // degraded condition (a wedged / non-responsive engine), so
                // warn once with the topic so an operator debugging silent
                // state-root stalls has a log line to find. Same `!closing`
                // discipline as this adapter's publish/subscriber warnings.
                if closing.load(Ordering::SeqCst) {
                    return None;
                }
                if ticks >= MAX_TICKS {
                    tracing::warn!(
                        %topic,
                        timeout = ?(POLL * MAX_TICKS),
                        "state-root queryable abandoned reply: engine did not \
                         respond within the cap (wedged?); querier will retry"
                    );
                    return None;
                }
            }
        }
    }
}

#[cfg(test)]
mod root_reply_bounded_tests {
    //! ZEB-437: `recv_root_reply_bounded` races a state-root queryable's engine
    //! reply against a closing-poll (+ a wedged-engine cap) so a mid-encode
    //! `stop_node` can't pin adapter teardown past the ~1s closing SLA. Paused
    //! virtual time drives the polls deterministically (model:
    //! `epoch_republish_tests`).
    use super::recv_root_reply_bounded;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::oneshot;

    /// Happy path: the reply is already available, so the biased reply arm wins
    /// on the first poll — the servable packet is returned with zero
    /// virtual-clock advance (no closing-poll tick paid on the fast path).
    #[tokio::test(start_paused = true)]
    async fn returns_packet_when_engine_replies() {
        let (tx, rx) = oneshot::channel::<Result<Vec<u8>, String>>();
        tx.send(Ok(vec![1, 2, 3])).unwrap();
        let closing = Arc::new(AtomicBool::new(false));
        let start = tokio::time::Instant::now();
        let got = recv_root_reply_bounded(rx, &closing, "harmony/test/state-root-v1").await;
        assert_eq!(got, Some(vec![1, 2, 3]));
        assert_eq!(start.elapsed(), Duration::ZERO, "fast path pays no tick");
    }

    /// An engine-side encode error (`Ok(Err(_))`) is not servable → `None`,
    /// exactly as the old `if let Ok(Ok(_))` withheld the reply.
    #[tokio::test(start_paused = true)]
    async fn none_on_encode_error() {
        let (tx, rx) = oneshot::channel::<Result<Vec<u8>, String>>();
        tx.send(Err("encode boom".to_string())).unwrap();
        let closing = Arc::new(AtomicBool::new(false));
        let got = recv_root_reply_bounded(rx, &closing, "harmony/test/state-root-v1").await;
        assert_eq!(got, None);
    }

    /// Engine dropped the oneshot (shutdown race / engine gone) → `RecvError`
    /// → `None`, matching the old code's non-`Ok(Ok)` drop.
    #[tokio::test(start_paused = true)]
    async fn none_when_sender_dropped() {
        let (tx, rx) = oneshot::channel::<Result<Vec<u8>, String>>();
        drop(tx);
        let closing = Arc::new(AtomicBool::new(false));
        let got = recv_root_reply_bounded(rx, &closing, "harmony/test/state-root-v1").await;
        assert_eq!(got, None);
    }

    /// The bug's case: the engine never replies (mid-encode) and `closing`
    /// flips. The helper must abandon within one 500ms poll tick — not hang for
    /// the unbounded encode duration. `_tx` stays alive so this exercises the
    /// closing path, not the dropped-sender path.
    #[tokio::test(start_paused = true)]
    async fn abandons_on_closing_within_one_tick() {
        let (_tx, rx) = oneshot::channel::<Result<Vec<u8>, String>>();
        let closing = Arc::new(AtomicBool::new(true));
        let start = tokio::time::Instant::now();
        let got = recv_root_reply_bounded(rx, &closing, "harmony/test/state-root-v1").await;
        assert_eq!(got, None);
        assert_eq!(
            start.elapsed(),
            Duration::from_millis(500),
            "closing observed at the first poll tick"
        );
    }

    /// A wedged (non-closing) engine can't pin the queryable forever: the
    /// overall cap (10 × 500ms = 5s) frees it even though `closing` never flips.
    #[tokio::test(start_paused = true)]
    async fn caps_wedged_engine_at_five_seconds() {
        let (_tx, rx) = oneshot::channel::<Result<Vec<u8>, String>>();
        let closing = Arc::new(AtomicBool::new(false));
        let start = tokio::time::Instant::now();
        let got = recv_root_reply_bounded(rx, &closing, "harmony/test/state-root-v1").await;
        assert_eq!(got, None);
        assert_eq!(
            start.elapsed(),
            Duration::from_secs(5),
            "wedged engine freed at the 5s cap"
        );
    }

    /// A slow-but-legitimate encode: the reply arrives after several
    /// closing-poll ticks while `closing` stays false. The helper must return
    /// it (not abandon) at the moment it lands — this is the fix's core
    /// interleaving, distinct from the already-ready fast path, and it must
    /// resolve before the 5s cap would fire.
    #[tokio::test(start_paused = true)]
    async fn returns_packet_that_arrives_mid_wait() {
        let (tx, rx) = oneshot::channel::<Result<Vec<u8>, String>>();
        let closing = Arc::new(AtomicBool::new(false));
        // Deliver the reply 1.2s in — past two 500ms poll ticks, well under cap.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1200)).await;
            let _ = tx.send(Ok(vec![7, 7]));
        });
        let start = tokio::time::Instant::now();
        let got = recv_root_reply_bounded(rx, &closing, "harmony/test/state-root-v1").await;
        assert_eq!(got, Some(vec![7, 7]));
        assert_eq!(
            start.elapsed(),
            Duration::from_millis(1200),
            "reply returned when it arrived, not at a poll tick or the cap"
        );
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

/// Spawn a Zenoh publisher + subscriber + queryable + root-fetch driver
/// for one community's state-root topic
/// (`harmony/community/{id_hex}/state-root-v1`).
///
/// Wires:
///   - `publisher_rx` (engine's outbound bytes) → `session.put(key, bytes)`
///   - Zenoh subscriber on the same key → `subscriber_tx` (engine's inbound)
///   - Zenoh queryable on the same key → `root_serve_tx` (engine encodes a
///     fresh root packet per query; ZEB-434 D1/D2)
///   - `fetch_request_rx` (per-community fetch driver) → Zenoh GET on the
///     same key, replies piped into `subscriber_tx` (ZEB-434 D3/D4)
///
/// `closing` is the event-loop-wide shutdown flag; when set, transport
/// errors are downgraded to silence so a clean `stop_node` doesn't spam
/// "publish failed" / "subscriber closed unexpectedly" warnings.
///
/// Returns a `JoinHandle<()>` so the registry / `start_node` can await
/// teardown if needed. Internally spawns four child tasks (publisher,
/// queryable, root-fetch driver, subscriber) and joins them before the
/// outer handle resolves.
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
    root_serve_tx: tokio::sync::mpsc::Sender<crate::community_state_sync::RootServeRequest>,
    mut fetch_request_rx: tokio::sync::mpsc::Receiver<CommunityRootFetchRequest>,
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
                        // ZEB-916 Q1: wire-packet volume + publish frequency
                        // (both otherwise unlogged). One line per debounced
                        // state-root publish.
                        let wire_bytes = bytes.len();
                        match session_pub.put(&key_pub, bytes).await {
                            Ok(()) => tracing::info!(
                                target: "harmony_volume",
                                kind = "state_root_publish",
                                topic = %topic_pub,
                                wire_bytes,
                                "state-root wire publish"
                            ),
                            Err(e) => {
                                if !closing_pub.load(Ordering::SeqCst) {
                                    tracing::warn!(
                                        topic = %topic_pub,
                                        error = %e,
                                        "community state-root publish failed"
                                    );
                                }
                            }
                        }
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                        if closing_pub.load(Ordering::SeqCst) { break; }
                    }
                }
            }
        });

        // ZEB-434 D1/D2: queryable on the state-root key. Each inbound
        // query is forwarded to the engine via `root_serve_tx`; the
        // engine's single-writer task encodes a fresh wire packet and
        // replies through the oneshot. No selector parsing — the key
        // carries no parameters (full-state exchange, no `since`).
        let session_qbl = Arc::clone(&session);
        let key_qbl = key_expr.clone();
        let topic_qbl = topic.clone();
        let closing_qbl = Arc::clone(&closing);
        // Clone for the queryable task. The original parameter stays
        // alive until the outer join, so the engine's serve channel
        // closes only at full adapter teardown (the engine latches
        // recv()==None either way).
        let root_serve_tx_qbl = root_serve_tx.clone();
        let qbl_handle = tokio::spawn(async move {
            let qbl = match session_qbl.declare_queryable(&key_qbl).await {
                Ok(q) => q,
                Err(e) => {
                    if !closing_qbl.load(Ordering::SeqCst) {
                        tracing::error!(topic = %topic_qbl, error = %e,
                            "failed to declare community state-root queryable");
                    }
                    return;
                }
            };
            loop {
                tokio::select! {
                    biased;
                    res = qbl.recv_async() => {
                        let Ok(query) = res else { break; };
                        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                        if root_serve_tx_qbl.send(reply_tx).await.is_err() {
                            // Engine gone — stop serving.
                            break;
                        }
                        // ZEB-437: bounded wait so a mid-encode stop_node can't
                        // pin this queryable (and thus adapter teardown) past
                        // the ~1s closing SLA. `None` = reply abandoned (encode
                        // error, engine gone, closing, or the wedged-engine
                        // cap). On a closing-abandon, break now instead of
                        // looping back to pay the outer 1s poll.
                        match recv_root_reply_bounded(reply_rx, &closing_qbl, &topic_qbl).await {
                            Some(packet) => {
                                if let Err(e) = query.reply(query.key_expr(), packet).await {
                                    tracing::warn!(topic = %topic_qbl, error = %e,
                                        "community state-root queryable reply failed");
                                }
                            }
                            None => {
                                if closing_qbl.load(Ordering::SeqCst) {
                                    break;
                                }
                            }
                        }
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                        if closing_qbl.load(Ordering::SeqCst) { break; }
                    }
                }
            }
        });

        // ZEB-434 D3/D4: root-fetch query driver. The per-community
        // fetch driver (registry-spawned `run_root_fetch_driver`) sends
        // one `CommunityRootFetchRequest` per attempt; this task
        // executes the zenoh GET, pipes every reply into the engine's
        // normal inbound path (`subscriber_tx`), and reports the reply
        // count. Simplified channel-log query-request driver: no
        // paging/progress, 10s GET timeout. ConsolidationMode::None so
        // multiple responders' roots all reach the engine (the CRDT
        // merge dedupes).
        let session_rf = Arc::clone(&session);
        let key_rf = topic.clone();
        let subscriber_tx_rf = subscriber_tx.clone();
        let closing_rf = Arc::clone(&closing);
        let rf_handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    maybe = fetch_request_rx.recv() => {
                        let Some(req) = maybe else { break; };
                        let receiver = match session_rf
                            .get(&key_rf)
                            .consolidation(zenoh::query::ConsolidationMode::None)
                            .timeout(std::time::Duration::from_secs(10))
                            .await
                        {
                            Ok(r) => r,
                            Err(e) => {
                                if !closing_rf.load(Ordering::SeqCst) {
                                    tracing::warn!(key = %key_rf, error = %e,
                                        "community state-root fetch query failed");
                                }
                                // req.report drops → driver maps to NoReply.
                                continue;
                            }
                        };
                        // Inner reply-drain loop with closing-poll arm
                        // (mirrors the channel-log query driver): a hung
                        // peer must not block teardown past ~500ms.
                        // ZEB-812: never await subscriber_tx inside the
                        // reply-drain arm — that holds zenoh's reply
                        // channel hostage on engine backpressure and can
                        // park zenoh's net thread (see `reply_spill`
                        // module doc).
                        let mut replies: usize = 0;
                        let mut spill = crate::reply_spill::ReplySpill::new(
                            subscriber_tx_rf.clone(),
                            ROOT_FETCH_SPILL_MAX,
                        );
                        let drained_clean: bool = loop {
                            tokio::select! {
                                biased;
                                res = receiver.recv_async() => {
                                    let Ok(reply) = res else { break true; };
                                    if let Ok(sample) = reply.into_result() {
                                        let bytes: Vec<u8> =
                                            sample.payload().to_bytes().to_vec();
                                        match spill.accept(bytes) {
                                            crate::reply_spill::AcceptOutcome::Accepted => {
                                                replies = replies.saturating_add(1);
                                            }
                                            crate::reply_spill::AcceptOutcome::DroppedFull => {}
                                            crate::reply_spill::AcceptOutcome::ConsumerGone => {
                                                return; // engine teardown
                                            }
                                        }
                                    }
                                }
                                _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {
                                    if closing_rf.load(Ordering::SeqCst) { break false; }
                                }
                            }
                        };
                        if spill.dropped() > 0 {
                            tracing::warn!(key = %key_rf, dropped = spill.dropped(),
                                "community state-root fetch: reply storm exceeded spill cap; \
                                 overflow dropped (next reconcile re-fetches)");
                        }
                        // ZEB-816: peak buffer depth against the cap (see the
                        // owner-state site above). Captured before flush().
                        tracing::debug!(
                            target: "harmony_channel",
                            key = %key_rf,
                            replies,
                            spill_peak = spill.peak(),
                            spill_cap = ROOT_FETCH_SPILL_MAX,
                            "community state-root fetch: reply drain complete"
                        );
                        // ZEB-812: post-drain delivery; report only once
                        // the page has landed (or never, on shutdown/
                        // teardown — the old no-report semantics).
                        let flushed_clean = drained_clean
                            && match spill.flush(&closing_rf).await {
                                crate::reply_spill::FlushOutcome::Flushed => true,
                                crate::reply_spill::FlushOutcome::ConsumerGone => return,
                                crate::reply_spill::FlushOutcome::ShutdownAbandoned => false,
                            };
                        if flushed_clean {
                            let _ = req.report.send(replies);
                        }
                        // !flushed_clean: report drops without a value →
                        // fetch driver sees NoReply (its shutdown watch
                        // ends it promptly during teardown anyway).
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                        if closing_rf.load(Ordering::SeqCst) { break; }
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
        let _ = qbl_handle.await;
        let _ = rf_handle.await;
        let _ = sub_handle.await;
    })
}

/// Max accepted voting wire payload (CBOR `EncryptedEnvelope`). Voting events
/// are small (typically <2 KiB); cap peer-controlled payloads before
/// materializing them to prevent allocation-DoS. Enforced on BOTH the live
/// subscriber and the backfill requester (both ingest remote-controlled bytes).
const MAX_VOTING_PAYLOAD_BYTES: usize = 64 * 1024;

/// ZEB-718: encrypt a plaintext voting frame under the community's CURRENT
/// epoch (+ voting AAD) as an `EncryptedEnvelope` CBOR. Returns `None`
/// (drop) if epoch state is missing or encode fails. Mirrors the adapter's
/// outbound-put crypto so backfill replies are wire-identical to live
/// packets — and, crucially, so a served event passes the requester's
/// current-epoch cut (this is what recovers ZEB-717 cross-rotation drops).
async fn voting_encrypt_current_epoch(
    crdt_state: &tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>,
    community_id: crate::owner_state_types::SpaceId,
    plaintext: &[u8],
) -> Option<Vec<u8>> {
    let st = crdt_state.lock().await;
    let space = st.spaces.get(&community_id)?;
    let envelope = crate::community_state_sync::encrypt_for_topic_with_aad(
        space,
        plaintext,
        crate::community_state_sync::VOTING_TOPIC_AAD,
    )
    .ok()?;
    let mut w = Vec::new();
    ciborium::into_writer(&envelope, &mut w).ok()?;
    Some(w)
}

/// ZEB-718: decode + **current-epoch-only** decrypt an `EncryptedEnvelope`
/// CBOR backfill reply. Returns `None` (drop) on decode failure,
/// stale/unknown epoch (the ZEB-717 cut, preserved on the backfill path —
/// a kicked-then-rotated member's served envelope can't be decrypted), or
/// decrypt failure. Mirrors the adapter's inbound receive cut.
async fn voting_decrypt_current_epoch_cut(
    crdt_state: &tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>,
    community_id: crate::owner_state_types::SpaceId,
    raw: &[u8],
) -> Option<Vec<u8>> {
    let envelope: crate::community_state_sync::EncryptedEnvelope =
        ciborium::from_reader(raw).ok()?;
    let st = crdt_state.lock().await;
    let space = st.spaces.get(&community_id)?;
    match space.current_epoch {
        Some(cur) if cur == envelope.epoch => {}
        _ => return None,
    }
    crate::community_state_sync::decrypt_for_topic_with_aad(
        space,
        &envelope,
        crate::community_state_sync::VOTING_TOPIC_AAD,
    )
    .ok()
}

/// ZEB-932: per-round drain byte cap for a voting RBSR reconcile (mirrors the
/// channel log's `MAX_RBSR_ROUND_BYTES`) — a runaway responder can't force
/// unbounded allocation on the requester.
const MAX_VOTING_RBSR_ROUND_BYTES: usize = 16 * 1024 * 1024;

/// ZEB-932: how many requester ticks between forced full-dump backstops. RBSR
/// runs every `backfill_interval` tick (cheap); the O(all-events) full dump —
/// the safety net for archive-window divergence, old peers, and wedged rounds —
/// runs only every `VOTING_FULL_DUMP_BACKSTOP_TICKS` ticks (or on RBSR failure).
/// At the 300 s tick this is a ~1 h floor, replacing the pre-RBSR every-300 s
/// full dump (~12× fewer full dumps).
const VOTING_FULL_DUMP_BACKSTOP_TICKS: u32 = 12;

/// ZEB-932: seal one voting RBSR message (a request) under the community's
/// current epoch + `VOTING_RBSR_AAD`. Returns `None` if epoch state is missing.
async fn voting_rbsr_seal(
    crdt_state: &tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>,
    community_id: crate::owner_state_types::SpaceId,
    msg: &crate::channel_rbsr::RbsrMessage,
) -> Option<Vec<u8>> {
    let plaintext = crate::channel_rbsr::encode_message(msg);
    let st = crdt_state.lock().await;
    let space = st.spaces.get(&community_id)?;
    let envelope = crate::community_state_sync::encrypt_for_topic_with_aad(
        space,
        &plaintext,
        crate::community_state_sync::VOTING_RBSR_AAD,
    )
    .ok()?;
    let mut w = Vec::new();
    ciborium::into_writer(&envelope, &mut w).ok()?;
    Some(w)
}

/// ZEB-932: open a sealed voting RBSR message — cap-before-alloc, current-epoch
/// cut (`envelope.epoch == current_epoch`), `VOTING_RBSR_AAD`, then decode +
/// `validate_message` at the trust boundary. `None` on any failure, so the
/// requester's frame classifier falls through to treating the frame as an inline
/// `Have` event body.
async fn voting_rbsr_open(
    crdt_state: &tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>,
    community_id: crate::owner_state_types::SpaceId,
    raw: &[u8],
) -> Option<crate::channel_rbsr::RbsrMessage> {
    if raw.len() > MAX_VOTING_PAYLOAD_BYTES {
        return None;
    }
    let envelope: crate::community_state_sync::EncryptedEnvelope =
        ciborium::from_reader(raw).ok()?;
    let st = crdt_state.lock().await;
    let space = st.spaces.get(&community_id)?;
    match space.current_epoch {
        Some(cur) if cur == envelope.epoch => {}
        _ => return None,
    }
    let plaintext = crate::community_state_sync::decrypt_for_topic_with_aad(
        space,
        &envelope,
        crate::community_state_sync::VOTING_RBSR_AAD,
    )
    .ok()?;
    let msg = crate::channel_rbsr::decode_message(&plaintext).ok()?;
    crate::channel_rbsr::validate_message(&msg).ok()?;
    Some(msg)
}

/// ZEB-932: seal an RBSR reply + its `Have` event bodies under a SINGLE current-
/// epoch snapshot (one `space` lock). Holding one snapshot for every frame is the
/// ZEB-920 guarantee for voting: a rotation between frames can't split epochs and
/// leave a body the reply advertised as resolved under a different epoch (the
/// requester's cut would then drop it → a silent gap). The reply binds
/// `VOTING_RBSR_AAD`; each body binds `VOTING_TOPIC_AAD` (wire-identical to a
/// live/backfill event, so it passes the requester's current-epoch cut and
/// applies through the same path). Returns `[sealed_reply, body_1, …]`, or `None`
/// if epoch state is missing / any encode fails (responder then answers nothing →
/// requester falls back).
async fn voting_rbsr_seal_reply_and_bodies(
    crdt_state: &tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>,
    community_id: crate::owner_state_types::SpaceId,
    reply: &crate::channel_rbsr::RbsrMessage,
    bodies: &[Vec<u8>],
) -> Option<Vec<Vec<u8>>> {
    let plaintext_reply = crate::channel_rbsr::encode_message(reply);
    let st = crdt_state.lock().await;
    let space = st.spaces.get(&community_id)?;
    let reply_env = crate::community_state_sync::encrypt_for_topic_with_aad(
        space,
        &plaintext_reply,
        crate::community_state_sync::VOTING_RBSR_AAD,
    )
    .ok()?;
    let mut out = Vec::with_capacity(1 + bodies.len());
    let mut rw = Vec::new();
    ciborium::into_writer(&reply_env, &mut rw).ok()?;
    out.push(rw);
    for body in bodies {
        let env = crate::community_state_sync::encrypt_for_topic_with_aad(
            space,
            body,
            crate::community_state_sync::VOTING_TOPIC_AAD,
        )
        .ok()?;
        let mut bw = Vec::new();
        ciborium::into_writer(&env, &mut bw).ok()?;
        out.push(bw);
    }
    Some(out)
}

/// Outcome of one RBSR reconcile attempt against remote responders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VotingReconcileOutcome {
    /// Reconciled to convergence with a responder — no full dump needed.
    Converged,
    /// No remote RBSR responder answered round 0 (old peer / nobody online).
    NoResponder,
    /// A responder answered but the exchange failed (extra reply, round cap,
    /// seal/get failure) — fall back to the full dump.
    Failed,
}

/// ZEB-932: decide whether to run the full-dump backstop this tick. RBSR is
/// attempted every tick (cheap); the O(all-events) full dump runs only when RBSR
/// did not converge (no responder / failure) or the periodic backstop is due —
/// bounding the expensive dump to a ~`backstop_every`-tick floor while keeping
/// RBSR anti-entropy frequent. `since_full` = ticks since the last full dump.
fn need_full_dump(outcome: VotingReconcileOutcome, since_full: u32, backstop_every: u32) -> bool {
    !matches!(outcome, VotingReconcileOutcome::Converged) || since_full + 1 >= backstop_every
}

/// ZEB-932: drive one RBSR reconcile session (multiple rounds) as the requester
/// against remote responders on `rbsr_topic`. Each round seals the request as the
/// GET payload (`Locality::Remote` excludes our own responder; `Consolidation
/// ::None` streams every frame), classifies the returned frames — the one that
/// opens under `VOTING_RBSR_AAD` is the reply; the rest are inline `Have` event
/// bodies decrypted through the current-epoch cut and applied — then computes the
/// next request. Returns the outcome so the caller decides on the full-dump
/// fallback.
async fn drive_voting_rbsr(
    session: &zenoh::Session,
    rbsr_topic: &str,
    crdt_state: &tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>,
    community_id: crate::owner_state_types::SpaceId,
    hooks: &VotingRbsrHooks,
    apply_backfilled: &VotingBackfillApplyFn,
    closing: &AtomicBool,
) -> VotingReconcileOutcome {
    let mut request = (hooks.initial)().await;
    let mut round: u32 = 0;
    loop {
        if closing.load(Ordering::SeqCst) {
            return VotingReconcileOutcome::Failed;
        }
        round += 1;
        if round > crate::channel_rbsr::MAX_RBSR_ROUNDS {
            return VotingReconcileOutcome::Failed;
        }
        let Some(sealed) = voting_rbsr_seal(crdt_state, community_id, &request).await else {
            return VotingReconcileOutcome::Failed;
        };
        let receiver = match session
            .get(rbsr_topic)
            .payload(sealed)
            .consolidation(zenoh::query::ConsolidationMode::None)
            .allowed_destination(zenoh::sample::Locality::Remote)
            .timeout(std::time::Duration::from_secs(10))
            .await
        {
            Ok(r) => r,
            Err(_) => return VotingReconcileOutcome::Failed,
        };

        let mut reply: Option<crate::channel_rbsr::RbsrMessage> = None;
        let mut saw_extra_reply = false;
        let mut round_bytes = 0usize;
        loop {
            tokio::select! {
                biased;
                res = receiver.recv_async() => {
                    let Ok(r) = res else { break; };
                    let Ok(sample) = r.into_result() else { continue; };
                    let payload_len = sample.payload().len();
                    if payload_len > MAX_VOTING_PAYLOAD_BYTES { continue; }
                    round_bytes = round_bytes.saturating_add(payload_len);
                    if round_bytes > MAX_VOTING_RBSR_ROUND_BYTES {
                        return VotingReconcileOutcome::Failed;
                    }
                    let raw = sample.payload().to_bytes().to_vec();
                    // The frame that opens under VOTING_RBSR_AAD is the reply;
                    // everything else is an inline Have event body.
                    if let Some(msg) = voting_rbsr_open(crdt_state, community_id, &raw).await {
                        if reply.is_some() {
                            saw_extra_reply = true;
                        } else {
                            reply = Some(msg);
                        }
                        continue;
                    }
                    if let Some(plaintext) =
                        voting_decrypt_current_epoch_cut(crdt_state, community_id, &raw).await
                    {
                        (apply_backfilled)(plaintext).await;
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {
                    if closing.load(Ordering::SeqCst) {
                        return VotingReconcileOutcome::Failed;
                    }
                }
            }
        }

        if saw_extra_reply {
            // Multiple holders with divergent logs could falsely converge — fall
            // back to the dedup-tolerant full dump.
            return VotingReconcileOutcome::Failed;
        }
        let Some(reply_msg) = reply else {
            return if round == 1 {
                VotingReconcileOutcome::NoResponder
            } else {
                VotingReconcileOutcome::Failed
            };
        };
        match (hooks.process_reply)(reply_msg).await {
            None => return VotingReconcileOutcome::Converged,
            Some(next) => request = next,
        }
    }
}

/// ZEB-298+ZEB-312 PR 1: per-community Zenoh adapter for the VotingLog
/// data plane. Topic: `harmony/community/{id_hex}/voting` (live pub/sub).
///
/// ZEB-718: additionally spawns a **backfill responder** (a queryable on
/// `harmony/community/{id_hex}/voting/backfill` that re-encrypts the
/// engine's live events under the current epoch) and a **backfill
/// requester** (a periodic `get` that decrypts replies through the
/// current-epoch cut and applies them via the engine's coordinate-dedup
/// path). Full-dump (no RBSR, no watermark) — voting volume is sparse.
///
/// The function is fire-and-forget: `event_loop::run`'s select! arm
/// calls it and drops the `JoinHandle`. The spawned tasks exit when
/// `closing` is set, when the engine drops its publisher_tx, or when
/// Zenoh closes the subscriber. Engine teardown is driven by the
/// `VotingLogEnginesMap` lock in stop_inner (same as state-root v1).
#[allow(clippy::too_many_arguments)]
pub fn spawn_voting_log_zenoh_adapter(
    session: Arc<zenoh::Session>,
    community_id_hex: String,
    // ZEB-717: epoch-key source for adapter-side voting crypto.
    community_id: crate::owner_state_types::SpaceId,
    crdt_state: Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
    mut publisher_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    subscriber_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    // ZEB-718: backfill responder read closure + requester apply closure +
    // the periodic pull floor.
    read_for_backfill: VotingBackfillReadFn,
    apply_backfilled: VotingBackfillApplyFn,
    backfill_interval: std::time::Duration,
    // ZEB-932: optional RBSR halves — Some spawns the rbsr responder + RBSR-first
    // requester; None keeps the pure full-dump path.
    rbsr_hooks: Option<VotingRbsrHooks>,
    closing: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    let topic = format!("harmony/community/{}/voting", community_id_hex);
    let backfill_topic = format!("harmony/community/{}/voting/backfill", community_id_hex);
    let rbsr_topic = format!("harmony/community/{}/voting/rbsr", community_id_hex);

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

        // Outbound: drain engine's publisher_rx → encrypt → Zenoh put.
        let session_pub = Arc::clone(&session);
        let key_pub = key_expr.clone();
        let topic_pub = topic.clone();
        let closing_pub = Arc::clone(&closing);
        let crdt_state_pub = Arc::clone(&crdt_state);
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
                        let Some(plaintext) = maybe else { break; };
                        // ZEB-717: encrypt the engine's plaintext SignedVotingEvent
                        // CBOR under the community's current epoch key (+ voting AAD)
                        // before it reaches the wire. Missing epoch state ⇒ drop the
                        // outbound (a node without the key is not a broadcasting
                        // member; the engine already applied locally).
                        let wire: Option<Vec<u8>> = {
                            let st = crdt_state_pub.lock().await;
                            match st.spaces.get(&community_id) {
                                Some(space) => match crate::community_state_sync::encrypt_for_topic_with_aad(
                                    space,
                                    &plaintext,
                                    crate::community_state_sync::VOTING_TOPIC_AAD,
                                ) {
                                    Ok(envelope) => {
                                        let mut w = Vec::new();
                                        match ciborium::into_writer(&envelope, &mut w) {
                                            Ok(()) => Some(w),
                                            Err(e) => {
                                                tracing::warn!(
                                                    topic = %topic_pub,
                                                    error = %e,
                                                    "voting envelope encode failed; dropping outbound"
                                                );
                                                None
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            topic = %topic_pub,
                                            error = %e,
                                            "voting encrypt failed; dropping outbound"
                                        );
                                        None
                                    }
                                },
                                None => {
                                    tracing::warn!(
                                        topic = %topic_pub,
                                        "no community space for voting encrypt; dropping outbound"
                                    );
                                    None
                                }
                            }
                        };
                        if let Some(wire) = wire {
                            if let Err(e) = session_pub.put(&key_pub, wire).await {
                                if !closing_pub.load(Ordering::SeqCst) {
                                    tracing::warn!(
                                        topic = %topic_pub,
                                        error = %e,
                                        "community voting-log publish failed"
                                    );
                                }
                            }
                        }
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                        if closing_pub.load(Ordering::SeqCst) { break; }
                    }
                }
            }
        });

        // ── ZEB-718 backfill responder (queryable) ──────────────────────
        // On each query, read the engine's live events (plaintext frames)
        // and reply one per event, re-encrypted under the CURRENT epoch so
        // the requester's current-epoch cut accepts them (this is what
        // recovers a cross-rotation-dropped vote — ZEB-717 §2.1).
        let session_qbl = Arc::clone(&session);
        let crdt_state_qbl = Arc::clone(&crdt_state);
        let closing_qbl = Arc::clone(&closing);
        let backfill_topic_qbl = backfill_topic.clone();
        let qbl_handle = tokio::spawn(async move {
            let bf_key = match zenoh::key_expr::KeyExpr::try_from(backfill_topic_qbl.clone()) {
                Ok(k) => k,
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        topic = %backfill_topic_qbl,
                        "voting backfill key_expr invalid; responder skipped"
                    );
                    return;
                }
            };
            let qbl = match session_qbl.declare_queryable(&bf_key).await {
                Ok(q) => q,
                Err(e) => {
                    if !closing_qbl.load(Ordering::SeqCst) {
                        tracing::warn!(
                            error = %e,
                            topic = %backfill_topic_qbl,
                            "failed to declare voting backfill queryable"
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
                        let frames = (read_for_backfill)().await;
                        let mut served_frames = 0usize;
                        let mut served_bytes = 0usize;
                        for frame in frames {
                            // Re-encrypt under the CURRENT epoch at serve time.
                            let Some(wire) = voting_encrypt_current_epoch(
                                &crdt_state_qbl, community_id, &frame,
                            ).await else {
                                // No current epoch state — nothing serveable.
                                break;
                            };
                            let wire_len = wire.len();
                            if let Err(e) = query.reply(query.key_expr(), wire).await {
                                tracing::debug!(
                                    topic = %backfill_topic_qbl,
                                    error = %e,
                                    "voting backfill reply failed"
                                );
                                break;
                            }
                            // Count only frames that actually left the node — a None
                            // encrypt or a failed reply breaks out uncounted — so the
                            // Q1 numbers reflect bytes truly served, not attempted.
                            served_frames += 1;
                            served_bytes += wire_len;
                        }
                        // ZEB-916 Q1: full-dump volume served to one requester.
                        tracing::info!(
                            target: "harmony_volume",
                            kind = "voting_backfill_serve",
                            topic = %backfill_topic_qbl,
                            frames = served_frames,
                            served_bytes,
                            "voting backfill dump served"
                        );
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                        if closing_qbl.load(Ordering::SeqCst) { break; }
                    }
                }
            }
        });

        // ── ZEB-932 RBSR responder (queryable) ──────────────────────────
        // When RBSR hooks are present, answer voting/rbsr GETs: open the
        // sealed request (VOTING_RBSR_AAD + current-epoch cut), run the
        // engine's respond half, and stream back [sealed_reply, body_1, …] —
        // reply under VOTING_RBSR_AAD, bodies under VOTING_TOPIC_AAD, all
        // sealed under ONE epoch snapshot (ZEB-920). A payload-less or
        // unopenable GET, or a respond/seal miss, replies nothing (the
        // requester reads no-reply as no-responder → full-dump fallback).
        let rbsr_resp_handle = rbsr_hooks.as_ref().map(|hooks| {
            let session_rbsr = Arc::clone(&session);
            let crdt_state_rbsr = Arc::clone(&crdt_state);
            let closing_rbsr = Arc::clone(&closing);
            let rbsr_topic_resp = rbsr_topic.clone();
            let respond = hooks.respond.clone();
            tokio::spawn(async move {
                let key = match zenoh::key_expr::KeyExpr::try_from(rbsr_topic_resp.clone()) {
                    Ok(k) => k,
                    Err(e) => {
                        tracing::error!(
                            error = %e,
                            topic = %rbsr_topic_resp,
                            "voting rbsr key_expr invalid; responder skipped"
                        );
                        return;
                    }
                };
                let qbl = match session_rbsr.declare_queryable(&key).await {
                    Ok(q) => q,
                    Err(e) => {
                        if !closing_rbsr.load(Ordering::SeqCst) {
                            tracing::warn!(
                                error = %e,
                                topic = %rbsr_topic_resp,
                                "failed to declare voting rbsr queryable"
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
                            let Some(payload) = query.payload() else { continue; };
                            if payload.len() > MAX_VOTING_PAYLOAD_BYTES { continue; }
                            let raw = payload.to_bytes().to_vec();
                            let Some(request) =
                                voting_rbsr_open(&crdt_state_rbsr, community_id, &raw).await
                            else {
                                continue;
                            };
                            let Some((reply_msg, bodies)) = (respond)(request).await else {
                                continue;
                            };
                            let Some(frames) = voting_rbsr_seal_reply_and_bodies(
                                &crdt_state_rbsr, community_id, &reply_msg, &bodies,
                            )
                            .await
                            else {
                                continue;
                            };
                            for wire in frames {
                                if query.reply(query.key_expr(), wire).await.is_err() {
                                    break;
                                }
                            }
                        }
                        _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                            if closing_rbsr.load(Ordering::SeqCst) { break; }
                        }
                    }
                }
            })
        });

        // ── ZEB-718 backfill requester (ZEB-932: RBSR-first) ────────────
        // Pulls on spawn (join/reconnect catch-up) and every
        // `backfill_interval` (anti-entropy floor). Each reply is decrypted
        // through the current-epoch cut and applied via the engine's
        // coordinate-dedup path (recovering in-lane gaps).
        let session_req = Arc::clone(&session);
        let crdt_state_req = Arc::clone(&crdt_state);
        let closing_req = Arc::clone(&closing);
        let backfill_topic_req = backfill_topic.clone();
        let rbsr_topic_req = rbsr_topic.clone();
        let req_handle = tokio::spawn(async move {
            // ZEB-932: RBSR-first anti-entropy. RBSR runs every tick (cheap); the
            // O(all-events) full dump runs only when RBSR didn't converge (no
            // responder / failure) or the periodic backstop is due
            // (VOTING_FULL_DUMP_BACKSTOP_TICKS × backfill_interval ≈ 1 h).
            let mut since_full: u32 = VOTING_FULL_DUMP_BACKSTOP_TICKS;
            loop {
                // 1) RBSR reconcile attempt — converges the common case in a few
                // small rounds instead of re-shipping the whole log.
                let outcome = if let Some(hooks) = rbsr_hooks.as_ref() {
                    drive_voting_rbsr(
                        &session_req,
                        &rbsr_topic_req,
                        &crdt_state_req,
                        community_id,
                        hooks,
                        &apply_backfilled,
                        &closing_req,
                    )
                    .await
                } else {
                    VotingReconcileOutcome::NoResponder
                };
                if closing_req.load(Ordering::SeqCst) {
                    return;
                }

                // 2) Full-dump backstop / fallback.
                if need_full_dump(outcome, since_full, VOTING_FULL_DUMP_BACKSTOP_TICKS) {
                    since_full = 0;
                    // One full-dump pull. ConsolidationMode::None streams every
                    // per-event reply; Locality::Remote excludes our own
                    // responder's self-reply.
                    match session_req
                        .get(backfill_topic_req.as_str())
                        .consolidation(zenoh::query::ConsolidationMode::None)
                        .allowed_destination(zenoh::sample::Locality::Remote)
                        // Bound the query so a hung/never-completing round can't
                        // stall anti-entropy forever (mirrors the RBSR get path).
                        // ZEB-812 audit note: this drain awaits `apply_backfilled`
                        // inline (an app-side await inside the reply arm), which
                        // is the shape that wedged the channel-log drain — but
                        // here the 10s query timeout above closes the reply
                        // stream regardless, so a slow apply bounds one voting
                        // round at 10s instead of parking zenoh indefinitely.
                        // Left as-is deliberately; a spill would buy little (the
                        // payloads are applied, not forwarded to a channel).
                        .timeout(std::time::Duration::from_secs(10))
                        .await
                    {
                        Ok(receiver) => {
                            let mut recv_frames = 0usize;
                            let mut recv_bytes = 0usize;
                            loop {
                                tokio::select! {
                                    biased;
                                    res = receiver.recv_async() => {
                                        let Ok(reply) = res else { break; };
                                        if let Ok(sample) = reply.into_result() {
                                            // Cap peer-controlled reply payloads before
                                            // materializing — parity with the live
                                            // subscriber's allocation-DoS guard.
                                            let payload_len = sample.payload().len();
                                            if payload_len > MAX_VOTING_PAYLOAD_BYTES {
                                                continue;
                                            }
                                            recv_frames += 1;
                                            recv_bytes += payload_len;
                                            let raw = sample.payload().to_bytes().to_vec();
                                            if let Some(plaintext) = voting_decrypt_current_epoch_cut(
                                                &crdt_state_req, community_id, &raw,
                                            ).await {
                                                (apply_backfilled)(plaintext).await;
                                            }
                                        }
                                    }
                                    _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {
                                        if closing_req.load(Ordering::SeqCst) { return; }
                                    }
                                }
                            }
                            // ZEB-916 Q1: the multi-responder fan-in
                            // (ConsolidationMode::None → every remote responder's
                            // full dump arrives) is the dominant voting-sync term and
                            // is invisible from the responder side alone.
                            tracing::info!(
                                target: "harmony_volume",
                                kind = "voting_backfill_recv",
                                topic = %backfill_topic_req,
                                frames = recv_frames,
                                recv_bytes,
                                "voting backfill dump received"
                            );
                        }
                        Err(e) => {
                            if !closing_req.load(Ordering::SeqCst) {
                                tracing::debug!(
                                    topic = %backfill_topic_req,
                                    error = %e,
                                    "voting backfill get failed"
                                );
                            }
                        }
                    }
                } else {
                    since_full = since_full.saturating_add(1);
                }
                // Wait `backfill_interval` before the next pull, waking every
                // second so a flipped `closing` unblocks teardown promptly.
                let step = std::time::Duration::from_secs(1);
                let mut remaining = backfill_interval;
                loop {
                    if closing_req.load(Ordering::SeqCst) {
                        return;
                    }
                    if remaining.is_zero() {
                        break;
                    }
                    let this = remaining.min(step);
                    tokio::time::sleep(this).await;
                    remaining = remaining.saturating_sub(this);
                }
            }
        });

        // Inbound: Zenoh subscriber → decrypt (current-epoch-only) → engine's subscriber_tx.
        let session_sub = session;
        let key_sub = key_expr;
        let topic_sub = topic;
        let closing_sub = Arc::clone(&closing);
        let crdt_state_sub = crdt_state;
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
                                let raw: Vec<u8> = sample.payload().to_bytes().to_vec();
                                // ZEB-717: decrypt at the wire boundary with the
                                // current-epoch-only cut (spec §3 D3). A kicked-then-
                                // rotated member holds only a stale epoch key, so its
                                // envelope's epoch != current_epoch and is dropped here —
                                // even though this node still retains that old key in
                                // old_epoch_keys. The engine downstream sees plaintext
                                // SignedVotingEvent CBOR exactly as before.
                                let plaintext: Vec<u8> = {
                                    let envelope: crate::community_state_sync::EncryptedEnvelope =
                                        match ciborium::from_reader(raw.as_slice()) {
                                            Ok(env) => env,
                                            Err(e) => {
                                                // debug, not warn: a mesh peer (incl. the kicked
                                                // member this change contains) can spam malformed
                                                // envelopes — one warn/packet would flood logs.
                                                tracing::debug!(
                                                    topic = %topic_sub,
                                                    error = %e,
                                                    "drop voting packet (envelope decode)"
                                                );
                                                continue;
                                            }
                                        };
                                    let st = crdt_state_sub.lock().await;
                                    let Some(space) = st.spaces.get(&community_id) else {
                                        continue;
                                    };
                                    match space.current_epoch {
                                        Some(cur) if cur == envelope.epoch => {}
                                        _ => {
                                            // debug, not warn: stale-epoch drops are both the
                                            // attack-containment path AND expected for legit votes
                                            // in flight across a rotation — warn would flood.
                                            tracing::debug!(
                                                topic = %topic_sub,
                                                epoch = envelope.epoch,
                                                "drop voting packet (stale/unknown epoch)"
                                            );
                                            continue;
                                        }
                                    }
                                    match crate::community_state_sync::decrypt_for_topic_with_aad(
                                        space,
                                        &envelope,
                                        crate::community_state_sync::VOTING_TOPIC_AAD,
                                    ) {
                                        Ok(pt) => pt,
                                        Err(e) => {
                                            // debug, not warn: tag mismatch = tamper / cross-plane
                                            // replay from a peer — attacker-controllable, so one
                                            // warn/packet would flood.
                                            tracing::debug!(
                                                topic = %topic_sub,
                                                error = %e,
                                                "drop voting packet (decrypt)"
                                            );
                                            continue;
                                        }
                                    }
                                };
                                if subscriber_tx.send(plaintext).await.is_err() {
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
        let _ = qbl_handle.await;
        let _ = req_handle.await;
        if let Some(h) = rbsr_resp_handle {
            let _ = h.await;
        }
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
    rbsr_hooks: Option<RbsrAdapterHooks>,
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
            Option<Vec<u8>>,
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
    // ZEB-593: a dedicated query family, declared only when RBSR hooks are
    // present. Kept separate from `since/**` (that queryable strips the GET
    // payload on the `since=None` branch, which would break RBSR's payload).
    let rbsr_queryable_prefix = format!(
        "harmony/channels/{}/{}/rbsr/**",
        community_id_hex, channel_id_hex
    );
    // Split the hooks into the responder half (queryable task) and the
    // requester halves (query-request driver).
    let rbsr_respond_qbl = rbsr_hooks.as_ref().map(|h| Arc::clone(&h.respond));
    let rbsr_initial_qr = rbsr_hooks.as_ref().map(|h| Arc::clone(&h.initial));
    let rbsr_ingest_qr = rbsr_hooks.as_ref().map(|h| Arc::clone(&h.ingest));

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
                        // ZEB-585: an optional AEAD-sealed watermark vector
                        // rides as the GET request payload. `since == None`
                        // is the full-reconcile sentinel — IGNORE any payload
                        // then, so a malformed or hostile peer can't downgrade
                        // a full reconcile to a partial diff. Cap-before-alloc
                        // on the ZBytes length (mirrors the pairing-scope
                        // guard); over cap → ignore it and serve the key-expr
                        // scalar `since`.
                        let watermark_sealed = if since.is_some() {
                            query.payload().and_then(|p| {
                                if p.len()
                                    > crate::community_channel_log::MAX_WATERMARK_VECTOR_BYTES
                                {
                                    tracing::debug!(
                                        %qkey,
                                        len = p.len(),
                                        "channel-log watermark vector over cap; serving scalar"
                                    );
                                    None
                                } else {
                                    Some(p.to_bytes().to_vec())
                                }
                            })
                        } else {
                            None
                        };
                        let packets =
                            (read_for_query_qbl)(since, limit, watermark_sealed).await;
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

        // ── RBSR queryable task (ZEB-593) ──────────────────────────
        // Declared only when RBSR hooks are present. Answers GETs on
        // `…/rbsr/**`: opens the sealed request, computes the reply, and streams
        // `[sealed_reply, have_packet, …]` under ConsolidationMode::None.
        // Stateless per-GET — the round number in the key is a trace hint; all
        // state rides in the payload.
        if let Some(rbsr_respond) = rbsr_respond_qbl {
            let session_rbsr = Arc::clone(&session);
            let key_rbsr = rbsr_queryable_prefix.clone();
            let closing_rbsr = Arc::clone(&closing);
            let _rbsr_qbl_handle = tokio::spawn(async move {
                let qbl = match session_rbsr.declare_queryable(&key_rbsr).await {
                    Ok(q) => q,
                    Err(e) => {
                        if !closing_rbsr.load(Ordering::SeqCst) {
                            tracing::error!(
                                prefix = %key_rbsr,
                                error = %e,
                                "failed to declare rbsr queryable"
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
                            if parse_rbsr_key(&qkey).is_none() {
                                tracing::debug!(%qkey, "ignoring malformed rbsr selector");
                                continue;
                            }
                            // Cap-before-alloc on the request payload (mirrors
                            // open_rbsr_message's pre-decrypt cap).
                            let sealed_request = match query.payload() {
                                Some(p)
                                    if p.len()
                                        <= crate::community_channel_log::MAX_RBSR_MESSAGE_BYTES =>
                                {
                                    p.to_bytes().to_vec()
                                }
                                Some(p) => {
                                    tracing::debug!(%qkey, len = p.len(), "rbsr request over cap; ignoring");
                                    continue;
                                }
                                // RBSR requires a payload; a payload-less GET is
                                // not a valid round → reply nothing.
                                None => continue,
                            };
                            if let Some((sealed_reply, have_packets)) =
                                (rbsr_respond)(sealed_request).await
                            {
                                let mut frames = Vec::with_capacity(1 + have_packets.len());
                                frames.push(sealed_reply);
                                frames.extend(have_packets);
                                for frame in frames {
                                    if let Err(e) = query.reply(query.key_expr(), frame).await {
                                        tracing::warn!(
                                            prefix = %key_rbsr,
                                            error = %e,
                                            "rbsr queryable reply failed"
                                        );
                                        break;
                                    }
                                }
                            }
                            // `None` → reply nothing; the requester sees zero
                            // frames and falls back to the vector path.
                        }
                        _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                            if closing_rbsr.load(Ordering::SeqCst) { break; }
                        }
                    }
                }
            });
        }

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
                        let Some(mut req) = maybe else { break; };
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
                        // ZEB-593: RBSR-first. When hooks are present, attempt a
                        // multi-round RBSR reconcile before the watermark GET.
                        // Done → events already ingested via the inbound path;
                        // report a 0-event completed page so the backfill driver
                        // stops paging. VectorFallback (no rbsr/** responder) →
                        // proceed with the watermark GET as-is. FullReconcile
                        // (round cap) → force a full `since=None` reconcile.
                        if let (Some(initial), Some(ingest)) =
                            (&rbsr_initial_qr, &rbsr_ingest_qr)
                        {
                            let session_rb = Arc::clone(&session_qr);
                            let cid_rb = community_id_hex_qr.clone();
                            let ch_rb = channel_id_hex_qr.clone();
                            let closing_rb = Arc::clone(&closing_qr);
                            let (mode, transferred) = drive_rbsr_rounds(
                                || (initial)(),
                                move |round, sealed| {
                                    let session = Arc::clone(&session_rb);
                                    let key = format_rbsr_key(&cid_rb, &ch_rb, round);
                                    let closing = Arc::clone(&closing_rb);
                                    async move {
                                        rbsr_get_frames(&session, &key, sealed, &closing).await
                                    }
                                },
                                |frames| (ingest)(frames),
                            )
                            .await;
                            match mode {
                                crate::channel_backfill::ReconcileMode::Done => {
                                    // The RBSR path bypasses the since-drain
                                    // progress emitter; fire one terminal tick
                                    // so §14.2 (≥1 backfill-progress event) holds
                                    // and the UI sees the transferred count.
                                    (emit_backfill_progress_qr)(
                                        transferred as u32,
                                        Some(transferred as u32),
                                    );
                                    if let Some(tx) = req.outcome_tx.take() {
                                        let _ = tx.send(
                                            crate::community_channel_log_engine::BackfillPageReport {
                                                replies: 0,
                                                limit,
                                            },
                                        );
                                    }
                                    continue;
                                }
                                crate::channel_backfill::ReconcileMode::FullReconcile => {
                                    req.since = None;
                                    req.watermark_sealed = None;
                                }
                                // VectorFallback (or the never-returned
                                // RbsrContinue) → use the watermark GET below.
                                _ => {}
                            }
                        }
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
                        // ZEB-585: forward the engine-sealed per-author
                        // watermark vector (if any) as the GET request
                        // payload; the queryable opens it with the channel
                        // key. Old responders ignore the payload and use the
                        // key-expr scalar `since` — no wire break.
                        let mut get_builder = session_qr
                            .get(&key)
                            .consolidation(zenoh::query::ConsolidationMode::None);
                        if let Some(bytes) = req.watermark_sealed.take() {
                            get_builder = get_builder.payload(bytes);
                        }
                        let receiver = match get_builder.await {
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
                        //
                        // ZEB-812: this loop must NEVER await subscriber_tx.
                        // A `send().await` here holds zenoh's reply channel
                        // hostage while waiting on engine backpressure —
                        // the reply channel fills, zenoh's single net
                        // thread parks in `flume wait_send<Reply>`, and the
                        // node's ENTIRE zenoh transport wedges (ZEB-803).
                        // The parked await also starves the closing-poll
                        // arm below. Replies go through a local spill
                        // (capped — one GET's volume is limit × responders,
                        // and the responder count is unbounded) with
                        // try_send forwarding; the blocking delivery
                        // happens in `flush()` after the stream closes.
                        let mut spill = crate::reply_spill::ReplySpill::new(
                            subscriber_tx_qr.clone(),
                            CHANNEL_BACKFILL_SPILL_MAX,
                        );
                        let drained_clean: bool = loop {
                            tokio::select! {
                                biased;
                                res = receiver.recv_async() => {
                                    let Ok(reply) = res else { break true; };
                                    if let Ok(sample) = reply.into_result() {
                                        let bytes: Vec<u8> = sample.payload().to_bytes().to_vec();
                                        match spill.accept(bytes) {
                                            crate::reply_spill::AcceptOutcome::Accepted => {}
                                            crate::reply_spill::AcceptOutcome::DroppedFull => {
                                                // Cap overflow (reply storm):
                                                // dropped + counted; surfaced
                                                // after the drain. Not counted
                                                // in `fetched` — progress and
                                                // the page report describe
                                                // what will actually land.
                                                continue;
                                            }
                                            crate::reply_spill::AcceptOutcome::ConsumerGone => {
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
                                        }
                                        fetched = fetched.saturating_add(1);
                                        // Spec §10: emit channel-backfill-progress
                                        // every N replies. `total_estimate` is
                                        // `None` — we don't know the total until
                                        // the receiver closes (Zenoh streams
                                        // replies one-at-a-time). `fetched` now
                                        // counts replies pulled off the zenoh
                                        // stream (ZEB-812), which may run ahead
                                        // of what the engine has absorbed.
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
                        if spill.dropped() > 0 {
                            tracing::warn!(%key, dropped = spill.dropped(),
                                "channel backfill: reply storm exceeded spill cap; \
                                 overflow dropped (RBSR / next backfill round re-fetches)");
                        }
                        // ZEB-816: peak buffer depth against the cap, on the
                        // same `harmony_channel` target as the "backfill page
                        // completed" line — one grep now shows reply volume
                        // AND buffer pressure, telling a real storm (peak near
                        // cap) from a never-exercised spill (peak 0). Captured
                        // before flush() consumes the spill.
                        tracing::debug!(
                            target: "harmony_channel",
                            %key,
                            replies = fetched,
                            spill_peak = spill.peak(),
                            spill_cap = CHANNEL_BACKFILL_SPILL_MAX,
                            "channel backfill: reply drain complete"
                        );
                        // ZEB-812: the zenoh stream is closed; deliver what
                        // the consumer hasn't absorbed yet. Blocking on the
                        // engine HERE is the intended request-level
                        // backpressure (no next request until this page
                        // lands), and flush() keeps the closing poll live.
                        // ShutdownAbandoned matches the !drained_clean
                        // no-report shutdown semantics.
                        let flushed_clean = drained_clean
                            && match spill.flush(&closing_qr).await {
                                crate::reply_spill::FlushOutcome::Flushed => true,
                                crate::reply_spill::FlushOutcome::ConsumerGone => return,
                                crate::reply_spill::FlushOutcome::ShutdownAbandoned => false,
                            };
                        // Spec §10: emit a final progress tick at end-of-
                        // request. We always fire on a clean drain+flush
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
                        // flipped is racy noise. ZEB-812: the gate is
                        // `flushed_clean` — a report only fires once the
                        // page has actually LANDED in the engine channel,
                        // preserving the pre-spill meaning of a report
                        // for BackfillLatch callers.
                        if flushed_clean {
                            (emit_backfill_progress_qr)(fetched, Some(fetched));
                            // ZEB-418 P3a: page-completion report for
                            // callers that asked (BackfillLatch).
                            // `replies` = raw packets drained off the
                            // reply stream (pre-verification); `limit`
                            // = the effective clamped limit the GET
                            // selector above was built with. Send-
                            // error (receiver dropped) is fine —
                            // fire-and-forget callers. On the
                            // !drained_clean shutdown path `req` (and
                            // the sender) drop without a report, so
                            // the receiver observes a closed channel
                            // = "query aborted".
                            if let Some(tx) = req.outcome_tx.take() {
                                let _ = tx.send(
                                    crate::community_channel_log_engine::BackfillPageReport {
                                        replies: fetched as usize,
                                        limit,
                                    },
                                );
                            }
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

/// Serve-gate predicate (ZEB-395): a CID is servable iff it is unencrypted OR it
/// is an allowlisted community-root CID. Shared by the queryable loop and its
/// unit tests so the two can never drift.
fn content_cid_servable(
    cid: &ContentId,
    serve_allowlist: &crate::content_store::CommunityServeAllowlist,
) -> bool {
    !cid.flags().encrypted || serve_allowlist.contains(cid)
}

/// ZEB-343: the peer-to-peer CAS serve primitive. Declares a single Zenoh
/// queryable on `harmony/content/*/**` and answers content GETs for servable
/// CIDs held in the local store: all unencrypted CIDs plus allowlisted encrypted
/// community-root CIDs (opted in via `ContentStore::put_serveable`). Private
/// encrypted blobs (DMs, private profiles) are never allowlisted and get no
/// reply.
///
/// `lookup` is the local-store accessor (production wires it to a
/// `CasOp::GetLocal` round-trip; tests wire a HashMap) — passed in to avoid an
/// engine↔adapter circular dep, exactly like channel-log's `read_for_query`
/// (event_loop.rs:4073).
///
/// Serve gate (ZEB-395): each request is filtered through `content_cid_servable`
/// (see that fn) before any reply.
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
    serve_allowlist: crate::content_store::CommunityServeAllowlist,
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
                    if !content_cid_servable(&cid, &serve_allowlist) {
                        continue; // private encrypted content stays unservable
                    }
                    let Some(bytes) = (lookup)(cid).await else {
                        continue;
                    };
                    if !cid.verify_hash(&bytes) {
                        tracing::warn!(%qkey, "content-serve: local bytes failed hash==cid; not serving");
                        continue;
                    }
                    let serve_bytes = bytes.len();
                    match query.reply(query.key_expr(), bytes).await {
                        Ok(()) => {
                            // ZEB-922: a successful serve is demonstrated
                            // demand — refresh the lease (no-op for
                            // unencrypted CIDs, which are never in the map).
                            // Also the first success-path observability here.
                            serve_allowlist.touch(&cid);
                            tracing::debug!(%qkey, "content-serve: served");
                            // ZEB-916 Q1: blob-transfer volume (all content
                            // types; state-root segments are the bulk).
                            tracing::info!(
                                target: "harmony_volume",
                                kind = "content_serve",
                                %qkey,
                                serve_bytes,
                                "content blob served"
                            );
                        }
                        Err(e) => {
                            tracing::warn!(%qkey, error = %e, "content-serve reply failed");
                        }
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

#[cfg(test)]
mod content_serve_gate_tests {
    // Exercise the PRODUCTION predicate `content_cid_servable` (the same fn the
    // queryable loop calls), so the test can never drift from the real gate.
    use super::content_cid_servable;
    use crate::content_store::CommunityServeAllowlist;
    use harmony_content::cid::{ContentFlags, ContentId};

    #[test]
    fn gate_serves_unencrypted_always() {
        let cid = ContentId::for_book(b"pub", ContentFlags::default()).unwrap();
        let allow = CommunityServeAllowlist::new();
        assert!(content_cid_servable(&cid, &allow));
    }

    #[test]
    fn gate_refuses_encrypted_unless_allowlisted() {
        let enc = ContentFlags {
            encrypted: true,
            ..ContentFlags::default()
        };
        let cid = ContentId::for_book(b"sec", enc).unwrap();
        let allow = CommunityServeAllowlist::new();
        assert!(
            !content_cid_servable(&cid, &allow),
            "encrypted + not allowlisted => refuse"
        );
        allow.allow(cid);
        assert!(
            content_cid_servable(&cid, &allow),
            "encrypted + allowlisted => serve"
        );
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

/// ZEB-812: cap on the channel-backfill reply spill (`reply_spill` module).
/// One GET's reply volume is `limit × responders` under
/// `ConsolidationMode::None`, and the responder count is unbounded (every
/// community member may answer), so the spill needs its own explicit bound:
/// 8 pages ≈ 8 simultaneous responders at the max page limit before overflow
/// drops kick in (dropped replies are re-fetched by RBSR / the next round).
const CHANNEL_BACKFILL_SPILL_MAX: usize = 8 * CHANNEL_BACKFILL_MAX_LIMIT;

/// ZEB-812: cap on the root-fetch reply spills (owner-state + community
/// state-root drivers). Roots are one small wire (≤ MAX_ROOT_WIRE_BYTES,
/// enforced at the owner-state site) per responder per GET, so 1024 pending
/// covers three orders of magnitude more responders than any real fleet
/// before overflow drops kick in.
const ROOT_FETCH_SPILL_MAX: usize = 1024;

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

/// Build the RBSR query key `"harmony/channels/{cid}/{ch}/rbsr/{round}"`.
/// `{round}` is a routing/trace hint only — every RBSR round parameter rides
/// in the GET payload (the sealed `RbsrMessage`).
fn format_rbsr_key(cid_hex: &str, ch_hex: &str, round: u32) -> String {
    format!("harmony/channels/{cid_hex}/{ch_hex}/rbsr/{round}")
}

/// Parse the `{round}` out of an RBSR query key, returning `None` when the
/// selector is not a well-formed `…/rbsr/{round}` key. cid/ch are not
/// re-validated here — Zenoh already routed the query by the registered
/// per-channel `…/rbsr/**` key expression; this only guards the literal and a
/// numeric round so a malformed (or wrong-family, e.g. `…/since/…`) key is
/// skipped rather than mis-served.
fn parse_rbsr_key(key: &str) -> Option<u32> {
    // harmony / channels / {cid} / {ch_id} / rbsr / {round}
    //    0         1          2       3        4       5
    let parts: Vec<&str> = key.split('/').collect();
    // Exactly 6 segments — reject trailing junk (`…/rbsr/0/extra`) so the
    // queryable never answers an unintended selector.
    if parts.len() != 6 || parts[4] != "rbsr" {
        return None;
    }
    parts[5].parse::<u32>().ok()
}

/// One requester-side RBSR step outcome, returned by the engine's
/// ingest-and-advance step after a round's reply frames are processed.
/// `pub` because it surfaces through the `pub` [`RbsrAdapterHooks::ingest`]
/// closure type that crosses the adapter bridge. The success variants carry
/// `ingested` — the count of `Have` event packets actually ingested this round
/// — so the driver's progress accounting reflects real transfers, not the raw
/// frame count (which a multi-responder GET inflates with extra reply frames).
#[derive(Debug)]
pub enum RbsrStep {
    /// The reconcile converged — no ranges mismatch; catch-up is complete.
    Converged { ingested: usize },
    /// More rounds needed; carries the next round's sealed request message.
    Continue { ingested: usize, next: Vec<u8> },
    /// The reply could not be opened/processed (decrypt/decode failure or a
    /// resolution shortfall) — abandon RBSR and fall back to the vector path.
    Failed,
}

/// Drive the multi-round, requester-pull RBSR catch-up to a terminal
/// [`crate::channel_backfill::ReconcileMode`]. The three closures keep the two
/// trust domains apart: the **engine** seals/opens/ingests (it holds the
/// channel key + the reconcile source) via `rbsr_initial` / `rbsr_ingest_and_next`;
/// the **adapter** performs the network round-trip (it holds the Zenoh session)
/// via `rbsr_get`. This function only shuttles opaque sealed bytes between them
/// and applies the committed reconcile-mode policy:
///
/// - round 0 draws zero replies → `VectorFallback` (no `rbsr/**` responder).
/// - a round reports converged → `Done`.
/// - a reply fails to open/process → `VectorFallback` (AEAD authenticity gives
///   malformed/tampered fallback for free).
/// - the round cap is reached without converging → `FullReconcile`.
///
/// Returns `(mode, transferred)` where `transferred` is the count of inline
/// `Have` event packets pulled across all rounds (the reply-message frame per
/// round is excluded) — the caller uses it to emit a backfill-progress event,
/// since the RBSR path bypasses the `since/**` drain loop that normally counts.
async fn drive_rbsr_rounds<InitFut, GetFut, NextFut>(
    rbsr_initial: impl Fn() -> InitFut,
    rbsr_get: impl Fn(u32, Vec<u8>) -> GetFut,
    rbsr_ingest_and_next: impl Fn(Vec<Vec<u8>>) -> NextFut,
) -> (crate::channel_backfill::ReconcileMode, usize)
where
    InitFut: std::future::Future<Output = Vec<u8>>,
    GetFut: std::future::Future<Output = Vec<Vec<u8>>>,
    NextFut: std::future::Future<Output = RbsrStep>,
{
    use crate::channel_backfill::{
        reconcile_mode_after_round, reconcile_mode_after_round0, ReconcileMode,
    };
    let mut sealed = rbsr_initial().await;
    let mut transferred = 0usize;
    for round in 0..crate::channel_rbsr::MAX_RBSR_ROUNDS {
        let frames = rbsr_get(round, sealed).await;
        if round == 0 && reconcile_mode_after_round0(frames.len()) == ReconcileMode::VectorFallback
        {
            return (ReconcileMode::VectorFallback, transferred);
        }
        // The engine classifies frames (reply vs Have packets), so `ingested`
        // is the real count of events pulled this round — not the raw frame
        // count (which a multi-responder GET inflates with extra reply frames).
        let (converged, next) = match rbsr_ingest_and_next(frames).await {
            RbsrStep::Failed => return (ReconcileMode::VectorFallback, transferred),
            RbsrStep::Converged { ingested } => {
                transferred += ingested;
                (true, None)
            }
            RbsrStep::Continue { ingested, next } => {
                transferred += ingested;
                (false, Some(next))
            }
        };
        if reconcile_mode_after_round(round, converged) == ReconcileMode::Done {
            return (ReconcileMode::Done, transferred);
        }
        sealed = match next {
            Some(next) => next,
            // Converged was already turned into `Done` above; fall back rather
            // than panic if the policy ever changes out from under this.
            None => return (ReconcileMode::Done, transferred),
        };
    }
    // Exhausted the round cap without converging → full-reconcile safety net.
    (ReconcileMode::FullReconcile, transferred)
}

/// Per-round buffer ceiling for the RBSR requester. Under `ConsolidationMode::None`
/// a round can draw frames from multiple remote holders; the 10s timeout bounds
/// *time*, not *memory*, so a buggy/malicious peer could otherwise force a large
/// allocation before ingest. 16 MiB is generous for any legitimate round (tens
/// of thousands of small event packets) yet bounds the worst case — over the cap,
/// the round aborts and the driver drops to the paged vector path.
const MAX_RBSR_ROUND_BYTES: usize = 16 * 1024 * 1024;
/// Per-frame overhead charged toward [`MAX_RBSR_ROUND_BYTES`] so that a flood of
/// tiny/empty frames is bounded by frame *count* too (~256k frames at this rate),
/// not just total payload bytes.
const RBSR_FRAME_OVERHEAD: usize = 64;

/// ZEB-593: issue one RBSR round GET and drain its reply frames. The 10s
/// per-round timeout is mandatory (the `since/**` GET has none — fine for a
/// one-shot, fatal if a peer answers round 0 then hangs round 1). Frames are
/// returned unordered (the engine classifies each as the sealed reply vs a
/// `Have` packet — see `rbsr_ingest_and_next`); an empty vec means no responder
/// answered (or the per-round byte cap was hit) → the driver falls back.
async fn rbsr_get_frames(
    session: &zenoh::Session,
    key: &str,
    sealed: Vec<u8>,
    closing: &AtomicBool,
) -> Vec<Vec<u8>> {
    let receiver = match session
        .get(key)
        .payload(sealed)
        // Reconcile only against REMOTE responders: the requester also declares
        // an `rbsr/**` queryable, and its own all-`Skip` self-reply (it built
        // the request over its own log) would otherwise be mixed into the round
        // and could force premature convergence. Excluding self also makes "zero
        // replies on round 0" cleanly mean "no remote responder" → vector
        // fallback. Unlike the dedup-tolerant `since/**` path, RBSR's stateful
        // per-round reply cannot absorb a self-reply.
        .allowed_destination(zenoh::sample::Locality::Remote)
        .consolidation(zenoh::query::ConsolidationMode::None)
        .timeout(std::time::Duration::from_secs(10))
        .await
    {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let mut frames = Vec::new();
    let mut total_bytes = 0usize;
    loop {
        tokio::select! {
            biased;
            res = receiver.recv_async() => {
                let Ok(reply) = res else { break; };
                if let Ok(sample) = reply.into_result() {
                    let frame = sample.payload().to_bytes().to_vec();
                    // Charge a fixed per-frame overhead on top of the payload so
                    // a flood of tiny/empty frames (which would grow the `Vec`
                    // without moving a payload-only byte count) also hits the cap.
                    total_bytes =
                        total_bytes.saturating_add(frame.len().saturating_add(RBSR_FRAME_OVERHEAD));
                    if total_bytes > MAX_RBSR_ROUND_BYTES {
                        // Over the per-round buffer cap → abandon this round's
                        // buffer and let the driver fall back to the paged
                        // vector path rather than hold an unbounded allocation.
                        tracing::debug!(%key, total_bytes, "rbsr round exceeded buffer cap; falling back");
                        return Vec::new();
                    }
                    frames.push(frame);
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {
                if closing.load(Ordering::SeqCst) { break; }
            }
        }
    }
    frames
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
        // ZEB-799: publisher and subscriber are the SAME session (local
        // routing), so peers are never wanted here.
        let cfg = hermetic_zenoh_config();
        let session = Arc::new(zenoh::open(cfg).await.expect("zenoh open"));

        let (pub_tx, pub_rx) = mpsc::channel::<Vec<u8>>(8);
        let (sub_tx, mut sub_rx) = mpsc::channel::<Vec<u8>>(8);
        let (qreq_tx, qreq_rx) =
            mpsc::channel::<crate::community_channel_log_engine::BackfillQueryRequest>(2);

        let read_for_query = Arc::new(
            |_since: Option<crate::owner_state_types::Hlc>,
             _limit: usize,
             _watermark: Option<Vec<u8>>| {
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
            None,
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

    /// ZEB-418 P3a Task 3: after a backfill query's reply stream
    /// closes naturally, the qr-driver sends exactly one
    /// `BackfillPageReport` on the request's `outcome_tx` (when
    /// `Some`): `replies` = raw packets drained off the Zenoh reply
    /// stream, `limit` = the effective clamped limit the GET selector
    /// was built with (here: `limit == 0` in the request → per-engine
    /// `backfill_default_limit`, which we set to a distinctive 7).
    ///
    /// Same single-session in-memory Zenoh shape as the round-trip
    /// test above: the adapter's own queryable answers the adapter's
    /// own GET via Zenoh local routing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn channel_log_qr_driver_reports_page_completion_on_stream_close() {
        use crate::community_channel_log_engine::{BackfillPageReport, BackfillQueryRequest};

        // ZEB-799: the adapter's own queryable answers the adapter's own GET,
        // so this session has nothing to discover.
        let cfg = hermetic_zenoh_config();
        let session = Arc::new(zenoh::open(cfg).await.expect("zenoh open"));

        // pub side unused; keep the sender alive so the publisher
        // task doesn't exit early.
        let (_pub_tx, pub_rx) = mpsc::channel::<Vec<u8>>(8);
        let (sub_tx, mut sub_rx) = mpsc::channel::<Vec<u8>>(8);
        let (qreq_tx, qreq_rx) = mpsc::channel::<BackfillQueryRequest>(2);

        // The queryable answers every backfill query with three
        // packets.
        let read_for_query = Arc::new(
            |_since: Option<crate::owner_state_types::Hlc>,
             _limit: usize,
             _watermark: Option<Vec<u8>>| {
                Box::pin(async move { vec![vec![0xA1_u8], vec![0xA2], vec![0xA3]] })
                    as std::pin::Pin<Box<dyn std::future::Future<Output = Vec<Vec<u8>>> + Send>>
            },
        );

        let closing = Arc::new(AtomicBool::new(false));
        let emit_progress: Arc<dyn Fn(u32, Option<u32>) + Send + Sync + 'static> =
            Arc::new(|_, _| {});
        const TEST_DEFAULT_LIMIT: usize = 7;
        let _adapter = spawn_channel_log_zenoh_adapter(
            Arc::clone(&session),
            "eeff".repeat(8),
            "1122".repeat(8),
            pub_rx,
            sub_tx,
            qreq_rx,
            read_for_query,
            emit_progress,
            16,
            TEST_DEFAULT_LIMIT,
            Arc::clone(&closing),
            None,
        );

        // The queryable declaration is async; a GET that fires before
        // it lands gets an immediately-closed reply stream (a clean
        // zero-reply drain — which itself exercises the report path).
        // Retry until a report shows all three packets, mirroring the
        // warmup loop in the round-trip test above.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let report: BackfillPageReport = loop {
            let (tx, rx) = tokio::sync::oneshot::channel::<BackfillPageReport>();
            qreq_tx
                .send(BackfillQueryRequest {
                    since: None,
                    limit: 0,
                    outcome_tx: Some(tx),
                    watermark_sealed: None,
                })
                .await
                .expect("qreq send");
            let report = tokio::time::timeout(std::time::Duration::from_secs(2), rx)
                .await
                .expect("driver never reported within 2s")
                .expect("driver dropped outcome_tx without sending a report");
            assert_eq!(
                report.limit, TEST_DEFAULT_LIMIT,
                "limit==0 request must report the clamped per-engine default"
            );
            if report.replies == 3 {
                break report;
            }
            assert_eq!(
                report.replies, 0,
                "partial drain unexpected with a 3-packet queryable"
            );
            assert!(
                std::time::Instant::now() < deadline,
                "queryable never came online within 5s"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };
        assert_eq!(report.replies, 3);

        // The drained packets were also forwarded down the normal
        // subscriber path (reply plumbing unchanged by the report).
        for _ in 0..3 {
            let pkt = tokio::time::timeout(std::time::Duration::from_secs(2), sub_rx.recv())
                .await
                .expect("sub recv timeout")
                .expect("sub_rx open");
            assert!(matches!(pkt.as_slice(), [0xA1] | [0xA2] | [0xA3]));
        }

        closing.store(true, Ordering::SeqCst);
        drop(qreq_tx);
    }

    /// ZEB-812 shared harness: a qr-driver adapter whose queryable answers
    /// every backfill GET with `replies` distinct one-byte packets, a
    /// deliberately SMALL subscriber channel (`sub_bound`), and a progress
    /// collector with interval 1 — so each reply pulled off the zenoh
    /// stream is externally observable regardless of what the consumer
    /// does. Warms the queryable with one actively-consumed request (the
    /// declaration race from the sibling test) before returning.
    #[allow(clippy::type_complexity)]
    async fn qr_stalled_consumer_harness(
        channel_hex: &str,
        replies: usize,
        sub_bound: usize,
    ) -> (
        tokio::task::JoinHandle<()>,
        mpsc::Sender<crate::community_channel_log_engine::BackfillQueryRequest>,
        mpsc::Receiver<Vec<u8>>,
        mpsc::Sender<Vec<u8>>,
        mpsc::Sender<Vec<u8>>,
        Arc<std::sync::Mutex<Vec<(u32, Option<u32>)>>>,
        Arc<AtomicBool>,
        Arc<zenoh::Session>,
    ) {
        use crate::community_channel_log_engine::BackfillQueryRequest;

        let cfg = hermetic_zenoh_config();
        let session = Arc::new(zenoh::open(cfg).await.expect("zenoh open"));

        let (pub_tx, pub_rx) = mpsc::channel::<Vec<u8>>(8);
        let (sub_tx, mut sub_rx) = mpsc::channel::<Vec<u8>>(sub_bound);
        let (qreq_tx, qreq_rx) = mpsc::channel::<BackfillQueryRequest>(2);

        let n = replies;
        let read_for_query = Arc::new(
            move |_since: Option<crate::owner_state_types::Hlc>,
                  _limit: usize,
                  _watermark: Option<Vec<u8>>| {
                Box::pin(async move { (0..n).map(|i| vec![i as u8]).collect::<Vec<_>>() })
                    as std::pin::Pin<Box<dyn std::future::Future<Output = Vec<Vec<u8>>> + Send>>
            },
        );

        let ticks: Arc<std::sync::Mutex<Vec<(u32, Option<u32>)>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let ticks_emit = Arc::clone(&ticks);
        let emit_progress: Arc<dyn Fn(u32, Option<u32>) + Send + Sync + 'static> =
            Arc::new(move |fetched, total| {
                ticks_emit.lock().unwrap().push((fetched, total));
            });

        let closing = Arc::new(AtomicBool::new(false));
        let adapter = spawn_channel_log_zenoh_adapter(
            Arc::clone(&session),
            "eeff".repeat(8),
            channel_hex.repeat(8),
            pub_rx,
            sub_tx.clone(),
            qreq_rx,
            read_for_query,
            emit_progress,
            1, // a progress tick per pulled reply — the ZEB-812 observable
            64,
            Arc::clone(&closing),
            None,
        );
        // The pub side is unused but must stay alive for the adapter's
        // lifetime — it rides back to the caller in the return tuple.

        // Warm up: fire actively-consumed requests until one drains all
        // `replies` packets (queryable declaration is async; early GETs see
        // a closed stream). Consuming concurrently keeps the small channel
        // from filling during warmup.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            let (tx, rx) = tokio::sync::oneshot::channel();
            qreq_tx
                .send(BackfillQueryRequest {
                    since: None,
                    limit: replies,
                    outcome_tx: Some(tx),
                    watermark_sealed: None,
                })
                .await
                .expect("qreq send (warmup)");
            let mut rx = rx;
            let mut got = 0usize;
            // PR #559 review (Qodo): the deadline must bound THIS await too,
            // not just the between-attempts check — otherwise an adapter
            // that produces neither report nor packets wedges the test
            // instead of failing it within the stated budget.
            let report = loop {
                tokio::select! {
                    r = &mut rx => break r.ok(),
                    p = sub_rx.recv() => {
                        p.expect("sub_rx open during warmup");
                        got += 1;
                    }
                    _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                        panic!(
                            "warmup wedged: no page report or packet before the 15s deadline"
                        );
                    }
                }
            };
            while sub_rx.try_recv().is_ok() {
                got += 1;
            }
            if report.map(|r| r.replies) == Some(replies) {
                assert_eq!(got, replies, "warmup consumed exactly one page");
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "queryable never came online within 15s"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        ticks.lock().unwrap().clear();

        (
            adapter, qreq_tx, sub_rx, sub_tx, pub_tx, ticks, closing, session,
        )
    }

    /// ZEB-812 invariant: draining zenoh's reply stream must NEVER block on
    /// engine (subscriber-channel) backpressure. A stalled consumer with a
    /// full subscriber channel must not stop replies being pulled off the
    /// zenoh stream — that block is exactly what backed up into zenoh's net
    /// thread (`flume wait_send<Reply>`) and wedged the whole session in
    /// ZEB-803. The mid-drain progress ticks are the direct observable:
    /// with interval 1, the highest `total=None` tick IS the number of
    /// replies pulled, no matter what the consumer does. Second half pins
    /// spill-is-not-drop: once the consumer resumes, every reply arrives,
    /// in order, and the page report fires.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn zeb812_qr_driver_pulls_full_reply_stream_despite_stalled_consumer() {
        use crate::community_channel_log_engine::BackfillQueryRequest;

        const REPLIES: usize = 32;
        const SUB_BOUND: usize = 4; // production is 64; what matters is FULL

        let (_adapter, qreq_tx, mut sub_rx, _sub_tx, _pub_tx, ticks, closing, _session) =
            qr_stalled_consumer_harness("3344", REPLIES, SUB_BOUND).await;

        // Fire a request and do NOT consume sub_rx: the stalled-consumer
        // wedge. The zenoh stream must still be fully pulled.
        let (tx, rx) = tokio::sync::oneshot::channel();
        qreq_tx
            .send(BackfillQueryRequest {
                since: None,
                limit: REPLIES,
                outcome_tx: Some(tx),
                watermark_sealed: None,
            })
            .await
            .expect("qreq send");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let pulled = ticks
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, total)| total.is_none())
                .map(|(n, _)| *n)
                .max()
                .unwrap_or(0);
            if pulled >= REPLIES as u32 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "ZEB-812: drain stalled at {pulled}/{REPLIES} replies pulled — the \
                 reply-drain loop is blocking on subscriber-channel backpressure \
                 instead of draining zenoh's reply stream"
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }

        // Spill is not drop: resume consuming; every packet arrives in order.
        let mut got: Vec<u8> = Vec::with_capacity(REPLIES);
        while got.len() < REPLIES {
            let pkt = tokio::time::timeout(std::time::Duration::from_secs(10), sub_rx.recv())
                .await
                .expect("resumed consumer timed out waiting for spilled replies")
                .expect("sub_rx open");
            assert_eq!(pkt.len(), 1);
            got.push(pkt[0]);
        }
        let expect: Vec<u8> = (0..REPLIES as u8).collect();
        assert_eq!(
            got, expect,
            "spilled replies must arrive complete and in order"
        );

        // And the page report fires once the flush lands.
        let report = tokio::time::timeout(std::time::Duration::from_secs(10), rx)
            .await
            .expect("page report timed out")
            .expect("driver dropped outcome_tx");
        assert_eq!(report.replies, REPLIES);

        closing.store(true, Ordering::SeqCst);
        drop(qreq_tx);
    }

    /// ZEB-812 companion invariant: a stalled consumer must not make
    /// `stop()` latency unbounded. The drain loop's 500ms closing-poll arm
    /// only runs between select iterations — an await parked inside the
    /// reply arm's body starves it, so pre-fix the qr task (and therefore
    /// the adapter JoinHandle, which joins it) never exits. Post-fix both
    /// the drain phase and the spill-flush phase keep the closing poll
    /// live, so the adapter must come down within a few seconds.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn zeb812_qr_driver_shutdown_unblocks_despite_stalled_consumer() {
        use crate::community_channel_log_engine::BackfillQueryRequest;

        const REPLIES: usize = 32;
        const SUB_BOUND: usize = 4;

        let (adapter, qreq_tx, _sub_rx, _sub_tx, _pub_tx, ticks, closing, _session) =
            qr_stalled_consumer_harness("5566", REPLIES, SUB_BOUND).await;

        let (tx, _rx) = tokio::sync::oneshot::channel();
        qreq_tx
            .send(BackfillQueryRequest {
                since: None,
                limit: REPLIES,
                outcome_tx: Some(tx),
                watermark_sealed: None,
            })
            .await
            .expect("qreq send");

        // Wait until the drain has demonstrably started (≥1 reply pulled)
        // so closing flips mid-wedge, not before the GET.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let pulled = ticks
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, total)| total.is_none())
                .map(|(n, _)| *n)
                .max()
                .unwrap_or(0);
            if pulled >= 1 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "drain never started within 10s"
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }

        closing.store(true, Ordering::SeqCst);
        // Exit must come from the closing flag, not from a closed request
        // channel — keep the sender alive across the join.
        let join = tokio::time::timeout(std::time::Duration::from_secs(5), adapter).await;
        assert!(
            join.is_ok(),
            "ZEB-812: adapter did not shut down within 5s of the closing flag — \
             the reply-drain (or spill-flush) is parked on subscriber-channel \
             backpressure with the closing poll starved"
        );
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

    #[test]
    fn parse_rbsr_key_round_trips() {
        let cid = "aa".repeat(16);
        let ch = "bb".repeat(16);
        for round in [0u32, 5, crate::channel_rbsr::MAX_RBSR_ROUNDS] {
            let key = format_rbsr_key(&cid, &ch, round);
            assert_eq!(
                parse_rbsr_key(&key),
                Some(round),
                "round {round} must round-trip through format/parse"
            );
        }
    }

    #[test]
    fn parse_rbsr_key_rejects_wrong_family_and_malformed() {
        let cid = "aa".repeat(16);
        let ch = "bb".repeat(16);
        // A `since/**` key is a different family — must not parse as RBSR.
        let since_key = format!("harmony/channels/{cid}/{ch}/since/0/100");
        assert_eq!(parse_rbsr_key(&since_key), None, "since key is not rbsr");
        // Non-numeric round.
        let bad_round = format!("harmony/channels/{cid}/{ch}/rbsr/notanumber");
        assert_eq!(
            parse_rbsr_key(&bad_round),
            None,
            "non-numeric round rejected"
        );
        // Too few segments.
        assert_eq!(
            parse_rbsr_key("harmony/channels/aa/bb"),
            None,
            "short key rejected"
        );
        // Trailing junk past the round must be rejected (strict, exactly 6).
        let trailing = format!("harmony/channels/{cid}/{ch}/rbsr/0/extra");
        assert_eq!(parse_rbsr_key(&trailing), None, "trailing segment rejected");
    }

    #[tokio::test]
    async fn drive_rbsr_rounds_round0_no_reply_falls_back_to_vector() {
        // An old peer with no rbsr/** queryable draws zero replies on round 0.
        let (mode, _transferred) = drive_rbsr_rounds(
            || async { b"init".to_vec() },
            |_round, _sealed| async { Vec::<Vec<u8>>::new() },
            |_frames| async {
                RbsrStep::Continue {
                    ingested: 0,
                    next: b"unused".to_vec(),
                }
            },
        )
        .await;
        assert_eq!(mode, crate::channel_backfill::ReconcileMode::VectorFallback);
    }

    #[tokio::test]
    async fn drive_rbsr_rounds_converges_and_threads_sealed_requests() {
        use std::cell::{Cell, RefCell};
        let seen: RefCell<Vec<(u32, Vec<u8>)>> = RefCell::new(Vec::new());
        let ingest_calls = Cell::new(0u32);
        let (mode, transferred) = drive_rbsr_rounds(
            || async { b"init".to_vec() },
            |round, sealed| {
                seen.borrow_mut().push((round, sealed));
                async { vec![vec![0xAAu8]] }
            },
            |_frames| {
                let n = ingest_calls.get();
                ingest_calls.set(n + 1);
                async move {
                    if n == 0 {
                        RbsrStep::Continue {
                            ingested: 2,
                            next: b"round1".to_vec(),
                        }
                    } else {
                        RbsrStep::Converged { ingested: 3 }
                    }
                }
            },
        )
        .await;
        assert_eq!(mode, crate::channel_backfill::ReconcileMode::Done);
        assert_eq!(
            transferred, 5,
            "transferred accumulates each step's ingested count (2 + 3)",
        );
        assert_eq!(
            seen.into_inner(),
            vec![(0u32, b"init".to_vec()), (1u32, b"round1".to_vec())],
            "each round's sealed request is the prior round's `Continue` payload",
        );
    }

    #[tokio::test]
    async fn drive_rbsr_rounds_hits_cap_returns_full_reconcile() {
        use std::cell::Cell;
        let gets = Cell::new(0u32);
        let (mode, _transferred) = drive_rbsr_rounds(
            || async { b"init".to_vec() },
            |_round, _sealed| {
                gets.set(gets.get() + 1);
                async { vec![vec![0xAAu8]] }
            },
            |_frames| async {
                RbsrStep::Continue {
                    ingested: 0,
                    next: b"more".to_vec(),
                }
            },
        )
        .await;
        assert_eq!(mode, crate::channel_backfill::ReconcileMode::FullReconcile);
        assert_eq!(
            gets.get(),
            crate::channel_rbsr::MAX_RBSR_ROUNDS,
            "issues exactly the round cap's worth of GETs before falling back",
        );
    }

    #[tokio::test]
    async fn drive_rbsr_rounds_ingest_failure_falls_back_to_vector() {
        let (mode, _transferred) = drive_rbsr_rounds(
            || async { b"init".to_vec() },
            |_round, _sealed| async { vec![vec![0xAAu8]] },
            |_frames| async { RbsrStep::Failed },
        )
        .await;
        assert_eq!(mode, crate::channel_backfill::ReconcileMode::VectorFallback);
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

/// ZEB-373: build a started zenoh Runtime + Session from `config`, returning the
/// Runtime handle (for dynamic `connect_peer` dialing) alongside the Session.
/// Replaces `zenoh::open(config)` — the `internal` feature exposes
/// `RuntimeBuilder` + `session::init`, which `zenoh::open` uses under the hood.
///
/// Order MUST mirror `zenoh::open` (`Session::new`, zenoh-1.9.0
/// api/session.rs:1431): `build()` → `session::init` (register the session face)
/// → `runtime.start()` (bind listeners + dial the static `connect/endpoints`
/// seed). Starting before init would bind/dial before the face exists, opening a
/// window where a declaration/sample on a freshly-formed transport lands before
/// `new_primitives` registers us — i.e. it would NOT be parity with ZEB-368.
///
/// `pub` so the ZEB-373 acceptance integration test
/// (`tests/zeb_373_dynamic_dial_integration.rs`) can build a real Runtime through
/// the same path production uses; integration tests compile against the public API.
pub async fn open_session_with_runtime(
    config: zenoh::Config,
) -> zenoh::Result<(zenoh::internal::runtime::Runtime, zenoh::Session)> {
    // ZEB-695: install the durable panic-capture hook before any zenoh net
    // thread exists. The hook is process-global, so once installed here it also
    // catches panics from sessions opened via other funnels in the same process.
    // Idempotent + debug/CI-gated, so this is cheap and a no-op in release.
    crate::panic_capture::install_once();
    let mut runtime = zenoh::internal::runtime::RuntimeBuilder::new(config)
        .build()
        .await?;
    // Register the session face BEFORE starting (binding listeners + dialing).
    let session = zenoh::session::init(runtime.clone().into()).await?;
    runtime.start().await?;
    Ok((runtime, session))
}

/// ZEB-799: zenoh config for a test that opens a session it does not intend to
/// share with anyone.
///
/// `zenoh::Config::default()` leaves multicast scouting and gossip ENABLED, so
/// such a session peers with every other zenoh node reachable on the host —
/// including a developer's standing `harmony-app serve` nodes. `session.close()`
/// then has to tear those peer links down and hits its internal timeout, so the
/// test fails **deterministically on a workstation and never in CI**, where the
/// runner has no neighbours. Measured: `Config::default()` FAIL 3/3 at ~10.55s
/// vs hermetic PASS at 0.124s.
///
/// Use this ONLY for single-session tests. Several multi-session integration
/// tests deliberately rely on loopback scouting for their two sessions to
/// discover each other (see `community_presence_two_engine_integration.rs`) —
/// applying this there would break them. Naming the constraint here because the
/// obvious sweep (`s/Config::default()/hermetic/`) is wrong.
#[cfg(test)]
pub(crate) fn hermetic_zenoh_config() -> zenoh::Config {
    let mut cfg = zenoh::Config::default();
    cfg.insert_json5("scouting/multicast/enabled", "false")
        .expect("disable multicast scouting");
    cfg.insert_json5("scouting/gossip/enabled", "false")
        .expect("disable gossip scouting");
    cfg
}

/// ZEB-912: session mode for the zenoh runtime, from `HARMONY_ZENOH_MODE`.
/// Default (unset/empty) = "peer", today's production mode. "router" opts a
/// node into zenoh's router routing hat — the only hat with linkstate
/// multi-hop data routing in zenoh 1.9.0 (`routing.peer.mode` is a deprecated
/// no-op; see docs/research/2026-08-12-zeb912-r3-zenoh-multihop-spike.md).
pub(crate) fn zenoh_session_mode() -> &'static str {
    let raw = std::env::var("HARMONY_ZENOH_MODE").ok();
    parse_zenoh_mode(raw.as_deref())
}

/// Pure core of [`zenoh_session_mode`]. Any value other than exactly "router"
/// (trimmed) falls back to "peer" — misconfiguration must fail toward current
/// behavior, not toward a novel topology. Unrecognized non-empty values warn.
pub(crate) fn parse_zenoh_mode(raw: Option<&str>) -> &'static str {
    match raw.map(str::trim) {
        Some("router") => "router",
        Some("") | None => "peer",
        Some(other) => {
            tracing::warn!(
                value = %other,
                "HARMONY_ZENOH_MODE: unrecognized value; using \"peer\""
            );
            "peer"
        }
    }
}

#[cfg(test)]
mod zeb912_mode_knob_tests {
    use super::parse_zenoh_mode;

    /// ZEB-912: misconfiguration must fail toward today's behavior ("peer"),
    /// never toward a novel topology. Only the exact (trimmed) "router" opts in.
    #[test]
    fn parse_zenoh_mode_defaults_and_opt_in() {
        assert_eq!(parse_zenoh_mode(None), "peer");
        assert_eq!(parse_zenoh_mode(Some("")), "peer");
        assert_eq!(parse_zenoh_mode(Some("   ")), "peer");
        assert_eq!(parse_zenoh_mode(Some("router")), "router");
        assert_eq!(parse_zenoh_mode(Some(" router ")), "router");
        assert_eq!(parse_zenoh_mode(Some("Router")), "peer");
        assert_eq!(parse_zenoh_mode(Some("linkstate")), "peer");
        assert_eq!(parse_zenoh_mode(Some("peer")), "peer");
    }

    /// ZEB-912: pin that the `mode` and `timestamping/enabled` key paths remain
    /// valid in the zenoh version we build against (zeb616 pattern — a schema
    /// rename in a zenoh upgrade must fail here, not at node boot).
    #[test]
    fn mode_and_timestamping_keys_are_valid() {
        let mut config = zenoh::Config::default();
        config
            .insert_json5("mode", "\"router\"")
            .expect("mode must be a valid zenoh config key");
        config
            .insert_json5("timestamping/enabled", "false")
            .expect("timestamping/enabled must be a valid zenoh config key");
    }
}

#[cfg(test)]
mod zeb616_lease_config_tests {
    /// ZEB-616 Component C: pin that the keepalive/lease config key paths
    /// remain valid in the zenoh version we build against. `insert_json5`
    /// returns `Err` for an unknown key path, so this fails loudly if a zenoh
    /// upgrade renames the schema (which would otherwise break node boot at
    /// `zenoh::open`).
    #[test]
    fn lease_and_keepalive_keys_are_valid() {
        let mut config = zenoh::Config::default();
        config
            .insert_json5("transport/link/tx/lease", "4000")
            .expect("transport/link/tx/lease must be a valid zenoh config key");
        config
            .insert_json5("transport/link/tx/keep_alive", "4")
            .expect("transport/link/tx/keep_alive must be a valid zenoh config key");
    }
}

#[cfg(test)]
mod zeb620_event_listener_pin_tests {
    /// ZEB-620 schema pin: the reconnect supervisor consumes zenoh's unstable
    /// transport/link event-listener surface (enabled via the `unstable`
    /// feature on the `zenoh` dependency). This is a pure name-resolution check
    /// — constructing a `Session` needs a runtime, so we only force these paths
    /// to resolve and their signatures to match. The explicit fn-pointer types
    /// pin the method name, receiver, and return builder type in one shot. If a
    /// zenoh bump renames or drops the surface, this fails here, not deep in the
    /// supervisor wiring.
    #[test]
    fn zenoh_unstable_event_listener_surface_exists() {
        use zenoh::handlers::DefaultHandler;
        use zenoh::session::{
            LinkEvent, LinkEventsListenerBuilder, SessionInfo, TransportEvent,
            TransportEventsListenerBuilder,
        };

        // The builder-returning accessors the supervisor calls to subscribe.
        let _transport_listener: fn(
            &SessionInfo,
        ) -> TransportEventsListenerBuilder<'_, DefaultHandler> =
            SessionInfo::transport_events_listener;
        let _link_listener: fn(&SessionInfo) -> LinkEventsListenerBuilder<'_, DefaultHandler> =
            SessionInfo::link_events_listener;

        // The event payload types the supervisor matches on (Put/Delete).
        let _ = core::mem::size_of::<TransportEvent>();
        let _ = core::mem::size_of::<LinkEvent>();
    }
}

#[cfg(test)]
mod zeb620_boot_seed_tests {
    use super::build_connect_endpoints;
    use crate::iroh_zenoh_registration::seed_boot_peers_into_supervisor;
    use crate::owner_state_types::{Hlc, OwnerAddr};
    use crate::reachability_record::ReachabilityAnnouncePayload;
    use crate::reachability_resolver::ReachabilityResolver;
    use crate::reconnect_supervisor::{ReconnectTrigger, SupervisorHandle};

    fn payload(node_id: [u8; 32], announced_at_ms: u64) -> ReachabilityAnnouncePayload {
        ReachabilityAnnouncePayload {
            iroh_node_id: node_id,
            home_relay_url: String::new(),
            direct_addresses: vec![],
            announced_at_ms,
            identity_signature: [0; 64],
            butler_set: vec![],
            bs_at: 0,
        }
    }

    fn hlc(wall_ms: u64) -> Hlc {
        Hlc {
            wall_ms,
            logical: 0,
            device_id: String::new(),
        }
    }

    /// ZEB-620 Task 5: boot peers enter the reconnect supervisor as `NewPeer`
    /// kicks (recency-ordered, newest first) — NOT zenoh's `connect/endpoints`
    /// static seed. The connect-endpoints builder takes no resolver, so a known
    /// peer structurally cannot leak an `iroh/` locator into the zenoh config.
    #[test]
    fn boot_seeds_kick_supervisor_not_config() {
        let resolver = ReachabilityResolver::new();
        let older = [0x11u8; 32];
        let newer = [0x22u8; 32];
        // Two known peers; `newer` has the fresher announced_at_ms.
        resolver.update(OwnerAddr([0xA1; 16]), payload(older, 1_000), hlc(1_000));
        resolver.update(OwnerAddr([0xA2; 16]), payload(newer, 2_000), hlc(2_000));
        let self_nid = [0xEEu8; 32]; // not among the peers

        // (1) Boot seeding kicks the supervisor `NewPeer` for each peer, ordered
        // newest-first.
        let handle = SupervisorHandle::new();
        let ordered = seed_boot_peers_into_supervisor(&resolver, &self_nid, &handle);
        assert_eq!(ordered, vec![newer, older], "seeds ordered newest-first");
        assert_eq!(
            handle.pending_trigger(older),
            Some(ReconnectTrigger::NewPeer),
            "older peer seeded as NewPeer"
        );
        assert_eq!(
            handle.pending_trigger(newer),
            Some(ReconnectTrigger::NewPeer),
            "newer peer seeded as NewPeer"
        );

        // (2) `connect/endpoints` carries only the LAN endpoint — never an iroh
        // locator — even though the resolver knows peers. The builder takes no
        // resolver, so this is structural, not incidental.
        let connect_eps =
            build_connect_endpoints(Some("tcp/192.0.2.7:7447")).expect("connect eps build");
        assert!(
            connect_eps.iter().all(|e| !e.contains("iroh/")),
            "no iroh locator in connect/endpoints: {connect_eps:?}"
        );
        assert!(
            connect_eps.iter().any(|e| e.contains("192.0.2.7")),
            "LAN connect endpoint preserved: {connect_eps:?}"
        );

        // No endpoint → no connect/endpoints entries at all.
        assert!(
            build_connect_endpoints(None)
                .expect("empty build")
                .is_empty(),
            "no endpoint yields no connect/endpoints"
        );
    }
}

#[cfg(test)]
mod dispatch_attachment_tests {
    use super::*;

    /// ZEB-669 slice 1 harness: publish through the production
    /// `dispatch_action` arm on an in-process zenoh session and return
    /// the attachment the subscriber observed. Zenoh requires the
    /// `multi_thread` tokio flavor (`current_thread` panics on
    /// `zenoh::open` — see `community_channel_log_engine.rs` fixtures).
    async fn publish_and_observe_attachment(key_expr: &str) -> Option<String> {
        // ZEB-799: single in-process session, subscriber declared on itself.
        let session = zenoh::open(hermetic_zenoh_config())
            .await
            .expect("zenoh open");
        let sub = session
            .declare_subscriber(key_expr)
            .await
            .expect("declare subscriber");
        let (zenoh_tx, _zenoh_rx) = mpsc::channel::<ZenohEvent>(8);
        let app: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink> =
            std::sync::Arc::new(crate::node_event_sink::RecordingSink::new());
        let closing = Arc::new(AtomicBool::new(false));
        dispatch_action(
            RuntimeAction::Publish {
                key_expr: key_expr.to_string(),
                // Real announce payloads are a 4-byte BE u32 size
                // (`parse_content_announcement`); mirror that shape.
                payload: 1234u32.to_be_bytes().to_vec(),
            },
            &session,
            &zenoh_tx,
            &app,
            &closing,
            "zid-under-test",
        )
        .await;
        let sample = tokio::time::timeout(Duration::from_secs(10), sub.recv_async())
            .await
            .expect("sample within 10s")
            .expect("subscriber alive");
        sample
            .attachment()
            .and_then(|a| String::from_utf8(a.to_bytes().to_vec()).ok())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn announce_publish_carries_own_zid_attachment() {
        let key = format!("{}{}", crate::ANNOUNCE_PREFIX, "aa".repeat(32));
        assert_eq!(
            publish_and_observe_attachment(&key).await.as_deref(),
            Some("zid-under-test"),
            "announce publishes must attach the local zid so \
             ObservedHolders can attribute the announcing session"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn non_announce_publish_carries_no_attachment() {
        assert_eq!(
            publish_and_observe_attachment("harmony/profile/deadbeef").await,
            None,
            "the zid attachment stays scoped to capacity + announce keys"
        );
    }

    /// Full receive-path loopback (PR #448 review, CodeRabbit): the
    /// production Subscribe arm performs the `sample.attachment()` →
    /// `source_zid` extraction that the helper-unit tests can't see.
    /// Publish arm attaches → Subscribe arm extracts → the extracted
    /// value feeds `note_announce_sample` and counts.
    #[tokio::test(flavor = "multi_thread")]
    async fn subscription_arm_extracts_source_zid_and_feeds_holders() {
        // ZEB-799: this is the test that actually failed. It asserts on the
        // Subscribe arm's zid extraction against its own session and wants no
        // peers at all — but `Config::default()` gave it every harmony node on
        // the host, and `session.close()` below then timed out waiting on them.
        let session = zenoh::open(hermetic_zenoh_config())
            .await
            .expect("zenoh open");
        let (zenoh_tx, mut zenoh_rx) = mpsc::channel::<ZenohEvent>(8);
        let app: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink> =
            std::sync::Arc::new(crate::node_event_sink::RecordingSink::new());
        let closing = Arc::new(AtomicBool::new(false));
        dispatch_action(
            RuntimeAction::Subscribe {
                key_expr: format!("{}*", crate::ANNOUNCE_PREFIX),
            },
            &session,
            &zenoh_tx,
            &app,
            &closing,
            "publisher-zid",
        )
        .await;
        let cid_hex = "ab".repeat(32);
        dispatch_action(
            RuntimeAction::Publish {
                key_expr: format!("{}{cid_hex}", crate::ANNOUNCE_PREFIX),
                payload: 1234u32.to_be_bytes().to_vec(),
            },
            &session,
            &zenoh_tx,
            &app,
            &closing,
            "publisher-zid",
        )
        .await;
        let ev = tokio::time::timeout(Duration::from_secs(10), zenoh_rx.recv())
            .await
            .expect("event within 10s")
            .expect("channel alive");
        let ZenohEvent::Subscription {
            key_expr,
            payload,
            source_zid,
        } = ev
        else {
            panic!("expected Subscription event");
        };
        assert_eq!(source_zid.as_deref(), Some("publisher-zid"));
        // A receiver node (different own zid) must count the sample.
        let holders = Arc::new(std::sync::Mutex::new(
            crate::observed_holders::ObservedHolders::new(),
        ));
        note_announce_sample(
            &holders,
            &key_expr,
            &payload,
            source_zid.as_deref(),
            "receiver-zid",
            || 10,
        );
        assert_eq!(holders.lock().unwrap().peer_count(&cid_hex), 1);
        // Tear down the detached forwarder task the Subscribe arm
        // spawned: mark closing (suppresses the session-lost emit) and
        // close the session so `recv_async` errors and the task exits —
        // otherwise the held subscriber keeps zenoh io threads alive
        // past test end (nextest LEAK).
        closing.store(true, Ordering::SeqCst);
        session.close().await.expect("session close");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    #[test]
    fn publish_attaches_zid_matches_announce_and_capacity_only() {
        assert!(publish_attaches_zid("harmony/announce/aabb"));
        assert!(publish_attaches_zid("harmony/compute/capacity/node1"));
        assert!(!publish_attaches_zid("harmony/profile/aabb"));
        assert!(!publish_attaches_zid("harmony/vines/aabb/follows"));
        // Prefix discipline: no trailing-slash bypass.
        assert!(!publish_attaches_zid("harmony/announcements/aabb"));
    }
}

#[cfg(test)]
mod note_announce_sample_tests {
    use super::*;

    fn holders() -> Arc<std::sync::Mutex<crate::observed_holders::ObservedHolders>> {
        Arc::new(std::sync::Mutex::new(
            crate::observed_holders::ObservedHolders::new(),
        ))
    }

    fn announce_key() -> (String, String) {
        let cid_hex = "ab".repeat(32);
        (format!("{}{cid_hex}", crate::ANNOUNCE_PREFIX), cid_hex)
    }

    /// Real announce payloads are a 4-byte BE u32 size.
    const PAYLOAD: [u8; 4] = 1234u32.to_be_bytes();

    #[test]
    fn foreign_zid_announce_feeds_the_holder_map() {
        let h = holders();
        let (key, cid_hex) = announce_key();
        note_announce_sample(&h, &key, &PAYLOAD, Some("peer-zid"), "own-zid", || 10);
        assert_eq!(h.lock().unwrap().peer_count(&cid_hex), 1);
    }

    #[test]
    fn own_zid_announce_does_not_self_count() {
        let h = holders();
        let (key, cid_hex) = announce_key();
        note_announce_sample(&h, &key, &PAYLOAD, Some("own-zid"), "own-zid", || 10);
        assert_eq!(h.lock().unwrap().peer_count(&cid_hex), 0);
    }

    #[test]
    fn missing_source_zid_is_skipped() {
        let h = holders();
        let (key, cid_hex) = announce_key();
        note_announce_sample(&h, &key, &PAYLOAD, None, "own-zid", || 10);
        assert_eq!(h.lock().unwrap().peer_count(&cid_hex), 0);
    }

    #[test]
    fn non_announce_key_is_ignored() {
        let h = holders();
        let (_, cid_hex) = announce_key();
        note_announce_sample(
            &h,
            &format!("harmony/profile/{cid_hex}"),
            &PAYLOAD,
            Some("peer-zid"),
            "own-zid",
            || 10,
        );
        assert_eq!(h.lock().unwrap().peer_count(&cid_hex), 0);
    }
}

#[cfg(test)]
mod note_storage_record_sample_tests {
    use super::*;
    use crate::storage_records::StorageRecordStore;
    use crate::storage_signing::{self, HostingReportEntry, PledgeEntry, PledgeListPayload};

    fn store() -> Arc<std::sync::Mutex<StorageRecordStore>> {
        Arc::new(std::sync::Mutex::new(StorageRecordStore::new(None)))
    }

    fn signer() -> harmony_identity::PrivateIdentity {
        harmony_identity::PrivateIdentity::generate(&mut rand::rngs::OsRng)
    }

    fn signed_pledges(
        id: &harmony_identity::PrivateIdentity,
        updated_at: u64,
    ) -> (String, Vec<u8>) {
        let owner = hex::encode(id.public_identity().address_hash);
        let mut p = PledgeListPayload {
            owner_address: owner.clone(),
            pledges: vec![PledgeEntry {
                to: "someone".into(),
                bytes: 9,
            }],
            updated_at,
            identity_pub: None,
            sig: None,
            v2: Default::default(),
        };
        storage_signing::sign_pledge_list(id, &mut p);
        (
            format!("{}{owner}/pledges", crate::STORAGE_RECORD_PREFIX),
            serde_json::to_vec(&p).unwrap(),
        )
    }

    /// Empty revocation projection (ZEB-679) — most routing tests don't
    /// exercise the revocation path.
    fn rvk() -> crate::revoked_device_projection::RevokedDeviceProjection {
        crate::revoked_device_projection::RevokedDeviceProjection::new()
    }

    /// ZEB-679 end-to-end: the projection handle threaded through the
    /// router actually gates a revoked signer's dual-signed record.
    #[test]
    fn revoked_signer_sample_dropped_through_router_zeb679() {
        use crate::enrollment_verify::quorum_fixtures::{mint_quorum_world, WORLD_NOW};
        let world = mint_quorum_world(0x30);
        let s = store();
        let id = signer();
        let owner = hex::encode(id.public_identity().address_hash);
        let mut p = crate::storage_signing::PledgeListPayload {
            owner_address: owner.clone(),
            pledges: vec![],
            updated_at: 5,
            identity_pub: None,
            sig: None,
            v2: Default::default(),
        };
        let material = crate::storage_signing::StorageSignerMaterial {
            sk: std::sync::Arc::new(world.a_sk.clone()),
            cert: world.a_cert.clone(),
            signer_certs: Vec::new(),
        };
        crate::storage_signing::sign_pledge_list_v2(&id, &material, &mut p).expect("dual-sign");
        let key = format!("{}{owner}/pledges", crate::STORAGE_RECORD_PREFIX);
        let bytes = serde_json::to_vec(&p).unwrap();

        let revoked = rvk();
        let dead_key = world.a_cert.device_pubkeys.classical.ed25519_verify;
        let keys: std::collections::BTreeSet<[u8; 32]> = std::iter::once(dead_key).collect();
        revoked.union_from_members(std::iter::once((
            crate::owner_state_types::OwnerAddr(world.owner_id),
            &keys,
        )));
        assert!(
            !note_storage_record_sample(&s, &key, &bytes, &revoked, || WORLD_NOW * 1000),
            "revoked signer's record must not land"
        );
        assert!(s.lock().unwrap().pledge_list(&owner).is_none());
        // Same record through an EMPTY projection lands (control).
        assert!(note_storage_record_sample(&s, &key, &bytes, &rvk(), || {
            WORLD_NOW * 1000
        }));
    }

    #[test]
    fn signed_pledge_sample_routes_and_reports_change() {
        let s = store();
        let id = signer();
        let (key, payload) = signed_pledges(&id, 5);
        assert!(note_storage_record_sample(&s, &key, &payload, &rvk(), || 1));
        let owner = hex::encode(id.public_identity().address_hash);
        assert!(s.lock().unwrap().pledge_list(&owner).is_some());
    }

    #[test]
    fn replayed_and_rejected_samples_report_no_change() {
        let s = store();
        let id = signer();
        let (key, payload) = signed_pledges(&id, 5);
        assert!(note_storage_record_sample(&s, &key, &payload, &rvk(), || 1));
        // LWW replay: same updated_at ⇒ IgnoredOlder ⇒ no change.
        assert!(!note_storage_record_sample(
            &s,
            &key,
            &payload,
            &rvk(),
            || 1
        ));
        // Garbage payload ⇒ Rejected ⇒ no change.
        assert!(!note_storage_record_sample(
            &s,
            &key,
            b"not json",
            &rvk(),
            || 1
        ));
    }

    #[test]
    fn hosting_sample_stamps_the_lazy_receipt_clock() {
        let s = store();
        let id = signer();
        let owner = hex::encode(id.public_identity().address_hash);
        let mut h = crate::storage_signing::HostingReportPayload {
            owner_address: owner.clone(),
            reports: vec![HostingReportEntry {
                beneficiary: "b".into(),
                bytes: 1,
                cids: 1,
            }],
            updated_at: 5,
            identity_pub: None,
            sig: None,
            v2: Default::default(),
        };
        storage_signing::sign_hosting_report(&id, &mut h);
        let key = format!("{}{owner}/hosting", crate::STORAGE_RECORD_PREFIX);
        assert!(note_storage_record_sample(
            &s,
            &key,
            &serde_json::to_vec(&h).unwrap(),
            &rvk(),
            || 777,
        ));
        assert_eq!(
            s.lock()
                .unwrap()
                .hosting_report(&owner)
                .unwrap()
                .received_at_ms,
            777
        );
    }

    #[test]
    fn non_storage_and_unknown_kind_keys_are_ignored() {
        let s = store();
        assert!(!note_storage_record_sample(
            &s,
            "harmony/announce/aabb",
            b"",
            &rvk(),
            || 1
        ));
        assert!(!note_storage_record_sample(
            &s,
            "harmony/storage/owner/unknown-kind",
            b"",
            &rvk(),
            || 1
        ));
        assert!(!note_storage_record_sample(
            &s,
            "harmony/storage/owner",
            b"",
            &rvk(),
            || 1
        ));
    }
}

#[cfg(test)]
mod zeb932_voting_rbsr_cadence_tests {
    use super::{need_full_dump, VotingReconcileOutcome};

    #[test]
    fn converged_skips_full_dump_until_backstop_is_due() {
        let every = 12u32;
        // Converged and backstop not yet due → no full dump.
        assert!(!need_full_dump(VotingReconcileOutcome::Converged, 0, every));
        assert!(!need_full_dump(
            VotingReconcileOutcome::Converged,
            10,
            every
        ));
        // Converged but the periodic floor is due (since_full + 1 >= every).
        assert!(need_full_dump(VotingReconcileOutcome::Converged, 11, every));
        assert!(need_full_dump(VotingReconcileOutcome::Converged, 50, every));
    }

    #[test]
    fn non_convergence_always_forces_a_full_dump() {
        let every = 12u32;
        for since in [0u32, 1, 5, 11] {
            assert!(
                need_full_dump(VotingReconcileOutcome::NoResponder, since, every),
                "no responder must fall back to the full dump"
            );
            assert!(
                need_full_dump(VotingReconcileOutcome::Failed, since, every),
                "a failed reconcile must fall back to the full dump"
            );
        }
    }
}
