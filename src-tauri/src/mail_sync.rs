//! Mailbox sync: walks the gateway-published Merkle tree and registers
//! header-only entries with MailManager. Lazy body fetch on demand.
//!
//! See docs/specs/2026-04-14-client-mail-receive-design.md.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::Serialize;
use tauri::{AppHandle, Emitter};
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

pub struct MailSync {
    state: Arc<Mutex<SyncState>>,
    fetch_tx: mpsc::Sender<FetchRequest>,
    mail_mgr: Arc<Mutex<MailManager>>,
    own_addr_hex: String,
    app: AppHandle,
    in_flight_bodies: InFlightMap,
}

impl MailSync {
    pub fn new(
        fetch_tx: mpsc::Sender<FetchRequest>,
        mail_mgr: Arc<Mutex<MailManager>>,
        own_addr_hex: String,
        app: AppHandle,
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
        // Full implementation in C7/C9.
        tracing::debug!(?root, "start_or_queue_walk called (stub — C7/C9 will implement)");
    }

    fn emit_status(&self, event: SyncStatusEvent) {
        if let Err(e) = self.app.emit("mail-sync-status", &event) {
            tracing::warn!(error = %e, "failed to emit mail-sync-status");
        }
    }
}
