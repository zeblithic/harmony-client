//! Unit tests for AEAD helpers in community_state_sync.rs.

use harmony_app::community_state_sync::{
    decrypt_blob, decrypt_root_publish, encrypt_blob, encrypt_root_publish, CommunityCryptoError,
};
use harmony_app::owner_state_types::EpochKey;

#[test]
fn encrypt_root_publish_round_trips() {
    let mk = EpochKey::new([0x42; 32]);
    let plaintext = b"hello-community-root-publish".to_vec();

    let wire = encrypt_root_publish(&mk, &plaintext).expect("encrypt");
    assert_ne!(wire, plaintext, "ciphertext must differ from plaintext");

    let recovered = decrypt_root_publish(&mk, &wire).expect("decrypt");
    assert_eq!(recovered, plaintext);
}

#[test]
fn encrypt_root_publish_rejects_wrong_key() {
    let mk_a = EpochKey::new([0x01; 32]);
    let mk_b = EpochKey::new([0x02; 32]);
    let plaintext = b"secret".to_vec();
    let wire = encrypt_root_publish(&mk_a, &plaintext).expect("encrypt");
    let err = decrypt_root_publish(&mk_b, &wire).unwrap_err();
    assert!(matches!(err, CommunityCryptoError::AeadFailed));
}

#[test]
fn encrypt_blob_is_deterministic_for_same_key_and_plaintext() {
    // Deterministic: encrypt_blob uses a fixed-derivation nonce so
    // the same (key, plaintext) produces the same ciphertext —
    // letting the ContentStore content-address it identically across
    // replicas. encrypt_root_publish uses a random nonce by contrast
    // (each publish is a distinct wire packet and we want freshness).
    let mk = EpochKey::new([0xaa; 32]);
    let plaintext = b"deterministic-blob".to_vec();
    let a = encrypt_blob(&mk, &plaintext).expect("encrypt a");
    let b = encrypt_blob(&mk, &plaintext).expect("encrypt b");
    assert_eq!(
        a, b,
        "blob encryption must be deterministic for content addressing"
    );
}

#[test]
fn encrypt_blob_round_trips() {
    let mk = EpochKey::new([0xbb; 32]);
    let plaintext = b"event-log-cbor-bytes-go-here".to_vec();
    let ct = encrypt_blob(&mk, &plaintext).expect("encrypt");
    let recovered = decrypt_blob(&mk, &ct).expect("decrypt");
    assert_eq!(recovered, plaintext);
}

#[test]
fn decrypt_blob_rejects_wrong_key() {
    // encrypt_blob has no AAD, so the only thing rejecting a wrong-key
    // decrypt is the Poly1305 tag — pinning that here.
    let mk_a = EpochKey::new([0x11; 32]);
    let mk_b = EpochKey::new([0x22; 32]);
    let plaintext = b"blob-secret".to_vec();
    let wire = encrypt_blob(&mk_a, &plaintext).expect("encrypt");
    let err = decrypt_blob(&mk_b, &wire).unwrap_err();
    assert!(matches!(err, CommunityCryptoError::AeadFailed));
}

#[test]
fn decrypt_root_publish_rejects_truncated_wire() {
    // Wire shorter than NONCE_LEN + TAG_LEN must be rejected before
    // any slicing — guards the Truncated error variant.
    let mk = EpochKey::new([0x33; 32]);
    let too_short = vec![0u8; 27]; // 1 byte short of 12 + 16
    let err = decrypt_root_publish(&mk, &too_short).unwrap_err();
    assert!(matches!(err, CommunityCryptoError::Truncated));
}

#[test]
fn decrypt_blob_rejects_truncated_wire() {
    let mk = EpochKey::new([0x44; 32]);
    let too_short = vec![0u8; 27];
    let err = decrypt_blob(&mk, &too_short).unwrap_err();
    assert!(matches!(err, CommunityCryptoError::Truncated));
}

/// CR Major (PR #106 R6): verify the root→blob epoch-key binding contract.
///
/// A packet where the root wire is encrypted under K(0) and the blob is
/// encrypted under K(1) MUST be rejected: if the receiver successfully
/// decrypts the root with K(0) and then tries to decrypt the blob with
/// K(0), the blob decrypt fails (the Poly1305 tag is wrong).
///
/// `handle_incoming_publish` now captures `root_key_used` and passes it
/// directly to `decrypt_blob`, so a blob encrypted under any other key
/// is rejected regardless of whether that other key is also "known".
#[test]
fn handle_incoming_publish_rejects_mismatched_root_blob_keys() {
    // K(0) and K(1) are both "known" (would both appear in old_epoch_keys).
    let k0 = EpochKey::new([0x10; 32]);
    let k1 = EpochKey::new([0x11; 32]);

    let root_plaintext = b"root-payload-bytes".to_vec();
    let blob_plaintext = b"blob-payload-bytes".to_vec();

    // Construct a wire packet:
    //   - root encrypted under K(0)
    //   - blob encrypted under K(1)  ← mismatched
    let root_wire = encrypt_root_publish(&k0, &root_plaintext).expect("encrypt root with k0");
    let blob_ct_k1 = encrypt_blob(&k1, &blob_plaintext).expect("encrypt blob with k1");

    // Root decrypts successfully with K(0).
    let root_decrypted = decrypt_root_publish(&k0, &root_wire).expect("root must decrypt with k0");
    assert_eq!(root_decrypted, root_plaintext);

    // Blob decrypt with the root_key_used (K(0)) MUST fail — the blob was
    // encrypted under K(1). This is the binding contract: after
    // handle_incoming_publish captures `root_key_used = &k0`, it calls
    // `decrypt_blob(root_key_used, &blob_ciphertext)` and gets AeadFailed.
    let err = decrypt_blob(&k0, &blob_ct_k1).unwrap_err();
    assert!(
        matches!(err, CommunityCryptoError::AeadFailed),
        "blob encrypted under K(1) must be rejected when decrypted with K(0): {err:?}"
    );
}
