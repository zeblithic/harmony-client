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
use crate::owner_state_types::{EpochKey, GrantEntry, OwnerAddr, ReceivedFileGrant};
use harmony_content::cid::ContentId;
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

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
    /// Stored (CAS) byte length of the file's content. For v3 streaming-encrypted
    /// content this is the chunked-AEAD ciphertext length: a 9-byte header plus,
    /// per 64 KiB frame, a 16-byte tag (see `file_stream_crypto::v3_ciphertext_len`),
    /// so it exceeds the plaintext length by the header + per-frame tag overhead.
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

/// Wall-clock now in epoch-ms — the `received_at` stamp on an ingested grant
/// (also the `granted_at` / deposit `now_ms` stamp on the owner-side share path
/// in `lib.rs`).
pub(crate) fn now_epoch_ms() -> u64 {
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
    /// A crypto step on an authenticated-to-us grant failed: either a blob
    /// opened with our device key but its plaintext was not a canonical
    /// [`FileGrantInner`] (real corruption, NOT a wrong-recipient miss), the
    /// re-seal of the recovered DEK under the shared KeyTree failed, or an
    /// at-rest unseal in [`open_received_file`] failed (wrong KeyTree / tampered
    /// blob).
    #[error("crypto on a received grant failed: {0}")]
    Decode(#[from] CryptoError),
    /// No received grant recorded for the requested cid.
    #[error("no received grant for the requested cid")]
    NotFound,
}

/// Grantee ingest (C4): apply an inbound `grant_push` payload to `state`.
///
/// `grant_push_bytes` is the wire value of `DepositPayload.grant_push` — CBOR
/// of `Vec<serde_bytes Vec<u8>>`, the per-device sealed blobs produced by
/// [`seal_grant_for_devices`]. Each blob is tried against THIS device's X25519
/// private key via `open_from_owner_with_info` under [`FILE_GRANT_SEAL_INFO`];
/// on the FIRST blob that opens, the inner [`FileGrantInner`] is parsed, the DEK
/// is RE-SEALED under the grantee's own shared `keytree` via
/// [`seal_dek_at_rest`], and THAT at-rest blob is stored as
/// `state.received_file_grants[cid].sealed_dek`. Returns `Ok(Some(cid))`.
///
/// Re-sealing under the shared KeyTree (rather than storing the opened
/// per-device envelope verbatim) makes the stored grant openable by ANY of the
/// grantee's bound devices — exactly like `file_deks` — because
/// `received_file_grants` replicates fleet-wide via Flow A but the per-device
/// seal only opens on the one device it was sealed to. The DEK still never lands
/// unsealed in `OwnerState`; confidentiality now rests on the grantee's shared
/// KeyTree instead of a single device key. [`open_received_file`] recovers it.
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
    keytree: &KeyTree,
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
        let mut inner: FileGrantInner = canonical_cbor_decode(&plaintext)?;
        let cid = ContentId::from_bytes(inner.cid);
        // Re-seal the recovered DEK under the grantee's OWN shared KeyTree so
        // any of the grantee's bound devices can open the stored grant (Flow A),
        // not only the device this per-device envelope was sealed to. Mirrors
        // `file_deks`. The `EpochKey` copy is `ZeroizeOnDrop`; the raw
        // `inner.dek` copy is not, so scrub it as soon as the seal has consumed
        // it (defense-in-depth — don't let the plaintext key linger, on the
        // seal-error path too).
        let sealed_dek = seal_dek_at_rest(keytree, &EpochKey::new(inner.dek));
        inner.dek.zeroize();
        let sealed_dek = sealed_dek?;
        state.received_file_grants.insert(
            inner.cid,
            ReceivedFileGrant {
                granter_owner,
                cid: inner.cid,
                file_name: inner.file_name,
                file_size: inner.file_size,
                mime: inner.mime,
                sealed_dek,
                received_at: now_epoch_ms(),
            },
        );
        return Ok(Some(cid));
    }
    Ok(None)
}

/// Grantee read (C4): recover the per-file DEK for a previously-ingested grant.
///
/// Looks up `state.received_file_grants[cid]` and unseals its `sealed_dek` with
/// the grantee's shared `keytree` via [`open_dek_at_rest`], returning the DEK as
/// an [`EpochKey`] ready for `community_state_sync::decrypt_blob`. Because the
/// grant was re-sealed under the SHARED KeyTree at ingest (see
/// [`ingest_grant_push`]), ANY of the grantee's bound devices opens it — not
/// only the device the deposit was sealed to. `Err(NotFound)` if no grant is
/// recorded for `cid`; `Err(Decode)` if the at-rest unseal fails (wrong KeyTree
/// / tampered blob).
pub fn open_received_file(
    state: &OwnerState,
    keytree: &KeyTree,
    cid: ContentId,
) -> Result<EpochKey, FileGrantIngestError> {
    let grant = state
        .received_file_grants
        .get(&cid.to_bytes())
        .ok_or(FileGrantIngestError::NotFound)?;
    Ok(open_dek_at_rest(keytree, &grant.sealed_dek)?)
}

// ─────────────────────────────────────────────────────────────────────────
// Task 6 (C2/C5): owner-side share orchestration cores.
//
// These are the PURE, NodeState-free halves of the `grant_read` / `revoke_read`
// / `list_grants` IPC commands (the async orchestration + Tauri wrappers live in
// `lib.rs`). Kept here beside the seal/ingest primitives so the honesty gates,
// the per-device seal, and the DTO projection are unit-testable over a bare
// `OwnerState` + `KeyTree` (no keychain, no Tauri) — mirroring the grantee-side
// tests in `tests/file_sharing_grantee.rs`.
// ─────────────────────────────────────────────────────────────────────────

/// Stable `ineligible:`-prefixed rejection: a PUBLIC (unencrypted) file has no
/// per-file DEK, so it cannot be per-viewer shared (a public CID cannot be
/// retro-ACLed — design constraint 2). The frontend switches on the `ineligible:`
/// prefix (`FileDetailPanel.svelte`).
pub const INELIGIBLE_PUBLIC: &str = "ineligible: only encrypted files can be shared";

/// Stable `ineligible:`-prefixed rejection: a grant rides the friend content
/// transport, so the grantee must be an ACTIVE friend.
pub const INELIGIBLE_NON_FRIEND: &str = "ineligible: can only share with friends";

/// Stable `ineligible:`-prefixed rejection: the grantee has ZERO resolvable
/// device X25519 keys (`grantee_device_x25519s` returned empty — their
/// device-identity-pubs haven't propagated to this owner's fleet yet). There
/// is no device to seal the grant to, so recording the grant now would let
/// the owner's ShareList claim "shared" (with a `grantedAt`) for a delivery
/// the backend cannot make — the render-only-what-you-can-deliver rule this
/// feature is built on. A later re-share reaches the device once its key is
/// learned.
pub const INELIGIBLE_NO_DEVICES: &str = "ineligible: recipient's devices not yet reachable";

/// The built, ready-to-deposit outcome of [`build_grant_push`] paired with the
/// resolved grantee — handed from the Phase-1 IPC core to the Phase-2 deposit
/// send so the send rung (`lib.rs`) delivers exactly these bytes to exactly this
/// owner's butler set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantSendPlan {
    /// The grantee's master `OwnerAddr` (deposit recipient).
    pub grantee_owner: OwnerAddr,
    /// The `DepositPayload.grant_push` wire value (canonical CBOR of the
    /// per-device sealed blobs). [`build_grant_push`] never returns an
    /// empty-list `grant_push` — a grantee with no known device X25519s is
    /// rejected with [`INELIGIBLE_NO_DEVICES`] before this is built.
    pub grant_push: Vec<u8>,
}

/// One row of the owner's "Shared with" list, projected for the frontend. Serde
/// camelCase: `granteeAddress` / `displayName` / `grantedAt`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileGrantDto {
    /// The grantee's 16-byte master `owner_id`, hex-encoded.
    pub grantee_address: String,
    /// The grantee's friend-graph display name; `None` when the grantee is not a
    /// currently-known friend (e.g. an old grant to a since-unfriended peer).
    pub display_name: Option<String>,
    /// Wall-clock ms when the grant was recorded.
    pub granted_at: u64,
}

/// One row of the grantee's "Shared with me" list, projected for the frontend
/// from [`crate::owner_state_types::ReceivedFileGrant`]. Serde camelCase:
/// `cid` / `granterAddress` / `displayName` / `fileName` / `fileSize` / `mime` /
/// `receivedAt`. NOT a canonical-CBOR wire type — never `canonical_cbor_encode`d,
/// so the same-length-key rule does not apply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReceivedGrantDto {
    /// The shared file's encrypted root CID, hex-encoded.
    pub cid: String,
    /// The granting owner's 16-byte master `owner_id`, hex-encoded.
    pub granter_address: String,
    /// The granter's friend-graph display name; `None` when the granter is not a
    /// currently-known friend (frontend falls back to `granter_address`).
    pub display_name: Option<String>,
    /// Display file name.
    pub file_name: String,
    /// Stored (CAS) byte length of the file's content. For v3 streaming-encrypted
    /// content this is the chunked-AEAD ciphertext length: a 9-byte header plus,
    /// per 64 KiB frame, a 16-byte tag (see `file_stream_crypto::v3_ciphertext_len`),
    /// so it exceeds the plaintext length by the header + per-frame tag overhead;
    /// the UI shows it as-is.
    pub file_size: u64,
    /// MIME type string.
    pub mime: String,
    /// Wall-clock ms when this grant was ingested.
    pub received_at: u64,
}

/// Project `state.received_file_grants` into DTO rows for the grantee's
/// "Shared with me" surface, newest first. The granter's `display_name` is
/// resolved from the friend graph exactly as [`list_grants_inner`] resolves the
/// grantee's (`None` when the granter is not a currently-known friend). Pure;
/// unit-testable without a live node.
pub fn list_received_grants_inner(state: &OwnerState) -> Vec<ReceivedGrantDto> {
    let mut rows: Vec<ReceivedGrantDto> = state
        .received_file_grants
        .values()
        // ZEB-727: hide dismissed grants — active iff `received_at > dismissed_at`
        // (LWW; a legitimate re-share with a fresher `received_at` reappears).
        // Belt-and-suspenders with the merge sweep: keeps the projection honest
        // even for a grant a not-yet-swept local snapshot still holds.
        .filter(|g| match state.dismissed_received_grants.get(&g.cid) {
            Some(&dismissed_at) => g.received_at > dismissed_at,
            None => true,
        })
        .map(|g| ReceivedGrantDto {
            cid: hex::encode(g.cid),
            granter_address: hex::encode(g.granter_owner.0),
            display_name: state
                .friend_graph
                .friends
                .get(&g.granter_owner)
                .and_then(|f| f.display.clone()),
            file_name: g.file_name.clone(),
            file_size: g.file_size,
            mime: g.mime.clone(),
            received_at: g.received_at,
        })
        .collect();
    // Newest first; tie-break on cid for a deterministic order.
    rows.sort_by(|a, b| {
        b.received_at
            .cmp(&a.received_at)
            .then_with(|| a.cid.cmp(&b.cid))
    });
    rows
}

/// ZEB-727: grantee-local "dismiss this shared-with-me entry". Removes the
/// `received_file_grants` entry AND records an LWW tombstone
/// (`dismissed_received_grants[cid] = max(existing, now_ms)`) so the removal
/// CONVERGES across the grantee's own bound devices — a plain `remove` would be
/// resurrected by the add-wins union merge (`merge_remote_into_local`). Returns
/// `true` iff state changed (the entry was present, or the tombstone advanced);
/// the caller uses this to gate `notify_dirty` + the `shared-with-me-updated`
/// event.
///
/// Recording the tombstone even when the local entry is already gone is
/// intentional: a sibling device may still hold the grant, and the tombstone is
/// what suppresses it there on the next merge. Dismiss is REVERSIBLE — a later
/// owner re-share ingests with a fresh `received_at > dismissed_at` (the shared
/// file's root CID is stable) and reappears, so the tombstone is timestamped,
/// not permanent. Pure; unit-testable without a live node.
pub fn dismiss_received_grant_inner(state: &mut OwnerState, cid: [u8; 32], now_ms: u64) -> bool {
    let removed = state.received_file_grants.remove(&cid).is_some();
    let slot = state.dismissed_received_grants.entry(cid).or_insert(0);
    let prev = *slot;
    *slot = (*slot).max(now_ms);
    removed || *slot != prev
}

/// Encode per-device sealed grant blobs into the `DepositPayload.grant_push`
/// wire value: CBOR of `Vec<serde_bytes Vec<u8>>` (each element a byte-string) —
/// the EXACT shape [`ingest_grant_push`] decodes. Infallible: a `Vec<ByteBuf>`
/// always serializes into an in-memory `Vec`.
pub fn encode_grant_push(sealed_blobs: &[Vec<u8>]) -> Vec<u8> {
    let list: Vec<serde_bytes::ByteBuf> = sealed_blobs
        .iter()
        .cloned()
        .map(serde_bytes::ByteBuf::from)
        .collect();
    let mut bytes = Vec::new();
    ciborium::into_writer(&list, &mut bytes)
        .expect("CBOR encode of Vec<ByteBuf> into a Vec is infallible");
    bytes
}

/// Validate a share request and build the per-device-sealed `grant_push` wire
/// value WITHOUT mutating `state`, allowlisting, or touching the network (Phase
/// 1 IPC core — pure over `&OwnerState`).
///
/// Enforces THREE honesty gates in order — the file must have been encrypted
/// at ingest ([`INELIGIBLE_PUBLIC`] when `file_deks` lacks `cid`), the grantee
/// must be an ACTIVE friend ([`INELIGIBLE_NON_FRIEND`]), and the grantee must
/// have at least one resolvable device X25519 key
/// ([`INELIGIBLE_NO_DEVICES`] when [`grantee_device_x25519s`] returns empty —
/// their device-identity-pubs haven't propagated to this owner's fleet yet)
/// — then unseals the owner's at-rest DEK, wraps `{cid, file_meta, dek}`
/// sealed per grantee device ([`seal_grant_for_devices`]), and returns the
/// CBOR `grant_push` bytes.
///
/// The no-devices gate matters because the caller (`grant_read_impl`) records
/// the `GrantEntry` / allowlists the CID / emits `grants-updated` only AFTER
/// this returns `Ok` — an empty-list `grant_push` would let those all fire
/// for a grant with nothing to open, leaving the owner's ShareList claiming
/// "shared" for a delivery the backend cannot make. A later re-share reaches
/// the device once its key is learned.
pub fn build_grant_push(
    state: &OwnerState,
    keytree: &KeyTree,
    cid: [u8; 32],
    grantee_owner: OwnerAddr,
    file_name: String,
    file_size: u64,
    mime: String,
) -> Result<Vec<u8>, String> {
    // Gate 1 — only an encrypted-at-ingest file has a DEK to share.
    let Some(sealed_dek) = state.file_deks.get(&cid) else {
        return Err(INELIGIBLE_PUBLIC.to_string());
    };
    // Gate 2 — grants deliver over the friend transport; grantee must be Active.
    let is_active_friend = matches!(
        state.friend_graph.friends.get(&grantee_owner),
        Some(f) if f.status == crate::friend_graph::FriendStatus::Active
    );
    if !is_active_friend {
        return Err(INELIGIBLE_NON_FRIEND.to_string());
    }
    // Unseal the owner's at-rest DEK and wrap it per grantee device.
    let dek = open_dek_at_rest(keytree, sealed_dek).map_err(|e| format!("unseal DEK: {e:?}"))?;
    let mut inner = FileGrantInner {
        cid,
        file_name,
        file_size,
        mime,
        dek: *dek.as_bytes(),
    };
    let devices = grantee_device_x25519s(state, grantee_owner);
    // Gate 3 — nothing to open yet. Reject rather than emit an empty
    // grant_push: recording/allowlisting/emitting for a grant the grantee
    // cannot decrypt would be a completed-grant claim the backend cannot
    // deliver (ZEB-674 T6 review fix). Scrub the raw DEK copy before bailing.
    if devices.is_empty() {
        inner.dek.zeroize();
        return Err(INELIGIBLE_NO_DEVICES.to_string());
    }
    // The per-device seals have consumed the DEK; scrub the raw `inner.dek`
    // copy (the `dek` `EpochKey` is `ZeroizeOnDrop`, this plain `[u8; 32]` is
    // not) before returning — on the seal-error path too.
    let sealed = seal_grant_for_devices(&inner, &devices).map_err(|e| format!("seal grant: {e}"));
    inner.dek.zeroize();
    Ok(encode_grant_push(&sealed?))
}

/// Record an owner-local grant (C2). UPSERT semantics: a re-share to the same
/// grantee refreshes the single row's `granted_at` rather than appending a
/// duplicate, so the ShareList shows exactly one row per grantee.
///
/// The CALLER MUST `notify_dirty()` + persist after a mutation — a `file_grants`
/// change without it is NEVER persisted NOR replicated (ack-outruns-payload =
/// permanent loss, ZEB-709).
pub fn record_grant(
    state: &mut OwnerState,
    cid: [u8; 32],
    grantee_owner: OwnerAddr,
    granted_at: u64,
) {
    let entries = state.file_grants.entry(cid).or_default();
    // Upsert: bump `granted_at` forward on an existing entry — PRESERVING any
    // `revoked_at`, so a re-share after a revoke REACTIVATES the grant via
    // `granted_at > revoked_at` (ZEB-725 LWW-element-set). Otherwise append a
    // fresh, never-revoked entry.
    if let Some(existing) = entries
        .iter_mut()
        .find(|g| g.grantee_owner == grantee_owner)
    {
        existing.granted_at = granted_at;
    } else {
        entries.push(GrantEntry {
            grantee_owner,
            granted_at,
            revoked_at: 0,
        });
    }
}

/// Project `state.file_grants[cid]` into frontend DTO rows, resolving each
/// grantee's `display_name` from the friend graph (`None` when the grantee is
/// not a currently-known friend). Empty vec when `cid` has no grants. Pure.
pub fn list_grants_inner(state: &OwnerState, cid: [u8; 32]) -> Vec<FileGrantDto> {
    let Some(grants) = state.file_grants.get(&cid) else {
        return Vec::new();
    };
    grants
        .iter()
        // ACTIVE only: a tombstoned (revoked) entry stays in `file_grants` for
        // convergence but must not appear in the "Shared with" list (ZEB-725).
        .filter(|g| g.granted_at > g.revoked_at)
        .map(|g| FileGrantDto {
            grantee_address: hex::encode(g.grantee_owner.0),
            display_name: state
                .friend_graph
                .friends
                .get(&g.grantee_owner)
                .and_then(|f| f.display.clone()),
            granted_at: g.granted_at,
        })
        .collect()
}

/// LAZY revoke (design "Revocation semantics — lazy"), ZEB-725 convergent form:
/// TOMBSTONE the grantee's `GrantEntry` for `cid` by stamping `revoked_at`
/// forward past `granted_at` (so `granted_at > revoked_at` is now false and the
/// grant is inactive). The entry is KEPT, not removed — a removed record would
/// be re-added by a plain grow-only union with a stale sibling, resurrecting the
/// revoked grantee; a tombstone that merges by `max` instead CONVERGES the
/// revoke across the owner's devices. Returns whether an ACTIVE grant was
/// revoked (idempotent no-op — `false` — when already inactive or absent).
///
/// Does NOT touch the DEK or the serve allowlist — an already-granted grantee
/// keeps the DEK and the CID stays served (constraint 3: access to this CID
/// version cannot be withdrawn without rotation). This only drops the grantee
/// from the owner's ShareList and stops future re-delivery. The CALLER MUST
/// `notify_dirty()` + persist on a `true` return (ZEB-709).
pub fn revoke_grant_inner(
    state: &mut OwnerState,
    cid: [u8; 32],
    grantee_owner: OwnerAddr,
    now_ms: u64,
) -> bool {
    let Some(grants) = state.file_grants.get_mut(&cid) else {
        return false;
    };
    let Some(entry) = grants.iter_mut().find(|g| g.grantee_owner == grantee_owner) else {
        return false;
    };
    // Already inactive ⇒ idempotent no-op (no state change, no re-emit).
    if entry.granted_at <= entry.revoked_at {
        return false;
    }
    // Stamp revoked_at forward PAST granted_at so the grant is inactive even
    // under clock skew (revoked_at >= granted_at ⇒ `granted_at > revoked_at`
    // is false), while remaining a `max`-mergeable LWW timestamp.
    entry.revoked_at = now_ms.max(entry.granted_at);
    true
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
        // Structure: nonce(12) + ciphertext(32) + tag(16) = 60 bytes.
        assert_eq!(sealed.len(), 60);
        // The raw DEK must not appear VERBATIM anywhere in the sealed blob — a
        // length inequality alone would pass even if the DEK were embedded
        // beside padding. Scan every 32-byte window.
        let raw: &[u8] = dek.as_bytes();
        assert!(
            !sealed.windows(raw.len()).any(|w| w == raw),
            "the raw DEK must not appear verbatim in the sealed-at-rest blob"
        );
        // Positive proof: the seal is reversible under the same tree.
        let opened = open_dek_at_rest(&tree, &sealed).expect("open");
        assert_eq!(opened.as_bytes(), dek.as_bytes());
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

    // ── Task 6 IPC cores ────────────────────────────────────────────────

    use crate::friend_graph::{FriendEntry, FriendOrigin, FriendStatus};
    use crate::owner_state_types::{Hlc, OwnerDeviceEntry};

    const GRANTEE: OwnerAddr = OwnerAddr([0x77u8; 16]);
    const CID: [u8; 32] = [0xC1u8; 32];

    fn friend_entry(status: FriendStatus, display: Option<&str>) -> FriendEntry {
        FriendEntry {
            master_ed25519: [0x33; 32],
            display: display.map(str::to_owned),
            status,
            established_via: FriendOrigin::Token,
            referrable: false,
            learned_at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "t".into(),
            },
            sealed_secret: None,
        }
    }

    fn device_pub(seed: u8) -> [u8; 64] {
        let mut p = [0u8; 64];
        p[..32].copy_from_slice(&[seed; 32]);
        p
    }

    /// Seed `state` with an encrypted-at-ingest file DEK for [`CID`], plus an
    /// Active friend [`GRANTEE`] carrying one learned device X25519 — the happy
    /// path for `build_grant_push`.
    fn seed_grantable(tree: &KeyTree, display: Option<&str>) -> OwnerState {
        let mut state = OwnerState::default();
        let dek = generate_file_dek();
        state
            .file_deks
            .insert(CID, seal_dek_at_rest(tree, &dek).expect("seal dek"));
        state
            .friend_graph
            .friends
            .insert(GRANTEE, friend_entry(FriendStatus::Active, display));
        state.owner_device_cache.devices.insert(
            GRANTEE,
            OwnerDeviceEntry {
                devices: vec![],
                device_identity_pubs: vec![Some(device_pub(0xA1))],
                learned_at: Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "t".into(),
                },
                device_tunnel_contacts: vec![],
            },
        );
        state
    }

    #[test]
    fn grant_read_rejects_public_cid() {
        // No `file_deks[cid]` → the file was ingested public; ineligible.
        let tree = test_tree();
        let mut state = seed_grantable(&tree, None);
        state.file_deks.clear(); // simulate a public (unencrypted) CID
        let err = build_grant_push(&state, &tree, CID, GRANTEE, "f".into(), 1, "m".into())
            .expect_err("public cid must be ineligible");
        assert!(err.starts_with("ineligible:"), "stable prefix: {err}");
        assert_eq!(err, INELIGIBLE_PUBLIC);
    }

    #[test]
    fn grant_read_rejects_when_no_device_keys() {
        // Encrypted file present, grantee IS an Active friend, but their
        // device-identity-pubs have not propagated yet — no
        // `owner_device_cache` entry at all, so `grantee_device_x25519s`
        // resolves empty. There is no device to seal the grant to; recording
        // it anyway would let the owner's ShareList claim "shared" for a
        // delivery the backend cannot make (the honesty fix under test).
        let tree = test_tree();
        let mut state = seed_grantable(&tree, None);
        state.owner_device_cache.devices.remove(&GRANTEE);
        let err = build_grant_push(&state, &tree, CID, GRANTEE, "f".into(), 1, "m".into())
            .expect_err("grantee with no resolvable device keys must be ineligible");
        assert!(err.starts_with("ineligible:"), "stable prefix: {err}");
        assert_eq!(err, INELIGIBLE_NO_DEVICES);
        // build_grant_push is pure (never mutates state on any path, success
        // or failure) — confirm nothing was recorded for the rejected grant.
        assert!(
            !state.file_grants.contains_key(&CID),
            "a rejected grant must record no GrantEntry"
        );
    }

    #[test]
    fn grant_read_rejects_non_friend() {
        // Encrypted file present, but the grantee is not a friend at all.
        let tree = test_tree();
        let mut state = seed_grantable(&tree, None);
        state.friend_graph.friends.clear();
        let err = build_grant_push(&state, &tree, CID, GRANTEE, "f".into(), 1, "m".into())
            .expect_err("non-friend must be ineligible");
        assert!(err.starts_with("ineligible:"), "stable prefix: {err}");
        assert_eq!(err, INELIGIBLE_NON_FRIEND);

        // A merely-Pending friend is also ineligible (not yet mutual).
        state
            .friend_graph
            .friends
            .insert(GRANTEE, friend_entry(FriendStatus::Pending, None));
        let err2 = build_grant_push(&state, &tree, CID, GRANTEE, "f".into(), 1, "m".into())
            .expect_err("pending friend must be ineligible");
        assert_eq!(err2, INELIGIBLE_NON_FRIEND);
    }

    #[test]
    fn grant_then_list_reflects_grantee() {
        let tree = test_tree();
        let mut state = seed_grantable(&tree, Some("Alice"));

        // The seal must succeed and open on the grantee's device (a real,
        // openable grant — not just a byte blob).
        let grant_push = build_grant_push(
            &state,
            &tree,
            CID,
            GRANTEE,
            "notes.md".into(),
            9,
            "text/plain".into(),
        )
        .expect("grant to an active friend with a known device succeeds");
        let blobs: Vec<serde_bytes::ByteBuf> =
            ciborium::from_reader(grant_push.as_slice()).expect("grant_push decodes");
        assert_eq!(blobs.len(), 1, "one sealed blob per known device");

        // Record it, then the ShareList reflects the grantee with the display.
        record_grant(&mut state, CID, GRANTEE, 4_242);
        let list = list_grants_inner(&state, CID);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].grantee_address, hex::encode(GRANTEE.0));
        assert_eq!(list[0].display_name.as_deref(), Some("Alice"));
        assert_eq!(list[0].granted_at, 4_242);

        // Re-share upserts (no duplicate row), refreshing granted_at.
        record_grant(&mut state, CID, GRANTEE, 5_000);
        let list2 = list_grants_inner(&state, CID);
        assert_eq!(list2.len(), 1, "re-share upserts, never duplicates");
        assert_eq!(list2[0].granted_at, 5_000);
    }

    #[test]
    fn revoke_tombstones_record_hides_and_reactivates() {
        let tree = test_tree();
        let mut state = seed_grantable(&tree, Some("Alice"));
        let dek_before = state.file_deks.get(&CID).cloned().expect("dek present");

        record_grant(&mut state, CID, GRANTEE, 4_242);
        assert_eq!(list_grants_inner(&state, CID).len(), 1);

        // Revoke TOMBSTONES the grantee: the ShareList omits it...
        assert!(
            revoke_grant_inner(&mut state, CID, GRANTEE, 5_000),
            "an active grant was revoked"
        );
        assert!(
            list_grants_inner(&state, CID).is_empty(),
            "list omits the revoked grantee"
        );
        // ...but the entry is KEPT (tombstoned, not dropped) so the revoke
        // converges across devices, and it is inactive (granted_at <= revoked_at).
        let entry = state.file_grants[&CID]
            .iter()
            .find(|g| g.grantee_owner == GRANTEE)
            .expect("tombstoned entry retained for convergence");
        assert!(
            entry.granted_at <= entry.revoked_at,
            "tombstoned entry is inactive: granted_at {} <= revoked_at {}",
            entry.granted_at,
            entry.revoked_at
        );
        // ...and the DEK is UNTOUCHED (already-granted access is not withdrawn).
        assert_eq!(
            state.file_deks.get(&CID),
            Some(&dek_before),
            "revoke must not touch the file DEK (lazy)"
        );

        // Revoking again is an idempotent no-op (already inactive).
        assert!(
            !revoke_grant_inner(&mut state, CID, GRANTEE, 6_000),
            "nothing active left to revoke"
        );

        // Re-sharing REACTIVATES: granted_at bumps past revoked_at.
        record_grant(&mut state, CID, GRANTEE, 7_000);
        assert_eq!(
            list_grants_inner(&state, CID).len(),
            1,
            "re-share after revoke reactivates the grant"
        );
    }

    #[test]
    fn list_received_grants_inner_projects_sorts_and_resolves_names() {
        let granter_friend = OwnerAddr([0x11; 16]);
        let granter_stranger = OwnerAddr([0x22; 16]);
        let cid_a = [0xAA; 32];
        let cid_b = [0xBB; 32];

        let mut state = OwnerState::default();
        // A friend granter → display name resolves.
        state.friend_graph.friends.insert(
            granter_friend,
            friend_entry(FriendStatus::Active, Some("Alice")),
        );
        // Older grant from a friend.
        state.received_file_grants.insert(
            cid_a,
            ReceivedFileGrant {
                granter_owner: granter_friend,
                cid: cid_a,
                file_name: "a.pdf".into(),
                file_size: 100,
                mime: "application/pdf".into(),
                sealed_dek: vec![1, 2, 3],
                received_at: 1_000,
            },
        );
        // Newer grant from a non-friend stranger.
        state.received_file_grants.insert(
            cid_b,
            ReceivedFileGrant {
                granter_owner: granter_stranger,
                cid: cid_b,
                file_name: "b.png".into(),
                file_size: 200,
                mime: "image/png".into(),
                sealed_dek: vec![4, 5],
                received_at: 2_000,
            },
        );

        let rows = list_received_grants_inner(&state);

        // Sorted by received_at DESC → newest (cid_b) first.
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].cid, hex::encode(cid_b));
        assert_eq!(rows[0].granter_address, hex::encode(granter_stranger.0));
        assert_eq!(rows[0].display_name, None, "stranger has no friend display");
        assert_eq!(rows[0].file_name, "b.png");
        assert_eq!(rows[0].file_size, 200);
        assert_eq!(rows[0].mime, "image/png");
        assert_eq!(rows[0].received_at, 2_000);

        assert_eq!(rows[1].cid, hex::encode(cid_a));
        assert_eq!(rows[1].display_name.as_deref(), Some("Alice"));

        // Empty map → empty vec (proven-empty).
        assert!(list_received_grants_inner(&OwnerState::default()).is_empty());
    }

    #[test]
    fn dismiss_received_grant_inner_gcs_and_tombstones() {
        let cid = [0x33; 32];
        let mut state = OwnerState::default();
        state.received_file_grants.insert(
            cid,
            ReceivedFileGrant {
                granter_owner: OwnerAddr([0x01; 16]),
                cid,
                file_name: "doc.pdf".into(),
                file_size: 10,
                mime: "application/pdf".into(),
                sealed_dek: vec![1, 2, 3],
                received_at: 100,
            },
        );

        assert!(
            dismiss_received_grant_inner(&mut state, cid, 200),
            "dismissing a present grant is a state change"
        );
        assert!(
            !state.received_file_grants.contains_key(&cid),
            "dismiss removes the received-grant entry"
        );
        assert_eq!(
            state.dismissed_received_grants.get(&cid),
            Some(&200),
            "dismiss records the LWW tombstone at now_ms"
        );

        // Re-dismiss with an OLDER stamp is a no-op (max-join keeps the newer).
        assert!(
            !dismiss_received_grant_inner(&mut state, cid, 150),
            "re-dismiss with an older stamp changes nothing"
        );
        assert_eq!(state.dismissed_received_grants.get(&cid), Some(&200));
    }

    #[test]
    fn list_received_grants_inner_hides_dismissed() {
        let cid = [0x44; 32];
        let mut state = OwnerState::default();
        state.received_file_grants.insert(
            cid,
            ReceivedFileGrant {
                granter_owner: OwnerAddr([0x01; 16]),
                cid,
                file_name: "hidden.bin".into(),
                file_size: 5,
                mime: "application/octet-stream".into(),
                sealed_dek: vec![9],
                received_at: 100,
            },
        );
        // Tombstone AT received_at → not active (predicate is strict `>`) → hidden.
        state.dismissed_received_grants.insert(cid, 100);
        assert!(
            list_received_grants_inner(&state).is_empty(),
            "a grant with dismissed_at >= received_at is hidden from the projection"
        );
    }

    /// ZEB-727 design crux at the projection layer: the dismiss tombstone is
    /// LWW-timestamped, NOT permanent. A legitimate owner re-share (fresh
    /// `received_at > dismissed_at`) reappears. A permanent-set tombstone (the
    /// ZEB-722 burn idiom) would keep it hidden forever — this is the regression
    /// guard for that.
    #[test]
    fn re_share_reactivates_in_projection() {
        let cid = [0x55; 32];
        let mut state = OwnerState::default();
        state.dismissed_received_grants.insert(cid, 200);
        // Owner re-shares: ingest stamps a FRESH received_at (300 > 200).
        state.received_file_grants.insert(
            cid,
            ReceivedFileGrant {
                granter_owner: OwnerAddr([0x02; 16]),
                cid,
                file_name: "re-shared.txt".into(),
                file_size: 3,
                mime: "text/plain".into(),
                sealed_dek: vec![3],
                received_at: 300,
            },
        );
        let rows = list_received_grants_inner(&state);
        assert_eq!(
            rows.len(),
            1,
            "re-share (received_at 300 > dismissed 200) reappears in the projection"
        );
        assert_eq!(rows[0].received_at, 300);
    }
}
