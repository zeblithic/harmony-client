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

use crate::dm_signing::{seal_to_owner_with_info, DmSignError};
use crate::owner_state_crdt::OwnerState;
use crate::owner_state_crypto::sealed::CanonicalPayloadSealed;
use crate::owner_state_crypto::{
    canonical_cbor_encode, decrypt_file_dek, encrypt_file_dek, CanonicalPayload, CryptoError,
    KeyTree,
};
use crate::owner_state_types::{EpochKey, OwnerAddr};
use serde::{Deserialize, Serialize};

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

/// HKDF domain-separation `info` for per-device grant seals. A FRESH string
/// — deliberately distinct from every DM / epoch-key / DEK-at-rest info so a
/// ciphertext sealed as a file grant can never be opened in another context
/// (and vice-versa). Bump the version suffix on any wire-format change.
pub const FILE_GRANT_SEAL_INFO: &[u8] = b"harmony-file-grant-v1";

/// The plaintext sealed to each grantee device: everything a grantee needs to
/// fetch and decrypt the shared file — the encrypted root CID, its display
/// metadata, and the per-file DEK. Sealed per grantee-device via
/// [`seal_grant_for_devices`]; the grantee unwraps it with their device X25519
/// private key and parses this struct back out.
///
/// 2-char field keys (codebase convention; satisfies `canonical_cbor_encode`'s
/// same-length-keys precondition — every field name encodes to 2 bytes). Both
/// byte-array fields ride as definite-length CBOR arrays (mirrors
/// `DepositFrame`'s fixed `[u8; 16]` owner fields), which is deterministic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileGrantInner {
    /// The shared file's encrypted root ContentId, canonical 32-byte form.
    #[serde(rename = "ci")]
    pub cid: [u8; 32],
    /// Display file name (for the grantee's received-files UI).
    #[serde(rename = "fn")]
    pub file_name: String,
    /// Plaintext byte length (pre-encryption).
    #[serde(rename = "fs")]
    pub file_size: u64,
    /// MIME type string.
    #[serde(rename = "mi")]
    pub mime: String,
    /// The per-file Data Encryption Key (raw 32 bytes). Confidential — only
    /// ever travels sealed inside this struct's per-device envelope.
    #[serde(rename = "dk")]
    pub dek: [u8; 32],
}

impl CanonicalPayloadSealed for FileGrantInner {}
impl CanonicalPayload for FileGrantInner {}

/// Fan-out sealing failed. Either the `FileGrantInner` could not be
/// canonically CBOR-encoded, or a per-device seal was rejected (e.g. a
/// low-order recipient X25519 pubkey).
#[derive(Debug, thiserror::Error)]
pub enum FileGrantSealError {
    #[error("canonical CBOR encode of FileGrantInner failed: {0}")]
    Encode(#[from] CryptoError),
    #[error("sealing grant to a device failed: {0}")]
    Seal(#[from] DmSignError),
}

/// Seal a `FileGrantInner` for a grantee's known devices — one sealed blob per
/// device, in the SAME order as `devices`. Each blob is
/// `seal_to_owner_with_info(dev, canonical_cbor(inner), FILE_GRANT_SEAL_INFO)`,
/// so it opens only with that device's X25519 private key and only under the
/// file-grant `info` (a foreign device / wrong domain fails with an AEAD tag
/// mismatch). The `inner` is CBOR-encoded ONCE and sealed N times.
pub fn seal_grant_for_devices(
    inner: &FileGrantInner,
    devices: &[[u8; 32]],
) -> Result<Vec<Vec<u8>>, FileGrantSealError> {
    let plaintext = canonical_cbor_encode(inner)?;
    devices
        .iter()
        .map(|dev| {
            seal_to_owner_with_info(dev, &plaintext, FILE_GRANT_SEAL_INFO)
                .map_err(FileGrantSealError::from)
        })
        .collect()
}

/// Resolve a grantee owner's known device X25519 public keys from the local
/// `owner_device_cache`. For each cached device with a propagated identity pub
/// (`Some([u8; 64])` = `X25519_pub(32) ‖ Ed25519_pub(32)`), take the X25519
/// half (`p[0..32]`). Devices known-only-by-hash (`None`) are skipped — we
/// cannot seal to a device whose X25519 key we don't hold. An unknown grantee
/// (no cache entry) yields an empty vec.
pub fn grantee_device_x25519s(state: &OwnerState, grantee_owner: OwnerAddr) -> Vec<[u8; 32]> {
    let Some(entry) = state.owner_device_cache.devices.get(&grantee_owner) else {
        return Vec::new();
    };
    entry
        .device_identity_pubs
        .iter()
        .filter_map(|maybe_pub| {
            maybe_pub.as_ref().map(|p| {
                let mut x = [0u8; 32];
                x.copy_from_slice(&p[0..32]);
                x
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state_crypto::canonical_cbor_decode;

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

    #[test]
    fn file_grant_inner_cbor_round_trip() {
        let inner = FileGrantInner {
            cid: [0x01; 32],
            file_name: "notes.md".into(),
            file_size: 12_345,
            mime: "text/markdown".into(),
            dek: [0x02; 32],
        };
        let bytes = canonical_cbor_encode(&inner).expect("encode");
        let back: FileGrantInner = canonical_cbor_decode(&bytes).expect("decode");
        assert_eq!(
            back, inner,
            "FileGrantInner round-trips through canonical CBOR"
        );
    }

    #[test]
    fn grantee_device_x25519s_extracts_first32_and_skips_none() {
        use crate::owner_state_types::{Hlc, OwnerAddr, OwnerDeviceEntry};

        // Two devices carry a propagated identity pub (X25519 ‖ Ed25519);
        // the middle device is known-by-hash only (None). Only the two
        // X25519 halves come back, in order, `None` skipped.
        let mut pub_a = [0u8; 64];
        pub_a[..32].copy_from_slice(&[0xA1; 32]);
        let mut pub_c = [0u8; 64];
        pub_c[..32].copy_from_slice(&[0xC3; 32]);

        let grantee = OwnerAddr([7u8; 16]);
        let mut state = OwnerState::default();
        state.owner_device_cache.devices.insert(
            grantee,
            OwnerDeviceEntry {
                // `grantee_device_x25519s` reads only `device_identity_pubs`.
                devices: vec![],
                device_identity_pubs: vec![Some(pub_a), None, Some(pub_c)],
                learned_at: Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "t".into(),
                },
                device_tunnel_contacts: vec![],
            },
        );

        let xs = grantee_device_x25519s(&state, grantee);
        assert_eq!(xs, vec![[0xA1u8; 32], [0xC3u8; 32]]);

        // Unknown grantee → empty (no cache entry).
        assert!(grantee_device_x25519s(&state, OwnerAddr([9u8; 16])).is_empty());
    }
}
