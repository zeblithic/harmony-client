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
