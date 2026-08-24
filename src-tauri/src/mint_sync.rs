//! Mint Phase 2 sync engine. Mirrors owner_state_sync's shape.

use crate::mint_sync_types::MintSyncState;
use crate::mint_sync_types::{AccountRow, MintSnapshot, MintSyncError, SettingRow, TransactionRow};
use crate::owner_state_crypto::FleetKeySet;
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex as TokioMutex, Notify, Semaphore};

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
///
/// `now_ms` is the receiver's own trusted wall clock in epoch-milliseconds
/// (`clock_trust::receiver_now_ms()`), or `None` when that clock is
/// unreadable. It bounds every peer-supplied `updated_at` (ZEB-848 T-MINT): an
/// unparseable or far-future-dated stamp is dropped at ingest so it can no
/// longer win the per-row string LWW (nor drive a poison hard-delete) and
/// revert honest ledger edits. `None` fails open on the forward-skew half (a
/// bad LOCAL clock must not drop honest state) but still drops an unparseable
/// stamp — see `clock_trust::reject_rfc3339_future`.
pub(crate) fn apply_remote_snapshot(
    conn: &mut Connection,
    remote: &MintSnapshot,
    account_deletion_floor: &HashMap<String, String>,
    now_ms: Option<u64>,
) -> Result<HashMap<String, String>, MintSyncError> {
    let tx = conn.transaction()?;
    // Collect account IDs suppressed by the deletion floor so we can skip
    // transactions that reference them (MAJOR 7: prevents FK violations /
    // orphan rows when a remote account was blocked but its transactions
    // were not).
    let mut suppressed_account_ids = std::collections::HashSet::new();
    for r in &remote.accounts {
        match upsert_account_lww(&tx, r, account_deletion_floor, now_ms)? {
            // Both skip-outcomes mean "do not apply this account's transactions":
            // Suppressed (stale w.r.t. our delete floor) and RejectedNoLocalAccount
            // (poison stamp, account never inserted — an in-range txn referencing
            // it would FK-abort the whole snapshot).
            UpsertOutcome::Suppressed | UpsertOutcome::RejectedNoLocalAccount => {
                suppressed_account_ids.insert(r.id.clone());
            }
            UpsertOutcome::Applied => {}
        }
    }
    for r in &remote.transactions {
        if suppressed_account_ids.contains(&r.account_id) {
            // Owning account was suppressed by deletion floor; skip to avoid
            // FK violations and orphan rows.
            continue;
        }
        upsert_transaction_lww(&tx, r, now_ms)?;
    }
    for r in &remote.settings {
        upsert_setting_lww(&tx, r, now_ms)?;
    }

    // Apply remote deletion floor: hard-delete local accounts that the remote
    // device has deleted, and collect floor entries to merge into local state.
    // Note: this does NOT go through delete_account (which requires a reassign
    // target). The user already deleted the account on the other device; the
    // transactions are stale and the cascade is expected. Explicit DELETE is
    // required because the schema has no ON DELETE CASCADE for transactions.
    let mut floor_to_merge: HashMap<String, String> = HashMap::new();
    for (id, remote_ts) in &remote.account_deletion_floor {
        // ZEB-848: a poison deletion stamp (unparseable, or far-future beyond the
        // control-tier skew) must NOT trigger a hard-delete of a live local
        // account, nor be merged into our own deletion floor (where it would
        // block every honest future re-create forever). Drop it at ingest.
        if crate::clock_trust::reject_rfc3339_future(
            remote_ts,
            now_ms,
            crate::clock_trust::MAX_FORWARD_SKEW_MS,
        ) {
            continue;
        }
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
    /// Blocked by the deletion floor — the peer's row is stale w.r.t. our delete.
    Suppressed,
    /// The row's `updated_at` was rejected (poison / forward-skew / non-canonical)
    /// and no local account exists. Skip its dependent transactions — an in-range
    /// transaction referencing a never-inserted account would violate the accounts
    /// FK and roll back the whole snapshot.
    RejectedNoLocalAccount,
}

fn upsert_account_lww(
    tx: &rusqlite::Transaction,
    r: &AccountRow,
    floor: &HashMap<String, String>,
    now_ms: Option<u64>,
) -> Result<UpsertOutcome, MintSyncError> {
    // ZEB-848: bound the peer `updated_at` before it can win the string LWW.
    // Drop a poison / forward-skew / non-canonical stamp. The skip decision must
    // be account-aware: if the account is NOT already local, tell the caller to
    // skip its dependent transactions (RejectedNoLocalAccount) — an in-range txn
    // referencing this never-inserted account would violate the accounts FK and
    // roll back the WHOLE snapshot. If the account DOES exist locally, its honest
    // row stays and its transactions can still merge, so return `Applied` (a
    // rejected stamp is NOT a deletion-floor suppression).
    if crate::clock_trust::reject_rfc3339_future(
        &r.updated_at,
        now_ms,
        crate::clock_trust::MAX_FORWARD_SKEW_MS,
    ) {
        let exists = tx
            .query_row(
                "SELECT 1 FROM accounts WHERE id = ?1",
                params![r.id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        return Ok(if exists {
            UpsertOutcome::Applied
        } else {
            UpsertOutcome::RejectedNoLocalAccount
        });
    }
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
    now_ms: Option<u64>,
) -> Result<(), MintSyncError> {
    // ZEB-848: drop a poison peer `updated_at` before it can win the LWW.
    if crate::clock_trust::reject_rfc3339_future(
        &r.updated_at,
        now_ms,
        crate::clock_trust::MAX_FORWARD_SKEW_MS,
    ) {
        return Ok(());
    }
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

fn upsert_setting_lww(
    tx: &rusqlite::Transaction,
    r: &SettingRow,
    now_ms: Option<u64>,
) -> Result<(), MintSyncError> {
    // ZEB-848: drop a poison peer `updated_at` before the ON CONFLICT LWW.
    if crate::clock_trust::reject_rfc3339_future(
        &r.updated_at,
        now_ms,
        crate::clock_trust::MAX_FORWARD_SKEW_MS,
    ) {
        return Ok(());
    }
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
    /// ZEB-982: seals `mint_sync_state.cbor` at rest. Set together with
    /// `sync_state_path` (production ctor); `None` in test constructors.
    cipher: Option<crate::device_dataset_file::DeviceCipher>,
    /// Production passes `Some(sink)` so successful subscriber merges
    /// can fire a `"mint-changed"` event (ZEB-445: via the mode-agnostic
    /// sink). Test constructors pass `None` — the emit is a best-effort UI
    /// notification, not a correctness requirement, so skipping it in
    /// tests is safe.
    app_handle: Option<std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>>,
    /// ZEB-845: node-wide bounded-adoption floor (see `hlc_adopt_floor` module
    /// docs). Read at `next_hlc_mint`, fed at the inbound mint-root apply.
    adopt_floor: crate::hlc_adopt_floor::HlcAdoptFloor,
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
            cipher: None,
            app_handle: None,
            // ZEB-845: test constructors default to a fresh (empty/identity)
            // floor. Production wires the real node-wide handle via `new`.
            adopt_floor: crate::hlc_adopt_floor::HlcAdoptFloor::new(),
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
        cipher: crate::device_dataset_file::DeviceCipher,
        publisher_tx: mpsc::Sender<Vec<u8>>,
        subscriber_rx: mpsc::Receiver<Vec<u8>>,
        debounce_ms: u64,
        app_handle: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>,
        adopt_floor: crate::hlc_adopt_floor::HlcAdoptFloor,
    ) -> (Self, MintSyncEngineHandle) {
        let dirty = Arc::new(Notify::new());
        let (flush_tx, flush_rx) = mpsc::channel::<()>(4);
        let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>(1);
        let shared = EngineShared {
            mint_db: mint_db.clone(),
            content_store: content_store.clone(),
            sync_state: sync_state.clone(),
            sync_state_path: Some(sync_state_path.clone()),
            cipher: Some(cipher),
            app_handle: Some(app_handle),
            // ZEB-845: node-wide bounded-adoption floor.
            adopt_floor,
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

    /// ZEB-982: the at-rest cipher paired with [`Self::sync_state_path`].
    pub fn dataset_cipher(&self) -> Option<crate::device_dataset_file::DeviceCipher> {
        self.shared.cipher.clone()
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
        // ZEB-848: read the receiver's trusted wall clock ONCE per snapshot,
        // from SystemTime only (never a peer/HLC value), and thread it into the
        // ingest gate. `None` on an unreadable clock fails open on forward-skew.
        let now_ms = crate::clock_trust::receiver_now_ms();
        let floor_to_merge = tokio::task::spawn_blocking(
            move || -> Result<HashMap<String, String>, MintSyncError> {
                let mut conn = mint_db.lock().expect("mint_db lock poisoned");
                let st = sync_state.blocking_lock();
                apply_remote_snapshot(&mut conn, &remote, &st.account_deletion_floor, now_ms)
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
            // ZEB-805 r2: classified like the zenoh path so the two agree on
            // what a transient fetch failure is, even though this caller does
            // not retry.
            .map_err(|e| MintSyncError::BlobFetchFailed {
                cid: root_cid,
                detail: e.to_string(),
            })?
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

/// ZEB-805 (r1): run one inbound mint frame through the pipeline and, on a
/// transient CAS miss, schedule a bounded retry instead of dropping it.
///
/// Shared by the subscriber arm (fresh frames, full budget) and the fetch-retry
/// arm (re-injected frames, decremented budget), mirroring the same split in
/// `community_state_sync` and `fleet_sync`. Before this, mint was the last of
/// the three engines to treat a transient miss as terminal — the review round
/// on ZEB-805 caught that the PR claimed cross-engine consistency as its
/// deliverable while leaving one engine inconsistent.
///
/// Re-injection is admissible for the same reason it is in the other two:
/// `handle_incoming_publish_zenoh`'s step-3 replay check is READ-ONLY and the
/// tracker only advances after a successful apply (step 6, CRITICAL 3), so the
/// same frame is still acceptable on a later pass. If that ever changes, every
/// retry here silently becomes a rejected duplicate.
///
/// Unlike the other two engines this keeps no counters: mint has no
/// `network_health` consumer to read them. Both drop sites log at WARN.
#[allow(clippy::too_many_arguments)]
async fn process_inbound_zenoh(
    wire: Vec<u8>,
    shared: &EngineShared,
    keys: &FleetKeySet,
    device_id: &str,
    sync_state_path: &std::path::Path,
    attempts_left: u8,
    retry_tx: &mpsc::Sender<(Vec<u8>, u8)>,
    retry_sem: &Arc<Semaphore>,
) {
    let err = match handle_incoming_publish_zenoh(&wire, shared, keys, device_id, sync_state_path)
        .await
    {
        Ok(()) => return,
        Err(e) => e,
    };

    // ZEB-805 r2: the transient class is defined ON the error type, not
    // re-derived here. Matching a variant list at the call site is what let the
    // r1 retry silently exclude transport errors.
    let Some(cid) = err.transient_fetch_cid() else {
        // Deterministic failure (crypto, decode, apply): the same bytes would
        // fail the same way, so a retry buys nothing.
        tracing::warn!(target: "mint_sync", "incoming publish dropped: {err}");
        return;
    };

    if attempts_left == 0 {
        tracing::warn!(
            target: "mint_sync",
            root_cid = ?cid,
            blob_bytes = cid.payload_size(),
            budget_ms = crate::content_store::STATE_ROOT_FETCH_TIMEOUT_MS,
            attempts = crate::content_store::fetch_retry::ATTEMPTS,
            "mint state-root fetch retry budget exhausted — publish dropped"
        );
        return;
    }

    // Permit BEFORE spawn, so what is capped is the number of retained wire
    // buffers, not merely channel depth.
    let Ok(permit) = Arc::clone(retry_sem).try_acquire_owned() else {
        tracing::warn!(
            target: "mint_sync",
            root_cid = ?cid,
            "mint fetch-retry in-flight cap saturated — retry dropped"
        );
        return;
    };
    tracing::info!(
        target: "mint_sync",
        root_cid = ?cid,
        attempts_left,
        delay_ms = crate::content_store::fetch_retry::DELAY_MS,
        "mint state-root blob fetch failed — retry scheduled"
    );
    let tx = retry_tx.clone();
    tokio::spawn(async move {
        // Held for the sleeper's whole lifetime; released on task end.
        let _permit = permit;
        tokio::time::sleep(std::time::Duration::from_millis(
            crate::content_store::fetch_retry::DELAY_MS,
        ))
        .await;
        if tx.try_send((wire, attempts_left - 1)).is_err() {
            tracing::warn!(
                target: "mint_sync",
                "mint fetch-retry re-injection channel unavailable — retry dropped"
            );
        }
    });
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
    // ZEB-982: this task only runs for the path-ful production engine, whose
    // ctor always pairs the path with a cipher.
    let cipher = shared
        .cipher
        .clone()
        .expect("ZEB-982: a path-ful mint engine always carries a cipher");
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

    // ZEB-805 (r1): bounded fetch-retry plumbing. Capacity == the in-flight cap
    // so a full permit set can never overflow the channel.
    let (fetch_retry_tx, mut fetch_retry_rx) =
        mpsc::channel::<(Vec<u8>, u8)>(crate::content_store::fetch_retry::MAX_INFLIGHT);
    let fetch_retry_sem = Arc::new(Semaphore::new(
        crate::content_store::fetch_retry::MAX_INFLIGHT,
    ));

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
                    &cipher,
                    &keys,
                    &device_id,
                    &publisher_tx,
                    &shared.adopt_floor,
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
                    &cipher,
                    &keys,
                    &device_id,
                    &publisher_tx,
                    &shared.adopt_floor,
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
                // ZEB-805: a fresh inbound frame carries the full retry budget.
                process_inbound_zenoh(
                    bytes,
                    &shared,
                    &keys,
                    &device_id,
                    &sync_state_path,
                    crate::content_store::fetch_retry::ATTEMPTS,
                    &fetch_retry_tx,
                    &fetch_retry_sem,
                )
                .await;
            }
            // ZEB-805: a fetch-retry sleeper handing its frame back after the
            // delay. Re-enters the FULL inbound pipeline, so the read-only
            // replay check kills a retry a newer root has already superseded.
            Some((wire, attempts_left)) = fetch_retry_rx.recv() => {
                process_inbound_zenoh(
                    wire,
                    &shared,
                    &keys,
                    &device_id,
                    &sync_state_path,
                    attempts_left,
                    &fetch_retry_tx,
                    &fetch_retry_sem,
                )
                .await;
            }
            _ = shutdown_rx.recv() => {
                // Final flush on shutdown if there's pending dirty state.
                if let Err(e) = publish_root_now_zenoh(
                    &mint_db,
                    &content_store,
                    &sync_state,
                    &sync_state_path,
                    &cipher,
                    &keys,
                    &device_id,
                    &publisher_tx,
                    &shared.adopt_floor,
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
    floor: &crate::hlc_adopt_floor::HlcAdoptFloor, // ZEB-845
) -> crate::owner_state_types::Hlc {
    use std::time::{SystemTime, UNIX_EPOCH};
    // ZEB-845: bounded causal adoption — clamp local wall up to a verified
    // remote wall (≤ CAP), same as the other mint seams.
    let wall_ms = floor.merged_now(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
    );
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
#[allow(clippy::too_many_arguments)]
async fn publish_root_now_zenoh(
    mint_db: &Arc<std::sync::Mutex<rusqlite::Connection>>,
    content_store: &Arc<dyn crate::content_store::ContentStore>,
    sync_state: &Arc<TokioMutex<MintSyncState>>,
    sync_state_path: &std::path::Path,
    cipher: &crate::device_dataset_file::DeviceCipher,
    keys: &FleetKeySet,
    device_id: &str,
    publisher_tx: &mpsc::Sender<Vec<u8>>,
    adopt_floor: &crate::hlc_adopt_floor::HlcAdoptFloor, // ZEB-845
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
    let at = next_hlc_mint(sync_state, device_id, adopt_floor).await;
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
        let cipher = cipher.clone();
        match tokio::task::spawn_blocking(move || {
            crate::mint_sync_persist::save(&cipher, &path, &st_snap)
        })
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
            // ZEB-805 r2: a transport/event-loop failure is the SAME transient
            // class as a miss and must retry identically. r1 keyed the new
            // retry on `MissingBlob` alone, so this arm — reported as the
            // catch-all `Other` — was treated as deterministic and dropped the
            // publish. That is precisely the asymmetry r1 had just removed from
            // `community_state_sync`, reintroduced here in the same commit.
            tracing::warn!(
                target: "mint_sync",
                root_cid = ?payload.root_cid,
                blob_bytes = payload.root_cid.payload_size(),
                budget_ms = crate::content_store::STATE_ROOT_FETCH_TIMEOUT_MS,
                error = %e,
                "mint state-root blob fetch error — will retry if budget remains"
            );
            return Err(MintSyncError::BlobFetchFailed {
                cid: payload.root_cid,
                detail: e.to_string(),
            });
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
        // ZEB-845: verified sibling mint-root — feed the adoption floor after
        // the replay-tracker advance (step 6). Every earlier `?`/echo-suppress/
        // replay-skip return leaves the floor untouched.
        shared.adopt_floor.observe(payload.at.wall_ms);
        let st_snap = st.clone();
        let path = sync_state_path.to_path_buf();
        let cipher = shared
            .cipher
            .clone()
            .expect("ZEB-982: a path-ful mint engine always carries a cipher");
        match tokio::task::spawn_blocking(move || {
            crate::mint_sync_persist::save(&cipher, &path, &st_snap)
        })
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

    fn seed_setting(conn: &Connection, key: &str, value: &str, updated_at: &str) {
        // ON CONFLICT so we overwrite the migration-seeded `default_currency`
        // (epoch `updated_at`) with a real user-edit stamp for LWW tests.
        conn.execute(
            "INSERT INTO settings (key, value, updated_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            params![key, value, updated_at],
        )
        .unwrap();
    }

    fn account_name(conn: &Connection, id: &str) -> String {
        conn.query_row("SELECT name FROM accounts WHERE id = ?", [id], |r| r.get(0))
            .unwrap()
    }

    fn account_exists(conn: &Connection, id: &str) -> bool {
        conn.query_row("SELECT 1 FROM accounts WHERE id = ?", [id], |_| Ok(()))
            .optional()
            .unwrap()
            .is_some()
    }

    fn tx_exists(conn: &Connection, id: &str) -> bool {
        conn.query_row("SELECT 1 FROM transactions WHERE id = ?", [id], |_| Ok(()))
            .optional()
            .unwrap()
            .is_some()
    }

    fn tx_amount(conn: &Connection, id: &str) -> String {
        conn.query_row("SELECT amount FROM transactions WHERE id = ?", [id], |r| {
            r.get(0)
        })
        .unwrap()
    }

    fn setting_value(conn: &Connection, key: &str) -> String {
        conn.query_row("SELECT value FROM settings WHERE key = ?", [key], |r| {
            r.get(0)
        })
        .unwrap()
    }

    // ── ZEB-848 T-MINT: bound peer `updated_at` at the mint ingest boundary ──
    //
    // Fixed receiver "now" for all bound tests (≈2025-10-09), so the forward-skew
    // gate is evaluated against a deterministic clock with no wall-clock flakiness.
    const T_NOW: u64 = 1_760_000_000_000;

    fn ms_to_rfc3339(ms: u64) -> String {
        chrono::DateTime::from_timestamp_millis(ms as i64)
            .unwrap()
            .to_rfc3339()
    }

    #[test]
    fn poison_far_future_account_row_loses_to_local() {
        let mut conn = fresh_db();
        let local_ts = ms_to_rfc3339(T_NOW - 60_000);
        seed_account(&mut conn, "a1", "honest", &local_ts);
        // Remote poison: same id, far-future stamp, different name. The far-future
        // stamp would win the raw string LWW forever without the ingest gate.
        let remote = MintSnapshot {
            schema_version: 1,
            accounts: vec![AccountRow {
                id: "a1".into(),
                name: "POISON".into(),
                created_at: local_ts.clone(),
                updated_at: "9999-12-31T23:59:59Z".into(),
            }],
            transactions: vec![],
            settings: vec![],
            account_deletion_floor: HashMap::new(),
            captured_at: "2026-05-19T12:00:00Z".into(),
        };
        apply_remote_snapshot(&mut conn, &remote, &HashMap::new(), Some(T_NOW)).unwrap();
        // Local name survives (poison rejected, no LWW overwrite).
        assert_eq!(account_name(&conn, "a1"), "honest");
    }

    #[test]
    fn poison_unparseable_account_row_loses_to_local() {
        let mut conn = fresh_db();
        let local_ts = ms_to_rfc3339(T_NOW - 60_000);
        seed_account(&mut conn, "a1", "honest", &local_ts);
        // "zzzz" is not RFC-3339 and also sorts ABOVE every real stamp under a
        // raw string compare, so it too would win LWW without the gate.
        let remote = MintSnapshot {
            schema_version: 1,
            accounts: vec![AccountRow {
                id: "a1".into(),
                name: "POISON".into(),
                created_at: local_ts.clone(),
                updated_at: "zzzz".into(),
            }],
            transactions: vec![],
            settings: vec![],
            account_deletion_floor: HashMap::new(),
            captured_at: "2026-05-19T12:00:00Z".into(),
        };
        apply_remote_snapshot(&mut conn, &remote, &HashMap::new(), Some(T_NOW)).unwrap();
        assert_eq!(account_name(&conn, "a1"), "honest");
    }

    #[test]
    fn poison_far_future_transaction_row_does_not_overwrite_amount() {
        let mut conn = fresh_db();
        let local_ts = ms_to_rfc3339(T_NOW - 60_000);
        seed_account(&mut conn, "a1", "acct", &local_ts);
        seed_tx(&mut conn, "t1", "a1", "honest", &local_ts); // seed_tx sets amount '1'
        let remote = MintSnapshot {
            schema_version: 1,
            accounts: vec![],
            transactions: vec![TransactionRow {
                id: "t1".into(),
                transaction_date: "2026-05-01".into(),
                amount: "999.99".into(),
                currency: "USD".into(),
                account_id: "a1".into(),
                description: "POISON".into(),
                metadata: None,
                created_at: local_ts.clone(),
                updated_at: "9999-12-31T23:59:59Z".into(),
                deleted_at: None,
            }],
            settings: vec![],
            account_deletion_floor: HashMap::new(),
            captured_at: "2026-05-19T12:00:00Z".into(),
        };
        apply_remote_snapshot(&mut conn, &remote, &HashMap::new(), Some(T_NOW)).unwrap();
        assert_eq!(tx_amount(&conn, "t1"), "1"); // seeded amount survives
    }

    #[test]
    fn poison_setting_row_loses_to_local() {
        let mut conn = fresh_db();
        let local_ts = ms_to_rfc3339(T_NOW - 60_000);
        seed_setting(&conn, "default_currency", "USD", &local_ts);
        let remote = MintSnapshot {
            schema_version: 1,
            accounts: vec![],
            transactions: vec![],
            settings: vec![SettingRow {
                key: "default_currency".into(),
                value: "XXX".into(),
                updated_at: "zzzz".into(),
            }],
            account_deletion_floor: HashMap::new(),
            captured_at: "2026-05-19T12:00:00Z".into(),
        };
        apply_remote_snapshot(&mut conn, &remote, &HashMap::new(), Some(T_NOW)).unwrap();
        assert_eq!(setting_value(&conn, "default_currency"), "USD");
    }

    #[test]
    fn legit_newer_remote_still_wins_control() {
        // Gate must NOT over-reject an honest newer edit within tolerance.
        let mut conn = fresh_db();
        seed_account(&mut conn, "a1", "old", &ms_to_rfc3339(T_NOW - 60_000));
        let remote = MintSnapshot {
            schema_version: 1,
            accounts: vec![AccountRow {
                id: "a1".into(),
                name: "new".into(),
                created_at: ms_to_rfc3339(T_NOW - 60_000),
                updated_at: ms_to_rfc3339(T_NOW - 1_000),
            }],
            transactions: vec![],
            settings: vec![],
            account_deletion_floor: HashMap::new(),
            captured_at: "2026-05-19T12:00:00Z".into(),
        };
        apply_remote_snapshot(&mut conn, &remote, &HashMap::new(), Some(T_NOW)).unwrap();
        assert_eq!(account_name(&conn, "a1"), "new");
    }

    #[test]
    fn none_clock_fails_open_on_skew_but_still_drops_unparseable() {
        // Unreadable local clock: far-future applies (fail-open on skew)…
        let mut conn = fresh_db();
        seed_account(&mut conn, "a1", "old", &ms_to_rfc3339(T_NOW - 60_000));
        let remote_future = MintSnapshot {
            schema_version: 1,
            accounts: vec![AccountRow {
                id: "a1".into(),
                name: "future".into(),
                created_at: ms_to_rfc3339(T_NOW - 60_000),
                updated_at: "9999-12-31T23:59:59Z".into(),
            }],
            transactions: vec![],
            settings: vec![],
            account_deletion_floor: HashMap::new(),
            captured_at: "2026-05-19T12:00:00Z".into(),
        };
        apply_remote_snapshot(&mut conn, &remote_future, &HashMap::new(), None).unwrap();
        assert_eq!(account_name(&conn, "a1"), "future");
        // …but an unparseable stamp is still dropped even with no clock.
        let remote_garbage = MintSnapshot {
            schema_version: 1,
            accounts: vec![AccountRow {
                id: "a1".into(),
                name: "GARBAGE".into(),
                created_at: ms_to_rfc3339(T_NOW - 60_000),
                updated_at: "zzzz".into(),
            }],
            transactions: vec![],
            settings: vec![],
            account_deletion_floor: HashMap::new(),
            captured_at: "2026-05-19T12:00:00Z".into(),
        };
        apply_remote_snapshot(&mut conn, &remote_garbage, &HashMap::new(), None).unwrap();
        assert_eq!(account_name(&conn, "a1"), "future"); // unchanged
    }

    #[test]
    fn control_tier_boundary_discriminates_from_display_tier() {
        let mut conn = fresh_db();
        seed_account(&mut conn, "a1", "old", &ms_to_rfc3339(T_NOW - 60_000));
        // now + 4 min (< 5 min control tol) → accepted (overwrite).
        let within = MintSnapshot {
            schema_version: 1,
            accounts: vec![AccountRow {
                id: "a1".into(),
                name: "within".into(),
                created_at: ms_to_rfc3339(T_NOW - 60_000),
                updated_at: ms_to_rfc3339(T_NOW + 4 * 60_000),
            }],
            transactions: vec![],
            settings: vec![],
            account_deletion_floor: HashMap::new(),
            captured_at: "2026-05-19T12:00:00Z".into(),
        };
        apply_remote_snapshot(&mut conn, &within, &HashMap::new(), Some(T_NOW)).unwrap();
        assert_eq!(account_name(&conn, "a1"), "within");
        // now + 10 min → rejected under the control tier (would be ACCEPTED at the
        // 30-min display tier — this is the tier discriminator).
        let tier_probe = MintSnapshot {
            schema_version: 1,
            accounts: vec![AccountRow {
                id: "a1".into(),
                name: "probe".into(),
                created_at: ms_to_rfc3339(T_NOW - 60_000),
                updated_at: ms_to_rfc3339(T_NOW + 10 * 60_000),
            }],
            transactions: vec![],
            settings: vec![],
            account_deletion_floor: HashMap::new(),
            captured_at: "2026-05-19T12:00:00Z".into(),
        };
        apply_remote_snapshot(&mut conn, &tier_probe, &HashMap::new(), Some(T_NOW)).unwrap();
        assert_eq!(account_name(&conn, "a1"), "within"); // unchanged — probe rejected
    }

    #[test]
    fn poison_deletion_floor_does_not_hard_delete_or_merge() {
        let mut conn = fresh_db();
        seed_account(&mut conn, "a1", "keepme", &ms_to_rfc3339(T_NOW - 60_000));
        let remote = MintSnapshot {
            schema_version: 1,
            accounts: vec![],
            transactions: vec![],
            settings: vec![],
            account_deletion_floor: {
                let mut m = HashMap::new();
                m.insert("a1".to_string(), "9999-12-31T23:59:59Z".to_string());
                m
            },
            captured_at: "2026-05-19T12:00:00Z".into(),
        };
        let merged =
            apply_remote_snapshot(&mut conn, &remote, &HashMap::new(), Some(T_NOW)).unwrap();
        // Account NOT hard-deleted; poison NOT merged into the floor.
        assert!(account_exists(&conn, "a1"));
        assert!(!merged.contains_key("a1"));
    }

    #[test]
    fn honest_deletion_floor_still_deletes() {
        // Control for the previous test: an in-tolerance floor stamp still deletes.
        let mut conn = fresh_db();
        seed_account(&mut conn, "a1", "deleteme", &ms_to_rfc3339(T_NOW - 60_000));
        let remote = MintSnapshot {
            schema_version: 1,
            accounts: vec![],
            transactions: vec![],
            settings: vec![],
            account_deletion_floor: {
                let mut m = HashMap::new();
                m.insert("a1".to_string(), ms_to_rfc3339(T_NOW - 30_000));
                m
            },
            captured_at: "2026-05-19T12:00:00Z".into(),
        };
        let merged =
            apply_remote_snapshot(&mut conn, &remote, &HashMap::new(), Some(T_NOW)).unwrap();
        assert!(!account_exists(&conn, "a1"));
        assert!(merged.contains_key("a1"));
    }

    // ── ZEB-848 PR #585 converge round 1 ──

    /// Fix A: a poison-rejected account that is NOT already local must skip its
    /// dependent transactions, so an in-range txn referencing the never-inserted
    /// account cannot FK-abort (and roll back) the WHOLE snapshot. A second
    /// honest account in the same snapshot proves the rest still applies.
    #[test]
    fn rejected_missing_account_skips_dependent_txn_no_fk_abort() {
        let mut conn = fresh_db();
        let remote = MintSnapshot {
            schema_version: 1,
            accounts: vec![
                AccountRow {
                    id: "a1".into(),
                    name: "POISON".into(),
                    created_at: ms_to_rfc3339(T_NOW - 60_000),
                    updated_at: "9999-12-31T23:59:59Z".into(), // poison → dropped, a1 never inserted
                },
                AccountRow {
                    id: "a2".into(),
                    name: "honest".into(),
                    created_at: ms_to_rfc3339(T_NOW - 60_000),
                    updated_at: ms_to_rfc3339(T_NOW - 1_000), // in-range → applied
                },
            ],
            transactions: vec![TransactionRow {
                id: "t1".into(),
                transaction_date: "2026-05-01".into(),
                amount: "10.00".into(),
                currency: "USD".into(),
                account_id: "a1".into(), // references the dropped account
                description: "orphan".into(),
                metadata: None,
                created_at: ms_to_rfc3339(T_NOW - 60_000),
                updated_at: ms_to_rfc3339(T_NOW - 1_000), // in-range on its own merit
                deleted_at: None,
            }],
            settings: vec![],
            account_deletion_floor: HashMap::new(),
            captured_at: "2026-05-19T12:00:00Z".into(),
        };
        // No error / rollback: the whole snapshot must apply atomically minus the
        // skipped rows.
        apply_remote_snapshot(&mut conn, &remote, &HashMap::new(), Some(T_NOW)).unwrap();
        assert!(
            !account_exists(&conn, "a1"),
            "poison account never inserted"
        );
        assert!(
            !tx_exists(&conn, "t1"),
            "dependent txn skipped (no FK abort)"
        );
        assert!(account_exists(&conn, "a2"), "honest account still applied");
    }

    /// Fix A: when the poison-rejected account DOES exist locally, its honest row
    /// survives (poison update dropped) and its in-range transaction still merges
    /// against the surviving local account — no FK problem, so `Applied` (not a
    /// skip) is the correct outcome.
    #[test]
    fn rejected_existing_account_still_merges_its_txn() {
        let mut conn = fresh_db();
        let local_ts = ms_to_rfc3339(T_NOW - 60_000);
        seed_account(&mut conn, "a1", "honest", &local_ts);
        let remote = MintSnapshot {
            schema_version: 1,
            accounts: vec![AccountRow {
                id: "a1".into(),
                name: "POISON".into(),
                created_at: local_ts.clone(),
                updated_at: "9999-12-31T23:59:59Z".into(), // poison update → dropped
            }],
            transactions: vec![TransactionRow {
                id: "t1".into(),
                transaction_date: "2026-05-01".into(),
                amount: "10.00".into(),
                currency: "USD".into(),
                account_id: "a1".into(),
                description: "merged".into(),
                metadata: None,
                created_at: local_ts.clone(),
                updated_at: ms_to_rfc3339(T_NOW - 1_000), // in-range
                deleted_at: None,
            }],
            settings: vec![],
            account_deletion_floor: HashMap::new(),
            captured_at: "2026-05-19T12:00:00Z".into(),
        };
        apply_remote_snapshot(&mut conn, &remote, &HashMap::new(), Some(T_NOW)).unwrap();
        // Poison update dropped: honest local name + updated_at survive.
        assert_eq!(account_name(&conn, "a1"), "honest");
        let a1_updated: String = conn
            .query_row("SELECT updated_at FROM accounts WHERE id = 'a1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(a1_updated, local_ts);
        // Its in-range txn still merged (existing local account → no FK problem).
        assert!(tx_exists(&conn, "t1"));
        assert_eq!(tx_amount(&conn, "t1"), "10.00");
    }

    /// Fix B: a non-UTC (`+05:00`) remote stamp whose INSTANT is within tolerance
    /// but whose STRING sorts above the local UTC stamp must not overwrite the
    /// local row — the canonical-UTC parse rejects the non-zero offset before it
    /// can win the raw-string LWW.
    #[test]
    fn non_utc_offset_remote_cannot_overwrite_utc_local() {
        let mut conn = fresh_db();
        // Honest local: UTC (`+00:00`) string, in-range.
        let local_ts = ms_to_rfc3339(T_NOW - 60_000);
        seed_account(&mut conn, "a1", "honest", &local_ts);
        // Remote poison: the SAME instant as `now`, re-encoded `+05:00`. Its
        // instant is within tolerance (the skew check alone would accept it), yet
        // its string ("…T23:…+05:00") sorts ABOVE the local "…+00:00" string and
        // would win a raw-string LWW — the exploit the canonical-UTC parse closes.
        let plus5 = chrono::FixedOffset::east_opt(5 * 3600).unwrap();
        let non_utc_ts = chrono::DateTime::from_timestamp_millis(T_NOW as i64)
            .unwrap()
            .with_timezone(&plus5)
            .to_rfc3339();
        // Precondition of the exploit: the non-UTC string does sort high.
        assert!(
            non_utc_ts.as_str() > local_ts.as_str(),
            "test fixture must reproduce the sort-above condition: {non_utc_ts} vs {local_ts}"
        );
        let remote = MintSnapshot {
            schema_version: 1,
            accounts: vec![AccountRow {
                id: "a1".into(),
                name: "POISON".into(),
                created_at: local_ts.clone(),
                updated_at: non_utc_ts,
            }],
            transactions: vec![],
            settings: vec![],
            account_deletion_floor: HashMap::new(),
            captured_at: "2026-05-19T12:00:00Z".into(),
        };
        apply_remote_snapshot(&mut conn, &remote, &HashMap::new(), Some(T_NOW)).unwrap();
        assert_eq!(account_name(&conn, "a1"), "honest");
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
        apply_remote_snapshot(&mut local, &remote, &HashMap::new(), None).unwrap();
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
        apply_remote_snapshot(&mut local, &remote, &HashMap::new(), None).unwrap();
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
        apply_remote_snapshot(&mut local, &remote, &HashMap::new(), None).unwrap();
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
        apply_remote_snapshot(&mut local, &remote, &HashMap::new(), None).unwrap();
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
        let floor_to_merge = apply_remote_snapshot(&mut local, &remote, &floor, None).unwrap();
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

        let floor_to_merge =
            apply_remote_snapshot(&mut local, &remote, &HashMap::new(), None).unwrap();

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

        let floor_to_merge =
            apply_remote_snapshot(&mut local, &remote, &local_floor, None).unwrap();

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
        apply_remote_snapshot(&mut local, &remote, &floor, None).unwrap();
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

    /// ZEB-805 r1: build a valid encrypted mint root-publish envelope whose
    /// blob is NOT in the store — the incident condition, on the zenoh path.
    fn absent_blob_wire(keys: &FleetKeySet) -> (Vec<u8>, crate::owner_state_types::ContentId) {
        let absent = crate::owner_state_types::ContentId::from_bytes([0xEE; 32]);
        let payload = crate::mint_sync_types::MintRootPublishPayload {
            root_cid: absent,
            at: crate::owner_state_types::Hlc {
                wall_ms: 1_700_000_000_000,
                logical: 0,
                // NOT the receiving device, or echo-suppression eats it at step 2.
                device_id: "remote-dev".into(),
            },
        };
        let mut payload_bytes = Vec::new();
        ciborium::into_writer(&payload, &mut payload_bytes).expect("payload encode");
        let wire = crate::owner_state_crypto::encrypt_root_publish(&keys.newest(), &payload_bytes)
            .expect("encrypt");
        (wire, absent)
    }

    /// ZEB-805 r2: every fetch fails with a transport ERROR rather than a miss.
    struct ErroringStore;
    #[async_trait::async_trait]
    impl crate::content_store::ContentStore for ErroringStore {
        async fn put(
            &self,
            _cid: crate::owner_state_types::ContentId,
            _blob: Vec<u8>,
        ) -> Result<(), crate::content_store::ContentStoreError> {
            Ok(())
        }
        async fn get(
            &self,
            cid: &crate::owner_state_types::ContentId,
        ) -> Result<Option<Vec<u8>>, crate::content_store::ContentStoreError> {
            self.get_with_budget(cid, std::time::Duration::from_millis(0))
                .await
        }
        async fn get_with_budget(
            &self,
            _cid: &crate::owner_state_types::ContentId,
            _budget: std::time::Duration,
        ) -> Result<Option<Vec<u8>>, crate::content_store::ContentStoreError> {
            Err(crate::content_store::ContentStoreError::Io(
                "injected zenoh transport failure".into(),
            ))
        }
    }

    fn retry_test_shared() -> (EngineShared, FleetKeySet) {
        retry_test_shared_with(Arc::new(crate::content_store::InMemoryStub::default()))
    }

    fn retry_test_shared_with(
        content_store: Arc<dyn crate::content_store::ContentStore>,
    ) -> (EngineShared, FleetKeySet) {
        // ZEB-845: default to a fresh (empty/identity) floor — callers that
        // need to observe the feed use retry_test_shared_with_floor below.
        retry_test_shared_with_floor(content_store, crate::hlc_adopt_floor::HlcAdoptFloor::new())
    }

    /// ZEB-845: variant of `retry_test_shared_with` that threads a
    /// caller-supplied adoption floor, so a test can observe `.observe()`
    /// calls fed by the inbound zenoh path via the SAME `EngineShared` the
    /// engine uses (rather than inventing parallel construction machinery).
    fn retry_test_shared_with_floor(
        content_store: Arc<dyn crate::content_store::ContentStore>,
        adopt_floor: crate::hlc_adopt_floor::HlcAdoptFloor,
    ) -> (EngineShared, FleetKeySet) {
        let keys = FleetKeySet::new(Arc::new(
            crate::owner_state_crypto::KeyTree::derive_at_epoch(&[7u8; 32], 0).expect("derive"),
        ));
        let shared = EngineShared {
            mint_db: Arc::new(std::sync::Mutex::new(fresh_db())),
            content_store,
            sync_state: Arc::new(TokioMutex::new(MintSyncState::default())),
            sync_state_path: None,
            cipher: Some(crate::device_dataset_file::test_cipher()),
            app_handle: None,
            adopt_floor,
        };
        (shared, keys)
    }

    /// ZEB-805 r1: a transient CAS miss on the zenoh path must schedule a
    /// bounded retry, not drop the publish.
    ///
    /// Before r1 this engine was the last of the three to treat a miss as
    /// terminal — `community_state_sync` and `fleet_sync` both retried, while
    /// mint logged and discarded the frame. The PR that fixed the other two
    /// claimed cross-engine consistency as its deliverable, so leaving one
    /// engine inconsistent was the gap a reviewer caught.
    ///
    /// The re-injected frame must be byte-identical (it re-enters the FULL
    /// pipeline, replay check included) and must carry a DECREMENTED budget —
    /// an off-by-one here is the difference between bounded and unbounded.
    ///
    /// `start_paused`: the 2 s delay costs no wall-clock.
    ///
    /// The re-injection is awaited by dropping this test's own sender and then
    /// `recv()`-ing bare, rather than wrapping a `timeout` around it. The
    /// spawned sleeper holds the only other sender, so the two outcomes are
    /// distinguished without an arbitrary duration constant: `Some` means a
    /// retry was scheduled and delivered, `None` means none was scheduled — the
    /// regression this guards — and it reports immediately instead of after a
    /// timeout that only ever fires on the failing path.
    #[tokio::test(start_paused = true)]
    async fn mint_zenoh_fetch_miss_schedules_a_bounded_retry() {
        let (shared, keys) = retry_test_shared();
        let (wire, _absent) = absent_blob_wire(&keys);
        let (tx, mut rx) = mpsc::channel::<(Vec<u8>, u8)>(8);
        let sem = Arc::new(Semaphore::new(8));

        process_inbound_zenoh(
            wire.clone(),
            &shared,
            &keys,
            "local-dev",
            std::path::Path::new("/nonexistent"),
            crate::content_store::fetch_retry::ATTEMPTS,
            &tx,
            &sem,
        )
        .await;

        drop(tx);
        let (got_wire, got_attempts) = rx
            .recv()
            .await
            .expect("a miss must schedule a retry, not drop the publish");
        assert_eq!(got_wire, wire, "the ORIGINAL frame is re-injected verbatim");
        assert_eq!(
            got_attempts,
            crate::content_store::fetch_retry::ATTEMPTS - 1,
            "the retry must carry a decremented budget"
        );
    }

    /// The budget is bounded: at zero remaining attempts the miss is terminal
    /// and nothing is re-injected. Without this, the arm above would be an
    /// unbounded retry loop under a publish flood.
    #[tokio::test(start_paused = true)]
    async fn mint_zenoh_fetch_miss_at_zero_budget_is_terminal() {
        let (shared, keys) = retry_test_shared();
        let (wire, _absent) = absent_blob_wire(&keys);
        let (tx, mut rx) = mpsc::channel::<(Vec<u8>, u8)>(8);
        let sem = Arc::new(Semaphore::new(8));

        process_inbound_zenoh(
            wire,
            &shared,
            &keys,
            "local-dev",
            std::path::Path::new("/nonexistent"),
            0,
            &tx,
            &sem,
        )
        .await;
        drop(tx);

        assert!(
            rx.recv().await.is_none(),
            "an exhausted budget must NOT re-inject"
        );
    }

    /// ZEB-805 r2: a transport ERROR on the fetch must retry exactly like a
    /// miss. Found by review after r1 shipped.
    ///
    /// r1 fixed this same asymmetry in `community_state_sync` — where a
    /// content-store error terminated while a miss retried — and introduced it
    /// here in the same commit, by keying mint's new retry on `MissingBlob`
    /// alone. A zenoh/event-loop failure surfaced as the catch-all `Other`,
    /// which the retry treated as deterministic, so the publish was dropped and
    /// the ledger stayed stale until someone published another root.
    ///
    /// The predicate now lives on `MintSyncError`, so this test also guards the
    /// classification rather than one call site's match.
    ///
    /// See `mint_zenoh_fetch_miss_schedules_a_bounded_retry` for why the
    /// re-injection is awaited without a `timeout`.
    #[tokio::test(start_paused = true)]
    async fn mint_zenoh_fetch_transport_error_retries_like_a_miss() {
        let (shared, keys) = retry_test_shared_with(Arc::new(ErroringStore));
        let (wire, _absent) = absent_blob_wire(&keys);
        let (tx, mut rx) = mpsc::channel::<(Vec<u8>, u8)>(8);
        let sem = Arc::new(Semaphore::new(8));

        process_inbound_zenoh(
            wire.clone(),
            &shared,
            &keys,
            "local-dev",
            std::path::Path::new("/nonexistent"),
            crate::content_store::fetch_retry::ATTEMPTS,
            &tx,
            &sem,
        )
        .await;

        drop(tx);
        let (got_wire, got_attempts) = rx
            .recv()
            .await
            .expect("a transport error must schedule a retry, not drop the publish");
        assert_eq!(got_wire, wire, "the ORIGINAL frame is re-injected verbatim");
        assert_eq!(
            got_attempts,
            crate::content_store::fetch_retry::ATTEMPTS - 1,
            "the retry must carry a decremented budget"
        );
    }

    /// The classification predicate itself: transient variants yield their cid,
    /// deterministic ones yield `None`. Pins the contract the retry depends on.
    #[test]
    fn transient_fetch_cid_separates_retryable_from_deterministic() {
        let cid = crate::owner_state_types::ContentId::from_bytes([0x11; 32]);
        assert_eq!(
            MintSyncError::MissingBlob(cid).transient_fetch_cid(),
            Some(cid)
        );
        assert_eq!(
            MintSyncError::BlobFetchFailed {
                cid,
                detail: "transport".into()
            }
            .transient_fetch_cid(),
            Some(cid)
        );
        for deterministic in [
            MintSyncError::Crypto("bad key".into()),
            MintSyncError::Cbor("bad decode".into()),
            MintSyncError::SchemaTooNew {
                remote: 9,
                local_max: 1,
            },
            MintSyncError::Other("misc".into()),
        ] {
            assert_eq!(
                deterministic.transient_fetch_cid(),
                None,
                "{deterministic:?} must NOT retry — the same bytes fail the same way"
            );
        }
    }

    // ---- ZEB-845: wire MintSyncEngine to the shared HLC adoption floor ----

    /// ZEB-845: build a valid encrypted root-publish envelope whose blob IS
    /// present in `content_store` — the "verified inbound apply" condition
    /// the adoption-floor feed gates on. Mirrors the encode/encrypt/put
    /// sequence in `publish_root_now_zenoh` steps 2-6, but lets the caller
    /// pick the sibling `device_id` and `wall_ms` directly (rather than
    /// minting them), so tests can control echo-suppression and the fed
    /// value precisely.
    async fn verified_sibling_root_wire(
        keys: &FleetKeySet,
        content_store: &Arc<dyn crate::content_store::ContentStore>,
        device_id: &str,
        wall_ms: u64,
    ) -> Vec<u8> {
        let snap = MintSnapshot {
            schema_version: 1,
            accounts: vec![],
            transactions: vec![],
            settings: vec![],
            account_deletion_floor: HashMap::new(),
            captured_at: "2026-08-01T00:00:00Z".into(),
        };
        let mut cbor = Vec::new();
        ciborium::into_writer(&snap, &mut cbor).expect("cbor encode");
        let kt = keys.newest();
        let lookup = crate::owner_state_crypto::space_lookup_key(&kt, b"mint-ledger-v1");
        let ciphertext =
            crate::owner_state_crypto::encrypt_entry(&kt, &lookup, &cbor).expect("encrypt entry");
        let root_cid =
            crate::owner_state_types::ContentId::from_bytes(blake3::hash(&ciphertext).into());
        content_store
            .put(root_cid, ciphertext)
            .await
            .expect("content_store put");

        let payload = crate::mint_sync_types::MintRootPublishPayload {
            root_cid,
            at: crate::owner_state_types::Hlc {
                wall_ms,
                logical: 0,
                device_id: device_id.into(),
            },
        };
        let mut payload_bytes = Vec::new();
        ciborium::into_writer(&payload, &mut payload_bytes).expect("payload encode");
        crate::owner_state_crypto::encrypt_root_publish(&kt, &payload_bytes)
            .expect("encrypt envelope")
    }

    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    /// A verified sibling mint-root apply (decrypt + decode + merge + record
    /// all succeed) must feed the node-wide adoption floor with the
    /// envelope's `at.wall_ms` — mirrors the community/channel-log/fleet
    /// feed sites (ZEB-790 §A).
    #[tokio::test]
    async fn verified_mint_root_apply_feeds_adopt_floor() {
        let floor = crate::hlc_adopt_floor::HlcAdoptFloor::new();
        let cs: Arc<dyn crate::content_store::ContentStore> =
            Arc::new(crate::content_store::InMemoryStub::default());
        let (shared, keys) = retry_test_shared_with_floor(cs.clone(), floor.clone());

        let now = now_ms();
        let remote_wall = now + 2_000; // ahead of now, inside the 5000ms CAP
        let wire = verified_sibling_root_wire(&keys, &cs, "remote-dev", remote_wall).await;

        handle_incoming_publish_zenoh(
            &wire,
            &shared,
            &keys,
            "local-dev",
            std::path::Path::new("/nonexistent"),
        )
        .await
        .expect("verified sibling root must apply cleanly");

        assert_eq!(
            floor.merged_now(now),
            remote_wall + 1,
            "verified mint-root apply must feed the adoption floor",
        );
    }

    /// Mint-side: once the floor has observed a high remote wall, a local
    /// `next_hlc_mint` call adopts it (clamped within the CAP) rather than
    /// minting off the raw local wall clock.
    #[tokio::test]
    async fn mint_after_observe_clamps_within_cap() {
        let floor = crate::hlc_adopt_floor::HlcAdoptFloor::new();
        let sync_state = Arc::new(TokioMutex::new(MintSyncState::default()));

        let remote_wall = now_ms() + 3_000;
        floor.observe(remote_wall);

        let minted = next_hlc_mint(&sync_state, "local-dev", &floor).await;
        assert!(
            minted.wall_ms > remote_wall,
            "local mint must adopt the fed remote wall: got {}, expected > {}",
            minted.wall_ms,
            remote_wall,
        );
    }

    /// Regression: mint-state row merge resolves purely by per-row
    /// `updated_at` LWW (`upsert_account_lww`). Feeding the adoption floor
    /// from the envelope's `at.wall_ms` must NOT leak into that comparison —
    /// a "poisoned" (far-future) envelope wall must not make a stale remote
    /// row win.
    #[tokio::test]
    async fn poisoned_envelope_wall_does_not_affect_row_merge_lww() {
        let mut conn = fresh_db();
        seed_account(&mut conn, "a1", "Local Wins", "2026-06-01T00:00:00Z");
        let mint_db = Arc::new(std::sync::Mutex::new(conn));
        let cs: Arc<dyn crate::content_store::ContentStore> =
            Arc::new(crate::content_store::InMemoryStub::default());
        let keys = FleetKeySet::new(Arc::new(
            crate::owner_state_crypto::KeyTree::derive_at_epoch(&[7u8; 32], 0).expect("derive"),
        ));
        let shared = EngineShared {
            mint_db: mint_db.clone(),
            content_store: cs.clone(),
            sync_state: Arc::new(TokioMutex::new(MintSyncState::default())),
            sync_state_path: None,
            cipher: Some(crate::device_dataset_file::test_cipher()),
            app_handle: None,
            adopt_floor: crate::hlc_adopt_floor::HlcAdoptFloor::new(),
        };

        // Remote row is OLDER than local — must lose per-row LWW.
        let remote_snap = MintSnapshot {
            schema_version: 1,
            accounts: vec![AccountRow {
                id: "a1".into(),
                name: "Remote Should Lose".into(),
                created_at: "2026-05-01T00:00:00Z".into(),
                updated_at: "2026-05-01T00:00:00Z".into(),
            }],
            transactions: vec![],
            settings: vec![],
            account_deletion_floor: HashMap::new(),
            captured_at: "2026-05-01T00:00:00Z".into(),
        };
        let mut cbor = Vec::new();
        ciborium::into_writer(&remote_snap, &mut cbor).unwrap();
        let kt = keys.newest();
        let lookup = crate::owner_state_crypto::space_lookup_key(&kt, b"mint-ledger-v1");
        let ciphertext = crate::owner_state_crypto::encrypt_entry(&kt, &lookup, &cbor).unwrap();
        let root_cid =
            crate::owner_state_types::ContentId::from_bytes(blake3::hash(&ciphertext).into());
        cs.put(root_cid, ciphertext).await.unwrap();

        // Envelope `at.wall_ms` is "poisoned": far in the future, well past
        // anything the row-level LWW should ever see.
        let payload = crate::mint_sync_types::MintRootPublishPayload {
            root_cid,
            at: crate::owner_state_types::Hlc {
                wall_ms: 9_999_999_999_000,
                logical: 0,
                device_id: "remote-dev".into(),
            },
        };
        let mut payload_bytes = Vec::new();
        ciborium::into_writer(&payload, &mut payload_bytes).unwrap();
        let wire = crate::owner_state_crypto::encrypt_root_publish(&kt, &payload_bytes).unwrap();

        handle_incoming_publish_zenoh(
            &wire,
            &shared,
            &keys,
            "local-dev",
            std::path::Path::new("/nonexistent"),
        )
        .await
        .expect("verified apply must succeed");

        let name: String = mint_db
            .lock()
            .unwrap()
            .query_row("SELECT name FROM accounts WHERE id = 'a1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            name, "Local Wins",
            "row merge must resolve by per-row updated_at LWW, not the envelope's poisoned wall_ms"
        );
    }

    /// Rejection-inert #1: an echo-suppressed inbound root (own device_id,
    /// step 2) returns before the floor feed at step 6 and must not move it.
    #[tokio::test]
    async fn echo_suppressed_inbound_root_does_not_feed_adopt_floor() {
        let floor = crate::hlc_adopt_floor::HlcAdoptFloor::new();
        let cs: Arc<dyn crate::content_store::ContentStore> =
            Arc::new(crate::content_store::InMemoryStub::default());
        let (shared, keys) = retry_test_shared_with_floor(cs.clone(), floor.clone());

        let remote_wall = now_ms() + 2_000;
        // device_id == the receiver's own id => echo-suppressed at step 2.
        let wire = verified_sibling_root_wire(&keys, &cs, "local-dev", remote_wall).await;

        handle_incoming_publish_zenoh(
            &wire,
            &shared,
            &keys,
            "local-dev",
            std::path::Path::new("/nonexistent"),
        )
        .await
        .expect("echo-suppression returns Ok(()), not an error");

        assert_eq!(
            floor.merged_now(0),
            0,
            "an echo-suppressed inbound root must not feed the adoption floor"
        );
    }

    /// Rejection-inert #2: a CAS-miss (step 4, `MissingBlob`) returns `Err`
    /// before the merge/apply and before the floor feed at step 6 — reuses
    /// the existing ZEB-805 `absent_blob_wire` fixture rather than inventing
    /// new machinery.
    #[tokio::test]
    async fn cas_miss_inbound_root_does_not_feed_adopt_floor() {
        let floor = crate::hlc_adopt_floor::HlcAdoptFloor::new();
        let (shared, keys) = retry_test_shared_with_floor(
            Arc::new(crate::content_store::InMemoryStub::default()),
            floor.clone(),
        );
        // absent_blob_wire's at.wall_ms is 1_700_000_000_000 and device_id
        // "remote-dev" — far ahead of `merged_now(0)`'s baseline, so any
        // erroneous feed would be unmistakable.
        let (wire, _absent) = absent_blob_wire(&keys);

        let err = handle_incoming_publish_zenoh(
            &wire,
            &shared,
            &keys,
            "local-dev",
            std::path::Path::new("/nonexistent"),
        )
        .await
        .expect_err("a CAS miss must surface as an error, not a silent success");
        assert!(matches!(err, MintSyncError::MissingBlob(_)));

        assert_eq!(
            floor.merged_now(0),
            0,
            "a CAS-miss (rejected before the merge+feed) must not feed the adoption floor"
        );
    }

    /// Rejection-inert #3: a replay-skipped inbound root (step 3 — not
    /// strictly newer than the tracked HLC for that device) returns before
    /// the merge/apply and before the floor feed at step 6.
    ///
    /// This is the one rejection branch worth pinning specifically: the
    /// replay tracker is PERSISTED across a restart (`mint_sync_persist`)
    /// but the adoption floor is NOT (session-only by design — see
    /// `hlc_adopt_floor` module docs). So a fresh-boot floor starts at 0
    /// while the tracker can already hold a high watermark for a device —
    /// if the feed were ever misplaced onto this branch, a replayed/stale
    /// frame carrying `0 < W' <= tracked_W` would wrongly advance that
    /// fresh floor. Seed the tracker directly (bypassing a real apply) to
    /// simulate exactly that post-restart shape.
    #[tokio::test]
    async fn replay_skipped_inbound_root_does_not_feed_adopt_floor() {
        let floor = crate::hlc_adopt_floor::HlcAdoptFloor::new();
        let cs: Arc<dyn crate::content_store::ContentStore> =
            Arc::new(crate::content_store::InMemoryStub::default());
        let (shared, keys) = retry_test_shared_with_floor(cs.clone(), floor.clone());

        let now = now_ms();
        let tracked_wall = now + 5_000;
        {
            let mut st = shared.sync_state.lock().await;
            st.replay_tracker.insert(
                "remote-dev".to_string(),
                crate::owner_state_types::Hlc {
                    wall_ms: tracked_wall,
                    logical: 0,
                    device_id: "remote-dev".into(),
                },
            );
        }

        // Lower wall than the tracked watermark, same device — not strictly
        // newer, so step 3 skips it as a duplicate/replay.
        let replayed_wall = now + 1_000;
        let wire = verified_sibling_root_wire(&keys, &cs, "remote-dev", replayed_wall).await;

        handle_incoming_publish_zenoh(
            &wire,
            &shared,
            &keys,
            "local-dev",
            std::path::Path::new("/nonexistent"),
        )
        .await
        .expect("a replay-skip returns Ok(()), not an error");

        assert_eq!(
            floor.merged_now(0),
            0,
            "a replay-skipped inbound root must not feed the adoption floor"
        );
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
        apply_remote_snapshot(&mut local, &remote, &HashMap::new(), None).unwrap();
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
