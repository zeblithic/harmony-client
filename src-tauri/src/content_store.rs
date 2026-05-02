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

    #[tokio::test]
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
}
