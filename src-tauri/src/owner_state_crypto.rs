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
    #[error("OS RNG failed: {0}")]
    Rng(String),
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
///
/// Fields are private intentionally: callers should never need raw key
/// bytes. All consumers go through `space_lookup_key`, `encrypt_entry`,
/// `decrypt_entry`, `encrypt_root_publish`, and `decrypt_root_publish`,
/// which take `&KeyTree` and select the appropriate sub-key internally.
/// This shrinks the surface where key material can leak into logs,
/// debug formatting, or accidental copies — `Zeroizing` only protects
/// the memory at drop time, not against unintended use.
pub struct KeyTree {
    entry_aead: Zeroizing<[u8; 32]>,
    root_aead: Zeroizing<[u8; 32]>,
    lookup: Zeroizing<[u8; 32]>,
    nonce: Zeroizing<[u8; 32]>,
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

/// AAD for state-root-publish AEAD. Domain-separated from per-entry AAD
/// (which is the per-space lookup key). Note: AAD alone does not provide
/// keystream separation — that's why we ALSO use a separate AEAD key
/// (`root_aead` vs `entry_aead`). See ZEB-211 round-2 "Key separation".
const AAD_ROOT_PUBLISH: &[u8] = b"state-root-pointer";

/// Encrypt a state-root-publish payload for the Zenoh topic.
///
/// Layout: `nonce(12) || ChaCha20-Poly1305-ciphertext-with-tag`. Nonce is
/// fresh-random per publish (CSPRNG). Determinism is intentionally NOT
/// required here — root publishes are pub/sub events, not content-addressed.
///
/// `payload` is typically the canonical-CBOR encoding of `{root_cid, at}`,
/// but this function is bytes-in/bytes-out — Phase 2 owns the CBOR shape.
pub fn encrypt_root_publish(keys: &KeyTree, payload: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let mut nonce_bytes = [0u8; 12];
    OsRng
        .try_fill_bytes(&mut nonce_bytes)
        .map_err(|e| CryptoError::Rng(e.to_string()))?;

    let cipher = ChaCha20Poly1305::new_from_slice(keys.root_aead.as_ref())
        .expect("ChaCha20-Poly1305 accepts a 32-byte key");
    let ciphertext = cipher
        .encrypt(
            Nonce::from_slice(&nonce_bytes),
            chacha20poly1305::aead::Payload {
                msg: payload,
                aad: AAD_ROOT_PUBLISH,
            },
        )
        .map_err(|_| CryptoError::AeadEncrypt)?;

    let mut blob = Vec::with_capacity(12 + ciphertext.len());
    blob.extend_from_slice(&nonce_bytes);
    blob.extend_from_slice(&ciphertext);
    Ok(blob)
}

/// Decrypt a state-root-publish blob produced by `encrypt_root_publish`.
pub fn decrypt_root_publish(keys: &KeyTree, blob: &[u8]) -> Result<Vec<u8>, CryptoError> {
    if blob.len() < 12 + 16 {
        return Err(CryptoError::AeadDecrypt);
    }
    let (nonce_bytes, ciphertext) = blob.split_at(12);
    let cipher = ChaCha20Poly1305::new_from_slice(keys.root_aead.as_ref())
        .expect("ChaCha20-Poly1305 accepts a 32-byte key");
    cipher
        .decrypt(
            Nonce::from_slice(nonce_bytes),
            chacha20poly1305::aead::Payload {
                msg: ciphertext,
                aad: AAD_ROOT_PUBLISH,
            },
        )
        .map_err(|_| CryptoError::AeadDecrypt)
}

/// Per-publisher HLC tracker for state-root replay protection.
///
/// Per ZEB-211 round-5: "last accepted" is keyed by `at.device_id`,
/// not global per-owner — different bound devices' clocks can interleave
/// with arbitrary wall_ms ordering, so a global rule would falsely reject
/// legitimate publishes.
///
/// Receivers call `try_accept` after AEAD-decrypting a state-root publish
/// payload but BEFORE applying the new `root_cid`.
#[derive(Debug, Default)]
pub struct RootReplayTracker {
    last_accepted: HashMap<String, Hlc>,
}

impl RootReplayTracker {
    /// Returns `Ok(())` if `at` is strictly newer than the last accepted
    /// HLC from the same publisher (`at.device_id`), and updates the
    /// tracker's record. Returns `Err(CryptoError::ReplayRejected)` if not.
    pub fn try_accept(&mut self, at: &Hlc) -> Result<(), CryptoError> {
        if let Some(last) = self.last_accepted.get(&at.device_id) {
            if !at.is_strictly_newer_than(last) {
                return Err(CryptoError::ReplayRejected(at.device_id.clone()));
            }
        }
        self.last_accepted.insert(at.device_id.clone(), at.clone());
        Ok(())
    }
}

/// Hybrid Logical Clock. Mirrors the type defined in the ZEB-206 spec.
///
/// Phase 2 of ZEB-215 will move this to a shared types module; Phase 1
/// keeps it here so the crypto module is self-contained for testing.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Hlc {
    pub wall_ms: u64,
    pub logical: u32,
    pub device_id: String,
}

/// Deterministic CBOR encoder for the owner-state crypto surface.
///
/// **Contract (current scope — Phase 1):** produces byte-identical output
/// across instances of *this* crate version when the caller's serde tree
/// satisfies all of:
/// - Map types are `BTreeMap` (NOT `HashMap`) — `serde`'s `BTreeMap`
///   serializer iterates in `BTreeMap` order, which is `K::Ord`
/// - All map keys at every nesting level have the same encoded length
///   (e.g., all-ULID, all-UUID, all-`u64`); see "Why same-length keys"
/// - No `f32` / `f64` fields anywhere in the tree
/// - No CBOR tags (`#[serde(...)]` attributes that emit tag 6)
/// - All collections are definite-length (Rust standard collections give
///   this for free; only custom `Serialize` impls can break it)
///
/// **Why same-length keys:** RFC 8949 §4.2.1 requires bytewise
/// lexicographic ordering of map keys *as encoded*, which prepends a
/// CBOR length byte. For text strings of equal length the CBOR length
/// byte is identical and bytewise-of-encoded matches Rust's `String` Ord
/// (bytewise of UTF-8). For text strings of *different* length the
/// length byte differs, so RFC 8949 sorts shorter-first while Rust's
/// `String` Ord sorts character-wise — and the two disagree.
/// As long as every map at a given nesting level uses keys with the
/// same encoded length, ciborium + BTreeMap produces output that is
/// (a) deterministic across bound devices running this code, and
/// (b) coincidentally compliant with RFC 8949 §4.2.1 for that level.
///
/// **What we do NOT yet enforce:**
/// - Strict RFC 8949 §4.2.1 length-first key ordering for mixed-length
///   keys at the encoder level (ciborium 0.2 has no canonical writer)
/// - Type-level prevention of `HashMap` / `f64` / etc. — currently
///   relies on the per-call review checklist above
///
/// Both belong to the followup that locks down canonical CBOR once
/// Phase 2 fixes the type universe (the actual `Space`, `OutboxEntry`
/// CBOR shapes). See ZEB-220 (filed as part of PR #72 review).
pub fn canonical_cbor_encode<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, CryptoError> {
    let mut buf = Vec::new();
    ciborium::into_writer(value, &mut buf).map_err(|e| CryptoError::CborEncode(format!("{e}")))?;
    Ok(buf)
}

/// Decoder paired with `canonical_cbor_encode`. Round-tripping is
/// symmetric for any value that encoded successfully under the
/// encoder's contract above.
///
/// Rejects inputs with trailing bytes after the first valid CBOR value.
/// Without this check an attacker could append arbitrary bytes to a
/// known-good blob and produce a different byte sequence that decodes
/// to the same value, defeating any downstream code that fingerprints
/// (BLAKE3-hashes, signs, dedup-checks) the encoded form.
pub fn canonical_cbor_decode<T: serde::de::DeserializeOwned>(
    bytes: &[u8],
) -> Result<T, CryptoError> {
    let mut cursor = std::io::Cursor::new(bytes);
    let value =
        ciborium::from_reader(&mut cursor).map_err(|e| CryptoError::CborDecode(format!("{e}")))?;
    if cursor.position() as usize != bytes.len() {
        return Err(CryptoError::CborDecode(format!(
            "trailing bytes after canonical value: consumed {} of {}",
            cursor.position(),
            bytes.len()
        )));
    }
    Ok(value)
}

impl Hlc {
    /// Lexicographic ordering on `(wall_ms, logical, device_id)`.
    ///
    /// Per ZEB-211 round-5: integers compared numerically; `device_id`
    /// compared bytewise (the `String` Ord impl provides this for UTF-8).
    /// Replay-protection check uses `self.is_strictly_newer_than(&last_accepted)`.
    pub fn is_strictly_newer_than(&self, other: &Hlc) -> bool {
        (self.wall_ms, self.logical, self.device_id.as_str())
            > (other.wall_ms, other.logical, other.device_id.as_str())
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

    #[test]
    fn encrypt_root_publish_round_trip() {
        let kt = KeyTree::derive(&TEST_SEED).expect("derive");
        let payload = b"any cbor-encoded plaintext".to_vec();

        let blob = encrypt_root_publish(&kt, &payload).expect("encrypt");
        let recovered = decrypt_root_publish(&kt, &blob).expect("decrypt");

        assert_eq!(recovered, payload);
    }

    #[test]
    fn encrypt_root_publish_uses_random_nonces() {
        // Random nonces — two encryptions of the same plaintext must
        // produce different blobs. (Different from per-entry deterministic.)
        let kt = KeyTree::derive(&TEST_SEED).expect("derive");
        let payload = b"identical".to_vec();

        let blob1 = encrypt_root_publish(&kt, &payload).expect("e1");
        let blob2 = encrypt_root_publish(&kt, &payload).expect("e2");
        assert_ne!(blob1, blob2);
    }

    #[test]
    fn decrypt_root_publish_rejects_wrong_key() {
        // A blob encrypted with one owner's key must not decrypt with
        // another owner's key.
        let kt_a = KeyTree::derive(&[0u8; 32]).expect("derive a");
        let kt_b = KeyTree::derive(&[1u8; 32]).expect("derive b");
        let payload = b"private".to_vec();

        let blob = encrypt_root_publish(&kt_a, &payload).expect("encrypt-a");
        let result = decrypt_root_publish(&kt_b, &blob);
        assert!(matches!(result, Err(CryptoError::AeadDecrypt)));
    }

    #[test]
    fn decrypt_root_publish_rejects_per_entry_blob() {
        // Domain separation: a blob encrypted as a per-entry value
        // (different AAD) must not decrypt as a root-publish payload.
        // This protects the key-separation invariant.
        let kt = KeyTree::derive(&TEST_SEED).expect("derive");
        let lookup = space_lookup_key(&kt, b"some-space");
        let payload = b"not a root payload".to_vec();
        let entry_blob = encrypt_entry(&kt, &lookup, &payload).expect("encrypt-entry");
        let result = decrypt_root_publish(&kt, &entry_blob);
        assert!(matches!(result, Err(CryptoError::AeadDecrypt)));
    }

    #[test]
    fn hlc_lexicographic_ordering_per_zeb_211() {
        let a = Hlc {
            wall_ms: 100,
            logical: 0,
            device_id: "alice".into(),
        };
        let b = Hlc {
            wall_ms: 100,
            logical: 0,
            device_id: "alice".into(),
        };
        assert!(!a.is_strictly_newer_than(&b));
        assert!(!b.is_strictly_newer_than(&a));

        // wall_ms dominates.
        let later_wall = Hlc {
            wall_ms: 101,
            logical: 0,
            device_id: "alice".into(),
        };
        assert!(later_wall.is_strictly_newer_than(&a));
        assert!(!a.is_strictly_newer_than(&later_wall));

        // logical breaks wall_ms ties.
        let later_logical = Hlc {
            wall_ms: 100,
            logical: 1,
            device_id: "alice".into(),
        };
        assert!(later_logical.is_strictly_newer_than(&a));

        // device_id breaks (wall_ms, logical) ties — bytewise UTF-8.
        let later_device = Hlc {
            wall_ms: 100,
            logical: 0,
            device_id: "bob".into(),
        };
        assert!(later_device.is_strictly_newer_than(&a));

        // Within tie, smaller bytewise device_id is older.
        let earlier_device = Hlc {
            wall_ms: 100,
            logical: 0,
            device_id: "aardvark".into(),
        };
        assert!(a.is_strictly_newer_than(&earlier_device));
    }

    fn hlc(wall_ms: u64, logical: u32, device_id: &str) -> Hlc {
        Hlc {
            wall_ms,
            logical,
            device_id: device_id.into(),
        }
    }

    #[test]
    fn replay_tracker_accepts_first_publish_from_each_publisher() {
        let mut tracker = RootReplayTracker::default();
        assert!(tracker.try_accept(&hlc(100, 0, "alice")).is_ok());
        // Different publisher (different device_id) — separate counter,
        // also accepted on first publish.
        assert!(tracker.try_accept(&hlc(50, 0, "bob")).is_ok());
    }

    #[test]
    fn replay_tracker_accepts_strictly_newer_from_same_publisher() {
        let mut tracker = RootReplayTracker::default();
        tracker.try_accept(&hlc(100, 0, "alice")).expect("first");
        assert!(tracker.try_accept(&hlc(100, 1, "alice")).is_ok());
        assert!(tracker.try_accept(&hlc(101, 0, "alice")).is_ok());
    }

    #[test]
    fn replay_tracker_rejects_replayed_publish_from_same_publisher() {
        let mut tracker = RootReplayTracker::default();
        tracker.try_accept(&hlc(100, 0, "alice")).expect("first");
        // Replay of the same HLC: rejected (not strictly newer).
        let result = tracker.try_accept(&hlc(100, 0, "alice"));
        assert!(matches!(result, Err(CryptoError::ReplayRejected(d)) if d == "alice"));
        // Older HLC: also rejected.
        let result = tracker.try_accept(&hlc(99, 999, "alice"));
        assert!(matches!(result, Err(CryptoError::ReplayRejected(_))));
    }

    #[test]
    fn replay_tracker_independent_per_publisher() {
        // Alice and Bob's clocks may interleave arbitrarily. The tracker
        // checks each publisher independently — a bob publish at wall_ms=50
        // is fine even after an alice publish at wall_ms=200.
        let mut tracker = RootReplayTracker::default();
        tracker.try_accept(&hlc(200, 0, "alice")).expect("alice-1");
        // Bob's first publish at lower wall_ms must succeed (per-publisher).
        tracker.try_accept(&hlc(50, 0, "bob")).expect("bob-1");
        // Bob's second publish at still-lower wall_ms is rejected (not
        // strictly newer than bob's last-accepted at wall_ms=50).
        let result = tracker.try_accept(&hlc(40, 0, "bob"));
        assert!(matches!(result, Err(CryptoError::ReplayRejected(_))));
    }

    use std::collections::BTreeMap;

    #[test]
    fn canonical_cbor_byte_identical_for_same_value() {
        // The deterministic-encryption property of the CRDT relies on
        // byte-identical CBOR output across implementations and runs.
        let mut value: BTreeMap<String, u32> = BTreeMap::new();
        value.insert("foo".into(), 1);
        value.insert("bar".into(), 2);

        let bytes1 = canonical_cbor_encode(&value).expect("encode 1");
        let bytes2 = canonical_cbor_encode(&value).expect("encode 2");
        assert_eq!(bytes1, bytes2);
    }

    #[test]
    fn canonical_cbor_round_trip() {
        let value = Hlc {
            wall_ms: 12345,
            logical: 7,
            device_id: "alice".into(),
        };
        let bytes = canonical_cbor_encode(&value).expect("encode");
        let recovered: Hlc = canonical_cbor_decode(&bytes).expect("decode");
        assert_eq!(value, recovered);
    }

    #[test]
    fn canonical_cbor_decode_rejects_garbage() {
        let result = canonical_cbor_decode::<Hlc>(b"not cbor at all");
        assert!(matches!(result, Err(CryptoError::CborDecode(_))));
    }

    #[test]
    fn canonical_cbor_decode_rejects_trailing_bytes() {
        // A canonical decoder must reject inputs with extra bytes after the
        // first valid value — otherwise an attacker can append garbage to a
        // known-good blob and produce a different byte sequence that decodes
        // to the same value, breaking any downstream code that fingerprints
        // (BLAKE3-hashes, signs, dedup-checks) the encoded form.
        let value = Hlc {
            wall_ms: 7,
            logical: 0,
            device_id: "alice".into(),
        };
        let mut bytes = canonical_cbor_encode(&value).expect("encode");
        bytes.push(0x00);
        bytes.push(0xFF);
        let result = canonical_cbor_decode::<Hlc>(&bytes);
        assert!(matches!(result, Err(CryptoError::CborDecode(_))));
    }

    #[test]
    fn integration_full_round_trip_with_replay_protection() {
        // Simulate the end-to-end happy path that Phase 2 will rely on:
        // 1. Two bound devices derive identical KeyTrees from the same seed.
        // 2. Device A encrypts a Space entry; produces a deterministic
        //    storage_blob and cipher_cid.
        // 3. Device B (via DAG-sync) fetches the storage_blob and
        //    decrypts to the same cleartext.
        // 4. Device A also publishes a state-root-publish payload with
        //    its current HLC; Device B accepts via the replay tracker.
        // 5. A captured-and-replayed copy of the same publish is rejected.

        let master_seed = [42u8; 32];
        let kt_a = KeyTree::derive(&master_seed).expect("device a");
        let kt_b = KeyTree::derive(&master_seed).expect("device b");

        // Encrypt a Space entry on device A.
        let space_id = b"some-channel-id";
        let lookup = space_lookup_key(&kt_a, space_id);
        let cleartext = b"channel #general at update_at=t1".to_vec();
        let blob = encrypt_entry(&kt_a, &lookup, &cleartext).expect("encrypt-a");
        let cipher_cid = blake3::hash(&blob);

        // Device B derives the same lookup key independently and decrypts.
        let lookup_b = space_lookup_key(&kt_b, space_id);
        assert_eq!(lookup, lookup_b);
        let recovered = decrypt_entry(&kt_b, &lookup_b, &blob).expect("decrypt-b");
        assert_eq!(recovered, cleartext);

        // Device A publishes a state-root pointer.
        let publish_at = hlc(1000, 0, "device-a");
        let publish_payload =
            canonical_cbor_encode(&(cipher_cid.as_bytes().to_vec(), publish_at.clone()))
                .expect("encode publish");
        let publish_blob = encrypt_root_publish(&kt_a, &publish_payload).expect("encrypt-publish");

        // Device B's replay tracker accepts the first publish from device-a.
        let mut tracker_b = RootReplayTracker::default();
        let recovered_publish =
            decrypt_root_publish(&kt_b, &publish_blob).expect("decrypt-publish");
        let (_recovered_cid, recovered_at): (Vec<u8>, Hlc) =
            canonical_cbor_decode(&recovered_publish).expect("decode publish");
        tracker_b.try_accept(&recovered_at).expect("accept first");

        // Adversary captures and replays the SAME blob — even though AEAD
        // verifies (it's a valid blob), the replay tracker rejects it.
        let recovered_replay = decrypt_root_publish(&kt_b, &publish_blob).expect("decrypt-replay");
        let (_, recovered_at_2): (Vec<u8>, Hlc) =
            canonical_cbor_decode(&recovered_replay).expect("decode replay");
        let result = tracker_b.try_accept(&recovered_at_2);
        assert!(matches!(result, Err(CryptoError::ReplayRejected(d)) if d == "device-a"));
    }
}
