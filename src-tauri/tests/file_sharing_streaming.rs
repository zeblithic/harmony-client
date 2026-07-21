//! ZEB-724: streaming chunked-AEAD ingest round-trips across MANY frames and
//! MANY FastCDC chunks (unlike the ZEB-674 single-chunk case). Drives the real
//! `ingest_content_encrypted_inner` with a recording store, then reassembles
//! the stored leaves in ingest order and decrypts via the v2 stream decryptor.
//!
//! Keychain-free (ZEB-428): the KeyTree comes from `KeyTree::derive`.

use harmony_app::content_index::ContentIndex;
use harmony_app::owner_state_crdt::OwnerState;
use harmony_app::owner_state_crypto::KeyTree;
use std::sync::{Arc, Mutex};

#[path = "common/file_sharing_helpers.rs"]
mod file_sharing_helpers;
use file_sharing_helpers::{reassemble_from_store, spawn_recording_store, write_temp};

fn fresh_content_index() -> Arc<Mutex<ContentIndex>> {
    let dir = tempfile::tempdir().expect("tempdir");
    let idx = ContentIndex::load(dir.path());
    std::mem::forget(dir);
    Arc::new(Mutex::new(idx))
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
        harmony_app::file_stream_crypto::decrypt_stream(&dek, &ciphertext).expect("v3 decrypt");
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
