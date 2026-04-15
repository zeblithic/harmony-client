//! Mailbox sync: walks the gateway-published Merkle tree and registers
//! header-only entries with MailManager. Lazy body fetch on demand.
//!
//! See docs/specs/2026-04-14-client-mail-receive-design.md.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Runtime};
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
enum SyncState {
    Idle {
        last_walked_root: Option<[u8; CID_LEN]>,
    },
    Walking {
        root: [u8; CID_LEN],
        started_at: Instant,
        pending_root: Option<[u8; CID_LEN]>,
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

pub struct MailSync<R: Runtime = tauri::Wry> {
    state: Arc<Mutex<SyncState>>,
    fetch_tx: mpsc::Sender<FetchRequest>,
    refresh_tx: mpsc::Sender<RefreshRequest>,
    mail_mgr: Arc<Mutex<MailManager>>,
    own_addr_hex: String,
    app: AppHandle<R>,
    in_flight_bodies: InFlightMap,
}

impl<R: Runtime> MailSync<R> {
    pub fn new(
        fetch_tx: mpsc::Sender<FetchRequest>,
        refresh_tx: mpsc::Sender<RefreshRequest>,
        mail_mgr: Arc<Mutex<MailManager>>,
        own_addr_hex: String,
        app: AppHandle<R>,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(SyncState::Idle {
                last_walked_root: None,
            })),
            fetch_tx,
            refresh_tx,
            mail_mgr,
            own_addr_hex,
            app,
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

    /// Handle a reply from the cold-start Zenoh `get` query.
    /// Empty payload means the gateway has no mail for this address yet.
    pub async fn handle_startup_query_reply(self: Arc<Self>, payload: Option<&[u8]>) {
        match payload {
            None | Some(b"") => {
                tracing::info!("startup query: no mail for this address yet");
            }
            Some(bytes) => {
                if let Ok(root) = <[u8; CID_LEN]>::try_from(bytes) {
                    self.start_or_queue_walk(root).await;
                } else {
                    tracing::warn!(
                        len = bytes.len(),
                        "ignoring malformed startup query reply"
                    );
                }
            }
        }
    }

    /// Manual refresh trigger from UI. Issues a fresh Zenoh get for the
    /// current root and walks if the gateway has one.
    pub async fn refresh_now(self: Arc<Self>) {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self.refresh_tx.send(reply_tx).await.is_err() {
            tracing::warn!("refresh channel closed; cannot refresh");
            return;
        }
        match tokio::time::timeout(std::time::Duration::from_secs(10), reply_rx).await {
            Ok(Ok(Ok(payload))) => {
                self.handle_startup_query_reply(payload.as_deref()).await;
            }
            Ok(Ok(Err(e))) => tracing::warn!(error = %e, "refresh root query failed"),
            Ok(Err(_)) => tracing::warn!("refresh reply channel dropped"),
            Err(_) => tracing::warn!("refresh root query timed out (10s)"),
        }
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
        let Role::Primary(tx) = role else { unreachable!() };

        // RAII guard: removes the in-flight entry when this function returns
        // OR is dropped mid-fetch (cancellation). Ensures a cancelled primary
        // never strands its CID in the map indefinitely.
        struct InFlightGuard<'a> {
            map: &'a InFlightMap,
            cid: [u8; CID_LEN],
        }
        impl Drop for InFlightGuard<'_> {
            fn drop(&mut self) {
                if let Ok(mut m) = self.map.lock() {
                    m.remove(&self.cid);
                }
            }
        }
        let _guard = InFlightGuard {
            map: &self.in_flight_bodies,
            cid,
        };

        // Perform the actual fetch + verification + persistence.
        let result: Result<Vec<u8>, String> = async {
            let bytes = self.fetch_cas(cid).await?;

            // BLAKE3 verify: bytes must hash to the claimed CID.
            let computed = blake3::hash(&bytes);
            if computed.as_bytes() != &cid {
                return Err(format!(
                    "hash mismatch: claimed {}, computed {}",
                    hex::encode(cid),
                    hex::encode(computed.as_bytes())
                ));
            }

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
        {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            match &mut *state {
                SyncState::Walking { pending_root, .. } => {
                    *pending_root = Some(root);
                    return;
                }
                _ => {
                    *state = SyncState::Walking {
                        root,
                        started_at: Instant::now(),
                        pending_root: None,
                    };
                }
            }
        }

        let me = Arc::clone(&self);
        tokio::spawn(async move {
            me.run_walk_pass(root).await;
        });
    }

    /// Fetch a CAS blob via the event_loop's fetch channel. 30-second budget
    /// covers both the outbound send (which can block on channel backpressure)
    /// and the reply — a stalled fetcher must never strand the walker in
    /// Walking state indefinitely.
    async fn fetch_cas(&self, cid: [u8; CID_LEN]) -> Result<Vec<u8>, String> {
        let cid_hex = hex::encode(cid);
        let work = async {
            let (reply_tx, reply_rx) = oneshot::channel();
            self.fetch_tx
                .send(FetchRequest {
                    cid_hex: cid_hex.clone(),
                    reply: reply_tx,
                })
                .await
                .map_err(|_| "fetch channel closed".to_string())?;
            reply_rx
                .await
                .map_err(|_| "fetch reply channel dropped".to_string())?
        };
        match tokio::time::timeout(std::time::Duration::from_secs(30), work).await {
            Ok(result) => result,
            Err(_) => Err(format!("fetch timeout for {cid_hex}")),
        }
    }

    async fn run_walk_pass(self: Arc<Self>, root: [u8; CID_LEN]) {
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

        // Step 3: walk pages (Task C8 implements; stub returns None).
        let skip_summary = self.walk_pages(&folder.page_cids).await;

        // Step 4: finalize state.
        self.finish_walk(root, skip_summary);
    }

    /// Walk the page CID list, fetching up to `MAX_CONCURRENT_PAGES` pages in
    /// parallel, parsing each, and registering every entry as header-only.
    ///
    /// Hybrid error policy (Q7): page fetch and parse failures are logged
    /// and that page is skipped — other pages continue. Returns
    /// `Some(summary)` when any page or entry was skipped so the caller can
    /// finalize in Error state.
    async fn walk_pages(&self, page_cids: &[[u8; CID_LEN]]) -> Option<String> {
        use futures::future::join_all;
        use harmony_mailbox::mailbox::MailPage;
        use tokio::sync::Semaphore;

        // Bounds peak in-flight FetchRequest count (memory + channel pressure),
        // NOT effective parallelism — the downstream fetcher in event_loop
        // controls how many CAS fetches actually run concurrently. If that
        // fetcher is serial, the walk is serial regardless of this bound.
        const MAX_CONCURRENT_PAGES: usize = 8;
        let sem = Arc::new(Semaphore::new(MAX_CONCURRENT_PAGES));

        // Fetch all pages in parallel (bounded by semaphore).
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

        // Parse each page, register entries, collecting skips.
        let mut skipped_pages: Vec<String> = Vec::new();
        let mut skipped_entries: usize = 0;
        let mut new_entry_cids: Vec<String> = Vec::new();

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
            let page = match MailPage::from_bytes(&bytes) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(
                        page_cid = %page_cid_hex,
                        error = %e,
                        "page parse failed; skipping"
                    );
                    skipped_pages.push(page_cid_hex);
                    continue;
                }
            };
            for entry in page.entries {
                // Inner scope: std::sync::Mutex guard does NOT cross .await.
                // register_header_only is synchronous and the guard drops at
                // the end of this iteration before the next loop turn.
                let mut mgr = self.mail_mgr.lock().unwrap_or_else(|p| p.into_inner());
                match mgr.register_header_only(entry) {
                    Ok(crate::mail::RegisterOutcome::Inserted { cid }) => {
                        new_entry_cids.push(cid);
                    }
                    Ok(crate::mail::RegisterOutcome::Duplicate) => {}
                    Err(e) => {
                        tracing::warn!(error = %e, "register_header_only failed; skipping entry");
                        skipped_entries += 1;
                    }
                }
            }
        }

        // Emit per-new-entry Tauri events so the frontend updates its inbox.
        // Matches Phase 0 receive_message pattern: one event per newly-inserted
        // entry, with the EntryRecord as payload. Emitted AFTER all
        // registrations so we don't surface entries that might turn out to
        // fail later in the same page.
        //
        // Delta-only stream: the frontend must seed its inbox view from a
        // `list_folder`-backed command on mount. If the walker crashes after
        // save_index but before emit, the entries are persisted but no event
        // fires on a subsequent re-walk (Duplicate → silent). The UI recovers
        // from persisted state on next list_folder, not from replayed events.
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
            if let Err(e) = self.app.emit("mail-received", &entry) {
                tracing::warn!(error = %e, "failed to emit mail-received");
            }
        }

        if skipped_pages.is_empty() && skipped_entries == 0 {
            None
        } else {
            Some(format!(
                "skipped {} page(s), {} entr(y/ies)",
                skipped_pages.len(),
                skipped_entries
            ))
        }
    }

    fn finish_walk_error(self: Arc<Self>, error: String) {
        // Single locked scope: read prior last_walked_root and install the
        // new Error state atomically. (Splitting read and write across two
        // lock acquisitions would be correct but pointlessly gives another
        // observer a chance to see intermediate state.)
        //
        // Known limitation: when prior state is Walking, we lose the pre-walk
        // last_walked_root (not carried in the Walking variant). Acceptable
        // for Phase 2 — tracked in ZEB-114 follow-ups; surfaces only once
        // last_walked_root becomes load-bearing (C12 refresh_now / UI).
        {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            let last_walked = match &*state {
                SyncState::Walking { .. } => None,
                SyncState::Idle { last_walked_root }
                | SyncState::Error {
                    last_walked_root, ..
                } => *last_walked_root,
            };
            *state = SyncState::Error {
                last_error: error.clone(),
                last_walked_root: last_walked,
            };
        }
        self.emit_status(SyncStatusEvent {
            state: "error",
            error: Some(error),
        });
    }

    fn finish_walk(self: Arc<Self>, root: [u8; CID_LEN], skip_summary: Option<String>) {
        let pending = {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            let pending = if let SyncState::Walking { pending_root, .. } = &*state {
                *pending_root
            } else {
                None
            };
            *state = match skip_summary.clone() {
                None => SyncState::Idle {
                    last_walked_root: Some(root),
                },
                Some(summary) => SyncState::Error {
                    last_error: summary,
                    last_walked_root: Some(root),
                },
            };
            pending
        };

        // Emit terminal event based on the new state.
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

        // Re-walk if pending root was queued.
        if let Some(next_root) = pending {
            let me = Arc::clone(&self);
            tokio::spawn(async move {
                me.start_or_queue_walk(next_root).await;
            });
        }
    }

    fn emit_status(&self, event: SyncStatusEvent) {
        if let Err(e) = self.app.emit("mail-sync-status", &event) {
            tracing::warn!(error = %e, "failed to emit mail-sync-status");
        }
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

    /// Build a test MailSync with a mock Tauri AppHandle.
    fn make_test_mail_sync(
        fetch_tx: mpsc::Sender<FetchRequest>,
        mail_mgr: Arc<Mutex<MailManager>>,
    ) -> Arc<MailSync<tauri::test::MockRuntime>> {
        let app = tauri::test::mock_app();
        // Throwaway refresh channel — tests don't exercise refresh_now directly.
        let (refresh_tx, _refresh_rx) = mpsc::channel(1);
        Arc::new(MailSync::new(
            fetch_tx,
            refresh_tx,
            mail_mgr,
            "00112233445566778899aabbccddeeff".to_string(),
            app.handle().clone(),
        ))
    }

    #[tokio::test]
    async fn walk_aborts_on_root_fetch_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let mail_mgr = Arc::new(Mutex::new(MailManager::load(
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
            &tmp.path().join("mail"),
            [0u8; 16],
        )));
        let (fetch_tx, fetch_rx) = mpsc::channel(16);

        // Construct a valid MailRoot pointing at a folder CID we won't serve.
        let folder_cid = [0xF0; CID_LEN];
        let root = MailRoot::new_empty([0u8; 16], 1700000000)
            .with_folder(FolderKind::Inbox, folder_cid, 1700000001);
        let root_bytes = root.to_bytes();
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
    async fn wait_for_terminal_state<R: tauri::Runtime>(
        sync: &MailSync<R>,
        deadline: std::time::Duration,
    ) {
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
            next_page: None,
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

        let root = MailRoot::new_empty([0u8; 16], 1_700_000_000)
            .with_folder(FolderKind::Inbox, folder_cid, 1_700_000_001);
        let root_bytes = root.to_bytes();
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
            next_page: None,
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

        let root = MailRoot::new_empty([0u8; 16], 1_700_000_000)
            .with_folder(FolderKind::Inbox, folder_cid, 1_700_000_001);
        let root_bytes = root.to_bytes();
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
            &tmp.path().join("mail"),
            [0u8; 16],
        )));
        let (fetch_tx, fetch_rx) = mpsc::channel(32);

        // Build TWO different roots, each with one entry.
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
                next_page: None,
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

            let root = MailRoot::new_empty([0; 16], 1_700_000_000)
                .with_folder(FolderKind::Inbox, folder_cid, 1_700_000_001);
            let root_bytes = root.to_bytes();
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

    /// Regression test for the cancellation-leak hazard (C10 review I-2):
    /// if the primary's future is dropped mid-fetch, the in-flight map entry
    /// MUST be cleared so subsequent callers can start a fresh fetch rather
    /// than be stuck subscribing to a dead channel forever.
    #[tokio::test]
    async fn fetch_body_cancellation_clears_in_flight_entry() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let tmp = tempfile::tempdir().unwrap();
        let mail_mgr = Arc::new(Mutex::new(MailManager::load(
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
}
