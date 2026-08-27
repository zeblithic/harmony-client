//! ZEB-674 Task 1: per-file DEK encrypt-on-ingest + sealed-DEK-at-rest tests.
//!
//! Drives `ingest_content_encrypted_inner` with a recording ingest handler
//! (mirrors `tests/content/folder_ingest_walker_integration.rs`) so the
//! round-trip exercises the real path: fresh DEK → streamed v3-frame encrypt
//! (`file_stream_crypto::FrameSealer`, ZEB-724) → encrypted+serveable ingest →
//! sealed-DEK store on `OwnerState`.
//!
//! ZEB-724 streams the plaintext through the chunker via a duplex pipe, so
//! `ingest_content_encrypted_inner` now takes a `tokio::fs::File` reader
//! instead of a `Vec<u8>`. It also means even a SMALL plaintext no longer
//! necessarily maps to "one leaf's bytes ARE the whole ciphertext" the way
//! the ZEB-674 whole-blob `encrypt_blob` path did — the ciphertext is
//! whatever the FastCDC chunker emits over the v3 STREAM byte-stream. Tests
//! that need the actual ciphertext reassemble the recorded DAG via
//! `harmony_content::dag::reassemble` and decrypt with
//! `file_stream_crypto::decrypt_stream` (see `reassemble_from_store` below;
//! this mirrors `tests/file_sharing_streaming.rs`, which is the ZEB-724
//! multi-frame/multi-chunk counterpart of this file's round trip).
//!
//! No owner is minted and the keychain is never touched: the KeyTree is
//! obtained via `KeyTree::derive` (the same primitive a mint produces), so
//! the ZEB-428 keychain-isolation rule is satisfied by avoidance.

use std::sync::{Arc, Mutex};

use harmony_app::content_index::ContentIndex;
use harmony_app::owner_state_crdt::OwnerState;
use harmony_app::owner_state_crypto::KeyTree;
use harmony_app::owner_state_types::{OwnerAddr, ReceivedFileGrant};
use harmony_content::cid::{ContentFlags, ContentId};

#[path = "common/file_sharing_helpers.rs"]
mod file_sharing_helpers;
use file_sharing_helpers::{reassemble_from_store, spawn_recording_store, write_temp};

/// Fresh in-memory `ContentIndex` backed by a leaked tempdir — matches the
/// `folder_ingest_walker_integration.rs` / `path_ingest_tests` patterns.
fn fresh_content_index() -> Arc<Mutex<ContentIndex>> {
    let dir = tempfile::tempdir().expect("tempdir");
    let idx = ContentIndex::load(
        Some(&harmony_app::device_dataset_file::test_cipher()),
        dir.path(),
    );
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
    let (_dir, path) = write_temp(&plaintext).await;
    let reader = tokio::fs::File::open(&path).await.unwrap();

    let result = harmony_app::ingest_content_encrypted_inner(
        &ingest_tx,
        &content_index,
        None,
        &crdt_state,
        &std::sync::atomic::AtomicBool::new(false),
        &keytree,
        None,
        reader,
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

    // (3) Reassemble the recorded ciphertext DAG and decrypt it with the v3
    //     stream decryptor. ZEB-724 streams the ciphertext through the
    //     chunker, so we can no longer assume "single chunk ⇒ leaf bytes ARE
    //     the whole ciphertext" — reassembly is required even for small
    //     inputs (the v3 header + AEAD tag still round through the chunker).
    let ciphertext = reassemble_from_store(&store, &root_bytes);
    let recovered =
        harmony_app::file_stream_crypto::decrypt_stream(&dek, &ciphertext).expect("v3 decrypt");
    assert_eq!(
        recovered, plaintext,
        "decrypted ciphertext must equal the original plaintext"
    );
}

/// The value stored in `OwnerState.file_deks` after a real encrypted ingest is
/// the SEALED blob, never the raw DEK bytes. Chunking-independent — untouched
/// by the ZEB-724 streaming rework beyond the reader-argument shape.
#[tokio::test]
async fn sealed_dek_at_rest_is_not_plaintext() {
    let keytree = KeyTree::derive(&[0x42u8; 32]).expect("keytree");
    let crdt_state = Arc::new(tokio::sync::Mutex::new(OwnerState::default()));
    let content_index = fresh_content_index();
    let (ingest_tx, _store) = spawn_recording_store();

    let (_dir, path) = write_temp(b"top secret").await;
    let reader = tokio::fs::File::open(&path).await.unwrap();

    let result = harmony_app::ingest_content_encrypted_inner(
        &ingest_tx,
        &content_index,
        None,
        &crdt_state,
        &std::sync::atomic::AtomicBool::new(false),
        &keytree,
        None,
        reader,
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
/// ORIGINAL PLAINTEXT once `decrypt_personal_file_if_held` runs.
///
/// ZEB-724 Task 3: `decrypt_personal_file_if_held` is now v3 STREAM-aware
/// (`file_stream_crypto::decrypt_stream`), so this reassembles the recorded
/// DAG into the real ciphertext and asserts the full decrypt-on-read
/// round-trip through the production function, not just the ingest half.
#[tokio::test]
async fn owner_encrypted_file_decrypts_to_plaintext() {
    let keytree = KeyTree::derive(&[0x42u8; 32]).expect("keytree");
    let crdt_state = Arc::new(tokio::sync::Mutex::new(OwnerState::default()));
    let content_index = fresh_content_index();
    let (ingest_tx, store) = spawn_recording_store();

    let plaintext = b"ZEB-674 T12 owner read path".to_vec();
    let (_dir, path) = write_temp(&plaintext).await;
    let reader = tokio::fs::File::open(&path).await.unwrap();

    let result = harmony_app::ingest_content_encrypted_inner(
        &ingest_tx,
        &content_index,
        None,
        &crdt_state,
        &std::sync::atomic::AtomicBool::new(false),
        &keytree,
        None,
        reader,
        "secret.txt".to_string(),
    )
    .await
    .expect("encrypted ingest succeeds");

    let root_bytes: [u8; 32] =
        <[u8; 32]>::try_from(hex::decode(&result.cid).expect("cid hex")).expect("32 bytes");
    let ciphertext = reassemble_from_store(&store, &root_bytes);
    assert_ne!(
        ciphertext, plaintext,
        "fetched bytes are ciphertext pre-decrypt"
    );

    let cid = ContentId::from_bytes(root_bytes);
    let st = crdt_state.lock().await;
    let recovered = harmony_app::decrypt_personal_file_if_held(ciphertext, cid, &st, &keytree)
        .expect("owner decrypts their own encrypted file");
    assert_eq!(
        recovered, plaintext,
        "decrypt-on-read must recover the original plaintext through the v3 path"
    );
}

/// A PUBLIC (unencrypted-flag) CID is never decrypted: the fetched bytes pass
/// through byte-identical, even with a loaded owner + keytree. Guards against
/// the personal-file decrypt engaging for non-encrypted content. Untouched by
/// ZEB-724 — never drives `ingest_content_encrypted_inner`.
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
///
/// ZEB-724 Task 3: like `owner_encrypted_file_decrypts_to_plaintext`, this
/// now asserts the full decrypt-on-read round-trip through the v3-aware
/// `decrypt_personal_file_if_held`, not just the ingest + grantee-state setup.
#[tokio::test]
async fn received_grant_file_decrypts_to_plaintext() {
    let keytree = KeyTree::derive(&[0x42u8; 32]).expect("keytree");
    let owner_state = Arc::new(tokio::sync::Mutex::new(OwnerState::default()));
    let content_index = fresh_content_index();
    let (ingest_tx, store) = spawn_recording_store();

    let plaintext = b"ZEB-674 T12 grantee read path".to_vec();
    let (_dir, path) = write_temp(&plaintext).await;
    let reader = tokio::fs::File::open(&path).await.unwrap();

    let result = harmony_app::ingest_content_encrypted_inner(
        &ingest_tx,
        &content_index,
        None,
        &owner_state,
        &std::sync::atomic::AtomicBool::new(false),
        &keytree,
        None,
        reader,
        "shared.txt".to_string(),
    )
    .await
    .expect("encrypted ingest succeeds");

    let cid = root_cid_from_hex(&result.cid);
    let ciphertext = reassemble_from_store(&store, &cid.to_bytes());
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
        .expect("grantee decrypts a file shared with them");
    assert_eq!(
        recovered, plaintext,
        "grantee decrypt-on-read must recover the original plaintext through the v3 path"
    );
}

/// COMMUNITY-SAFETY guarantee: a community/space artifact also carries the
/// ENCRYPTED flag, but its key lives in the epoch-key path — NOT this node's
/// personal `file_deks` / `received_file_grants`. `decrypt_personal_file_if_held`
/// must return such bytes UNCHANGED (the "encrypted but no personal DEK held"
/// branch) so those artifacts keep flowing to `decrypt_and_verify_artifact`
/// undisturbed. This is distinct from `public_file_passes_through_unchanged`,
/// which exercises the flag-CLEAR path; here the flag is SET yet no personal
/// DEK is held. Personal decrypt must never eat a community payload. Untouched
/// by ZEB-724 — never drives `ingest_content_encrypted_inner`.
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
/// encrypted ingest, this reassembles the recorded ciphertext DAG, flips one
/// byte, and decrypts directly with `file_stream_crypto::decrypt_stream` —
/// the real ZEB-724 decrypt primitive the ingest's ciphertext is written for.
///
/// ZEB-724 adaptation: the original test drove this through
/// `decrypt_personal_file_if_held`, which is not yet DAG/v3-aware (Task 3).
/// Exercising `decrypt_stream` directly keeps the tamper-detection guarantee
/// under real test coverage instead of leaving this test assertion-free.
#[tokio::test]
async fn tampered_ciphertext_surfaces_error() {
    let keytree = KeyTree::derive(&[0x42u8; 32]).expect("keytree");
    let crdt_state = Arc::new(tokio::sync::Mutex::new(OwnerState::default()));
    let content_index = fresh_content_index();
    let (ingest_tx, store) = spawn_recording_store();

    let plaintext = b"ZEB-674 T12 tamper-detection path".to_vec();
    let (_dir, path) = write_temp(&plaintext).await;
    let reader = tokio::fs::File::open(&path).await.unwrap();

    let result = harmony_app::ingest_content_encrypted_inner(
        &ingest_tx,
        &content_index,
        None,
        &crdt_state,
        &std::sync::atomic::AtomicBool::new(false),
        &keytree,
        None,
        reader,
        "secret.txt".to_string(),
    )
    .await
    .expect("encrypted ingest succeeds");

    let root_bytes: [u8; 32] =
        <[u8; 32]>::try_from(hex::decode(&result.cid).expect("cid hex")).expect("32 bytes");
    let sealed = {
        let st = crdt_state.lock().await;
        st.file_deks.get(&root_bytes).cloned().expect("sealed DEK")
    };
    let dek = harmony_app::file_sharing::open_dek_at_rest(&keytree, &sealed).expect("unseal");

    let mut ciphertext = reassemble_from_store(&store, &root_bytes);
    // Flip the last byte, which lands in the final frame's Poly1305 tag, so
    // authentication fails. (Any single-byte change to nonce/body/tag breaks
    // AEAD; the tag byte makes the intent explicit.)
    let last = ciphertext.len() - 1;
    ciphertext[last] ^= 0xff;

    let recovered = harmony_app::file_stream_crypto::decrypt_stream(&dek, &ciphertext);
    assert!(
        recovered.is_err(),
        "tampered ciphertext must surface a decrypt error, not silent corruption"
    );
}

/// ZEB-1012: a post-ingest failure — here the ZEB-1011 detach-reject arm,
/// forced deterministically via a pre-set detach flag — must best-effort
/// evict EXACTLY the admitted set via `ContentVerbRequest::RollbackIngest`,
/// and commit no DEK row. The capturing verb channel stands in for the event
/// loop's rollback arm (whose cascade is unit-tested in
/// `event_loop::pin_cascade_tests`); this test pins the caller half: which
/// CIDs the failure arm names, and that it names them at all.
#[tokio::test]
async fn failed_ingest_rolls_back_admitted_cids() {
    let keytree = KeyTree::derive(&[0x42u8; 32]).expect("keytree");
    let crdt_state = Arc::new(tokio::sync::Mutex::new(OwnerState::default()));
    let content_index = fresh_content_index();
    let (ingest_tx, store) = spawn_recording_store();

    // Capturing verb channel: records each RollbackIngest's cid list and
    // acks with the count, the way the real arm replies.
    let (verb_tx, mut verb_rx) =
        tokio::sync::mpsc::channel::<harmony_app::event_loop::ContentVerbRequest>(8);
    let rollbacks: Arc<Mutex<Vec<Vec<[u8; 32]>>>> = Arc::new(Mutex::new(Vec::new()));
    let rollbacks_c = rollbacks.clone();
    tokio::spawn(async move {
        while let Some(req) = verb_rx.recv().await {
            if let harmony_app::event_loop::ContentVerbRequest::RollbackIngest { cids, reply } = req
            {
                let n = cids.len();
                rollbacks_c.lock().unwrap().push(cids);
                let _ = reply.send(Ok(n));
            }
        }
    });

    let (_dir, path) = write_temp(b"ZEB-1012 rollback on detach reject").await;
    let reader = tokio::fs::File::open(&path).await.unwrap();

    let result = harmony_app::ingest_content_encrypted_inner(
        &ingest_tx,
        &content_index,
        Some(&verb_tx),
        &crdt_state,
        // Detached BEFORE the ingest: the ZEB-1011 guard must reject after
        // the whole ciphertext tree was already admitted — a deterministic
        // post-admission failure arm.
        &std::sync::atomic::AtomicBool::new(true),
        &keytree,
        None,
        reader,
        "doomed.txt".to_string(),
    )
    .await;
    let err = result.expect_err("detached owner-state must reject the ingest");
    assert!(
        err.contains("detached"),
        "failure must be the detach rejection, got: {err}"
    );

    // The rollback names EXACTLY the set the recording store admitted.
    // Scoped so the std-mutex guards drop before the tokio lock below
    // (clippy::await_holding_lock).
    {
        let rollbacks = rollbacks.lock().unwrap();
        assert_eq!(rollbacks.len(), 1, "exactly one rollback request");
        let rolled: std::collections::HashSet<String> =
            rollbacks[0].iter().map(hex::encode).collect();
        let admitted: std::collections::HashSet<String> =
            store.lock().unwrap().keys().cloned().collect();
        assert!(!admitted.is_empty(), "sanity: chunks were admitted");
        assert_eq!(rolled, admitted, "rollback covers the exact admitted set");
    }

    // No DEK row was committed (the detach guard fired before the insert).
    assert!(
        crdt_state.lock().await.file_deks.is_empty(),
        "no file_deks row after a detach-rejected ingest"
    );
}

/// A sealed DEK stored on `OwnerState.file_deks` survives a save→reload cycle
/// and still unseals to a usable DEK. Chunking-independent — untouched by
/// ZEB-724, never drives `ingest_content_encrypted_inner`.
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
    save_crdt(
        &harmony_app::device_dataset_file::test_cipher(),
        &path,
        &state,
    )
    .expect("save_crdt");
    let reloaded =
        load_crdt(&harmony_app::device_dataset_file::test_cipher(), &path).expect("load_crdt");

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
