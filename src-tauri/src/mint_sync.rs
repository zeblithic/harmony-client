//! Mint Phase 2 sync engine. Mirrors owner_state_sync's shape.

use crate::mint_sync_types::MintSyncState;
use crate::mint_sync_types::{AccountRow, MintSnapshot, MintSyncError, SettingRow, TransactionRow};
use crate::owner_state_crypto::FleetKeySet;
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex as TokioMutex, Notify};

/// Read the full ledger state into a [`MintSnapshot`].
///
/// Runs three SELECTs inside one transaction so the snapshot is a
/// consistent read across tables. Includes tombstoned rows so peers
/// see the tombstone and converge on delete.
pub(crate) fn snapshot_current_db(conn: &mut Connection) -> Result<MintSnapshot, MintSyncError> {
    let tx = conn.transaction()?;
    let accounts: Vec<AccountRow> = tx
        .prepare("SELECT id, name, created_at, updated_at FROM accounts ORDER BY id")?
        .query_map([], |r| {
            Ok(AccountRow {
                id: r.get(0)?,
                name: r.get(1)?,
                created_at: r.get(2)?,
                updated_at: r.get(3)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    let transactions: Vec<TransactionRow> = tx
        .prepare(
            "SELECT id, transaction_date, amount, currency, account_id, description, \
                    metadata, created_at, updated_at, deleted_at \
             FROM transactions ORDER BY id",
        )?
        .query_map([], |r| {
            Ok(TransactionRow {
                id: r.get(0)?,
                transaction_date: r.get(1)?,
                amount: r.get(2)?,
                currency: r.get(3)?,
                account_id: r.get(4)?,
                description: r.get(5)?,
                metadata: r.get(6)?,
                created_at: r.get(7)?,
                updated_at: r.get(8)?,
                deleted_at: r.get(9)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    let settings: Vec<SettingRow> = tx
        .prepare("SELECT key, value, updated_at FROM settings ORDER BY key")?
        .query_map([], |r| {
            Ok(SettingRow {
                key: r.get(0)?,
                value: r.get(1)?,
                updated_at: r.get(2)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    tx.commit()?;
    Ok(MintSnapshot {
        schema_version: crate::mint_sync_types::MINT_SCHEMA_VERSION,
        accounts,
        transactions,
        settings,
        account_deletion_floor: HashMap::new(), // populated by publish path from sync_state
        captured_at: chrono::Utc::now().to_rfc3339(),
    })
}

/// Apply a remote snapshot to the local DB. Runs in a single SQLite
/// transaction — either all rows merge or none do.
///
/// **Caller contract:** the caller must verify
/// `remote.schema_version <= LOCAL_MAX_SCHEMA_VERSION` BEFORE calling this
/// function. The subscriber (Task 9) does this check; direct callers
/// (tests, future code paths) must replicate it. This function does NOT
/// re-check schema_version — silent application of an unknown version
/// risks silent data corruption.
///
/// `account_deletion_floor` is the per-device map of hard-deleted
/// account IDs → deletion timestamp; peer rows older than the floor
/// are dropped to prevent zombie-resurrect.
///
/// Returns a `HashMap<String, String>` of floor entries from the remote
/// snapshot that the caller must merge into local `MintSyncState.account_deletion_floor`.
/// Returning them (rather than merging in-place) keeps this function
/// SQLite-only with no reference to async state.
pub(crate) fn apply_remote_snapshot(
    conn: &mut Connection,
    remote: &MintSnapshot,
    account_deletion_floor: &HashMap<String, String>,
) -> Result<HashMap<String, String>, MintSyncError> {
    let tx = conn.transaction()?;
    // Collect account IDs suppressed by the deletion floor so we can skip
    // transactions that reference them (MAJOR 7: prevents FK violations /
    // orphan rows when a remote account was blocked but its transactions
    // were not).
    let mut suppressed_account_ids = std::collections::HashSet::new();
    for r in &remote.accounts {
        if upsert_account_lww(&tx, r, account_deletion_floor)? == UpsertOutcome::Suppressed {
            suppressed_account_ids.insert(r.id.clone());
        }
    }
    for r in &remote.transactions {
        if suppressed_account_ids.contains(&r.account_id) {
            // Owning account was suppressed by deletion floor; skip to avoid
            // FK violations and orphan rows.
            continue;
        }
        upsert_transaction_lww(&tx, r)?;
    }
    for r in &remote.settings {
        upsert_setting_lww(&tx, r)?;
    }

    // Apply remote deletion floor: hard-delete local accounts that the remote
    // device has deleted, and collect floor entries to merge into local state.
    // Note: this does NOT go through delete_account (which requires a reassign
    // target). The user already deleted the account on the other device; the
    // transactions are stale and the cascade is expected. Explicit DELETE is
    // required because the schema has no ON DELETE CASCADE for transactions.
    let mut floor_to_merge: HashMap<String, String> = HashMap::new();
    for (id, remote_ts) in &remote.account_deletion_floor {
        // Check whether the local account exists and whether it's stale.
        let local_updated_at: Option<String> = tx
            .query_row(
                "SELECT updated_at FROM accounts WHERE id = ?",
                [id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(local_ts) = local_updated_at {
            if &local_ts <= remote_ts {
                // Account is stale w.r.t. the remote deletion — hard-delete it
                // and its orphan transactions in the same SQLite transaction.
                tx.execute("DELETE FROM transactions WHERE account_id = ?", [id])?;
                tx.execute("DELETE FROM accounts WHERE id = ?", [id])?;
            }
            // If local_ts > remote_ts the account was re-created after the
            // remote delete; leave it in place. Still merge the floor entry
            // so this device won't silently resurrect the old version later.
        }
        // Collect into the to-merge map. Each id in remote.account_deletion_floor
        // is unique so entry() never collides within this loop — or_insert_with
        // always inserts.
        floor_to_merge
            .entry(id.clone())
            .or_insert_with(|| remote_ts.clone());
    }

    tx.commit()?;
    Ok(floor_to_merge)
}

/// Outcome of a single account upsert — used to propagate suppression
/// information back to `apply_remote_snapshot` so dependent transactions
/// can be skipped.
#[derive(PartialEq, Eq)]
enum UpsertOutcome {
    Applied,
    Suppressed,
}

fn upsert_account_lww(
    tx: &rusqlite::Transaction,
    r: &AccountRow,
    floor: &HashMap<String, String>,
) -> Result<UpsertOutcome, MintSyncError> {
    if let Some(floor_ts) = floor.get(&r.id) {
        if &r.updated_at <= floor_ts {
            return Ok(UpsertOutcome::Suppressed); // peer's row is stale w.r.t. our delete
        }
    }
    let local_updated_at: Option<String> = tx
        .query_row(
            "SELECT updated_at FROM accounts WHERE id = ?",
            [&r.id],
            |row| row.get(0),
        )
        .optional()?;
    match local_updated_at {
        None => {
            tx.execute(
                "INSERT INTO accounts (id, name, created_at, updated_at) VALUES (?1, ?2, ?3, ?4)",
                params![r.id, r.name, r.created_at, r.updated_at],
            )?;
        }
        Some(local) if r.updated_at > local => {
            tx.execute(
                "UPDATE accounts SET name = ?1, updated_at = ?2 WHERE id = ?3",
                params![r.name, r.updated_at, r.id],
            )?;
        }
        Some(_) => {}
    }
    Ok(UpsertOutcome::Applied)
}

fn upsert_transaction_lww(
    tx: &rusqlite::Transaction,
    r: &TransactionRow,
) -> Result<(), MintSyncError> {
    let local_updated_at: Option<String> = tx
        .query_row(
            "SELECT updated_at FROM transactions WHERE id = ?",
            [&r.id],
            |row| row.get(0),
        )
        .optional()?;
    match local_updated_at {
        None => {
            tx.execute(
                "INSERT INTO transactions \
                 (id, transaction_date, amount, currency, account_id, description, \
                  metadata, created_at, updated_at, deleted_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    r.id,
                    r.transaction_date,
                    r.amount,
                    r.currency,
                    r.account_id,
                    r.description,
                    r.metadata,
                    r.created_at,
                    r.updated_at,
                    r.deleted_at,
                ],
            )?;
        }
        Some(local) if r.updated_at > local => {
            tx.execute(
                "UPDATE transactions SET \
                 transaction_date = ?1, amount = ?2, currency = ?3, account_id = ?4, \
                 description = ?5, metadata = ?6, updated_at = ?7, deleted_at = ?8 \
                 WHERE id = ?9",
                params![
                    r.transaction_date,
                    r.amount,
                    r.currency,
                    r.account_id,
                    r.description,
                    r.metadata,
                    r.updated_at,
                    r.deleted_at,
                    r.id,
                ],
            )?;
        }
        Some(_) => {}
    }
    Ok(())
}

fn upsert_setting_lww(tx: &rusqlite::Transaction, r: &SettingRow) -> Result<(), MintSyncError> {
    tx.execute(
        "INSERT INTO settings (key, value, updated_at) VALUES (?1, ?2, ?3) \
         ON CONFLICT(key) DO UPDATE SET \
           value = excluded.value, updated_at = excluded.updated_at \
         WHERE excluded.updated_at > settings.updated_at",
        params![r.key, r.value, r.updated_at],
    )?;
    Ok(())
}

pub const DEFAULT_DEBOUNCE_MS: u64 = 250;
pub const DEFAULT_BOOT_FLUSH_DELAY_MS: u64 = 500;

/// Shared state needed by both the internal task and the test-only subscriber
/// path. Cloned at construction so `handle_incoming_envelope_for_test` can
/// fetch + merge without going through the internal task's event loop.
#[derive(Clone)]
struct EngineShared {
    mint_db: Arc<std::sync::Mutex<rusqlite::Connection>>,
    content_store: Arc<dyn crate::content_store::ContentStore>,
    sync_state: Arc<TokioMutex<MintSyncState>>,
    /// Path to the persisted sync-state file. Used by `mint_delete_account`
    /// (CRITICAL 2) to flush the deletion floor to disk after an in-memory
    /// update. `None` in test constructors — tests don't persist to disk.
    sync_state_path: Option<std::path::PathBuf>,
    /// Production passes `Some(sink)` so successful subscriber merges
    /// can fire a `"mint-changed"` event (ZEB-445: via the mode-agnostic
    /// sink). Test constructors pass `None` — the emit is a best-effort UI
    /// notification, not a correctness requirement, so skipping it in
    /// tests is safe.
    app_handle: Option<std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>>,
}

/// Mint Phase 2 sync engine. Mirrors owner_state_sync's shape.
pub struct MintSyncEngine {
    dirty: Arc<Notify>,
    flush_now: mpsc::Sender<()>,
    shutdown: mpsc::Sender<()>,
    shared: EngineShared,
}

/// Handle wrapping one or two task JoinHandles (internal task + optional
/// subscriber task for the real Zenoh engine). Implements `Future` by
/// polling the first handle; the subscriber task is fire-and-forget.
pub struct MintSyncEngineHandle {
    primary: tokio::task::JoinHandle<()>,
    /// Optional secondary task (real-engine subscriber task). Dropped
    /// when the MintSyncEngineHandle is dropped; the subscriber task
    /// exits when its shutdown channel closes.
    #[allow(dead_code)]
    secondary: Option<tokio::task::JoinHandle<()>>,
}

impl std::future::Future for MintSyncEngineHandle {
    type Output = Result<(), tokio::task::JoinError>;
    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        std::pin::Pin::new(&mut self.primary).poll(cx)
    }
}

/// ZEB-702 T2: `MintSyncEngine` is a distinct engine type (not the generic
/// `FleetSyncEngine<S>`), so it carries its own delegation-only
/// `RepublishDirty` impl. `republish_dirty` is exactly `notify_dirty`, so the
/// transport-up-edge re-offer reuses the existing debounced publish path with
/// no behavior change.
impl crate::fleet_sync::RepublishDirty for MintSyncEngine {
    fn republish_dirty(&self) {
        self.notify_dirty();
    }
}

impl MintSyncEngine {
    /// Test constructor that schedules a one-shot boot-hook flush.
    /// The "real" `new` (Task 11) will use this same boot-hook pattern with
    /// `DEFAULT_BOOT_FLUSH_DELAY_MS`. Splitting it out lets the Task 7/8/9
    /// tests use `new_for_test` without the boot-hook firing on them.
    pub async fn new_for_test_with_boot_delay(
        mint_db: Arc<std::sync::Mutex<rusqlite::Connection>>,
        content_store: Arc<dyn crate::content_store::ContentStore>,
        sync_state: Arc<TokioMutex<MintSyncState>>,
        boot_delay: std::time::Duration,
    ) -> (Self, MintSyncEngineHandle) {
        let (engine, handle) = Self::new_for_test(mint_db, content_store, sync_state).await;
        let flush_tx = engine.flush_now.clone();
        tokio::spawn(async move {
            tokio::time::sleep(boot_delay).await;
            // Send-error is benign: engine was shut down before the boot
            // hook fired, which is the expected outcome of a short-lived
            // test that exits before 500ms elapses.
            let _ = flush_tx.send(()).await;
        });
        (engine, handle)
    }
}

impl MintSyncEngine {
    /// Test constructor: no Zenoh, just an in-memory ContentStore.
    /// The real `new` (Task 11) takes a Zenoh session + identity key.
    pub async fn new_for_test(
        mint_db: Arc<std::sync::Mutex<rusqlite::Connection>>,
        content_store: Arc<dyn crate::content_store::ContentStore>,
        sync_state: Arc<TokioMutex<MintSyncState>>,
    ) -> (Self, MintSyncEngineHandle) {
        Self::new_for_test_with_debounce(
            mint_db,
            content_store,
            sync_state,
            std::time::Duration::from_millis(DEFAULT_DEBOUNCE_MS),
        )
        .await
    }

    pub async fn new_for_test_with_debounce(
        mint_db: Arc<std::sync::Mutex<rusqlite::Connection>>,
        content_store: Arc<dyn crate::content_store::ContentStore>,
        sync_state: Arc<TokioMutex<MintSyncState>>,
        debounce: std::time::Duration,
    ) -> (Self, MintSyncEngineHandle) {
        let dirty = Arc::new(Notify::new());
        // Flush channel capacity 4: enough headroom for a small burst of
        // flush requests without blocking the caller. Still NOT load-bearing
        // for ordering — flush_now() returns when the send completes, NOT
        // when publish_root_now finishes. A future refactor (likely Task 9 or
        // Task 11) should add an oneshot::Sender<Result<()>> in each flush
        // message so callers can await the actual publish result. Tracked as
        // a follow-up; tests currently work around this via sleep().
        let (flush_tx, flush_rx) = mpsc::channel::<()>(4);
        let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>(1);
        let shared = EngineShared {
            mint_db: mint_db.clone(),
            content_store: content_store.clone(),
            sync_state: sync_state.clone(),
            sync_state_path: None,
            app_handle: None,
        };
        let dirty_for_task = dirty.clone();
        let handle = tokio::spawn(internal_task(
            mint_db,
            content_store,
            sync_state,
            dirty_for_task,
            flush_rx,
            shutdown_rx,
            debounce,
        ));
        (
            Self {
                dirty,
                flush_now: flush_tx,
                shutdown: shutdown_tx,
                shared,
            },
            MintSyncEngineHandle {
                primary: handle,
                secondary: None,
            },
        )
    }

    /// Production constructor. Mirrors owner_state_sync::SyncEngine::new —
    /// channel-based: encryption happens inside the engine; Zenoh I/O is
    /// bridged externally by event_loop via `publisher_tx` / `subscriber_rx`.
    ///
    /// `publisher_tx`: bytes written here are forwarded by event_loop to Zenoh put.
    /// `subscriber_rx`: encrypted bytes received from Zenoh are fed here by event_loop.
    /// `content_store`: shared CAS used to put (publish side) and get (subscribe side)
    ///   encrypted snapshot blobs. In production this is RuntimeContentStore.
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        keys: FleetKeySet,
        device_id: String,
        mint_db: Arc<std::sync::Mutex<rusqlite::Connection>>,
        content_store: Arc<dyn crate::content_store::ContentStore>,
        sync_state: Arc<TokioMutex<MintSyncState>>,
        sync_state_path: std::path::PathBuf,
        publisher_tx: mpsc::Sender<Vec<u8>>,
        subscriber_rx: mpsc::Receiver<Vec<u8>>,
        debounce_ms: u64,
        app_handle: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>,
    ) -> (Self, MintSyncEngineHandle) {
        let dirty = Arc::new(Notify::new());
        let (flush_tx, flush_rx) = mpsc::channel::<()>(4);
        let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>(1);
        let shared = EngineShared {
            mint_db: mint_db.clone(),
            content_store: content_store.clone(),
            sync_state: sync_state.clone(),
            sync_state_path: Some(sync_state_path.clone()),
            app_handle: Some(app_handle),
        };
        let dirty_for_task = dirty.clone();
        let handle = tokio::spawn(internal_task_zenoh(
            shared.clone(),
            sync_state_path,
            keys,
            device_id,
            dirty_for_task,
            flush_rx,
            shutdown_rx,
            std::time::Duration::from_millis(debounce_ms),
            publisher_tx,
            subscriber_rx,
        ));
        (
            Self {
                dirty,
                flush_now: flush_tx,
                shutdown: shutdown_tx,
                shared,
            },
            MintSyncEngineHandle {
                primary: handle,
                secondary: None,
            },
        )
    }

    pub fn notify_dirty(&self) {
        self.dirty.notify_one();
    }

    /// Synchronously enqueue a flush request. Blocks until the internal task
    /// accepts the message (capacity 4 — see `new_for_test`).
    ///
    /// Returns `Err(MintSyncError::Other(...))` if the channel is closed
    /// (i.e. the engine has been shut down). Callers must treat that as a
    /// "no-op, engine is gone" outcome rather than a retryable failure.
    pub async fn flush_now(&self) -> Result<(), MintSyncError> {
        self.flush_now
            .send(())
            .await
            .map_err(|_| MintSyncError::Other("flush channel closed".into()))?;
        Ok(())
    }

    /// Signal the internal task to exit. Idempotent: if the engine has
    /// already shut down (channel closed), this is silently treated as
    /// success. Callers awaiting the engine handle should observe
    /// completion regardless.
    pub async fn shutdown(&self) -> Result<(), MintSyncError> {
        // `let _`: channel-closed is the "already shut down" case, which is
        // a successful no-op from the caller's perspective.
        let _ = self.shutdown.send(()).await;
        Ok(())
    }
}

impl MintSyncEngine {
    /// Test entry point that simulates a Zenoh-delivered envelope.
    /// Used by Task 8/9 tests; the real Zenoh subscriber path goes directly
    /// through `handle_incoming_decoded` after decryption.
    pub async fn handle_incoming_envelope_for_test(
        &self,
        root_cid: crate::owner_state_types::ContentId,
    ) -> Result<(), MintSyncError> {
        self.shared.handle_incoming(root_cid).await
    }

    /// Returns the shared sync-state handle so callers (e.g. delete_account
    /// IPC) can lock the account deletion floor without going through the
    /// engine's event loop.
    pub fn sync_state_handle(&self) -> Arc<TokioMutex<MintSyncState>> {
        self.shared.sync_state.clone()
    }

    /// Returns the path to the persisted sync-state file, or `None` in
    /// test engines (which don't persist to disk).
    pub fn sync_state_path(&self) -> Option<&std::path::Path> {
        self.shared.sync_state_path.as_deref()
    }
}

impl EngineShared {
    /// Inner merge path: takes a pre-decoded snapshot, runs schema check
    /// and apply_remote_snapshot inside spawn_blocking. Used by both the
    /// real Zenoh subscriber (Task 11) and the test entry point.
    async fn handle_incoming_decoded(
        &self,
        remote: crate::mint_sync_types::MintSnapshot,
    ) -> Result<(), MintSyncError> {
        if remote.schema_version > crate::mint_sync_types::LOCAL_MAX_SCHEMA_VERSION {
            return Err(MintSyncError::SchemaTooNew {
                remote: remote.schema_version,
                local_max: crate::mint_sync_types::LOCAL_MAX_SCHEMA_VERSION,
            });
        }
        let mint_db = self.mint_db.clone();
        let sync_state = self.sync_state.clone();
        let floor_to_merge = tokio::task::spawn_blocking(
            move || -> Result<HashMap<String, String>, MintSyncError> {
                let mut conn = mint_db.lock().expect("mint_db lock poisoned");
                let st = sync_state.blocking_lock();
                apply_remote_snapshot(&mut conn, &remote, &st.account_deletion_floor)
            },
        )
        .await
        .map_err(|e| MintSyncError::Other(format!("spawn_blocking: {e}")))??;
        // Merge the remote deletion floor entries into local sync_state so this
        // device also blocks future zombie-resurrects from other stale peers.
        {
            let mut st = self.sync_state.lock().await;
            for (id, remote_ts) in floor_to_merge {
                let entry = st
                    .account_deletion_floor
                    .entry(id)
                    .or_insert_with(|| remote_ts.clone());
                if remote_ts > *entry {
                    *entry = remote_ts;
                }
            }
        }
        if let Some(app) = &self.app_handle {
            // Tauri serialized `()` as JSON `null` — Value::Null is
            // wire-identical (ZEB-445).
            app.emit("mint-changed", serde_json::Value::Null);
        }
        Ok(())
    }

    /// Test path: fetches a cleartext-CBOR blob from ContentStore by CID and merges.
    /// The real Zenoh subscriber path uses handle_incoming_decoded directly
    /// after decrypting the blob.
    async fn handle_incoming(
        &self,
        root_cid: crate::owner_state_types::ContentId,
    ) -> Result<(), MintSyncError> {
        // ZEB-805: same state-root payload class as the zenoh path below, so it
        // takes the same budget rather than the 500 ms small-read default.
        let blob = self
            .content_store
            .get_with_budget(
                &root_cid,
                std::time::Duration::from_millis(crate::content_store::STATE_ROOT_FETCH_TIMEOUT_MS),
            )
            .await
            .map_err(|e| MintSyncError::Other(format!("content_store.get: {e}")))?
            .ok_or(MintSyncError::MissingBlob(root_cid))?;
        // Task 9 test path: cleartext blob (matching Task 8's stubbed publish).
        // Real Zenoh path decrypts before calling handle_incoming_decoded.
        let remote: crate::mint_sync_types::MintSnapshot = ciborium::from_reader(&blob[..])
            .map_err(|e| MintSyncError::Cbor(format!("subscriber decode: {e}")))?;
        self.handle_incoming_decoded(remote).await
    }
}

async fn internal_task(
    mint_db: Arc<std::sync::Mutex<rusqlite::Connection>>,
    content_store: Arc<dyn crate::content_store::ContentStore>,
    sync_state: Arc<TokioMutex<MintSyncState>>,
    dirty: Arc<Notify>,
    mut flush_rx: mpsc::Receiver<()>,
    mut shutdown_rx: mpsc::Receiver<()>,
    debounce: std::time::Duration,
) {
    // Pin the `Notified` future OUTSIDE the loop so its state persists across
    // iterations. If constructed inside the loop, a select! arm that doesn't
    // match the dirty branch drops the future and silently consumes the pending
    // notification permit without delivering it. Pinning outside ensures the
    // underlying `Notified` survives the dropped &mut, retains its state, and
    // the next iteration's poll returns Ready — the permit's effect is
    // preserved. After successfully observing the notification, replace the
    // pinned future with a fresh `Notified` to start waiting for the next one.
    // Mirrors owner_state_sync::internal_task's pinning idiom exactly.
    let notified = dirty.notified();
    tokio::pin!(notified);

    let mut scheduled: Option<tokio::time::Instant> = None;
    loop {
        let next_wake = scheduled
            .unwrap_or_else(|| tokio::time::Instant::now() + std::time::Duration::from_secs(3600));
        tokio::select! {
            _ = notified.as_mut() => {
                // Re-pin a fresh Notified so the next iteration waits for a
                // new notification rather than immediately returning Ready.
                notified.set(dirty.notified());
                scheduled = Some(tokio::time::Instant::now() + debounce);
            }
            _ = tokio::time::sleep_until(next_wake), if scheduled.is_some() => {
                scheduled = None;
                if let Err(e) = publish_root_now(&mint_db, &content_store, &sync_state).await {
                    tracing::warn!(target: "mint_sync", "publish_root_now failed: {e}");
                }
            }
            _ = flush_rx.recv() => {
                scheduled = None;
                if let Err(e) = publish_root_now(&mint_db, &content_store, &sync_state).await {
                    tracing::warn!(target: "mint_sync", "flush publish failed: {e}");
                }
            }
            _ = shutdown_rx.recv() => break,
        }
    }
}

/// Returns true when a snapshot carries no user-authored data — only
/// `apply_migrations`-inserted defaults (the schema-init `default_currency =
/// 'USD'` row stamped with epoch `updated_at`) and no accounts/transactions/
/// deletion-floor entries. Used by both publish paths to skip the no-op
/// publish that would otherwise fire on every fresh-install boot-hook.
///
/// Migration rows are identified by `updated_at == "1970-01-01T00:00:00Z"`
/// — see `mint.rs::apply_migrations` for why epoch is the load-bearing
/// discriminator (any user-edited setting carries `chrono::Utc::now()`).
fn snapshot_has_no_user_data(snap: &MintSnapshot) -> bool {
    let user_settings = snap
        .settings
        .iter()
        .filter(|s| s.updated_at != "1970-01-01T00:00:00Z")
        .count();
    snap.accounts.is_empty()
        && snap.transactions.is_empty()
        && user_settings == 0
        && snap.account_deletion_floor.is_empty()
}

async fn publish_root_now(
    mint_db: &Arc<std::sync::Mutex<rusqlite::Connection>>,
    content_store: &Arc<dyn crate::content_store::ContentStore>,
    sync_state: &Arc<TokioMutex<MintSyncState>>,
) -> Result<(), MintSyncError> {
    let mint_db = mint_db.clone();
    let mut snap = tokio::task::spawn_blocking(move || {
        let mut conn = mint_db.lock().expect("mint_db lock poisoned");
        snapshot_current_db(&mut conn)
    })
    .await
    .map_err(|e| MintSyncError::Other(format!("spawn_blocking: {e}")))??;

    // Attach deletion floor so peers can learn about hard-deleted accounts.
    {
        let st = sync_state.lock().await;
        snap.account_deletion_floor = st.account_deletion_floor.clone();
    }

    if snapshot_has_no_user_data(&snap) {
        tracing::debug!(
            target: "mint_sync",
            accounts = snap.accounts.len(),
            transactions = snap.transactions.len(),
            settings = snap.settings.len(),
            floor = snap.account_deletion_floor.len(),
            "empty snapshot — skipping publish",
        );
        return Ok(());
    }

    let mut cbor = Vec::new();
    ciborium::into_writer(&snap, &mut cbor)
        .map_err(|e| MintSyncError::Cbor(format!("publish encode: {e}")))?;
    // TODO(Task 11): encrypt via space_lookup_key(&kt, b"mint-ledger-v1")
    // For Task 8, we put cleartext into the stub — encryption lands when the
    // engine is wired with identity. The InMemoryStub doesn't care.
    let root_cid = crate::owner_state_types::ContentId::from_bytes(blake3::hash(&cbor).into());
    content_store
        .put(root_cid, cbor)
        .await
        .map_err(|e| MintSyncError::Other(format!("content_store.put: {e}")))?;

    // TODO(Task 11): publish (root_cid, hlc) via Zenoh after encrypt_root_publish.
    tracing::info!(target: "mint_sync", root_cid = ?root_cid, "published mint snapshot");
    Ok(())
}

// ── Real Zenoh engine internals ──────────────────────────────────────────────

/// Internal task for the production Zenoh-backed engine. Mirrors
/// owner_state_sync::internal_task but uses `publisher_tx` for outbound bytes
/// and `subscriber_rx` for inbound encrypted bytes from the event loop bridge.
#[allow(clippy::too_many_arguments)]
async fn internal_task_zenoh(
    shared: EngineShared,
    sync_state_path: std::path::PathBuf,
    keys: FleetKeySet,
    device_id: String,
    dirty: Arc<Notify>,
    mut flush_rx: mpsc::Receiver<()>,
    mut shutdown_rx: mpsc::Receiver<()>,
    debounce: std::time::Duration,
    publisher_tx: mpsc::Sender<Vec<u8>>,
    mut subscriber_rx: mpsc::Receiver<Vec<u8>>,
) {
    let mint_db = shared.mint_db.clone();
    let content_store = shared.content_store.clone();
    let sync_state = shared.sync_state.clone();
    // Latched after subscriber_rx closes (event_loop shut down or engine
    // cleanly stopped). Mirrors owner_state_sync's `inbound_closed` latch.
    let mut inbound_closed = false;
    let mut scheduled: Option<tokio::time::Instant> = None;

    // Pin the `Notified` future OUTSIDE the loop so its state persists across
    // iterations. If constructed inside the loop, a select! arm that doesn't
    // match the dirty branch drops the future and silently consumes the pending
    // notification permit without delivering it. Pinning outside ensures the
    // underlying `Notified` survives the dropped &mut, retains its state, and
    // the next iteration's poll returns Ready — the permit's effect is
    // preserved. After successfully observing the notification, replace the
    // pinned future with a fresh `Notified` to start waiting for the next one.
    // Mirrors owner_state_sync::internal_task's pinning idiom exactly.
    let notified = dirty.notified();
    tokio::pin!(notified);

    loop {
        let next_wake = scheduled
            .unwrap_or_else(|| tokio::time::Instant::now() + std::time::Duration::from_secs(3600));
        tokio::select! {
            _ = notified.as_mut() => {
                // Re-pin a fresh Notified so the next iteration waits for a
                // new notification rather than immediately returning Ready.
                notified.set(dirty.notified());
                scheduled = Some(tokio::time::Instant::now() + debounce);
            }
            _ = tokio::time::sleep_until(next_wake), if scheduled.is_some() => {
                scheduled = None;
                if let Err(e) = publish_root_now_zenoh(
                    &mint_db,
                    &content_store,
                    &sync_state,
                    &sync_state_path,
                    &keys,
                    &device_id,
                    &publisher_tx,
                )
                .await
                {
                    tracing::warn!(target: "mint_sync", "publish_root_now_zenoh failed: {e}");
                }
            }
            _ = flush_rx.recv() => {
                scheduled = None;
                if let Err(e) = publish_root_now_zenoh(
                    &mint_db,
                    &content_store,
                    &sync_state,
                    &sync_state_path,
                    &keys,
                    &device_id,
                    &publisher_tx,
                )
                .await
                {
                    tracing::warn!(target: "mint_sync", "flush publish_zenoh failed: {e}");
                }
            }
            maybe_bytes = subscriber_rx.recv(), if !inbound_closed => {
                let Some(bytes) = maybe_bytes else {
                    tracing::error!(
                        target: "mint_sync",
                        "mint inbound subscriber channel closed; continuing in publish-only mode"
                    );
                    inbound_closed = true;
                    continue;
                };
                if let Err(e) = handle_incoming_publish_zenoh(
                    &bytes,
                    &shared,
                    &keys,
                    &device_id,
                    &sync_state_path,
                )
                .await
                {
                    tracing::warn!(target: "mint_sync", "incoming publish dropped: {e}");
                }
            }
            _ = shutdown_rx.recv() => {
                // Final flush on shutdown if there's pending dirty state.
                if let Err(e) = publish_root_now_zenoh(
                    &mint_db,
                    &content_store,
                    &sync_state,
                    &sync_state_path,
                    &keys,
                    &device_id,
                    &publisher_tx,
                )
                .await
                {
                    tracing::warn!(target: "mint_sync", "shutdown flush failed: {e}");
                }
                break;
            }
        }
    }
}

/// Build a strictly-newer HLC for the local device, using the replay tracker
/// inside `sync_state` as the monotonic counter (same device-id key as
/// owner_state_sync::next_hlc). Mirrors owner_state_sync::next_hlc exactly.
async fn next_hlc_mint(
    sync_state: &Arc<TokioMutex<MintSyncState>>,
    device_id: &str,
) -> crate::owner_state_types::Hlc {
    use std::time::{SystemTime, UNIX_EPOCH};
    let wall_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let mut st = sync_state.lock().await;
    let prev = st.replay_tracker.get(device_id).cloned();
    let (logical, prev_wall) = match prev.as_ref() {
        Some(p) if p.wall_ms == wall_ms => (p.logical.saturating_add(1), p.wall_ms),
        Some(p) if p.wall_ms > wall_ms => (p.logical.saturating_add(1), p.wall_ms),
        Some(p) => (0, p.wall_ms),
        None => (0, 0),
    };
    let effective_wall = std::cmp::max(wall_ms, prev_wall);
    let now = crate::owner_state_types::Hlc {
        wall_ms: effective_wall,
        logical,
        device_id: device_id.to_owned(),
    };
    st.replay_tracker.insert(device_id.to_owned(), now.clone());
    now
}

/// Encrypt and publish the current mint snapshot. Called by internal_task_zenoh.
async fn publish_root_now_zenoh(
    mint_db: &Arc<std::sync::Mutex<rusqlite::Connection>>,
    content_store: &Arc<dyn crate::content_store::ContentStore>,
    sync_state: &Arc<TokioMutex<MintSyncState>>,
    sync_state_path: &std::path::Path,
    keys: &FleetKeySet,
    device_id: &str,
    publisher_tx: &mpsc::Sender<Vec<u8>>,
) -> Result<(), MintSyncError> {
    // 1. Snapshot the DB.
    let mint_db_c = mint_db.clone();
    let mut snap = tokio::task::spawn_blocking(move || {
        let mut conn = mint_db_c.lock().expect("mint_db lock poisoned");
        snapshot_current_db(&mut conn)
    })
    .await
    .map_err(|e| MintSyncError::Other(format!("spawn_blocking: {e}")))??;

    // Attach deletion floor so peers can learn about hard-deleted accounts.
    {
        let st = sync_state.lock().await;
        snap.account_deletion_floor = st.account_deletion_floor.clone();
    }

    if snapshot_has_no_user_data(&snap) {
        tracing::debug!(
            target: "mint_sync",
            accounts = snap.accounts.len(),
            transactions = snap.transactions.len(),
            settings = snap.settings.len(),
            floor = snap.account_deletion_floor.len(),
            "empty snapshot — skipping zenoh publish",
        );
        return Ok(());
    }

    // 2. CBOR-encode the snapshot.
    let mut cbor = Vec::new();
    ciborium::into_writer(&snap, &mut cbor)
        .map_err(|e| MintSyncError::Cbor(format!("zenoh publish encode: {e}")))?;

    // 3. Encrypt the snapshot blob (deterministic nonce, for CID stability).
    //    Publish always uses the newest installed epoch (ZEB-668 S5).
    let kt = keys.newest();
    let lookup = crate::owner_state_crypto::space_lookup_key(&kt, b"mint-ledger-v1");
    let ciphertext = crate::owner_state_crypto::encrypt_entry(&kt, &lookup, &cbor)
        .map_err(|e| MintSyncError::Crypto(format!("encrypt_entry: {e}")))?;

    // 4. Derive CID from ciphertext (not cleartext — mirrors decrypt side).
    let root_cid =
        crate::owner_state_types::ContentId::from_bytes(blake3::hash(&ciphertext).into());

    // 5. Store encrypted blob in CAS so peers can fetch via root_cid.
    content_store
        .put(root_cid, ciphertext)
        .await
        .map_err(|e| MintSyncError::Other(format!("content_store.put: {e}")))?;

    // 6. Build state-root payload with a fresh HLC.
    let at = next_hlc_mint(sync_state, device_id).await;
    let payload = crate::mint_sync_types::MintRootPublishPayload { root_cid, at };
    let mut payload_bytes = Vec::new();
    ciborium::into_writer(&payload, &mut payload_bytes)
        .map_err(|e| MintSyncError::Cbor(format!("zenoh payload encode: {e}")))?;

    // 7. Encrypt the envelope (random nonce; same epoch as the blob).
    let wire = crate::owner_state_crypto::encrypt_root_publish(&kt, &payload_bytes)
        .map_err(|e| MintSyncError::Crypto(format!("encrypt_root_publish: {e}")))?;

    // 8. Forward to event_loop's Zenoh bridge.
    publisher_tx
        .send(wire)
        .await
        .map_err(|_| MintSyncError::Other("publisher channel closed".into()))?;

    // 9. Persist replay tracker so that on restart the local HLC for this
    //    device is remembered, preventing stale HLC re-use (MAJOR 5).
    //    Awaited (not detached) to prevent concurrent saves from racing and
    //    overwriting newer state with an older snapshot.
    {
        let st_snap = sync_state.lock().await.clone();
        let path = sync_state_path.to_path_buf();
        match tokio::task::spawn_blocking(move || crate::mint_sync_persist::save(&path, &st_snap))
            .await
        {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!(target: "mint_sync", "persist sync_state after publish failed: {e}");
            }
            Err(e) => {
                tracing::warn!(target: "mint_sync", "persist sync_state after publish join failed: {e}");
            }
        }
    }

    tracing::info!(target: "mint_sync", root_cid = ?root_cid, "published encrypted mint snapshot");
    Ok(())
}

/// Process an inbound encrypted envelope from Zenoh. Mirrors
/// owner_state_sync::handle_incoming_publish.
///
/// Ordering:
///   1. Decrypt envelope
///   2. Echo-suppress own publishes
///   3. Replay check (read-only)
///   4. Fetch + decrypt + decode blob
///   5. Apply via `handle_incoming_decoded` (includes `mint-changed` emit)  ← CRITICAL 1
///   6. Advance replay tracker + persist                                      ← CRITICAL 3
async fn handle_incoming_publish_zenoh(
    wire: &[u8],
    shared: &EngineShared,
    keys: &FleetKeySet,
    device_id: &str,
    sync_state_path: &std::path::Path,
) -> Result<(), MintSyncError> {
    let sync_state = &shared.sync_state;
    let content_store = &shared.content_store;

    // 1. Decrypt the root-publish envelope, trying every installed epoch
    //    newest-first (ZEB-668 S5 dual-epoch read window). The winner is
    //    bound for the blob decrypt below — a publish is single-epoch.
    let (kt, payload_bytes) = keys
        .accept_set()
        .into_iter()
        .find_map(|candidate| {
            crate::owner_state_crypto::decrypt_root_publish(&candidate, wire)
                .ok()
                .map(|b| (candidate, b))
        })
        .ok_or_else(|| {
            MintSyncError::Crypto(format!(
                "decrypt_root_publish: no installed epoch opened it (epochs {:?})",
                keys.epochs()
            ))
        })?;
    let payload: crate::mint_sync_types::MintRootPublishPayload =
        ciborium::from_reader(&payload_bytes[..])
            .map_err(|e| MintSyncError::Cbor(format!("envelope decode: {e}")))?;

    // 2. Echo suppression — own publishes loop back through Zenoh.
    if payload.at.device_id == device_id {
        return Ok(());
    }

    // 3. Replay check (read-only). We do NOT advance the tracker here;
    //    advancement moves to step 6, AFTER a successful apply (CRITICAL 3).
    {
        let st = sync_state.lock().await;
        let accept = match st.replay_tracker.get(&payload.at.device_id) {
            None => true,
            Some(existing) => payload.at.is_strictly_newer_than(existing),
        };
        if !accept {
            return Ok(()); // duplicate; already seen
        }
    }

    // 4. Fetch the encrypted blob from CAS using root_cid.
    let blob_ciphertext = match content_store
        .get_with_budget(
            &payload.root_cid,
            std::time::Duration::from_millis(crate::content_store::STATE_ROOT_FETCH_TIMEOUT_MS),
        )
        .await
    {
        Ok(Some(b)) => b,
        Ok(None) => {
            // Blob missing — peer's CAS hasn't replicated yet, or the fetch
            // timed out.
            //
            // ZEB-805: this arm used to `return Ok(())`, which made a swallowed
            // fetch failure indistinguishable from success at the call site —
            // the worst of the three sync engines' handling of one condition.
            // It now returns the same `MissingBlob` the sibling fetch site in
            // this file (`handle_incoming_publish`) already returns, so the two
            // paths agree. The engine loop logs it as "incoming publish
            // dropped" and continues; nothing becomes fatal.
            //
            // `budget_ms` is logged beside the cid (whose Debug carries the
            // payload size) so an oversized blob under an undersized budget is
            // legible on sight.
            tracing::warn!(
                target: "mint_sync",
                root_cid = ?payload.root_cid,
                budget_ms = crate::content_store::STATE_ROOT_FETCH_TIMEOUT_MS,
                "missing mint blob (fetch timeout or not yet replicated)"
            );
            return Err(MintSyncError::MissingBlob(payload.root_cid));
        }
        Err(e) => {
            return Err(MintSyncError::Other(format!("content_store.get: {e}")));
        }
    };

    // 5a. Decrypt the blob.
    let lookup = crate::owner_state_crypto::space_lookup_key(&kt, b"mint-ledger-v1");
    let blob_cleartext = crate::owner_state_crypto::decrypt_entry(&kt, &lookup, &blob_ciphertext)
        .map_err(|e| MintSyncError::Crypto(format!("decrypt_entry: {e}")))?;

    // 5b. Decode the snapshot.
    let remote: crate::mint_sync_types::MintSnapshot =
        ciborium::from_reader(&blob_cleartext[..])
            .map_err(|e| MintSyncError::Cbor(format!("blob decode: {e}")))?;

    // 5c. Apply via handle_incoming_decoded, which:
    //     - checks schema_version
    //     - runs apply_remote_snapshot in spawn_blocking
    //     - emits "mint-changed" Tauri event (CRITICAL 1)
    shared.handle_incoming_decoded(remote).await?;

    tracing::info!(
        target: "mint_sync",
        root_cid = ?payload.root_cid,
        peer = %payload.at.device_id,
        "merged remote mint snapshot"
    );

    // 6. Advance replay tracker + persist AFTER successful apply (CRITICAL 3).
    //    A failure in any earlier step leaves the tracker un-advanced so the
    //    peer's republish will be retried.
    //    Awaited (not detached) to prevent concurrent saves from racing and
    //    overwriting newer state with an older snapshot.
    {
        let mut st = sync_state.lock().await;
        st.replay_tracker
            .insert(payload.at.device_id.clone(), payload.at.clone());
        let st_snap = st.clone();
        let path = sync_state_path.to_path_buf();
        match tokio::task::spawn_blocking(move || crate::mint_sync_persist::save(&path, &st_snap))
            .await
        {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!(target: "mint_sync", "persist sync_state failed: {e}");
            }
            Err(e) => {
                tracing::warn!(target: "mint_sync", "persist sync_state join failed: {e}");
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mint::open_in_memory;

    fn fresh_db() -> Connection {
        // open_in_memory() already runs apply_migrations internally.
        open_in_memory().unwrap()
    }

    fn seed_account(conn: &mut Connection, id: &str, name: &str, updated_at: &str) {
        conn.execute(
            "INSERT INTO accounts (id, name, created_at, updated_at) VALUES (?1, ?2, ?3, ?4)",
            params![id, name, "2026-05-01T00:00:00Z", updated_at],
        )
        .unwrap();
    }

    fn seed_tx(conn: &mut Connection, id: &str, acct: &str, desc: &str, updated_at: &str) {
        conn.execute(
            "INSERT INTO transactions \
             (id, transaction_date, amount, currency, account_id, description, created_at, updated_at) \
             VALUES (?1, '2026-05-01', '1', 'USD', ?2, ?3, '2026-05-01T00:00:00Z', ?4)",
            params![id, acct, desc, updated_at],
        )
        .unwrap();
    }

    #[test]
    fn snapshot_round_trips_empty_db() {
        let mut conn = fresh_db();
        let snap = snapshot_current_db(&mut conn).unwrap();
        assert_eq!(snap.accounts.len(), 0);
        assert_eq!(snap.transactions.len(), 0);
        // settings may have a seeded default_currency row from migration; that's OK.
    }

    #[test]
    fn snapshot_includes_tombstones() {
        let mut conn = fresh_db();
        seed_account(&mut conn, "a1", "Chase", "2026-05-01T00:00:00Z");
        seed_tx(&mut conn, "t1", "a1", "x", "2026-05-01T00:00:00Z");
        conn.execute(
            "UPDATE transactions SET deleted_at = ?1, updated_at = ?1 WHERE id = ?2",
            params!["2026-05-02T00:00:00Z", "t1"],
        )
        .unwrap();
        let snap = snapshot_current_db(&mut conn).unwrap();
        assert_eq!(snap.transactions.len(), 1);
        assert!(snap.transactions[0].deleted_at.is_some());
    }

    #[test]
    fn apply_inserts_new_rows() {
        let mut local = fresh_db();
        let remote = MintSnapshot {
            schema_version: 1,
            accounts: vec![AccountRow {
                id: "a1".into(),
                name: "Chase".into(),
                created_at: "2026-05-01T00:00:00Z".into(),
                updated_at: "2026-05-01T00:00:00Z".into(),
            }],
            transactions: vec![TransactionRow {
                id: "t1".into(),
                transaction_date: "2026-05-01".into(),
                amount: "-12.34".into(),
                currency: "USD".into(),
                account_id: "a1".into(),
                description: "Coffee".into(),
                metadata: None,
                created_at: "2026-05-01T00:00:00Z".into(),
                updated_at: "2026-05-01T00:00:00Z".into(),
                deleted_at: None,
            }],
            settings: vec![],
            account_deletion_floor: HashMap::new(),
            captured_at: "2026-05-19T12:00:00Z".into(),
        };
        apply_remote_snapshot(&mut local, &remote, &HashMap::new()).unwrap();
        let snap = snapshot_current_db(&mut local).unwrap();
        assert_eq!(snap.accounts.len(), 1);
        assert_eq!(snap.transactions.len(), 1);
    }

    #[test]
    fn apply_lww_keeps_newer_local() {
        let mut local = fresh_db();
        seed_account(&mut local, "a1", "Chase Newer", "2026-05-02T00:00:00Z");
        let remote = MintSnapshot {
            schema_version: 1,
            accounts: vec![AccountRow {
                id: "a1".into(),
                name: "Chase Older".into(),
                created_at: "2026-05-01T00:00:00Z".into(),
                updated_at: "2026-05-01T00:00:00Z".into(),
            }],
            transactions: vec![],
            settings: vec![],
            account_deletion_floor: HashMap::new(),
            captured_at: "2026-05-19T12:00:00Z".into(),
        };
        apply_remote_snapshot(&mut local, &remote, &HashMap::new()).unwrap();
        let name: String = local
            .query_row("SELECT name FROM accounts WHERE id = ?", ["a1"], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(name, "Chase Newer");
    }

    #[test]
    fn apply_lww_replaces_older_local() {
        let mut local = fresh_db();
        seed_account(&mut local, "a1", "Chase Older", "2026-05-01T00:00:00Z");
        let remote = MintSnapshot {
            schema_version: 1,
            accounts: vec![AccountRow {
                id: "a1".into(),
                name: "Chase Newer".into(),
                created_at: "2026-05-01T00:00:00Z".into(),
                updated_at: "2026-05-02T00:00:00Z".into(),
            }],
            transactions: vec![],
            settings: vec![],
            account_deletion_floor: HashMap::new(),
            captured_at: "2026-05-19T12:00:00Z".into(),
        };
        apply_remote_snapshot(&mut local, &remote, &HashMap::new()).unwrap();
        let name: String = local
            .query_row("SELECT name FROM accounts WHERE id = ?", ["a1"], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(name, "Chase Newer");
    }

    #[test]
    fn apply_propagates_tombstone() {
        let mut local = fresh_db();
        seed_account(&mut local, "a1", "Chase", "2026-05-01T00:00:00Z");
        seed_tx(&mut local, "t1", "a1", "live", "2026-05-01T00:00:00Z");
        let remote = MintSnapshot {
            schema_version: 1,
            accounts: vec![],
            transactions: vec![TransactionRow {
                id: "t1".into(),
                transaction_date: "2026-05-01".into(),
                amount: "1".into(),
                currency: "USD".into(),
                account_id: "a1".into(),
                description: "live".into(),
                metadata: None,
                created_at: "2026-05-01T00:00:00Z".into(),
                updated_at: "2026-05-02T00:00:00Z".into(),
                deleted_at: Some("2026-05-02T00:00:00Z".into()),
            }],
            settings: vec![],
            account_deletion_floor: HashMap::new(),
            captured_at: "2026-05-19T12:00:00Z".into(),
        };
        apply_remote_snapshot(&mut local, &remote, &HashMap::new()).unwrap();
        let deleted_at: Option<String> = local
            .query_row(
                "SELECT deleted_at FROM transactions WHERE id = ?",
                ["t1"],
                |r| r.get(0),
            )
            .unwrap();
        assert!(deleted_at.is_some());
    }

    #[test]
    fn deletion_floor_blocks_stale_account_resurrect() {
        let mut local = fresh_db();
        // local has NO record of a1 (it was previously hard-deleted).
        let mut floor = HashMap::new();
        floor.insert("a1".to_string(), "2026-05-02T00:00:00Z".to_string());
        let remote = MintSnapshot {
            schema_version: 1,
            accounts: vec![AccountRow {
                id: "a1".into(),
                name: "Chase Zombie".into(),
                created_at: "2026-05-01T00:00:00Z".into(),
                updated_at: "2026-05-01T00:00:00Z".into(), // older than floor
            }],
            transactions: vec![],
            settings: vec![],
            account_deletion_floor: {
                let mut m = HashMap::new();
                // Remote also carries its floor entry for a1.
                m.insert("a1".to_string(), "2026-05-02T00:00:00Z".to_string());
                m
            },
            captured_at: "2026-05-19T12:00:00Z".into(),
        };
        let floor_to_merge = apply_remote_snapshot(&mut local, &remote, &floor).unwrap();
        let exists: Option<String> = local
            .query_row("SELECT id FROM accounts WHERE id = ?", ["a1"], |r| r.get(0))
            .optional()
            .unwrap();
        assert!(exists.is_none(), "floor should have blocked the resurrect");
        // The returned floor map must contain the remote's a1 entry so the caller
        // can merge it into local sync_state.
        assert_eq!(
            floor_to_merge.get("a1").map(String::as_str),
            Some("2026-05-02T00:00:00Z"),
            "floor_to_merge must carry the remote's deletion entry for a1"
        );
    }

    #[test]
    fn apply_remote_floor_deletes_local_account() {
        // Scenario: B has account a1 with updated_at = "2026-05-01".
        // B receives a snapshot from A whose floor contains (a1, "2026-05-02")
        // (A deleted a1 after B last synced). B's apply must hard-delete a1 and
        // its transactions, and return a floor_to_merge entry for a1.
        let mut local = fresh_db();
        seed_account(&mut local, "a1", "Chase", "2026-05-01T00:00:00Z");
        seed_tx(&mut local, "t1", "a1", "Coffee", "2026-05-01T00:00:00Z");

        let remote = MintSnapshot {
            schema_version: 1,
            accounts: vec![], // a1 is absent — A deleted it.
            transactions: vec![],
            settings: vec![],
            account_deletion_floor: {
                let mut m = HashMap::new();
                m.insert("a1".to_string(), "2026-05-02T00:00:00Z".to_string());
                m
            },
            captured_at: "2026-05-19T12:00:00Z".into(),
        };

        let floor_to_merge = apply_remote_snapshot(&mut local, &remote, &HashMap::new()).unwrap();

        // Account must be gone.
        let acct_exists: Option<String> = local
            .query_row("SELECT id FROM accounts WHERE id = ?", ["a1"], |r| r.get(0))
            .optional()
            .unwrap();
        assert!(
            acct_exists.is_none(),
            "account a1 must be hard-deleted after remote floor apply"
        );

        // Orphan transaction must also be gone.
        let tx_count: i64 = local
            .query_row(
                "SELECT COUNT(*) FROM transactions WHERE account_id = ?",
                ["a1"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            tx_count, 0,
            "orphan transactions for a1 must be hard-deleted"
        );

        // floor_to_merge must contain the a1 entry so the caller merges it into
        // local sync_state (prevents future zombie-resurrects from other peers).
        assert_eq!(
            floor_to_merge.get("a1").map(String::as_str),
            Some("2026-05-02T00:00:00Z"),
            "floor_to_merge must carry the remote deletion entry for a1"
        );
    }

    #[test]
    fn apply_remote_floor_merges_into_local_floor() {
        // Scenario: local floor has a1 → "2026-05-01" (older).
        // Remote snapshot floor has a1 → "2026-05-03" (newer).
        // floor_to_merge must carry "2026-05-03" so the caller picks the max.
        let mut local = fresh_db();
        let mut local_floor = HashMap::new();
        local_floor.insert("a1".to_string(), "2026-05-01T00:00:00Z".to_string());

        let remote = MintSnapshot {
            schema_version: 1,
            accounts: vec![],
            transactions: vec![],
            settings: vec![],
            account_deletion_floor: {
                let mut m = HashMap::new();
                m.insert("a1".to_string(), "2026-05-03T00:00:00Z".to_string());
                m
            },
            captured_at: "2026-05-19T12:00:00Z".into(),
        };

        let floor_to_merge = apply_remote_snapshot(&mut local, &remote, &local_floor).unwrap();

        assert_eq!(
            floor_to_merge.get("a1").map(String::as_str),
            Some("2026-05-03T00:00:00Z"),
            "floor_to_merge must carry the newer remote timestamp"
        );
    }

    use std::sync::Arc;
    use tokio::sync::Mutex as TokioMutex;

    #[tokio::test]
    async fn publish_writes_to_content_store_and_zenoh_stub() {
        let mut conn = fresh_db();
        seed_account(&mut conn, "a1", "Chase", "2026-05-01T00:00:00Z");
        seed_tx(&mut conn, "t1", "a1", "x", "2026-05-01T00:00:00Z");
        let conn = Arc::new(std::sync::Mutex::new(conn));
        let cs_stub = Arc::new(crate::content_store::InMemoryStub::default());
        let cs: Arc<dyn crate::content_store::ContentStore> = cs_stub.clone();
        let sync_state = Arc::new(TokioMutex::new(MintSyncState::default()));

        let (engine, handle) = MintSyncEngine::new_for_test(conn, cs, sync_state).await;
        engine.flush_now().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert_eq!(cs_stub.debug_count().await, 1);

        engine.shutdown().await.unwrap();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn publish_skips_when_snapshot_is_empty() {
        let conn = Arc::new(std::sync::Mutex::new(fresh_db()));
        let cs_stub = Arc::new(crate::content_store::InMemoryStub::default());
        let cs: Arc<dyn crate::content_store::ContentStore> = cs_stub.clone();
        let sync_state = Arc::new(TokioMutex::new(MintSyncState::default()));

        let (engine, handle) = MintSyncEngine::new_for_test(conn, cs, sync_state).await;
        engine.flush_now().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert_eq!(cs_stub.debug_count().await, 0);

        engine.shutdown().await.unwrap();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn notify_dirty_triggers_debounced_publish() {
        let mut conn = fresh_db();
        seed_account(&mut conn, "a1", "Chase", "2026-05-01T00:00:00Z");
        let conn = Arc::new(std::sync::Mutex::new(conn));
        let cs_stub = Arc::new(crate::content_store::InMemoryStub::default());
        let cs: Arc<dyn crate::content_store::ContentStore> = cs_stub.clone();
        let sync_state = Arc::new(TokioMutex::new(MintSyncState::default()));

        let (engine, handle) = MintSyncEngine::new_for_test_with_debounce(
            conn,
            cs,
            sync_state,
            std::time::Duration::from_millis(50),
        )
        .await;

        engine.notify_dirty();
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert_eq!(cs_stub.debug_count().await, 1);

        engine.shutdown().await.unwrap();
        handle.await.unwrap();
    }

    /// ZEB-702 T2: `MintSyncEngine` is a distinct engine type (not
    /// `FleetSyncEngine`), so it carries its own one-line `RepublishDirty`
    /// impl delegating to `notify_dirty()`. Driving the seam through the
    /// trait object schedules the same debounced publish. Models
    /// `notify_dirty_triggers_debounced_publish`.
    #[tokio::test]
    async fn republish_dirty_triggers_debounced_publish() {
        let mut conn = fresh_db();
        seed_account(&mut conn, "a1", "Chase", "2026-05-01T00:00:00Z");
        let conn = Arc::new(std::sync::Mutex::new(conn));
        let cs_stub = Arc::new(crate::content_store::InMemoryStub::default());
        let cs: Arc<dyn crate::content_store::ContentStore> = cs_stub.clone();
        let sync_state = Arc::new(TokioMutex::new(MintSyncState::default()));

        let (engine, handle) = MintSyncEngine::new_for_test_with_debounce(
            conn,
            cs,
            sync_state,
            std::time::Duration::from_millis(50),
        )
        .await;

        // Drive the seam through the trait object (the shape Task 3 consumes).
        let seam: &dyn crate::fleet_sync::RepublishDirty = &engine;
        seam.republish_dirty();
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert_eq!(cs_stub.debug_count().await, 1);

        engine.shutdown().await.unwrap();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn engine_new_and_shutdown_no_publish() {
        let conn = Arc::new(std::sync::Mutex::new(fresh_db()));
        let cs: Arc<dyn crate::content_store::ContentStore> =
            Arc::new(crate::content_store::InMemoryStub::default());
        let sync_state = Arc::new(TokioMutex::new(MintSyncState::default()));
        let (engine, handle) = MintSyncEngine::new_for_test(conn, cs, sync_state).await;
        engine.shutdown().await.unwrap();
        // Handle joins without panic.
        handle.await.unwrap();
    }

    #[test]
    fn deletion_floor_allows_newer_remote_through() {
        let mut local = fresh_db();
        let mut floor = HashMap::new();
        floor.insert("a1".to_string(), "2026-05-02T00:00:00Z".to_string());
        let remote = MintSnapshot {
            schema_version: 1,
            accounts: vec![AccountRow {
                id: "a1".into(),
                name: "Chase Re-created".into(),
                created_at: "2026-05-03T00:00:00Z".into(),
                updated_at: "2026-05-03T00:00:00Z".into(), // newer than floor
            }],
            transactions: vec![],
            settings: vec![],
            account_deletion_floor: HashMap::new(),
            captured_at: "2026-05-19T12:00:00Z".into(),
        };
        apply_remote_snapshot(&mut local, &remote, &floor).unwrap();
        let name: String = local
            .query_row("SELECT name FROM accounts WHERE id = ?", ["a1"], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(name, "Chase Re-created");
    }

    /// ZEB-805: a CAS miss must be OBSERVABLE to the caller.
    ///
    /// Honest scope note: this drives `handle_incoming_envelope_for_test`, which
    /// already returned `MissingBlob` before this ticket — so it characterises
    /// the contract rather than pinning the change. The change was on the zenoh
    /// path (`handle_incoming_publish_zenoh`), which used to `return Ok(())` and
    /// so made a swallowed fetch failure indistinguishable from success. Both
    /// paths now return this same variant; the zenoh one is covered by
    /// inspection plus the shared error type rather than by its own harness,
    /// because standing up a valid encrypted wire envelope for a three-line
    /// return-value change would be disproportionate.
    ///
    /// What this test does buy: if anyone ever "simplifies" either path back to
    /// swallowing the miss, the contract they would be breaking is written down
    /// and enforced here.
    #[tokio::test]
    async fn cas_miss_is_observable_to_the_caller() {
        let conn = Arc::new(std::sync::Mutex::new(fresh_db()));
        let cs: Arc<dyn crate::content_store::ContentStore> =
            Arc::new(crate::content_store::InMemoryStub::default());
        let sync_state = Arc::new(TokioMutex::new(MintSyncState::default()));
        let (engine, handle) = MintSyncEngine::new_for_test(conn, cs, sync_state).await;

        // A cid nothing ever put: the store misses.
        let absent = crate::owner_state_types::ContentId::from_bytes([0xEE; 32]);
        let err = engine
            .handle_incoming_envelope_for_test(absent)
            .await
            .expect_err("a CAS miss must NOT report success");
        assert!(
            matches!(err, MintSyncError::MissingBlob(c) if c == absent),
            "a miss must surface as MissingBlob naming the cid, got {err:?}"
        );

        engine.shutdown().await.unwrap();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn subscriber_applies_remote_snapshot_to_local() {
        // Two DBs, one shared ContentStore. Engine A publishes; engine B's
        // subscriber-for-test pulls and merges.
        let mut conn_a = fresh_db();
        seed_account(&mut conn_a, "a1", "Chase", "2026-05-01T00:00:00Z");
        seed_tx(&mut conn_a, "t1", "a1", "Coffee", "2026-05-01T00:00:00Z");
        let conn_a = Arc::new(std::sync::Mutex::new(conn_a));
        let conn_b = Arc::new(std::sync::Mutex::new(fresh_db()));
        // Hold both the concrete and the trait-object Arcs (Task 8 pattern).
        let cs_stub = Arc::new(crate::content_store::InMemoryStub::default());
        let cs: Arc<dyn crate::content_store::ContentStore> = cs_stub.clone();
        let sync_state_a = Arc::new(TokioMutex::new(MintSyncState::default()));
        let sync_state_b = Arc::new(TokioMutex::new(MintSyncState::default()));

        let (engine_a, handle_a) =
            MintSyncEngine::new_for_test(conn_a, cs.clone(), sync_state_a).await;
        let (engine_b, handle_b) =
            MintSyncEngine::new_for_test(conn_b.clone(), cs.clone(), sync_state_b).await;

        // A publishes.
        engine_a.flush_now().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Drive B's subscriber by replaying every CID that A put into the stub.
        let cids = cs_stub.debug_all_cids().await;
        assert_eq!(cids.len(), 1);
        engine_b
            .handle_incoming_envelope_for_test(cids[0])
            .await
            .unwrap();

        // B's DB should now contain a1 + t1 with matching identifiers and
        // descriptions (assert identity, not just row counts — a regression
        // that merged the wrong rows would slip past length-only checks).
        let snap_b = {
            let mut conn = conn_b.lock().unwrap();
            snapshot_current_db(&mut conn).unwrap()
        };
        assert_eq!(snap_b.accounts.len(), 1);
        assert_eq!(snap_b.accounts[0].id, "a1");
        assert_eq!(snap_b.accounts[0].name, "Chase");
        assert_eq!(snap_b.transactions.len(), 1);
        assert_eq!(snap_b.transactions[0].id, "t1");
        assert_eq!(snap_b.transactions[0].description, "Coffee");

        engine_a.shutdown().await.unwrap();
        engine_b.shutdown().await.unwrap();
        handle_a.await.unwrap();
        handle_b.await.unwrap();
    }

    #[test]
    fn apply_resurrects_after_tombstone() {
        let mut local = fresh_db();
        seed_account(&mut local, "a1", "Chase", "2026-05-01T00:00:00Z");
        seed_tx(&mut local, "t1", "a1", "x", "2026-05-01T00:00:00Z");
        local
            .execute(
                "UPDATE transactions SET deleted_at = ?1, updated_at = ?1 WHERE id = ?2",
                params!["2026-05-02T00:00:00Z", "t1"],
            )
            .unwrap();
        let remote = MintSnapshot {
            schema_version: 1,
            accounts: vec![],
            transactions: vec![TransactionRow {
                id: "t1".into(),
                transaction_date: "2026-05-01".into(),
                amount: "1".into(),
                currency: "USD".into(),
                account_id: "a1".into(),
                description: "x".into(),
                metadata: None,
                created_at: "2026-05-01T00:00:00Z".into(),
                updated_at: "2026-05-03T00:00:00Z".into(),
                deleted_at: None,
            }],
            settings: vec![],
            account_deletion_floor: HashMap::new(),
            captured_at: "2026-05-19T12:00:00Z".into(),
        };
        apply_remote_snapshot(&mut local, &remote, &HashMap::new()).unwrap();
        let deleted_at: Option<String> = local
            .query_row(
                "SELECT deleted_at FROM transactions WHERE id = ?",
                ["t1"],
                |r| r.get(0),
            )
            .unwrap();
        assert!(deleted_at.is_none());
    }

    #[tokio::test]
    async fn boot_hook_publishes_when_non_empty() {
        let mut conn = fresh_db();
        seed_account(&mut conn, "a1", "Chase", "2026-05-01T00:00:00Z");
        let conn = Arc::new(std::sync::Mutex::new(conn));
        // Dual-arc pattern: concrete stub for debug_count, erased for the engine.
        let cs_stub = Arc::new(crate::content_store::InMemoryStub::default());
        let cs: Arc<dyn crate::content_store::ContentStore> = cs_stub.clone();
        let sync_state = Arc::new(TokioMutex::new(MintSyncState::default()));

        let (engine, handle) = MintSyncEngine::new_for_test_with_boot_delay(
            conn,
            cs,
            sync_state,
            std::time::Duration::from_millis(50),
        )
        .await;

        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert_eq!(
            cs_stub.debug_count().await,
            1,
            "boot-hook should have published"
        );

        engine.shutdown().await.unwrap();
        handle.await.unwrap();
    }

    #[test]
    fn snapshot_with_only_epoch_setting_is_treated_as_empty() {
        // Migration default rows (`apply_migrations` inserts
        // `default_currency='USD'` stamped with the epoch) must not count
        // as user data — otherwise every fresh-install boot-hook publishes
        // a no-op snapshot. This is the load-bearing migration-row
        // discriminator behavior.
        let snap = MintSnapshot {
            schema_version: 1,
            accounts: vec![],
            transactions: vec![],
            settings: vec![SettingRow {
                key: "default_currency".into(),
                value: "USD".into(),
                updated_at: "1970-01-01T00:00:00Z".into(),
            }],
            account_deletion_floor: HashMap::new(),
            captured_at: "2026-05-27T12:00:00Z".into(),
        };
        assert!(snapshot_has_no_user_data(&snap));
    }

    #[test]
    fn snapshot_with_non_epoch_setting_is_not_empty() {
        // CodeRabbit PR #166: the boundary in the other direction. A
        // settings-only snapshot whose `updated_at` is anything other than
        // the epoch represents a real user edit (currency switch, theme
        // pref, etc.) and MUST publish — even with zero accounts and zero
        // transactions, otherwise we'd silently drop the only state the
        // user has authored on a new device.
        let snap = MintSnapshot {
            schema_version: 1,
            accounts: vec![],
            transactions: vec![],
            settings: vec![SettingRow {
                key: "default_currency".into(),
                value: "EUR".into(),
                updated_at: "2026-05-27T12:00:00Z".into(),
            }],
            account_deletion_floor: HashMap::new(),
            captured_at: "2026-05-27T12:00:00Z".into(),
        };
        assert!(!snapshot_has_no_user_data(&snap));
    }

    #[tokio::test]
    async fn boot_hook_skips_when_empty() {
        let conn = Arc::new(std::sync::Mutex::new(fresh_db()));
        let cs_stub = Arc::new(crate::content_store::InMemoryStub::default());
        let cs: Arc<dyn crate::content_store::ContentStore> = cs_stub.clone();
        let sync_state = Arc::new(TokioMutex::new(MintSyncState::default()));

        let (engine, handle) = MintSyncEngine::new_for_test_with_boot_delay(
            conn,
            cs,
            sync_state,
            std::time::Duration::from_millis(50),
        )
        .await;

        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert_eq!(
            cs_stub.debug_count().await,
            0,
            "boot-hook on empty db should no-op"
        );

        engine.shutdown().await.unwrap();
        handle.await.unwrap();
    }
}
