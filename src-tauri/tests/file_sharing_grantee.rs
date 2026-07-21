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
use harmony_app::file_sharing::{
    ingest_grant_push, open_received_file, seal_grant_for_devices, FileGrantInner,
};
use harmony_app::owner_state_crdt::OwnerState;
use harmony_app::owner_state_types::{EpochKey, OwnerAddr};
use harmony_content::cid::ContentId;

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
    let mut state = OwnerState::default();

    // Ingest → Some(cid), and the record lands with the matched sealed blob.
    let ingested = ingest_grant_push(&mut state, &grantee_priv, granter, &grant_push)
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
    assert_eq!(
        rec.sealed_dek, sealed[0],
        "stores the MATCHED sealed blob verbatim"
    );
    assert_ne!(
        rec.sealed_dek.as_slice(),
        dek_bytes.as_slice(),
        "the stored blob must be the sealed envelope, never the raw DEK"
    );

    // Open → the DEK, and it actually decrypts the file back to plaintext.
    let recovered = open_received_file(&state, &grantee_priv, ContentId::from_bytes(cid_bytes))
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
    let out = ingest_grant_push(&mut state, &us_priv, OwnerAddr([0x22u8; 16]), &grant_push)
        .expect("ingest ok");
    assert_eq!(out, None, "no blob opens with our key → Ok(None)");
    assert!(
        state.received_file_grants.is_empty(),
        "no state change when nothing opened"
    );
}
