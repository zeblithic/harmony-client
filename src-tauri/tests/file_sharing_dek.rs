//! ZEB-674 Task 1: per-file DEK encrypt-on-ingest + sealed-DEK-at-rest tests.
//!
//! Drives `ingest_content_encrypted_inner` with a recording ingest handler
//! (mirrors `tests/content/folder_ingest_walker_integration.rs`) so the
//! round-trip exercises the real path: fresh DEK → whole-blob encrypt →
//! encrypted+serveable ingest → sealed-DEK store on `OwnerState`.
//!
//! A small (single-chunk, well under `ChunkerConfig::DEFAULT.min_chunk =
//! 256 KiB`) plaintext means exactly one leaf is emitted and its bytes ARE
//! the whole ciphertext, so the round-trip can decrypt it directly without
//! reassembling a bundle tree.
//!
//! No owner is minted and the keychain is never touched: the KeyTree is
//! obtained via `KeyTree::derive` (the same primitive a mint produces), so
//! the ZEB-428 keychain-isolation rule is satisfied by avoidance.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use harmony_app::content_index::ContentIndex;
use harmony_app::event_loop::IngestRequest;
use harmony_app::owner_state_crdt::OwnerState;
use harmony_app::owner_state_crypto::KeyTree;

/// Records each ingested `(cid_hex, bytes)` — a stand-in content store.
type Store = Arc<Mutex<HashMap<String, Vec<u8>>>>;

fn spawn_recording_store() -> (tokio::sync::mpsc::Sender<IngestRequest>, Store) {
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

/// Fresh in-memory `ContentIndex` backed by a leaked tempdir — matches the
/// `folder_ingest_walker_integration.rs` / `path_ingest_tests` patterns.
fn fresh_content_index() -> Arc<Mutex<ContentIndex>> {
    let dir = tempfile::tempdir().expect("tempdir");
    let idx = ContentIndex::load(dir.path());
    std::mem::forget(dir);
    Arc::new(Mutex::new(idx))
}

fn root_cid_from_hex(hex_str: &str) -> harmony_content::cid::ContentId {
    let bytes: [u8; 32] =
        <[u8; 32]>::try_from(hex::decode(hex_str).expect("cid hex")).expect("cid is 32 bytes");
    harmony_content::cid::ContentId::from_bytes(bytes)
}

#[tokio::test]
async fn encrypted_ingest_dek_round_trip() {
    let keytree = KeyTree::derive(&[0x42u8; 32]).expect("keytree");
    let crdt_state = Arc::new(tokio::sync::Mutex::new(OwnerState::default()));
    let content_index = fresh_content_index();
    let (ingest_tx, store) = spawn_recording_store();

    let plaintext = b"ZEB-674 per-file DEK round trip".to_vec();

    let result = harmony_app::ingest_content_encrypted_inner(
        &ingest_tx,
        &content_index,
        &crdt_state,
        &keytree,
        None,
        plaintext.clone(),
        "secret.txt".to_string(),
    )
    .await
    .expect("encrypted ingest succeeds");

    // (1) The returned root CID is encrypted-flagged (EncryptedDurable).
    let root_bytes: [u8; 32] =
        <[u8; 32]>::try_from(hex::decode(&result.cid).expect("cid hex")).expect("32 bytes");
    assert!(
        root_cid_from_hex(&result.cid).flags().encrypted,
        "root CID must carry the encrypted flag"
    );

    // (2) A sealed DEK is stored on OwnerState, keyed by the root CID bytes.
    let sealed = {
        let st = crdt_state.lock().await;
        st.file_deks
            .get(&root_bytes)
            .cloned()
            .expect("sealed DEK stored under the root CID")
    };
    let dek =
        harmony_app::file_sharing::open_dek_at_rest(&keytree, &sealed).expect("unseal DEK at rest");

    // (3) The stored ciphertext decrypts under that DEK back to the original
    //     plaintext. Single-chunk ⇒ exactly one leaf whose bytes are the whole
    //     ciphertext.
    let ciphertext = {
        let s = store.lock().unwrap();
        assert_eq!(s.len(), 1, "single-chunk ingest emits exactly one leaf");
        s.values().next().unwrap().clone()
    };
    let recovered = harmony_app::community_state_sync::decrypt_blob(&dek, &ciphertext)
        .expect("decrypt_blob under unsealed DEK");
    assert_eq!(
        recovered, plaintext,
        "decrypted ciphertext must equal the original plaintext"
    );
}

/// The value stored in `OwnerState.file_deks` after a real encrypted ingest is
/// the SEALED blob, never the raw DEK bytes.
#[tokio::test]
async fn sealed_dek_at_rest_is_not_plaintext() {
    let keytree = KeyTree::derive(&[0x42u8; 32]).expect("keytree");
    let crdt_state = Arc::new(tokio::sync::Mutex::new(OwnerState::default()));
    let content_index = fresh_content_index();
    let (ingest_tx, _store) = spawn_recording_store();

    let result = harmony_app::ingest_content_encrypted_inner(
        &ingest_tx,
        &content_index,
        &crdt_state,
        &keytree,
        None,
        b"top secret".to_vec(),
        "s.txt".to_string(),
    )
    .await
    .expect("encrypted ingest succeeds");

    let root_bytes: [u8; 32] =
        <[u8; 32]>::try_from(hex::decode(&result.cid).expect("cid hex")).expect("32 bytes");
    let sealed = {
        let st = crdt_state.lock().await;
        st.file_deks.get(&root_bytes).cloned().expect("DEK stored")
    };
    // The unsealed DEK is 32 bytes; the stored value is a 60-byte sealed blob
    // (nonce 12 + ciphertext 32 + tag 16) and must differ from those 32 bytes.
    let dek = harmony_app::file_sharing::open_dek_at_rest(&keytree, &sealed).expect("unseal");
    assert_ne!(
        sealed.as_slice(),
        dek.as_bytes().as_slice(),
        "stored file_deks value must not be the raw DEK"
    );
    assert_eq!(
        sealed.len(),
        60,
        "sealed DEK blob is nonce(12)+ct(32)+tag(16)"
    );
}

/// A sealed DEK stored on `OwnerState.file_deks` survives a save→reload cycle
/// and still unseals to a usable DEK.
#[test]
fn file_deks_persist_reload() {
    use harmony_app::file_sharing::{generate_file_dek, open_dek_at_rest, seal_dek_at_rest};
    use harmony_app::owner_state_persist::{load_crdt, save_crdt};

    let keytree = KeyTree::derive(&[0x42u8; 32]).expect("keytree");
    let dek = generate_file_dek();
    let sealed = seal_dek_at_rest(&keytree, &dek).expect("seal");

    let cid_key = [0x99u8; 32];
    let mut state = OwnerState::default();
    state.file_deks.insert(cid_key, sealed);

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("crdt-v2.bin");
    save_crdt(&path, &state).expect("save_crdt");
    let reloaded = load_crdt(&path).expect("load_crdt");

    let reloaded_sealed = reloaded
        .file_deks
        .get(&cid_key)
        .cloned()
        .expect("file_deks entry survives reload");
    let reopened = open_dek_at_rest(&keytree, &reloaded_sealed).expect("unseal after reload");
    assert_eq!(
        reopened.as_bytes(),
        dek.as_bytes(),
        "reloaded DEK must match the original"
    );
}
