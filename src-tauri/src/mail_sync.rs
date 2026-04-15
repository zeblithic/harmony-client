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

/// Request to fetch a CAS blob from the gateway. Mirrors the existing
/// FetchRequest pattern used by event_loop's fetch_rx channel.
pub struct FetchRequest {
    pub cid_hex: String,
    pub reply: oneshot::Sender<Result<Vec<u8>, String>>,
}

/// In-flight body-fetch deduplication: multiple concurrent callers
/// asking for the same CID share one outgoing fetch via `watch`.
type InFlightMap = Arc<
    Mutex<HashMap<[u8; CID_LEN], tokio::sync::watch::Receiver<Option<Result<Vec<u8>, String>>>>>,
>;

pub struct MailSync<R: Runtime = tauri::Wry> {
    state: Arc<Mutex<SyncState>>,
    fetch_tx: mpsc::Sender<FetchRequest>,
    mail_mgr: Arc<Mutex<MailManager>>,
    own_addr_hex: String,
    app: AppHandle<R>,
    in_flight_bodies: InFlightMap,
}

impl<R: Runtime> MailSync<R> {
    pub fn new(
        fetch_tx: mpsc::Sender<FetchRequest>,
        mail_mgr: Arc<Mutex<MailManager>>,
        own_addr_hex: String,
        app: AppHandle<R>,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(SyncState::Idle {
                last_walked_root: None,
            })),
            fetch_tx,
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

    /// Manual refresh trigger from UI. Re-queries the gateway for the
    /// current root and walks if it has changed. Implementation in C12.
    pub async fn refresh_now(self: Arc<Self>) {
        tracing::info!("manual refresh requested — implementation in C12");
        // TODO(C12): wire actual query path.
    }

    /// Lazy body fetch. Called from the fetch_mail_body Tauri command.
    /// Implementation in C10.
    pub async fn fetch_body(self: Arc<Self>, _cid: [u8; CID_LEN]) -> Result<Vec<u8>, String> {
        Err("fetch_body not yet implemented (Task C10)".to_string())
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

    /// Stub for Task C8. Returns Some(summary) if any pages/entries skipped.
    async fn walk_pages(&self, _page_cids: &[[u8; CID_LEN]]) -> Option<String> {
        // Implementation in C8.
        None
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
    use harmony_mailbox::mailbox::{FolderKind, MailRoot};

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
        Arc::new(MailSync::new(
            fetch_tx,
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

        // Allow walker to run.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

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
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

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
}
