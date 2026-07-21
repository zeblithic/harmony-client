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
use harmony_app::owner_state_types::{OwnerAddr, ReceivedFileGrant};
use harmony_content::cid::{ContentFlags, ContentId};

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

/// ZEB-674 Task 12 (Gap B): the READ path. After the encrypted-file ingest
/// stores the ciphertext + sealed DEK, a fetch of that CID must return the
/// ORIGINAL PLAINTEXT once `decrypt_personal_file_if_held` runs — the exact
/// decrypt `export_content`/`fetch_content` now apply to the fetched bytes.
#[tokio::test]
async fn owner_encrypted_file_decrypts_to_plaintext() {
    let keytree = KeyTree::derive(&[0x42u8; 32]).expect("keytree");
    let crdt_state = Arc::new(tokio::sync::Mutex::new(OwnerState::default()));
    let content_index = fresh_content_index();
    let (ingest_tx, store) = spawn_recording_store();

    let plaintext = b"ZEB-674 T12 owner read path".to_vec();

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

    let cid = root_cid_from_hex(&result.cid);
    // Single-chunk ⇒ exactly one leaf whose bytes are the whole ciphertext (the
    // bytes a fetch of this CID would return before decrypt).
    let ciphertext = {
        let s = store.lock().unwrap();
        assert_eq!(s.len(), 1, "single-chunk ingest emits exactly one leaf");
        s.values().next().unwrap().clone()
    };
    assert_ne!(
        ciphertext, plaintext,
        "fetched bytes are ciphertext pre-decrypt"
    );

    let recovered = {
        let st = crdt_state.lock().await;
        harmony_app::decrypt_personal_file_if_held(ciphertext, cid, &st, &keytree)
            .expect("owner's file_deks DEK decrypts the fetched bytes")
    };
    assert_eq!(
        recovered, plaintext,
        "decrypt-on-read must return the original plaintext byte-for-byte"
    );
}

/// A PUBLIC (unencrypted-flag) CID is never decrypted: the fetched bytes pass
/// through byte-identical, even with a loaded owner + keytree. Guards against
/// the personal-file decrypt engaging for non-encrypted content.
#[test]
fn public_file_passes_through_unchanged() {
    let keytree = KeyTree::derive(&[0x42u8; 32]).expect("keytree");
    let state = OwnerState::default();

    let bytes = b"arbitrary public bytes, not ciphertext".to_vec();
    // A public CID: default flags ⇒ encrypted bit clear.
    let public_cid = ContentId::for_book(&bytes, ContentFlags::default()).expect("public cid");
    assert!(!public_cid.flags().encrypted, "sanity: CID is public");

    let out =
        harmony_app::decrypt_personal_file_if_held(bytes.clone(), public_cid, &state, &keytree)
            .expect("public pass-through never errors");
    assert_eq!(out, bytes, "public file bytes must be returned unchanged");
}

/// A file whose DEK lives in `received_file_grants` (shared WITH us, not our
/// own) also decrypts on read. Proves the second lookup branch: `file_deks` is
/// empty, the sealed DEK is only in the grant map.
#[tokio::test]
async fn received_grant_file_decrypts_to_plaintext() {
    let keytree = KeyTree::derive(&[0x42u8; 32]).expect("keytree");
    let owner_state = Arc::new(tokio::sync::Mutex::new(OwnerState::default()));
    let content_index = fresh_content_index();
    let (ingest_tx, store) = spawn_recording_store();

    let plaintext = b"ZEB-674 T12 grantee read path".to_vec();

    let result = harmony_app::ingest_content_encrypted_inner(
        &ingest_tx,
        &content_index,
        &owner_state,
        &keytree,
        None,
        plaintext.clone(),
        "shared.txt".to_string(),
    )
    .await
    .expect("encrypted ingest succeeds");

    let cid = root_cid_from_hex(&result.cid);
    let ciphertext = {
        let s = store.lock().unwrap();
        s.values().next().unwrap().clone()
    };
    // The DEK the owner sealed under the shared KeyTree; a grantee on the same
    // shared KeyTree opens it identically (open_received_file contract).
    let sealed_dek = {
        let st = owner_state.lock().await;
        st.file_deks
            .get(&cid.to_bytes())
            .cloned()
            .expect("sealed DEK")
    };

    // Grantee state: file_deks EMPTY, DEK only in received_file_grants.
    let mut grantee = OwnerState::default();
    grantee.received_file_grants.insert(
        cid.to_bytes(),
        ReceivedFileGrant {
            granter_owner: OwnerAddr([0u8; 16]),
            cid: cid.to_bytes(),
            file_name: "shared.txt".to_string(),
            file_size: ciphertext.len() as u64,
            mime: "application/octet-stream".to_string(),
            sealed_dek,
            received_at: 0,
        },
    );
    assert!(
        grantee.file_deks.is_empty(),
        "grantee owns no file_deks entry"
    );

    let recovered = harmony_app::decrypt_personal_file_if_held(ciphertext, cid, &grantee, &keytree)
        .expect("received_file_grants DEK decrypts the fetched bytes");
    assert_eq!(
        recovered, plaintext,
        "grantee decrypt-on-read must return the original plaintext"
    );
}

/// COMMUNITY-SAFETY guarantee: a community/space artifact also carries the
/// ENCRYPTED flag, but its key lives in the epoch-key path — NOT this node's
/// personal `file_deks` / `received_file_grants`. `decrypt_personal_file_if_held`
/// must return such bytes UNCHANGED (the "encrypted but no personal DEK held"
/// branch) so those artifacts keep flowing to `decrypt_and_verify_artifact`
/// undisturbed. This is distinct from `public_file_passes_through_unchanged`,
/// which exercises the flag-CLEAR path; here the flag is SET yet no personal
/// DEK is held. Personal decrypt must never eat a community payload.
#[test]
fn encrypted_but_no_personal_dek_passes_through_unchanged() {
    let keytree = KeyTree::derive(&[0x42u8; 32]).expect("keytree");
    let state = OwnerState::default();

    let bytes = b"community epoch-encrypted artifact bytes".to_vec();
    // An encrypted-flag CID with NO matching entry in file_deks/received_file_grants.
    let enc_flags = ContentFlags {
        encrypted: true,
        ..ContentFlags::default()
    };
    let cid = ContentId::for_book(&bytes, enc_flags).expect("encrypted cid");
    assert!(
        cid.flags().encrypted,
        "sanity: CID carries the encrypted flag"
    );
    assert!(
        state.file_deks.is_empty() && state.received_file_grants.is_empty(),
        "sanity: node holds no personal DEK for this CID"
    );

    let out = harmony_app::decrypt_personal_file_if_held(bytes.clone(), cid, &state, &keytree)
        .expect("no-personal-DEK path never errors");
    assert_eq!(
        out, bytes,
        "encrypted community artifact with no personal DEK must pass through byte-for-byte"
    );
}

/// TAMPER detection: a held DEK + a corrupted ciphertext must surface an `Err`
/// (AEAD authentication failure), never silent corruption. After a real
/// encrypted ingest, flipping one byte of the stored ciphertext and re-running
/// decrypt-on-read proves the ChaCha20-Poly1305 tag rejects the modified body.
#[tokio::test]
async fn tampered_ciphertext_surfaces_error() {
    let keytree = KeyTree::derive(&[0x42u8; 32]).expect("keytree");
    let crdt_state = Arc::new(tokio::sync::Mutex::new(OwnerState::default()));
    let content_index = fresh_content_index();
    let (ingest_tx, store) = spawn_recording_store();

    let plaintext = b"ZEB-674 T12 tamper-detection path".to_vec();

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

    let cid = root_cid_from_hex(&result.cid);
    let mut ciphertext = {
        let s = store.lock().unwrap();
        s.values().next().unwrap().clone()
    };
    // Flip the last byte, which lands in the Poly1305 tag, so authentication
    // fails. (Any single-byte change to nonce/body/tag breaks AEAD; the tag
    // byte makes the intent explicit.)
    let last = ciphertext.len() - 1;
    ciphertext[last] ^= 0xff;

    let recovered = {
        let st = crdt_state.lock().await;
        harmony_app::decrypt_personal_file_if_held(ciphertext, cid, &st, &keytree)
    };
    assert!(
        recovered.is_err(),
        "tampered ciphertext must surface a decrypt error, not silent corruption"
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
