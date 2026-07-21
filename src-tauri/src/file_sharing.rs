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

use crate::dm_signing::{open_from_owner_with_info, seal_to_owner_with_info, DmSignError};
use crate::owner_state_crdt::OwnerState;
use crate::owner_state_crypto::sealed::CanonicalPayloadSealed;
use crate::owner_state_crypto::{
    canonical_cbor_decode, canonical_cbor_encode, decrypt_file_dek, encrypt_file_dek,
    CanonicalPayload, CryptoError, KeyTree,
};
use crate::owner_state_types::{EpochKey, OwnerAddr, ReceivedFileGrant};
use harmony_content::cid::ContentId;
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
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
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

/// Redacting `Debug` (mirrors `EpochKey`'s custom impl): the non-secret
/// metadata fields print for diagnostics, but the raw DEK is NEVER emitted —
/// so a stray `{:?}` on a `FileGrantInner` (log line, panic message) can't leak
/// key material.
impl std::fmt::Debug for FileGrantInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileGrantInner")
            .field("cid", &self.cid)
            .field("file_name", &self.file_name)
            .field("file_size", &self.file_size)
            .field("mime", &self.mime)
            .field("dek", &"<redacted>")
            .finish()
    }
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

/// Wall-clock now in epoch-ms — the `received_at` stamp on an ingested grant.
fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Failure opening or decoding a received file grant (grantee read path, C4).
#[derive(Debug, thiserror::Error)]
pub enum FileGrantIngestError {
    /// The outer `grant_push` wire value (`Vec<serde_bytes Vec<u8>>`) did not
    /// decode — malformed / truncated CBOR.
    #[error("grant_push wire decode failed: {0}")]
    WireDecode(String),
    /// A blob opened with our device key but its plaintext was not a canonical
    /// [`FileGrantInner`] — real corruption, NOT a wrong-recipient miss.
    #[error("canonical CBOR decode of FileGrantInner failed: {0}")]
    Decode(#[from] CryptoError),
    /// No received grant recorded for the requested cid.
    #[error("no received grant for the requested cid")]
    NotFound,
    /// Opening the stored sealed DEK with this device's X25519 key failed
    /// (wrong device / tampered blob).
    #[error("opening the sealed grant failed: {0}")]
    Open(#[from] DmSignError),
}

/// Grantee ingest (C4): apply an inbound `grant_push` payload to `state`.
///
/// `grant_push_bytes` is the wire value of `DepositPayload.grant_push` — CBOR
/// of `Vec<serde_bytes Vec<u8>>`, the per-device sealed blobs produced by
/// [`seal_grant_for_devices`]. Each blob is tried against THIS device's X25519
/// private key via `open_from_owner_with_info` under [`FILE_GRANT_SEAL_INFO`];
/// on the FIRST blob that opens, the inner [`FileGrantInner`] is parsed and
/// recorded on `state.received_file_grants[cid]` — storing the MATCHED sealed
/// blob verbatim as `sealed_dek`, so the DEK stays sealed at rest and re-opens
/// lazily via [`open_received_file`]. Returns `Ok(Some(cid))`.
///
/// If NO blob opens with this device's key (the grant was sealed only to other
/// devices — the honest new-device edge), returns `Ok(None)` and leaves `state`
/// untouched. A blob that opens but does not parse is `Err(Decode)`.
///
/// `granter_owner` is the AUTHENTICATED deposit sender (the butler-verified
/// frame `sender_owner`), passed in by the recover-path demux — it is NOT read
/// from the sealed payload, which the seal does not authenticate (anyone can
/// seal to a recipient's pubkey), so trusting a payload-claimed granter would
/// let a depositor forge attribution.
pub fn ingest_grant_push(
    state: &mut OwnerState,
    my_device_x25519_priv: &[u8; 32],
    granter_owner: OwnerAddr,
    grant_push_bytes: &[u8],
) -> Result<Option<ContentId>, FileGrantIngestError> {
    // The outer list rides as CBOR byte-strings (`Vec<serde_bytes Vec<u8>>`,
    // per `DepositPayload.grant_push`); decode with the matching `ByteBuf`
    // element type rather than a plain `Vec<Vec<u8>>` (which would expect
    // array-of-int elements).
    let blobs: Vec<serde_bytes::ByteBuf> = ciborium::from_reader(grant_push_bytes)
        .map_err(|e| FileGrantIngestError::WireDecode(e.to_string()))?;
    for blob in blobs {
        let Ok(plaintext) =
            open_from_owner_with_info(my_device_x25519_priv, blob.as_ref(), FILE_GRANT_SEAL_INFO)
        else {
            // Not sealed to us — try the next device blob.
            continue;
        };
        // Opened with our key — this blob is ours. A parse failure now is
        // corruption of an authenticated-to-us payload, so it propagates.
        let inner: FileGrantInner = canonical_cbor_decode(&plaintext)?;
        let cid = ContentId::from_bytes(inner.cid);
        state.received_file_grants.insert(
            inner.cid,
            ReceivedFileGrant {
                granter_owner,
                cid: inner.cid,
                file_name: inner.file_name,
                file_size: inner.file_size,
                mime: inner.mime,
                sealed_dek: blob.into_vec(),
                received_at: now_epoch_ms(),
            },
        );
        return Ok(Some(cid));
    }
    Ok(None)
}

/// Grantee read (C4): recover the per-file DEK for a previously-ingested grant.
///
/// Looks up `state.received_file_grants[cid]`, re-opens its `sealed_dek` with
/// this device's X25519 private key under [`FILE_GRANT_SEAL_INFO`], parses the
/// inner [`FileGrantInner`], and returns its DEK as an [`EpochKey`] ready for
/// `community_state_sync::decrypt_blob`. `Err(NotFound)` if no grant is
/// recorded for `cid`; `Err(Open)` if this device did not hold the sealing key.
pub fn open_received_file(
    state: &OwnerState,
    my_device_x25519_priv: &[u8; 32],
    cid: ContentId,
) -> Result<EpochKey, FileGrantIngestError> {
    let grant = state
        .received_file_grants
        .get(&cid.to_bytes())
        .ok_or(FileGrantIngestError::NotFound)?;
    let plaintext = open_from_owner_with_info(
        my_device_x25519_priv,
        &grant.sealed_dek,
        FILE_GRANT_SEAL_INFO,
    )?;
    let inner: FileGrantInner = canonical_cbor_decode(&plaintext)?;
    Ok(EpochKey::new(inner.dek))
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
