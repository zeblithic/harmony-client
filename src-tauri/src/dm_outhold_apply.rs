//! ZEB-418 SP2 P2 Task 5: dm-outhold apply sweeper — local CAS admit +
//! status-driven GC.
//!
//! When a `dm-outhold-v1` row replicates to this device its blob is admitted
//! into the **local CAS** (so this device can serve CidNotify fetch-backs and
//! build butler deposits for that DM). When the matching `OutboxEntry` is in a
//! terminal state (`Complete`/`Expired`) or absent (`None` — user-deleted or
//! not yet arrived on this replica), the row is GC'd. Only the dataset **row**
//! is removed; CAS copies are retained (spec D14 as amended: they are
//! legitimate DM-history cache referenced by the replicated `InboxEntry`).
//!
//! ## Trigger model
//!
//! The `dm-outhold-v1` `FleetSyncEngine`'s `on_applied` callback sends a
//! nudge via [`outhold_nudge_on_applied`]; [`run_dm_outhold_sweeper`] debounces
//! the burst and runs one sweep per burst, plus a startup sweep for rows that
//! arrived while this device was offline.
//!
//! ## Resurrection-tolerant GC
//!
//! A stale sibling's merge can re-insert a GC'd row (the insert-once merge
//! sees a missing key as new). The next sweep re-evaluates: if the outbox
//! entry is still terminal the row is removed again. `None`-status rows
//! (no matching `OutboxEntry` on this replica) get a grace window
//! ([`OUTHOLD_ORPHAN_GC_GRACE_MS`]) before GC: the outhold doc and the
//! outbox `OwnerState` ride DIFFERENT datasets with no cross-dataset
//! ordering guarantee, so a row can land here before its outbox entry does
//! — immediate GC would lose the held blob exactly when the originator
//! goes offline (the scenario P2 exists for). Orphan tracking is local and
//! in-memory; resurrection re-arms a fresh grace window and converges once
//! every sibling has GC'd the row.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{mpsc, Mutex};

use crate::dm_outhold::DmOutholdDoc;
use crate::owner_state_types::{ContentId, DeliveryStatus};

/// Debounce window — same value as the inbox ingest sweeper's window so
/// on_applied bursts coalesce into one sweep on both channels.
pub const OUTHOLD_SWEEP_DEBOUNCE_MS: u64 = crate::dm_inbox_ingest::INGEST_SWEEP_DEBOUNCE_MS;

/// Grace window before GC'ing an outhold row whose outbox entry is absent
/// (`outbox_status` → `None`) on this replica (ZEB-418 P2, PR #222 round 1).
///
/// The outhold doc (dm-outhold-v1) and the outbox `OwnerState` replicate on
/// DIFFERENT datasets with no cross-dataset ordering guarantee: a row can
/// land on a sibling before the matching outbox entry does. GC'ing
/// immediately in that window would drop the held blob exactly when the
/// originator goes offline — the scenario P2 exists for. Age-of-row can't
/// serve as the grace clock (a row can be days old when a reconnecting
/// device first sees it), so the sweeper tracks LOCAL first-seen-as-orphan
/// time per key instead.
///
/// The tracker is in-memory: a process restart restarts the grace clock,
/// which only errs in the safe direction (rows live longer, never shorter).
pub const OUTHOLD_ORPHAN_GC_GRACE_MS: u64 = 10 * 60 * 1000;

/// Adapter for `FleetSyncConfig::on_applied`: nudges the outhold sweeper.
/// `try_send` on a capacity-1 channel — the nudge is a level trigger, so
/// dropping extras when the buffer is full (a sweep is already pending) is
/// correct.
pub fn outhold_nudge_on_applied(nudge_tx: mpsc::Sender<()>) -> Arc<dyn Fn() + Send + Sync> {
    Arc::new(move || {
        let _ = nudge_tx.try_send(());
    })
}

// ── Injectable context ──────────────────────────────────────────────────────

/// Injectable context for [`sweep_once`]: CAS put + outbox-status query.
/// Production implements this over real `NodeState` handles; tests use a
/// stub with programmable responses.
#[async_trait]
pub trait DmOutholdCtx: Send + Sync {
    /// Insert `blob` into the local CAS under `cid`.  Idempotent by CID
    /// (content-addressed, so a duplicate put is a no-op in production).
    async fn cas_put(
        &self,
        cid: crate::owner_state_types::ContentId,
        blob: Vec<u8>,
    ) -> Result<(), String>;

    /// Return the current delivery status of the `OutboxEntry` whose
    /// `(space_id, message_cid)` matches the given pair, or `None` if no
    /// such entry exists on this replica (deleted, user-GC'd, or not yet
    /// replicated).
    ///
    /// Production: one short lock over `OwnerState`, linear scan of
    /// `state.outbox.values()` — the outbox is small and sweeps are
    /// debounced.
    async fn outbox_status(
        &self,
        space_id: &[u8; 16],
        message_cid: &crate::owner_state_types::ContentId,
    ) -> Option<crate::owner_state_types::DeliveryStatus>;
}

// ── Production impl ─────────────────────────────────────────────────────────

/// Production [`DmOutholdCtx`] over the real `start_node` handles.
pub struct ProdDmOutholdCtx {
    /// Runtime owner-state CRDT (`NodeState`'s `crdt_state` Arc).
    pub crdt_state: Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
    /// Shared CAS handle (`RuntimeContentStore` in production).
    pub content_store: Arc<dyn crate::content_store::ContentStore>,
}

#[async_trait]
impl DmOutholdCtx for ProdDmOutholdCtx {
    async fn cas_put(&self, cid: ContentId, blob: Vec<u8>) -> Result<(), String> {
        self.content_store
            .put(cid, blob)
            .await
            .map_err(|e| format!("cas put: {e}"))
    }

    async fn outbox_status(
        &self,
        space_id: &[u8; 16],
        message_cid: &ContentId,
    ) -> Option<DeliveryStatus> {
        let state = self.crdt_state.lock().await;
        state
            .outbox
            .values()
            .find(|e| e.space_id.0 == *space_id && e.message_cid.as_ref() == Some(message_cid))
            .map(|e| e.delivery_status)
    }
}

// ── Sweep stats (testable inner function) ────────────────────────────────────

/// Counters returned by one sweep pass; used in unit tests (callers assert
/// on these instead of peeking at private doc state directly).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepStats {
    /// Rows for which `cas_put` was called successfully.
    pub admitted: usize,
    /// Rows removed (terminal status, grace-expired orphan, OR bad key).
    pub gc_removed: usize,
    /// Rows left pending because `cas_put` returned an error.
    pub cas_error_retains: usize,
    /// Orphan rows (`outbox_status` → `None`) retained because they are
    /// still inside [`OUTHOLD_ORPHAN_GC_GRACE_MS`].
    pub orphan_retains: usize,
}

/// One sweep over the outhold doc.
///
/// **Lock discipline:** snapshots the doc under a SHORT lock, releases it,
/// processes each entry (calling potentially-async ctx methods), then
/// re-locks to apply removals.  This avoids holding the doc lock across
/// `ctx.*` awaits, which would invite deadlock when the ProdCtx takes
/// the `crdt_state` Mutex.
///
/// **Orphan tracking:** `orphan_first_seen` maps row key → the `now_ms` of
/// the first sweep that saw the row with `outbox_status == None`. Orphans
/// are retained until [`OUTHOLD_ORPHAN_GC_GRACE_MS`] elapses (cross-dataset
/// replication skew — see the constant's doc). The caller owns the map
/// across sweeps; [`run_dm_outhold_sweeper`] keeps one for its lifetime.
pub async fn sweep_once(
    doc: &Arc<Mutex<DmOutholdDoc>>,
    ctx: &dyn DmOutholdCtx,
    notify_dirty: &(dyn Fn() + Send + Sync),
    orphan_first_seen: &mut HashMap<String, u64>,
    now_ms: u64,
) -> SweepStats {
    // 1. Snapshot — hold the lock ONLY long enough to clone the entries.
    let snapshot: Vec<(String, crate::dm_outhold::DmOutholdEntry)> = {
        let guard = doc.lock().await;
        guard
            .entries
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    };

    let mut to_remove: Vec<String> = Vec::new();
    let mut stats = SweepStats::default();

    for (key, entry) in &snapshot {
        // 2. Parse key "{space_id_hex}:{message_cid_hex}".
        let parsed = parse_outhold_key(key);
        let (space_id_bytes, cid) = match parsed {
            Some(pair) => pair,
            None => {
                tracing::warn!(
                    key = %key,
                    "ZEB-418 outhold: unparseable key — GC'ing corrupt row"
                );
                to_remove.push(key.clone());
                stats.gc_removed += 1;
                continue;
            }
        };

        // 3. Query outbox status — no doc lock held here.
        let status = ctx.outbox_status(&space_id_bytes, &cid).await;

        match status {
            Some(DeliveryStatus::Pending) | Some(DeliveryStatus::Partial) => {
                // Status became known — drop any orphan tracking.
                orphan_first_seen.remove(key.as_str());
                // Live entry: admit blob into local CAS.
                match ctx.cas_put(cid, entry.storage_blob.clone()).await {
                    Ok(()) => {
                        stats.admitted += 1;
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            key = %key,
                            "ZEB-418 outhold: CAS put failed; row left pending for retry"
                        );
                        stats.cas_error_retains += 1;
                        // Leave the row — the next nudge/startup sweep retries
                        // (dirty-latch pattern mirrors dm_inbox_ingest).
                    }
                }
            }

            Some(DeliveryStatus::Complete) | Some(DeliveryStatus::Expired) => {
                // Status became known — drop any orphan tracking.
                orphan_first_seen.remove(key.as_str());
                // Terminal outbox entry — GC the row.
                to_remove.push(key.clone());
                stats.gc_removed += 1;
            }

            None => {
                // No matching OutboxEntry on this replica (user-deleted, or
                // the entry has not yet replicated here). The outhold doc
                // and the outbox OwnerState ride DIFFERENT datasets with no
                // cross-dataset ordering guarantee, so retain the row under
                // a LOCAL first-seen-as-orphan grace window before GC'ing
                // (see OUTHOLD_ORPHAN_GC_GRACE_MS).
                //
                // Resurrection via a sibling's re-merge re-arms a fresh
                // grace window on this replica, which is the safe
                // direction; the fleet converges once every sibling has
                // GC'd the row.
                match orphan_first_seen.get(key.as_str()) {
                    None => {
                        // First sweep that sees this row as an orphan.
                        orphan_first_seen.insert(key.clone(), now_ms);
                        stats.orphan_retains += 1;
                    }
                    // ZEB-791: `now_ms >= first` bounds the forward direction.
                    // `first` is stamped locally on the arm above, so only a
                    // backward clock step can exceed `now_ms` — but while it
                    // does, `saturating_sub` holds the grace window open
                    // forever and the orphan is never collected. Fail closed:
                    // fall through to the GC arm.
                    Some(&first)
                        if now_ms >= first && now_ms - first < OUTHOLD_ORPHAN_GC_GRACE_MS =>
                    {
                        stats.orphan_retains += 1;
                    }
                    Some(_) => {
                        // Grace expired with the status still unknown — GC.
                        to_remove.push(key.clone());
                        stats.gc_removed += 1;
                    }
                }
            }
        }
    }

    // 4. Apply removals under the lock (idempotent: a concurrent merge could
    //    have re-inserted a GC'd key, which will be re-evaluated on the next
    //    sweep).
    if !to_remove.is_empty() {
        let mut guard = doc.lock().await;
        for key in &to_remove {
            guard.entries.remove(key.as_str());
        }
        // Publish the removals so siblings learn the row is gone.
        notify_dirty();
    }

    // 5. Tracker hygiene: GC'd keys leave the tracker, and entries for rows
    //    that vanished via other paths (e.g. removed between sweeps) must
    //    not leak — prune to keys present in this sweep's snapshot.
    for key in &to_remove {
        orphan_first_seen.remove(key.as_str());
    }
    let snapshot_keys: std::collections::HashSet<&str> =
        snapshot.iter().map(|(k, _)| k.as_str()).collect();
    orphan_first_seen.retain(|k, _| snapshot_keys.contains(k.as_str()));

    stats
}

/// Parse "{space_id_hex(32 chars)}:{message_cid_hex(64 chars)}" back to
/// ([u8; 16], ContentId).  Returns None on any parse error.
fn parse_outhold_key(key: &str) -> Option<([u8; 16], ContentId)> {
    let (space_hex, cid_hex) = key.split_once(':')?;
    let space_bytes = hex::decode(space_hex).ok()?;
    let space_id: [u8; 16] = space_bytes.try_into().ok()?;
    let cid_bytes = hex::decode(cid_hex).ok()?;
    let cid_arr: [u8; 32] = cid_bytes.try_into().ok()?;
    Some((space_id, ContentId::from_bytes(cid_arr)))
}

// ── Public sweeper task ──────────────────────────────────────────────────────

/// The outhold-sweeper task: one startup sweep (rows that arrived while
/// this device was offline), then one debounced sweep per `on_applied`
/// nudge burst.  Exits when all nudge senders are dropped (engine shutdown).
///
/// ZEB-418 P2 Task 6: wire this in `start_node` alongside the dm-outhold
/// `FleetSyncEngine` construction:
///
/// ```text
/// let (nudge_tx, nudge_rx) = tokio::sync::mpsc::channel(1);
/// // FleetSyncConfig { on_applied: Some(outhold_nudge_on_applied(nudge_tx)), .. }
/// tokio::spawn(run_dm_outhold_sweeper(
///     Arc::clone(&dm_outhold_doc),
///     prod_ctx,
///     nudge_rx,
///     Arc::new(move || engine.notify_dirty()),
///     Duration::from_millis(OUTHOLD_SWEEP_DEBOUNCE_MS),
/// ));
/// // Shutdown: drop nudge_tx → recv() yields None → task exits.
/// ```
pub async fn run_dm_outhold_sweeper(
    doc: Arc<Mutex<DmOutholdDoc>>,
    ctx: Arc<dyn DmOutholdCtx>,
    mut nudge_rx: mpsc::Receiver<()>,
    notify_dirty: Arc<dyn Fn() + Send + Sync>,
    debounce: Duration,
) {
    // Orphan grace tracker, owned across the startup sweep and the nudge
    // loop (in-memory by design — see OUTHOLD_ORPHAN_GC_GRACE_MS).
    let mut orphan_first_seen: HashMap<String, u64> = HashMap::new();
    let wall_now_ms = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    };

    // Startup sweep: process rows deposited while this device was offline.
    sweep_once(
        &doc,
        ctx.as_ref(),
        notify_dirty.as_ref(),
        &mut orphan_first_seen,
        wall_now_ms(),
    )
    .await;

    while nudge_rx.recv().await.is_some() {
        // Debounce: let the rest of a merge burst land, then drain any
        // extra nudges so the burst coalesces into one sweep.
        tokio::time::sleep(debounce).await;
        while nudge_rx.try_recv().is_ok() {}
        sweep_once(
            &doc,
            ctx.as_ref(),
            notify_dirty.as_ref(),
            &mut orphan_first_seen,
            wall_now_ms(),
        )
        .await;
    }
    // recv() == None: all nudge senders dropped (engine shutdown).
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex as StdMutex;

    // ── Stub context ─────────────────────────────────────────────────────────

    struct StubCtx {
        /// space_id_hex:cid_hex → status (None = entry absent)
        status_map: StdMutex<HashMap<String, Option<DeliveryStatus>>>,
        /// Whether cas_put should fail.
        cas_fail: bool,
        /// CIDs successfully put.
        cas_puts: StdMutex<Vec<ContentId>>,
        /// Total cas_put call count (including failures).
        cas_call_count: AtomicUsize,
    }

    impl StubCtx {
        fn new(entries: impl IntoIterator<Item = (String, Option<DeliveryStatus>)>) -> Self {
            Self {
                status_map: StdMutex::new(entries.into_iter().collect()),
                cas_fail: false,
                cas_puts: StdMutex::new(Vec::new()),
                cas_call_count: AtomicUsize::new(0),
            }
        }

        fn with_cas_fail(mut self) -> Self {
            self.cas_fail = true;
            self
        }

        fn cas_puts(&self) -> Vec<ContentId> {
            self.cas_puts.lock().unwrap().clone()
        }

        fn cas_calls(&self) -> usize {
            self.cas_call_count.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl DmOutholdCtx for StubCtx {
        async fn cas_put(&self, cid: ContentId, _blob: Vec<u8>) -> Result<(), String> {
            self.cas_call_count.fetch_add(1, Ordering::SeqCst);
            if self.cas_fail {
                return Err("simulated CAS failure".into());
            }
            self.cas_puts.lock().unwrap().push(cid);
            Ok(())
        }

        async fn outbox_status(
            &self,
            space_id: &[u8; 16],
            message_cid: &ContentId,
        ) -> Option<DeliveryStatus> {
            let key = format!(
                "{}:{}",
                hex::encode(space_id),
                hex::encode(message_cid.to_bytes())
            );
            self.status_map.lock().unwrap().get(&key).copied().flatten()
        }
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    const SPACE: [u8; 16] = [0x11u8; 16];
    const CID_BYTES: [u8; 32] = [0x22u8; 32];

    fn make_doc_with_one_entry() -> (Arc<Mutex<DmOutholdDoc>>, String, ContentId) {
        let cid = ContentId::from_bytes(CID_BYTES);
        let key = DmOutholdDoc::key(&SPACE, &CID_BYTES);
        let entry = crate::dm_outhold::DmOutholdEntry {
            storage_blob: vec![0xAA, 0xBB],
            space_id: SPACE,
            created_at: crate::owner_state_types::Hlc {
                wall_ms: 1000,
                logical: 0,
                device_id: "dev".into(),
            },
        };
        let mut doc = DmOutholdDoc::default();
        doc.entries.insert(key.clone(), entry);
        (Arc::new(Mutex::new(doc)), key, cid)
    }

    fn stub_key(space: &[u8; 16], cid_bytes: &[u8; 32]) -> String {
        format!("{}:{}", hex::encode(space), hex::encode(cid_bytes))
    }

    fn noop_notify() -> Arc<dyn Fn() + Send + Sync> {
        Arc::new(|| {})
    }

    fn counting_notify() -> (Arc<dyn Fn() + Send + Sync>, Arc<AtomicUsize>) {
        let count = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&count);
        (
            Arc::new(move || {
                c.fetch_add(1, Ordering::SeqCst);
            }),
            count,
        )
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    /// A pending outbox entry: cas_put called once with the right cid;
    /// row is retained.
    #[tokio::test]
    async fn sweep_inserts_blob_for_pending_entry() {
        let (doc, key, cid) = make_doc_with_one_entry();
        let ctx = StubCtx::new([(stub_key(&SPACE, &CID_BYTES), Some(DeliveryStatus::Pending))]);

        let stats = sweep_once(
            &doc,
            &ctx,
            noop_notify().as_ref(),
            &mut HashMap::new(),
            1_000,
        )
        .await;

        assert_eq!(stats.admitted, 1);
        assert_eq!(stats.gc_removed, 0);
        assert_eq!(stats.cas_error_retains, 0);

        // cas_put called with the correct CID
        assert_eq!(ctx.cas_puts(), vec![cid]);

        // Row retained (outbox still live)
        assert!(doc.lock().await.entries.contains_key(&key));
    }

    /// Partial delivery status should also keep the row and admit the blob.
    #[tokio::test]
    async fn sweep_inserts_blob_for_partial_entry() {
        let (doc, key, cid) = make_doc_with_one_entry();
        let ctx = StubCtx::new([(stub_key(&SPACE, &CID_BYTES), Some(DeliveryStatus::Partial))]);

        let stats = sweep_once(
            &doc,
            &ctx,
            noop_notify().as_ref(),
            &mut HashMap::new(),
            1_000,
        )
        .await;
        assert_eq!(stats.admitted, 1);
        assert_eq!(ctx.cas_puts(), vec![cid]);
        assert!(doc.lock().await.entries.contains_key(&key));
    }

    /// Complete outbox entry → row removed, no cas_put, notify_dirty fired.
    #[tokio::test]
    async fn sweep_gcs_row_when_outbox_complete() {
        let (doc, key, _cid) = make_doc_with_one_entry();
        let ctx = StubCtx::new([(stub_key(&SPACE, &CID_BYTES), Some(DeliveryStatus::Complete))]);
        let (notify, count) = counting_notify();

        let stats = sweep_once(&doc, &ctx, notify.as_ref(), &mut HashMap::new(), 1_000).await;
        assert_eq!(stats.gc_removed, 1);
        assert_eq!(stats.admitted, 0);
        assert_eq!(ctx.cas_calls(), 0, "no cas_put for terminal entry");
        assert!(!doc.lock().await.entries.contains_key(&key));
        assert_eq!(count.load(Ordering::SeqCst), 1, "notify_dirty fired once");
    }

    /// Expired outbox entry → row GC'd.
    #[tokio::test]
    async fn sweep_gcs_row_when_outbox_expired() {
        let (doc, key, _cid) = make_doc_with_one_entry();
        let ctx = StubCtx::new([(stub_key(&SPACE, &CID_BYTES), Some(DeliveryStatus::Expired))]);

        let stats = sweep_once(
            &doc,
            &ctx,
            noop_notify().as_ref(),
            &mut HashMap::new(),
            1_000,
        )
        .await;
        assert_eq!(stats.gc_removed, 1);
        assert!(!doc.lock().await.entries.contains_key(&key));
    }

    /// Missing outbox entry (None) → the first sweep retains the row under
    /// the orphan grace window; a sweep at/after the window's edge GC's it.
    #[tokio::test]
    async fn sweep_gcs_row_when_outbox_missing() {
        let (doc, key, _cid) = make_doc_with_one_entry();
        // status_map has no entry for this key → outbox_status returns None
        let ctx = StubCtx::new([]);
        let mut orphans = HashMap::new();

        // Phase 1: first sight of the orphan — retained, tracked.
        let stats = sweep_once(&doc, &ctx, noop_notify().as_ref(), &mut orphans, 1_000).await;
        assert_eq!(stats.orphan_retains, 1, "first sight retains under grace");
        assert_eq!(stats.gc_removed, 0);
        assert!(doc.lock().await.entries.contains_key(&key));
        assert!(orphans.contains_key(&key), "orphan is tracked");

        // Phase 2: grace elapsed, status still unknown — GC'd.
        let stats2 = sweep_once(
            &doc,
            &ctx,
            noop_notify().as_ref(),
            &mut orphans,
            1_000 + OUTHOLD_ORPHAN_GC_GRACE_MS,
        )
        .await;
        assert_eq!(stats2.gc_removed, 1, "grace expired → GC");
        assert_eq!(stats2.orphan_retains, 0);
        assert!(!doc.lock().await.entries.contains_key(&key));
        assert!(!orphans.contains_key(&key), "GC'd key leaves the tracker");
    }

    /// Replication-skew resolution: the row arrives BEFORE its outbox entry
    /// (sweep 1 → None → retained); the outbox entry then lands (Pending) —
    /// sweep 2 admits the blob and clears the orphan tracking, so a far-
    /// future sweep 3 does NOT GC the row.
    #[tokio::test]
    async fn sweep_orphan_resolves_when_outbox_entry_arrives() {
        let (doc, key, cid) = make_doc_with_one_entry();
        let ctx = StubCtx::new([]); // outbox entry not replicated yet
        let mut orphans = HashMap::new();

        // Sweep 1: orphan — retained under grace, no CAS activity.
        let stats = sweep_once(&doc, &ctx, noop_notify().as_ref(), &mut orphans, 1_000).await;
        assert_eq!(stats.orphan_retains, 1);
        assert_eq!(ctx.cas_calls(), 0);
        assert!(doc.lock().await.entries.contains_key(&key));

        // The outbox entry replicates here: status becomes Pending.
        ctx.status_map
            .lock()
            .unwrap()
            .insert(stub_key(&SPACE, &CID_BYTES), Some(DeliveryStatus::Pending));

        // Sweep 2: blob admitted to CAS; orphan tracking cleared.
        let stats2 = sweep_once(&doc, &ctx, noop_notify().as_ref(), &mut orphans, 2_000).await;
        assert_eq!(stats2.admitted, 1);
        assert_eq!(ctx.cas_puts(), vec![cid]);
        assert!(
            !orphans.contains_key(&key),
            "known status clears the orphan tracker"
        );

        // Sweep 3 (far future): status is Pending → never GC'd, even way
        // past any grace clock that might have leaked.
        let stats3 = sweep_once(
            &doc,
            &ctx,
            noop_notify().as_ref(),
            &mut orphans,
            2_000 + 100 * OUTHOLD_ORPHAN_GC_GRACE_MS,
        )
        .await;
        assert_eq!(stats3.gc_removed, 0, "Pending row is never orphan-GC'd");
        assert!(doc.lock().await.entries.contains_key(&key));
    }

    /// ZEB-791: an orphan whose recorded `first_seen` sits AHEAD of `now_ms`
    /// must still be GC-able.
    ///
    /// `first_seen` is written locally by a previous sweep, so the only way it
    /// exceeds `now_ms` is a backward local clock step. While it does,
    /// `saturating_sub` reported 0ms elapsed, `0 < GRACE` always held, and the
    /// row was retained on every future sweep — the grace window never closed.
    #[tokio::test]
    async fn sweep_gcs_orphan_whose_first_seen_is_in_the_future() {
        let (doc, key, _cid) = make_doc_with_one_entry();
        let ctx = StubCtx::new([]); // status still unknown → orphan
        let mut orphans = HashMap::new();
        // A previous sweep stamped the orphan at a high clock value...
        orphans.insert(key.clone(), 10 * OUTHOLD_ORPHAN_GC_GRACE_MS);
        // ...and the clock has since stepped back well below it.
        let stats = sweep_once(&doc, &ctx, noop_notify().as_ref(), &mut orphans, 1_000).await;
        assert_eq!(
            stats.gc_removed, 1,
            "a future-stamped orphan must not hold the grace window open forever"
        );
        assert!(!doc.lock().await.entries.contains_key(&key));
    }

    /// Failing cas_put → row retained; a second sweep retries (counter == 2).
    #[tokio::test]
    async fn sweep_retries_row_on_cas_failure() {
        let (doc, key, _cid) = make_doc_with_one_entry();
        let ctx = Arc::new(
            StubCtx::new([(stub_key(&SPACE, &CID_BYTES), Some(DeliveryStatus::Pending))])
                .with_cas_fail(),
        );
        let notify = noop_notify();

        // First sweep: cas_put fails → row retained
        let stats = sweep_once(
            &doc,
            ctx.as_ref(),
            notify.as_ref(),
            &mut HashMap::new(),
            1_000,
        )
        .await;
        assert_eq!(stats.cas_error_retains, 1);
        assert_eq!(stats.gc_removed, 0);
        assert_eq!(ctx.cas_calls(), 1);
        assert!(doc.lock().await.entries.contains_key(&key));

        // Second sweep: still failing, still retained; total calls == 2
        let stats2 = sweep_once(
            &doc,
            ctx.as_ref(),
            notify.as_ref(),
            &mut HashMap::new(),
            2_000,
        )
        .await;
        assert_eq!(stats2.cas_error_retains, 1);
        assert_eq!(ctx.cas_calls(), 2);
        assert!(doc.lock().await.entries.contains_key(&key));
    }

    /// Bad key → GC'd with a warn; no ctx calls; notify_dirty fired.
    #[tokio::test]
    async fn sweep_gcs_unparseable_key() {
        let cid = ContentId::from_bytes(CID_BYTES);
        let bad_key = "not-a-valid-key".to_string();
        let entry = crate::dm_outhold::DmOutholdEntry {
            storage_blob: vec![1, 2, 3],
            space_id: SPACE,
            created_at: crate::owner_state_types::Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "d".into(),
            },
        };
        let mut raw_doc = DmOutholdDoc::default();
        raw_doc.entries.insert(bad_key.clone(), entry);
        let doc = Arc::new(Mutex::new(raw_doc));
        let ctx = StubCtx::new([(stub_key(&SPACE, &CID_BYTES), Some(DeliveryStatus::Pending))]);
        let (notify, count) = counting_notify();

        let stats = sweep_once(&doc, &ctx, notify.as_ref(), &mut HashMap::new(), 1_000).await;
        assert_eq!(stats.gc_removed, 1);
        assert_eq!(ctx.cas_calls(), 0, "corrupt key must not trigger ctx calls");
        assert!(!doc.lock().await.entries.contains_key(&bad_key));
        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "notify_dirty fired for bad-key removal"
        );

        // Suppress unused-variable warning
        let _ = cid;
    }

    /// notify_dirty fires exactly when rows were removed; a pure-admit
    /// sweep (Pending/Partial, no removals) must NOT call notify_dirty.
    #[tokio::test]
    async fn gc_publishes_via_notify_dirty() {
        // Scenario 1: admit-only (Pending) → no notify_dirty
        let (doc_admit, _key_admit, _) = make_doc_with_one_entry();
        let ctx_admit =
            StubCtx::new([(stub_key(&SPACE, &CID_BYTES), Some(DeliveryStatus::Pending))]);
        let (notify_admit, count_admit) = counting_notify();
        sweep_once(
            &doc_admit,
            &ctx_admit,
            notify_admit.as_ref(),
            &mut HashMap::new(),
            1_000,
        )
        .await;
        assert_eq!(
            count_admit.load(Ordering::SeqCst),
            0,
            "admit-only sweep must not call notify_dirty"
        );

        // Scenario 2: GC (Complete) → notify_dirty fired
        let (doc_gc, _key_gc, _) = make_doc_with_one_entry();
        let ctx_gc = StubCtx::new([(stub_key(&SPACE, &CID_BYTES), Some(DeliveryStatus::Complete))]);
        let (notify_gc, count_gc) = counting_notify();
        sweep_once(
            &doc_gc,
            &ctx_gc,
            notify_gc.as_ref(),
            &mut HashMap::new(),
            1_000,
        )
        .await;
        assert_eq!(
            count_gc.load(Ordering::SeqCst),
            1,
            "GC sweep must call notify_dirty exactly once"
        );
    }

    /// outhold_nudge_on_applied is a non-blocking level trigger (like
    /// ingest_nudge_on_applied): a full buffer drops extras without
    /// blocking or panicking.
    #[tokio::test]
    async fn outhold_nudge_on_applied_is_nonblocking_level_trigger() {
        let (tx, mut rx) = mpsc::channel(1);
        let nudge = outhold_nudge_on_applied(tx);
        nudge();
        nudge(); // buffer full — must not panic or block
        assert_eq!(rx.recv().await, Some(()));
        assert!(rx.try_recv().is_err(), "extras coalesced into one nudge");
    }

    /// Startup sweep fires without a nudge; a subsequent nudge triggers a
    /// debounced second sweep.  Validates the run_dm_outhold_sweeper loop
    /// shape under paused time.
    #[tokio::test(start_paused = true)]
    async fn startup_sweep_and_nudge_sweep() {
        let (doc, key, _cid) = make_doc_with_one_entry();
        let ctx = Arc::new(StubCtx::new([(
            stub_key(&SPACE, &CID_BYTES),
            Some(DeliveryStatus::Pending),
        )]));

        let dirty = Arc::new(AtomicUsize::new(0));
        let notify_dirty: Arc<dyn Fn() + Send + Sync> = {
            let d = Arc::clone(&dirty);
            Arc::new(move || {
                d.fetch_add(1, Ordering::SeqCst);
            })
        };

        let (nudge_tx, nudge_rx) = mpsc::channel(1);

        let handle = tokio::spawn(run_dm_outhold_sweeper(
            Arc::clone(&doc),
            Arc::clone(&ctx) as Arc<dyn DmOutholdCtx>,
            nudge_rx,
            notify_dirty,
            Duration::from_millis(OUTHOLD_SWEEP_DEBOUNCE_MS),
        ));

        // Startup sweep must have run (cas_put called) without any nudge.
        // Under paused time, await for one yield cycle.
        for _ in 0..10_000 {
            if ctx.cas_calls() >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(ctx.cas_calls() >= 1, "startup sweep must admit the row");
        assert!(
            doc.lock().await.entries.contains_key(&key),
            "Pending row retained"
        );

        // A nudge triggers a debounced follow-up sweep.
        let calls_before = ctx.cas_calls();
        nudge_tx.send(()).await.expect("sweeper alive");
        for _ in 0..10_000 {
            if ctx.cas_calls() > calls_before {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(ctx.cas_calls() > calls_before, "nudge sweep ran");

        // Dropping the last nudge sender shuts down cleanly.
        drop(nudge_tx);
        tokio::time::timeout(Duration::from_secs(30), handle)
            .await
            .expect("sweeper must exit when nudge senders drop")
            .expect("sweeper must not panic");
    }

    /// End-to-end engine-wiring proof (ZEB-418 P2 Task 6): a real
    /// `FleetSyncEngine<DmOutholdDoc>` configured exactly as `start_node`
    /// configures it (DmOutholdPersist sink, `merge_from` merger,
    /// `publish_seen: true`, lookup tag `b"dm-outhold-v1"`, `on_applied` =
    /// the outhold nudge) must emit an outbound wire frame on the publisher
    /// channel when a local hold write is followed by `notify_dirty` +
    /// `flush_now`. Mirrors
    /// `dm_inbox_ingest::dm_inbox_engine_publishes_on_local_write`
    /// site-for-site.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dm_outhold_engine_publishes_on_local_write() {
        use crate::content_store::{ContentStore, InMemoryStub};
        use crate::dm_outhold_persist::DmOutholdPersist;
        use crate::fleet_sync::{FleetSyncConfig, FleetSyncEngine, Merger, DEFAULT_DEBOUNCE_MS};
        use crate::owner_state_crypto::KeyTree;

        let dir = tempfile::tempdir().unwrap();
        let kt = Arc::new(KeyTree::derive(&[0x55u8; 32]).expect("derive kt"));
        let doc = Arc::new(Mutex::new(DmOutholdDoc::default()));
        let tracker = Arc::new(Mutex::new(harmony_crdt_sync::ReplayTracker::new(
            "dev-A".to_string(),
        )));
        let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(64);
        let (_in_tx, in_rx) = mpsc::channel::<Vec<u8>>(64);
        let cas: Arc<dyn ContentStore> = Arc::new(InMemoryStub::default());
        let merger: Merger<DmOutholdDoc> = Arc::new(|local, remote| local.merge_from(remote));
        // The nudge channel exactly as start_node wires it (the receiver
        // half would feed `run_dm_outhold_sweeper`; this test proves the
        // ENGINE wiring, so the rx is simply held).
        let (nudge_tx, _nudge_rx) = mpsc::channel::<()>(1);

        let engine = FleetSyncEngine::<DmOutholdDoc>::new(FleetSyncConfig {
            keys: crate::owner_state_crypto::FleetKeySet::new(kt),
            device_id: "dev-A".to_string(),
            state: Arc::clone(&doc),
            merger,
            replay_tracker: Arc::clone(&tracker),
            content_store: cas,
            publisher_tx: out_tx,
            subscriber_rx: in_rx,
            persist: Arc::new(DmOutholdPersist {
                doc_path: dir.path().join("dm_outhold.cbor"),
                replay_path: dir.path().join("dm_outhold_replay.cbor"),
            }),
            lookup_key_tag: b"dm-outhold-v1",
            debounce_ms: DEFAULT_DEBOUNCE_MS,
            publish_seen: true,
            on_applied: Some(outhold_nudge_on_applied(nudge_tx)),
            sibling_acks: Arc::new(Mutex::new(harmony_crdt_sync::MonotoneMap::new())),
        });

        // A local hold write (what send_dm's Task-3 hold path does under
        // the doc lock), then force the engine to publish.
        {
            let key = DmOutholdDoc::key(&SPACE, &CID_BYTES);
            let entry = crate::dm_outhold::DmOutholdEntry {
                storage_blob: vec![0xAA, 0xBB],
                space_id: SPACE,
                created_at: crate::owner_state_types::Hlc {
                    wall_ms: 1000,
                    logical: 0,
                    device_id: "dev-A".into(),
                },
            };
            doc.lock().await.entries.insert(key, entry);
        }
        engine.notify_dirty();
        engine.flush_now().await.unwrap();

        // The local write must have driven a (non-empty) publish frame
        // onto the outbound channel.
        let frame = tokio::time::timeout(Duration::from_secs(5), out_rx.recv())
            .await
            .expect("publish frame produced within 5s")
            .expect("publisher channel yielded Some(frame)");
        assert!(!frame.is_empty(), "published wire frame must be non-empty");

        let _ = engine.shutdown().await;
    }
}
