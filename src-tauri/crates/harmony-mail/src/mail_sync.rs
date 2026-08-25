//! Mailbox sync: walks the gateway-published Merkle tree and registers
//! header-only entries with MailManager. Lazy body fetch on demand.
//!
//! See docs/specs/2026-04-14-client-mail-receive-design.md.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::Serialize;
use tokio::sync::{mpsc, oneshot};

use crate::mail::MailManager;

pub const CID_LEN: usize = 32;

/// Status payload emitted on the `mail-sync-status` Tauri event.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncStatusEvent {
    pub state: &'static str, // "idle" | "syncing" | "error"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Internal walker state.
#[derive(Debug)]
#[allow(dead_code)] // pre-existing; tracked for cleanup
enum SyncState {
    Idle {
        last_walked_root: Option<[u8; CID_LEN]>,
    },
    Walking {
        root: [u8; CID_LEN],
        started_at: Instant,
        pending_root: Option<[u8; CID_LEN]>,
        /// The `last_walked_root` value at the moment the Idle/Error → Walking
        /// transition happened. Carried so a strict-failure (`finish_walk_error`)
        /// transitioning back out of Walking can preserve the previous
        /// successful root rather than dropping it on the floor.
        prev_last_walked_root: Option<[u8; CID_LEN]>,
    },
    Error {
        last_error: String,
        last_walked_root: Option<[u8; CID_LEN]>,
    },
}

/// Request to fetch a CAS blob from the gateway. Re-exported from
/// `event_loop` so lib.rs can clone the same `fetch_tx` Sender into
/// both the event loop (which owns the receiver) and MailSync (which
/// produces requests during walker traversal and lazy body fetch).
pub use crate::event_loop::FetchRequest;

/// Request to query the gateway for the current root CID. Reply carries
/// the raw payload (Some) or None if no mail exists for this address.
pub type RefreshRequest = oneshot::Sender<Result<Option<Vec<u8>>, String>>;

/// In-flight body-fetch deduplication: multiple concurrent callers
/// asking for the same CID share one outgoing fetch via `watch`.
type InFlightMap = Arc<
    Mutex<HashMap<[u8; CID_LEN], tokio::sync::watch::Receiver<Option<Result<Vec<u8>, String>>>>>,
>;

pub struct MailSync {
    state: Arc<Mutex<SyncState>>,
    fetch_tx: mpsc::Sender<FetchRequest>,
    refresh_tx: mpsc::Sender<RefreshRequest>,
    mail_mgr: Arc<Mutex<MailManager>>,
    sink: Arc<dyn crate::node_event_sink::NodeEventSink>,
    in_flight_bodies: InFlightMap,
}

impl MailSync {
    pub fn new(
        fetch_tx: mpsc::Sender<FetchRequest>,
        refresh_tx: mpsc::Sender<RefreshRequest>,
        mail_mgr: Arc<Mutex<MailManager>>,
        sink: Arc<dyn crate::node_event_sink::NodeEventSink>,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(SyncState::Idle {
                last_walked_root: None,
            })),
            fetch_tx,
            refresh_tx,
            mail_mgr,
            sink,
            in_flight_bodies: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Handle a root CID payload received via Zenoh sub on
    /// `harmony/mail/v1/{addr}/root`. Spawns a walker pass.
    pub async fn handle_root_push(self: Arc<Self>, payload: &[u8]) {
        let Ok(root) = <[u8; CID_LEN]>::try_from(payload) else {
            tracing::warn!(
                len = payload.len(),
                "ignoring malformed root push (expected 32 bytes)"
            );
            return;
        };
        self.start_or_queue_walk(root).await;
    }

    /// Handle a reply from the cold-start Zenoh `get` query (or a manual
    /// refresh, which routes through the same handler).
    /// - `None` / empty payload → gateway explicitly says "no mail yet".
    ///   Emit `idle` so any prior `error` from a failed previous attempt
    ///   gets cleared from the UI.
    /// - Non-empty 32-byte payload → kick off a walk.
    /// - Malformed (wrong length) → log; leave state as-is.
    pub async fn handle_startup_query_reply(self: Arc<Self>, payload: Option<&[u8]>) {
        match payload {
            None | Some(b"") => {
                tracing::info!("startup/refresh query: no mail for this address yet");
                self.clear_error_to_idle();
            }
            Some(bytes) => {
                if let Ok(root) = <[u8; CID_LEN]>::try_from(bytes) {
                    self.start_or_queue_walk(root).await;
                } else {
                    tracing::warn!(len = bytes.len(), "ignoring malformed startup query reply");
                }
            }
        }
    }

    /// Transition Error → Idle without disturbing Walking/Idle. Called when
    /// a refresh proves the gateway is reachable and reports no mail —
    /// the previous error event no longer reflects truth.
    fn clear_error_to_idle(self: &Arc<Self>) {
        let should_emit = {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            if let SyncState::Error {
                last_walked_root, ..
            } = &*state
            {
                let last = *last_walked_root;
                *state = SyncState::Idle {
                    last_walked_root: last,
                };
                true
            } else {
                false
            }
        };
        if should_emit {
            self.emit_status(SyncStatusEvent {
                state: "idle",
                error: None,
            });
        }
    }

    /// Manual refresh trigger from UI. Issues a fresh Zenoh get for the
    /// current root and walks if the gateway has one.
    pub async fn refresh_now(self: Arc<Self>) {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self.refresh_tx.send(reply_tx).await.is_err() {
            let msg = "refresh channel closed".to_string();
            tracing::warn!("{msg}");
            self.report_query_error(msg);
            return;
        }
        match tokio::time::timeout(std::time::Duration::from_secs(10), reply_rx).await {
            Ok(Ok(Ok(Some(payload)))) => {
                self.handle_startup_query_reply(Some(&payload)).await;
            }
            Ok(Ok(Ok(None))) => {
                // No responder answered. Distinguish from explicit empty
                // payload (which means "no mail yet" and is success) so the
                // user sees a real error and can retry — silent treatment
                // would look like the inbox is genuinely empty.
                tracing::warn!("refresh root query: no responder");
                self.report_query_error("no gateway responded to refresh".to_string());
            }
            Ok(Ok(Err(e))) => {
                tracing::warn!(error = %e, "refresh root query failed");
                self.report_query_error(format!("refresh failed: {e}"));
            }
            Ok(Err(_)) => {
                tracing::warn!("refresh reply channel dropped");
                self.report_query_error("refresh reply dropped".to_string());
            }
            Err(_) => {
                tracing::warn!("refresh root query timed out (10s)");
                self.report_query_error("refresh timed out (10s)".to_string());
            }
        }
    }

    /// Surface a query/refresh failure to the UI AND record it on the
    /// walker state machine, so a later successful refresh (which calls
    /// `clear_error_to_idle`) can detect the prior error and clear it.
    /// Public so the cold-start spawn in `event_loop` can use the same
    /// channel as `refresh_now`.
    ///
    /// ZEB-443: dedup is derived from the CURRENT state — an identical
    /// message while already in `Error` is suppressed (no rewrite, no
    /// re-emit). The mail-root retry driver re-reports indefinitely at
    /// its backoff cap; without this, a node that can't reach a gateway
    /// re-emits the same UI event every attempt. Deriving from state
    /// (rather than a caller-side latch) means ANY transition away from
    /// `Error` — a startup reply, a successful manual `refresh_now` —
    /// naturally re-arms reporting for the next outage.
    ///
    /// If the walker is mid-`Walking`, the in-flight pass is left alone —
    /// it will emit its own terminal status when it finishes. Recording
    /// the error during a Walking pass would also trip `finish_walk`'s
    /// pending-root accounting. The transient emit is informational; the
    /// active walk is the source of truth.
    ///
    /// Returns whether the error was surfaced (false = identical
    /// duplicate suppressed).
    pub fn report_query_error(self: &Arc<Self>, message: String) -> bool {
        {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            match &*state {
                SyncState::Walking { .. } => {
                    // Don't overwrite Walking; the active pass owns the state.
                }
                SyncState::Error { last_error, .. } if *last_error == message => {
                    // Same error already surfaced and still current —
                    // suppress the duplicate state rewrite and emit.
                    return false;
                }
                SyncState::Idle { last_walked_root }
                | SyncState::Error {
                    last_walked_root, ..
                } => {
                    let last = *last_walked_root;
                    *state = SyncState::Error {
                        last_error: message.clone(),
                        last_walked_root: last,
                    };
                }
            }
        }
        self.emit_status(SyncStatusEvent {
            state: "error",
            error: Some(message),
        });
        true
    }

    /// Lazy body fetch. Called from the fetch_mail_body Tauri command.
    ///
    /// In-flight dedup: if another caller is already fetching the same CID,
    /// the second caller awaits the first's result via a shared `watch`
    /// channel rather than issuing a duplicate outbound fetch.
    ///
    /// On success: persists the blob and promotes the Pending entry to Local
    /// via MailManager::mark_body_received.
    ///
    /// Cancellation: if the primary's future is dropped mid-fetch, an RAII
    /// drop guard removes the in-flight entry so subsequent callers start a
    /// fresh fetch rather than subscribe to a dead channel. (Subscribers
    /// already awaiting the dropped `tx` will wake via `rx.changed()`
    /// returning `Err` and surface "in-flight fetch cancelled" — they do NOT
    /// hang, but they also don't auto-retry; the next explicit call does.)
    pub async fn fetch_body(self: Arc<Self>, cid: [u8; CID_LEN]) -> Result<Vec<u8>, String> {
        // Atomic check-or-register under a single lock to close the TOCTOU
        // window: without this, two concurrent first-callers could both
        // observe an empty map, both insert their own watch channels
        // (second overwrites first), and both issue duplicate outbound
        // fetches — violating the dedup contract this test suite asserts.
        enum Role {
            Subscriber(tokio::sync::watch::Receiver<Option<Result<Vec<u8>, String>>>),
            Primary(tokio::sync::watch::Sender<Option<Result<Vec<u8>, String>>>),
        }
        let role = {
            let mut map = self
                .in_flight_bodies
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if let Some(rx) = map.get(&cid).cloned() {
                Role::Subscriber(rx)
            } else {
                let (tx, rx) = tokio::sync::watch::channel(None);
                map.insert(cid, rx);
                Role::Primary(tx)
            }
            // Lock drops at end of block, before any .await.
        };

        if let Role::Subscriber(mut rx) = role {
            loop {
                if let Some(result) = rx.borrow().clone() {
                    return result;
                }
                if rx.changed().await.is_err() {
                    return Err("in-flight fetch cancelled".to_string());
                }
            }
        }
        let Role::Primary(tx) = role else {
            unreachable!()
        };

        // RAII guard: removes the in-flight entry when this function returns
        // OR is dropped mid-fetch (cancellation). Ensures a cancelled primary
        // never strands its CID in the map indefinitely.
        struct InFlightGuard<'a> {
            map: &'a InFlightMap,
            cid: [u8; CID_LEN],
        }
        impl Drop for InFlightGuard<'_> {
            fn drop(&mut self) {
                // Recover from poison so cancellation cleanup still runs even
                // if a previous holder panicked. Matches the lock recovery
                // pattern used elsewhere in this module.
                let mut m = self.map.lock().unwrap_or_else(|p| p.into_inner());
                m.remove(&self.cid);
            }
        }
        let _guard = InFlightGuard {
            map: &self.in_flight_bodies,
            cid,
        };

        // Perform the actual fetch (BLAKE3-verified by fetch_cas) + parse
        // + persist.
        let result: Result<Vec<u8>, String> = async {
            let bytes = self.fetch_cas(cid).await?;

            // Structural validation: bytes must parse as a HarmonyMessage.
            harmony_mailbox::message::HarmonyMessage::from_bytes(&bytes)
                .map_err(|e| format!("parse: {e}"))?;

            // Persist via MailManager (writes blob, promotes matching Pending entries).
            let cid_hex = hex::encode(cid);
            {
                let mut mgr = self.mail_mgr.lock().unwrap_or_else(|p| p.into_inner());
                mgr.mark_body_received(&cid_hex, &bytes)?;
            }

            Ok(bytes)
        }
        .await;

        // Publish the result to all subscribers. The drop guard clears the
        // map entry AFTER this function returns (drop order: guard runs
        // before tx). A late subscriber that clones the rx between publish
        // and guard-drop sees Some(result) on the first borrow() and
        // returns without awaiting — correct.
        let _ = tx.send(Some(result.clone()));
        result
    }

    async fn start_or_queue_walk(self: Arc<Self>, root: [u8; CID_LEN]) {
        // Single-flight: if already walking, queue the new root for later.
        // Skip duplicates against the active root, the queued pending root,
        // and (for Idle only) the most recent successfully-walked root.
        let should_spawn = {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            match &mut *state {
                SyncState::Walking {
                    root: walking_root,
                    pending_root,
                    prev_last_walked_root,
                    ..
                } => {
                    // Also dedup against prev_last_walked_root: if we just
                    // successfully walked this root immediately before the
                    // current pass, a re-push (e.g., from a different gateway
                    // peer that hadn't seen the newer root yet) is redundant
                    // — queueing it would schedule a wasted second pass.
                    if walking_root == &root
                        || pending_root.as_ref() == Some(&root)
                        || prev_last_walked_root.as_ref() == Some(&root)
                    {
                        return;
                    }
                    *pending_root = Some(root);
                    false
                }
                SyncState::Idle { last_walked_root } => {
                    if last_walked_root.as_ref() == Some(&root) {
                        // Already on this root — no work needed.
                        return;
                    }
                    let prev = *last_walked_root;
                    *state = SyncState::Walking {
                        root,
                        started_at: Instant::now(),
                        pending_root: None,
                        prev_last_walked_root: prev,
                    };
                    true
                }
                SyncState::Error {
                    last_walked_root, ..
                } => {
                    // Don't dedup on Error: a retry is exactly what the user
                    // wants. Carry the prior last_walked_root forward so a
                    // subsequent failure can preserve it.
                    let prev = *last_walked_root;
                    *state = SyncState::Walking {
                        root,
                        started_at: Instant::now(),
                        pending_root: None,
                        prev_last_walked_root: prev,
                    };
                    true
                }
            }
        };

        if should_spawn {
            let me = Arc::clone(&self);
            tokio::spawn(async move {
                me.run_walk_loop(root).await;
            });
        }
    }

    /// Drives walk passes back-to-back inside a single spawned task. Each
    /// pass either terminates (returns None) or hands off the queued
    /// `pending_root` to the next iteration. Looping inline avoids the
    /// `tokio::spawn` race where a newer push could overtake an older
    /// queued root and leave `last_walked_root` pointing at stale state.
    async fn run_walk_loop(self: Arc<Self>, mut root: [u8; CID_LEN]) {
        loop {
            match self.clone().run_walk_pass(root).await {
                Some(next) => root = next,
                None => return,
            }
        }
    }

    /// Fetch a CAS blob via the event_loop's fetch channel and verify the
    /// returned bytes hash to the requested CID. 30-second budget covers
    /// both the outbound send (which can block on channel backpressure)
    /// and the reply — a stalled fetcher must never strand the walker in
    /// Walking state indefinitely.
    ///
    /// The hash check is centralized here so every CAS read (root, folder,
    /// page, body) is integrity-verified — without it a misbehaving or
    /// stale responder could steer the walker down the wrong tree.
    async fn fetch_cas(&self, cid: [u8; CID_LEN]) -> Result<Vec<u8>, String> {
        let cid_hex = hex::encode(cid);
        let work = async {
            let (reply_tx, reply_rx) = oneshot::channel();
            self.fetch_tx
                .send(FetchRequest {
                    cid_hex: cid_hex.clone(),
                    reply: reply_tx,
                    max_bytes: None,
                    serveable: false,
                })
                .await
                .map_err(|_| "fetch channel closed".to_string())?;
            reply_rx
                .await
                .map_err(|_| "fetch reply channel dropped".to_string())?
        };
        let bytes = match tokio::time::timeout(std::time::Duration::from_secs(30), work).await {
            Ok(result) => result?,
            Err(_) => return Err(format!("fetch timeout for {cid_hex}")),
        };
        let computed = blake3::hash(&bytes);
        if computed.as_bytes() != &cid {
            return Err(format!(
                "hash mismatch: claimed {cid_hex}, computed {}",
                hex::encode(computed.as_bytes())
            ));
        }
        Ok(bytes)
    }

    /// One full walk pass. Returns `Some(next_root)` when a queued
    /// `pending_root` needs another pass, or `None` when the walker
    /// should park.
    async fn run_walk_pass(self: Arc<Self>, root: [u8; CID_LEN]) -> Option<[u8; CID_LEN]> {
        use harmony_mailbox::mailbox::{FolderKind, MailFolder, MailRoot};

        self.emit_status(SyncStatusEvent {
            state: "syncing",
            error: None,
        });

        // Step 1: fetch + parse root.
        let root_bytes = match self.fetch_cas(root).await {
            Ok(b) => b,
            Err(e) => return self.finish_walk_error(format!("root fetch: {e}")),
        };
        let mail_root = match MailRoot::from_bytes(&root_bytes) {
            Ok(r) => r,
            Err(e) => return self.finish_walk_error(format!("root parse: {e}")),
        };

        // Step 2: fetch + parse Inbox folder.
        let folder_cid: [u8; CID_LEN] = *mail_root.folder_cid(FolderKind::Inbox);
        let folder_bytes = match self.fetch_cas(folder_cid).await {
            Ok(b) => b,
            Err(e) => return self.finish_walk_error(format!("folder fetch: {e}")),
        };
        let folder = match MailFolder::from_bytes(&folder_bytes) {
            Ok(f) => f,
            Err(e) => return self.finish_walk_error(format!("folder parse: {e}")),
        };

        // Step 3: walk pages. Local persistence failures are treated as
        // fatal (they leave in-memory state inconsistent); remote
        // page fetch/parse failures are skipped per the hybrid policy.
        match self.walk_pages(&folder.page_cids).await {
            Ok(skip_summary) => self.finish_walk(root, skip_summary),
            Err(fatal) => self.finish_walk_error(fatal),
        }
    }

    /// Walk the page CID list, fetching up to `MAX_CONCURRENT_PAGES` pages in
    /// parallel, parsing each, and registering every entry as header-only.
    ///
    /// Hybrid error policy: page fetch and parse failures are logged and the
    /// page is skipped (returned as `Ok(Some(skip_summary))`). Local
    /// persistence failures (`register_header_only_no_persist`, `flush_index`)
    /// are fatal — they leave in-memory state inconsistent — and bubble up
    /// as `Err`.
    ///
    /// Page entries are registered in REVERSE wire order so the inbox's
    /// newest-first invariant (insert at index 0) ends up matching the
    /// gateway's newest-first ordering of pages and within-page entries.
    async fn walk_pages(&self, page_cids: &[[u8; CID_LEN]]) -> Result<Option<String>, String> {
        use futures::future::join_all;
        use harmony_mailbox::mailbox::MailPage;
        use tokio::sync::Semaphore;

        // Bounds peak in-flight FetchRequest count (memory + channel pressure),
        // NOT effective parallelism — the downstream fetcher in event_loop
        // controls how many CAS fetches actually run concurrently. If that
        // fetcher is serial, the walk is serial regardless of this bound.
        const MAX_CONCURRENT_PAGES: usize = 8;
        let sem = Arc::new(Semaphore::new(MAX_CONCURRENT_PAGES));

        // Fetch all pages in parallel (bounded by semaphore). fetch_cas
        // BLAKE3-verifies every blob before returning bytes.
        let fetch_results: Vec<(String, Result<Vec<u8>, String>)> =
            join_all(page_cids.iter().map(|cid| {
                let cid = *cid;
                let cid_hex = hex::encode(cid);
                let sem = Arc::clone(&sem);
                async move {
                    let _permit = sem.acquire().await.unwrap();
                    let bytes = self.fetch_cas(cid).await;
                    (cid_hex, bytes)
                }
            }))
            .await;

        // Parse pages in wire order, capturing skip stats. Successful pages
        // are buffered for reverse-order registration below.
        let mut skipped_pages: Vec<String> = Vec::new();
        let mut parsed_pages: Vec<MailPage> = Vec::with_capacity(page_cids.len());

        for (page_cid_hex, fetch_result) in fetch_results {
            let bytes = match fetch_result {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(
                        page_cid = %page_cid_hex,
                        error = %e,
                        "page fetch failed; skipping"
                    );
                    skipped_pages.push(page_cid_hex);
                    continue;
                }
            };
            match MailPage::from_bytes(&bytes) {
                Ok(p) => parsed_pages.push(p),
                Err(e) => {
                    tracing::warn!(
                        page_cid = %page_cid_hex,
                        error = %e,
                        "page parse failed; skipping"
                    );
                    skipped_pages.push(page_cid_hex);
                }
            }
        }

        // Register entries in REVERSE wire order under a single lock, then
        // flush index once. This keeps display ordering newest-first (each
        // insert(0) on top of an oldest-first build) and amortizes disk I/O
        // — one save_index per walk instead of one per entry.
        let mut new_entry_cids: Vec<String> = Vec::new();
        {
            let mut mgr = self.mail_mgr.lock().unwrap_or_else(|p| p.into_inner());
            for page in parsed_pages.into_iter().rev() {
                for entry in page.entries.into_iter().rev() {
                    match mgr.register_header_only_no_persist(entry) {
                        Ok(crate::mail::RegisterOutcome::Inserted { cid }) => {
                            new_entry_cids.push(cid);
                        }
                        Ok(crate::mail::RegisterOutcome::Duplicate) => {}
                        Err(e) => {
                            // Local error — index may be inconsistent. Abort
                            // the walk so we don't silently lose entries.
                            return Err(format!("register header: {e}"));
                        }
                    }
                }
            }
            mgr.flush_index().map_err(|e| format!("flush index: {e}"))?;
        }

        // Emit per-new-entry Tauri events so the frontend updates its inbox.
        // Matches Phase 0 receive_message pattern: one event per newly-inserted
        // entry, with the EntryRecord as payload. Emitted AFTER flush so
        // crash-recovery from disk matches what we surfaced over the channel.
        //
        // Delta-only stream: the frontend must seed its inbox view from a
        // `list_folder`-backed command on mount. If the walker crashes
        // between flush and emit, the entries are persisted but no event
        // fires on a subsequent re-walk (Duplicate → silent). The UI
        // recovers from persisted state on next list_folder, not from
        // replayed events.
        //
        // Resolve all entries in ONE lock acquisition (O(N)) rather than
        // re-scanning the inbox per CID (O(N²) and previously capped at 1000
        // which would silently drop events for large backfills).
        let new_cid_set: std::collections::HashSet<String> =
            new_entry_cids.iter().cloned().collect();
        let entries_to_emit: Vec<crate::mail::EntryRecord> = if new_cid_set.is_empty() {
            Vec::new()
        } else {
            let mgr = self.mail_mgr.lock().unwrap_or_else(|p| p.into_inner());
            // list_folder returns an owned Vec — safe to drop guard after.
            mgr.list_folder("inbox", 0, usize::MAX)
                .into_iter()
                .filter(|e| new_cid_set.contains(&e.message_cid))
                .collect()
            // guard drops here at end of block scope.
        };
        for entry in entries_to_emit {
            crate::node_event_sink::emit_ser(&*self.sink, "mail-received", &entry);
        }

        if skipped_pages.is_empty() {
            Ok(None)
        } else {
            Ok(Some(format!("skipped {} page(s)", skipped_pages.len())))
        }
    }

    /// Single locked scope: read prior `last_walked_root`/`pending_root`,
    /// then install the new state atomically. If a `pending_root` was
    /// queued during the failed walk, transition directly to a fresh
    /// Walking state for that root and return it — the caller's loop will
    /// pick it up as the next pass. Otherwise fall through to Error and
    /// return None.
    fn finish_walk_error(self: Arc<Self>, error: String) -> Option<[u8; CID_LEN]> {
        let next_root = {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            let (carried_last_walked, pending) = match &*state {
                SyncState::Walking {
                    pending_root,
                    prev_last_walked_root,
                    ..
                } => (*prev_last_walked_root, *pending_root),
                SyncState::Idle { last_walked_root }
                | SyncState::Error {
                    last_walked_root, ..
                } => (*last_walked_root, None),
            };
            match pending {
                Some(next) => {
                    *state = SyncState::Walking {
                        root: next,
                        started_at: Instant::now(),
                        pending_root: None,
                        prev_last_walked_root: carried_last_walked,
                    };
                }
                None => {
                    *state = SyncState::Error {
                        last_error: error.clone(),
                        last_walked_root: carried_last_walked,
                    };
                }
            }
            pending
        };
        // Skip the terminal emit when a queued root is about to drive a
        // fresh pass — otherwise the UI flickers error → syncing for an
        // error message that already references the stale failed walk.
        // The next pass's run_walk_pass will emit "syncing" itself.
        if next_root.is_none() {
            self.emit_status(SyncStatusEvent {
                state: "error",
                error: Some(error),
            });
        }
        next_root
    }

    fn finish_walk(
        self: Arc<Self>,
        root: [u8; CID_LEN],
        skip_summary: Option<String>,
    ) -> Option<[u8; CID_LEN]> {
        let next_root = {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            let pending = if let SyncState::Walking { pending_root, .. } = &*state {
                *pending_root
            } else {
                None
            };
            match pending {
                Some(next) => {
                    // Promote queued root to active inline — same lock,
                    // no `tokio::spawn` race window where a newer push
                    // could overtake.
                    *state = SyncState::Walking {
                        root: next,
                        started_at: Instant::now(),
                        pending_root: None,
                        prev_last_walked_root: Some(root),
                    };
                }
                None => {
                    *state = match skip_summary.clone() {
                        None => SyncState::Idle {
                            last_walked_root: Some(root),
                        },
                        Some(summary) => SyncState::Error {
                            last_error: summary,
                            last_walked_root: Some(root),
                        },
                    };
                }
            }
            pending
        };

        // Only emit a terminal event when this pass actually parks the
        // walker. If a pending root was queued, the next pass will emit
        // its own "syncing" — skipping this avoids an idle→syncing flicker.
        if next_root.is_none() {
            let event = match skip_summary {
                None => SyncStatusEvent {
                    state: "idle",
                    error: None,
                },
                Some(summary) => SyncStatusEvent {
                    state: "error",
                    error: Some(summary),
                },
            };
            self.emit_status(event);
        }

        next_root
    }

    fn emit_status(&self, event: SyncStatusEvent) {
        crate::node_event_sink::emit_ser(&*self.sink, "mail-sync-status", &event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::BodyState;
    use harmony_mailbox::mailbox::{
        FolderKind, MailFolder, MailPage, MailRoot, MessageEntry, MAILBOX_VERSION,
    };
    use harmony_mailbox::message::{
        unique_message_id, HarmonyMessage, MailMessageType, MessageFlags, Recipient, RecipientType,
        ADDRESS_HASH_LEN,
    };

    // Local copy of the mail.rs test helper since test helpers don't cross module boundaries.
    fn make_test_harmony_message(subject: &str, sender: [u8; ADDRESS_HASH_LEN]) -> HarmonyMessage {
        HarmonyMessage {
            version: 0x01,
            message_type: MailMessageType::Email,
            flags: MessageFlags::new(false, false, false),
            timestamp: 1_744_403_200,
            message_id: unique_message_id(),
            in_reply_to: None,
            sender_address: sender,
            recipients: vec![Recipient {
                address_hash: [0xBB; ADDRESS_HASH_LEN],
                recipient_type: RecipientType::To,
            }],
            subject: subject.to_string(),
            body: "Hello, world!".to_string(),
            attachments: vec![],
        }
    }

    /// Test harness: a stub fetch responder backed by a HashMap of CID → bytes.
    /// Bytes not in the map return NotFound errors.
    struct StubFetcher {
        responses: HashMap<[u8; CID_LEN], Vec<u8>>,
    }

    impl StubFetcher {
        fn new() -> Self {
            Self {
                responses: HashMap::new(),
            }
        }
        fn insert(&mut self, cid: [u8; CID_LEN], bytes: Vec<u8>) {
            self.responses.insert(cid, bytes);
        }
        async fn run(self, mut rx: mpsc::Receiver<FetchRequest>) {
            while let Some(req) = rx.recv().await {
                let cid_bytes = hex::decode(&req.cid_hex).unwrap();
                let cid: [u8; CID_LEN] = cid_bytes.try_into().unwrap();
                let result = self
                    .responses
                    .get(&cid)
                    .cloned()
                    .ok_or_else(|| format!("not found: {}", req.cid_hex));
                let _ = req.reply.send(result);
            }
        }
    }

    /// Build a test MailSync with a recording event sink.
    fn make_test_mail_sync(
        fetch_tx: mpsc::Sender<FetchRequest>,
        mail_mgr: Arc<Mutex<MailManager>>,
    ) -> Arc<MailSync> {
        // NodeEventSink is impl'd on Arc<RecordingSink>, so wrap once more
        // to coerce into Arc<dyn NodeEventSink>.
        let sink: Arc<dyn crate::node_event_sink::NodeEventSink> =
            Arc::new(crate::node_event_sink::RecordingSink::new());
        // Throwaway refresh channel — tests don't exercise refresh_now directly.
        let (refresh_tx, _refresh_rx) = mpsc::channel(1);
        Arc::new(MailSync::new(fetch_tx, refresh_tx, mail_mgr, sink))
    }

    #[tokio::test]
    async fn walk_aborts_on_root_fetch_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let mail_mgr = Arc::new(Mutex::new(MailManager::load(
            Some(&crate::device_dataset_file::test_cipher()),
            &tmp.path().join("mail"),
            [0u8; 16],
        )));
        let (fetch_tx, fetch_rx) = mpsc::channel(16);

        // No responses inserted — root fetch will fail.
        tokio::spawn(StubFetcher::new().run(fetch_rx));

        let sync = make_test_mail_sync(fetch_tx, mail_mgr.clone());
        let bad_root = [0xDE; CID_LEN];
        sync.clone().handle_root_push(&bad_root).await;

        wait_for_terminal_state(&sync, std::time::Duration::from_secs(2)).await;

        // Inbox empty, state should be Error.
        let inbox = mail_mgr.lock().unwrap().list_folder("inbox", 0, 100);
        assert_eq!(inbox.len(), 0);
        {
            let guard = sync.state.lock().unwrap();
            match &*guard {
                SyncState::Error { last_error, .. } => {
                    assert!(
                        last_error.contains("not found") || last_error.contains("timeout"),
                        "unexpected error: {last_error}"
                    );
                }
                other => panic!("expected Error, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn walk_aborts_on_folder_fetch_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let mail_mgr = Arc::new(Mutex::new(MailManager::load(
            Some(&crate::device_dataset_file::test_cipher()),
            &tmp.path().join("mail"),
            [0u8; 16],
        )));
        let (fetch_tx, fetch_rx) = mpsc::channel(16);

        // Construct a valid MailRoot pointing at a folder CID we won't serve.
        let folder_cid = [0xF0; CID_LEN];
        let root = MailRoot::new_empty([0u8; 16], 1700000000).with_folder(
            FolderKind::Inbox,
            folder_cid,
            1700000001,
        );
        let root_bytes = root
            .to_bytes()
            .expect("test fixture root encodes at MAILBOX_VERSION");
        let root_cid: [u8; 32] = *blake3::hash(&root_bytes).as_bytes();

        let mut stub = StubFetcher::new();
        stub.insert(root_cid, root_bytes);
        // folder_cid intentionally NOT inserted.
        tokio::spawn(stub.run(fetch_rx));

        let sync = make_test_mail_sync(fetch_tx, mail_mgr.clone());
        sync.clone().handle_root_push(&root_cid).await;
        wait_for_terminal_state(&sync, std::time::Duration::from_secs(2)).await;

        let inbox = mail_mgr.lock().unwrap().list_folder("inbox", 0, 100);
        assert_eq!(inbox.len(), 0);
        {
            let guard = sync.state.lock().unwrap();
            match &*guard {
                SyncState::Error { .. } => {}
                other => panic!("expected Error, got {other:?}"),
            }
        }
    }

    /// Poll sync.state until it reaches Idle or Error (or deadline). Returns on
    /// terminal state or after the deadline (whichever comes first).
    async fn wait_for_terminal_state(sync: &MailSync, deadline: std::time::Duration) {
        let start = std::time::Instant::now();
        while start.elapsed() < deadline {
            let is_terminal = matches!(
                &*sync.state.lock().unwrap(),
                SyncState::Idle { .. } | SyncState::Error { .. }
            );
            if is_terminal {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn walk_single_page_registers_all_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let mail_mgr = Arc::new(Mutex::new(MailManager::load(
            Some(&crate::device_dataset_file::test_cipher()),
            &tmp.path().join("mail"),
            [0u8; 16],
        )));
        let (fetch_tx, fetch_rx) = mpsc::channel(16);

        // Build: 1 page with 3 entries → 1 folder pointing at it → 1 root.
        let entries: Vec<MessageEntry> = (0..3)
            .map(|i| MessageEntry {
                message_cid: [i as u8; 32],
                message_id: [i as u8; 16],
                sender_address: [0xCC; 16],
                timestamp: 1_700_000_000 + i as u64,
                subject_snippet: format!("entry {i}"),
                read: false,
            })
            .collect();

        let page = MailPage {
            version: MAILBOX_VERSION,
            entries,
        };
        let page_bytes = page.to_bytes().unwrap();
        let page_cid: [u8; 32] = *blake3::hash(&page_bytes).as_bytes();

        let folder = MailFolder {
            version: MAILBOX_VERSION,
            message_count: 3,
            unread_count: 3,
            page_cids: vec![page_cid],
        };
        let folder_bytes = folder.to_bytes().unwrap();
        let folder_cid: [u8; 32] = *blake3::hash(&folder_bytes).as_bytes();

        let root = MailRoot::new_empty([0u8; 16], 1_700_000_000).with_folder(
            FolderKind::Inbox,
            folder_cid,
            1_700_000_001,
        );
        let root_bytes = root
            .to_bytes()
            .expect("test fixture root encodes at MAILBOX_VERSION");
        let root_cid: [u8; 32] = *blake3::hash(&root_bytes).as_bytes();

        let mut stub = StubFetcher::new();
        stub.insert(root_cid, root_bytes);
        stub.insert(folder_cid, folder_bytes);
        stub.insert(page_cid, page_bytes);
        tokio::spawn(stub.run(fetch_rx));

        let sync = make_test_mail_sync(fetch_tx, mail_mgr.clone());
        sync.clone().handle_root_push(&root_cid).await;

        wait_for_terminal_state(&sync, std::time::Duration::from_secs(2)).await;

        let inbox = mail_mgr.lock().unwrap().list_folder("inbox", 0, 100);
        assert_eq!(inbox.len(), 3, "all 3 entries registered");
        assert!(
            inbox
                .iter()
                .all(|e| e.body_state == crate::mail::BodyState::Pending),
            "all entries should be Pending (header-only)"
        );
        match &*sync.state.lock().unwrap() {
            SyncState::Idle { .. } => {}
            other => panic!("expected Idle after successful walk, got {other:?}"),
        };
    }

    #[tokio::test]
    async fn walk_skips_missing_page_continues_others() {
        let tmp = tempfile::tempdir().unwrap();
        let mail_mgr = Arc::new(Mutex::new(MailManager::load(
            Some(&crate::device_dataset_file::test_cipher()),
            &tmp.path().join("mail"),
            [0u8; 16],
        )));
        let (fetch_tx, fetch_rx) = mpsc::channel(16);

        // 2 pages; only page1 served. page2 → 404, skipped.
        let entry1 = MessageEntry {
            message_cid: [1; 32],
            message_id: [1; 16],
            sender_address: [0; 16],
            timestamp: 1_700_000_001,
            subject_snippet: "page1 entry".to_string(),
            read: false,
        };
        let page1 = MailPage {
            version: MAILBOX_VERSION,
            entries: vec![entry1],
        };
        let page1_bytes = page1.to_bytes().unwrap();
        let page1_cid: [u8; 32] = *blake3::hash(&page1_bytes).as_bytes();

        let page2_cid: [u8; 32] = [0xFE; 32]; // unserved → 404

        let folder = MailFolder {
            version: MAILBOX_VERSION,
            message_count: 2,
            unread_count: 2,
            page_cids: vec![page1_cid, page2_cid],
        };
        let folder_bytes = folder.to_bytes().unwrap();
        let folder_cid: [u8; 32] = *blake3::hash(&folder_bytes).as_bytes();

        let root = MailRoot::new_empty([0u8; 16], 1_700_000_000).with_folder(
            FolderKind::Inbox,
            folder_cid,
            1_700_000_001,
        );
        let root_bytes = root
            .to_bytes()
            .expect("test fixture root encodes at MAILBOX_VERSION");
        let root_cid: [u8; 32] = *blake3::hash(&root_bytes).as_bytes();

        let mut stub = StubFetcher::new();
        stub.insert(root_cid, root_bytes);
        stub.insert(folder_cid, folder_bytes);
        stub.insert(page1_cid, page1_bytes);
        // page2_cid intentionally NOT inserted.
        tokio::spawn(stub.run(fetch_rx));

        let sync = make_test_mail_sync(fetch_tx, mail_mgr.clone());
        sync.clone().handle_root_push(&root_cid).await;
        wait_for_terminal_state(&sync, std::time::Duration::from_secs(2)).await;

        let inbox = mail_mgr.lock().unwrap().list_folder("inbox", 0, 100);
        assert_eq!(inbox.len(), 1, "page1 entry registered, page2 skipped");
        match &*sync.state.lock().unwrap() {
            SyncState::Error { last_error, .. } => {
                assert!(
                    last_error.contains("page") || last_error.contains("skip"),
                    "error should mention skipped pages: got {last_error}"
                );
            }
            other => panic!("expected Error after skip, got {other:?}"),
        };
    }

    #[tokio::test]
    async fn pending_root_during_walk_runs_after_current_pass() {
        let tmp = tempfile::tempdir().unwrap();
        let mail_mgr = Arc::new(Mutex::new(MailManager::load(
            Some(&crate::device_dataset_file::test_cipher()),
            &tmp.path().join("mail"),
            [0u8; 16],
        )));
        let (fetch_tx, fetch_rx) = mpsc::channel(32);

        // Build TWO different roots, each with one entry.
        #[allow(clippy::type_complexity)] // pre-existing test helper; tracked for cleanup
        fn make_tree(seed: u8) -> ([u8; 32], Vec<([u8; 32], Vec<u8>)>) {
            let entry = MessageEntry {
                message_cid: [seed; 32],
                message_id: [seed; 16],
                sender_address: [0; 16],
                timestamp: 1_700_000_000 + seed as u64,
                subject_snippet: format!("seed {seed}"),
                read: false,
            };
            let page = MailPage {
                version: MAILBOX_VERSION,
                entries: vec![entry],
            };
            let page_bytes = page.to_bytes().unwrap();
            let page_cid: [u8; 32] = *blake3::hash(&page_bytes).as_bytes();

            let folder = MailFolder {
                version: MAILBOX_VERSION,
                message_count: 1,
                unread_count: 1,
                page_cids: vec![page_cid],
            };
            let folder_bytes = folder.to_bytes().unwrap();
            let folder_cid: [u8; 32] = *blake3::hash(&folder_bytes).as_bytes();

            let root = MailRoot::new_empty([0; 16], 1_700_000_000).with_folder(
                FolderKind::Inbox,
                folder_cid,
                1_700_000_001,
            );
            let root_bytes = root
                .to_bytes()
                .expect("test fixture root encodes at MAILBOX_VERSION");
            let root_cid: [u8; 32] = *blake3::hash(&root_bytes).as_bytes();
            (
                root_cid,
                vec![
                    (root_cid, root_bytes),
                    (folder_cid, folder_bytes),
                    (page_cid, page_bytes),
                ],
            )
        }

        let (root1, blobs1) = make_tree(0xAA);
        let (root2, blobs2) = make_tree(0xBB);

        let mut stub = StubFetcher::new();
        for (cid, bytes) in blobs1.into_iter().chain(blobs2.into_iter()) {
            stub.insert(cid, bytes);
        }
        tokio::spawn(stub.run(fetch_rx));

        let sync = make_test_mail_sync(fetch_tx, mail_mgr.clone());

        // Push root1, then immediately push root2 before root1 walk completes.
        // The second push must coalesce as pending_root on the Walking state.
        sync.clone().handle_root_push(&root1).await;
        sync.clone().handle_root_push(&root2).await;

        // Wait until the walker returns to Idle (both walks complete).
        wait_for_terminal_state(&sync, std::time::Duration::from_secs(5)).await;

        let inbox = mail_mgr.lock().unwrap().list_folder("inbox", 0, 100);
        let message_ids: std::collections::HashSet<String> =
            inbox.iter().map(|e| e.message_id.clone()).collect();
        assert!(
            message_ids.contains(&hex::encode([0xAA; 16])),
            "root1 entry missing; inbox: {inbox:?}"
        );
        assert!(
            message_ids.contains(&hex::encode([0xBB; 16])),
            "root2 entry missing; inbox: {inbox:?}"
        );
        // After both walks complete, state should be Idle.
        {
            let guard = sync.state.lock().unwrap();
            assert!(
                matches!(&*guard, SyncState::Idle { .. }),
                "expected Idle after both walks, got {:?}",
                *guard
            );
        }
    }

    #[tokio::test]
    async fn fetch_body_returns_bytes_and_marks_local() {
        let tmp = tempfile::tempdir().unwrap();
        let mail_mgr = Arc::new(Mutex::new(MailManager::load(
            Some(&crate::device_dataset_file::test_cipher()),
            &tmp.path().join("mail"),
            [0u8; 16],
        )));
        let (fetch_tx, fetch_rx) = mpsc::channel(16);

        // Register a Pending entry for a known CID first.
        let msg = make_test_harmony_message("subj", [0xAA; 16]);
        let bytes = msg.to_bytes().unwrap();
        let cid: [u8; 32] = *blake3::hash(&bytes).as_bytes();
        let entry = MessageEntry {
            message_cid: cid,
            message_id: msg.message_id,
            sender_address: msg.sender_address,
            timestamp: msg.timestamp,
            subject_snippet: "subj".to_string(),
            read: false,
        };
        mail_mgr
            .lock()
            .unwrap()
            .register_header_only(entry)
            .unwrap();

        let mut stub = StubFetcher::new();
        stub.insert(cid, bytes.clone());
        tokio::spawn(stub.run(fetch_rx));

        let sync = make_test_mail_sync(fetch_tx, mail_mgr.clone());
        let result = sync.clone().fetch_body(cid).await.unwrap();
        assert_eq!(result, bytes);

        // Entry promoted to Local by mark_body_received.
        let inbox = mail_mgr.lock().unwrap().list_folder("inbox", 0, 100);
        assert_eq!(inbox[0].body_state, BodyState::Local);
    }

    #[tokio::test]
    async fn fetch_body_rejects_hash_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let mail_mgr = Arc::new(Mutex::new(MailManager::load(
            Some(&crate::device_dataset_file::test_cipher()),
            &tmp.path().join("mail"),
            [0u8; 16],
        )));
        let (fetch_tx, fetch_rx) = mpsc::channel(16);

        let claimed_cid: [u8; 32] = [0xDD; 32];
        let mut stub = StubFetcher::new();
        stub.insert(claimed_cid, b"wrong bytes that don't hash".to_vec());
        tokio::spawn(stub.run(fetch_rx));

        let sync = make_test_mail_sync(fetch_tx, mail_mgr.clone());
        let result = sync.fetch_body(claimed_cid).await;
        assert!(result.is_err(), "should reject; got {result:?}");
        assert!(
            result.unwrap_err().contains("hash mismatch"),
            "error should mention hash mismatch"
        );
    }

    #[tokio::test]
    async fn fetch_body_dedups_concurrent_calls() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let tmp = tempfile::tempdir().unwrap();
        let mail_mgr = Arc::new(Mutex::new(MailManager::load(
            Some(&crate::device_dataset_file::test_cipher()),
            &tmp.path().join("mail"),
            [0u8; 16],
        )));
        let (fetch_tx, mut fetch_rx) = mpsc::channel::<FetchRequest>(16);

        let msg = make_test_harmony_message("s", [0xCC; 16]);
        let bytes = msg.to_bytes().unwrap();
        let cid: [u8; 32] = *blake3::hash(&bytes).as_bytes();

        // Also register a Pending entry so mark_body_received has something to promote.
        let entry = MessageEntry {
            message_cid: cid,
            message_id: msg.message_id,
            sender_address: msg.sender_address,
            timestamp: msg.timestamp,
            subject_snippet: "s".to_string(),
            read: false,
        };
        mail_mgr
            .lock()
            .unwrap()
            .register_header_only(entry)
            .unwrap();

        // Custom fetcher that counts how many times it was asked.
        let count = Arc::new(AtomicUsize::new(0));
        let count_clone = Arc::clone(&count);
        let bytes_clone = bytes.clone();
        tokio::spawn(async move {
            while let Some(req) = fetch_rx.recv().await {
                count_clone.fetch_add(1, Ordering::SeqCst);
                // Slow response so the second fetch_body call lands while first is in flight.
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                let _ = req.reply.send(Ok(bytes_clone.clone()));
            }
        });

        let sync = make_test_mail_sync(fetch_tx, mail_mgr);
        let h1 = tokio::spawn({
            let s = sync.clone();
            async move { s.fetch_body(cid).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let h2 = tokio::spawn({
            let s = sync.clone();
            async move { s.fetch_body(cid).await }
        });

        let r1 = h1.await.unwrap().unwrap();
        let r2 = h2.await.unwrap().unwrap();
        assert_eq!(r1, bytes);
        assert_eq!(r2, bytes);
        assert_eq!(count.load(Ordering::SeqCst), 1, "should only fetch once");
    }

    /// Regression test for the cancellation-leak hazard: if the primary's
    /// future is dropped mid-fetch, the in-flight map entry MUST be cleared
    /// so subsequent callers can start a fresh fetch rather than be stuck
    /// subscribing to a dead channel forever.
    #[tokio::test]
    async fn fetch_body_cancellation_clears_in_flight_entry() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let tmp = tempfile::tempdir().unwrap();
        let mail_mgr = Arc::new(Mutex::new(MailManager::load(
            Some(&crate::device_dataset_file::test_cipher()),
            &tmp.path().join("mail"),
            [0u8; 16],
        )));
        let (fetch_tx, mut fetch_rx) = mpsc::channel::<FetchRequest>(16);

        let msg = make_test_harmony_message("cancel", [0xEE; 16]);
        let bytes = msg.to_bytes().unwrap();
        let cid: [u8; 32] = *blake3::hash(&bytes).as_bytes();
        let entry = MessageEntry {
            message_cid: cid,
            message_id: msg.message_id,
            sender_address: msg.sender_address,
            timestamp: msg.timestamp,
            subject_snippet: "cancel".to_string(),
            read: false,
        };
        mail_mgr
            .lock()
            .unwrap()
            .register_header_only(entry)
            .unwrap();

        // Slow fetcher counts invocations.
        let count = Arc::new(AtomicUsize::new(0));
        let count_clone = Arc::clone(&count);
        let bytes_clone = bytes.clone();
        tokio::spawn(async move {
            while let Some(req) = fetch_rx.recv().await {
                count_clone.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                let _ = req.reply.send(Ok(bytes_clone.clone()));
            }
        });

        let sync = make_test_mail_sync(fetch_tx, mail_mgr.clone());

        // Start a fetch and then cancel it mid-flight by aborting the task.
        let h1 = tokio::spawn({
            let s = sync.clone();
            async move { s.fetch_body(cid).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        h1.abort();
        // Give the drop guard a moment to run.
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;

        // Second call must succeed: a stranded map entry would cause it to
        // subscribe to the dead tx, get rx.changed().await Err, and fail.
        let r2 = sync.clone().fetch_body(cid).await;
        assert!(
            r2.is_ok(),
            "post-cancel fetch should start fresh, got {r2:?}"
        );
        assert_eq!(r2.unwrap(), bytes);

        // The counter should now be at least 2: the aborted fetch started
        // the fetcher once, and the post-cancel fetch_body started it again.
        let n = count.load(Ordering::SeqCst);
        assert!(
            n >= 2,
            "expected ≥2 outbound fetches after cancel-then-retry, got {n}"
        );
    }

    /// Builds a one-entry mailbox tree keyed by `seed`. Returns the root CID
    /// and every CAS blob the walker will need to reach the entry.
    #[allow(clippy::type_complexity)] // pre-existing test helper; tracked for cleanup
    fn make_one_entry_tree(seed: u8) -> ([u8; 32], Vec<([u8; 32], Vec<u8>)>) {
        let entry = MessageEntry {
            message_cid: [seed; 32],
            message_id: [seed; 16],
            sender_address: [0; 16],
            timestamp: 1_700_000_000 + seed as u64,
            subject_snippet: format!("seed {seed}"),
            read: false,
        };
        let page = MailPage {
            version: MAILBOX_VERSION,
            entries: vec![entry],
        };
        let page_bytes = page.to_bytes().unwrap();
        let page_cid: [u8; 32] = *blake3::hash(&page_bytes).as_bytes();

        let folder = MailFolder {
            version: MAILBOX_VERSION,
            message_count: 1,
            unread_count: 1,
            page_cids: vec![page_cid],
        };
        let folder_bytes = folder.to_bytes().unwrap();
        let folder_cid: [u8; 32] = *blake3::hash(&folder_bytes).as_bytes();

        let root = MailRoot::new_empty([0; 16], 1_700_000_000).with_folder(
            FolderKind::Inbox,
            folder_cid,
            1_700_000_001,
        );
        let root_bytes = root
            .to_bytes()
            .expect("test fixture root encodes at MAILBOX_VERSION");
        let root_cid: [u8; 32] = *blake3::hash(&root_bytes).as_bytes();
        (
            root_cid,
            vec![
                (root_cid, root_bytes),
                (folder_cid, folder_bytes),
                (page_cid, page_bytes),
            ],
        )
    }

    /// A duplicate of the most-recently-walked root (or active root, or
    /// queued root) must NOT trigger a second walk pass. Without this,
    /// the cold-start `get` reply plus the first live `/root` push for the
    /// same CID would cause the walker to re-traverse the entire tree.
    #[tokio::test]
    async fn duplicate_root_after_idle_does_not_rewalk() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let tmp = tempfile::tempdir().unwrap();
        let mail_mgr = Arc::new(Mutex::new(MailManager::load(
            Some(&crate::device_dataset_file::test_cipher()),
            &tmp.path().join("mail"),
            [0u8; 16],
        )));
        let (fetch_tx, mut fetch_rx) = mpsc::channel::<FetchRequest>(32);

        let (root_cid, blobs) = make_one_entry_tree(0xAA);
        let map: HashMap<[u8; 32], Vec<u8>> = blobs.into_iter().collect();
        let count = Arc::new(AtomicUsize::new(0));
        let count_clone = Arc::clone(&count);
        tokio::spawn(async move {
            while let Some(req) = fetch_rx.recv().await {
                count_clone.fetch_add(1, Ordering::SeqCst);
                let cid_bytes = hex::decode(&req.cid_hex).unwrap();
                let cid: [u8; 32] = cid_bytes.try_into().unwrap();
                let _ = req
                    .reply
                    .send(map.get(&cid).cloned().ok_or_else(|| "nf".into()));
            }
        });

        let sync = make_test_mail_sync(fetch_tx, mail_mgr.clone());
        sync.clone().handle_root_push(&root_cid).await;
        wait_for_terminal_state(&sync, std::time::Duration::from_secs(2)).await;
        let after_first = count.load(Ordering::SeqCst);

        // Re-push the same root. Should be deduped.
        sync.clone().handle_root_push(&root_cid).await;
        // Give any (incorrectly) spawned walk a chance to issue fetches.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let after_second = count.load(Ordering::SeqCst);

        assert_eq!(
            after_first, after_second,
            "duplicate root push should not re-fetch any blobs"
        );
    }

    /// If a strict failure (root/folder fetch/parse) happens while a newer
    /// `pending_root` is queued, that newer root MUST be picked up on the
    /// next pass — the failed walk's transition to Error must not silently
    /// drop the queued root.
    #[tokio::test]
    async fn pending_root_runs_after_strict_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let mail_mgr = Arc::new(Mutex::new(MailManager::load(
            Some(&crate::device_dataset_file::test_cipher()),
            &tmp.path().join("mail"),
            [0u8; 16],
        )));
        let (fetch_tx, mut fetch_rx) = mpsc::channel::<FetchRequest>(32);

        // Tree 1: root_cid maps to NO blob (root fetch will fail).
        let bad_root: [u8; 32] = [0xDE; 32];

        // Tree 2: a fully-served valid tree.
        let (good_root, good_blobs) = make_one_entry_tree(0xBB);
        let map: HashMap<[u8; 32], Vec<u8>> = good_blobs.into_iter().collect();

        // Slow fetcher so the second push lands while the first walk's
        // initial root fetch is still in flight.
        tokio::spawn(async move {
            while let Some(req) = fetch_rx.recv().await {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                let cid_bytes = hex::decode(&req.cid_hex).unwrap();
                let cid: [u8; 32] = cid_bytes.try_into().unwrap();
                let _ = req
                    .reply
                    .send(map.get(&cid).cloned().ok_or_else(|| "nf".into()));
            }
        });

        let sync = make_test_mail_sync(fetch_tx, mail_mgr.clone());
        sync.clone().handle_root_push(&bad_root).await;
        // Give the walker a moment to enter Walking on bad_root before the
        // second push so good_root coalesces as pending_root.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        sync.clone().handle_root_push(&good_root).await;

        wait_for_terminal_state(&sync, std::time::Duration::from_secs(5)).await;

        let inbox = mail_mgr.lock().unwrap().list_folder("inbox", 0, 100);
        assert_eq!(
            inbox.len(),
            1,
            "good_root entry should be registered after bad_root fails; inbox: {inbox:?}"
        );
    }

    /// During an active walk, a push for a root that was successfully
    /// walked _immediately before_ the current pass (carried in
    /// `prev_last_walked_root`) MUST also be deduped. Without this, a
    /// secondary gateway peer that hadn't yet seen the newer root would
    /// schedule a redundant pass after the current one finishes.
    #[tokio::test]
    async fn pending_root_dedups_against_prev_last_walked() {
        let tmp = tempfile::tempdir().unwrap();
        let mail_mgr = Arc::new(Mutex::new(MailManager::load(
            Some(&crate::device_dataset_file::test_cipher()),
            &tmp.path().join("mail"),
            [0u8; 16],
        )));
        let (fetch_tx, _fetch_rx) = mpsc::channel::<FetchRequest>(8);
        let sync = make_test_mail_sync(fetch_tx, mail_mgr);

        let r_old: [u8; CID_LEN] = [0xA1; CID_LEN];
        let r_active: [u8; CID_LEN] = [0xB2; CID_LEN];

        // Manually install a Walking state with prev_last_walked_root = r_old.
        // (Bypasses the spawn so we don't race the walker.)
        {
            let mut state = sync.state.lock().unwrap();
            *state = SyncState::Walking {
                root: r_active,
                started_at: Instant::now(),
                pending_root: None,
                prev_last_walked_root: Some(r_old),
            };
        }

        // Re-push r_old (the just-walked root) — should be deduped, not
        // overwrite pending_root.
        sync.clone().start_or_queue_walk(r_old).await;

        let guard = sync.state.lock().unwrap();
        match &*guard {
            SyncState::Walking { pending_root, .. } => {
                assert_eq!(
                    *pending_root, None,
                    "prev_last_walked_root match should not enqueue a new walk"
                );
            }
            other => panic!("expected Walking unchanged, got {other:?}"),
        }
    }

    /// `report_query_error` MUST install SyncState::Error (not just emit
    /// the event) — otherwise a subsequent successful empty reply can't
    /// recognize that there's an error to clear. Regression test for the
    /// state/event divergence introduced when the function was first
    /// wired up.
    #[tokio::test]
    async fn report_query_error_installs_error_state() {
        let tmp = tempfile::tempdir().unwrap();
        let mail_mgr = Arc::new(Mutex::new(MailManager::load(
            Some(&crate::device_dataset_file::test_cipher()),
            &tmp.path().join("mail"),
            [0u8; 16],
        )));
        let (fetch_tx, _fetch_rx) = mpsc::channel::<FetchRequest>(8);
        let sync = make_test_mail_sync(fetch_tx, mail_mgr);

        sync.report_query_error("startup query failed".to_string());

        match &*sync.state.lock().unwrap() {
            SyncState::Error { last_error, .. } => {
                assert_eq!(last_error, "startup query failed");
            }
            other => panic!("expected Error after report_query_error, got {other:?}"),
        }

        // And a follow-up successful empty reply clears it.
        sync.clone().handle_startup_query_reply(Some(b"")).await;
        let guard = sync.state.lock().unwrap();
        assert!(
            matches!(&*guard, SyncState::Idle { .. }),
            "expected Idle after empty reply, got {:?}",
            *guard
        );
    }

    /// ZEB-443: identical consecutive errors are suppressed — the mail-
    /// root retry driver re-reports indefinitely at its backoff cap, and
    /// without state-derived dedup an unreachable gateway re-emits the
    /// same UI event every attempt. A DIFFERENT message is new
    /// information and surfaces; ANY transition away from `Error` (here
    /// a successful empty startup reply — the same path a successful
    /// manual refresh takes) re-arms reporting for the next outage.
    #[tokio::test]
    async fn report_query_error_dedupes_identical_consecutive_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let mail_mgr = Arc::new(Mutex::new(MailManager::load(
            Some(&crate::device_dataset_file::test_cipher()),
            &tmp.path().join("mail"),
            [0u8; 16],
        )));
        let (fetch_tx, _fetch_rx) = mpsc::channel::<FetchRequest>(8);
        let sync = make_test_mail_sync(fetch_tx, mail_mgr);

        // First failure surfaces.
        assert!(sync.report_query_error("no gateway".to_string()));
        // Identical repeat (the retry loop) is suppressed.
        assert!(!sync.report_query_error("no gateway".to_string()));
        // A different message is new information — surfaces.
        assert!(sync.report_query_error("timed out".to_string()));
        assert!(!sync.report_query_error("timed out".to_string()));

        // Success clears Error → reporting re-arms for the next outage.
        sync.clone().handle_startup_query_reply(Some(b"")).await;
        assert!(matches!(
            &*sync.state.lock().unwrap(),
            SyncState::Idle { .. }
        ));
        assert!(sync.report_query_error("no gateway".to_string()));
    }

    /// A successful "no mail yet" reply (empty payload) following a prior
    /// failed walk MUST clear the Error state back to Idle — otherwise the
    /// UI stays stuck on the warning icon even though the gateway is now
    /// reachable and reports no mail.
    #[tokio::test]
    async fn empty_startup_reply_clears_prior_error_state() {
        let tmp = tempfile::tempdir().unwrap();
        let mail_mgr = Arc::new(Mutex::new(MailManager::load(
            Some(&crate::device_dataset_file::test_cipher()),
            &tmp.path().join("mail"),
            [0u8; 16],
        )));
        let (fetch_tx, _fetch_rx) = mpsc::channel::<FetchRequest>(8);
        let sync = make_test_mail_sync(fetch_tx, mail_mgr);

        // Force the walker into Error directly (no active Walking →
        // finish_walk_error still installs Error and emits status).
        sync.clone()
            .finish_walk_error("simulated previous failure".to_string());
        assert!(matches!(
            &*sync.state.lock().unwrap(),
            SyncState::Error { .. }
        ));

        // Empty reply = gateway explicitly says "no mail yet".
        sync.clone().handle_startup_query_reply(Some(b"")).await;

        let guard = sync.state.lock().unwrap();
        assert!(
            matches!(&*guard, SyncState::Idle { .. }),
            "expected Idle after empty reply, got {:?}",
            *guard
        );
    }

    /// Walker iterates pages and within-page entries in REVERSE wire order
    /// so the inbox's newest-first invariant (insert at index 0) preserves
    /// the gateway's intended ordering. Failure mode without the reverse:
    /// the oldest entry ends up at the top of the inbox.
    #[tokio::test]
    async fn walk_preserves_newest_first_wire_order() {
        let tmp = tempfile::tempdir().unwrap();
        let mail_mgr = Arc::new(Mutex::new(MailManager::load(
            Some(&crate::device_dataset_file::test_cipher()),
            &tmp.path().join("mail"),
            [0u8; 16],
        )));
        let (fetch_tx, fetch_rx) = mpsc::channel(32);

        // Wire: page entries[0] is newest. Build [newest, mid, oldest].
        let entries: Vec<MessageEntry> = (0..3)
            .rev()
            .map(|i| MessageEntry {
                message_cid: [i as u8; 32],
                message_id: [i as u8; 16],
                sender_address: [0; 16],
                timestamp: 1_700_000_000 + i as u64,
                subject_snippet: format!("entry {i}"),
                read: false,
            })
            .collect();
        // entries now = [i=2 (newest), i=1, i=0 (oldest)]

        let page = MailPage {
            version: MAILBOX_VERSION,
            entries,
        };
        let page_bytes = page.to_bytes().unwrap();
        let page_cid: [u8; 32] = *blake3::hash(&page_bytes).as_bytes();
        let folder = MailFolder {
            version: MAILBOX_VERSION,
            message_count: 3,
            unread_count: 3,
            page_cids: vec![page_cid],
        };
        let folder_bytes = folder.to_bytes().unwrap();
        let folder_cid: [u8; 32] = *blake3::hash(&folder_bytes).as_bytes();
        let root = MailRoot::new_empty([0; 16], 1_700_000_000).with_folder(
            FolderKind::Inbox,
            folder_cid,
            1_700_000_001,
        );
        let root_bytes = root
            .to_bytes()
            .expect("test fixture root encodes at MAILBOX_VERSION");
        let root_cid: [u8; 32] = *blake3::hash(&root_bytes).as_bytes();

        let mut stub = StubFetcher::new();
        stub.insert(root_cid, root_bytes);
        stub.insert(folder_cid, folder_bytes);
        stub.insert(page_cid, page_bytes);
        tokio::spawn(stub.run(fetch_rx));

        let sync = make_test_mail_sync(fetch_tx, mail_mgr.clone());
        sync.clone().handle_root_push(&root_cid).await;
        wait_for_terminal_state(&sync, std::time::Duration::from_secs(2)).await;

        let inbox = mail_mgr.lock().unwrap().list_folder("inbox", 0, 100);
        let ids: Vec<String> = inbox.iter().map(|e| e.message_id.clone()).collect();
        // Display order should match wire order: newest (i=2) at index 0.
        assert_eq!(
            ids,
            vec![
                hex::encode([2u8; 16]),
                hex::encode([1u8; 16]),
                hex::encode([0u8; 16])
            ],
            "inbox should preserve newest-first wire order"
        );
    }
}
