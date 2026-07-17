//! Generic per-owner replicated-dataset sync engine (ZEB-417 SP1).
//!
//! `FleetSyncEngine<S>` is the consumer-agnostic extraction of the
//! debounced-publish + replay-protected-merge engine that previously
//! lived (copy-pasted) in `owner_state_sync.rs` and `mint_sync.rs`. It
//! owns a tokio task running the debounce timer, the publish path, and
//! the inbound merge path; the dataset `S`, the merge function, and the
//! durability sink are all injected via `FleetSyncConfig<S>`.
//!
//! CRITICAL ordering — apply-before-advance: the inbound path performs a
//! READ-ONLY replay check, fetches + decrypts + decodes the blob, merges,
//! and ONLY THEN advances the replay tracker (mint's model). A failure
//! between the check and the apply leaves the tracker un-advanced, so the
//! peer's next publish is retried rather than silently lost. This is the
//! opposite of the donor `owner_state_sync` advance-before-apply scheme,
//! which is intentionally NOT replicated here.
//!
//! ## SP1↔SP2 seam (the Butler, ZEB-418)
//!
//! SP2 (the Butler) deposits into a fleet dataset through exactly two
//! operations and never reaches into replication:
//!   * `write(dataset, op)` — a consumer mutates the dataset's `S` under its
//!     lock, then calls `FleetSyncEngine::notify_dirty` to schedule the
//!     debounced publish/fan-out. (The Notes IPC commands are the reference
//!     `write` site.)
//!   * `FleetSyncEngine::list_online_devices` — the butler-set source. SP1
//!     derives it best-effort from the replay tracker (devices seen
//!     publishing recently); SP2 refines "online" with liveness/presence.

use crate::content_store::{ContentStore, ContentStoreError};
#[cfg(test)]
use crate::owner_state_crypto::KeyTree;
use crate::owner_state_crypto::{
    canonical_cbor_decode, canonical_cbor_encode, decrypt_entry, decrypt_root_publish,
    encrypt_entry, encrypt_root_publish, sealed::CanonicalPayloadSealed, space_lookup_key,
    CanonicalPayload, FleetKeySet,
};
use crate::owner_state_types::Hlc;
use harmony_content::cid::ContentId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Mutex, Notify};
use tokio::task::JoinHandle;

/// Default debounce window between a `notify_dirty` and the resulting
/// state-root publish. MUST equal `owner_state_sync::DEFAULT_DEBOUNCE_MS`
/// — small enough to feel near-instant to a human, large enough to
/// collapse keystroke-rate mutations.
pub const DEFAULT_DEBOUNCE_MS: u64 = 250;

/// Engine-local root-publish envelope. Superset of legacy RootPublishPayload:
/// `seen` is skip-if-empty so an empty-seen envelope encodes byte-identically.
/// All three keys (rc/at/sn) are 2 chars (same-length-keys canonical precondition).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetRootPublish {
    #[serde(rename = "rc")]
    pub root_cid: ContentId,
    #[serde(rename = "at")]
    pub at: Hlc,
    #[serde(rename = "sn", default, skip_serializing_if = "BTreeMap::is_empty")]
    pub seen: BTreeMap<String, Hlc>,
}

// ZEB-220 sealed CanonicalPayload registration. `RootPublishPayload` and the
// rest of the Phase 2 wire types are registered via the `impl_canonical!`
// macro in owner_state_types.rs; that macro is module-private, so we register
// `FleetRootPublish` with the same two impls the macro expands to (matching
// the manual `OwnerState` registration at the foot of owner_state_types.rs).
impl CanonicalPayloadSealed for FleetRootPublish {}
impl CanonicalPayload for FleetRootPublish {}

pub const MAX_DEVICES_PER_OWNER: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MergeOutcome {
    pub changed: bool,
}

/// Merge function: fold a decoded remote snapshot into local `S`,
/// reporting whether anything changed. Aliased to keep the boxed-closure
/// type out of struct fields (clippy::type_complexity).
pub type Merger<S> = Arc<dyn Fn(&mut S, S) -> MergeOutcome + Send + Sync>;

/// Errors surfaced by the generic engine. Mirrors the donor
/// `owner_state_sync::SyncError` shape but with a `Persist(String)`
/// variant (the durability sink is now an injected `FleetPersist` trait,
/// so its concrete error type is consumer-defined and flattened to a
/// string at the boundary).
#[derive(thiserror::Error, Debug)]
pub enum SyncError {
    #[error("cbor encode: {0}")]
    CborEncode(String),
    #[error("cbor decode: {0}")]
    CborDecode(String),
    #[error("crypto: {0}")]
    Crypto(String),
    #[error("content store: {0}")]
    ContentStore(#[from] ContentStoreError),
    #[error("transport channel closed")]
    TransportClosed,
    #[error("persist: {0}")]
    Persist(String),
}

/// Durability sink for the engine. The engine snapshots `state` and
/// `replay_tracker` under their async locks, then calls `persist` inside
/// a `spawn_blocking` so the (potentially fsync-heavy) write does not
/// stall the tokio worker. Consumers map their own persistence error
/// into `SyncError::Persist(_)`.
pub trait FleetPersist<S>: Send + Sync {
    fn persist(&self, state: &S, replay_tracker: &BTreeMap<String, Hlc>) -> Result<(), SyncError>;
}

/// Construction-time configuration for a `FleetSyncEngine<S>`. Every
/// collaborator (state, merger, durability sink, transport channels,
/// CAS) is injected so the engine itself is consumer-agnostic.
pub struct FleetSyncConfig<S> {
    /// Installed fleet KeyTrees (ZEB-668 S5). Publishes use the newest
    /// epoch; inbound decrypts try every installed epoch (dual-epoch read
    /// window during a fleet key rotation). Engines whose dataset never
    /// rotates (the fleet-keys carrier itself) pass a 1-set that is never
    /// swapped.
    pub keys: FleetKeySet,
    /// Local device id — the HLC source for our own publishes.
    pub device_id: String,
    /// Shared replicated dataset.
    pub state: Arc<Mutex<S>>,
    /// Merge a decoded remote snapshot into local state, reporting
    /// whether anything changed.
    pub merger: Merger<S>,
    /// Per-publisher last-accepted HLC (replay protection).
    pub replay_tracker: Arc<Mutex<BTreeMap<String, Hlc>>>,
    /// Content-addressed store (root blobs live here).
    pub content_store: Arc<dyn ContentStore>,
    /// Outbound wire frames (the engine's publisher side).
    pub publisher_tx: mpsc::Sender<Vec<u8>>,
    /// Inbound wire frames (the engine's subscriber side).
    pub subscriber_rx: mpsc::Receiver<Vec<u8>>,
    /// Durability sink.
    pub persist: Arc<dyn FleetPersist<S>>,
    /// Lookup-key tag for the single root blob in CAS.
    pub lookup_key_tag: &'static [u8],
    /// Debounce window in milliseconds.
    pub debounce_ms: u64,
    /// When true, publishes carry a `seen` digest (the publisher's view
    /// of every device's latest HLC) and inbound publishes record the
    /// sibling's acknowledgement of us into `sibling_acks`.
    pub publish_seen: bool,
    /// Optional callback fired after an inbound merge that actually
    /// changed local state.
    pub on_applied: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Per-sibling record of "their latest seen-of-me" HLC, populated
    /// from inbound `seen` digests when `publish_seen` is set.
    pub sibling_acks: Arc<Mutex<BTreeMap<String, Hlc>>>,
}

/// Generic per-owner sync engine. Construction spawns the internal task;
/// `shutdown().await` stops it cleanly with one final flush.
pub struct FleetSyncEngine<S: Send + 'static> {
    notify_dirty: Arc<Notify>,
    /// Set to `true` by `notify_dirty()`; cleared by the task after each
    /// publish. Prevents the shutdown path from emitting a spurious
    /// publish when the `Notify` permit was left over from before the
    /// most-recent actual publish.
    has_pending_dirty: Arc<AtomicBool>,
    flush_now_tx: mpsc::Sender<tokio::sync::oneshot::Sender<Result<(), SyncError>>>,
    persist_now_tx: mpsc::Sender<tokio::sync::oneshot::Sender<Result<(), SyncError>>>,
    /// Carries `Result<(), SyncError>` so the final publish + persist
    /// errors propagate to the caller rather than being silently
    /// swallowed by `()`.
    shutdown_tx: mpsc::Sender<tokio::sync::oneshot::Sender<Result<(), SyncError>>>,
    replay_tracker: Arc<Mutex<BTreeMap<String, Hlc>>>,
    sibling_acks: Arc<Mutex<BTreeMap<String, Hlc>>>,
    device_id: String,
    task: Mutex<Option<JoinHandle<()>>>,
    _s: std::marker::PhantomData<fn() -> S>,
}

/// Abort the internal task if the engine is dropped without an explicit
/// `shutdown().await`. The task only returns when it receives a shutdown
/// message; once the engine (and thus `shutdown_tx`) is gone, that select arm
/// is permanently disabled, so a plain drop would otherwise orphan the task —
/// e.g. an aborted boot that constructed some FleetSync engines before a later
/// step returned `Err` (ZEB-460). On the normal `shutdown().await` path the
/// task has already returned and the handle was taken, so this is a no-op.
impl<S: Send + 'static> Drop for FleetSyncEngine<S> {
    fn drop(&mut self) {
        if let Some(handle) = self.task.get_mut().take() {
            handle.abort();
        }
    }
}

/// ZEB-702 (Component B): object-safe seam for re-offering the current
/// dataset root when a transport link forms. `republish_dirty` delegates to
/// the engine's own `notify_dirty()`, so the re-offer rides the existing
/// debounce + publish path — byte-identical content, idempotent on receivers
/// (LWW/HLC merge). Task 3's transport-epoch listener holds a
/// `Vec<Arc<dyn RepublishDirty>>` over every owner-scoped dataset engine and
/// nudges each on the transport up-edge, closing the late-joiner hole where a
/// link that forms after the last publish would otherwise carry nothing until
/// the next local mutation. Implemented here for the generic
/// `FleetSyncEngine<S>` (covers fleet-net, dm-inbox, dm-outhold, owner-trust,
/// fleet-keys, owner-quorum-req, community-device-intro, notes, and the relay
/// datasets); owner-state's `owner_state_sync::SyncEngine` WRAPPER carries its
/// own delegating impl (the generic one cannot reach its private inner
/// engine), and the distinct `MintSyncEngine` likewise in `mint_sync.rs`.
pub trait RepublishDirty: Send + Sync {
    /// Schedule a debounced re-publish of the current dataset root.
    /// Non-blocking; coalesces with any pending dirty state.
    fn republish_dirty(&self);
}

impl<S> FleetSyncEngine<S>
where
    S: CanonicalPayload + serde::de::DeserializeOwned + Clone + Send + 'static,
{
    /// Construct the engine and spawn its internal task.
    pub fn new(config: FleetSyncConfig<S>) -> Self {
        let notify_dirty = Arc::new(Notify::new());
        let has_pending_dirty = Arc::new(AtomicBool::new(false));
        let (flush_now_tx, flush_now_rx) = mpsc::channel(8);
        let (persist_now_tx, persist_now_rx) = mpsc::channel(8);
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

        let replay_tracker = Arc::clone(&config.replay_tracker);
        let sibling_acks = Arc::clone(&config.sibling_acks);
        let device_id = config.device_id.clone();

        let task = tokio::spawn(internal_task(Ctx {
            keys: config.keys,
            device_id: config.device_id,
            state: config.state,
            merger: config.merger,
            replay_tracker: config.replay_tracker,
            content_store: config.content_store,
            publisher_tx: config.publisher_tx,
            subscriber_rx: config.subscriber_rx,
            persist: config.persist,
            lookup_key_tag: config.lookup_key_tag,
            debounce: Duration::from_millis(config.debounce_ms),
            publish_seen: config.publish_seen,
            on_applied: config.on_applied,
            sibling_acks: config.sibling_acks,
            notify_dirty: Arc::clone(&notify_dirty),
            has_pending_dirty: Arc::clone(&has_pending_dirty),
            flush_now_rx,
            persist_now_rx,
            shutdown_rx,
        }));

        FleetSyncEngine {
            notify_dirty,
            has_pending_dirty,
            flush_now_tx,
            persist_now_tx,
            shutdown_tx,
            replay_tracker,
            sibling_acks,
            device_id,
            task: Mutex::new(Some(task)),
            _s: std::marker::PhantomData,
        }
    }

    /// Hint that local state has mutated and a debounced publish should
    /// fire after `debounce_ms`. Non-blocking.
    pub fn notify_dirty(&self) {
        self.has_pending_dirty.store(true, Ordering::Release);
        self.notify_dirty.notify_one();
    }

    /// Force an immediate publish, bypassing the debounce window.
    /// Returns when the publish has been written to the outbound channel
    /// and any persistence flush has completed.
    ///
    /// Unconditionally publishes even when the engine has no pending
    /// dirty state — this differs from the implicit shutdown flush,
    /// which is gated on `has_pending_dirty`. The "force publish"
    /// semantics are intentional for callers that need a fence-style
    /// sync point (tests, explicit "sync now" UI). On an idle engine the
    /// publish carries an advanced HLC but identical content, so peers
    /// see one extra encrypt/decrypt round-trip — acceptable for the
    /// cases that opt in to this method.
    pub async fn flush_now(&self) -> Result<(), SyncError> {
        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        self.flush_now_tx
            .send(resp_tx)
            .await
            .map_err(|_| SyncError::TransportClosed)?;
        resp_rx.await.map_err(|_| SyncError::TransportClosed)?
    }

    /// Force an immediate durable persist WITHOUT publishing. Returns when the
    /// on-disk write has completed. Unlike `flush_now`, this never touches the
    /// network publish path, so durability cannot be starved by a stalled publish
    /// (e.g. no zenoh responder). Used by the durable-on-commit fences where the
    /// only requirement is that local state reach disk; any pending state-root
    /// publish still fires on the next debounce. Mirrors
    /// `community_state_sync::CommunitySyncEngine::persist_now`.
    pub async fn persist_now(&self) -> Result<(), SyncError> {
        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        self.persist_now_tx
            .send(resp_tx)
            .await
            .map_err(|_| SyncError::TransportClosed)?;
        resp_rx.await.map_err(|_| SyncError::TransportClosed)?
    }

    /// Stop the internal task, flushing any pending writes first. Must be
    /// called explicitly during graceful shutdown — `Drop` is best-effort
    /// only.
    ///
    /// Returns `Err(SyncError)` if the final publish or persist pass
    /// failed. Callers should log this rather than swallow it: a silent
    /// failure here means the very last delta a user made before quitting
    /// was not durably persisted.
    ///
    /// If the engine task was already gone (channel closed before our
    /// send landed), returns `Ok(())` — there was nothing to flush.
    ///
    /// Note: we DO NOT `handle.await` the JoinHandle. The internal task
    /// is spawned via `tokio::spawn` on whatever runtime called
    /// `FleetSyncEngine::new`; shutdown may be invoked from a different
    /// runtime. Awaiting a JoinHandle from a different runtime than the
    /// one it was spawned on is not part of tokio's documented contract
    /// and risks deadlocking under future tokio releases. The
    /// `resp_rx.await` already gives the flush-complete guarantee — the
    /// task sends on `resp_tx` as the LAST step before `return`, so
    /// dropping the JoinHandle just lets the task's final stack-pop
    /// happen on its own runtime.
    pub async fn shutdown(&self) -> Result<(), SyncError> {
        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        let result = if self.shutdown_tx.send(resp_tx).await.is_ok() {
            // Mirror `flush_now`'s error propagation. Three outcomes:
            //   Ok(Ok(()))    flush succeeded
            //   Ok(Err(e))    flush failed (publish or persist)
            //   Err(_)        oneshot cancelled — task panicked or
            //                 dropped resp_tx mid-flush. We surface this
            //                 as TransportClosed rather than silently
            //                 swallowing as Ok(()), since that would mask
            //                 exactly the failure mode this API surfaces.
            resp_rx.await.map_err(|_| SyncError::TransportClosed)?
        } else {
            // shutdown_tx.send failed — the receiver is gone, so the task
            // already exited (e.g. a previous shutdown). No flush to wait
            // on; treat as already-done.
            Ok(())
        };
        // Drop the JoinHandle without awaiting it (see doc above for why).
        let _ = self.task.lock().await.take();
        result
    }

    /// Device ids the engine has accepted a publish from, excluding our
    /// own device.
    pub async fn list_online_devices(&self) -> Vec<String> {
        self.replay_tracker
            .lock()
            .await
            .keys()
            .filter(|d| *d != &self.device_id)
            .cloned()
            .collect()
    }

    /// Count of siblings whose last `seen`-of-us is at least as new as our
    /// own latest published HLC — i.e. peers that have acknowledged our
    /// most-recent state. Returns 0 if we have not published yet.
    pub async fn synced_device_count(&self) -> usize {
        let my_latest = {
            let tracker = self.replay_tracker.lock().await;
            match tracker.get(&self.device_id) {
                Some(h) => h.clone(),
                None => return 0,
            }
        };
        let acks = self.sibling_acks.lock().await;
        acks.values()
            .filter(|seen_of_me| !my_latest.is_strictly_newer_than(seen_of_me))
            .count()
    }
}

/// ZEB-702 T2: delegation-only — `republish_dirty` is exactly `notify_dirty`,
/// so the transport-up-edge re-offer reuses the engine's existing debounced
/// publish path with no behavior change.
impl<S> RepublishDirty for FleetSyncEngine<S>
where
    S: CanonicalPayload + serde::de::DeserializeOwned + Clone + Send + 'static,
{
    fn republish_dirty(&self) {
        self.notify_dirty();
    }
}

/// Internal task context. Mirrors the donor `InternalCtx` but carries the
/// injected `merger`, `persist`, `lookup_key_tag`, `publish_seen`,
/// `on_applied`, and `sibling_acks` plus the generic dataset `S`.
struct Ctx<S> {
    keys: FleetKeySet,
    device_id: String,
    state: Arc<Mutex<S>>,
    merger: Merger<S>,
    replay_tracker: Arc<Mutex<BTreeMap<String, Hlc>>>,
    content_store: Arc<dyn ContentStore>,
    publisher_tx: mpsc::Sender<Vec<u8>>,
    subscriber_rx: mpsc::Receiver<Vec<u8>>,
    persist: Arc<dyn FleetPersist<S>>,
    lookup_key_tag: &'static [u8],
    debounce: Duration,
    publish_seen: bool,
    on_applied: Option<Arc<dyn Fn() + Send + Sync>>,
    sibling_acks: Arc<Mutex<BTreeMap<String, Hlc>>>,
    notify_dirty: Arc<Notify>,
    has_pending_dirty: Arc<AtomicBool>,
    flush_now_rx: mpsc::Receiver<tokio::sync::oneshot::Sender<Result<(), SyncError>>>,
    persist_now_rx: mpsc::Receiver<tokio::sync::oneshot::Sender<Result<(), SyncError>>>,
    shutdown_rx: mpsc::Receiver<tokio::sync::oneshot::Sender<Result<(), SyncError>>>,
}

async fn internal_task<S>(mut ctx: Ctx<S>)
where
    S: CanonicalPayload + serde::de::DeserializeOwned + Clone + Send + 'static,
{
    let mut next_wakeup: Option<Instant> = None;
    // Latched after we observe `None` on `subscriber_rx`. Without this,
    // select! polls a closed channel every iteration and the arm-pattern
    // `Some(bytes) = recv()` silently filters out None — inbound sync is
    // permanently dead with no surfaced signal. The latch lets us log
    // once and gate the arm off so the engine's other branches
    // (notify_dirty, flush_now, shutdown) keep working as a degraded
    // publish-only mode.
    let mut inbound_closed = false;

    // Pin the `Notified` future OUTSIDE the loop so its state persists
    // across iterations. `tokio::select!` polls every branch in each
    // iteration; when both `notified()` and another branch are Ready
    // simultaneously, select picks one and drops the &mut reference to
    // the others. If `notified()` was constructed inside the loop, that
    // drop discards the consumed permit and the wakeup is silently lost.
    // Pinning outside means the underlying `Notified` survives the
    // dropped &mut, retains its `Done(consumed)` state, and the next
    // iteration's poll returns `Ready` immediately — the permit's effect
    // is preserved regardless of which branch select picked.
    //
    // After we successfully observe the notification, we replace the
    // pinned future with a fresh `Notified` to start waiting again.
    let notify = Arc::clone(&ctx.notify_dirty);
    let notified = notify.notified();
    tokio::pin!(notified);

    loop {
        // Compute the sleep duration for the wakeup branch.
        let sleep_dur = next_wakeup
            .map(|t| t.saturating_duration_since(Instant::now()))
            .unwrap_or(Duration::from_secs(3600));

        tokio::select! {
            _ = notified.as_mut() => {
                // Extend (or arm) the debounce window on every dirty
                // signal. This is a sliding debounce: multiple rapid
                // calls reset the timer, collapsing to one publish
                // `debounce` after the last call in the burst.
                if ctx.has_pending_dirty.load(Ordering::Relaxed) {
                    next_wakeup = Some(Instant::now() + ctx.debounce);
                }
                // Replace the pinned future with a fresh one so we wait
                // for the next notification. Without this, subsequent
                // polls of the Done future return Ready in a tight loop.
                notified.set(notify.notified());
            }
            _ = tokio::time::sleep(sleep_dur), if next_wakeup.is_some() => {
                next_wakeup = None;
                // Use `swap` rather than `store(false)` so we can restore
                // the dirty bit if the publish itself fails. Otherwise a
                // transient publish error silently consumes the dirty
                // signal and the next `shutdown()`/wakeup misses the
                // pending-state guard.
                let was_dirty = ctx.has_pending_dirty.swap(false, Ordering::AcqRel);
                let pub_result = publish_root_now(&ctx).await;
                if let Err(e) = &pub_result {
                    tracing::warn!(error = %e, "publish_root_now failed");
                    if was_dirty {
                        // Re-arm so the next debounce wakeup / shutdown
                        // retries this publish instead of skipping it.
                        ctx.has_pending_dirty.store(true, Ordering::Release);
                    }
                }
                if let Err(e) = persist_now(&ctx).await {
                    tracing::warn!(error = %e, "persist_now failed");
                }
            }
            Some(resp_tx) = ctx.flush_now_rx.recv() => {
                // `flush_now` ALWAYS publishes, even on an idle engine (no
                // `has_pending_dirty` guard, unlike the shutdown arm).
                // This is intentional — see the public-facing doc on
                // `FleetSyncEngine::flush_now`. The cost is one extra put
                // + AEAD round-trip if the engine happens to be idle,
                // which is acceptable for tests and explicit "force
                // publish" callers.
                next_wakeup = None;
                // Same swap-and-restore pattern as the wakeup arm: if the
                // publish fails, preserve the dirty latch so the engine
                // retries on its next signal instead of losing work.
                let was_dirty = ctx.has_pending_dirty.swap(false, Ordering::AcqRel);
                let pub_result = publish_root_now(&ctx).await;
                if let Err(ref e) = pub_result {
                    tracing::warn!(error = %e, "flush_now publish_root_now failed");
                    if was_dirty {
                        ctx.has_pending_dirty.store(true, Ordering::Release);
                    }
                }
                let persist_result = persist_now(&ctx).await;
                let result = pub_result.and(persist_result);
                let _ = resp_tx.send(result);
            }
            Some(resp_tx) = ctx.persist_now_rx.recv() => {
                // Publish-INDEPENDENT durable persist (durable-on-commit fence).
                // Persists state + tracker to disk without the publish leg, so a
                // stalled publish (no zenoh responder) can never starve durability
                // — the ZEB-509 bug this path fixes. Deliberately does NOT touch
                // `has_pending_dirty`: any pending state-root publish still fires
                // on the next debounce / flush_now. Mirrors the community engine's
                // persist_now arm.
                let persist_result = persist_now(&ctx).await;
                let _ = resp_tx.send(persist_result);
            }
            maybe_bytes = ctx.subscriber_rx.recv(), if !inbound_closed => {
                let Some(bytes) = maybe_bytes else {
                    // Whoever owned the inbound sender dropped it. Log
                    // loudly so this surfaces in the host logs, then latch
                    // the arm off. Engine remains alive in publish-only
                    // mode; the consumer can still mutate + persist.
                    tracing::error!(
                        "fleet-sync inbound subscriber channel closed; \
                         sync inbound disabled (engine continuing in publish-only mode)"
                    );
                    inbound_closed = true;
                    continue;
                };
                match handle_incoming_publish(&mut ctx, bytes).await {
                    Inbound::Applied(outcome) => {
                        if let Err(e) = persist_now(&ctx).await {
                            tracing::warn!(error = %e, "persist_now failed");
                        }
                        if outcome.changed {
                            if let Some(cb) = ctx.on_applied.as_ref() {
                                cb();
                            }
                        }
                    }
                    // Duplicate / Echo / Dropped: no persist, no callback.
                    Inbound::Duplicate | Inbound::Echo | Inbound::Dropped => {}
                }
            }
            Some(resp_tx) = ctx.shutdown_rx.recv() => {
                // Flush only if there is genuinely unpublished dirty state.
                let pub_result = if ctx.has_pending_dirty.load(Ordering::Relaxed) {
                    publish_root_now(&ctx).await
                } else {
                    Ok(())
                };
                let persist_result = persist_now(&ctx).await;
                // Surface either failure to the caller. Persist failure is
                // the more critical of the two — losing the final disk
                // flush silently corrupts the next-boot replay tracker /
                // state.
                let _ = resp_tx.send(pub_result.and(persist_result));
                return;
            }
        }
    }
}

/// Snapshot state + tracker under their async locks, then hop to a
/// blocking thread for the (potentially fsync-heavy) durability write so
/// the tokio worker is not stalled.
async fn persist_now<S>(ctx: &Ctx<S>) -> Result<(), SyncError>
where
    S: Clone + Send + 'static,
{
    let state_snap = ctx.state.lock().await.clone();
    let tracker_snap = ctx.replay_tracker.lock().await.clone();
    let persist = Arc::clone(&ctx.persist);
    tokio::task::spawn_blocking(move || persist.persist(&state_snap, &tracker_snap))
        .await
        .map_err(|e| SyncError::Persist(format!("spawn_blocking join: {e}")))?
}

/// Build, encrypt, store, and publish the current state root.
async fn publish_root_now<S>(ctx: &Ctx<S>) -> Result<(), SyncError>
where
    S: CanonicalPayload + Clone + Send + 'static,
{
    // Snapshot state under brief lock.
    let snapshot = {
        let state = ctx.state.lock().await;
        state.clone()
    };

    // 1. Canonical-CBOR encode the dataset as the cleartext root blob.
    let blob_cleartext =
        canonical_cbor_encode(&snapshot).map_err(|e| SyncError::CborEncode(e.to_string()))?;

    // 2. Encrypt with deterministic per-entry AEAD using the fixed
    //    root-blob lookup key, so the cipher CID is reproducible across
    //    two devices encrypting the same state. Publish always uses the
    //    newest installed epoch (ZEB-668 S5); the whole publish (entry +
    //    root envelope) is single-epoch by construction.
    let kt = ctx.keys.newest();
    let lookup = space_lookup_key(&kt, ctx.lookup_key_tag);
    let blob_ciphertext = encrypt_entry(&kt, &lookup, &blob_cleartext)
        .map_err(|e| SyncError::Crypto(e.to_string()))?;

    // 3. cipher CID = structured ContentId over the ciphertext. The
    //    `encrypted: true` flag (+ default `ephemeral: false`) maps to
    //    EncryptedDurable so the blob never auto-burns.
    let root_cid = ContentId::for_book(
        &blob_ciphertext,
        harmony_content::cid::ContentFlags {
            encrypted: true,
            ..Default::default()
        },
    )
    .map_err(|e| SyncError::Crypto(format!("ContentId::for_book: {e}")))?;

    // 4. Put into the content store.
    ctx.content_store.put(root_cid, blob_ciphertext).await?;

    // 5. Mint a strictly-newer HLC and assemble the publish envelope.
    let at = mint_next_hlc(&ctx.replay_tracker, &ctx.device_id).await;
    let seen = if ctx.publish_seen {
        // Snapshot the tracker into the `seen` digest, capped at
        // MAX_DEVICES_PER_OWNER so a pathological roster can't unbound
        // the envelope. When over the cap, keep the most-recently-active
        // devices (newest wall_ms) rather than truncating in BTreeMap
        // key (device_id) order, which would arbitrarily drop the
        // lexicographically-last devices.
        let tracker = ctx.replay_tracker.lock().await;
        let mut entries: Vec<(String, Hlc)> = tracker
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        entries.sort_by(|a, b| b.1.wall_ms.cmp(&a.1.wall_ms));
        entries.truncate(MAX_DEVICES_PER_OWNER);
        entries.into_iter().collect()
    } else {
        BTreeMap::new()
    };
    let payload = FleetRootPublish { root_cid, at, seen };
    let payload_bytes =
        canonical_cbor_encode(&payload).map_err(|e| SyncError::CborEncode(e.to_string()))?;

    // 6. Encrypt with random-nonce root AEAD (same epoch as the entry).
    let wire =
        encrypt_root_publish(&kt, &payload_bytes).map_err(|e| SyncError::Crypto(e.to_string()))?;

    // 7. Send onto the outbound channel.
    ctx.publisher_tx
        .send(wire)
        .await
        .map_err(|_| SyncError::TransportClosed)?;

    Ok(())
}

/// Build a strictly-newer HLC than the last one this device published.
/// Extracted as a free function (reused by later tasks). Mirrors the
/// donor `owner_state_sync::next_hlc` wall-ms / logical-saturation logic
/// exactly.
pub async fn mint_next_hlc(tracker: &Arc<Mutex<BTreeMap<String, Hlc>>>, device_id: &str) -> Hlc {
    let wall_ms = now_wall_ms();
    let mut tracker = tracker.lock().await;
    let now = compute_next_hlc(&tracker, device_id, wall_ms);
    tracker.insert(device_id.to_string(), now.clone());
    now
}

/// Compute the HLC that `mint_next_hlc` *would* produce from the current
/// tracker, **without** advancing it. Lets a caller decide whether a write
/// will actually apply before committing the HLC — minting on a no-op write
/// would advance `tracker[device]` above the last published HLC with no
/// corresponding publish, skewing the durability indicator and wasting a tick.
///
/// There is an inherent (sub-millisecond) race with a subsequent
/// `mint_next_hlc`: the real mint reads the clock again and the tracker may
/// have advanced, so the minted HLC is always `>=` this peek. That direction
/// is the safe one — if this peek is strictly-newer-than some value, the real
/// mint is too.
pub fn peek_next_hlc(tracker_snapshot: &BTreeMap<String, Hlc>, device_id: &str) -> Hlc {
    compute_next_hlc(tracker_snapshot, device_id, now_wall_ms())
}

fn now_wall_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Pure HLC computation shared by `mint_next_hlc` (which commits the result to
/// the tracker) and `peek_next_hlc` (which does not). No side effects.
fn compute_next_hlc(tracker: &BTreeMap<String, Hlc>, device_id: &str, wall_ms: u64) -> Hlc {
    // Read once — both the logical-counter computation and the
    // effective-wall pin need the same `prev`. Re-fetching from the map
    // is cheap but invites future divergence; folding into one lookup
    // keeps the read consistent.
    //
    // `saturating_add` on the logical counter: under sustained backward
    // NTP correction or repeated clock-monotonicity faults we'd
    // repeatedly bump `logical` without advancing `wall_ms`. An unchecked
    // u32 add would eventually wrap and produce an HLC smaller than the
    // previous one, breaking the strict-newer monotonicity that replay
    // protection depends on. Saturation pins the value at u32::MAX
    // instead — pathological but bounded; further publishes from the same
    // device on the same wall_ms tick would be rejected by the receiver
    // until the wall clock advances, which is preferable to silent
    // replay.
    let prev = tracker.get(device_id);
    let (logical, prev_wall) = match prev {
        Some(p) if p.wall_ms == wall_ms => (p.logical.saturating_add(1), p.wall_ms),
        Some(p) if p.wall_ms > wall_ms => (p.logical.saturating_add(1), p.wall_ms),
        Some(p) => (0, p.wall_ms),
        None => (0, 0),
    };
    let effective_wall = std::cmp::max(wall_ms, prev_wall);

    Hlc {
        wall_ms: effective_wall,
        logical,
        device_id: device_id.to_string(),
    }
}

/// Outcome of processing one inbound state-root publish.
#[derive(Debug)]
enum Inbound {
    /// Replay-rejected as a duplicate (not strictly newer). No change.
    Duplicate,
    /// Our own publish echoed back. No change.
    Echo,
    /// Dropped before applying (decrypt/decode/fetch-miss/blob-decrypt
    /// failure). No change; tracker NOT advanced — the peer's next
    /// publish is retried.
    Dropped,
    /// Merge applied AND tracker advanced. Persist + maybe-callback.
    Applied(MergeOutcome),
}

/// Process an incoming publish using MINT ordering (apply-before-advance):
/// read-only replay check → fetch → decrypt → decode → merge → THEN
/// advance the tracker → record sibling ack. A failure between the
/// read-only check and the tracker advance leaves the tracker
/// un-advanced, so the peer's next publish is retried.
#[allow(clippy::needless_pass_by_ref_mut)]
async fn handle_incoming_publish<S>(ctx: &mut Ctx<S>, wire: Vec<u8>) -> Inbound
where
    S: CanonicalPayload + serde::de::DeserializeOwned + Clone + Send + 'static,
{
    // 1. Decrypt the wire payload, trying every installed epoch newest-first
    //    (ZEB-668 S5 dual-epoch read window). A publish is entirely
    //    single-epoch, so whichever tree opens the root envelope is bound
    //    for the entry decrypt below.
    let (kt, payload_bytes) = {
        let mut opened = None;
        let mut last_err = None;
        for candidate in ctx.keys.accept_set() {
            match decrypt_root_publish(&candidate, &wire) {
                Ok(b) => {
                    opened = Some((candidate, b));
                    break;
                }
                Err(e) => last_err = Some(e),
            }
        }
        match opened {
            Some(pair) => pair,
            None => {
                // One log for the whole accept set — per-candidate failures
                // are expected during a rotation window, not noteworthy.
                tracing::warn!(
                    error = %last_err.map(|e| e.to_string()).unwrap_or_default(),
                    epochs = ?ctx.keys.epochs(),
                    "incoming publish dropped: decrypt_root_publish (no installed epoch opened it)"
                );
                return Inbound::Dropped;
            }
        }
    };
    // 2. Decode the envelope.
    let payload: FleetRootPublish = match canonical_cbor_decode(&payload_bytes) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "incoming publish dropped: envelope decode");
            return Inbound::Dropped;
        }
    };

    // 3. Echo-suppress: ignore our own publishes reflected back.
    if payload.at.device_id == ctx.device_id {
        return Inbound::Echo;
    }

    // 4. Replay check — READ-ONLY. Accept iff no prior entry or strictly
    //    newer. Do NOT mutate the tracker here (apply-before-advance).
    {
        let tracker = ctx.replay_tracker.lock().await;
        let accept = match tracker.get(&payload.at.device_id) {
            None => true,
            Some(existing) => payload.at.is_strictly_newer_than(existing),
        };
        if !accept {
            return Inbound::Duplicate;
        }
    }

    // 5. Fetch the encrypted root blob from CAS.
    let blob_ciphertext = match ctx.content_store.get(&payload.root_cid).await {
        Ok(Some(b)) => b,
        Ok(None) => {
            // A missing blob means a fetch timeout or a corrupted-admit
            // drop. Both collapse onto the same recovery path: drop the
            // publish (tracker un-advanced) and rely on the next
            // state-root from any peer (CRDT eventual consistency).
            tracing::warn!(
                cid = ?payload.root_cid,
                "incoming publish dropped: missing root blob (fetch timeout or admit-rejected)"
            );
            return Inbound::Dropped;
        }
        Err(e) => {
            tracing::warn!(error = %e, "incoming publish dropped: content_store.get error");
            return Inbound::Dropped;
        }
    };

    // 6. Decrypt with the same lookup key the publisher used — under the
    //    epoch that opened the root envelope in step 1.
    let lookup = space_lookup_key(&kt, ctx.lookup_key_tag);
    let blob_cleartext = match decrypt_entry(&kt, &lookup, &blob_ciphertext) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "incoming publish dropped: blob decrypt");
            return Inbound::Dropped;
        }
    };

    // 7. Decode into a remote snapshot.
    let remote: S = match canonical_cbor_decode(&blob_cleartext) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "incoming publish dropped: blob decode");
            return Inbound::Dropped;
        }
    };

    // 8. Merge the remote snapshot into local state.
    let outcome = {
        let mut local = ctx.state.lock().await;
        (ctx.merger)(&mut local, remote)
    };

    // 9. NOW advance the tracker — only after a successful apply.
    ctx.replay_tracker
        .lock()
        .await
        .insert(payload.at.device_id.clone(), payload.at.clone());

    // 10. If running with `publish_seen`, record the sibling's
    //     acknowledgement of us: their `seen[our_device]`, if newer than
    //     what we already have recorded for that sibling.
    if ctx.publish_seen {
        if let Some(their_seen_of_me) = payload.seen.get(&ctx.device_id) {
            let mut acks = ctx.sibling_acks.lock().await;
            let newer = match acks.get(&payload.at.device_id) {
                None => true,
                Some(existing) => their_seen_of_me.is_strictly_newer_than(existing),
            };
            if newer {
                acks.insert(payload.at.device_id.clone(), their_seen_of_me.clone());
            }
        }
    }

    Inbound::Applied(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
    use crate::owner_state_types::{Hlc, RootPublishPayload};
    use harmony_content::cid::{ContentFlags, ContentId};

    fn fixed_cid() -> ContentId {
        ContentId::for_book(
            b"fleet-sync-pin-fixture",
            ContentFlags {
                encrypted: true,
                ..Default::default()
            },
        )
        .expect("cid")
    }
    fn fixed_hlc() -> Hlc {
        Hlc {
            wall_ms: 1_700_000_000_000,
            logical: 0,
            device_id: "dev-A".into(),
        }
    }

    #[test]
    fn fleet_root_publish_with_empty_seen_is_byte_identical_to_legacy() {
        let cid = fixed_cid();
        let at = fixed_hlc();
        let legacy_bytes = canonical_cbor_encode(&RootPublishPayload {
            root_cid: cid,
            at: at.clone(),
        })
        .expect("legacy");
        let fleet_bytes = canonical_cbor_encode(&FleetRootPublish {
            root_cid: cid,
            at,
            seen: BTreeMap::new(),
        })
        .expect("fleet");
        assert_eq!(
            fleet_bytes, legacy_bytes,
            "empty-seen FleetRootPublish must equal legacy RootPublishPayload bytes"
        );
    }

    #[test]
    fn fleet_root_publish_with_seen_round_trips() {
        let mut seen = BTreeMap::new();
        seen.insert("dev-B".to_string(), fixed_hlc());
        let env = FleetRootPublish {
            root_cid: fixed_cid(),
            at: fixed_hlc(),
            seen,
        };
        let bytes = canonical_cbor_encode(&env).expect("encode");
        let back: FleetRootPublish = canonical_cbor_decode(&bytes).expect("decode");
        assert_eq!(back, env);
    }
}

#[cfg(test)]
mod engine_tests {
    use super::*;
    use crate::content_store::InMemoryStub;
    use async_trait::async_trait;
    use std::time::Duration;

    // ---- Toy dataset -------------------------------------------------
    //
    // Consumer-independent fixture so the engine test never depends on
    // OwnerState / MintState. Both field names are 2 chars to satisfy the
    // canonical-CBOR same-length-keys precondition (one map level at a
    // time: `ToyDoc` has the single key `en`; `ToyEntry` has `ct`/`vl`).

    #[derive(Clone, Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
    struct ToyDoc {
        #[serde(rename = "en")]
        entries: BTreeMap<String, ToyEntry>,
    }

    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    struct ToyEntry {
        #[serde(rename = "ct")]
        ctr: u64,
        #[serde(rename = "vl")]
        val: String,
    }

    // Manual CanonicalPayload registration (the macro is module-private;
    // mirror the manual FleetRootPublish registration above).
    impl CanonicalPayloadSealed for ToyDoc {}
    impl CanonicalPayload for ToyDoc {}
    impl CanonicalPayloadSealed for ToyEntry {}
    impl CanonicalPayload for ToyEntry {}

    /// Per-key LWW by counter: remote replaces local iff its `ctr` is
    /// strictly greater (or the key is new). Reports whether anything
    /// changed.
    fn toy_merge(local: &mut ToyDoc, remote: ToyDoc) -> MergeOutcome {
        let mut changed = false;
        for (k, rv) in remote.entries {
            match local.entries.get(&k) {
                Some(lv) if lv.ctr >= rv.ctr => {}
                _ => {
                    local.entries.insert(k, rv);
                    changed = true;
                }
            }
        }
        MergeOutcome { changed }
    }

    fn make_kt() -> Arc<KeyTree> {
        Arc::new(KeyTree::derive(&[0x11u8; 32]).expect("kt"))
    }

    const TOY_TAG: &[u8] = b"fleet-sync-toy-root-blob-v1";

    /// No-op durability sink — these tests assert convergence + tracker
    /// ordering, not on-disk persistence.
    struct NoopPersist;
    impl FleetPersist<ToyDoc> for NoopPersist {
        fn persist(
            &self,
            _state: &ToyDoc,
            _tracker: &BTreeMap<String, Hlc>,
        ) -> Result<(), SyncError> {
            Ok(())
        }
    }

    /// All the handles the test harness shuttles between engines.
    struct Built {
        engine: FleetSyncEngine<ToyDoc>,
        state: Arc<Mutex<ToyDoc>>,
        tracker: Arc<Mutex<BTreeMap<String, Hlc>>>,
        out_rx: mpsc::Receiver<Vec<u8>>,
        in_tx: mpsc::Sender<Vec<u8>>,
    }

    fn build_engine(device_id: &str, cas: Arc<dyn ContentStore>, publish_seen: bool) -> Built {
        build_engine_with_keys(device_id, cas, publish_seen, FleetKeySet::new(make_kt()))
    }

    fn build_engine_with_keys(
        device_id: &str,
        cas: Arc<dyn ContentStore>,
        publish_seen: bool,
        keys: FleetKeySet,
    ) -> Built {
        let (out_tx, out_rx) = mpsc::channel(64);
        let (in_tx, in_rx) = mpsc::channel(64);
        let state = Arc::new(Mutex::new(ToyDoc::default()));
        let tracker = Arc::new(Mutex::new(BTreeMap::new()));
        let engine = FleetSyncEngine::new(FleetSyncConfig {
            keys,
            device_id: device_id.to_string(),
            state: Arc::clone(&state),
            merger: Arc::new(|local: &mut ToyDoc, remote: ToyDoc| toy_merge(local, remote)),
            replay_tracker: Arc::clone(&tracker),
            content_store: cas,
            publisher_tx: out_tx,
            subscriber_rx: in_rx,
            persist: Arc::new(NoopPersist),
            lookup_key_tag: TOY_TAG,
            debounce_ms: 50,
            publish_seen,
            on_applied: None,
            sibling_acks: Arc::new(Mutex::new(BTreeMap::new())),
        });
        Built {
            engine,
            state,
            tracker,
            out_rx,
            in_tx,
        }
    }

    /// Inspectable durability sink: records every `(state, tracker)` snapshot
    /// the engine asks to persist so a test can assert the mutated state
    /// actually reached the persist backend.
    #[derive(Clone, Default)]
    struct RecordingPersist {
        persisted: Arc<std::sync::Mutex<Vec<ToyDoc>>>,
    }
    impl FleetPersist<ToyDoc> for RecordingPersist {
        fn persist(
            &self,
            state: &ToyDoc,
            _tracker: &BTreeMap<String, Hlc>,
        ) -> Result<(), SyncError> {
            self.persisted
                .lock()
                .expect("persist mutex")
                .push(state.clone());
            Ok(())
        }
    }

    /// Like `build_engine`, but with an inspectable persist backend and a
    /// caller-chosen publisher-channel capacity so a test can saturate the
    /// outbound publish leg. Returns the engine plus the handles the
    /// persist-only tests need.
    struct BuiltInspectable {
        engine: FleetSyncEngine<ToyDoc>,
        state: Arc<Mutex<ToyDoc>>,
        persist: RecordingPersist,
        out_rx: mpsc::Receiver<Vec<u8>>,
        out_tx: mpsc::Sender<Vec<u8>>,
        /// Held only to keep the inbound subscriber channel open for the
        /// engine's lifetime; dropping it would close `subscriber_rx` and make
        /// the engine task log a spurious "inbound subscriber channel closed"
        /// error during these persist-only tests. Mirrors `build_engine`, which
        /// keeps its `in_tx` alive in `Built` for the same reason.
        #[allow(dead_code)]
        in_tx: mpsc::Sender<Vec<u8>>,
    }

    fn build_engine_inspectable(
        device_id: &str,
        cas: Arc<dyn ContentStore>,
        publisher_capacity: usize,
    ) -> BuiltInspectable {
        let (out_tx, out_rx) = mpsc::channel(publisher_capacity);
        // Keep the inbound sender alive (stored in BuiltInspectable below) so
        // the engine task doesn't immediately observe a closed subscriber
        // channel and log a spurious error during these persist-only tests.
        let (in_tx, in_rx) = mpsc::channel(64);
        let state = Arc::new(Mutex::new(ToyDoc::default()));
        let tracker = Arc::new(Mutex::new(BTreeMap::new()));
        let persist = RecordingPersist::default();
        let engine = FleetSyncEngine::new(FleetSyncConfig {
            keys: FleetKeySet::new(make_kt()),
            device_id: device_id.to_string(),
            state: Arc::clone(&state),
            merger: Arc::new(|local: &mut ToyDoc, remote: ToyDoc| toy_merge(local, remote)),
            replay_tracker: Arc::clone(&tracker),
            content_store: cas,
            publisher_tx: out_tx.clone(),
            subscriber_rx: in_rx,
            persist: Arc::new(persist.clone()),
            lookup_key_tag: TOY_TAG,
            debounce_ms: 3_600_000, // effectively disable the debounce wakeup
            publish_seen: false,
            on_applied: None,
            sibling_acks: Arc::new(Mutex::new(BTreeMap::new())),
        });
        BuiltInspectable {
            engine,
            state,
            persist,
            out_rx,
            out_tx,
            in_tx,
        }
    }

    /// `persist_now` must durably write the mutated state to the persist
    /// backend WITHOUT emitting any publish on the outbound channel. Mirrors
    /// `community_state_sync::persist_now_fences_crdt_without_publishing_zeb462`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn persist_now_persists_without_publishing() {
        let cas: Arc<dyn ContentStore> = Arc::new(InMemoryStub::default());
        let built = build_engine_inspectable("dev-persist", Arc::clone(&cas), 8);

        // Mutate local state, then fence it durable via the persist-only path.
        {
            let mut doc = built.state.lock().await;
            doc.entries.insert(
                "k1".into(),
                ToyEntry {
                    ctr: 1,
                    val: "durable".into(),
                },
            );
        }

        // (1) persist_now returns Ok.
        built.engine.persist_now().await.expect("persist_now");

        // (2) the mutated state reached the persist backend.
        let persisted = built
            .persist
            .persisted
            .lock()
            .expect("persist mutex")
            .clone();
        assert!(
            persisted
                .iter()
                .any(|doc| doc.entries.get("k1").is_some_and(|e| e.val == "durable")),
            "persist_now must durably write the mutated state to the persist backend"
        );

        // (3) ZERO publishes on the outbound channel during persist_now.
        let mut out_rx = built.out_rx;
        assert!(
            matches!(out_rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
            "persist_now must NOT publish — the outbound channel must be empty"
        );

        built.engine.shutdown().await.ok();
    }

    /// The precise property `flush_now` lacks: `persist_now` completes promptly
    /// even when the outbound publisher channel is saturated to capacity, since
    /// it never touches the publish leg. A `flush_now` here would block on the
    /// saturated publisher send. (ZEB-509: durability must never wait on publish.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn persist_now_persists_without_publishing_under_saturated_publisher() {
        let cas: Arc<dyn ContentStore> = Arc::new(InMemoryStub::default());
        // Capacity-1 publisher channel whose receiver is never drained.
        let built = build_engine_inspectable("dev-persist-sat", Arc::clone(&cas), 1);

        // Saturate the publisher channel to capacity (rx held, never drained),
        // so any publish leg would back-pressure indefinitely.
        built
            .out_tx
            .send(vec![0xab; 4])
            .await
            .expect("prime saturated publisher channel");
        assert!(
            built.out_tx.try_send(vec![0xcd; 4]).is_err(),
            "publisher channel must be provably full for this test to be meaningful"
        );

        {
            let mut doc = built.state.lock().await;
            doc.entries.insert(
                "k1".into(),
                ToyEntry {
                    ctr: 1,
                    val: "durable-under-saturation".into(),
                },
            );
        }

        // persist_now must return Ok PROMPTLY despite the saturated publisher.
        let result = tokio::time::timeout(Duration::from_secs(5), built.engine.persist_now()).await;
        assert!(
            matches!(result, Ok(Ok(()))),
            "persist_now must complete even with the publisher channel saturated; got {result:?}"
        );

        // State still reached disk.
        let persisted = built
            .persist
            .persisted
            .lock()
            .expect("persist mutex")
            .clone();
        assert!(
            persisted.iter().any(|doc| doc
                .entries
                .get("k1")
                .is_some_and(|e| e.val == "durable-under-saturation")),
            "persist_now must persist mutated state even under a saturated publisher"
        );

        // The only frame on the channel is the one the test primed — no publish.
        let mut out_rx = built.out_rx;
        assert_eq!(
            out_rx.try_recv().expect("primed frame"),
            vec![0xab; 4],
            "the channel must hold only the test's priming frame"
        );
        assert!(
            matches!(out_rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
            "persist_now must not have published any additional frame"
        );

        // Drain so shutdown's final flush isn't itself starved by the saturation.
        built.engine.shutdown().await.ok();
    }

    /// Bounded condition-polling helper (no fixed sleeps).
    async fn wait_until<F, Fut>(mut cond: F, timeout: Duration) -> bool
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if cond().await {
                return true;
            }
            if tokio::time::Instant::now() > deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    }

    /// ZEB-702 T2: the `RepublishDirty` seam must delegate to the engine's
    /// debounced-publish path. Driving it through an `Arc<dyn RepublishDirty>`
    /// (the exact shape Task 3's transport-epoch listener consumes) schedules
    /// the same debounced publish a `notify_dirty()` would — observed as a
    /// frame on the outbound publisher channel. Models
    /// `mint_sync::notify_dirty_triggers_debounced_publish`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn republish_dirty_schedules_debounced_publish() {
        let cas: Arc<dyn ContentStore> = Arc::new(InMemoryStub::default());
        let Built {
            engine,
            mut out_rx,
            in_tx,
            ..
        } = build_engine("dev-republish", Arc::clone(&cas), false);
        // Keep the inbound sender alive so the engine task doesn't log a
        // spurious "inbound subscriber channel closed" error mid-test.
        let _in_tx = in_tx;

        // Drive the seam through the trait object, exactly as Task 3 will.
        let engine = Arc::new(engine);
        let handle: Arc<dyn RepublishDirty> = engine.clone();
        handle.republish_dirty();

        // A debounced publish (50 ms window) must land on the outbound channel.
        let frame = tokio::time::timeout(Duration::from_secs(5), out_rx.recv())
            .await
            .expect("republish_dirty must trigger a debounced publish within 5s")
            .expect("publisher channel closed before a frame arrived");
        assert!(
            !frame.is_empty(),
            "republish_dirty debounced publish produced an empty frame"
        );

        engine.shutdown().await.ok();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_engine_without_shutdown_aborts_internal_task() {
        // Regression (ZEB-460 review): a FleetSyncEngine dropped WITHOUT
        // shutdown().await — e.g. an aborted boot that built some engines before
        // a later dataset load returned Err — must not orphan its internal task.
        // The task holds an Arc clone of `state`; the Drop impl aborts the task
        // so that clone is released. Without the abort the task loops forever and
        // the clone leaks (this poll would time out).
        let cas: Arc<dyn ContentStore> = Arc::new(InMemoryStub::default());
        let built = build_engine("dev-drop", Arc::clone(&cas), false);
        let state = Arc::clone(&built.state);
        assert!(
            Arc::strong_count(&state) >= 2,
            "internal task should hold a state clone while the engine is alive"
        );
        // Drop ONLY the engine-bearing Built; keep `state` to observe the task.
        drop(built);
        // After abort + reap, the task's `state` clone is released → count == 1.
        let mut released = false;
        for _ in 0..100 {
            if Arc::strong_count(&state) == 1 {
                released = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            released,
            "internal task was not aborted on drop — its state clone leaked"
        );
    }

    /// ZEB-668 S5: a publish encrypted under epoch-1 keys converges on a
    /// receiver whose accept set holds {0, 1}, and is dropped by a receiver
    /// holding only {0} (post-rotation exclusion — the revoked-device case).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dual_epoch_window_accepts_newer_epoch_and_old_only_set_drops_it() {
        let seed = [0u8; 32];
        let e0 = || Arc::new(KeyTree::derive_at_epoch(&seed, 0).expect("e0"));
        let e1 = || Arc::new(KeyTree::derive_at_epoch(&seed, 1).expect("e1"));

        let cas: Arc<dyn ContentStore> = Arc::new(InMemoryStub::default());
        // Publisher on epoch 1 only.
        let a = build_engine_with_keys("dev-A", Arc::clone(&cas), false, FleetKeySet::new(e1()));
        // Receiver B: dual-epoch window {1, 0}.
        let b_keys = FleetKeySet::new(e0());
        b_keys.install(e1());
        let b = build_engine_with_keys("dev-B", Arc::clone(&cas), false, b_keys);
        // Receiver C: epoch 0 only (e.g. a device that never installed the bump).
        let c = build_engine_with_keys("dev-C", Arc::clone(&cas), false, FleetKeySet::new(e0()));

        // Fan A's publishes out to BOTH receivers.
        let mut a = a;
        let b_in = b.in_tx.clone();
        let c_in = c.in_tx.clone();
        let mut a_out = std::mem::replace(&mut a.out_rx, mpsc::channel(1).1);
        let forwarder = tokio::spawn(async move {
            while let Some(frame) = a_out.recv().await {
                let _ = b_in.send(frame.clone()).await;
                let _ = c_in.send(frame).await;
            }
        });

        {
            let mut doc = a.state.lock().await;
            doc.entries.insert(
                "k1".into(),
                ToyEntry {
                    ctr: 1,
                    val: "rotated".into(),
                },
            );
        }
        a.engine.flush_now().await.expect("flush A");

        let b_state = Arc::clone(&b.state);
        let converged = wait_until(
            || {
                let b_state = Arc::clone(&b_state);
                async move {
                    let doc = b_state.lock().await;
                    doc.entries.get("k1").is_some_and(|e| e.val == "rotated")
                }
            },
            Duration::from_secs(5),
        )
        .await;
        assert!(converged, "dual-epoch receiver did not converge within 5s");

        // C (epoch 0 only) must NOT have applied the epoch-1 publish. B has
        // already converged on the same frames, so C has had its chance.
        let c_doc = c.state.lock().await;
        assert!(
            c_doc.entries.is_empty(),
            "old-only key set must drop an epoch-1 publish"
        );
        drop(c_doc);

        let _ = a.engine.shutdown().await;
        let _ = b.engine.shutdown().await;
        let _ = c.engine.shutdown().await;
        forwarder.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_engines_converge() {
        // Shared CAS so both engines name the same blobs.
        let cas: Arc<dyn ContentStore> = Arc::new(InMemoryStub::default());
        let mut a = build_engine("dev-A", Arc::clone(&cas), false);
        let mut b = build_engine("dev-B", Arc::clone(&cas), false);

        // Forwarder: pump A.out -> B.in and B.out -> A.in. Move the out
        // receivers out of each Built (replacing with a dead receiver) so
        // the remaining fields (engine/state/tracker) stay usable.
        let a_in = a.in_tx.clone();
        let b_in = b.in_tx.clone();
        let mut a_out = std::mem::replace(&mut a.out_rx, mpsc::channel(1).1);
        let mut b_out = std::mem::replace(&mut b.out_rx, mpsc::channel(1).1);
        let forwarder = tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some(frame) = a_out.recv() => {
                        let _ = b_in.send(frame).await;
                    }
                    Some(frame) = b_out.recv() => {
                        let _ = a_in.send(frame).await;
                    }
                    else => break,
                }
            }
        });

        // Mutate A's doc, then force a publish.
        {
            let mut doc = a.state.lock().await;
            doc.entries.insert(
                "k1".into(),
                ToyEntry {
                    ctr: 1,
                    val: "hello".into(),
                },
            );
        }
        a.engine.flush_now().await.expect("flush A");

        // B must converge to A's entry.
        let b_state = Arc::clone(&b.state);
        let converged = wait_until(
            || {
                let b_state = Arc::clone(&b_state);
                async move {
                    let doc = b_state.lock().await;
                    doc.entries.get("k1").is_some_and(|e| e.val == "hello")
                }
            },
            Duration::from_secs(5),
        )
        .await;
        assert!(converged, "B did not converge to A's entry within 5s");

        let _ = a.engine.shutdown().await;
        let _ = b.engine.shutdown().await;
        forwarder.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn synced_device_count_reflects_fan_out() {
        // Three engines sharing ONE CAS so they all name the same blobs.
        // All three run with `publish_seen = true`: B and C must EMIT a
        // `seen` digest that contains dev-A's HLC (their acknowledgement of
        // A's state), and A must RECORD those acks into `sibling_acks` —
        // both halves require the flag.
        let cas: Arc<dyn ContentStore> = Arc::new(InMemoryStub::default());
        let mut a = build_engine("dev-A", Arc::clone(&cas), true);
        let mut b = build_engine("dev-B", Arc::clone(&cas), true);
        let mut c = build_engine("dev-C", Arc::clone(&cas), true);

        // Fan-out forwarder: every engine's outbound frame is delivered to
        // BOTH of the other two engines' inboxes. Move each out-receiver
        // out of its Built (replacing with a dead receiver) so the
        // remaining fields (engine/state) stay usable.
        let a_in = a.in_tx.clone();
        let b_in = b.in_tx.clone();
        let c_in = c.in_tx.clone();
        let mut a_out = std::mem::replace(&mut a.out_rx, mpsc::channel(1).1);
        let mut b_out = std::mem::replace(&mut b.out_rx, mpsc::channel(1).1);
        let mut c_out = std::mem::replace(&mut c.out_rx, mpsc::channel(1).1);
        let forwarder = {
            let a_in = a_in.clone();
            let b_in = b_in.clone();
            let c_in = c_in.clone();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        Some(frame) = a_out.recv() => {
                            // A.out -> B.in & C.in
                            let _ = b_in.send(frame.clone()).await;
                            let _ = c_in.send(frame).await;
                        }
                        Some(frame) = b_out.recv() => {
                            // B.out -> A.in & C.in
                            let _ = a_in.send(frame.clone()).await;
                            let _ = c_in.send(frame).await;
                        }
                        Some(frame) = c_out.recv() => {
                            // C.out -> A.in & B.in
                            let _ = a_in.send(frame.clone()).await;
                            let _ = b_in.send(frame).await;
                        }
                        else => break,
                    }
                }
            })
        };

        // A does a local write, then forces a publish.
        {
            let mut doc = a.state.lock().await;
            doc.entries.insert(
                "k1".into(),
                ToyEntry {
                    ctr: 1,
                    val: "from-A".into(),
                },
            );
        }
        a.engine.flush_now().await.expect("flush A");

        // B and C must both converge to A's entry.
        let b_state = Arc::clone(&b.state);
        let c_state = Arc::clone(&c.state);
        let both_converged = wait_until(
            || {
                let b_state = Arc::clone(&b_state);
                let c_state = Arc::clone(&c_state);
                async move {
                    let b_has = b_state
                        .lock()
                        .await
                        .entries
                        .get("k1")
                        .is_some_and(|e| e.val == "from-A");
                    let c_has = c_state
                        .lock()
                        .await
                        .entries
                        .get("k1")
                        .is_some_and(|e| e.val == "from-A");
                    b_has && c_has
                }
            },
            Duration::from_secs(5),
        )
        .await;
        assert!(
            both_converged,
            "B and C did not both converge to A's entry within 5s"
        );

        // An INBOUND merge does NOT schedule a republish, so B and C have
        // not yet emitted a `seen` digest carrying dev-A's HLC. Force each
        // to publish so their acknowledgement flows back to A.
        b.engine.flush_now().await.expect("flush B");
        c.engine.flush_now().await.expect("flush C");

        // A must now observe that BOTH siblings have acked its latest
        // state — `synced_device_count` == 2.
        let synced_two = wait_until(
            || {
                let engine = &a.engine;
                async move { engine.synced_device_count().await == 2 }
            },
            Duration::from_secs(5),
        )
        .await;
        assert!(
            synced_two,
            "A's synced_device_count never reached 2 (acks from B and C) within 5s; \
             observed {}",
            a.engine.synced_device_count().await
        );
        assert_eq!(
            a.engine.synced_device_count().await,
            2,
            "synced_device_count should count exactly the two acking siblings"
        );

        // A freshly-built, isolated engine (no peers, never published) must
        // report 0 acked devices — "1 device — not yet backed up" maps to
        // zero synced siblings.
        let lone = build_engine("dev-lone", Arc::clone(&cas), true);
        assert_eq!(
            lone.engine.synced_device_count().await,
            0,
            "an isolated engine with no peers must report 0 synced devices"
        );

        let _ = a.engine.shutdown().await;
        let _ = b.engine.shutdown().await;
        let _ = c.engine.shutdown().await;
        let _ = lone.engine.shutdown().await;
        forwarder.abort();
    }

    /// CAS wrapper whose `get` misses (returns Ok(None)) until `armed` is
    /// set, after which `get` delegates to the inner store. `put` always
    /// delegates so the blob is present in the inner store from the
    /// start. `get_attempts` counts every `get` call (incremented FIRST,
    /// before the armed branch) so a test can observe that B's engine
    /// actually reached the CAS-fetch step — a POSITIVE signal that turns
    /// the "nothing happened" assertion into a deterministic guard with no
    /// wall-clock dependency.
    struct GateGetCas {
        inner: Arc<dyn ContentStore>,
        armed: std::sync::atomic::AtomicBool,
        get_attempts: Arc<std::sync::atomic::AtomicUsize>,
    }
    #[async_trait]
    impl ContentStore for GateGetCas {
        async fn put(&self, cid: ContentId, blob: Vec<u8>) -> Result<(), ContentStoreError> {
            self.inner.put(cid, blob).await
        }
        async fn get(&self, cid: &ContentId) -> Result<Option<Vec<u8>>, ContentStoreError> {
            self.get_attempts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.armed.load(std::sync::atomic::Ordering::SeqCst) {
                self.inner.get(cid).await
            } else {
                // Miss (Ok(None)) — NOT a hang — so the engine completes
                // the failing fetch path and the counter reflects a real
                // attempt.
                Ok(None)
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn blob_miss_is_dropped_and_recovered_on_next_publish() {
        // Shared inner store so A's puts are nominally fetchable; B's view
        // is gated so its first `get` misses, later gets succeed.
        let inner: Arc<dyn ContentStore> = Arc::new(InMemoryStub::default());
        let get_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let gate = Arc::new(GateGetCas {
            inner: Arc::clone(&inner),
            armed: std::sync::atomic::AtomicBool::new(false),
            get_attempts: Arc::clone(&get_attempts),
        });
        let mut a = build_engine("dev-A", Arc::clone(&inner), false);
        let b = build_engine("dev-B", Arc::clone(&gate) as Arc<dyn ContentStore>, false);

        // Forward A.out -> B.in only (one-directional is enough here).
        let b_in = b.in_tx.clone();
        let mut a_out = std::mem::replace(&mut a.out_rx, mpsc::channel(1).1);
        let forwarder = tokio::spawn(async move {
            while let Some(frame) = a_out.recv().await {
                let _ = b_in.send(frame).await;
            }
        });

        // First publish — B's get MISSES (gate not armed).
        {
            let mut doc = a.state.lock().await;
            doc.entries.insert(
                "k1".into(),
                ToyEntry {
                    ctr: 1,
                    val: "v1".into(),
                },
            );
        }
        a.engine.flush_now().await.expect("flush A #1");

        // B must NOT converge, and its tracker must NOT record dev-A
        // (apply-before-advance: a fetch miss leaves the tracker
        // un-advanced). Gate the negative assertion on a POSITIVE signal —
        // that B's engine actually reached the (missing) CAS fetch —
        // rather than a fixed wall-clock window (which is false-green on a
        // loaded machine). Once a get has been attempted, the
        // apply-before-advance ordering guarantees the tracker is only
        // advanced AFTER a successful fetch; so observing get_attempts >= 1
        // and then asserting "no dev-A entry" is a deterministic guard.
        let b_state = Arc::clone(&b.state);
        let reached_fetch = wait_until(
            || {
                let get_attempts = Arc::clone(&get_attempts);
                async move { get_attempts.load(std::sync::atomic::Ordering::SeqCst) >= 1 }
            },
            Duration::from_secs(5),
        )
        .await;
        assert!(
            reached_fetch,
            "B's engine never reached the CAS fetch step within 5s"
        );
        assert!(
            !b.tracker.lock().await.contains_key("dev-A"),
            "B's tracker advanced for dev-A despite a blob-fetch miss"
        );
        assert!(
            !b_state.lock().await.entries.contains_key("k1"),
            "B converged despite the blob being unfetchable"
        );

        // Now arm the gate (blob becomes fetchable) and have A republish.
        gate.armed.store(true, std::sync::atomic::Ordering::SeqCst);
        {
            let mut doc = a.state.lock().await;
            doc.entries.insert(
                "k1".into(),
                ToyEntry {
                    ctr: 2,
                    val: "v2".into(),
                },
            );
        }
        a.engine.flush_now().await.expect("flush A #2");

        let converged = wait_until(
            || {
                let b_state = Arc::clone(&b_state);
                async move {
                    let doc = b_state.lock().await;
                    doc.entries.get("k1").is_some_and(|e| e.val == "v2")
                }
            },
            Duration::from_secs(5),
        )
        .await;
        assert!(
            converged,
            "B did not converge after the blob became fetchable"
        );

        let _ = a.engine.shutdown().await;
        let _ = b.engine.shutdown().await;
        forwarder.abort();
    }

    /// CAS wrapper whose `get` ALWAYS errors. `put` delegates so A can
    /// still publish a real (fetchable-by-others) blob. `get_attempts`
    /// counts every `get` call (incremented FIRST, before returning Err)
    /// so a test can observe that B's engine actually reached the
    /// (failing) CAS-fetch step.
    struct ErrGetCas {
        inner: Arc<dyn ContentStore>,
        get_attempts: Arc<std::sync::atomic::AtomicUsize>,
    }
    #[async_trait]
    impl ContentStore for ErrGetCas {
        async fn put(&self, cid: ContentId, blob: Vec<u8>) -> Result<(), ContentStoreError> {
            self.inner.put(cid, blob).await
        }
        async fn get(&self, _cid: &ContentId) -> Result<Option<Vec<u8>>, ContentStoreError> {
            self.get_attempts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(ContentStoreError::Io("forced get failure".into()))
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_before_advance_failure_does_not_advance_tracker() {
        // A publishes through a normal shared CAS so we can capture a
        // valid wire frame. B fetches through a CAS whose get always
        // errors, so the inbound apply fails AFTER the read-only replay
        // check but BEFORE the merge — the tracker must stay un-advanced.
        let inner: Arc<dyn ContentStore> = Arc::new(InMemoryStub::default());
        let mut a = build_engine("dev-A", Arc::clone(&inner), false);
        let get_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let err_cas: Arc<dyn ContentStore> = Arc::new(ErrGetCas {
            inner: Arc::clone(&inner),
            get_attempts: Arc::clone(&get_attempts),
        });
        let b = build_engine("dev-B", err_cas, false);

        // Capture exactly one valid wire frame from A.
        {
            let mut doc = a.state.lock().await;
            doc.entries.insert(
                "k1".into(),
                ToyEntry {
                    ctr: 1,
                    val: "v1".into(),
                },
            );
        }
        a.engine.flush_now().await.expect("flush A");
        let frame = tokio::time::timeout(Duration::from_secs(2), a.out_rx.recv())
            .await
            .expect("A produced a publish frame")
            .expect("frame not None");

        // Feed it to B.
        b.in_tx.send(frame).await.expect("inject frame");

        // Gate the negative assertion on a POSITIVE signal: wait until B's
        // engine has actually reached the (failing) CAS fetch. A fixed
        // wall-clock window would be false-green on a loaded machine (the
        // engine task may simply not have run yet). In apply-before-advance
        // ordering the tracker insert happens strictly AFTER the CAS get;
        // so once a get has been attempted, if the (buggy) advance-before-
        // apply ordering were present the tracker would ALREADY contain
        // dev-A. Asserting "no dev-A entry" right after observing
        // get_attempts >= 1 is therefore a deterministic guard with no
        // wall-clock dependency.
        let reached_fetch = wait_until(
            || {
                let get_attempts = Arc::clone(&get_attempts);
                async move { get_attempts.load(std::sync::atomic::Ordering::SeqCst) >= 1 }
            },
            Duration::from_secs(5),
        )
        .await;
        assert!(
            reached_fetch,
            "B's engine never reached the CAS fetch step within 5s"
        );

        // After the failing fetch, B's tracker must have NO key for dev-A
        // (proving advance happens only after a successful apply).
        assert!(
            !b.tracker.lock().await.contains_key("dev-A"),
            "tracker advanced for dev-A despite the apply failing (get error)"
        );
        assert!(
            !b.state.lock().await.entries.contains_key("k1"),
            "B applied the merge despite the fetch error"
        );

        let _ = a.engine.shutdown().await;
        let _ = b.engine.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_online_devices_returns_seen_peers_excluding_self() {
        // Build A and B sharing one CAS. A publishes, B merges A's state.
        // After convergence:
        //   - B.list_online_devices() == ["dev-A"]  (B saw A publish)
        //   - A.list_online_devices() does NOT contain "dev-A" (A's tracker
        //     only has its own entry from publishing, which is excluded by
        //     the self-filter).
        let cas: Arc<dyn ContentStore> = Arc::new(InMemoryStub::default());
        let mut a = build_engine("dev-A", Arc::clone(&cas), false);
        let mut b = build_engine("dev-B", Arc::clone(&cas), false);

        // Bidirectional forwarder (same pattern as two_engines_converge).
        let a_in = a.in_tx.clone();
        let b_in = b.in_tx.clone();
        let mut a_out = std::mem::replace(&mut a.out_rx, mpsc::channel(1).1);
        let mut b_out = std::mem::replace(&mut b.out_rx, mpsc::channel(1).1);
        let forwarder = tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some(frame) = a_out.recv() => {
                        let _ = b_in.send(frame).await;
                    }
                    Some(frame) = b_out.recv() => {
                        let _ = a_in.send(frame).await;
                    }
                    else => break,
                }
            }
        });

        // A writes an entry and publishes.
        {
            let mut doc = a.state.lock().await;
            doc.entries.insert(
                "k1".into(),
                ToyEntry {
                    ctr: 1,
                    val: "from-A".into(),
                },
            );
        }
        a.engine.flush_now().await.expect("flush A");

        // Wait until B has merged A's entry (B's tracker now has "dev-A").
        let b_state = Arc::clone(&b.state);
        let converged = wait_until(
            || {
                let b_state = Arc::clone(&b_state);
                async move {
                    b_state
                        .lock()
                        .await
                        .entries
                        .get("k1")
                        .is_some_and(|e| e.val == "from-A")
                }
            },
            Duration::from_secs(5),
        )
        .await;
        assert!(converged, "B did not converge to A's entry within 5s");

        // B must report "dev-A" as an online peer (it merged A's publish).
        let mut b_online = b.engine.list_online_devices().await;
        b_online.sort();
        assert_eq!(
            b_online,
            vec!["dev-A".to_string()],
            "B.list_online_devices() must equal [\"dev-A\"] after merging A's publish"
        );

        // B's own id must NOT appear in the list (self-exclusion).
        assert!(
            !b_online.contains(&"dev-B".to_string()),
            "B.list_online_devices() must not contain B's own device id"
        );

        // A's tracker only records its own entry (from the publish path,
        // which inserts the publisher's own device_id). The self-filter
        // must exclude it, yielding an empty list.
        let a_online = a.engine.list_online_devices().await;
        assert!(
            !a_online.contains(&"dev-A".to_string()),
            "A.list_online_devices() must not contain A's own device id"
        );

        let _ = a.engine.shutdown().await;
        let _ = b.engine.shutdown().await;
        forwarder.abort();
    }
}
