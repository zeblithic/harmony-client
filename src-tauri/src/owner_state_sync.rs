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
    shutdown_tx: mpsc::Sender<tokio::sync::oneshot::Sender<()>>,
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
    pub async fn shutdown(&self) {
        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        if self.shutdown_tx.send(resp_tx).await.is_ok() {
            let _ = resp_rx.await;
        }
        if let Some(handle) = self.task.lock().await.take() {
            let _ = handle.await;
        }
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
    shutdown_rx: mpsc::Receiver<tokio::sync::oneshot::Sender<()>>,
}

async fn internal_task(mut ctx: InternalCtx) {
    use std::time::Instant;

    let mut next_wakeup: Option<Instant> = None;

    loop {
        // Compute the sleep duration for the wakeup branch.
        let sleep_dur = next_wakeup
            .map(|t| t.saturating_duration_since(Instant::now()))
            .unwrap_or(std::time::Duration::from_secs(3600));

        tokio::select! {
            _ = ctx.notify_dirty.notified() => {
                // Extend (or arm) the debounce window on every dirty
                // signal. This is a sliding debounce: multiple rapid
                // calls reset the timer, collapsing to one publish
                // 100ms after the last call in the burst.
                if ctx.has_pending_dirty.load(Ordering::Relaxed) {
                    next_wakeup = Some(Instant::now() + ctx.debounce);
                }
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
                next_wakeup = None;
                ctx.has_pending_dirty.store(false, Ordering::Relaxed);
                let pub_result = publish_root_now(&ctx).await;
                let persist_result = persist_both(&ctx.state, &ctx.tracker, &ctx.paths).await;
                let result = pub_result.and(persist_result);
                let _ = resp_tx.send(result);
            }
            Some(bytes) = ctx.subscriber_rx.recv() => {
                if let Err(e) = handle_incoming_publish(&mut ctx, bytes).await {
                    tracing::warn!(error = %e, "incoming publish dropped");
                }
                if let Err(e) = persist_both(&ctx.state, &ctx.tracker, &ctx.paths).await {
                    tracing::warn!(error = %e, "persist_both failed");
                }
            }
            Some(resp_tx) = ctx.shutdown_rx.recv() => {
                // Flush only if there is genuinely unpublished dirty state.
                if ctx.has_pending_dirty.load(Ordering::Relaxed) {
                    let _ = publish_root_now(&ctx).await;
                }
                let _ = persist_both(&ctx.state, &ctx.tracker, &ctx.paths).await;
                let _ = resp_tx.send(());
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
    let state_snap = state.lock().await.clone();
    let tracker_snap = tracker.lock().await.clone();
    crate::owner_state_persist::save_crdt(&paths.crdt, &state_snap)?;
    crate::owner_state_persist::save_replay(&paths.replay, &tracker_snap)?;
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
    let logical = match tracker.get(&ctx.device_id) {
        Some(prev) if prev.wall_ms == wall_ms => prev.logical + 1,
        Some(prev) if prev.wall_ms > wall_ms => prev.logical + 1, // wall non-monotonic
        _ => 0,
    };
    let prev_wall = tracker.get(&ctx.device_id).map(|p| p.wall_ms).unwrap_or(0);
    let effective_wall = std::cmp::max(wall_ms, prev_wall);

    let now = Hlc {
        wall_ms: effective_wall,
        logical,
        device_id: ctx.device_id.clone(),
    };
    tracker.insert(ctx.device_id.clone(), now.clone());
    now
}

#[allow(clippy::needless_pass_by_ref_mut)]
async fn handle_incoming_publish(ctx: &mut InternalCtx, wire: Vec<u8>) -> Result<(), SyncError> {
    // 1. Decrypt the Zenoh wire payload.
    let payload_bytes =
        decrypt_root_publish(&ctx.kt, &wire).map_err(|e| SyncError::Crypto(e.to_string()))?;
    let payload: RootPublishPayload =
        canonical_cbor_decode(&payload_bytes).map_err(|e| SyncError::CborDecode(e.to_string()))?;

    // 2. Replay protection.
    {
        let mut tracker = ctx.tracker.lock().await;
        let accept = match tracker.get(&payload.at.device_id) {
            None => true,
            Some(existing) => payload.at.is_strictly_newer_than(existing),
        };
        if !accept {
            return Ok(());
        }
        tracker.insert(payload.at.device_id.clone(), payload.at.clone());
    }

    // 3. Fetch the encrypted root blob from CAS.
    let blob_ciphertext = ctx.content_store.get(&payload.root_cid)?.ok_or_else(|| {
        // Phase 3b will replace InMemoryStub with real CAS; for
        // 3a, a missing blob means the subscriber and publisher
        // aren't sharing the same stub (e.g. cross-process). Log
        // and skip — never panic.
        SyncError::Crypto("ContentStore returned None for root_cid".into())
    })?;

    // 4. Decrypt with the same lookup key the publisher used.
    let lookup = space_lookup_key(&ctx.kt, OWNER_STATE_ROOT_BLOB_TAG);
    let blob_cleartext = decrypt_entry(&ctx.kt, &lookup, &blob_ciphertext)
        .map_err(|e| SyncError::Crypto(e.to_string()))?;

    // 5. Decode into a remote OwnerState snapshot.
    let remote: OwnerState =
        canonical_cbor_decode(&blob_cleartext).map_err(|e| SyncError::CborDecode(e.to_string()))?;

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

    Ok(())
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
        engine.shutdown().await;
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
        engine.shutdown().await;
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
        engine.shutdown().await;
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
        engine.shutdown().await;
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
        engine.shutdown().await;
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

        engine.shutdown().await;
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
        engine.shutdown().await;
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

        engine.shutdown().await;
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

        engine.shutdown().await;
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

        engine.shutdown().await;
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

        engine.shutdown().await;
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

        engine.shutdown().await;
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
            engine.shutdown().await;
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

        engine2.shutdown().await;
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

        engine.shutdown().await;
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

        dev.a_engine.shutdown().await;
        dev.b_engine.shutdown().await;
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

        dev.a_engine.shutdown().await;
        dev.b_engine.shutdown().await;
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

        dev.a_engine.shutdown().await;
        dev.b_engine.shutdown().await;
    }
}
