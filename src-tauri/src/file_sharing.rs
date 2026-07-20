//! ZEB-674 Task 1 (C1): per-file Data Encryption Key (DEK) generation and
//! at-rest sealing for encrypted personal-file sharing.
//!
//! The encryption foundation for the feature: an ingested personal file is
//! encrypted whole-blob under a FRESH per-file DEK, stored as an
//! `EncryptedDurable` CID, and the DEK is persisted sealed-at-rest on
//! `OwnerState.file_deks` (keyed by the ingest root CID). This module owns the
//! DEK lifecycle; the encrypt-on-ingest wiring lives in
//! `lib::ingest_content_encrypted_inner`.
//!
//! - The DEK reuses [`EpochKey`] (a 32-byte `ZeroizeOnDrop` newtype) so it
//!   plugs directly into `community_state_sync::{encrypt_blob, decrypt_blob}`.
//! - Sealing reuses the `FriendEntry` sealed-secret idiom
//!   (`owner_state_crypto::{encrypt_file_dek, decrypt_file_dek}`), which seals
//!   the DEK under the owner's `KeyTree` — shared across the owner's bound
//!   devices, so `file_deks` replicates and unseals on any of them (Flow A).

use crate::owner_state_crypto::{decrypt_file_dek, encrypt_file_dek, CryptoError, KeyTree};
use crate::owner_state_types::EpochKey;

/// Generate a fresh per-file DEK (32 random bytes from OS entropy). Mirrors
/// how a fresh `EpochKey` is minted for a new community (`EpochKey::random`).
pub fn generate_file_dek() -> EpochKey {
    EpochKey::random()
}

/// Seal a per-file DEK for at-rest storage in `OwnerState.file_deks`. The
/// returned bytes are the KeyTree-sealed blob — NEVER the raw DEK. Fallible:
/// the underlying primitive draws a random nonce from the OS RNG.
pub fn seal_dek_at_rest(tree: &KeyTree, dek: &EpochKey) -> Result<Vec<u8>, CryptoError> {
    encrypt_file_dek(tree, dek.as_bytes())
}

/// Open a blob produced by [`seal_dek_at_rest`], recovering the DEK. Returns
/// `CryptoError::AeadDecrypt` on a wrong KeyTree / AAD mismatch / corruption.
pub fn open_dek_at_rest(tree: &KeyTree, sealed: &[u8]) -> Result<EpochKey, CryptoError> {
    let bytes = decrypt_file_dek(tree, sealed)?;
    Ok(EpochKey::new(*bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_tree() -> KeyTree {
        KeyTree::derive(&[0x11u8; 32]).expect("keytree")
    }

    #[test]
    fn dek_seal_open_round_trip() {
        let tree = test_tree();
        let dek = generate_file_dek();
        let sealed = seal_dek_at_rest(&tree, &dek).expect("seal");
        let opened = open_dek_at_rest(&tree, &sealed).expect("open");
        assert_eq!(opened.as_bytes(), dek.as_bytes());
    }

    #[test]
    fn generate_file_dek_is_fresh_each_call() {
        // Astronomically unlikely to collide; guards against a constant DEK.
        assert_ne!(
            generate_file_dek().as_bytes(),
            generate_file_dek().as_bytes()
        );
    }

    #[test]
    fn sealed_blob_is_not_the_raw_dek() {
        let tree = test_tree();
        let dek = generate_file_dek();
        let sealed = seal_dek_at_rest(&tree, &dek).expect("seal");
        assert_ne!(
            sealed.as_slice(),
            dek.as_bytes().as_slice(),
            "the sealed-at-rest blob must not be the plaintext DEK"
        );
        // nonce(12) + ciphertext(32) + tag(16) = 60 bytes.
        assert_eq!(sealed.len(), 60);
    }

    #[test]
    fn open_with_wrong_tree_fails() {
        let dek = generate_file_dek();
        let sealed = seal_dek_at_rest(&test_tree(), &dek).expect("seal");
        let other = KeyTree::derive(&[0x22u8; 32]).expect("keytree");
        assert!(open_dek_at_rest(&other, &sealed).is_err());
    }
}
