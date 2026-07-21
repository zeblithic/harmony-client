#![allow(dead_code)]

//! Shared file-sharing integration-test helpers (ZEB-726 Task 3).
//!
//! `spawn_recording_store`, `write_temp`, and `reassemble_from_store` (plus
//! the `Store` type alias they share) used to be copied verbatim into
//! `file_sharing_dek.rs`, `file_sharing_streaming.rs`, and
//! `file_sharing_grantee.rs`. This module de-duplicates them. Each binary
//! includes it directly via `#[path = "common/file_sharing_helpers.rs"]`
//! (not through `common/mod.rs`, so unrelated fixtures in `common/` don't
//! get dragged into these binaries) and imports only the helpers it
//! actually calls. `#![allow(dead_code)]` covers the rest, since not every
//! binary uses every helper.
//!
//! Canonicalized from `file_sharing_dek.rs`'s copy. The three copies were
//! NOT byte-identical: `file_sharing_streaming.rs` used a
//! `Vec<(String, Vec<u8>)>`-backed `Store` (preserving insertion order) and
//! a 256-slot channel, while `file_sharing_dek.rs` and
//! `file_sharing_grantee.rs` used a `HashMap`-backed `Store` (128- and
//! 256-slot channels respectively). Neither difference is behavior-bearing:
//! `reassemble_from_store` replays every recorded `(cid_hex, data)` pair
//! into a `MemoryBookStore` keyed by `ContentId` — itself a map — so
//! insertion order never survives to the result. The channel capacity is
//! likewise inert: `send_ingest` (`src/lib.rs`) awaits each request's
//! oneshot reply before sending the next one, so at most one
//! `IngestRequest` is ever in flight regardless of buffer size.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use harmony_app::event_loop::IngestRequest;
use harmony_content::book::{BookStore, MemoryBookStore};
use harmony_content::cid::ContentId;

/// Records each ingested `(cid_hex, bytes)` — a stand-in content store.
pub type Store = Arc<Mutex<HashMap<String, Vec<u8>>>>;

pub fn spawn_recording_store() -> (tokio::sync::mpsc::Sender<IngestRequest>, Store) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<IngestRequest>(128);
    let store: Store = Arc::new(Mutex::new(HashMap::new()));
    let store_c = store.clone();
    tokio::spawn(async move {
        while let Some(req) = rx.recv().await {
            // Insert BEFORE acking the reply so `send_ingest`'s reply-await
            // guarantees the bytes are visible once the ingest call returns.
            store_c
                .lock()
                .unwrap()
                .insert(req.cid_hex.clone(), req.data);
            let _ = req.reply.send(Ok(()));
        }
    });
    (tx, store)
}

/// Write `bytes` to a fresh tempfile and return the guard + path —
/// `ingest_content_encrypted_inner` takes an opened `tokio::fs::File`
/// reader rather than an in-memory `Vec<u8>`.
pub async fn write_temp(bytes: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("input.bin");
    tokio::fs::write(&path, bytes).await.unwrap();
    (dir, path)
}

/// Reassemble the DAG the recording store captured, via the real
/// `harmony_content::dag::reassemble` (`&dyn BookStore`, not a bare
/// closure). Replays every recorded `(cid_hex, data)` pair into a
/// `MemoryBookStore` keyed by its real `ContentId` and hands that to
/// `reassemble`.
pub fn reassemble_from_store(store: &Store, root: &[u8; 32]) -> Vec<u8> {
    let mut book_store = MemoryBookStore::new();
    for (cid_hex, data) in store.lock().unwrap().iter() {
        let bytes: [u8; 32] =
            <[u8; 32]>::try_from(hex::decode(cid_hex).expect("cid hex")).expect("cid 32 bytes");
        book_store.store(ContentId::from_bytes(bytes), data.clone());
    }
    let root_cid = ContentId::from_bytes(*root);
    harmony_content::dag::reassemble(&root_cid, &book_store).expect("reassemble")
}
