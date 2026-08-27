//! ZEB-674 Task 4 (C4): grantee receive + read path.
//!
//! Exercises the grantee-side primitives: ingesting an inbound `grant_push`
//! payload onto `OwnerState.received_file_grants` (`ingest_grant_push`) and
//! re-opening the sealed DEK on demand to decrypt the shared file
//! (`open_received_file`).
//!
//! No owner is minted and the keychain is never touched: X25519 device
//! keypairs are derived deterministically via HKDF, so the ZEB-428
//! keychain-isolation rule is satisfied by avoidance (mirrors
//! `file_sharing_grants.rs`).

use harmony_app::community_state_sync::{decrypt_blob, encrypt_blob};
use harmony_app::content_index::ContentIndex;
use harmony_app::file_sharing::{
    ingest_grant_push, open_dek_at_rest, open_received_file, seal_grant_for_devices, FileGrantInner,
};
use harmony_app::owner_state_crdt::OwnerState;
use harmony_app::owner_state_crypto::KeyTree;
use harmony_app::owner_state_types::{EpochKey, OwnerAddr, ReceivedFileGrant};
use harmony_content::cid::ContentId;
use std::sync::{Arc, Mutex};

#[path = "common/file_sharing_helpers.rs"]
mod file_sharing_helpers;
use file_sharing_helpers::{reassemble_from_store, spawn_recording_store, write_temp};

/// Deterministic grantee shared KeyTree (mirrors `file_sharing_dek.rs`). A fresh
/// derivation from the same material models a DIFFERENT device of the same owner
/// — the re-seal must open under any of them.
fn test_keytree() -> KeyTree {
    KeyTree::derive(&[0x9Au8; 32]).expect("keytree")
}

/// Deterministic test X25519 keypair (mirrors `file_sharing_grants.rs`).
/// Returns (priv_scalar, pub).
fn make_x25519_keypair(seed_byte: u8) -> ([u8; 32], [u8; 32]) {
    use hkdf::Hkdf;
    use sha2::Sha256;
    use x25519_dalek::{PublicKey, StaticSecret};

    let seed = [seed_byte; 32];
    let hk = Hkdf::<Sha256>::new(None, &seed);
    let mut scalar = [0u8; 32];
    hk.expand(b"harmony-zeb-674-test-x25519-scalar", &mut scalar)
        .expect("HKDF 32 bytes always works");

    let secret = StaticSecret::from(scalar);
    let public = PublicKey::from(&secret);
    (scalar, *public.as_bytes())
}

/// Build the realistic `grant_push` wire value from per-device sealed blobs:
/// CBOR of `Vec<serde_bytes Vec<u8>>` (each element a byte-string), exactly as
/// the C3 sender rung produces it (see `butler_deposit::butler_cannot_open_grant_push`).
fn wrap_grant_push(sealed_blobs: &[Vec<u8>]) -> Vec<u8> {
    let list: Vec<serde_bytes::ByteBuf> = sealed_blobs
        .iter()
        .cloned()
        .map(serde_bytes::ByteBuf::from)
        .collect();
    let mut bytes = Vec::new();
    ciborium::into_writer(&list, &mut bytes).expect("encode grant_push list");
    bytes
}

/// End-to-end grantee path: seal a grant to a grantee device → ingest it →
/// re-open the DEK → decrypt the file's ciphertext back to plaintext.
#[test]
fn grantee_ingest_then_decrypt() {
    let (grantee_priv, grantee_pub) = make_x25519_keypair(0x41);

    // The per-file DEK the granter used to encrypt the file. The test encrypts
    // the plaintext under THIS key, so a correctly-recovered DEK must decrypt it.
    let dek_bytes = [0x5Au8; 32];
    let dek = EpochKey::new(dek_bytes);
    let plaintext = b"the-shared-file-body-contents".to_vec();
    let ciphertext = encrypt_blob(&dek, &plaintext).expect("encrypt file under DEK");

    let cid_bytes = [0xC1u8; 32];
    let inner = FileGrantInner {
        cid: cid_bytes,
        file_name: "shared-notes.md".to_string(),
        file_size: plaintext.len() as u64,
        mime: "text/markdown".to_string(),
        dek: dek_bytes,
    };

    // Seal to the grantee's (single) device and wrap as the deposit wire value.
    let sealed = seal_grant_for_devices(&inner, &[grantee_pub]).expect("seal grant");
    let grant_push = wrap_grant_push(&sealed);

    let granter = OwnerAddr([0x11u8; 16]);
    let keytree = test_keytree();
    let mut state = OwnerState::default();

    // Ingest → Some(cid). The recovered DEK is RE-SEALED under the grantee's
    // shared KeyTree (device-agnostic), not the opened per-device envelope.
    let ingested = ingest_grant_push(&mut state, &grantee_priv, &keytree, granter, &grant_push)
        .expect("ingest ok")
        .expect("a blob opened with our device key");
    assert_eq!(ingested, ContentId::from_bytes(cid_bytes), "returned cid");

    let rec = state
        .received_file_grants
        .get(&cid_bytes)
        .expect("received_file_grants populated for cid");
    assert_eq!(
        rec.granter_owner, granter,
        "granter is the passed-in sender"
    );
    assert_eq!(rec.cid, cid_bytes);
    assert_eq!(rec.file_name, "shared-notes.md");
    assert_eq!(rec.file_size, plaintext.len() as u64);
    assert_eq!(rec.mime, "text/markdown");
    assert_ne!(
        rec.sealed_dek.as_slice(),
        dek_bytes.as_slice(),
        "the stored blob must be the sealed envelope, never the raw DEK"
    );
    assert_ne!(
        rec.sealed_dek, sealed[0],
        "the stored blob is the KeyTree re-seal, NOT the per-device envelope"
    );

    // Device-agnostic: a FRESH KeyTree of the same shared material (a different
    // device of the same owner) opens the stored blob directly.
    let via_other_device = open_dek_at_rest(&test_keytree(), &rec.sealed_dek)
        .expect("a different device with the same shared KeyTree opens the re-sealed DEK");
    assert_eq!(
        via_other_device.as_bytes(),
        &dek_bytes,
        "device-agnostic DEK"
    );

    // Open (grantee read path) → the DEK via the shared KeyTree; it decrypts the
    // file back to plaintext.
    let recovered = open_received_file(&state, &keytree, ContentId::from_bytes(cid_bytes))
        .expect("open received file");
    assert_eq!(recovered.as_bytes(), &dek_bytes, "recovered DEK matches");

    let decrypted = decrypt_blob(&recovered, &ciphertext).expect("decrypt with recovered DEK");
    assert_eq!(decrypted, plaintext, "recovered DEK decrypts the file");
}

/// A grant sealed ONLY to a device this owner does not hold → `Ok(None)` and no
/// state mutation (the honest new-device / wrong-recipient edge).
#[test]
fn grantee_ingest_no_matching_device_is_none() {
    // Seal to `other`'s device; ingest with `us` (a different key we DO hold).
    let (_other_priv, other_pub) = make_x25519_keypair(0x51);
    let (us_priv, _us_pub) = make_x25519_keypair(0x52);

    let inner = FileGrantInner {
        cid: [0xD2u8; 32],
        file_name: "not-for-us.bin".to_string(),
        file_size: 10,
        mime: "application/octet-stream".to_string(),
        dek: [0x77u8; 32],
    };
    let sealed = seal_grant_for_devices(&inner, &[other_pub]).expect("seal grant");
    let grant_push = wrap_grant_push(&sealed);

    let mut state = OwnerState::default();
    let keytree = test_keytree();
    let out = ingest_grant_push(
        &mut state,
        &us_priv,
        &keytree,
        OwnerAddr([0x22u8; 16]),
        &grant_push,
    )
    .expect("ingest ok");
    assert_eq!(out, None, "no blob opens with our key → Ok(None)");
    assert!(
        state.received_file_grants.is_empty(),
        "no state change when nothing opened"
    );
}

// --- ZEB-994: replay idempotency of `ingest_grant_push`. -------------------
//
// The dm-inbox `ingested_by` ack and the owner-state grant record persist
// through two independent debounced engines; a crash between the two flushes
// replays the same deposit at the next startup sweep. An IDENTICAL re-apply
// must be a no-op `Ok(None)` — no `received_at` refresh, no nondeterministic
// re-seal churn — while a genuinely-changed grant (rotated DEK, different
// granter, new metadata) must still replace. The self-heal test pins the
// corrupt-stored-envelope edge: an unopenable `sealed_dek` counts as changed.

/// Re-ingesting the SAME grant_push bytes is a no-op: `Ok(None)`, stored
/// `received_at` and `sealed_dek` untouched, exactly one entry.
#[test]
fn reingest_identical_grant_is_noop_none() {
    let (grantee_priv, grantee_pub) = make_x25519_keypair(0x61);
    let inner = FileGrantInner {
        cid: [0xE1u8; 32],
        file_name: "replayed.md".to_string(),
        file_size: 21,
        mime: "text/markdown".to_string(),
        dek: [0x6Bu8; 32],
    };
    let sealed = seal_grant_for_devices(&inner, &[grantee_pub]).expect("seal grant");
    let grant_push = wrap_grant_push(&sealed);

    let granter = OwnerAddr([0x31u8; 16]);
    let keytree = test_keytree();
    let mut state = OwnerState::default();

    ingest_grant_push(&mut state, &grantee_priv, &keytree, granter, &grant_push)
        .expect("first ingest ok")
        .expect("first ingest records");

    // Sentinel stamp: a no-op re-apply must NOT refresh `received_at` (the
    // real first stamp is wall-clock; 12_345 can only survive if untouched).
    let rec = state
        .received_file_grants
        .get_mut(&inner.cid)
        .expect("entry recorded");
    rec.received_at = 12_345;
    let sealed_dek_before = rec.sealed_dek.clone();

    let replay = ingest_grant_push(&mut state, &grantee_priv, &keytree, granter, &grant_push)
        .expect("replay ingest ok");
    assert_eq!(replay, None, "identical re-apply is a no-op `Ok(None)`");

    let rec = state
        .received_file_grants
        .get(&inner.cid)
        .expect("entry still recorded");
    assert_eq!(rec.received_at, 12_345, "received_at untouched on no-op");
    assert_eq!(
        rec.sealed_dek, sealed_dek_before,
        "sealed_dek not re-sealed on no-op (no CRDT value churn)"
    );
    assert_eq!(state.received_file_grants.len(), 1, "exactly one entry");
}

/// A re-grant of the same cid with a ROTATED DEK is a genuine change: `Some`,
/// stored DEK replaced, `received_at` refreshed off the sentinel.
#[test]
fn reingest_rotated_dek_replaces() {
    let (grantee_priv, grantee_pub) = make_x25519_keypair(0x62);
    let cid = [0xE2u8; 32];
    let mk_inner = |dek: [u8; 32]| FileGrantInner {
        cid,
        file_name: "rotated.md".to_string(),
        file_size: 9,
        mime: "text/markdown".to_string(),
        dek,
    };
    let push_a = wrap_grant_push(
        &seal_grant_for_devices(&mk_inner([0xA1u8; 32]), &[grantee_pub]).expect("seal a"),
    );
    let push_b = wrap_grant_push(
        &seal_grant_for_devices(&mk_inner([0xB2u8; 32]), &[grantee_pub]).expect("seal b"),
    );

    let granter = OwnerAddr([0x32u8; 16]);
    let keytree = test_keytree();
    let mut state = OwnerState::default();

    ingest_grant_push(&mut state, &grantee_priv, &keytree, granter, &push_a)
        .expect("ingest a ok")
        .expect("a records");
    state
        .received_file_grants
        .get_mut(&cid)
        .expect("entry")
        .received_at = 12_345;

    let out = ingest_grant_push(&mut state, &grantee_priv, &keytree, granter, &push_b)
        .expect("ingest b ok");
    assert!(out.is_some(), "a rotated DEK is a genuine change → `Some`");

    let rec = state.received_file_grants.get(&cid).expect("entry");
    let stored = open_dek_at_rest(&keytree, &rec.sealed_dek).expect("open replaced dek");
    assert_eq!(
        stored.as_bytes(),
        &[0xB2u8; 32],
        "stored DEK is the rotation"
    );
    assert_ne!(rec.received_at, 12_345, "received_at refreshed on change");
}

/// The same grant bytes re-deposited by a DIFFERENT authenticated granter is a
/// genuine change (attribution matters): `Some`, granter-of-record replaced.
#[test]
fn reingest_different_granter_replaces() {
    let (grantee_priv, grantee_pub) = make_x25519_keypair(0x63);
    let inner = FileGrantInner {
        cid: [0xE3u8; 32],
        file_name: "reattributed.md".to_string(),
        file_size: 5,
        mime: "text/markdown".to_string(),
        dek: [0x6Du8; 32],
    };
    let grant_push =
        wrap_grant_push(&seal_grant_for_devices(&inner, &[grantee_pub]).expect("seal"));

    let keytree = test_keytree();
    let mut state = OwnerState::default();
    ingest_grant_push(
        &mut state,
        &grantee_priv,
        &keytree,
        OwnerAddr([0x33u8; 16]),
        &grant_push,
    )
    .expect("first ingest ok")
    .expect("records");

    let granter_b = OwnerAddr([0x44u8; 16]);
    let out = ingest_grant_push(&mut state, &grantee_priv, &keytree, granter_b, &grant_push)
        .expect("second ingest ok");
    assert!(out.is_some(), "a different granter is a genuine change");
    assert_eq!(
        state
            .received_file_grants
            .get(&inner.cid)
            .expect("entry")
            .granter_owner,
        granter_b,
        "granter-of-record replaced"
    );
}

/// Same DEK but changed display metadata (a re-share after rename) is a
/// genuine change: `Some`, stored metadata replaced.
#[test]
fn reingest_changed_metadata_replaces() {
    let (grantee_priv, grantee_pub) = make_x25519_keypair(0x64);
    let cid = [0xE4u8; 32];
    let dek = [0x6Eu8; 32];
    let mk_inner = |name: &str| FileGrantInner {
        cid,
        file_name: name.to_string(),
        file_size: 7,
        mime: "text/markdown".to_string(),
        dek,
    };
    let push_a =
        wrap_grant_push(&seal_grant_for_devices(&mk_inner("old.md"), &[grantee_pub]).expect("a"));
    let push_b =
        wrap_grant_push(&seal_grant_for_devices(&mk_inner("new.md"), &[grantee_pub]).expect("b"));

    let granter = OwnerAddr([0x35u8; 16]);
    let keytree = test_keytree();
    let mut state = OwnerState::default();
    ingest_grant_push(&mut state, &grantee_priv, &keytree, granter, &push_a)
        .expect("ingest a ok")
        .expect("a records");

    let out = ingest_grant_push(&mut state, &grantee_priv, &keytree, granter, &push_b)
        .expect("ingest b ok");
    assert!(out.is_some(), "changed metadata is a genuine change");
    assert_eq!(
        state
            .received_file_grants
            .get(&cid)
            .expect("entry")
            .file_name,
        "new.md",
        "stored metadata replaced"
    );
}

/// A stored entry whose `sealed_dek` no longer opens (corruption) counts as
/// CHANGED: the identical re-apply replaces it, self-healing the entry.
#[test]
fn reingest_unopenable_stored_dek_self_heals() {
    let (grantee_priv, grantee_pub) = make_x25519_keypair(0x65);
    let inner = FileGrantInner {
        cid: [0xE5u8; 32],
        file_name: "healed.md".to_string(),
        file_size: 3,
        mime: "text/markdown".to_string(),
        dek: [0x6Fu8; 32],
    };
    let grant_push =
        wrap_grant_push(&seal_grant_for_devices(&inner, &[grantee_pub]).expect("seal"));

    let granter = OwnerAddr([0x36u8; 16]);
    let keytree = test_keytree();
    let mut state = OwnerState::default();
    ingest_grant_push(&mut state, &grantee_priv, &keytree, granter, &grant_push)
        .expect("first ingest ok")
        .expect("records");

    // Corrupt the stored envelope: the guard must treat "cannot open" as
    // changed, not as identical (fail toward re-recording).
    state
        .received_file_grants
        .get_mut(&inner.cid)
        .expect("entry")
        .sealed_dek = vec![0xFFu8; 60];

    let out = ingest_grant_push(&mut state, &grantee_priv, &keytree, granter, &grant_push)
        .expect("re-ingest ok");
    assert!(out.is_some(), "unopenable stored envelope → re-record");
    let rec = state.received_file_grants.get(&inner.cid).expect("entry");
    let stored = open_dek_at_rest(&keytree, &rec.sealed_dek).expect("healed envelope opens");
    assert_eq!(stored.as_bytes(), &inner.dek, "self-healed to the real DEK");
}

// --- ZEB-724 Task 3: grantee decrypt-on-read of a MULTI-FRAME v3 file. -----
//
// Everything below drives the real streaming ingest (`ingest_content_encrypted_inner`)
// so the ciphertext is the actual v3 STREAM byte-stream (crossing several 64 KiB
// frames), then wires up a grantee `OwnerState.received_file_grants` entry the
// way a real grant delivery would (minus the sealed-envelope wire format, which
// `grantee_ingest_then_decrypt` above already covers) and proves
// `decrypt_personal_file_if_held` recovers the original plaintext through the
// v3 path. Mirrors the reassembly pattern in `tests/file_sharing_streaming.rs`
// and `tests/file_sharing_dek.rs`.

fn fresh_content_index() -> Arc<Mutex<ContentIndex>> {
    let dir = tempfile::tempdir().expect("tempdir");
    let idx = ContentIndex::load(
        Some(&harmony_app::device_dataset_file::test_cipher()),
        dir.path(),
    );
    std::mem::forget(dir);
    Arc::new(Mutex::new(idx))
}

/// A grantee decrypting a MULTI-FRAME v3 file (crosses several 64 KiB frames)
/// via `decrypt_personal_file_if_held` must recover the original plaintext.
/// This is the read-side counterpart to `grantee_ingest_then_decrypt` — that
/// test proves the sealed-envelope grant delivery mechanics; this one proves
/// the actual bytes-on-the-wire decrypt through the v3 STREAM path once the
/// grant is recorded on `OwnerState.received_file_grants`.
#[tokio::test]
async fn grantee_decrypts_multi_frame_file() {
    let keytree = test_keytree();
    let crdt_state = Arc::new(tokio::sync::Mutex::new(OwnerState::default()));
    let content_index = fresh_content_index();
    let (ingest_tx, store) = spawn_recording_store();

    // ~200 KiB: crosses several 64 KiB v3 frames (DEFAULT_FRAME_SIZE).
    let plaintext: Vec<u8> = (0..200_000u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
        .collect();
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
        "multi-frame.bin".to_string(),
    )
    .await
    .expect("encrypted streaming ingest succeeds");

    let root_bytes = harmony_app::parse_cid_hex(&result.cid).expect("cid hex");
    let cid = ContentId::from_bytes(root_bytes);
    assert!(cid.flags().encrypted, "root CID carries the encrypted flag");

    let sealed_dek = {
        let st = crdt_state.lock().await;
        st.file_deks
            .get(&root_bytes)
            .cloned()
            .expect("sealed DEK stored under the root CID")
    };
    let ciphertext = reassemble_from_store(&store, &root_bytes);
    assert_ne!(
        ciphertext, plaintext,
        "sanity: fetched bytes are ciphertext pre-decrypt"
    );

    // Grantee state: file_deks EMPTY, DEK only in received_file_grants — the
    // grant delivery is modeled directly (the sealed-envelope wire mechanics
    // are covered by `grantee_ingest_then_decrypt`); both owner and grantee
    // share the same KeyTree material here, mirroring the device-agnostic
    // re-seal contract exercised there.
    let mut grantee_state = OwnerState::default();
    grantee_state.received_file_grants.insert(
        root_bytes,
        ReceivedFileGrant {
            granter_owner: OwnerAddr([0x33u8; 16]),
            cid: root_bytes,
            file_name: "multi-frame.bin".to_string(),
            file_size: ciphertext.len() as u64,
            mime: "application/octet-stream".to_string(),
            sealed_dek,
            received_at: 0,
        },
    );
    assert!(
        grantee_state.file_deks.is_empty(),
        "sanity: grantee owns no file_deks entry"
    );

    let recovered =
        harmony_app::decrypt_personal_file_if_held(ciphertext, cid, &grantee_state, &keytree)
            .expect("grantee decrypts the multi-frame v3 file");
    assert_eq!(
        recovered, plaintext,
        "grantee must recover the original plaintext through the v3 path"
    );
}
