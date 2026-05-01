//! Owner-state SyncEngine — debounced publishes + Zenoh-agnostic
//! channel surface + replay-protected subscriber merge path
//! (ZEB-215 Sub-A Phase 3a).
//!
//! See `docs/specs/2026-05-01-zeb-215-sub-a-phase3a-sync-design.md`
//! §"Architecture". Channel-based; the Zenoh adapter lives in
//! `event_loop.rs` (Task 19).

use crate::content_store::ContentStore;
use crate::owner_state_crdt::OwnerState;
use crate::owner_state_crypto::KeyTree;
use crate::owner_state_types::Hlc;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex, Notify};
use tokio::task::JoinHandle;

/// Default debounce window between a `notify_dirty` and the
/// resulting state-root publish. See spec §"Architecture" — small
/// enough to feel near-instant to a human, large enough to collapse
/// keystroke-rate mutations.
pub const DEFAULT_DEBOUNCE_MS: u64 = 250;

#[derive(thiserror::Error, Debug)]
pub enum SyncError {
    #[error("content store: {0}")]
    ContentStore(#[from] crate::content_store::ContentStoreError),
    #[error("crypto: {0}")]
    Crypto(String),
    #[error("CBOR encode: {0}")]
    CborEncode(String),
    #[error("CBOR decode: {0}")]
    CborDecode(String),
    #[error("persist: {0}")]
    Persist(#[from] crate::owner_state_persist::PersistError),
    #[error("transport channel closed")]
    TransportClosed,
}

/// Filesystem paths for both new files; assembled at boot from
/// `resolve_identity_dir()` and the spec's filename constants.
#[derive(Debug, Clone)]
pub struct PersistPaths {
    pub crdt: PathBuf,
    pub replay: PathBuf,
}

/// Owner-state sync engine. Owns a tokio task that runs the
/// debounce timer + publisher + subscriber + persistence flushes.
/// Construction spawns the task; `shutdown().await` stops it
/// cleanly with one final flush.
pub struct SyncEngine {
    notify_dirty: Arc<Notify>,
    /// Set to `true` by `notify_dirty()`; cleared by the task after
    /// each publish. Prevents the shutdown path from emitting a
    /// spurious publish when the `Notify` permit was left over from
    /// before the most-recent actual publish.
    has_pending_dirty: Arc<AtomicBool>,
    flush_now_tx: mpsc::Sender<tokio::sync::oneshot::Sender<Result<(), SyncError>>>,
    /// Carries `Result<(), SyncError>` so the final publish + persist
    /// errors propagate to the caller rather than being silently
    /// swallowed by `()`. Phase 3a's only persistent state lives here;
    /// dropping these errors masks data-durability regressions.
    shutdown_tx: mpsc::Sender<tokio::sync::oneshot::Sender<Result<(), SyncError>>>,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl SyncEngine {
    /// Construct the engine and spawn its internal task.
    ///
    /// `kt` derives the AEAD keys; `device_id` is the local device's
    /// HLC source; `state` and `tracker` are shared with the rest
    /// of the app via the same `Arc<Mutex<_>>`s.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        kt: Arc<KeyTree>,
        device_id: String,
        state: Arc<Mutex<OwnerState>>,
        tracker: Arc<Mutex<BTreeMap<String, Hlc>>>,
        content_store: Arc<dyn ContentStore>,
        publisher_tx: mpsc::Sender<Vec<u8>>,
        subscriber_rx: mpsc::Receiver<Vec<u8>>,
        paths: PersistPaths,
        debounce_ms: u64,
    ) -> Self {
        let notify_dirty = Arc::new(Notify::new());
        let has_pending_dirty = Arc::new(AtomicBool::new(false));
        let (flush_now_tx, flush_now_rx) = mpsc::channel(8);
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

        let task = tokio::spawn(internal_task(InternalCtx {
            kt,
            device_id,
            state,
            tracker,
            content_store,
            publisher_tx,
            subscriber_rx,
            paths,
            debounce: std::time::Duration::from_millis(debounce_ms),
            notify_dirty: Arc::clone(&notify_dirty),
            has_pending_dirty: Arc::clone(&has_pending_dirty),
            flush_now_rx,
            shutdown_rx,
        }));

        SyncEngine {
            notify_dirty,
            has_pending_dirty,
            flush_now_tx,
            shutdown_tx,
            task: Mutex::new(Some(task)),
        }
    }

    /// Hint that local CRDT state has mutated and a debounced
    /// publish should fire after `debounce_ms`. Non-blocking.
    pub fn notify_dirty(&self) {
        self.has_pending_dirty.store(true, Ordering::Relaxed);
        self.notify_dirty.notify_one();
    }

    /// Force an immediate publish, bypassing the debounce window.
    /// Returns when the publish has been written to the outbound
    /// channel and any persistence flush has completed.
    ///
    /// Unconditionally publishes even when the engine has no pending
    /// dirty state — this differs from the implicit shutdown flush,
    /// which is gated on `has_pending_dirty`. The "force publish"
    /// semantics are intentional for callers that need a fence-style
    /// sync point (tests, explicit "sync now" UI). On an idle engine
    /// the publish carries an advanced HLC but identical content, so
    /// peers see one extra encrypt/decrypt round-trip — acceptable for
    /// the cases that opt in to this method.
    pub async fn flush_now(&self) -> Result<(), SyncError> {
        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        self.flush_now_tx
            .send(resp_tx)
            .await
            .map_err(|_| SyncError::TransportClosed)?;
        resp_rx.await.map_err(|_| SyncError::TransportClosed)?
    }

    /// Stop the internal task, flushing any pending writes first.
    /// Must be called explicitly during graceful shutdown — `Drop`
    /// is best-effort only.
    ///
    /// Returns `Err(SyncError)` if the final publish or persist pass
    /// failed. Callers should log this rather than swallow it: a
    /// silent failure here means the very last delta a user made
    /// before quitting was not durably persisted.
    ///
    /// If the engine task was already gone (channel closed before our
    /// send landed), returns `Ok(())` — there was nothing to flush.
    ///
    /// Note: we DO NOT `handle.await` the JoinHandle. The internal
    /// task is spawned via `tokio::spawn` on whatever runtime called
    /// `SyncEngine::new` (Tauri's main runtime in production); but
    /// `stop_inner` invokes `shutdown()` from a fresh current-thread
    /// runtime via `std::thread::scope`. Awaiting a JoinHandle from
    /// a different runtime than the one it was spawned on is not part
    /// of tokio's documented contract and risks deadlocking under
    /// future tokio releases. The `resp_rx.await` already gives the
    /// flush-complete guarantee — the task sends on `resp_tx` as the
    /// LAST step before `return`, so dropping the JoinHandle just
    /// lets the task's final stack-pop happen on its own runtime.
    pub async fn shutdown(&self) -> Result<(), SyncError> {
        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        let result = if self.shutdown_tx.send(resp_tx).await.is_ok() {
            // The task receives, runs the final flush, then drops the
            // oneshot sender. If oneshot resolves to Err the task
            // panicked or was cancelled mid-flush.
            resp_rx.await.unwrap_or(Ok(()))
        } else {
            Ok(())
        };
        // Drop the JoinHandle without awaiting it (see doc above for why).
        let _ = self.task.lock().await.take();
        result
    }
}

struct InternalCtx {
    kt: Arc<KeyTree>,
    device_id: String,
    state: Arc<Mutex<OwnerState>>,
    tracker: Arc<Mutex<BTreeMap<String, Hlc>>>,
    content_store: Arc<dyn ContentStore>,
    publisher_tx: mpsc::Sender<Vec<u8>>,
    subscriber_rx: mpsc::Receiver<Vec<u8>>,
    /// Persistence paths — used by Tasks 13+ for CRDT + replay-tracker flush.
    paths: PersistPaths,
    debounce: std::time::Duration,
    notify_dirty: Arc<Notify>,
    has_pending_dirty: Arc<AtomicBool>,
    flush_now_rx: mpsc::Receiver<tokio::sync::oneshot::Sender<Result<(), SyncError>>>,
    shutdown_rx: mpsc::Receiver<tokio::sync::oneshot::Sender<Result<(), SyncError>>>,
}

async fn internal_task(mut ctx: InternalCtx) {
    use std::time::Instant;

    let mut next_wakeup: Option<Instant> = None;
    // Latched after we observe `None` on `subscriber_rx`. Without this,
    // select! polls a closed channel every iteration and the arm-pattern
    // `Some(bytes) = recv()` silently filters out None — inbound sync
    // is permanently dead with no surfaced signal. The latch lets us
    // log once and gate the arm off so the engine's other branches
    // (notify_dirty, flush_now, shutdown) keep working as a degraded
    // publish-only mode.
    let mut inbound_closed = false;

    // Pin the `Notified` future OUTSIDE the loop so its state persists
    // across iterations. `tokio::select!` polls every branch in each
    // iteration; when both `notified()` and another branch are Ready
    // simultaneously, select picks one and drops the &mut reference to
    // the others. If `notified()` was constructed inside the loop, that
    // drop discards the consumed permit and the wakeup is silently
    // lost. Pinning outside means the underlying `Notified` survives
    // the dropped &mut, retains its `Done(consumed)` state, and the
    // next iteration's poll returns `Ready` immediately — the permit's
    // effect is preserved regardless of which branch select picked.
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
            .unwrap_or(std::time::Duration::from_secs(3600));

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
                ctx.has_pending_dirty.store(false, Ordering::Relaxed);
                if let Err(e) = publish_root_now(&ctx).await {
                    tracing::warn!(error = %e, "publish_root_now failed");
                }
                if let Err(e) = persist_both(&ctx.state, &ctx.tracker, &ctx.paths).await {
                    tracing::warn!(error = %e, "persist_both failed");
                }
            }
            Some(resp_tx) = ctx.flush_now_rx.recv() => {
                // `flush_now` ALWAYS publishes, even on an idle engine
                // (no `has_pending_dirty` guard, unlike the shutdown
                // arm). This is intentional — see the public-facing
                // doc on `SyncEngine::flush_now`. The cost is one
                // extra Zenoh put + AES-GCM round-trip if the engine
                // happens to be idle, which is acceptable for tests
                // and explicit "force publish" callers.
                next_wakeup = None;
                ctx.has_pending_dirty.store(false, Ordering::Relaxed);
                let pub_result = publish_root_now(&ctx).await;
                let persist_result = persist_both(&ctx.state, &ctx.tracker, &ctx.paths).await;
                let result = pub_result.and(persist_result);
                let _ = resp_tx.send(result);
            }
            maybe_bytes = ctx.subscriber_rx.recv(), if !inbound_closed => {
                let Some(bytes) = maybe_bytes else {
                    // The Zenoh adapter or whoever owned `inbound_tx`
                    // dropped it. Log loudly so this surfaces in the
                    // Tauri-side logs, then latch the arm off. Engine
                    // remains alive in publish-only mode; user can
                    // still mutate local state and persist.
                    tracing::error!(
                        "owner-state inbound subscriber channel closed; \
                         sync inbound disabled (engine continuing in publish-only mode)"
                    );
                    inbound_closed = true;
                    continue;
                };
                let outcome = handle_incoming_publish(&mut ctx, bytes).await;
                if let Some(err) = outcome.error() {
                    tracing::warn!(error = %err, "incoming publish dropped");
                }
                if outcome.needs_persist() {
                    if let Err(e) =
                        persist_both(&ctx.state, &ctx.tracker, &ctx.paths).await
                    {
                        tracing::warn!(error = %e, "persist_both failed");
                    }
                }
            }
            Some(resp_tx) = ctx.shutdown_rx.recv() => {
                // Flush only if there is genuinely unpublished dirty state.
                let pub_result = if ctx.has_pending_dirty.load(Ordering::Relaxed) {
                    publish_root_now(&ctx).await
                } else {
                    Ok(())
                };
                let persist_result =
                    persist_both(&ctx.state, &ctx.tracker, &ctx.paths).await;
                // Surface either failure to the caller. Persist failure
                // is the more critical of the two — losing the final
                // disk flush silently corrupts the next-boot replay
                // tracker / CRDT state.
                let _ = resp_tx.send(pub_result.and(persist_result));
                return;
            }
        }
    }
}

use crate::owner_state_crypto::{
    canonical_cbor_decode, canonical_cbor_encode, decrypt_entry, decrypt_root_publish,
    encrypt_entry, encrypt_root_publish, space_lookup_key,
};
use crate::owner_state_types::{ContentId, RootPublishPayload};

/// Lookup-key tag for the single-blob OwnerState in 3a's
/// simplified CAS layout. See spec §"Root blob shape — Phase 3a
/// simplification". Phase 3b/c restructures into per-entry blobs.
const OWNER_STATE_ROOT_BLOB_TAG: &[u8] = b"owner-state-root-blob-v1";

async fn persist_both(
    state: &Arc<Mutex<OwnerState>>,
    tracker: &Arc<Mutex<BTreeMap<String, Hlc>>>,
    paths: &PersistPaths,
) -> Result<(), SyncError> {
    // Snapshot under the async locks, then hop to a blocking thread for
    // the actual file I/O. `save_crdt` / `save_replay` call `write_all`
    // + `sync_all` (fsync) + `persist` (atomic rename) + (on Unix) a
    // directory fsync — all blocking syscalls. Running them directly
    // on the tokio runtime stalls the worker for the full fsync cost
    // and starves debounce timers / inbound publishes.
    let state_snap = state.lock().await.clone();
    let tracker_snap = tracker.lock().await.clone();
    let paths = paths.clone();
    tokio::task::spawn_blocking(move || -> Result<(), SyncError> {
        crate::owner_state_persist::save_crdt(&paths.crdt, &state_snap)?;
        crate::owner_state_persist::save_replay(&paths.replay, &tracker_snap)?;
        Ok(())
    })
    .await
    .map_err(|e| {
        SyncError::Persist(crate::owner_state_persist::PersistError::Io(
            std::io::Error::other(format!("spawn_blocking join: {e}")),
        ))
    })??;
    Ok(())
}

async fn publish_root_now(ctx: &InternalCtx) -> Result<(), SyncError> {
    // Snapshot CRDT state under brief lock.
    let snapshot = {
        let state = ctx.state.lock().await;
        state.clone()
    };

    // 1. Canonical-CBOR encode the OwnerState as the cleartext "root blob."
    let blob_cleartext =
        canonical_cbor_encode(&snapshot).map_err(|e| SyncError::CborEncode(e.to_string()))?;

    // 2. Encrypt with deterministic per-entry AEAD using the fixed
    //    owner-state-root lookup key, so cipher_cid is reproducible
    //    across two devices encrypting the same state.
    let lookup = space_lookup_key(&ctx.kt, OWNER_STATE_ROOT_BLOB_TAG);
    let blob_ciphertext = encrypt_entry(&ctx.kt, &lookup, &blob_cleartext)
        .map_err(|e| SyncError::Crypto(e.to_string()))?;

    // 3. cipher_cid = BLAKE3 of the encrypted blob.
    let root_cid = ContentId(blake3::hash(&blob_ciphertext).into());

    // 4. Put into ContentStore (in 3a: InMemoryStub; 3b: real CAS).
    ctx.content_store.put(root_cid, blob_ciphertext)?;

    // 5. Build state-root payload.
    let now = next_hlc(ctx).await;
    let payload = RootPublishPayload { root_cid, at: now };
    let payload_bytes =
        canonical_cbor_encode(&payload).map_err(|e| SyncError::CborEncode(e.to_string()))?;

    // 6. Encrypt with random-nonce root AEAD (Phase 1).
    let wire = encrypt_root_publish(&ctx.kt, &payload_bytes)
        .map_err(|e| SyncError::Crypto(e.to_string()))?;

    // 7. Send onto outbound channel — Zenoh adapter forwards.
    ctx.publisher_tx
        .send(wire)
        .await
        .map_err(|_| SyncError::TransportClosed)?;

    Ok(())
}

/// Build a strictly-newer HLC than the last one we published. The
/// internal task is single-threaded so we don't need atomic ops;
/// caller holds an `&mut self` to the task's local state in a real
/// design, but for now we re-derive from system time + a per-task
/// monotonic counter cached in `ctx.tracker` keyed by our own
/// device_id.
async fn next_hlc(ctx: &InternalCtx) -> Hlc {
    use std::time::{SystemTime, UNIX_EPOCH};
    let wall_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let mut tracker = ctx.tracker.lock().await;
    // Read once — both the logical-counter computation and the
    // effective-wall pin need the same `prev`. Re-fetching from the
    // map is cheap but invites future divergence; folding into one
    // lookup keeps the read consistent.
    //
    // `saturating_add` on the logical counter: under sustained
    // backward NTP correction or repeated clock-monotonicity faults
    // we'd repeatedly bump `logical` without advancing `wall_ms`. An
    // unchecked u32 add would eventually wrap and produce an HLC
    // smaller than the previous one, breaking the strict-newer
    // monotonicity that replay protection depends on. Saturation
    // pins the value at u32::MAX instead — pathological but
    // bounded; further publishes from the same device on the same
    // wall_ms tick would be rejected by the receiver until the
    // wall clock advances, which is preferable to silent replay.
    let prev = tracker.get(&ctx.device_id).cloned();
    let (logical, prev_wall) = match prev.as_ref() {
        Some(p) if p.wall_ms == wall_ms => (p.logical.saturating_add(1), p.wall_ms),
        Some(p) if p.wall_ms > wall_ms => (p.logical.saturating_add(1), p.wall_ms),
        Some(p) => (0, p.wall_ms),
        None => (0, 0),
    };
    let effective_wall = std::cmp::max(wall_ms, prev_wall);

    let now = Hlc {
        wall_ms: effective_wall,
        logical,
        device_id: ctx.device_id.clone(),
    };
    tracker.insert(ctx.device_id.clone(), now.clone());
    now
}

/// Outcome of processing one inbound state-root publish. The variants
/// distinguish where the failure happened so the caller persists only
/// when local state actually changed — fsync per malformed wire packet
/// is wasteful, and persisting only on `Mutated | ErrPostMutation`
/// matches the single state-mutation point (the replay-tracker
/// `tracker.insert(...)` after the strictly-newer check).
#[derive(Debug)]
enum IncomingOutcome {
    /// Replay-rejected as a duplicate. No state change. Don't persist.
    Duplicate,
    /// Tracker advanced AND remote merge applied. Persist.
    Mutated,
    /// Failure occurred BEFORE the tracker advanced (decrypt-root,
    /// payload decode, or replay-check itself). No state change.
    /// Don't persist — disk is already consistent with memory.
    ErrPreMutation(SyncError),
    /// Failure occurred AFTER the tracker advanced but before the
    /// merge completed (blob fetch, blob decrypt, blob decode).
    /// Tracker is in-memory dirty; persist defensively so a restart
    /// doesn't replay the same publish.
    ErrPostMutation(SyncError),
}

impl IncomingOutcome {
    fn needs_persist(&self) -> bool {
        matches!(self, Self::Mutated | Self::ErrPostMutation(_))
    }

    fn error(&self) -> Option<&SyncError> {
        match self {
            Self::ErrPreMutation(e) | Self::ErrPostMutation(e) => Some(e),
            Self::Duplicate | Self::Mutated => None,
        }
    }
}

/// Process an incoming publish. See `IncomingOutcome` for the
/// return-value semantics.
#[allow(clippy::needless_pass_by_ref_mut)]
async fn handle_incoming_publish(ctx: &mut InternalCtx, wire: Vec<u8>) -> IncomingOutcome {
    // 1. Decrypt the Zenoh wire payload.
    let payload_bytes = match decrypt_root_publish(&ctx.kt, &wire) {
        Ok(b) => b,
        Err(e) => return IncomingOutcome::ErrPreMutation(SyncError::Crypto(e.to_string())),
    };
    let payload: RootPublishPayload = match canonical_cbor_decode(&payload_bytes) {
        Ok(p) => p,
        Err(e) => return IncomingOutcome::ErrPreMutation(SyncError::CborDecode(e.to_string())),
    };

    // 2. Replay protection. Tracker mutation is the single
    //    "post-mutation" boundary: every error past this point must
    //    persist; every error before it must NOT.
    {
        let mut tracker = ctx.tracker.lock().await;
        let accept = match tracker.get(&payload.at.device_id) {
            None => true,
            Some(existing) => payload.at.is_strictly_newer_than(existing),
        };
        if !accept {
            return IncomingOutcome::Duplicate;
        }
        tracker.insert(payload.at.device_id.clone(), payload.at.clone());
    }

    // 3. Fetch the encrypted root blob from CAS.
    let blob_ciphertext = match ctx.content_store.get(&payload.root_cid) {
        Ok(Some(b)) => b,
        Ok(None) => {
            // Phase 3b will replace InMemoryStub with real CAS; for
            // 3a, a missing blob means the subscriber and publisher
            // aren't sharing the same stub (e.g. cross-process). Log
            // and skip — never panic.
            return IncomingOutcome::ErrPostMutation(SyncError::Crypto(
                "ContentStore returned None for root_cid".into(),
            ));
        }
        Err(e) => return IncomingOutcome::ErrPostMutation(SyncError::ContentStore(e)),
    };

    // 4. Decrypt with the same lookup key the publisher used.
    let lookup = space_lookup_key(&ctx.kt, OWNER_STATE_ROOT_BLOB_TAG);
    let blob_cleartext = match decrypt_entry(&ctx.kt, &lookup, &blob_ciphertext) {
        Ok(b) => b,
        Err(e) => return IncomingOutcome::ErrPostMutation(SyncError::Crypto(e.to_string())),
    };

    // 5. Decode into a remote OwnerState snapshot.
    let remote: OwnerState = match canonical_cbor_decode(&blob_cleartext) {
        Ok(s) => s,
        Err(e) => return IncomingOutcome::ErrPostMutation(SyncError::CborDecode(e.to_string())),
    };

    // 6. Merge each entry through Phase 2's CRDT methods. Order
    //    matters slightly — Spaces must merge first because outbox/
    //    inbox/markers reference SpaceIds that the canonicalization
    //    rewrite needs to see resolved.
    {
        let mut local = ctx.state.lock().await;
        for (_, space) in remote.spaces {
            local.apply_space_with_canonicalization(space);
        }
        for (_, entry) in remote.outbox {
            local.apply_outbox(entry);
        }
        for (_, entry) in remote.inbox {
            local.apply_inbox(entry);
        }
        for (_, marker) in remote.markers {
            local.apply_marker(marker);
        }
        for tomb in remote.tombstones {
            local.tombstones.insert(tomb);
        }
    }

    // Tracker advanced and merge ran.
    IncomingOutcome::Mutated
}

#[cfg(test)]
mod debounce_tests {
    use super::*;
    use crate::content_store::InMemoryStub;
    use std::time::Duration;

    fn make_kt() -> Arc<KeyTree> {
        Arc::new(KeyTree::derive(&[0u8; 32]).expect("kt"))
    }

    fn paths() -> (tempfile::TempDir, PersistPaths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = PersistPaths {
            crdt: dir.path().join("crdt.cbor"),
            replay: dir.path().join("replay.cbor"),
        };
        (dir, paths)
    }

    /// One notify_dirty fires exactly one publish after the debounce.
    #[tokio::test]
    async fn single_notify_dirty_fires_one_publish() {
        let (pub_tx, mut pub_rx) = mpsc::channel(16);
        let (_sub_tx, sub_rx) = mpsc::channel(16);
        let (_dir, paths) = paths();
        let engine = SyncEngine::new(
            make_kt(),
            "test-device".into(),
            Arc::new(Mutex::new(OwnerState::default())),
            Arc::new(Mutex::new(BTreeMap::new())),
            Arc::new(InMemoryStub::default()),
            pub_tx,
            sub_rx,
            paths,
            50, // shorter debounce for tests
        );

        engine.notify_dirty();
        // Should fire within ~50ms; allow 500ms slack.
        let bytes = tokio::time::timeout(Duration::from_millis(500), pub_rx.recv())
            .await
            .expect("publish within timeout")
            .expect("not closed");
        assert!(!bytes.is_empty(), "publish bytes should be non-empty");
        let _ = engine.shutdown().await;
    }

    /// 50 rapid notify_dirty calls within one debounce window
    /// collapse to exactly one publish.
    #[tokio::test]
    async fn rapid_notify_dirty_collapses_to_one_publish() {
        let (pub_tx, mut pub_rx) = mpsc::channel(64);
        let (_sub_tx, sub_rx) = mpsc::channel(16);
        let (_dir, paths) = paths();
        let engine = SyncEngine::new(
            make_kt(),
            "test-device".into(),
            Arc::new(Mutex::new(OwnerState::default())),
            Arc::new(Mutex::new(BTreeMap::new())),
            Arc::new(InMemoryStub::default()),
            pub_tx,
            sub_rx,
            paths,
            100, // 100ms debounce
        );

        for _ in 0..50 {
            engine.notify_dirty();
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        // Wait long enough for the debounce to fire.
        tokio::time::sleep(Duration::from_millis(200)).await;
        // Drain channel and count publishes.
        let mut count = 0;
        while let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(50), pub_rx.recv()).await
        {
            count += 1;
        }
        assert_eq!(count, 1, "expected exactly one publish, got {}", count);
        let _ = engine.shutdown().await;
    }

    #[tokio::test]
    async fn flush_now_fires_immediately() {
        let (pub_tx, mut pub_rx) = mpsc::channel(16);
        let (_sub_tx, sub_rx) = mpsc::channel(16);
        let (_dir, paths) = paths();
        let engine = SyncEngine::new(
            make_kt(),
            "test-device".into(),
            Arc::new(Mutex::new(OwnerState::default())),
            Arc::new(Mutex::new(BTreeMap::new())),
            Arc::new(InMemoryStub::default()),
            pub_tx,
            sub_rx,
            paths,
            5000, // very long debounce — flush_now must beat it
        );

        engine.flush_now().await.unwrap();
        // Must fire within ~50ms — well below the 5000ms debounce.
        let bytes = tokio::time::timeout(Duration::from_millis(200), pub_rx.recv())
            .await
            .expect("publish within timeout")
            .expect("not closed");
        assert!(!bytes.is_empty());
        let _ = engine.shutdown().await;
    }

    #[tokio::test]
    async fn flush_now_cancels_pending_wakeup() {
        let (pub_tx, mut pub_rx) = mpsc::channel(16);
        let (_sub_tx, sub_rx) = mpsc::channel(16);
        let (_dir, paths) = paths();
        let engine = SyncEngine::new(
            make_kt(),
            "test-device".into(),
            Arc::new(Mutex::new(OwnerState::default())),
            Arc::new(Mutex::new(BTreeMap::new())),
            Arc::new(InMemoryStub::default()),
            pub_tx,
            sub_rx,
            paths,
            200,
        );

        engine.notify_dirty();
        // Don't wait for the debounce — call flush_now immediately.
        engine.flush_now().await.unwrap();
        // Drain — should see exactly one publish (flush_now's), not two.
        tokio::time::sleep(Duration::from_millis(400)).await;
        let mut count = 0;
        while let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(50), pub_rx.recv()).await
        {
            count += 1;
        }
        assert_eq!(count, 1, "flush_now should cancel pending wakeup");
        let _ = engine.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_flushes_pending_publish() {
        let (pub_tx, mut pub_rx) = mpsc::channel(16);
        let (_sub_tx, sub_rx) = mpsc::channel(16);
        let (_dir, paths) = paths();
        let engine = SyncEngine::new(
            make_kt(),
            "test-device".into(),
            Arc::new(Mutex::new(OwnerState::default())),
            Arc::new(Mutex::new(BTreeMap::new())),
            Arc::new(InMemoryStub::default()),
            pub_tx,
            sub_rx,
            paths,
            5000, // long debounce — shutdown must short-circuit it
        );

        engine.notify_dirty();
        let _ = engine.shutdown().await;
        // After shutdown, the pending publish must already have fired.
        let bytes = pub_rx.try_recv().expect("pending publish flushed");
        assert!(!bytes.is_empty());
    }

    #[tokio::test]
    async fn shutdown_without_pending_writes_does_not_publish() {
        let (pub_tx, mut pub_rx) = mpsc::channel(16);
        let (_sub_tx, sub_rx) = mpsc::channel(16);
        let (_dir, paths) = paths();
        let engine = SyncEngine::new(
            make_kt(),
            "test-device".into(),
            Arc::new(Mutex::new(OwnerState::default())),
            Arc::new(Mutex::new(BTreeMap::new())),
            Arc::new(InMemoryStub::default()),
            pub_tx,
            sub_rx,
            paths,
            5000,
        );

        let _ = engine.shutdown().await;
        // No notify_dirty was called, so nothing to flush.
        assert!(pub_rx.try_recv().is_err());
    }
}

#[cfg(test)]
mod skeleton_tests {
    use super::*;
    use crate::content_store::InMemoryStub;

    fn make_kt() -> Arc<KeyTree> {
        Arc::new(KeyTree::derive(&[0u8; 32]).expect("kt"))
    }

    #[tokio::test]
    async fn construct_and_shutdown_clean() {
        let (pub_tx, _pub_rx) = mpsc::channel(16);
        let (_sub_tx, sub_rx) = mpsc::channel(16);
        let dir = tempfile::tempdir().unwrap();
        let paths = PersistPaths {
            crdt: dir.path().join("crdt.cbor"),
            replay: dir.path().join("replay.cbor"),
        };
        let engine = SyncEngine::new(
            make_kt(),
            "test-device".into(),
            Arc::new(Mutex::new(OwnerState::default())),
            Arc::new(Mutex::new(BTreeMap::new())),
            Arc::new(InMemoryStub::default()),
            pub_tx,
            sub_rx,
            paths,
            DEFAULT_DEBOUNCE_MS,
        );
        let _ = engine.shutdown().await;
        // No assertions beyond "didn't hang or panic."
    }
}

#[cfg(test)]
mod subscriber_tests {
    use super::*;
    use crate::content_store::InMemoryStub;
    use crate::owner_state_crypto::{
        canonical_cbor_encode, encrypt_entry, encrypt_root_publish, space_lookup_key,
    };
    use crate::owner_state_types::RootPublishPayload;
    use std::time::Duration;

    fn make_kt() -> Arc<KeyTree> {
        Arc::new(KeyTree::derive(&[7u8; 32]).expect("kt"))
    }

    fn paths() -> (tempfile::TempDir, PersistPaths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = PersistPaths {
            crdt: dir.path().join("crdt.cbor"),
            replay: dir.path().join("replay.cbor"),
        };
        (dir, paths)
    }

    /// Build a wire payload for testing — re-uses the publisher's
    /// encryption path but with a controlled HLC.
    fn make_wire(
        kt: &Arc<KeyTree>,
        store: &Arc<dyn ContentStore>,
        state: &OwnerState,
        device_id: &str,
        wall_ms: u64,
        logical: u32,
    ) -> Vec<u8> {
        let blob_cleartext = canonical_cbor_encode(state).unwrap();
        let lookup = space_lookup_key(kt, b"owner-state-root-blob-v1");
        let blob_ciphertext = encrypt_entry(kt, &lookup, &blob_cleartext).unwrap();
        let root_cid = ContentId(blake3::hash(&blob_ciphertext).into());
        store.put(root_cid, blob_ciphertext).unwrap();
        let payload = RootPublishPayload {
            root_cid,
            at: Hlc {
                wall_ms,
                logical,
                device_id: device_id.into(),
            },
        };
        let payload_bytes = canonical_cbor_encode(&payload).unwrap();
        encrypt_root_publish(kt, &payload_bytes).unwrap()
    }

    #[tokio::test]
    async fn subscriber_accepts_strictly_newer_hlc_and_updates_tracker() {
        let (pub_tx, _pub_rx) = mpsc::channel(16);
        let (sub_tx, sub_rx) = mpsc::channel(16);
        let (_dir, paths) = paths();
        let kt = make_kt();
        let store = Arc::new(InMemoryStub::default()) as Arc<dyn ContentStore>;
        let tracker = Arc::new(Mutex::new(BTreeMap::new()));
        let state = Arc::new(Mutex::new(OwnerState::default()));
        let engine = SyncEngine::new(
            Arc::clone(&kt),
            "self-device".into(),
            Arc::clone(&state),
            Arc::clone(&tracker),
            Arc::clone(&store),
            pub_tx,
            sub_rx,
            paths,
            5000, // long debounce — keep self-publishes out of the way
        );

        let wire = make_wire(&kt, &store, &OwnerState::default(), "peer-bob", 1000, 0);
        sub_tx.send(wire).await.unwrap();
        // Give the subscriber branch a moment to process.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let t = tracker.lock().await;
        let stored = t.get("peer-bob").expect("peer accepted");
        assert_eq!(stored.wall_ms, 1000);
        assert_eq!(stored.logical, 0);
        drop(t);

        let _ = engine.shutdown().await;
    }

    #[tokio::test]
    async fn subscriber_rejects_strictly_older_hlc() {
        let (pub_tx, _pub_rx) = mpsc::channel(16);
        let (sub_tx, sub_rx) = mpsc::channel(16);
        let (_dir, paths) = paths();
        let kt = make_kt();
        let store = Arc::new(InMemoryStub::default()) as Arc<dyn ContentStore>;
        let tracker = Arc::new(Mutex::new(BTreeMap::new()));
        let state = Arc::new(Mutex::new(OwnerState::default()));
        let engine = SyncEngine::new(
            Arc::clone(&kt),
            "self-device".into(),
            Arc::clone(&state),
            Arc::clone(&tracker),
            Arc::clone(&store),
            pub_tx,
            sub_rx,
            paths,
            5000,
        );

        // First publish: at=2000.
        sub_tx
            .send(make_wire(
                &kt,
                &store,
                &OwnerState::default(),
                "peer-bob",
                2000,
                0,
            ))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Replay: at=1000 (older). Tracker must NOT regress.
        sub_tx
            .send(make_wire(
                &kt,
                &store,
                &OwnerState::default(),
                "peer-bob",
                1000,
                0,
            ))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let t = tracker.lock().await;
        let stored = t.get("peer-bob").expect("still present");
        assert_eq!(stored.wall_ms, 2000, "tracker must not regress");
        drop(t);

        let _ = engine.shutdown().await;
    }

    use crate::owner_state_types::{
        ContentId, DeliveryStatus, OutboxEntry, OutboxEntryId, OwnerAddr, ReadMarker, Space,
        SpaceId, SpaceKind,
    };

    fn folder(id: u8, ts: u64) -> Space {
        Space {
            id: SpaceId([id; 16]),
            kind: SpaceKind::Folder,
            parent: None,
            community_id: None,
            name: "F".into(),
            transport: None,
            members: vec![],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: Hlc {
                wall_ms: ts,
                logical: 0,
                device_id: "test".into(),
            },
            updated_at: Hlc {
                wall_ms: ts,
                logical: 0,
                device_id: "test".into(),
            },
        }
    }

    #[tokio::test]
    async fn subscriber_fetches_and_merges_remote_state() {
        let (pub_tx, _pub_rx) = mpsc::channel(16);
        let (sub_tx, sub_rx) = mpsc::channel(16);
        let (_dir, paths) = paths();
        let kt = make_kt();
        let store = Arc::new(InMemoryStub::default()) as Arc<dyn ContentStore>;
        let local_state = Arc::new(Mutex::new(OwnerState::default()));
        let engine = SyncEngine::new(
            Arc::clone(&kt),
            "self-device".into(),
            Arc::clone(&local_state),
            Arc::new(Mutex::new(BTreeMap::new())),
            Arc::clone(&store),
            pub_tx,
            sub_rx,
            paths,
            5000,
        );

        // Build a remote OwnerState containing a folder id=42.
        let mut remote = OwnerState::default();
        remote.spaces.insert(SpaceId([42; 16]), folder(42, 100));

        let wire = make_wire(&kt, &store, &remote, "peer-bob", 1000, 0);
        sub_tx.send(wire).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        let local = local_state.lock().await;
        assert!(
            local.spaces.contains_key(&SpaceId([42; 16])),
            "remote folder must merge into local"
        );
        drop(local);

        let _ = engine.shutdown().await;
    }

    #[tokio::test]
    async fn subscriber_merges_outbox_inbox_marker_entries() {
        let (pub_tx, _pub_rx) = mpsc::channel(16);
        let (sub_tx, sub_rx) = mpsc::channel(16);
        let (_dir, paths) = paths();
        let kt = make_kt();
        let store = Arc::new(InMemoryStub::default()) as Arc<dyn ContentStore>;
        let local_state = Arc::new(Mutex::new(OwnerState::default()));
        let engine = SyncEngine::new(
            Arc::clone(&kt),
            "self-device".into(),
            Arc::clone(&local_state),
            Arc::new(Mutex::new(BTreeMap::new())),
            Arc::clone(&store),
            pub_tx,
            sub_rx,
            paths,
            5000,
        );

        let mut remote = OwnerState::default();
        remote.spaces.insert(SpaceId([1; 16]), folder(1, 100));
        remote.outbox.insert(
            OutboxEntryId([7; 16]),
            OutboxEntry {
                id: OutboxEntryId([7; 16]),
                space_id: SpaceId([1; 16]),
                recipient_owners: vec![OwnerAddr([2; 16])],
                message_cid: ContentId([3; 32]),
                created_at: Hlc {
                    wall_ms: 100,
                    logical: 0,
                    device_id: "peer".into(),
                },
                delivered_to: Default::default(),
                delivery_status: DeliveryStatus::Pending,
            },
        );
        remote.markers.insert(
            SpaceId([1; 16]),
            ReadMarker {
                space_id: SpaceId([1; 16]),
                last_read_at: Hlc {
                    wall_ms: 200,
                    logical: 0,
                    device_id: "peer".into(),
                },
            },
        );

        let wire = make_wire(&kt, &store, &remote, "peer-bob", 1000, 0);
        sub_tx.send(wire).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        let local = local_state.lock().await;
        assert!(local.spaces.contains_key(&SpaceId([1; 16])));
        assert!(local.outbox.contains_key(&OutboxEntryId([7; 16])));
        assert!(local.markers.contains_key(&SpaceId([1; 16])));
        drop(local);

        let _ = engine.shutdown().await;
    }

    #[tokio::test]
    async fn subscriber_logs_and_skips_when_blob_missing() {
        // Build a wire payload but DON'T put the blob in the store —
        // simulate cross-process / cross-device case where the
        // publisher and subscriber don't share their stubs.
        let (pub_tx, _pub_rx) = mpsc::channel(16);
        let (sub_tx, sub_rx) = mpsc::channel(16);
        let (_dir, paths) = paths();
        let kt = make_kt();
        let store_publisher = Arc::new(InMemoryStub::default()) as Arc<dyn ContentStore>;
        let store_subscriber = Arc::new(InMemoryStub::default()) as Arc<dyn ContentStore>;
        let local_state = Arc::new(Mutex::new(OwnerState::default()));
        let tracker = Arc::new(Mutex::new(BTreeMap::new()));
        let engine = SyncEngine::new(
            Arc::clone(&kt),
            "self-device".into(),
            Arc::clone(&local_state),
            Arc::clone(&tracker),
            Arc::clone(&store_subscriber), // subscriber's stub is empty
            pub_tx,
            sub_rx,
            paths,
            5000,
        );

        let mut remote = OwnerState::default();
        remote.spaces.insert(SpaceId([42; 16]), folder(42, 100));

        // Publisher puts the blob in its OWN stub; subscriber's
        // stub never receives it.
        let wire = make_wire(&kt, &store_publisher, &remote, "peer-bob", 1000, 0);
        sub_tx.send(wire).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Subscriber must NOT have merged — local stays empty.
        let local = local_state.lock().await;
        assert!(
            local.spaces.is_empty(),
            "subscriber should have skipped the merge for missing blob"
        );
        drop(local);

        // BUT replay tracker should still have advanced — we accepted
        // the publish, just couldn't fetch the data. That's OK because
        // the next publish from the same peer will carry a newer HLC
        // and a new (hopefully present) root_cid.
        let t = tracker.lock().await;
        assert!(t.contains_key("peer-bob"), "tracker must still record");
        drop(t);

        let _ = engine.shutdown().await;
    }

    #[tokio::test]
    async fn replay_tracker_survives_engine_restart() {
        let (pub_tx, _pub_rx) = mpsc::channel(16);
        let (sub_tx, sub_rx) = mpsc::channel(16);
        let dir = tempfile::tempdir().unwrap();
        let paths = PersistPaths {
            crdt: dir.path().join("crdt.cbor"),
            replay: dir.path().join("replay.cbor"),
        };
        let kt = make_kt();
        let store = Arc::new(InMemoryStub::default()) as Arc<dyn ContentStore>;

        // Round 1: bring up engine, accept a publish, shut down.
        {
            let tracker = Arc::new(Mutex::new(BTreeMap::new()));
            let state = Arc::new(Mutex::new(OwnerState::default()));
            let engine = SyncEngine::new(
                Arc::clone(&kt),
                "self-device".into(),
                Arc::clone(&state),
                Arc::clone(&tracker),
                Arc::clone(&store),
                pub_tx.clone(),
                sub_rx,
                paths.clone(),
                5000,
            );
            sub_tx
                .send(make_wire(
                    &kt,
                    &store,
                    &OwnerState::default(),
                    "peer-bob",
                    5000,
                    0,
                ))
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
            let _ = engine.shutdown().await;
        }

        // Round 2: boot a fresh engine, load tracker from disk,
        // verify peer-bob's HLC is 5000. Then send an OLDER publish
        // and confirm rejection.
        let tracker_loaded = crate::owner_state_persist::load_replay(&paths.replay).unwrap();
        assert_eq!(tracker_loaded.get("peer-bob").unwrap().wall_ms, 5000);

        let (_pub_tx2, _pub_rx2) = mpsc::channel(16);
        let (sub_tx2, sub_rx2) = mpsc::channel(16);
        let tracker2 = Arc::new(Mutex::new(tracker_loaded));
        let state2 = Arc::new(Mutex::new(OwnerState::default()));
        let engine2 = SyncEngine::new(
            Arc::clone(&kt),
            "self-device".into(),
            Arc::clone(&state2),
            Arc::clone(&tracker2),
            Arc::clone(&store),
            _pub_tx2,
            sub_rx2,
            paths.clone(),
            5000,
        );
        // Send an older publish: at=2000 < 5000.
        sub_tx2
            .send(make_wire(
                &kt,
                &store,
                &OwnerState::default(),
                "peer-bob",
                2000,
                0,
            ))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        let t = tracker2.lock().await;
        assert_eq!(
            t.get("peer-bob").unwrap().wall_ms,
            5000,
            "replay tracker must reject the older HLC across restart"
        );
        drop(t);

        let _ = engine2.shutdown().await;
    }
}

#[cfg(test)]
mod publisher_tests {
    use super::*;
    use crate::content_store::InMemoryStub;
    use crate::owner_state_crypto::decrypt_root_publish;
    use crate::owner_state_types::RootPublishPayload;
    use ciborium::from_reader;

    fn make_kt() -> Arc<KeyTree> {
        Arc::new(KeyTree::derive(&[42u8; 32]).expect("kt"))
    }

    fn paths() -> (tempfile::TempDir, PersistPaths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = PersistPaths {
            crdt: dir.path().join("crdt.cbor"),
            replay: dir.path().join("replay.cbor"),
        };
        (dir, paths)
    }

    #[tokio::test]
    async fn publish_emits_decryptable_payload_with_blob_in_store() {
        let (pub_tx, mut pub_rx) = mpsc::channel(16);
        let (_sub_tx, sub_rx) = mpsc::channel(16);
        let (_dir, paths) = paths();
        let kt = make_kt();
        let store = Arc::new(InMemoryStub::default());
        let state = Arc::new(Mutex::new(OwnerState::default()));
        let engine = SyncEngine::new(
            Arc::clone(&kt),
            "alice-device".into(),
            Arc::clone(&state),
            Arc::new(Mutex::new(BTreeMap::new())),
            Arc::clone(&store) as Arc<dyn ContentStore>,
            pub_tx,
            sub_rx,
            paths,
            50,
        );

        engine.notify_dirty();
        let wire = tokio::time::timeout(std::time::Duration::from_millis(500), pub_rx.recv())
            .await
            .expect("publish within timeout")
            .expect("channel open");

        // Decrypt the wire payload with Phase-1 helper.
        let payload_bytes = decrypt_root_publish(&kt, &wire).expect("decrypt");
        let payload: RootPublishPayload = from_reader(&payload_bytes[..]).expect("CBOR decode");
        assert_eq!(payload.at.device_id, "alice-device");

        // The root_cid must reference a blob present in the stub.
        let blob = store.get(&payload.root_cid).unwrap().expect("blob present");
        assert!(!blob.is_empty());

        let _ = engine.shutdown().await;
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use crate::content_store::InMemoryStub;
    use crate::owner_state_types::{OwnerAddr, Space, SpaceId, SpaceKind, TransportBinding};
    use std::time::Duration;

    fn make_kt(seed: u8) -> Arc<KeyTree> {
        Arc::new(KeyTree::derive(&[seed; 32]).expect("kt"))
    }

    fn paths(name: &str, dir: &tempfile::TempDir) -> PersistPaths {
        PersistPaths {
            crdt: dir.path().join(format!("{}_crdt.cbor", name)),
            replay: dir.path().join(format!("{}_replay.cbor", name)),
        }
    }

    fn dm(id: u8, members: Vec<u8>, ts: u64) -> Space {
        let mut sorted = members.clone();
        sorted.sort();
        Space {
            id: SpaceId([id; 16]),
            kind: SpaceKind::Dm,
            parent: None,
            community_id: None,
            name: "DM".into(),
            transport: Some(TransportBinding::Reticulum {
                participants: vec![],
            }),
            members: sorted.into_iter().map(|i| OwnerAddr([i; 16])).collect(),
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: Hlc {
                wall_ms: ts,
                logical: 0,
                device_id: "test".into(),
            },
            updated_at: Hlc {
                wall_ms: ts,
                logical: 0,
                device_id: "test".into(),
            },
        }
    }

    /// Two SyncEngines share one InMemoryStub. A's publish flows to B
    /// via the cross-wired channels. Senders are stored to keep the
    /// forwarding tasks alive; they're not used directly so the
    /// `_`-prefix silences dead-code warnings.
    struct TwoDevices {
        a_engine: SyncEngine,
        b_engine: SyncEngine,
        a_state: Arc<Mutex<OwnerState>>,
        b_state: Arc<Mutex<OwnerState>>,
        _a_to_b_tx: mpsc::Sender<Vec<u8>>,
        _b_to_a_tx: mpsc::Sender<Vec<u8>>,
        _dir: tempfile::TempDir,
    }

    fn spawn_two_devices(kt_seed: u8) -> TwoDevices {
        let dir = tempfile::tempdir().unwrap();
        let kt = make_kt(kt_seed);
        let store = Arc::new(InMemoryStub::default()) as Arc<dyn ContentStore>;
        let a_state = Arc::new(Mutex::new(OwnerState::default()));
        let b_state = Arc::new(Mutex::new(OwnerState::default()));
        let a_tracker = Arc::new(Mutex::new(BTreeMap::new()));
        let b_tracker = Arc::new(Mutex::new(BTreeMap::new()));

        // A publishes → forwards into B's subscriber.
        let (a_pub_tx, mut a_pub_rx) = mpsc::channel::<Vec<u8>>(64);
        let (a_to_b_tx, b_sub_rx) = mpsc::channel::<Vec<u8>>(64);
        // Forwarding task: drain A's outbox into B's inbox.
        let a_to_b_forwarder = a_to_b_tx.clone();
        tokio::spawn(async move {
            while let Some(bytes) = a_pub_rx.recv().await {
                let _ = a_to_b_forwarder.send(bytes).await;
            }
        });

        // B publishes → forwards into A's subscriber.
        let (b_pub_tx, mut b_pub_rx) = mpsc::channel::<Vec<u8>>(64);
        let (b_to_a_tx, a_sub_rx) = mpsc::channel::<Vec<u8>>(64);
        let b_to_a_forwarder = b_to_a_tx.clone();
        tokio::spawn(async move {
            while let Some(bytes) = b_pub_rx.recv().await {
                let _ = b_to_a_forwarder.send(bytes).await;
            }
        });

        let a_engine = SyncEngine::new(
            Arc::clone(&kt),
            "device-a".into(),
            Arc::clone(&a_state),
            a_tracker,
            Arc::clone(&store),
            a_pub_tx,
            a_sub_rx,
            paths("a", &dir),
            50,
        );
        let b_engine = SyncEngine::new(
            Arc::clone(&kt),
            "device-b".into(),
            Arc::clone(&b_state),
            b_tracker,
            Arc::clone(&store),
            b_pub_tx,
            b_sub_rx,
            paths("b", &dir),
            50,
        );

        TwoDevices {
            a_engine,
            b_engine,
            a_state,
            b_state,
            _a_to_b_tx: a_to_b_tx,
            _b_to_a_tx: b_to_a_tx,
            _dir: dir,
        }
    }

    #[tokio::test]
    async fn one_way_convergence() {
        let dev = spawn_two_devices(123);
        // A applies a folder.
        let f = dm(1, vec![1, 2], 100);
        {
            let mut a = dev.a_state.lock().await;
            a.apply_space_with_canonicalization(f.clone());
        }
        dev.a_engine.notify_dirty();
        tokio::time::sleep(Duration::from_millis(300)).await;

        let b = dev.b_state.lock().await;
        assert!(b.spaces.contains_key(&SpaceId([1; 16])));
        drop(b);

        let _ = dev.a_engine.shutdown().await;
        let _ = dev.b_engine.shutdown().await;
    }

    #[tokio::test]
    async fn bidirectional_convergence() {
        let dev = spawn_two_devices(45);
        let dm_ab = dm(1, vec![1, 2], 100);
        let dm_cd = dm(2, vec![3, 4], 100);
        {
            let mut a = dev.a_state.lock().await;
            a.apply_space_with_canonicalization(dm_ab);
        }
        {
            let mut b = dev.b_state.lock().await;
            b.apply_space_with_canonicalization(dm_cd);
        }
        dev.a_engine.notify_dirty();
        dev.b_engine.notify_dirty();
        // Multiple debounce cycles to converge.
        tokio::time::sleep(Duration::from_millis(500)).await;

        let a = dev.a_state.lock().await;
        let b = dev.b_state.lock().await;
        assert!(a.spaces.contains_key(&SpaceId([1; 16])));
        assert!(a.spaces.contains_key(&SpaceId([2; 16])));
        assert!(b.spaces.contains_key(&SpaceId([1; 16])));
        assert!(b.spaces.contains_key(&SpaceId([2; 16])));
        drop(a);
        drop(b);

        let _ = dev.a_engine.shutdown().await;
        let _ = dev.b_engine.shutdown().await;
    }

    #[tokio::test]
    async fn cross_device_dedupe_through_sync() {
        // A and B independently create the same DM with different
        // ULIDs but the same sorted-members. After sync, both
        // converge on the smaller ULID.
        let dev = spawn_two_devices(7);
        let a_dm = dm(5, vec![1, 2], 100); // larger ULID — loser
        let b_dm = dm(1, vec![1, 2], 100); // smaller ULID — winner
        {
            let mut a = dev.a_state.lock().await;
            a.apply_space_with_canonicalization(a_dm);
        }
        {
            let mut b = dev.b_state.lock().await;
            b.apply_space_with_canonicalization(b_dm);
        }
        dev.a_engine.notify_dirty();
        dev.b_engine.notify_dirty();
        tokio::time::sleep(Duration::from_millis(500)).await;

        let a = dev.a_state.lock().await;
        let b = dev.b_state.lock().await;
        // Both must agree on the winner SpaceId(1) and have lost SpaceId(5).
        assert!(a.spaces.contains_key(&SpaceId([1; 16])));
        assert!(!a.spaces.contains_key(&SpaceId([5; 16])));
        assert!(b.spaces.contains_key(&SpaceId([1; 16])));
        assert!(!b.spaces.contains_key(&SpaceId([5; 16])));
        drop(a);
        drop(b);

        let _ = dev.a_engine.shutdown().await;
        let _ = dev.b_engine.shutdown().await;
    }

    use crate::owner_state_types::{ContentId, DeliveryStatus, OutboxEntry, OutboxEntryId};

    /// Phase 2 round-5 scenario, exercised end-to-end through real
    /// sync: A and B's DMs collapse via dedupe, then a lagging
    /// device C sends an outbox ack still referencing the OLD
    /// (loser) space_id. After canonicalization rewrites A's outbox
    /// to the winner space_id, C's lagging ack must still merge.
    #[tokio::test]
    async fn lagging_peer_ack_after_dedupe_still_merges() {
        let dev = spawn_two_devices(99);

        // A creates DM id=5 (will lose dedupe to B's id=1).
        let a_dm = dm(5, vec![1, 2], 100);
        {
            let mut a = dev.a_state.lock().await;
            a.apply_space_with_canonicalization(a_dm);
            // Plus an OutboxEntry on that DM.
            a.apply_outbox(OutboxEntry {
                id: OutboxEntryId([42; 16]),
                space_id: SpaceId([5; 16]),
                recipient_owners: vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])],
                message_cid: ContentId([7; 32]),
                created_at: Hlc {
                    wall_ms: 100,
                    logical: 0,
                    device_id: "device-a".into(),
                },
                delivered_to: [OwnerAddr([1; 16])].into_iter().collect(),
                delivery_status: DeliveryStatus::Partial,
            });
        }
        // B creates DM id=1 (winner).
        let b_dm = dm(1, vec![1, 2], 100);
        {
            let mut b = dev.b_state.lock().await;
            b.apply_space_with_canonicalization(b_dm);
        }

        dev.a_engine.notify_dirty();
        dev.b_engine.notify_dirty();
        tokio::time::sleep(Duration::from_millis(500)).await;

        // After sync: A's outbox should have been canonicalized to id=1.
        {
            let a = dev.a_state.lock().await;
            let entry = a.outbox.get(&OutboxEntryId([42; 16])).unwrap();
            assert_eq!(
                entry.space_id,
                SpaceId([1; 16]),
                "A's outbox must have canonicalized space_id"
            );
        }

        // Now A re-mutates its outbox with the SAME OutboxEntry but
        // still referencing the OLD space_id=5 (simulating a lagging
        // peer). Phase 2 round-5 made apply_outbox accept this.
        {
            let mut a = dev.a_state.lock().await;
            a.apply_outbox(OutboxEntry {
                id: OutboxEntryId([42; 16]),
                space_id: SpaceId([5; 16]), // lagging — old loser id
                recipient_owners: vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])],
                message_cid: ContentId([7; 32]),
                created_at: Hlc {
                    wall_ms: 100,
                    logical: 0,
                    device_id: "device-a".into(),
                },
                delivered_to: [OwnerAddr([2; 16])].into_iter().collect(),
                delivery_status: DeliveryStatus::Partial,
            });
        }
        dev.a_engine.notify_dirty();
        tokio::time::sleep(Duration::from_millis(300)).await;

        // After sync: A's entry still on canonicalized space_id=1,
        // and BOTH acks ({1, 2}) are present → Complete.
        let a = dev.a_state.lock().await;
        let entry = a.outbox.get(&OutboxEntryId([42; 16])).unwrap();
        assert_eq!(entry.space_id, SpaceId([1; 16]));
        assert_eq!(entry.delivered_to.len(), 2);
        assert_eq!(entry.delivery_status, DeliveryStatus::Complete);
        drop(a);

        let _ = dev.a_engine.shutdown().await;
        let _ = dev.b_engine.shutdown().await;
    }

    /// 50 randomized sequences of (mutate-on-A, mutate-on-B,
    /// publish-A, publish-B) operations. After draining, A and B
    /// must hold equal `OwnerState`s. Catches non-determinism in
    /// the merge path that scripted tests miss.
    #[tokio::test]
    async fn random_sequence_convergence_50x() {
        // Seedable PRNG — chosen so a regression reproduces.
        let mut rng_state: u64 = 0xdead_beef_cafe_babe;
        fn next(rng: &mut u64) -> u64 {
            // xorshift64
            let mut x = *rng;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *rng = x;
            x
        }

        for trial in 0..50 {
            let dev = spawn_two_devices((trial % 256) as u8);
            // Generate 8-12 random folder mutations split between A and B.
            let n_ops = 8 + (next(&mut rng_state) % 5) as u8;
            for op in 0..n_ops {
                let folder_id = 100 + op;
                let timestamp = 1000 + (next(&mut rng_state) % 10000);
                let to_a = next(&mut rng_state) & 1 == 0;
                let f = dm(
                    folder_id,
                    vec![1, 2 + (op % 3)], // distinct sorted-members per op
                    timestamp,
                );
                if to_a {
                    let mut a = dev.a_state.lock().await;
                    a.apply_space_with_canonicalization(f);
                } else {
                    let mut b = dev.b_state.lock().await;
                    b.apply_space_with_canonicalization(f);
                }
            }
            dev.a_engine.notify_dirty();
            dev.b_engine.notify_dirty();
            // Multiple debounce + sync cycles to let convergence settle.
            tokio::time::sleep(Duration::from_millis(800)).await;

            // Force final flushes both directions and let them propagate.
            dev.a_engine.flush_now().await.unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
            dev.b_engine.flush_now().await.unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
            dev.a_engine.flush_now().await.unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;

            let a = dev.a_state.lock().await;
            let b = dev.b_state.lock().await;
            assert_eq!(
                a.spaces, b.spaces,
                "trial {}: A and B spaces diverge\nA: {:?}\nB: {:?}",
                trial, a.spaces, b.spaces
            );
            drop(a);
            drop(b);

            let _ = dev.a_engine.shutdown().await;
            let _ = dev.b_engine.shutdown().await;
        }
    }
}
