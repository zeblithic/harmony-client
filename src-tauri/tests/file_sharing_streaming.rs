//! ZEB-724: streaming chunked-AEAD ingest round-trips across MANY frames and
//! MANY FastCDC chunks (unlike the ZEB-674 single-chunk case). Drives the real
//! `ingest_content_encrypted_inner` with a recording store, then reassembles
//! the stored leaves in ingest order and decrypts via the v2 stream decryptor.
//!
//! Keychain-free (ZEB-428): the KeyTree comes from `KeyTree::derive`.

use harmony_app::content_index::ContentIndex;
use harmony_app::event_loop::IngestRequest;
use harmony_app::owner_state_crdt::OwnerState;
use harmony_app::owner_state_crypto::KeyTree;
use harmony_content::book::{BookStore, MemoryBookStore};
use harmony_content::cid::ContentId;
use std::sync::{Arc, Mutex};

type Store = Arc<Mutex<Vec<(String, Vec<u8>)>>>; // (cid_hex, data) in ingest order

fn spawn_recording_store() -> (tokio::sync::mpsc::Sender<IngestRequest>, Store) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<IngestRequest>(256);
    let store: Store = Arc::new(Mutex::new(Vec::new()));
    let store_c = store.clone();
    tokio::spawn(async move {
        while let Some(req) = rx.recv().await {
            store_c
                .lock()
                .unwrap()
                .push((req.cid_hex.clone(), req.data));
            let _ = req.reply.send(Ok(()));
        }
    });
    (tx, store)
}

fn fresh_content_index() -> Arc<Mutex<ContentIndex>> {
    let dir = tempfile::tempdir().expect("tempdir");
    let idx = ContentIndex::load(dir.path());
    std::mem::forget(dir);
    Arc::new(Mutex::new(idx))
}

async fn write_temp(bytes: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("input.bin");
    tokio::fs::write(&path, bytes).await.unwrap();
    (dir, path)
}

/// Reassemble the DAG the recording store captured, via the real
/// `harmony_content::dag::reassemble`. `dag::reassemble` takes `&dyn
/// BookStore`, not a bare closure (confirmed against `dag.rs`), so we replay
/// every recorded `(cid_hex, data)` pair into a `MemoryBookStore` keyed by its
/// real `ContentId` and hand that to `reassemble`.
fn reassemble_from_store(store: &Store, root: &[u8; 32]) -> Vec<u8> {
    let mut book_store = MemoryBookStore::new();
    for (cid_hex, data) in store.lock().unwrap().iter() {
        let bytes: [u8; 32] =
            <[u8; 32]>::try_from(hex::decode(cid_hex).expect("cid hex")).expect("cid 32 bytes");
        book_store.store(ContentId::from_bytes(bytes), data.clone());
    }
    let root_cid = ContentId::from_bytes(*root);
    harmony_content::dag::reassemble(&root_cid, &book_store).expect("reassemble")
}

/// Drive the real `ingest_content_encrypted_inner`, then reassemble the
/// recorded ciphertext DAG and decrypt it with the v2 stream decryptor. The
/// invariant under test: the streamed round-trip must recover `plaintext`
/// byte-for-byte, across many frames and many FastCDC chunks.
async fn round_trip(plaintext: Vec<u8>) {
    let keytree = KeyTree::derive(&[0x42u8; 32]).expect("keytree");
    let crdt_state = Arc::new(tokio::sync::Mutex::new(OwnerState::default()));
    let content_index = fresh_content_index();
    let (ingest_tx, store) = spawn_recording_store();
    let (_dir, path) = write_temp(&plaintext).await;
    let reader = tokio::fs::File::open(&path).await.unwrap();

    let result = harmony_app::ingest_content_encrypted_inner(
        &ingest_tx,
        &content_index,
        &crdt_state,
        &keytree,
        None,
        reader,
        "big.bin".to_string(),
    )
    .await
    .expect("encrypted streaming ingest succeeds");

    // Recover the DEK the ingest stored, keyed by the root CID.
    let root_bytes = harmony_app::parse_cid_hex(&result.cid).expect("cid hex");
    let sealed = {
        let st = crdt_state.lock().await;
        st.file_deks
            .get(&root_bytes)
            .cloned()
            .expect("file_deks[root]")
    };
    let dek = harmony_app::file_sharing::open_dek_at_rest(&keytree, &sealed).expect("unseal dek");

    // Reassemble the ciphertext from the recorded chunks via the content DAG,
    // then decrypt with the v2 stream decryptor.
    let ciphertext = reassemble_from_store(&store, &root_bytes);
    let recovered =
        harmony_app::file_stream_crypto::decrypt_stream(&dek, &ciphertext).expect("v2 decrypt");
    assert_eq!(
        recovered, plaintext,
        "streamed round-trip must recover plaintext"
    );
}

#[tokio::test]
async fn streaming_round_trip_multi_frame_multi_chunk() {
    // ~1.5 MiB: crosses many 64 KiB frames AND multiple 256 KiB+ FastCDC chunks.
    let pt: Vec<u8> = (0..1_500_000u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
        .collect();
    round_trip(pt).await;
}

#[tokio::test]
async fn streaming_round_trip_empty_and_boundary() {
    round_trip(Vec::new()).await;
    round_trip(vec![0u8; 64 * 1024]).await; // exactly one frame
    round_trip(vec![7u8; 64 * 1024 + 1]).await; // one frame + 1
}

#[tokio::test]
#[ignore = "slow: >256 MiB; proves the cap is gone + bounded memory. Run with --ignored."]
async fn streaming_ingest_above_old_cap() {
    // 300 MiB > the removed 256 MiB cap. With streaming this must succeed.
    round_trip(vec![0x5Au8; 300 * 1024 * 1024]).await;
}
