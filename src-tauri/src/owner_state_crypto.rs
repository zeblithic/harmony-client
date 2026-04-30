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
}
