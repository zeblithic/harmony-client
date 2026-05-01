//! Content-addressed storage trait + in-memory stub (ZEB-215 Sub-A Phase 3a).
//!
//! See `docs/specs/2026-05-01-zeb-215-sub-a-phase3a-sync-design.md`
//! §"ContentStore trait". Phase 3b swaps `InMemoryStub` for the real
//! harmony-content client.

use crate::owner_state_types::ContentId;
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(thiserror::Error, Debug)]
pub enum ContentStoreError {
    #[error("content store I/O: {0}")]
    Io(String),
}

pub trait ContentStore: Send + Sync {
    fn put(&self, cid: ContentId, blob: Vec<u8>) -> Result<(), ContentStoreError>;
    fn get(&self, cid: &ContentId) -> Result<Option<Vec<u8>>, ContentStoreError>;
}

#[derive(Default)]
pub struct InMemoryStub {
    inner: Mutex<HashMap<ContentId, Vec<u8>>>,
}

impl ContentStore for InMemoryStub {
    fn put(&self, cid: ContentId, blob: Vec<u8>) -> Result<(), ContentStoreError> {
        self.inner
            .lock()
            .map_err(|e| ContentStoreError::Io(format!("lock poisoned: {e}")))?
            .insert(cid, blob);
        Ok(())
    }

    fn get(&self, cid: &ContentId) -> Result<Option<Vec<u8>>, ContentStoreError> {
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

    #[test]
    fn put_then_get_returns_blob() {
        let store = InMemoryStub::default();
        store.put(cid(1), vec![10, 20, 30]).unwrap();
        let blob = store.get(&cid(1)).unwrap().expect("blob present");
        assert_eq!(blob, vec![10, 20, 30]);
    }

    #[test]
    fn get_missing_returns_none() {
        let store = InMemoryStub::default();
        assert!(store.get(&cid(99)).unwrap().is_none());
    }

    #[test]
    fn concurrent_puts_all_land() {
        use std::sync::Arc;
        use std::thread;

        let store = Arc::new(InMemoryStub::default());
        let mut handles = vec![];
        for i in 0..50u8 {
            let s = Arc::clone(&store);
            handles.push(thread::spawn(move || {
                s.put(cid(i), vec![i]).unwrap();
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        for i in 0..50u8 {
            let blob = store.get(&cid(i)).unwrap().expect("blob present");
            assert_eq!(blob, vec![i]);
        }
    }
}
