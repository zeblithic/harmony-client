//! Owner-state encryption primitives per ZEB-211.
//!
//! See `docs/specs/2026-04-30-zeb-211-owner-state-encryption-design.md`.
//!
//! This module is pure crypto — no I/O, no Space/OutboxEntry knowledge.
//! Phase 2 of ZEB-215 wires it into the CRDT layer.

use chacha20poly1305::{aead::Aead, ChaCha20Poly1305, KeyInit, Nonce};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use rand::rngs::OsRng;
use rand::RngCore;
use sha2::Sha256;
use std::collections::HashMap;
use zeroize::Zeroizing;

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("HKDF expand failed: {0}")]
    Hkdf(String),
    #[error("AEAD encryption failed")]
    AeadEncrypt,
    #[error("AEAD decryption failed (forged or wrong key)")]
    AeadDecrypt,
    #[error("CBOR encode failed: {0}")]
    CborEncode(String),
    #[error("CBOR decode failed: {0}")]
    CborDecode(String),
    #[error("Replay rejected: at HLC not strictly newer than last accepted from device {0}")]
    ReplayRejected(String),
}

/// Salt versioning: bump `v1` if the encryption scheme itself changes;
/// bump `epoch-N` to rotate keys (after the future "wipe master from
/// device" action lands per ZEB-197 follow-on). v1 hard-codes epoch-0.
const HKDF_SALT: &[u8] = b"harmony-owner-state-v1-epoch-0";

const INFO_ENTRY_AEAD: &[u8] = b"entry-aead-key";
const INFO_ROOT_AEAD: &[u8] = b"root-aead-key";
const INFO_TREE_LOOKUP: &[u8] = b"tree-lookup";
const INFO_NONCE_DERIV: &[u8] = b"nonce-deriv";

/// Four owner-state keys derived deterministically from the master seed.
/// Every bound device that holds the seed computes identical keys.
pub struct KeyTree {
    pub entry_aead: Zeroizing<[u8; 32]>,
    pub root_aead: Zeroizing<[u8; 32]>,
    pub lookup: Zeroizing<[u8; 32]>,
    pub nonce: Zeroizing<[u8; 32]>,
}

impl KeyTree {
    /// Derive all four keys via HKDF-SHA256 with domain separation.
    pub fn derive(master_seed: &[u8; 32]) -> Result<Self, CryptoError> {
        let hk = Hkdf::<Sha256>::new(Some(HKDF_SALT), master_seed);

        let mut entry_aead = Zeroizing::new([0u8; 32]);
        hk.expand(INFO_ENTRY_AEAD, entry_aead.as_mut())
            .map_err(|e| CryptoError::Hkdf(format!("entry-aead: {e}")))?;

        let mut root_aead = Zeroizing::new([0u8; 32]);
        hk.expand(INFO_ROOT_AEAD, root_aead.as_mut())
            .map_err(|e| CryptoError::Hkdf(format!("root-aead: {e}")))?;

        let mut lookup = Zeroizing::new([0u8; 32]);
        hk.expand(INFO_TREE_LOOKUP, lookup.as_mut())
            .map_err(|e| CryptoError::Hkdf(format!("tree-lookup: {e}")))?;

        let mut nonce = Zeroizing::new([0u8; 32]);
        hk.expand(INFO_NONCE_DERIV, nonce.as_mut())
            .map_err(|e| CryptoError::Hkdf(format!("nonce-deriv: {e}")))?;

        Ok(Self {
            entry_aead,
            root_aead,
            lookup,
            nonce,
        })
    }
}

/// Derive the per-space Prolly Tree lookup key.
///
/// The lookup key is `HMAC-SHA256(owner_state_lookup_key, space_id_bytes)`
/// — a keyed MAC, NOT a plain hash, so observers without the lookup key
/// cannot enumerate the tree by precomputing hashes of known space IDs.
///
/// Returns 32 bytes for use as a Prolly Tree key AND as AAD when
/// encrypting that space's value (defense-in-depth against ciphertext
/// relocation; see ZEB-211 spec).
pub fn space_lookup_key(keys: &KeyTree, space_id_bytes: &[u8]) -> [u8; 32] {
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = <HmacSha256 as Mac>::new_from_slice(keys.lookup.as_ref())
        .expect("HMAC accepts any key length");
    mac.update(space_id_bytes);
    mac.finalize().into_bytes().into()
}

/// Domain-separation prefix for per-entry nonce derivation. Versions the
/// construction; bump if the nonce scheme itself changes.
const NONCE_DOMAIN_ENTRY: &[u8] = b"owner-state-entry-v1";

/// Derive the deterministic 12-byte nonce for an entry write.
///
/// `nonce = BLAKE3-keyed-MAC(nonce_key,
///                            domain_prefix || space_lookup_key || cleartext)[..12]`
///
/// Mixing `space_lookup_key` into the input prevents cross-space nonce
/// collisions when two different spaces happen to have identical cleartext
/// (per ZEB-211 round-2 fix). Same (space, cleartext) → same nonce, which
/// is what the CRDT requires for stable cipher-CIDs.
fn entry_nonce(keys: &KeyTree, space_lookup_key: &[u8; 32], cleartext: &[u8]) -> [u8; 12] {
    // BLAKE3's new_keyed expects &[u8; 32], which we have in keys.nonce.
    let nonce_key: &[u8; 32] = keys.nonce.as_ref().try_into().expect("nonce is 32 bytes");
    let mut hasher = blake3::Hasher::new_keyed(nonce_key);
    hasher.update(NONCE_DOMAIN_ENTRY);
    hasher.update(space_lookup_key);
    hasher.update(cleartext);

    let mut output = [0u8; 12];
    let mut xof = hasher.finalize_xof();
    xof.fill(&mut output);
    output
}

/// Encrypt a Space entry's cleartext for CAS storage.
///
/// Returns `storage_blob = nonce(12) || ChaCha20-Poly1305-ciphertext-with-tag`.
/// The CID stored in harmony-content is `BLAKE3(storage_blob)`.
///
/// Deterministic: same `(keys, space_lookup_key, cleartext)` → same blob.
pub fn encrypt_entry(
    keys: &KeyTree,
    space_lookup_key: &[u8; 32],
    cleartext: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    let nonce_bytes = entry_nonce(keys, space_lookup_key, cleartext);
    let cipher = ChaCha20Poly1305::new_from_slice(keys.entry_aead.as_ref())
        .expect("ChaCha20-Poly1305 accepts a 32-byte key");

    // AAD binds the ciphertext to its tree position (defense-in-depth
    // against relocation attacks; see ZEB-211 "Why AAD = space_lookup_key").
    let ciphertext = cipher
        .encrypt(
            Nonce::from_slice(&nonce_bytes),
            chacha20poly1305::aead::Payload {
                msg: cleartext,
                aad: space_lookup_key,
            },
        )
        .map_err(|_| CryptoError::AeadEncrypt)?;

    let mut blob = Vec::with_capacity(12 + ciphertext.len());
    blob.extend_from_slice(&nonce_bytes);
    blob.extend_from_slice(&ciphertext);
    Ok(blob)
}

/// Decrypt a storage blob produced by `encrypt_entry`.
///
/// Returns the cleartext on success, or `CryptoError::AeadDecrypt` if
/// the AAD doesn't match (relocation attack) or the blob is corrupt.
pub fn decrypt_entry(
    keys: &KeyTree,
    space_lookup_key: &[u8; 32],
    blob: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    if blob.len() < 12 + 16 {
        return Err(CryptoError::AeadDecrypt);
    }
    let (nonce_bytes, ciphertext) = blob.split_at(12);
    let cipher = ChaCha20Poly1305::new_from_slice(keys.entry_aead.as_ref())
        .expect("ChaCha20-Poly1305 accepts a 32-byte key");
    cipher
        .decrypt(
            Nonce::from_slice(nonce_bytes),
            chacha20poly1305::aead::Payload {
                msg: ciphertext,
                aad: space_lookup_key,
            },
        )
        .map_err(|_| CryptoError::AeadDecrypt)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 32-byte all-zeros master seed for deterministic test fixtures.
    const TEST_SEED: [u8; 32] = [0u8; 32];

    #[test]
    fn key_tree_derives_four_distinct_keys_deterministically() {
        let kt1 = KeyTree::derive(&TEST_SEED).expect("derive 1");
        let kt2 = KeyTree::derive(&TEST_SEED).expect("derive 2");

        // Same seed → same keys (every bound device computes the same tree).
        assert_eq!(kt1.entry_aead.as_ref(), kt2.entry_aead.as_ref());
        assert_eq!(kt1.root_aead.as_ref(), kt2.root_aead.as_ref());
        assert_eq!(kt1.lookup.as_ref(), kt2.lookup.as_ref());
        assert_eq!(kt1.nonce.as_ref(), kt2.nonce.as_ref());

        // The four keys must be distinct (domain separation).
        assert_ne!(kt1.entry_aead.as_ref(), kt1.root_aead.as_ref());
        assert_ne!(kt1.entry_aead.as_ref(), kt1.lookup.as_ref());
        assert_ne!(kt1.entry_aead.as_ref(), kt1.nonce.as_ref());
        assert_ne!(kt1.root_aead.as_ref(), kt1.lookup.as_ref());
        assert_ne!(kt1.root_aead.as_ref(), kt1.nonce.as_ref());
        assert_ne!(kt1.lookup.as_ref(), kt1.nonce.as_ref());
    }

    #[test]
    fn key_tree_different_seeds_produce_different_keys() {
        let kt1 = KeyTree::derive(&[0u8; 32]).expect("derive 1");
        let kt2 = KeyTree::derive(&[1u8; 32]).expect("derive 2");
        assert_ne!(kt1.entry_aead.as_ref(), kt2.entry_aead.as_ref());
    }

    #[test]
    fn space_lookup_key_is_deterministic_and_distinguishes_spaces() {
        let kt = KeyTree::derive(&TEST_SEED).expect("derive");

        let key_a1 = space_lookup_key(&kt, b"space-id-A");
        let key_a2 = space_lookup_key(&kt, b"space-id-A");
        let key_b = space_lookup_key(&kt, b"space-id-B");

        // Same input → same lookup key (deterministic; bound devices agree).
        assert_eq!(key_a1, key_a2);

        // Different space IDs → different lookup keys.
        assert_ne!(key_a1, key_b);

        // Output is exactly 32 bytes (SHA-256 size).
        assert_eq!(key_a1.len(), 32);
    }

    #[test]
    fn space_lookup_key_unrelated_to_plain_blake3_hash() {
        // Sanity: lookup key must be HMAC, not a plain hash. A plain
        // BLAKE3(space_id) would let observers enumerate by precomputing
        // hashes of known space IDs.
        let kt = KeyTree::derive(&TEST_SEED).expect("derive");
        let lookup = space_lookup_key(&kt, b"some-space");
        let plain = blake3::hash(b"some-space");
        assert_ne!(&lookup[..], plain.as_bytes().as_slice());
    }

    #[test]
    fn encrypt_entry_round_trip() {
        let kt = KeyTree::derive(&TEST_SEED).expect("derive");
        let lookup = space_lookup_key(&kt, b"alice-dm");
        let cleartext = b"hello, world".to_vec();

        let blob = encrypt_entry(&kt, &lookup, &cleartext).expect("encrypt");
        let recovered = decrypt_entry(&kt, &lookup, &blob).expect("decrypt");

        assert_eq!(recovered, cleartext);
        // Storage blob layout: nonce(12) || ciphertext_with_tag(N+16)
        assert_eq!(blob.len(), 12 + cleartext.len() + 16);
    }

    #[test]
    fn encrypt_entry_is_deterministic_for_same_inputs() {
        // The CRDT relies on this: two bound devices encrypting the same
        // (space, cleartext) pair must produce identical ciphertext + CID.
        let kt = KeyTree::derive(&TEST_SEED).expect("derive");
        let lookup = space_lookup_key(&kt, b"alice-dm");
        let cleartext = b"identical bytes".to_vec();

        let blob1 = encrypt_entry(&kt, &lookup, &cleartext).expect("e1");
        let blob2 = encrypt_entry(&kt, &lookup, &cleartext).expect("e2");
        assert_eq!(blob1, blob2);
    }

    #[test]
    fn encrypt_entry_cross_space_nonce_binding_prevents_collision() {
        // The CRITICAL fix from PR #71 round 2: two different spaces with
        // identical cleartext MUST produce different nonces. Otherwise
        // ChaCha20-Poly1305 keystream is reused under the same key,
        // catastrophically breaking confidentiality + integrity.
        let kt = KeyTree::derive(&TEST_SEED).expect("derive");
        let lookup_a = space_lookup_key(&kt, b"space-A");
        let lookup_b = space_lookup_key(&kt, b"space-B");
        let cleartext = b"identical cleartext".to_vec();

        let blob_a = encrypt_entry(&kt, &lookup_a, &cleartext).expect("a");
        let blob_b = encrypt_entry(&kt, &lookup_b, &cleartext).expect("b");

        // First 12 bytes are the nonce.
        let nonce_a = &blob_a[..12];
        let nonce_b = &blob_b[..12];
        assert_ne!(
            nonce_a, nonce_b,
            "cross-space nonce collision: ZEB-211 fix regressed"
        );
    }

    #[test]
    fn decrypt_entry_rejects_aad_mismatch_relocation() {
        // AAD-binding prevents relocating Space-A's ciphertext into
        // Space-B's tree slot.
        let kt = KeyTree::derive(&TEST_SEED).expect("derive");
        let lookup_a = space_lookup_key(&kt, b"space-A");
        let lookup_b = space_lookup_key(&kt, b"space-B");
        let cleartext = b"some content".to_vec();

        let blob_a = encrypt_entry(&kt, &lookup_a, &cleartext).expect("encrypt-a");
        // Try decrypting blob-A with space-B's lookup key — must fail.
        let result = decrypt_entry(&kt, &lookup_b, &blob_a);
        assert!(matches!(result, Err(CryptoError::AeadDecrypt)));
    }

    #[test]
    fn decrypt_entry_rejects_truncated_blob() {
        let kt = KeyTree::derive(&TEST_SEED).expect("derive");
        let lookup = space_lookup_key(&kt, b"some-space");
        // Less than 12+16=28 bytes can never be a valid blob.
        let result = decrypt_entry(&kt, &lookup, &[0u8; 27]);
        assert!(matches!(result, Err(CryptoError::AeadDecrypt)));
    }
}
