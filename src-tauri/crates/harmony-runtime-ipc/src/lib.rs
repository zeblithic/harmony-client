//! harmony-runtime-ipc — ZEB-548 Stage 1 (PR #6).
//!
//! The command-thread → event-loop request contracts, lifted out of
//! `harmony-app`'s `event_loop` module. A Tauri command (or a feature crate
//! like `harmony-mail`) constructs one of these and sends it over an
//! `mpsc`/`oneshot` channel into the single-threaded event loop, which owns the
//! `!Send` runtime; the `reply` sender carries the result back.
//!
//! These are pure message types — only `tokio::sync` channel handles, no
//! `harmony-*` deps, no Tauri. `harmony-app` re-exports them from `event_loop`
//! so its `crate::event_loop::{PublishRequest, FetchRequest, IngestRequest,
//! ContentVerbRequest}` call sites resolve unchanged.

use tokio::sync::oneshot;

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
    /// ZEB-1012 / ZEB-157: best-effort eviction of the CIDs a FAILED ingest
    /// had already admitted — a partial leaf list (mid-stream failure) or a
    /// fully-built root (post-ingest failure arm), duplicates tolerated.
    ///
    /// Evict-unclaimed semantics, deliberately NOT `Burn`: nothing reachable
    /// from a pinned or buddy-held root is touched, and no pin-intent /
    /// buddy-ledger bookkeeping is mutated — on a content-dedup collision
    /// with an already-pinned identical file, `Burn`'s intent removal would
    /// un-pin the user's good copy. Replies with the number of CIDs actually
    /// evicted (observability + tests); callers treat any error or a dropped
    /// reply as best-effort-failed and move on (the cache reclaims orphans
    /// under W-TinyLFU pressure and at restart regardless).
    RollbackIngest {
        cids: Vec<[u8; 32]>,
        reply: oneshot::Sender<Result<usize, String>>,
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
