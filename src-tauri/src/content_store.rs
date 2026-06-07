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
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};

/// Set of community state-root CIDs this node is willing to serve over CAS even
/// though they carry the `encrypted` flag (ZEB-395). Community roots are
/// epoch-key ciphertext shared among members; serving them by CID is safe (see
/// `docs/specs/2026-06-07-zeb-395-community-content-serve-policy-design.md` §3).
/// Private encrypted blobs (DMs, private profiles) are never inserted, so the
/// content-serve queryable keeps refusing them.
///
/// `std::sync::RwLock` (not tokio) is intentional: `allow`/`contains` lock,
/// mutate/read, and drop the guard synchronously — no guard is ever held across
/// an `.await`. The handle is `Clone` (Arc bump) and shared between the
/// production `RuntimeContentStore` (registration) and the content-serve
/// queryable (lookup).
#[derive(Clone, Default)]
pub struct CommunityServeAllowlist(Arc<RwLock<HashSet<ContentId>>>);

impl CommunityServeAllowlist {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark a community-root CID serveable. Idempotent. The write guard is held
    /// only across a non-panicking `HashSet::insert`, so poisoning cannot occur
    /// in practice; a poisoned lock is nonetheless handled by skipping the insert
    /// (best-effort, never a panic) rather than by promising later recovery.
    pub fn allow(&self, cid: ContentId) {
        if let Ok(mut g) = self.0.write() {
            g.insert(cid);
        }
    }

    /// True if `cid` is an allowlisted community-root CID. A poisoned lock reads
    /// as "not allowlisted" (fail closed — never serve on a poisoned guard).
    pub fn contains(&self, cid: &ContentId) -> bool {
        self.0.read().map(|g| g.contains(cid)).unwrap_or(false)
    }
}

#[derive(thiserror::Error, Debug)]
pub enum ContentStoreError {
    #[error("content store I/O: {0}")]
    Io(String),
}

#[async_trait]
pub trait ContentStore: Send + Sync {
    async fn put(&self, cid: ContentId, blob: Vec<u8>) -> Result<(), ContentStoreError>;
    async fn get(&self, cid: &ContentId) -> Result<Option<Vec<u8>>, ContentStoreError>;

    /// Like `put`, but also marks `cid` serveable to peers over CAS even though
    /// it carries the `encrypted` flag (ZEB-395 community-root sharing). The
    /// default impl is identical to `put`; only `RuntimeContentStore` registers
    /// the CID in its shared `CommunityServeAllowlist`. Callers use this ONLY
    /// for content that is safe to serve to any requester who can name the CID
    /// (community state-root ciphertext) — never for private blobs.
    async fn put_serveable(&self, cid: ContentId, blob: Vec<u8>) -> Result<(), ContentStoreError> {
        self.put(cid, blob).await
    }
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
#[derive(Debug)]
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
        /// `Some` for synchronous round-trip callers (e.g.
        /// `RuntimeContentStore::put`); `None` for fire-and-forget
        /// admit hops from the spawned-fetch task in `event_loop.rs`'s
        /// `GetOrFetch` arm. The PutLocal handler only replies if
        /// `Some`, avoiding wasted work on already-dropped receivers.
        reply: Option<tokio::sync::oneshot::Sender<Result<(), ContentStoreError>>>,
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
    /// Read-only local-cache lookup: return the bytes held under `cid` in the
    /// StorageTier cache, or `None` on a cache miss. Unlike `GetOrFetch`, this
    /// NEVER triggers a network fetch — it is the lookup path for the
    /// content-serve queryable, which must not recursively fetch while
    /// answering a peer's GET (that would invert the serve relationship and
    /// could deadlock the event loop). The cache only holds bytes that passed
    /// `hash==cid` verification at admit time (StorageTier::verify_cid), so a
    /// `Some(bytes)` reply is already integrity-checked.
    GetLocal {
        cid: ContentId,
        reply: tokio::sync::oneshot::Sender<Option<Vec<u8>>>,
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
    /// ZEB-395: when set, `put_serveable` records the put CID here so the
    /// content-serve queryable will serve it despite the `encrypted` flag.
    /// `None` for the legacy/test constructions that don't serve community
    /// roots. Shared (Arc clone) with `event_loop::run`'s serve queryable.
    serve_allowlist: Option<CommunityServeAllowlist>,
}

impl RuntimeContentStore {
    pub fn new(
        cas_op_tx: tokio::sync::mpsc::Sender<CasOp>,
        fetch_timeout: std::time::Duration,
    ) -> Self {
        Self {
            cas_op_tx,
            fetch_timeout,
            serve_allowlist: None,
        }
    }

    /// ZEB-395: attach the shared serve-allowlist so `put_serveable` registers
    /// community-root CIDs. Chained builder so the existing
    /// `RuntimeContentStore::new(...)` call sites stay untouched.
    pub fn with_serve_allowlist(mut self, allowlist: CommunityServeAllowlist) -> Self {
        self.serve_allowlist = Some(allowlist);
        self
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
                reply: Some(reply_tx),
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

    async fn put_serveable(&self, cid: ContentId, blob: Vec<u8>) -> Result<(), ContentStoreError> {
        // Admit first; only record as serveable after a successful put.
        let blob_len = blob.len();
        self.put(cid, blob).await?;
        if let Some(allowlist) = &self.serve_allowlist {
            allowlist.allow(cid);
            tracing::info!(?cid, blob_len, "ZEB366diag: put_serveable registered CID in serve-allowlist + CAS");
        } else {
            tracing::warn!(?cid, "ZEB366diag: put_serveable called but serve_allowlist is None (CID put to CAS but NOT allowlisted!)");
        }
        Ok(())
    }
}

#[cfg(any(test, feature = "test-fixtures"))]
impl InMemoryStub {
    pub async fn debug_count(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    pub async fn debug_all_cids(&self) -> Vec<crate::owner_state_types::ContentId> {
        self.inner.lock().unwrap().keys().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cid(byte: u8) -> ContentId {
        ContentId::from_bytes([byte; 32])
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
            if let Some(CasOp::PutLocal {
                cid,
                blob,
                reply: Some(reply),
            }) = cas_op_rx.recv().await
            {
                assert_eq!(cid, ContentId::from_bytes([0x42; 32]));
                assert_eq!(blob, vec![1, 2, 3]);
                let _ = reply.send(Ok(()));
            } else {
                panic!("expected CasOp::PutLocal with reply");
            }
        });

        store
            .put(ContentId::from_bytes([0x42; 32]), vec![1, 2, 3])
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
                assert_eq!(cid, ContentId::from_bytes([0x99; 32]));
                assert_eq!(timeout, std::time::Duration::from_millis(500));
                let _ = reply.send(Ok(Some(vec![7, 8, 9])));
            } else {
                panic!("expected CasOp::GetOrFetch");
            }
        });

        let blob = store.get(&ContentId::from_bytes([0x99; 32])).await.unwrap();
        assert_eq!(blob, Some(vec![7, 8, 9]));
        stub.await.unwrap();
    }

    #[tokio::test]
    async fn runtime_content_store_put_propagates_error() {
        let (cas_op_tx, mut cas_op_rx) = tokio::sync::mpsc::channel::<CasOp>(8);
        let store = RuntimeContentStore::new(cas_op_tx, std::time::Duration::from_millis(500));

        let stub = tokio::spawn(async move {
            if let Some(CasOp::PutLocal {
                reply: Some(reply), ..
            }) = cas_op_rx.recv().await
            {
                let _ = reply.send(Err(ContentStoreError::Io("admit rejected".into())));
            }
        });

        let err = store
            .put(ContentId::from_bytes([1; 32]), vec![1])
            .await
            .unwrap_err();
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
        let err = store
            .put(ContentId::from_bytes([0; 32]), vec![])
            .await
            .unwrap_err();
        match err {
            ContentStoreError::Io(msg) => {
                assert!(msg.contains("(send)"), "got msg: {msg}");
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

        let blob = store.get(&ContentId::from_bytes([0xAA; 32])).await.unwrap();
        assert_eq!(blob, None);
        stub.await.unwrap();
    }

    #[tokio::test]
    async fn runtime_content_store_reply_dropped_returns_io_error() {
        // Stub receives the message but drops the reply sender without
        // replying. RuntimeContentStore.put should then surface the
        // distinct (reply) error message — the spec calls out the
        // distinction between (send) and (reply) lifecycle failures
        // because they implicate different root causes (receiver gone
        // before delivery vs receiver panicked after delivery).
        let (cas_op_tx, mut cas_op_rx) = tokio::sync::mpsc::channel::<CasOp>(8);
        let store = RuntimeContentStore::new(cas_op_tx, std::time::Duration::from_millis(500));

        let stub = tokio::spawn(async move {
            // Receive the message but drop the reply sender without replying.
            if let Some(CasOp::PutLocal {
                reply: Some(reply), ..
            }) = cas_op_rx.recv().await
            {
                drop(reply);
            }
        });

        let err = store
            .put(ContentId::from_bytes([0; 32]), vec![])
            .await
            .unwrap_err();
        match err {
            ContentStoreError::Io(msg) => {
                assert!(msg.contains("(reply)"), "got msg: {msg}");
            }
        }
        stub.await.unwrap();
    }

    #[test]
    fn allowlist_allow_then_contains() {
        let a = CommunityServeAllowlist::new();
        let c = cid(7);
        assert!(!a.contains(&c), "fresh allowlist contains nothing");
        a.allow(c);
        assert!(a.contains(&c), "allowed CID is contained");
        assert!(!a.contains(&cid(8)), "un-added CID is not contained");
    }

    #[test]
    fn allowlist_clone_shares_state() {
        // Arc-backed: a clone observes inserts made via the original.
        let a = CommunityServeAllowlist::new();
        let b = a.clone();
        let c = cid(42);
        a.allow(c);
        assert!(b.contains(&c), "clone shares the underlying set");
    }

    #[tokio::test]
    async fn put_serveable_registers_cid_in_allowlist() {
        // RuntimeContentStore.with_serve_allowlist: put_serveable admits AND records
        // the CID; plain put admits but does NOT record.
        let (cas_op_tx, mut cas_op_rx) = tokio::sync::mpsc::channel::<CasOp>(8);
        let allowlist = CommunityServeAllowlist::new();
        let store = RuntimeContentStore::new(cas_op_tx, std::time::Duration::from_millis(500))
            .with_serve_allowlist(allowlist.clone());

        // Stub receiver: ack every PutLocal reply so put()/put_serveable() return Ok.
        let stub = tokio::spawn(async move {
            while let Some(op) = cas_op_rx.recv().await {
                if let CasOp::PutLocal {
                    reply: Some(reply), ..
                } = op
                {
                    let _ = reply.send(Ok(()));
                }
            }
        });

        let served = ContentId::from_bytes([0x11; 32]);
        let private = ContentId::from_bytes([0x22; 32]);
        store.put_serveable(served, vec![1, 2, 3]).await.unwrap();
        store.put(private, vec![4, 5, 6]).await.unwrap();

        assert!(
            allowlist.contains(&served),
            "put_serveable registers the CID"
        );
        assert!(
            !allowlist.contains(&private),
            "plain put does NOT register the CID"
        );
        drop(store);
        stub.await.unwrap();
    }

    #[tokio::test]
    async fn put_serveable_default_impl_routes_to_put() {
        // The default trait impl (InMemoryStub) routes put_serveable to put with no
        // allowlist concept and no panic.
        let store = InMemoryStub::default();
        store
            .put_serveable(ContentId::from_bytes([9; 32]), vec![7, 8])
            .await
            .unwrap();
        let got = store
            .get(&ContentId::from_bytes([9; 32]))
            .await
            .unwrap()
            .expect("blob present");
        assert_eq!(got, vec![7, 8]);
    }

    #[tokio::test]
    async fn put_serveable_without_allowlist_is_just_put() {
        // RuntimeContentStore with no allowlist set: put_serveable behaves like put.
        let (cas_op_tx, mut cas_op_rx) = tokio::sync::mpsc::channel::<CasOp>(8);
        let store = RuntimeContentStore::new(cas_op_tx, std::time::Duration::from_millis(500));
        let stub = tokio::spawn(async move {
            if let Some(CasOp::PutLocal {
                reply: Some(reply), ..
            }) = cas_op_rx.recv().await
            {
                let _ = reply.send(Ok(()));
            }
        });
        store
            .put_serveable(ContentId::from_bytes([3; 32]), vec![1])
            .await
            .unwrap();
        stub.await.unwrap();
    }

    #[tokio::test]
    async fn put_serveable_failed_put_does_not_register() {
        // Failure contract: if the underlying put fails, `?` returns early and
        // the CID is NEVER added to the allowlist (serving an un-admitted CID
        // would be a dangling entry that can never be satisfied locally).
        let (cas_op_tx, mut cas_op_rx) = tokio::sync::mpsc::channel::<CasOp>(8);
        let allowlist = CommunityServeAllowlist::new();
        let store = RuntimeContentStore::new(cas_op_tx, std::time::Duration::from_millis(500))
            .with_serve_allowlist(allowlist.clone());

        // Stub: reply Err to the PutLocal so put_serveable's `?` propagates.
        let stub = tokio::spawn(async move {
            if let Some(CasOp::PutLocal {
                reply: Some(reply), ..
            }) = cas_op_rx.recv().await
            {
                let _ = reply.send(Err(ContentStoreError::Io("admit rejected".into())));
            }
        });

        let cid = ContentId::from_bytes([0x55; 32]);
        let err = store.put_serveable(cid, vec![1, 2]).await.unwrap_err();
        match err {
            ContentStoreError::Io(msg) => assert!(msg.contains("admit rejected")),
        }
        assert!(
            !allowlist.contains(&cid),
            "a failed put must NOT register the CID as serveable"
        );
        stub.await.unwrap();
    }

    /// Guard the `CasOp::GetLocal` enum shape and oneshot reply plumbing.
    ///
    /// This test does NOT construct a `NodeRuntime` (no cheap harness exists;
    /// building one for a 3-line read-only handler is not warranted). Instead
    /// it constructs the variant, pattern-matches to extract `cid`/`reply`,
    /// manually sends a reply, and asserts the receiver gets it. The
    /// handler's runtime-cache read is covered end-to-end by the Task 14
    /// cross-peer e2e (owner A serving its cached avatar).
    #[tokio::test]
    async fn cas_getlocal_variant_shape_and_reply_plumbing() {
        use harmony_content::cid::{ContentFlags, ContentId as HcContentId};

        let cid = HcContentId::for_book(b"hi", ContentFlags::default()).unwrap();
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<Option<Vec<u8>>>();

        let op = CasOp::GetLocal {
            cid,
            reply: reply_tx,
        };

        // Pattern-match to verify the variant exists and fields are accessible.
        match op {
            CasOp::GetLocal {
                cid: extracted_cid,
                reply,
            } => {
                assert_eq!(extracted_cid, cid);
                // Simulate the handler: send Some(bytes) through the oneshot.
                let _ = reply.send(Some(vec![104, 105]));
            }
            _ => panic!("expected CasOp::GetLocal"),
        }

        let result = reply_rx.await.expect("oneshot should not be dropped");
        assert_eq!(result, Some(vec![104, 105]));
    }
}
