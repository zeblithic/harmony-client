# ZEB-215 Sub-A Phase 1: Owner-state crypto primitives — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a self-contained, well-tested Rust module (`owner_state_crypto.rs`) implementing every cryptographic primitive specified in ZEB-211: HKDF-SHA256 key derivation tree (4 keys), HMAC-SHA256 tree-lookup keys, BLAKE3-keyed-MAC nonce derivation with space-binding, deterministic per-entry AEAD, random-nonce root-publish AEAD, replay-protection HLC tracker, and canonical CBOR helpers.

**Architecture:** A single Rust file `src-tauri/src/owner_state_crypto.rs` with inline `#[cfg(test)] mod tests`. The module exposes a small public API (`KeyTree`, `space_lookup_key()`, `encrypt_entry()` / `decrypt_entry()`, `encrypt_root_publish()` / `decrypt_root_publish()`, `RootReplayTracker`) used by Phase 2's CRDT layer. No I/O. No knowledge of Space/OutboxEntry types. Pure functions over bytes, plus the small per-publisher HLC state machine.

**Tech Stack:** Rust 2021. Existing Cargo.toml deps: `chacha20poly1305 = "0.10"`, `hkdf = "0.12"`, `sha2 = "0.10"`, `blake3 = "1"`, `ciborium = "0.2"`, `rand = "0.8"`, `zeroize = "1"`. New dep this plan adds: `hmac = "0.12"` (compatible with existing `sha2 = "0.10"`).

**Phase 1 of 5 for Sub-A.** Phase 2 (CRDT primitives) will consume this module. Phases 3 (Zenoh sync), 4 (IPC commands), 5 (frontend) ride on top. Each phase ships as its own PR.

**Source-of-truth specs (already merged):**

- `docs/specs/2026-04-30-zeb-206-nav-tree-design.md` — umbrella nav-tree design
- `docs/specs/2026-04-30-zeb-211-owner-state-encryption-design.md` — encryption design (Phase 1's reference)

---

## File Structure

| File | Responsibility |
|---|---|
| `src-tauri/src/owner_state_crypto.rs` | New. Pure crypto primitives + per-publisher replay tracker. ~600 lines including inline tests. |
| `src-tauri/src/lib.rs` | Modify. Add `pub mod owner_state_crypto;` declaration. |
| `src-tauri/Cargo.toml` | Modify. Add `hmac = "0.12"` to `[dependencies]`. |

No other files touched in Phase 1. The module is intentionally library-shaped — Phase 2 wires it into the CRDT layer.

---

## Type contract (cross-cutting reference for all tasks)

```rust
use chacha20poly1305::{aead::Aead, ChaCha20Poly1305, Key, KeyInit, Nonce};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use zeroize::Zeroizing;
use rand::rngs::OsRng;
use rand::RngCore;
use std::collections::HashMap;

/// All four owner-state keys derived from the master seed via HKDF-SHA256.
/// Each Zeroizing-wrapped so they clear on drop.
pub struct KeyTree {
    pub entry_aead: Zeroizing<[u8; 32]>,
    pub root_aead:  Zeroizing<[u8; 32]>,
    pub lookup:     Zeroizing<[u8; 32]>,
    pub nonce:      Zeroizing<[u8; 32]>,
}

/// Per-publisher HLC tracker for state-root replay protection.
/// Keyed by HLC.device_id.
#[derive(Debug, Default)]
pub struct RootReplayTracker {
    last_accepted: HashMap<String, Hlc>,
}

/// Hybrid Logical Clock — defined in ZEB-206 spec, mirrored here.
/// Phase 2 will move this to a shared types module; Phase 1 keeps it
/// inside the crypto module so the file is self-contained for testing.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Hlc {
    pub wall_ms: u64,
    pub logical: u32,
    pub device_id: String,
}

/// Errors returned by this module. AEAD failures, replay rejections,
/// CBOR parse failures all map to specific variants for caller handling.
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
```

`thiserror` may need to be added if not already a dep — Task 1 verifies. The standard Tauri scaffold likely already has it.

---

## Task 1: Add `hmac` dep and create the module skeleton

**Files:**
- Modify: `src-tauri/Cargo.toml` (add one line)
- Create: `src-tauri/src/owner_state_crypto.rs` (skeleton + module-level doc comment)
- Modify: `src-tauri/src/lib.rs` (add `pub mod owner_state_crypto;`)

- [ ] **Step 1: Verify `thiserror` is already a dependency**

Run: `grep -E "^thiserror" src-tauri/Cargo.toml`
Expected: a line like `thiserror = "1"` or `thiserror = "2"`. If missing, add `thiserror = "1"` to `[dependencies]` in this same task.

- [ ] **Step 2: Add `hmac = "0.12"` to Cargo.toml**

Edit `src-tauri/Cargo.toml`. Locate the existing alphabetical block of `[dependencies]` containing `hkdf = "0.12"` and `sha2 = "0.10"`. Insert the line:

```toml
hmac = "0.12"
```

so the block contains both `hmac` and `hkdf` in alphabetical order.

- [ ] **Step 3: Run `cargo check` to confirm `hmac` resolves**

Run: `cargo check --manifest-path src-tauri/Cargo.toml 2>&1 | tail -10`
Expected: clean check (no errors). Cargo may pull `hmac` from the registry on first run; that's normal.

- [ ] **Step 4: Create the `owner_state_crypto.rs` skeleton**

Write `src-tauri/src/owner_state_crypto.rs`:

```rust
//! Owner-state encryption primitives per ZEB-211.
//!
//! See `docs/specs/2026-04-30-zeb-211-owner-state-encryption-design.md`.
//!
//! This module is pure crypto — no I/O, no Space/OutboxEntry knowledge.
//! Phase 2 of ZEB-215 wires it into the CRDT layer.

use chacha20poly1305::{aead::Aead, ChaCha20Poly1305, KeyInit, Nonce};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use zeroize::Zeroizing;
use rand::rngs::OsRng;
use rand::RngCore;
use std::collections::HashMap;

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

#[cfg(test)]
mod tests {
    // Tests added in subsequent tasks.
}
```

- [ ] **Step 5: Add module declaration to `lib.rs`**

Locate the `pub mod` lines near the top of `src-tauri/src/lib.rs` (alongside `pub mod identity;`, `pub mod owner_state;`, etc.) and insert (alphabetically):

```rust
pub mod owner_state_crypto;
```

- [ ] **Step 6: Verify build**

Run: `cargo build --manifest-path src-tauri/Cargo.toml 2>&1 | tail -10`
Expected: clean build (or warnings only — no errors). The unused-imports warnings on the new module are fine; subsequent tasks remove them.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/Cargo.toml src-tauri/src/owner_state_crypto.rs src-tauri/src/lib.rs
git commit -m "feat(zeb-215-sub-a): add owner_state_crypto module skeleton + hmac dep"
```

---

## Task 2: HKDF-SHA256 key derivation tree

**Files:**
- Modify: `src-tauri/src/owner_state_crypto.rs` (add `KeyTree` struct + `KeyTree::derive()` + tests)

- [ ] **Step 1: Write the failing test**

Append to the `mod tests` block in `src-tauri/src/owner_state_crypto.rs`:

```rust
    use super::*;

    /// 32-byte all-zeros master seed for deterministic test fixtures.
    const TEST_SEED: [u8; 32] = [0u8; 32];

    #[test]
    fn key_tree_derives_four_distinct_keys_deterministically() {
        let kt1 = KeyTree::derive(&TEST_SEED).expect("derive 1");
        let kt2 = KeyTree::derive(&TEST_SEED).expect("derive 2");

        // Same seed → same keys (every bound device computes the same tree).
        assert_eq!(kt1.entry_aead.as_ref(), kt2.entry_aead.as_ref());
        assert_eq!(kt1.root_aead.as_ref(),  kt2.root_aead.as_ref());
        assert_eq!(kt1.lookup.as_ref(),     kt2.lookup.as_ref());
        assert_eq!(kt1.nonce.as_ref(),      kt2.nonce.as_ref());

        // The four keys must be distinct (domain separation).
        assert_ne!(kt1.entry_aead.as_ref(), kt1.root_aead.as_ref());
        assert_ne!(kt1.entry_aead.as_ref(), kt1.lookup.as_ref());
        assert_ne!(kt1.entry_aead.as_ref(), kt1.nonce.as_ref());
        assert_ne!(kt1.root_aead.as_ref(),  kt1.lookup.as_ref());
        assert_ne!(kt1.root_aead.as_ref(),  kt1.nonce.as_ref());
        assert_ne!(kt1.lookup.as_ref(),     kt1.nonce.as_ref());
    }

    #[test]
    fn key_tree_different_seeds_produce_different_keys() {
        let kt1 = KeyTree::derive(&[0u8; 32]).expect("derive 1");
        let kt2 = KeyTree::derive(&[1u8; 32]).expect("derive 2");
        assert_ne!(kt1.entry_aead.as_ref(), kt2.entry_aead.as_ref());
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path src-tauri/Cargo.toml -p harmony-app owner_state_crypto::tests::key_tree -- --nocapture 2>&1 | tail -20`
Expected: compile error — `KeyTree` not found.

- [ ] **Step 3: Implement `KeyTree` and `derive()`**

In `src-tauri/src/owner_state_crypto.rs`, above the `#[cfg(test)] mod tests` block, add:

```rust
/// Salt versioning: bump `v1` if the encryption scheme itself changes;
/// bump `epoch-N` to rotate keys (after the future "wipe master from
/// device" action lands per ZEB-197 follow-on). v1 hard-codes epoch-0.
const HKDF_SALT: &[u8] = b"harmony-owner-state-v1-epoch-0";

const INFO_ENTRY_AEAD: &[u8] = b"entry-aead-key";
const INFO_ROOT_AEAD:  &[u8] = b"root-aead-key";
const INFO_TREE_LOOKUP:&[u8] = b"tree-lookup";
const INFO_NONCE_DERIV:&[u8] = b"nonce-deriv";

/// Four owner-state keys derived deterministically from the master seed.
/// Every bound device that holds the seed computes identical keys.
pub struct KeyTree {
    pub entry_aead: Zeroizing<[u8; 32]>,
    pub root_aead:  Zeroizing<[u8; 32]>,
    pub lookup:     Zeroizing<[u8; 32]>,
    pub nonce:      Zeroizing<[u8; 32]>,
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

        Ok(Self { entry_aead, root_aead, lookup, nonce })
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --manifest-path src-tauri/Cargo.toml -p harmony-app owner_state_crypto::tests::key_tree -- --nocapture 2>&1 | tail -10`
Expected: 2 tests pass.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/owner_state_crypto.rs
git commit -m "feat(zeb-215-sub-a): KeyTree HKDF derivation per ZEB-211"
```

---

## Task 3: HMAC-SHA256 tree-lookup-key derivation

**Files:**
- Modify: `src-tauri/src/owner_state_crypto.rs`

- [ ] **Step 1: Write the failing test**

Append to `mod tests`:

```rust
    #[test]
    fn space_lookup_key_is_deterministic_and_distinguishes_spaces() {
        let kt = KeyTree::derive(&TEST_SEED).expect("derive");

        let key_a1 = space_lookup_key(&kt, b"space-id-A");
        let key_a2 = space_lookup_key(&kt, b"space-id-A");
        let key_b  = space_lookup_key(&kt, b"space-id-B");

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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path src-tauri/Cargo.toml -p harmony-app owner_state_crypto::tests::space_lookup -- --nocapture 2>&1 | tail -10`
Expected: compile error — `space_lookup_key` not found.

- [ ] **Step 3: Implement `space_lookup_key()`**

In `src-tauri/src/owner_state_crypto.rs`, above the test module, add:

```rust
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
    let mut mac = HmacSha256::new_from_slice(keys.lookup.as_ref())
        .expect("HMAC accepts any key length");
    mac.update(space_id_bytes);
    mac.finalize().into_bytes().into()
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --manifest-path src-tauri/Cargo.toml -p harmony-app owner_state_crypto::tests::space_lookup -- --nocapture 2>&1 | tail -10`
Expected: 2 tests pass.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/owner_state_crypto.rs
git commit -m "feat(zeb-215-sub-a): space_lookup_key HMAC-SHA256 derivation"
```

---

## Task 4: Per-entry deterministic AEAD encrypt + decrypt

**Files:**
- Modify: `src-tauri/src/owner_state_crypto.rs`

- [ ] **Step 1: Write the failing tests**

Append to `mod tests`:

```rust
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
        assert_ne!(nonce_a, nonce_b, "cross-space nonce collision: ZEB-211 fix regressed");
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --manifest-path src-tauri/Cargo.toml -p harmony-app owner_state_crypto::tests::encrypt_entry owner_state_crypto::tests::decrypt_entry -- --nocapture 2>&1 | tail -15`
Expected: compile errors — `encrypt_entry` and `decrypt_entry` not found.

- [ ] **Step 3: Implement `encrypt_entry()` and `decrypt_entry()`**

In `src-tauri/src/owner_state_crypto.rs`, above the test module, add:

```rust
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
    let mut hasher = blake3::Hasher::new_keyed(keys.nonce.as_ref());
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
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --manifest-path src-tauri/Cargo.toml -p harmony-app owner_state_crypto::tests::encrypt_entry owner_state_crypto::tests::decrypt_entry -- --nocapture 2>&1 | tail -15`
Expected: 5 tests pass.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/owner_state_crypto.rs
git commit -m "feat(zeb-215-sub-a): per-entry deterministic AEAD with cross-space nonce binding"
```

---

## Task 5: Random-nonce root-publish AEAD encrypt + decrypt

**Files:**
- Modify: `src-tauri/src/owner_state_crypto.rs`

- [ ] **Step 1: Write the failing tests**

Append to `mod tests`:

```rust
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --manifest-path src-tauri/Cargo.toml -p harmony-app owner_state_crypto::tests::encrypt_root_publish owner_state_crypto::tests::decrypt_root_publish -- --nocapture 2>&1 | tail -15`
Expected: compile errors — `encrypt_root_publish` and `decrypt_root_publish` not found.

- [ ] **Step 3: Implement `encrypt_root_publish()` and `decrypt_root_publish()`**

Add to `src-tauri/src/owner_state_crypto.rs` above the test module:

```rust
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
    OsRng.fill_bytes(&mut nonce_bytes);

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
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --manifest-path src-tauri/Cargo.toml -p harmony-app owner_state_crypto::tests::encrypt_root_publish owner_state_crypto::tests::decrypt_root_publish -- --nocapture 2>&1 | tail -15`
Expected: 4 tests pass.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/owner_state_crypto.rs
git commit -m "feat(zeb-215-sub-a): root-publish AEAD with random nonces and key separation"
```

---

## Task 6: HLC type and lexicographic ordering

**Files:**
- Modify: `src-tauri/src/owner_state_crypto.rs`

- [ ] **Step 1: Write the failing tests**

Append to `mod tests`:

```rust
    #[test]
    fn hlc_lexicographic_ordering_per_zeb_211() {
        let a = Hlc { wall_ms: 100, logical: 0, device_id: "alice".into() };
        let b = Hlc { wall_ms: 100, logical: 0, device_id: "alice".into() };
        assert!(!a.is_strictly_newer_than(&b));
        assert!(!b.is_strictly_newer_than(&a));

        // wall_ms dominates.
        let later_wall = Hlc { wall_ms: 101, logical: 0, device_id: "alice".into() };
        assert!(later_wall.is_strictly_newer_than(&a));
        assert!(!a.is_strictly_newer_than(&later_wall));

        // logical breaks wall_ms ties.
        let later_logical = Hlc { wall_ms: 100, logical: 1, device_id: "alice".into() };
        assert!(later_logical.is_strictly_newer_than(&a));

        // device_id breaks (wall_ms, logical) ties — bytewise UTF-8.
        let later_device = Hlc { wall_ms: 100, logical: 0, device_id: "bob".into() };
        assert!(later_device.is_strictly_newer_than(&a));

        // Within tie, smaller bytewise device_id is older.
        let earlier_device = Hlc { wall_ms: 100, logical: 0, device_id: "aardvark".into() };
        assert!(a.is_strictly_newer_than(&earlier_device));
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path src-tauri/Cargo.toml -p harmony-app owner_state_crypto::tests::hlc_lexicographic_ordering -- --nocapture 2>&1 | tail -10`
Expected: compile error — `Hlc` not found OR `is_strictly_newer_than` not found.

- [ ] **Step 3: Implement `Hlc` and ordering**

Verify `serde` is already a dep (it is — used by other modules). Then add to `src-tauri/src/owner_state_crypto.rs` above the test module:

```rust
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
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --manifest-path src-tauri/Cargo.toml -p harmony-app owner_state_crypto::tests::hlc_lexicographic_ordering -- --nocapture 2>&1 | tail -10`
Expected: 1 test passes.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/owner_state_crypto.rs
git commit -m "feat(zeb-215-sub-a): HLC type + strictly-newer ordering"
```

---

## Task 7: Per-publisher replay-protection tracker

**Files:**
- Modify: `src-tauri/src/owner_state_crypto.rs`

- [ ] **Step 1: Write the failing tests**

Append to `mod tests`:

```rust
    fn hlc(wall_ms: u64, logical: u32, device_id: &str) -> Hlc {
        Hlc { wall_ms, logical, device_id: device_id.into() }
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --manifest-path src-tauri/Cargo.toml -p harmony-app owner_state_crypto::tests::replay_tracker -- --nocapture 2>&1 | tail -15`
Expected: compile error — `RootReplayTracker` not found.

- [ ] **Step 3: Implement `RootReplayTracker`**

Add to `src-tauri/src/owner_state_crypto.rs` above the test module:

```rust
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
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --manifest-path src-tauri/Cargo.toml -p harmony-app owner_state_crypto::tests::replay_tracker -- --nocapture 2>&1 | tail -15`
Expected: 4 tests pass.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/owner_state_crypto.rs
git commit -m "feat(zeb-215-sub-a): RootReplayTracker per-publisher HLC monotonicity"
```

---

## Task 8: Canonical CBOR encode + decode helpers

**Files:**
- Modify: `src-tauri/src/owner_state_crypto.rs`

- [ ] **Step 1: Write the failing tests**

Append to `mod tests`:

```rust
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
        let value = Hlc { wall_ms: 12345, logical: 7, device_id: "alice".into() };
        let bytes = canonical_cbor_encode(&value).expect("encode");
        let recovered: Hlc = canonical_cbor_decode(&bytes).expect("decode");
        assert_eq!(value, recovered);
    }

    #[test]
    fn canonical_cbor_decode_rejects_garbage() {
        let result = canonical_cbor_decode::<Hlc>(b"not cbor at all");
        assert!(matches!(result, Err(CryptoError::CborDecode(_))));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --manifest-path src-tauri/Cargo.toml -p harmony-app owner_state_crypto::tests::canonical_cbor -- --nocapture 2>&1 | tail -15`
Expected: compile errors — `canonical_cbor_encode` and `canonical_cbor_decode` not found.

- [ ] **Step 3: Implement canonical CBOR helpers**

`ciborium = "0.2"` is already in Cargo.toml. Its `into_writer` produces deterministic CBOR matching RFC 8949 §4.2 by default for primitive types and `BTreeMap` (sorted-key maps); it does NOT enforce sorting on `HashMap` values. The helper below uses `ciborium::into_writer` and documents the invariant; Phase 2's data types are required to use sorted-key collections (`BTreeMap` instead of `HashMap`) where they cross this boundary.

Add to `src-tauri/src/owner_state_crypto.rs` above the test module:

```rust
/// Canonical CBOR encoder. Produces deterministic output per RFC 8949 §4.2
/// when the input value's structure is canonical (sorted-key maps,
/// definite-length collections, no floats). The CRDT's deterministic
/// encryption property depends on byte-identical output across bound
/// devices — types crossing this boundary MUST use `BTreeMap` (which
/// `serde` serializes in sorted order) instead of `HashMap`.
pub fn canonical_cbor_encode<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, CryptoError> {
    let mut buf = Vec::new();
    ciborium::into_writer(value, &mut buf)
        .map_err(|e| CryptoError::CborEncode(format!("{e}")))?;
    Ok(buf)
}

/// Canonical CBOR decoder. Symmetric inverse of `canonical_cbor_encode`.
pub fn canonical_cbor_decode<T: serde::de::DeserializeOwned>(
    bytes: &[u8],
) -> Result<T, CryptoError> {
    ciborium::from_reader(bytes).map_err(|e| CryptoError::CborDecode(format!("{e}")))
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --manifest-path src-tauri/Cargo.toml -p harmony-app owner_state_crypto::tests::canonical_cbor -- --nocapture 2>&1 | tail -15`
Expected: 3 tests pass.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/owner_state_crypto.rs
git commit -m "feat(zeb-215-sub-a): canonical CBOR encode/decode helpers"
```

---

## Task 9: End-to-end integration smoke test

**Files:**
- Modify: `src-tauri/src/owner_state_crypto.rs`

- [ ] **Step 1: Write the failing test**

Append to `mod tests`:

```rust
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
        let publish_payload = canonical_cbor_encode(&(cipher_cid.as_bytes().to_vec(), publish_at.clone()))
            .expect("encode publish");
        let publish_blob = encrypt_root_publish(&kt_a, &publish_payload).expect("encrypt-publish");

        // Device B's replay tracker accepts the first publish from device-a.
        let mut tracker_b = RootReplayTracker::default();
        let recovered_publish = decrypt_root_publish(&kt_b, &publish_blob).expect("decrypt-publish");
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
```

- [ ] **Step 2: Run test to verify it passes**

Run: `cargo test --manifest-path src-tauri/Cargo.toml -p harmony-app owner_state_crypto::tests::integration_full_round_trip -- --nocapture 2>&1 | tail -15`
Expected: 1 test passes (all helper functions implemented in earlier tasks).

If this test fails despite all unit tests passing, the failure is informative — the integration likely caught a contract mismatch in one of the earlier helpers. Fix in the originating task and re-run all tests.

- [ ] **Step 3: Run the full module test suite for confidence**

Run: `cargo test --manifest-path src-tauri/Cargo.toml -p harmony-app owner_state_crypto -- --nocapture 2>&1 | tail -30`
Expected: all tests in the module pass (cumulative count from Tasks 2–9 — at least 19 tests).

- [ ] **Step 4: Verify clippy + fmt are clean**

Run: `cargo clippy --manifest-path src-tauri/Cargo.toml --package harmony-app -- -D warnings 2>&1 | tail -10`
Expected: no warnings.

Run: `cargo fmt --manifest-path src-tauri/Cargo.toml --all -- --check 2>&1 | tail -5`
Expected: no diff.

If `cargo fmt --check` shows a diff, run `cargo fmt --manifest-path src-tauri/Cargo.toml --all` and re-stage.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/owner_state_crypto.rs
git commit -m "test(zeb-215-sub-a): end-to-end crypto integration round-trip"
```

---

## Task 10: Phase 1 PR

**Files:** None this task — this is the ship step.

- [ ] **Step 1: Verify the branch is up-to-date with main**

Run:

```bash
git fetch origin
git log --oneline origin/main..HEAD | head -10
```

Expected: 9 commits (one per task above, Tasks 1–9).

If the branch was created from a stale main, rebase:

```bash
git rebase origin/main
```

Re-run the test suite after rebase if any conflicts surfaced.

- [ ] **Step 2: Push the branch**

Run: `git push -u origin <branch-name>` (the branch name is whatever subagent-driven-development created — likely `zeb-215-sub-a-phase1-crypto` or similar).

- [ ] **Step 3: Create the PR**

```bash
gh pr create --title "feat(zeb-215-sub-a): Phase 1 — owner-state crypto primitives" --body "$(cat <<'EOF'
## Summary
- Implements ZEB-211 (owner-state encryption design) as a self-contained Rust module
- HKDF-SHA256 4-key derivation tree (entry-aead, root-aead, lookup, nonce) with Zeroizing wrappers
- HMAC-SHA256 tree-lookup-key derivation
- Per-entry deterministic AEAD with cross-space nonce binding (prevents the catastrophic key-reuse vulnerability surfaced during ZEB-211 round-2 review)
- Random-nonce root-publish AEAD with separate AEAD key (key-separation per ZEB-211 round-2)
- HLC type + strictly-newer ordering (lexicographic on (wall_ms, logical, device_id))
- Per-publisher replay-protection tracker (rejects rolled-back state-root publishes)
- Canonical CBOR encode/decode helpers (deterministic-encryption invariant)

## Phase 1 of 5 for Sub-A
This is the foundational crypto layer. Phase 2 will add the CRDT primitives (`owner_state_crdt.rs`) that consume this module. Phases 3–5 (Zenoh sync, IPC commands, frontend NavService rewrite) follow.

## What's NOT in this PR
- No CRDT logic — this is a pure crypto library
- No I/O — Phase 3 owns Zenoh transport
- No Tauri commands — Phase 4 owns the IPC surface
- No frontend changes — Phase 5 owns NavService

## Test plan
- [x] Unit tests cover KeyTree derivation, lookup-key HMAC, deterministic per-entry AEAD, random-nonce root-publish AEAD, HLC ordering, replay tracker, canonical CBOR
- [x] Integration test simulates two bound devices, full round-trip, plus an active-adversary replay (rejected)
- [x] Cross-space nonce-binding test asserts the ZEB-211 round-2 fix (different spaces with identical cleartext produce different nonces)
- [x] AAD-binding negative test (relocation attack rejected)
- [x] cargo clippy -D warnings clean
- [x] cargo fmt --check clean

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

Expected: PR URL printed.

- [ ] **Step 4: Report the PR URL back to the user**

The user is the next gate — they review the PR, address any bot/human feedback, then merge. Phase 2 starts after merge.

---

## Verification gates (Phase 1 acceptance)

Before requesting review:

- `cargo test --manifest-path src-tauri/Cargo.toml -p harmony-app owner_state_crypto -- --nocapture` — all module tests green (≥19)
- `cargo build --manifest-path src-tauri/Cargo.toml` — clean build
- `cargo clippy --manifest-path src-tauri/Cargo.toml -- -D warnings` — clean
- `cargo fmt --manifest-path src-tauri/Cargo.toml --all -- --check` — clean
- `npx vitest run` — frontend tests still pass (no frontend changes in Phase 1, so this is a sanity check)
- `npx tsc --noEmit` — clean (no frontend changes, sanity check)

---

## Out of scope for Phase 1 (deferred to Phase 2+)

- `Space`, `OutboxEntry`, `InboxEntry`, `ReadMarker` types (Phase 2)
- CRDT merge semantics (Phase 2)
- Dependent-record canonicalization on dedupe (Phase 2)
- Tombstone vs leave handling (Phase 2)
- Owner-state Zenoh topic publish/subscribe (Phase 3)
- Tauri IPC commands (Phase 4)
- NavService rewrite (Phase 5)
- ULID generation (Phase 2 will add the `ulid` crate)
