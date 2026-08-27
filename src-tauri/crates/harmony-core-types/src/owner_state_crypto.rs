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
use zeroize::{Zeroize, Zeroizing};

pub use crate::owner_state_types::Hlc;

/// Sealed marker trait certifying that a type's `serde::Serialize` impl
/// produces deterministic CBOR output meeting the
/// `canonical_cbor_encode` contract. The trait is sealed (cannot be
/// impl'd outside this crate) so all impls live in `owner_state_types`
/// and a reviewer can audit them in one place.
///
/// Closes ZEB-220 (the sealed marker trait portion). The strict
/// RFC 8949 §4.2.1 cross-implementation encoder remains a separate
/// followup since ciborium 0.2 has no canonical writer.
pub trait CanonicalPayload: sealed::CanonicalPayloadSealed + serde::Serialize {}

pub mod sealed {
    /// Sealing supertrait for `CanonicalPayload`. ZEB-548 Stage 0: now
    /// `pub` (was `pub(crate)`) so the exported `impl_canonical!` macro can
    /// certify wire types that live in *other* workspace crates. The seal is
    /// now by convention — certify a type only via `impl_canonical!`, never a
    /// hand-written impl — rather than by crate visibility. ZEB-220's
    /// "audit the impls in one place" intent is preserved per-crate: each
    /// crate's `impl_canonical!` block sits next to the types it certifies.
    pub trait CanonicalPayloadSealed {}
}

// Base-type impls used by Phase 2 owner_state_types. New impls go in
// `owner_state_types`, NOT here — keep base-type impls together.
// Note: we deliberately don't implement CanonicalPayload for `[u8; N]`.
// The standard serde Serialize impl for `[u8; N]` emits a CBOR array
// (major type 4), not a bstr — but our wire format requires bstr for
// fixed-length identifiers. The Phase 2 newtypes (SpaceId, OwnerAddr,
// ContentId, OutboxEntryId) wrap `[u8; N]` AND override its Serialize
// impl with custom helpers that emit bstr. Their CanonicalPayload
// impls live in `impl_canonical!` in owner_state_types.rs, not via
// any blanket `[u8; N]` impl here.

impl sealed::CanonicalPayloadSealed for u8 {}
impl CanonicalPayload for u8 {}
impl sealed::CanonicalPayloadSealed for u16 {}
impl CanonicalPayload for u16 {}
impl sealed::CanonicalPayloadSealed for u32 {}
impl CanonicalPayload for u32 {}
impl sealed::CanonicalPayloadSealed for u64 {}
impl CanonicalPayload for u64 {}
impl sealed::CanonicalPayloadSealed for String {}
impl CanonicalPayload for String {}

impl<T: CanonicalPayload> sealed::CanonicalPayloadSealed for Option<T> {}
impl<T: CanonicalPayload> CanonicalPayload for Option<T> {}

impl<T: CanonicalPayload> sealed::CanonicalPayloadSealed for Vec<T> {}
impl<T: CanonicalPayload> CanonicalPayload for Vec<T> {}

impl<T: CanonicalPayload> sealed::CanonicalPayloadSealed for std::collections::BTreeSet<T> {}
impl<T: CanonicalPayload> CanonicalPayload for std::collections::BTreeSet<T> {}

impl<K: CanonicalPayload, V: CanonicalPayload> sealed::CanonicalPayloadSealed
    for std::collections::BTreeMap<K, V>
{
}
impl<K: CanonicalPayload, V: CanonicalPayload> CanonicalPayload
    for std::collections::BTreeMap<K, V>
{
}

impl<A: CanonicalPayload, B: CanonicalPayload> sealed::CanonicalPayloadSealed for (A, B) {}
impl<A: CanonicalPayload, B: CanonicalPayload> CanonicalPayload for (A, B) {}

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
    #[error(
        "unsupported fleet KeyTree epoch {0}: only epoch 0 is supported (rotation is out of scope)"
    )]
    UnsupportedEpoch(u32),
}

/// Per-epoch HKDF salt (ZEB-668 S5). Salt versioning: bump `v1` if the
/// encryption scheme itself changes; the epoch suffix rotates keys on a
/// fleet KeyTree epoch bump. Epoch 0 is byte-identical to the pre-S5 fixed
/// constant `harmony-owner-state-v1-epoch-0` — pinned by
/// `epoch_salt_zero_matches_legacy_constant` and the golden-vector test.
fn epoch_salt(epoch: u32) -> Vec<u8> {
    format!("harmony-owner-state-v1-epoch-{epoch}").into_bytes()
}

const INFO_ENTRY_AEAD: &[u8] = b"entry-aead-key";
const INFO_ROOT_AEAD: &[u8] = b"root-aead-key";
const INFO_TREE_LOOKUP: &[u8] = b"tree-lookup";
const INFO_NONCE_DERIV: &[u8] = b"nonce-deriv";
const INFO_FRIEND_AEAD: &[u8] = b"friend-secret-aead-key";

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
    /// Fleet KeyTree epoch this tree was derived at (ZEB-668 S5). Not key
    /// material — plain copy, no zeroize. Distinct from transport/tunnel/
    /// community epochs. Read-only outside the crate via [`KeyTree::epoch`] — a
    /// mutable `pub` field would let a caller relabel a derived tree's epoch
    /// without re-deriving its keys (CodeRabbit, PR #734).
    pub(crate) epoch: u32,
    entry_aead: Zeroizing<[u8; 32]>,
    root_aead: Zeroizing<[u8; 32]>,
    lookup: Zeroizing<[u8; 32]>,
    nonce: Zeroizing<[u8; 32]>,
    friend_aead: Zeroizing<[u8; 32]>,
}

impl KeyTree {
    /// The fleet KeyTree epoch this tree was derived at (ZEB-668 S5). Read-only
    /// accessor: the epoch is bound to the derived key material and must never be
    /// relabeled independently of it.
    pub fn epoch(&self) -> u32 {
        self.epoch
    }

    /// Derive all keys at epoch 0 — the pre-rotation tree every fleet starts
    /// on. Byte-identical to the pre-S5 derivation (golden-pinned).
    pub fn derive(master_seed: &[u8; 32]) -> Result<Self, CryptoError> {
        Self::derive_at_epoch(master_seed, 0)
    }

    /// Derive all keys via HKDF-SHA256 with domain separation, at the given
    /// fleet KeyTree epoch (ZEB-668 S5). Same seed + same epoch → identical
    /// keys on every device; different epochs share no key material.
    pub fn derive_at_epoch(master_seed: &[u8; 32], epoch: u32) -> Result<Self, CryptoError> {
        let hk = Hkdf::<Sha256>::new(Some(&epoch_salt(epoch)), master_seed);

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

        let mut friend_aead = Zeroizing::new([0u8; 32]);
        hk.expand(INFO_FRIEND_AEAD, friend_aead.as_mut())
            .map_err(|e| CryptoError::Hkdf(format!("friend-aead: {e}")))?;

        Ok(Self {
            epoch,
            entry_aead,
            root_aead,
            lookup,
            nonce,
            friend_aead,
        })
    }

    /// Generate a KeyTree with fresh **random** key material at `epoch`, for a
    /// master-less (quorum) fleet epoch bump (ZEB-677 S5). Fleet keys are
    /// symmetric AEAD material distributed to survivors as sealed blobs — never
    /// required to be re-derivable from the master seed (cert-only devices
    /// already adopt via [`Self::from_fleet_material`]). The initiator of a
    /// quorum bump holds no seed, so it mints new material here and seals it to
    /// the survivors.
    pub fn generate_at_epoch(epoch: u32) -> Self {
        let fill = || {
            let mut k = Zeroizing::new([0u8; 32]);
            OsRng.fill_bytes(k.as_mut());
            k
        };
        Self {
            epoch,
            entry_aead: fill(),
            root_aead: fill(),
            lookup: fill(),
            nonce: fill(),
            friend_aead: fill(),
        }
    }

    /// Export this KeyTree's key material for sealed distribution to an
    /// enrolled device. Only the seed-holding (inviter) device calls this.
    ///
    /// Stamps the tree's own `epoch` (ZEB-668 S5). The ZEB-492 "epoch
    /// parameter footgun" (a caller minting material the import side
    /// refuses) is gone by construction: the epoch comes from the tree the
    /// material was derived as, never from a caller.
    pub fn to_fleet_material(&self) -> FleetKeyMaterial {
        FleetKeyMaterial {
            epoch: self.epoch,
            entry_aead: *self.entry_aead,
            root_aead: *self.root_aead,
            lookup: *self.lookup,
            nonce: *self.nonce,
            friend_aead: *self.friend_aead,
        }
    }

    /// Reconstruct a KeyTree from distributed material (cert-only device, which
    /// has no master seed to re-derive from). Produces a KeyTree byte-identical
    /// to the originating seed-holder's, at the material's epoch.
    ///
    /// Accepts any epoch (ZEB-668 S5 — rotation-era material carries the
    /// epoch it was derived at; the pre-S5 `epoch != 0` rejection existed
    /// only because no rotation could mint such material yet). Kept fallible
    /// so a future material-format bump can gate here again
    /// ([`CryptoError::UnsupportedEpoch`] stays reserved for that).
    pub fn from_fleet_material(m: &FleetKeyMaterial) -> Result<Self, CryptoError> {
        Ok(Self {
            epoch: m.epoch,
            entry_aead: Zeroizing::new(m.entry_aead),
            root_aead: Zeroizing::new(m.root_aead),
            lookup: Zeroizing::new(m.lookup),
            nonce: Zeroizing::new(m.nonce),
            friend_aead: Zeroizing::new(m.friend_aead),
        })
    }
}

/// Runtime-swappable set of installed fleet KeyTrees (ZEB-668 S5).
///
/// The dual-epoch read window means an engine may need to decrypt inbound
/// publishes under either the previous or the newest epoch while always
/// publishing on the newest. Cloning is cheap (shared inner); every fleet
/// engine holds a clone of the same set, so an epoch install/prune applies
/// to all of them at once without restarts.
///
/// Non-empty by construction: `new` seeds the set and `retain_newest_only`
/// keeps one entry, so `newest()` cannot observe an empty set.
#[derive(Clone)]
pub struct FleetKeySet {
    /// Sorted descending by epoch; index 0 is the publish key.
    inner: std::sync::Arc<std::sync::RwLock<Vec<std::sync::Arc<KeyTree>>>>,
}

impl FleetKeySet {
    pub fn new(first: std::sync::Arc<KeyTree>) -> Self {
        Self {
            inner: std::sync::Arc::new(std::sync::RwLock::new(vec![first])),
        }
    }

    fn lock_read(&self) -> std::sync::RwLockReadGuard<'_, Vec<std::sync::Arc<KeyTree>>> {
        self.inner.read().unwrap_or_else(|e| e.into_inner())
    }

    /// Highest installed epoch — the publish key.
    pub fn newest(&self) -> std::sync::Arc<KeyTree> {
        std::sync::Arc::clone(&self.lock_read()[0])
    }

    /// Decrypt candidates, newest first. A publish is entirely single-epoch,
    /// so callers bind whichever tree decrypts the root for the rest of the
    /// message.
    pub fn accept_set(&self) -> Vec<std::sync::Arc<KeyTree>> {
        self.lock_read().clone()
    }

    /// Install a tree; idempotent by epoch (an already-installed epoch is
    /// left untouched). Keeps the descending sort.
    pub fn install(&self, kt: std::sync::Arc<KeyTree>) {
        let mut set = self.inner.write().unwrap_or_else(|e| e.into_inner());
        if set.iter().any(|k| k.epoch == kt.epoch) {
            return;
        }
        set.push(kt);
        set.sort_by(|a, b| b.epoch.cmp(&a.epoch));
    }

    /// Window close: drop everything but the newest epoch from the accept
    /// set. (Epoch-0 material persisted for the fleet-keys carrier lives
    /// outside this set — the carrier engine holds its own fixed tree.)
    pub fn retain_newest_only(&self) {
        let mut set = self.inner.write().unwrap_or_else(|e| e.into_inner());
        set.truncate(1);
    }

    /// Drop every epoch strictly below `min_epoch` (PR #455 round 2,
    /// Greptile P1): a replay-driven install must evict stale epochs the
    /// boot set carried (a restarted seed-holder boots at epoch 0), not
    /// merely add the new epoch beside them. Never empties the set — the
    /// newest epoch always survives even when `min_epoch` overshoots it.
    pub fn retain_min_epoch(&self, min_epoch: u32) {
        let mut set = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let newest = set[0].epoch;
        let floor = min_epoch.min(newest);
        set.retain(|k| k.epoch >= floor);
    }

    /// Installed epochs, descending.
    pub fn epochs(&self) -> Vec<u32> {
        self.lock_read().iter().map(|k| k.epoch).collect()
    }
}

/// Serializable export of a `KeyTree`'s raw key material, for sealed
/// distribution to cert-only enrolled devices (ZEB-492). Carries an explicit
/// `epoch` so a future KeyTree rotation is non-breaking (rotation itself is
/// out of scope — see the ZEB-492 spec).
///
/// SECURITY: this is the ONLY place `KeyTree` key bytes leave the type. No
/// `Debug` (would print key material). `ZeroizeOnDrop` wipes the key fields on
/// drop. Only ever moved through the SAS-sealed pairing channel and the
/// encrypted vault. Mirrors the `SecretVault` pattern (plain `[u8;32]` serde
/// fields + struct-level zeroize).
#[derive(serde::Serialize, serde::Deserialize, zeroize::ZeroizeOnDrop)]
pub struct FleetKeyMaterial {
    #[zeroize(skip)]
    pub epoch: u32,
    entry_aead: [u8; 32],
    root_aead: [u8; 32],
    lookup: [u8; 32],
    nonce: [u8; 32],
    friend_aead: [u8; 32],
}

/// Encode a set of fleet materials for the vault `fleet_keytree` slot
/// (ZEB-668 S5). Always writes the multi-epoch CBOR-array format; the vault
/// invariant is "epoch-0 + current (+ previous during a bump window)".
/// `Zeroizing`: the buffer holds raw key material.
pub fn encode_fleet_material_set(
    materials: &[FleetKeyMaterial],
) -> Result<Zeroizing<Vec<u8>>, String> {
    let mut buf = Zeroizing::new(Vec::new());
    ciborium::into_writer(&materials, &mut *buf)
        .map_err(|e| format!("encode material set: {e}"))?;
    Ok(buf)
}

/// Decode the vault `fleet_keytree` slot. Post-S5 slots hold a CBOR array of
/// `FleetKeyMaterial`; pre-S5 slots hold a single CBOR map — decoded as a
/// one-element set (its material is epoch 0 by construction). Discrimination
/// is structural (CBOR array vs map major type), no version byte needed.
pub fn decode_fleet_material_set(bytes: &[u8]) -> Result<Vec<FleetKeyMaterial>, String> {
    if let Ok(set) = ciborium::from_reader::<Vec<FleetKeyMaterial>, _>(bytes) {
        if set.is_empty() {
            return Err("fleet material set is empty".to_string());
        }
        return Ok(set);
    }
    ciborium::from_reader::<FleetKeyMaterial, _>(bytes)
        .map(|single| vec![single])
        .map_err(|e| format!("decode fleet_keytree (neither set nor legacy single): {e}"))
}

/// Derive the per-space Prolly Tree lookup key.
///
/// The lookup key is `HMAC-SHA256(owner_state_lookup_key, space_id_bytes)`
/// — a keyed MAC, NOT a plain hash, so observers without the lookup key
/// cannot enumerate the tree by precomputing hashes of known space IDs.
///
/// Returns 32 bytes (wrapped in `Zeroizing`) for use as a Prolly Tree key
/// AND as AAD when encrypting that space's value (defense-in-depth against
/// ciphertext relocation; see ZEB-211 spec). The `Zeroizing` wrapper
/// matches the protection on the source `KeyTree.lookup` material —
/// callers that store this in a struct field will get drop-time scrubbing
/// for free. AEAD calls take `&[u8; 32]`; pass `&*lookup` (Deref) or
/// `lookup.as_ref()` (AsRef).
pub fn space_lookup_key(keys: &KeyTree, space_id_bytes: &[u8]) -> Zeroizing<[u8; 32]> {
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = <HmacSha256 as Mac>::new_from_slice(keys.lookup.as_ref())
        .expect("HMAC accepts any key length");
    mac.update(space_id_bytes);
    Zeroizing::new(mac.finalize().into_bytes().into())
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

/// Domain-separated AAD base for friend-secret sealing; the friend's owner_id is
/// appended so a sealed secret cannot be relocated into another friend's entry.
const AAD_FRIEND_SECRET: &[u8] = b"friend-rendezvous-secret";

/// Seal a 32-byte friendship secret for storage in a `FriendEntry`. Random
/// nonce; layout `nonce(12) || ChaCha20-Poly1305 ciphertext+tag(48)`. AAD binds
/// the ciphertext to the specific friendship (`friend_owner_id`).
pub fn encrypt_friend_secret(
    keys: &KeyTree,
    friend_owner_id: &[u8; 16],
    secret: &[u8; 32],
) -> Result<Vec<u8>, CryptoError> {
    let mut nonce_bytes = [0u8; 12];
    OsRng
        .try_fill_bytes(&mut nonce_bytes)
        .map_err(|e| CryptoError::Rng(e.to_string()))?;
    let cipher = ChaCha20Poly1305::new_from_slice(keys.friend_aead.as_ref())
        .expect("ChaCha20-Poly1305 accepts a 32-byte key");
    let mut aad = AAD_FRIEND_SECRET.to_vec();
    aad.extend_from_slice(friend_owner_id);
    let ciphertext = cipher
        .encrypt(
            Nonce::from_slice(&nonce_bytes),
            chacha20poly1305::aead::Payload {
                msg: secret,
                aad: &aad,
            },
        )
        .map_err(|_| CryptoError::AeadEncrypt)?;
    let mut blob = Vec::with_capacity(12 + ciphertext.len());
    blob.extend_from_slice(&nonce_bytes);
    blob.extend_from_slice(&ciphertext);
    Ok(blob)
}

/// Open a blob produced by [`encrypt_friend_secret`]. Returns the 32-byte secret
/// (zeroizing) or `CryptoError::AeadDecrypt` on wrong key/AAD/corruption.
pub fn decrypt_friend_secret(
    keys: &KeyTree,
    friend_owner_id: &[u8; 16],
    blob: &[u8],
) -> Result<Zeroizing<[u8; 32]>, CryptoError> {
    if blob.len() != 12 + 32 + 16 {
        return Err(CryptoError::AeadDecrypt);
    }
    let (nonce_bytes, ciphertext) = blob.split_at(12);
    let cipher = ChaCha20Poly1305::new_from_slice(keys.friend_aead.as_ref())
        .expect("ChaCha20-Poly1305 accepts a 32-byte key");
    let mut aad = AAD_FRIEND_SECRET.to_vec();
    aad.extend_from_slice(friend_owner_id);
    let mut plaintext = cipher
        .decrypt(
            Nonce::from_slice(nonce_bytes),
            chacha20poly1305::aead::Payload {
                msg: ciphertext,
                aad: &aad,
            },
        )
        .map_err(|_| CryptoError::AeadDecrypt)?;
    let mut out = Zeroizing::new([0u8; 32]);
    out.copy_from_slice(&plaintext);
    plaintext.zeroize();
    Ok(out)
}

/// Domain-separated AAD for per-file DEK sealing at rest (ZEB-674). DISTINCT
/// from `AAD_FRIEND_SECRET` so a sealed DEK can never be opened as a friend
/// secret (or vice versa) even though both seal under `friend_aead` — the AAD
/// mismatch fails the Poly1305 tag. Unlike the friend-secret AAD there is no
/// per-target suffix: a file DEK is sealed to SELF, not bound to a counterparty.
const AAD_FILE_DEK: &[u8] = b"zeb-674-file-dek-at-rest";

/// Seal a 32-byte per-file DEK for at-rest storage in `OwnerState.file_deks`.
/// Mirrors [`encrypt_friend_secret`] exactly (random nonce; layout
/// `nonce(12) || ChaCha20-Poly1305 ciphertext+tag(48)`) but binds `AAD_FILE_DEK`
/// so the sealed blob is domain-separated from friend secrets. Reuses
/// `friend_aead` — the owner's KeyTree is shared across their bound devices, so
/// any of them can unseal, which is exactly the Flow-A replication contract for
/// `file_deks`.
pub fn encrypt_file_dek(keys: &KeyTree, dek: &[u8; 32]) -> Result<Vec<u8>, CryptoError> {
    let mut nonce_bytes = [0u8; 12];
    OsRng
        .try_fill_bytes(&mut nonce_bytes)
        .map_err(|e| CryptoError::Rng(e.to_string()))?;
    let cipher = ChaCha20Poly1305::new_from_slice(keys.friend_aead.as_ref())
        .expect("ChaCha20-Poly1305 accepts a 32-byte key");
    let ciphertext = cipher
        .encrypt(
            Nonce::from_slice(&nonce_bytes),
            chacha20poly1305::aead::Payload {
                msg: dek,
                aad: AAD_FILE_DEK,
            },
        )
        .map_err(|_| CryptoError::AeadEncrypt)?;
    let mut blob = Vec::with_capacity(12 + ciphertext.len());
    blob.extend_from_slice(&nonce_bytes);
    blob.extend_from_slice(&ciphertext);
    Ok(blob)
}

/// Open a blob produced by [`encrypt_file_dek`]. Returns the 32-byte DEK
/// (zeroizing) or `CryptoError::AeadDecrypt` on wrong key/AAD/corruption.
pub fn decrypt_file_dek(keys: &KeyTree, blob: &[u8]) -> Result<Zeroizing<[u8; 32]>, CryptoError> {
    if blob.len() != 12 + 32 + 16 {
        return Err(CryptoError::AeadDecrypt);
    }
    let (nonce_bytes, ciphertext) = blob.split_at(12);
    let cipher = ChaCha20Poly1305::new_from_slice(keys.friend_aead.as_ref())
        .expect("ChaCha20-Poly1305 accepts a 32-byte key");
    let mut plaintext = cipher
        .decrypt(
            Nonce::from_slice(nonce_bytes),
            chacha20poly1305::aead::Payload {
                msg: ciphertext,
                aad: AAD_FILE_DEK,
            },
        )
        .map_err(|_| CryptoError::AeadDecrypt)?;
    let mut out = Zeroizing::new([0u8; 32]);
    out.copy_from_slice(&plaintext);
    plaintext.zeroize();
    Ok(out)
}

/// Domain-separated AAD prefix for sealed dataset files at rest (ZEB-981).
/// DISTINCT from `AAD_FILE_DEK` and `AAD_FRIEND_SECRET` so a sealed dataset
/// file can never be opened as either (and vice versa) even though all three
/// seal under `friend_aead`. The file's canonical filename (label) is
/// appended so ciphertext moved between dataset files fails the Poly1305 tag.
const AAD_DATASET_AT_REST: &[u8] = b"zeb-981-dataset-at-rest:v2:";

fn dataset_aad(label: &str) -> Vec<u8> {
    let mut aad = Vec::with_capacity(AAD_DATASET_AT_REST.len() + label.len());
    aad.extend_from_slice(AAD_DATASET_AT_REST);
    aad.extend_from_slice(label.as_bytes());
    aad
}

/// Seal a dataset file image for at-rest storage (ZEB-981). Mirrors
/// [`encrypt_file_dek`] (random nonce; layout `nonce(12) ‖ ct+tag`) with the
/// dataset AAD domain. `label` is the file's canonical filename constant.
pub fn seal_dataset_file(
    keys: &KeyTree,
    label: &str,
    plaintext: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    let mut nonce_bytes = [0u8; 12];
    OsRng
        .try_fill_bytes(&mut nonce_bytes)
        .map_err(|e| CryptoError::Rng(e.to_string()))?;
    let cipher = ChaCha20Poly1305::new_from_slice(keys.friend_aead.as_ref())
        .expect("ChaCha20-Poly1305 accepts a 32-byte key");
    let ciphertext = cipher
        .encrypt(
            Nonce::from_slice(&nonce_bytes),
            chacha20poly1305::aead::Payload {
                msg: plaintext,
                aad: &dataset_aad(label),
            },
        )
        .map_err(|_| CryptoError::AeadEncrypt)?;
    let mut blob = Vec::with_capacity(12 + ciphertext.len());
    blob.extend_from_slice(&nonce_bytes);
    blob.extend_from_slice(&ciphertext);
    Ok(blob)
}

/// Open a blob produced by [`seal_dataset_file`]. Returns the inner file
/// image zeroized-on-drop, or `CryptoError::AeadDecrypt` on wrong
/// key/label/corruption — callers map that to quarantine-and-self-heal
/// (fleet sync re-populates content from peer devices).
pub fn open_dataset_file(
    keys: &KeyTree,
    label: &str,
    sealed: &[u8],
) -> Result<Zeroizing<Vec<u8>>, CryptoError> {
    if sealed.len() < 12 + 16 {
        return Err(CryptoError::AeadDecrypt);
    }
    let (nonce_bytes, ciphertext) = sealed.split_at(12);
    let cipher = ChaCha20Poly1305::new_from_slice(keys.friend_aead.as_ref())
        .expect("ChaCha20-Poly1305 accepts a 32-byte key");
    let plaintext = cipher
        .decrypt(
            Nonce::from_slice(nonce_bytes),
            chacha20poly1305::aead::Payload {
                msg: ciphertext,
                aad: &dataset_aad(label),
            },
        )
        .map_err(|_| CryptoError::AeadDecrypt)?;
    Ok(Zeroizing::new(plaintext))
}

/// Domain-separated AAD prefix for device-sealed files at rest (ZEB-982).
/// DISTINCT from `AAD_DATASET_AT_REST`: those seal under the fleet KeyTree's
/// `friend_aead`, these under the seed-derived device key, and neither
/// domain's ciphertext may ever open in the other. The file's canonical
/// filename (label) is appended so ciphertext moved between files fails the
/// Poly1305 tag.
const AAD_DEVICE_AT_REST: &[u8] = b"zeb-982-device-at-rest:v3:";

fn device_aad(label: &str) -> Vec<u8> {
    let mut aad = Vec::with_capacity(AAD_DEVICE_AT_REST.len() + label.len());
    aad.extend_from_slice(AAD_DEVICE_AT_REST);
    aad.extend_from_slice(label.as_bytes());
    aad
}

/// HKDF salt/info for the device dataset key (ZEB-982). Golden-pinned in
/// tests — changing either constant strands every sealed file on disk.
const DEVICE_DATASET_SALT: &[u8] = b"zeb-982-device-dataset-salt";
const INFO_DEVICE_DATASET_AEAD: &[u8] = b"device-dataset-aead";

/// Derive the device dataset at-rest key from the node identity master seed
/// (ZEB-982). The seed is available on every boot mode — keyless local-only
/// (ZEB-905), pre-mint, recovery CLI — which is exactly why this key, and
/// not the fleet KeyTree, seals the owner-state family. Same seed → same
/// key, so files survive re-pairing as long as the node identity does.
pub fn derive_device_dataset_key(seed: &[u8; 32]) -> Result<Zeroizing<[u8; 32]>, CryptoError> {
    let hk = Hkdf::<Sha256>::new(Some(DEVICE_DATASET_SALT), seed);
    let mut key = Zeroizing::new([0u8; 32]);
    hk.expand(INFO_DEVICE_DATASET_AEAD, key.as_mut())
        .map_err(|e| CryptoError::Hkdf(format!("device-dataset-aead: {e}")))?;
    Ok(key)
}

/// HKDF info for the mint ledger SQLCipher key (ZEB-985). Golden-pinned in
/// tests — changing it (or DEVICE_DATASET_SALT) strands every encrypted
/// ledger.db on disk.
const INFO_MINT_LEDGER_SQLCIPHER: &[u8] = b"mint-ledger-sqlcipher";

/// Derive the raw SQLCipher key for `mint/ledger.db` from the node identity
/// master seed (ZEB-985). A sibling of [`derive_device_dataset_key`] under
/// the same salt with a distinct info label: SQLCipher consumes the raw key
/// directly (`PRAGMA key = "x'…'"`, no PBKDF2 pass), so it must never be
/// byte-shared with the ChaCha20-Poly1305 dataset key.
pub fn derive_mint_ledger_key(seed: &[u8; 32]) -> Result<Zeroizing<[u8; 32]>, CryptoError> {
    let hk = Hkdf::<Sha256>::new(Some(DEVICE_DATASET_SALT), seed);
    let mut key = Zeroizing::new([0u8; 32]);
    hk.expand(INFO_MINT_LEDGER_SQLCIPHER, key.as_mut())
        .map_err(|e| CryptoError::Hkdf(format!("mint-ledger-sqlcipher: {e}")))?;
    Ok(key)
}

/// Seal a file image under the device dataset key (ZEB-982). Same layout as
/// [`seal_dataset_file`] (random nonce; `nonce(12) ‖ ct+tag`) in the device
/// AAD domain. `label` is the file's canonical filename constant.
pub fn seal_device_file(
    key: &[u8; 32],
    label: &str,
    plaintext: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    let mut nonce_bytes = [0u8; 12];
    OsRng
        .try_fill_bytes(&mut nonce_bytes)
        .map_err(|e| CryptoError::Rng(e.to_string()))?;
    let cipher =
        ChaCha20Poly1305::new_from_slice(key).expect("ChaCha20-Poly1305 accepts a 32-byte key");
    let ciphertext = cipher
        .encrypt(
            Nonce::from_slice(&nonce_bytes),
            chacha20poly1305::aead::Payload {
                msg: plaintext,
                aad: &device_aad(label),
            },
        )
        .map_err(|_| CryptoError::AeadEncrypt)?;
    let mut blob = Vec::with_capacity(12 + ciphertext.len());
    blob.extend_from_slice(&nonce_bytes);
    blob.extend_from_slice(&ciphertext);
    Ok(blob)
}

/// Open a blob produced by [`seal_device_file`]. Returns the inner file
/// image zeroized-on-drop, or `CryptoError::AeadDecrypt` on wrong
/// key/label/corruption. Callers do NOT uniformly quarantine on failure —
/// each persist family maps this to its own recovery contract (boot-fatal
/// for the owner family, quarantine for the sidecars; see the ZEB-982 spec).
pub fn open_device_file(
    key: &[u8; 32],
    label: &str,
    sealed: &[u8],
) -> Result<Zeroizing<Vec<u8>>, CryptoError> {
    if sealed.len() < 12 + 16 {
        return Err(CryptoError::AeadDecrypt);
    }
    let (nonce_bytes, ciphertext) = sealed.split_at(12);
    let cipher =
        ChaCha20Poly1305::new_from_slice(key).expect("ChaCha20-Poly1305 accepts a 32-byte key");
    let plaintext = cipher
        .decrypt(
            Nonce::from_slice(nonce_bytes),
            chacha20poly1305::aead::Payload {
                msg: ciphertext,
                aad: &device_aad(label),
            },
        )
        .map_err(|_| CryptoError::AeadDecrypt)?;
    Ok(Zeroizing::new(plaintext))
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
///
/// # Persistence (Phase 3 responsibility)
///
/// This tracker is in-memory only. On process restart it resets to empty,
/// which means a previously-captured AEAD-valid publish blob is accepted
/// again on the next boot. **Phase 3 (Zenoh transport integration) MUST
/// persist `last_accepted` durably** alongside the rest of owner-state,
/// otherwise an adversary who captures one valid publish can replay it
/// after every restart. A natural place is the same on-disk owner-state
/// store, written under a transactional barrier with the new `root_cid`
/// it gates so the two cannot diverge on crash.
///
/// # Bounded growth (Phase 3 responsibility)
///
/// `last_accepted` gains one entry per distinct `device_id`. The growth
/// is bounded in practice by the AEAD authentication gate before
/// `try_accept` is called: only publishes signed under the owner's
/// `root_aead` key (held by bound devices per ZEB-173/ZEB-197) reach the
/// tracker. The legitimate device set is therefore bounded by the
/// owner's bound-devices roster (typically <100), not by the public.
/// However, if Phase 3 ever decides to keep a long history of retired
/// device IDs (e.g., for audit), it should add LRU eviction or a
/// retention-window policy here to prevent slow growth from rotated
/// devices over years.
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

/// Deterministic CBOR encoder for the owner-state crypto surface.
///
/// **Contract (current scope — Phase 1):** produces byte-identical output
/// across instances of *this* crate version when the caller's serde tree
/// satisfies all of:
/// - Map types are `BTreeMap` (NOT `HashMap`) — `serde`'s `BTreeMap`
///   serializer iterates in `BTreeMap` order, which is `K::Ord`
/// - All map keys at every nesting level have the same encoded length
///   (e.g., all-ULID, all-UUID, all-`u64`); see "Why same-length keys".
///   This applies to struct-as-map encoding too: a struct's field names
///   become CBOR map keys, so all field names of a single struct must
///   have the same encoded length. `#[serde(rename = "x")]` is the
///   conventional fix (see `Hlc` for an example: `wall_ms`/`logical`/
///   `device_id` → `w`/`l`/`d`)
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
pub fn canonical_cbor_encode<T: CanonicalPayload>(value: &T) -> Result<Vec<u8>, CryptoError> {
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
pub fn canonical_cbor_decode<T: CanonicalPayload + serde::de::DeserializeOwned>(
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 32-byte all-zeros master seed for deterministic test fixtures.
    const TEST_SEED: [u8; 32] = [0u8; 32];

    #[test]
    fn generate_at_epoch_is_random_and_material_round_trips() {
        let a = KeyTree::generate_at_epoch(3);
        let b = KeyTree::generate_at_epoch(3);
        assert_eq!(a.epoch, 3);
        // Two independent generations differ (not master-derived, no shared seed).
        assert_ne!(a.entry_aead.as_ref(), b.entry_aead.as_ref());
        assert_ne!(a.root_aead.as_ref(), b.root_aead.as_ref());
        // The five sub-keys are distinct within one tree.
        assert_ne!(a.entry_aead.as_ref(), a.root_aead.as_ref());
        assert_ne!(a.lookup.as_ref(), a.nonce.as_ref());
        // Material round-trips byte-identical through the sealed-distribution path.
        let m = a.to_fleet_material();
        let back = KeyTree::from_fleet_material(&m).expect("round-trip");
        assert_eq!(back.entry_aead.as_ref(), a.entry_aead.as_ref());
        assert_eq!(back.friend_aead.as_ref(), a.friend_aead.as_ref());
        assert_eq!(back.epoch, 3);
        // Distinct from a master-derived tree at the same epoch.
        let derived = KeyTree::derive_at_epoch(&TEST_SEED, 3).expect("derive");
        assert_ne!(a.root_aead.as_ref(), derived.root_aead.as_ref());
    }

    #[test]
    fn key_tree_derives_four_distinct_keys_deterministically() {
        let kt1 = KeyTree::derive(&TEST_SEED).expect("derive 1");
        let kt2 = KeyTree::derive(&TEST_SEED).expect("derive 2");

        // Same seed → same keys (every bound device computes the same tree).
        assert_eq!(kt1.entry_aead.as_ref(), kt2.entry_aead.as_ref());
        assert_eq!(kt1.root_aead.as_ref(), kt2.root_aead.as_ref());
        assert_eq!(kt1.lookup.as_ref(), kt2.lookup.as_ref());
        assert_eq!(kt1.nonce.as_ref(), kt2.nonce.as_ref());
        assert_eq!(kt1.friend_aead.as_ref(), kt2.friend_aead.as_ref());

        // The four keys must be distinct (domain separation).
        assert_ne!(kt1.entry_aead.as_ref(), kt1.root_aead.as_ref());
        assert_ne!(kt1.entry_aead.as_ref(), kt1.lookup.as_ref());
        assert_ne!(kt1.entry_aead.as_ref(), kt1.nonce.as_ref());
        assert_ne!(kt1.root_aead.as_ref(), kt1.lookup.as_ref());
        assert_ne!(kt1.root_aead.as_ref(), kt1.nonce.as_ref());
        assert_ne!(kt1.lookup.as_ref(), kt1.nonce.as_ref());
    }

    /// ZEB-668 S5: epoch-0 derivation is pinned byte-for-byte. These vectors
    /// were captured from the pre-S5 implementation (the fixed
    /// `harmony-owner-state-v1-epoch-0` HKDF salt) and MUST NEVER be
    /// regenerated — a mismatch means every existing fleet's keys broke.
    #[test]
    fn epoch0_derivation_matches_golden_vectors() {
        let seed = [7u8; 32];
        let kt = KeyTree::derive(&seed).expect("derive");
        assert_eq!(
            hex::encode(kt.entry_aead.as_ref()),
            "3433d952f6018567e5c317a2f11ae19aacee303d458f5ff36efdb9a87d1bfb69"
        );
        assert_eq!(
            hex::encode(kt.root_aead.as_ref()),
            "03af1ea8b6aa773c5d0c5522eed5d67a87e59406b3fc9596fefb5058db54faca"
        );
        assert_eq!(
            hex::encode(kt.lookup.as_ref()),
            "815e90de81cb899bb1dd2f3a2632c54612ff76d7d0ddaecf957ddb0c5eceb454"
        );
        assert_eq!(
            hex::encode(kt.nonce.as_ref()),
            "aba43be2af6d80a96b54cf3ad47baea7cb4dd90df35b9cfd7da35316e17c7dce"
        );
        assert_eq!(
            hex::encode(kt.friend_aead.as_ref()),
            "8a2d15a00d95c68bfa4e314f1acb80a348c1644769696371dddcf6247c020513"
        );
    }

    #[test]
    fn friend_secret_seal_round_trip() {
        let kt = KeyTree::derive(&TEST_SEED).expect("derive");
        let friend = [0x33u8; 16];
        let secret = [0xABu8; 32];
        let blob = encrypt_friend_secret(&kt, &friend, &secret).expect("seal");
        assert_eq!(blob.len(), 12 + 32 + 16);
        assert_ne!(
            &blob[12..44],
            &secret[..],
            "must be ciphertext, not plaintext"
        );
        let back = decrypt_friend_secret(&kt, &friend, &blob).expect("open");
        assert_eq!(back.as_ref(), &secret);
    }

    #[test]
    fn friend_secret_seal_binds_to_friend_owner_id() {
        let kt = KeyTree::derive(&TEST_SEED).expect("derive");
        let blob = encrypt_friend_secret(&kt, &[1u8; 16], &[7u8; 32]).expect("seal");
        assert!(matches!(
            decrypt_friend_secret(&kt, &[2u8; 16], &blob),
            Err(CryptoError::AeadDecrypt)
        ));
    }

    #[test]
    fn friend_aead_key_distinct_from_other_subkeys() {
        let kt = KeyTree::derive(&TEST_SEED).expect("derive");
        assert_ne!(kt.friend_aead.as_ref(), kt.entry_aead.as_ref());
        assert_ne!(kt.friend_aead.as_ref(), kt.root_aead.as_ref());
        assert_ne!(kt.friend_aead.as_ref(), kt.lookup.as_ref());
        assert_ne!(kt.friend_aead.as_ref(), kt.nonce.as_ref());
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

    #[test]
    fn canonical_cbor_byte_identical_for_same_value() {
        // The deterministic-encryption property of the CRDT relies on
        // byte-identical CBOR output across implementations and runs.
        // Use a Phase 2 type (Hlc) so the test reflects the actual
        // type universe under the sealed-trait constraint.
        let value = Hlc {
            wall_ms: 100,
            logical: 5,
            device_id: "alice".into(),
        };
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
    fn hlc_cbor_uses_single_char_field_names_per_canonical_precondition() {
        // The canonical_cbor_encode docstring requires "all map keys at every
        // nesting level have the same encoded length." Hlc's struct-as-map
        // CBOR form must therefore use single-char field names so all three
        // keys encode to identical 2-byte (1 length-prefix + 1 char) sequences.
        // Without this, Hlc itself would silently violate the precondition
        // every other type relies on.
        let value = Hlc {
            wall_ms: 7,
            logical: 0,
            device_id: "alice".into(),
        };
        let bytes = canonical_cbor_encode(&value).expect("encode");

        // CBOR layout we lock in for Hlc forever (changing it is a wire-format
        // break): map(3) { "w": 7, "l": 0, "d": "alice" }
        let expected: &[u8] = &[
            0xa3, // map of 3 pairs
            0x61, b'w', // text(1) "w"
            0x07, // unsigned 7
            0x61, b'l', // text(1) "l"
            0x00, // unsigned 0
            0x61, b'd', // text(1) "d"
            0x65, b'a', b'l', b'i', b'c', b'e', // text(5) "alice"
        ];
        assert_eq!(
            bytes, expected,
            "Hlc CBOR wire format drifted — bound devices will fail to decode"
        );

        // Round-trip still works.
        let recovered: Hlc = canonical_cbor_decode(&bytes).expect("decode");
        assert_eq!(value, recovered);
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

    /// Compile-time test: every Phase 2 wire type must implement
    /// `CanonicalPayload`. If a type is added to `owner_state_types`
    /// and its impl is forgotten, this test fails at compile time
    /// with "the trait bound `T: CanonicalPayload` is not satisfied".
    #[test]
    fn phase2_types_implement_canonical_payload() {
        use crate::owner_state_types::*;
        fn assert_canonical<T: CanonicalPayload>() {}
        assert_canonical::<Hlc>();
        assert_canonical::<SpaceId>();
        assert_canonical::<OwnerAddr>();
        assert_canonical::<ContentId>();
        assert_canonical::<OutboxEntryId>();
        assert_canonical::<DmContentKey>();
        assert_canonical::<DeviceIdentityHash>();
        assert_canonical::<OwnerDeviceCache>();
        assert_canonical::<OwnerDeviceEntry>();
        assert_canonical::<SpaceKind>();
        assert_canonical::<NotificationPref>();
        // ZEB-474: assert_canonical::<ReticulumDest>() removed — ReticulumDest
        // type was deleted (flag-day-for-alpha wire-format change).
        assert_canonical::<TransportBinding>();
        assert_canonical::<Space>();
        assert_canonical::<DedupeKey>();
        assert_canonical::<DeliveryStatus>();
        assert_canonical::<OutboxEntry>();
        assert_canonical::<InboxKey>();
        assert_canonical::<InboxEntry>();
        assert_canonical::<ReadMarker>();
        // dm_envelope wire types (ZEB-216 Sub-B Phase 1) live in harmony-app;
        // their compile-time CanonicalPayload assertion moved there with the
        // crate split (ZEB-548 Stage 0) — see harmony-app's canonical_impls.rs.
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

    #[test]
    fn fleet_material_round_trips_to_identical_keytree() {
        let seed = [0x42u8; 32];
        let kt = KeyTree::derive(&seed).unwrap();
        let material = kt.to_fleet_material();
        assert_eq!(material.epoch, 0);
        let kt2 = KeyTree::from_fleet_material(&material).unwrap();
        let lk1 = space_lookup_key(&kt, b"notes-v1");
        let lk2 = space_lookup_key(&kt2, b"notes-v1");
        assert_eq!(lk1.as_slice(), lk2.as_slice());
        let lk = space_lookup_key(&kt, b"dm-inbox-v1");
        let ct = encrypt_entry(&kt, &lk, b"hello fleet").unwrap();
        let pt = decrypt_entry(&kt2, &lk, &ct).unwrap();
        assert_eq!(pt, b"hello fleet");
        let fid = [9u8; 16];
        let secret = [3u8; 32];
        let sealed = encrypt_friend_secret(&kt, &fid, &secret).unwrap();
        let opened = decrypt_friend_secret(&kt2, &fid, &sealed).unwrap();
        assert_eq!(opened.as_slice(), &secret);
        // root_aead is the only sub-key not exercised by the entry/friend
        // paths above; round-trip it source(kt)->target(kt2) too so a
        // dropped/swapped/corrupted root_aead in to/from_fleet_material is
        // detectable. Brings all 5 sub-keys under source->target validation.
        let root_blob = encrypt_root_publish(&kt, b"root pointer").unwrap();
        let root_pt = decrypt_root_publish(&kt2, &root_blob).unwrap();
        assert_eq!(root_pt, b"root pointer");
    }

    #[test]
    fn fleet_material_cbor_round_trips() {
        let kt = KeyTree::derive(&[7u8; 32]).unwrap();
        let m = kt.to_fleet_material();
        let mut buf = Vec::new();
        ciborium::into_writer(&m, &mut buf).unwrap();
        let back: FleetKeyMaterial = ciborium::from_reader(buf.as_slice()).unwrap();
        let kt_back = KeyTree::from_fleet_material(&back).unwrap();
        let lk = space_lookup_key(&kt, b"x");
        let ct = encrypt_entry(&kt, &lk, b"z").unwrap();
        assert_eq!(decrypt_entry(&kt_back, &lk, &ct).unwrap(), b"z");
    }

    #[test]
    fn fleet_material_from_different_seed_cannot_decrypt() {
        let kt_a = KeyTree::derive(&[1u8; 32]).unwrap();
        let kt_b = KeyTree::derive(&[2u8; 32]).unwrap();
        let lk = space_lookup_key(&kt_a, b"notes-v1");
        let ct = encrypt_entry(&kt_a, &lk, b"secret").unwrap();
        let kt_b2 = KeyTree::from_fleet_material(&kt_b.to_fleet_material()).unwrap();
        let lk_b = space_lookup_key(&kt_b2, b"notes-v1");
        assert!(decrypt_entry(&kt_b2, &lk_b, &ct).is_err());
    }

    /// ZEB-492 (Qodo/CodeAnt round 1, FIX E): material from an unsupported
    /// (future) epoch must be REJECTED at the cert-only boot boundary, not
    /// silently reconstructed. Only epoch 0 is supported today (rotation is out
    /// of scope).
    #[test]
    fn epoch_salt_zero_matches_legacy_constant() {
        // ZEB-668 S5: the pre-S5 salt was the fixed constant below. Epoch 0
        // must keep producing byte-identical salts (and therefore keys — see
        // `epoch0_derivation_matches_golden_vectors`).
        assert_eq!(epoch_salt(0).as_slice(), b"harmony-owner-state-v1-epoch-0");
    }

    #[test]
    fn distinct_epochs_derive_pairwise_distinct_keys() {
        let seed = [7u8; 32];
        let e0 = KeyTree::derive_at_epoch(&seed, 0).expect("e0");
        let e1 = KeyTree::derive_at_epoch(&seed, 1).expect("e1");
        assert_eq!(e0.epoch, 0);
        assert_eq!(e1.epoch, 1);
        assert_ne!(e0.entry_aead.as_ref(), e1.entry_aead.as_ref());
        assert_ne!(e0.root_aead.as_ref(), e1.root_aead.as_ref());
        assert_ne!(e0.lookup.as_ref(), e1.lookup.as_ref());
        assert_ne!(e0.nonce.as_ref(), e1.nonce.as_ref());
        assert_ne!(e0.friend_aead.as_ref(), e1.friend_aead.as_ref());
    }

    fn arc_tree(epoch: u32) -> std::sync::Arc<KeyTree> {
        std::sync::Arc::new(KeyTree::derive_at_epoch(&[5u8; 32], epoch).expect("derive"))
    }

    #[test]
    fn fleet_material_set_round_trips_and_legacy_single_decodes() {
        let kt0 = KeyTree::derive_at_epoch(&[5u8; 32], 0).expect("e0");
        let kt2 = KeyTree::derive_at_epoch(&[5u8; 32], 2).expect("e2");
        let set = vec![kt0.to_fleet_material(), kt2.to_fleet_material()];
        let bytes = encode_fleet_material_set(&set).expect("encode");
        let back = decode_fleet_material_set(&bytes).expect("decode set");
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].epoch, 0);
        assert_eq!(back[1].epoch, 2);

        // Legacy pre-S5 slot: a single bare FleetKeyMaterial map.
        let mut legacy = Vec::new();
        ciborium::into_writer(&kt0.to_fleet_material(), &mut legacy).expect("legacy encode");
        let migrated = decode_fleet_material_set(&legacy).expect("decode legacy");
        assert_eq!(migrated.len(), 1);
        assert_eq!(migrated[0].epoch, 0);

        // Empty sets are corrupt, not "no material".
        let mut empty = Vec::new();
        ciborium::into_writer(&Vec::<FleetKeyMaterial>::new(), &mut empty).expect("empty encode");
        assert!(decode_fleet_material_set(&empty).is_err());
    }

    #[test]
    fn fleet_key_set_newest_picks_highest_epoch() {
        let set = FleetKeySet::new(arc_tree(1));
        set.install(arc_tree(0));
        set.install(arc_tree(3));
        assert_eq!(set.newest().epoch, 3);
        assert_eq!(set.epochs(), vec![3, 1, 0]);
    }

    #[test]
    fn fleet_key_set_install_is_idempotent_by_epoch() {
        let set = FleetKeySet::new(arc_tree(2));
        set.install(arc_tree(2));
        set.install(arc_tree(2));
        assert_eq!(set.epochs(), vec![2]);
        assert_eq!(set.accept_set().len(), 1);
    }

    #[test]
    fn fleet_key_set_accept_set_is_descending() {
        let set = FleetKeySet::new(arc_tree(0));
        set.install(arc_tree(2));
        set.install(arc_tree(1));
        let epochs: Vec<u32> = set.accept_set().iter().map(|k| k.epoch).collect();
        assert_eq!(epochs, vec![2, 1, 0]);
    }

    #[test]
    fn fleet_key_set_retain_min_epoch_evicts_stale_but_never_empties() {
        let set = FleetKeySet::new(arc_tree(0));
        set.install(arc_tree(3));
        set.install(arc_tree(4));
        set.retain_min_epoch(3);
        assert_eq!(
            set.epochs(),
            vec![4, 3],
            "epoch 0 evicted, window pair kept"
        );
        // Overshooting min clamps to the newest epoch instead of emptying.
        set.retain_min_epoch(99);
        assert_eq!(set.epochs(), vec![4]);
    }

    #[test]
    fn fleet_key_set_retain_newest_only_keeps_one() {
        let set = FleetKeySet::new(arc_tree(0));
        set.install(arc_tree(1));
        set.retain_newest_only();
        assert_eq!(set.epochs(), vec![1]);
        assert_eq!(set.newest().epoch, 1);
    }

    #[test]
    fn fleet_material_any_epoch_round_trips() {
        // ZEB-668 S5 replaces `fleet_material_unsupported_epoch_rejected`:
        // rotation-era material carries the epoch it was derived at, and the
        // import side reconstructs a tree at that same epoch.
        let seed = [9u8; 32];
        let kt = KeyTree::derive_at_epoch(&seed, 3).expect("derive");
        let m = kt.to_fleet_material();
        assert_eq!(m.epoch, 3);
        let back = KeyTree::from_fleet_material(&m).expect("epoch 3 must import post-S5");
        assert_eq!(back.epoch, 3);
        assert_eq!(back.entry_aead.as_ref(), kt.entry_aead.as_ref());
        assert_eq!(back.friend_aead.as_ref(), kt.friend_aead.as_ref());
    }

    // ── ZEB-981: dataset-file at-rest envelope ───────────────────────────────

    #[test]
    fn dataset_file_seal_open_round_trip() {
        let keys = KeyTree::derive(&[7u8; 32]).unwrap();
        let sealed = seal_dataset_file(&keys, "notes.cbor", b"hello dataset").unwrap();
        assert_eq!(sealed.len(), 12 + 13 + 16, "nonce + ct + tag");
        let opened = open_dataset_file(&keys, "notes.cbor", &sealed).unwrap();
        assert_eq!(&opened[..], b"hello dataset");
    }

    #[test]
    fn dataset_file_label_swap_fails_tag() {
        let keys = KeyTree::derive(&[7u8; 32]).unwrap();
        let sealed = seal_dataset_file(&keys, "contacts.cbor", b"x").unwrap();
        assert!(open_dataset_file(&keys, "notes.cbor", &sealed).is_err());
    }

    #[test]
    fn dataset_file_tamper_and_foreign_key_fail() {
        let keys = KeyTree::derive(&[7u8; 32]).unwrap();
        let mut sealed = seal_dataset_file(&keys, "notes.cbor", b"x").unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 0x01;
        assert!(open_dataset_file(&keys, "notes.cbor", &sealed).is_err());
        let sealed_ok = seal_dataset_file(&keys, "notes.cbor", b"x").unwrap();
        let other = KeyTree::derive(&[8u8; 32]).unwrap();
        assert!(open_dataset_file(&other, "notes.cbor", &sealed_ok).is_err());
    }

    #[test]
    fn dataset_file_truncated_blob_fails_cleanly() {
        let keys = KeyTree::derive(&[7u8; 32]).unwrap();
        assert!(open_dataset_file(&keys, "notes.cbor", &[]).is_err());
        assert!(open_dataset_file(&keys, "notes.cbor", &[0u8; 11]).is_err());
    }

    // ── ZEB-982: device-sealed at-rest envelope ──────────────────────────────

    #[test]
    fn device_dataset_key_derivation_golden_pin() {
        // Changing DEVICE_DATASET_SALT or INFO_DEVICE_DATASET_AEAD strands
        // every sealed file on disk — this pin makes any drift loud.
        let key = derive_device_dataset_key(&[7u8; 32]).unwrap();
        let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex, "7f58aa4a60e8c386d2098a08589a17ee171745f2bb8f53f3d1ebd3c0360dd11a",
            "device dataset key derivation drifted"
        );
    }

    #[test]
    fn mint_ledger_key_derivation_golden_pin() {
        // ZEB-985: changing DEVICE_DATASET_SALT or INFO_MINT_LEDGER_SQLCIPHER
        // strands every encrypted ledger.db on disk — this pin makes any
        // drift loud. The derivation must also never collide with the
        // device dataset AEAD key (SQLCipher consumes the raw bytes).
        let key = derive_mint_ledger_key(&[7u8; 32]).unwrap();
        let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex, "0b900cb623591506c29170bb705a7e77392753df10ec9af4ab4290487832221c",
            "mint ledger key derivation drifted"
        );
        let device = derive_device_dataset_key(&[7u8; 32]).unwrap();
        assert_ne!(
            &*key, &*device,
            "mint ledger key must not equal device dataset key"
        );
    }

    #[test]
    fn device_file_seal_open_round_trip() {
        let key = derive_device_dataset_key(&[7u8; 32]).unwrap();
        let sealed = seal_device_file(&key, "owner_state.cbor", b"hello device").unwrap();
        assert_eq!(sealed.len(), 12 + 12 + 16, "nonce + ct + tag");
        let opened = open_device_file(&key, "owner_state.cbor", &sealed).unwrap();
        assert_eq!(&opened[..], b"hello device");
    }

    #[test]
    fn device_file_label_swap_fails_tag() {
        let key = derive_device_dataset_key(&[7u8; 32]).unwrap();
        let sealed = seal_device_file(&key, "owner_state_crdt.cbor", b"x").unwrap();
        assert!(open_device_file(&key, "state_root_replay.cbor", &sealed).is_err());
    }

    #[test]
    fn device_file_tamper_and_foreign_key_fail() {
        let key = derive_device_dataset_key(&[7u8; 32]).unwrap();
        let mut sealed = seal_device_file(&key, "owner_state.cbor", b"x").unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 0x01;
        assert!(open_device_file(&key, "owner_state.cbor", &sealed).is_err());
        let sealed_ok = seal_device_file(&key, "owner_state.cbor", b"x").unwrap();
        let other = derive_device_dataset_key(&[8u8; 32]).unwrap();
        assert!(open_device_file(&other, "owner_state.cbor", &sealed_ok).is_err());
    }

    #[test]
    fn device_file_truncated_blob_fails_cleanly() {
        let key = derive_device_dataset_key(&[7u8; 32]).unwrap();
        assert!(open_device_file(&key, "owner_state.cbor", &[]).is_err());
        assert!(open_device_file(&key, "owner_state.cbor", &[0u8; 11]).is_err());
    }

    #[test]
    fn device_and_dataset_domains_never_cross_open() {
        // Even with byte-identical key material, the AAD domains keep the
        // ZEB-981 fleet envelope and the ZEB-982 device envelope disjoint.
        let kt = KeyTree::derive(&[7u8; 32]).unwrap();
        let raw: [u8; 32] = *kt.friend_aead;
        let fleet_sealed = seal_dataset_file(&kt, "notes.cbor", b"x").unwrap();
        assert!(open_device_file(&raw, "notes.cbor", &fleet_sealed).is_err());
        let dev_sealed = seal_device_file(&raw, "notes.cbor", b"x").unwrap();
        assert!(open_dataset_file(&kt, "notes.cbor", &dev_sealed).is_err());
    }
}
