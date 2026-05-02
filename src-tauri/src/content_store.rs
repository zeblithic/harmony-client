//! Content-addressed storage trait + in-memory stub (ZEB-215 Sub-A Phase 3a)
//! and async-trait migration (Phase 3b).
//!
//! See `docs/specs/2026-05-01-zeb-215-sub-a-phase3a-sync-design.md`
//! §"ContentStore trait" and `docs/specs/2026-05-01-zeb-215-sub-a-phase3b-content-cas-design.md`.
//!
//! Phase 3a shipped a sync trait + `InMemoryStub`. Phase 3b makes the trait
//! async so the real `RuntimeContentStore` adapter can await network fetches
//! through the harmony-runtime event loop. `InMemoryStub` keeps in-process
//! semantics for unit tests; the new `RuntimeContentStore` (Task 4) wires
//! through the new `cas_op` mpsc channel into `event_loop::run`.

use crate::owner_state_types::ContentId;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(thiserror::Error, Debug)]
pub enum ContentStoreError {
    #[error("content store I/O: {0}")]
    Io(String),
}

#[async_trait]
pub trait ContentStore: Send + Sync {
    async fn put(&self, cid: ContentId, blob: Vec<u8>) -> Result<(), ContentStoreError>;
    async fn get(&self, cid: &ContentId) -> Result<Option<Vec<u8>>, ContentStoreError>;
}

#[derive(Default)]
pub struct InMemoryStub {
    inner: Mutex<HashMap<ContentId, Vec<u8>>>,
}

#[async_trait]
impl ContentStore for InMemoryStub {
    async fn put(&self, cid: ContentId, blob: Vec<u8>) -> Result<(), ContentStoreError> {
        self.inner
            .lock()
            .map_err(|e| ContentStoreError::Io(format!("lock poisoned: {e}")))?
            .insert(cid, blob);
        Ok(())
    }

    async fn get(&self, cid: &ContentId) -> Result<Option<Vec<u8>>, ContentStoreError> {
        Ok(self
            .inner
            .lock()
            .map_err(|e| ContentStoreError::Io(format!("lock poisoned: {e}")))?
            .get(cid)
            .cloned())
    }
}

/// Channel-protocol message between `RuntimeContentStore` (in
/// `SyncEngine`'s tokio task) and the harmony-runtime event loop.
///
/// The event loop owns the only `&mut NodeRuntime`, so the adapter
/// can't admit/fetch directly — it sends one of these messages and
/// awaits a oneshot reply. See spec §"Event loop handler" and
/// §"Re-entry" for the full protocol including the second-mpsc-hop
/// admit pattern used by `GetOrFetch` after a successful network GET.
pub enum CasOp {
    /// Admit `blob` to the local StorageTier cache under `cid`.
    /// Reply `Ok(())` once `runtime.tick()` has drained the
    /// resulting actions; reply `Err(...)` if the channel layer
    /// itself failed (StorageTier silently drops corrupted bytes —
    /// matches the existing ingest_rx pattern — so callers treat
    /// `Ok(())` as "we tried" rather than as proof of admit).
    PutLocal {
        cid: ContentId,
        blob: Vec<u8>,
        reply: tokio::sync::oneshot::Sender<Result<(), ContentStoreError>>,
    },
    /// Cache check, then on miss spawn a Zenoh GET wrapped in
    /// `tokio::time::timeout(timeout, ...)`. On fetch success,
    /// admit via a second `CasOp::PutLocal` hop before replying
    /// `Ok(Some(bytes))`. On timeout: `Ok(None)`. On hard transport
    /// error (zenoh::open failure, malformed key_expr): `Err(...)`.
    GetOrFetch {
        cid: ContentId,
        timeout: std::time::Duration,
        reply: tokio::sync::oneshot::Sender<Result<Option<Vec<u8>>, ContentStoreError>>,
    },
}

/// Default fetch budget for `RuntimeContentStore::get`. Wraps the
/// Zenoh GET in `tokio::time::timeout`; on miss the subscriber drops
/// the publish and CRDT eventual consistency carries recovery via
/// the next state-root from any peer.
pub const DEFAULT_FETCH_TIMEOUT_MS: u64 = 500;

/// Production `ContentStore` impl that delegates to the harmony-runtime
/// event loop via `cas_op_tx`. Used at SyncEngine construction in
/// `lib.rs::start_node` (Task 8); tests still use `InMemoryStub` for
/// in-process flows.
pub struct RuntimeContentStore {
    cas_op_tx: tokio::sync::mpsc::Sender<CasOp>,
    fetch_timeout: std::time::Duration,
}

impl RuntimeContentStore {
    pub fn new(
        cas_op_tx: tokio::sync::mpsc::Sender<CasOp>,
        fetch_timeout: std::time::Duration,
    ) -> Self {
        Self {
            cas_op_tx,
            fetch_timeout,
        }
    }
}

#[async_trait]
impl ContentStore for RuntimeContentStore {
    async fn put(&self, cid: ContentId, blob: Vec<u8>) -> Result<(), ContentStoreError> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.cas_op_tx
            .send(CasOp::PutLocal {
                cid,
                blob,
                reply: reply_tx,
            })
            .await
            .map_err(|_| ContentStoreError::Io("event loop unavailable (send)".into()))?;
        reply_rx
            .await
            .map_err(|_| ContentStoreError::Io("event loop unavailable (reply)".into()))?
    }

    async fn get(&self, cid: &ContentId) -> Result<Option<Vec<u8>>, ContentStoreError> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.cas_op_tx
            .send(CasOp::GetOrFetch {
                cid: *cid,
                timeout: self.fetch_timeout,
                reply: reply_tx,
            })
            .await
            .map_err(|_| ContentStoreError::Io("event loop unavailable (send)".into()))?;
        reply_rx
            .await
            .map_err(|_| ContentStoreError::Io("event loop unavailable (reply)".into()))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cid(byte: u8) -> ContentId {
        ContentId([byte; 32])
    }

    #[tokio::test]
    async fn put_then_get_returns_blob() {
        let store = InMemoryStub::default();
        store.put(cid(1), vec![10, 20, 30]).await.unwrap();
        let blob = store.get(&cid(1)).await.unwrap().expect("blob present");
        assert_eq!(blob, vec![10, 20, 30]);
    }

    #[tokio::test]
    async fn get_missing_returns_none() {
        let store = InMemoryStub::default();
        assert!(store.get(&cid(99)).await.unwrap().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_puts_all_land() {
        use std::sync::Arc;

        let store = Arc::new(InMemoryStub::default());
        let mut handles = vec![];
        for i in 0..50u8 {
            let s = Arc::clone(&store);
            handles.push(tokio::spawn(async move {
                s.put(cid(i), vec![i]).await.unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        for i in 0..50u8 {
            let blob = store.get(&cid(i)).await.unwrap().expect("blob present");
            assert_eq!(blob, vec![i]);
        }
    }

    #[tokio::test]
    async fn runtime_content_store_put_round_trip() {
        // RuntimeContentStore sends CasOp::PutLocal; stub receiver replies Ok(()).
        let (cas_op_tx, mut cas_op_rx) = tokio::sync::mpsc::channel::<CasOp>(8);
        let store = RuntimeContentStore::new(cas_op_tx, std::time::Duration::from_millis(500));

        // Stub receiver: handle exactly one PutLocal then exit.
        let stub = tokio::spawn(async move {
            if let Some(CasOp::PutLocal { cid, blob, reply }) = cas_op_rx.recv().await {
                assert_eq!(cid, ContentId([0x42; 32]));
                assert_eq!(blob, vec![1, 2, 3]);
                let _ = reply.send(Ok(()));
            } else {
                panic!("expected CasOp::PutLocal");
            }
        });

        store
            .put(ContentId([0x42; 32]), vec![1, 2, 3])
            .await
            .unwrap();
        stub.await.unwrap();
    }

    #[tokio::test]
    async fn runtime_content_store_get_round_trip() {
        let (cas_op_tx, mut cas_op_rx) = tokio::sync::mpsc::channel::<CasOp>(8);
        let store = RuntimeContentStore::new(cas_op_tx, std::time::Duration::from_millis(500));

        let stub = tokio::spawn(async move {
            if let Some(CasOp::GetOrFetch {
                cid,
                timeout,
                reply,
            }) = cas_op_rx.recv().await
            {
                assert_eq!(cid, ContentId([0x99; 32]));
                assert_eq!(timeout, std::time::Duration::from_millis(500));
                let _ = reply.send(Ok(Some(vec![7, 8, 9])));
            } else {
                panic!("expected CasOp::GetOrFetch");
            }
        });

        let blob = store.get(&ContentId([0x99; 32])).await.unwrap();
        assert_eq!(blob, Some(vec![7, 8, 9]));
        stub.await.unwrap();
    }

    #[tokio::test]
    async fn runtime_content_store_put_propagates_error() {
        let (cas_op_tx, mut cas_op_rx) = tokio::sync::mpsc::channel::<CasOp>(8);
        let store = RuntimeContentStore::new(cas_op_tx, std::time::Duration::from_millis(500));

        let stub = tokio::spawn(async move {
            if let Some(CasOp::PutLocal { reply, .. }) = cas_op_rx.recv().await {
                let _ = reply.send(Err(ContentStoreError::Io("admit rejected".into())));
            }
        });

        let err = store.put(ContentId([1; 32]), vec![1]).await.unwrap_err();
        match err {
            ContentStoreError::Io(msg) => assert!(msg.contains("admit rejected")),
        }
        stub.await.unwrap();
    }

    #[tokio::test]
    async fn runtime_content_store_channel_closed_returns_io_error() {
        let (cas_op_tx, cas_op_rx) = tokio::sync::mpsc::channel::<CasOp>(8);
        // Drop the receiver immediately. Subsequent sends fail.
        drop(cas_op_rx);

        let store = RuntimeContentStore::new(cas_op_tx, std::time::Duration::from_millis(500));
        let err = store.put(ContentId([0; 32]), vec![]).await.unwrap_err();
        match err {
            ContentStoreError::Io(msg) => {
                assert!(msg.contains("event loop unavailable"), "got msg: {msg}");
            }
        }
    }

    #[tokio::test]
    async fn runtime_content_store_get_returns_none_for_timeout_signal() {
        // The actual tokio::time::timeout enforcement lives in the event-loop
        // arm; this test verifies that whatever the event loop replies (here:
        // Ok(None) simulating a timeout) is propagated unchanged.
        let (cas_op_tx, mut cas_op_rx) = tokio::sync::mpsc::channel::<CasOp>(8);
        let store = RuntimeContentStore::new(cas_op_tx, std::time::Duration::from_millis(500));

        let stub = tokio::spawn(async move {
            if let Some(CasOp::GetOrFetch { reply, .. }) = cas_op_rx.recv().await {
                let _ = reply.send(Ok(None));
            }
        });

        let blob = store.get(&ContentId([0xAA; 32])).await.unwrap();
        assert_eq!(blob, None);
        stub.await.unwrap();
    }
}
