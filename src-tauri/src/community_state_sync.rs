//! Per-community state-CRDT sync — Phase 2 of ZEB-217 Sub-C.
//!
//! Mirrors the SHAPE of `crate::owner_state_sync::SyncEngine` but
//! multi-instance: one `CommunitySyncEngine` per joined community.
//! Each engine debounces local mutations into encrypted state-root
//! publishes, fetches remote state-root publishes from a per-community
//! Zenoh topic, DAG-syncs the encrypted blob via existing CAS
//! machinery, decrypts, and merges remote events into local
//! `CommunityState` after re-running `verify_event` per event.
//!
//! This file ships the AEAD helpers, the `CommunityRootPublishPayload`
//! wire type, the `CommunityRootHlcTracker`, and the
//! `CommunitySyncEngine`. The internal task ships the debounced
//! publish loop (notify_dirty → debounce → publish_root_now,
//! flush_now → force-publish, shutdown → final flush), the receive
//! pipeline (`handle_incoming_publish` decrypts root publishes,
//! fetches + decrypts the encrypted blob from CAS, re-runs
//! `verify_event` per event, and merges into local CRDT state, with
//! per-publisher-device replay protection via `CommunityRootHlcTracker`),
//! and the per-arm persist hooks (Task 10) that flush
//! `crdt.cbor` + `replay.cbor` to disk through `community_state_persist`
//! after debounce wakeups, after merge of incoming publishes, and on
//! shutdown. The multi-community lifecycle layer ships in
//! `CommunitySyncRegistry` (Task 11) — it owns
//! `BTreeMap<SpaceId, Arc<CommunitySyncEngine>>` under a Mutex,
//! derives per-community persist paths under
//! `identity_dir/communities/{id_hex}/`, loads any prior CRDT + replay
//! snapshot from disk before spawning each engine, and surfaces idempotent
//! `spawn_engine` / `stop_engine` / `shutdown_all` / `known_ids` for the
//! owner-state subscription scan in Task 12. The production
//! `IdentityResolver` impl `OwnerDeviceCacheResolver` (Task 13) wraps
//! Sub-A's RegisterDevice cache to bridge `event.actor: OwnerAddr` →
//! 64-byte identity_pub for receive-side `verify_event`.

use chacha20poly1305::aead::Aead;
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce};
use harmony_content::cid::ContentId;
use rand::rngs::OsRng;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex, Notify};
use tokio::task::JoinHandle;

use crate::community_membership::{SignedMembershipEvent, VerifyContext};
use crate::community_state_crdt::{CommunityState, InsertOutcome};
use crate::community_state_persist::{load_crdt, load_replay, save_crdt, save_replay};
use crate::content_store::ContentStore;
use crate::owner_state_crypto::{
    canonical_cbor_decode, sealed::CanonicalPayloadSealed, CanonicalPayload,
};
use crate::owner_state_types::{
    deserialize_bytes_from_bstr, serialize_bytes_as_bstr, EpochKey, Hlc, OwnerAddr, Space, SpaceId,
};

/// Errors specific to community-state encryption + decryption.
#[derive(thiserror::Error, Debug)]
pub enum CommunityCryptoError {
    #[error("AEAD operation failed (wrong key, malformed ciphertext, tag mismatch)")]
    AeadFailed,
    #[error("ciphertext too short to contain nonce + tag")]
    Truncated,
    /// `harmony_content::cid::ContentId::for_book` rejected the
    /// ciphertext input (e.g. exceeds the structured-CID size budget).
    /// Distinct from `AeadFailed` because the failure class is
    /// content-addressing, not cryptography — surfacing it as
    /// `AeadFailed` would mislead the operator into checking key
    /// material when the actual fault is in the blob layer.
    #[error("ContentId derivation failed: {0}")]
    ContentIdDerivation(String),
}

const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const MIN_WIRE_LEN: usize = NONCE_LEN + TAG_LEN;

/// Per-blob ciphertext overhead added by [`encrypt_blob`]: a prepended
/// 12-byte nonce + a 16-byte Poly1305 tag = 28 bytes. Callers that bound a
/// fetch by plaintext size (e.g. `download_channel_artifact_impl`) must add
/// this to their `max_bytes` for encrypted CIDs, since the assembled
/// ciphertext is larger than the plaintext it decrypts to.
pub const BLOB_ENCRYPTION_OVERHEAD: usize = NONCE_LEN + TAG_LEN;

/// Domain-separation prefix for the per-community blob nonce.
/// Combined with the SHA-256 of the plaintext to derive a deterministic
/// nonce — see `encrypt_blob` for the full derivation.
const COMMUNITY_BLOB_NONCE_PREFIX: &[u8] = b"harmony-community-blob-v1";

/// Domain-separation prefix for root-publish AEAD AAD. Bound to the
/// wire form so a re-encrypted blob from a different context can't be
/// substituted as a root-publish wire packet.
const COMMUNITY_ROOT_PUBLISH_AAD: &[u8] = b"harmony-community-root-publish-v1";

/// Encrypt a state-root publish payload with the community's
/// `EpochKey`. Random 12-byte nonce prepended to the ciphertext;
/// receiver splits and verifies via ChaCha20-Poly1305 AAD binding.
///
/// Random nonce is correct here (every publish is a distinct wire
/// packet — we WANT freshness; replay protection is the receiver's
/// `RootHlcTracker`, not nonce reuse).
pub fn encrypt_root_publish(
    mk: &EpochKey,
    plaintext: &[u8],
) -> Result<Vec<u8>, CommunityCryptoError> {
    let cipher = ChaCha20Poly1305::new(mk.as_chacha_key());
    let mut nonce_bytes = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ct = cipher
        .encrypt(
            nonce,
            chacha20poly1305::aead::Payload {
                msg: plaintext,
                aad: COMMUNITY_ROOT_PUBLISH_AAD,
            },
        )
        .map_err(|_| CommunityCryptoError::AeadFailed)?;

    let mut wire = Vec::with_capacity(NONCE_LEN + ct.len());
    wire.extend_from_slice(&nonce_bytes);
    wire.extend_from_slice(&ct);
    Ok(wire)
}

/// Decrypt a state-root publish wire packet produced by
/// `encrypt_root_publish`. Verifies the AAD binding; rejects packets
/// shorter than `NONCE_LEN + TAG_LEN` bytes before slicing to avoid
/// panics on truncated input.
pub fn decrypt_root_publish(mk: &EpochKey, wire: &[u8]) -> Result<Vec<u8>, CommunityCryptoError> {
    if wire.len() < MIN_WIRE_LEN {
        return Err(CommunityCryptoError::Truncated);
    }
    let cipher = ChaCha20Poly1305::new(mk.as_chacha_key());
    let nonce = Nonce::from_slice(&wire[..NONCE_LEN]);
    let ct = &wire[NONCE_LEN..];
    cipher
        .decrypt(
            nonce,
            chacha20poly1305::aead::Payload {
                msg: ct,
                aad: COMMUNITY_ROOT_PUBLISH_AAD,
            },
        )
        .map_err(|_| CommunityCryptoError::AeadFailed)
}

/// Encrypt the CBOR-encoded `CommunityState` blob with a deterministic
/// nonce — same (key, plaintext) yields the same ciphertext, so the
/// resulting `ContentId` is reproducible across replicas. Lets two
/// devices encrypting the same state hit the same ContentStore slot.
///
/// Nonce derivation: `SHA-256(prefix || mk_bytes || plaintext)[..12]`.
/// Binding the nonce to BOTH the key and plaintext ensures the same
/// pair always derives the same nonce; mixing the key into the nonce
/// is a nonce-reuse-resistance hedge (an attacker without `mk` cannot
/// derive the nonce, so a chosen-plaintext nonce-collision attack
/// requires already having the key).
pub fn encrypt_blob(mk: &EpochKey, plaintext: &[u8]) -> Result<Vec<u8>, CommunityCryptoError> {
    let mut h = Sha256::new();
    h.update(COMMUNITY_BLOB_NONCE_PREFIX);
    h.update(mk.as_bytes());
    h.update(plaintext);
    let digest = h.finalize();
    let nonce_bytes: [u8; NONCE_LEN] = digest[..NONCE_LEN]
        .try_into()
        .expect("SHA-256 digest is 32 bytes");

    let cipher = ChaCha20Poly1305::new(mk.as_chacha_key());
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ct = cipher
        .encrypt(nonce, plaintext)
        .map_err(|_| CommunityCryptoError::AeadFailed)?;

    let mut wire = Vec::with_capacity(NONCE_LEN + ct.len());
    wire.extend_from_slice(&nonce_bytes);
    wire.extend_from_slice(&ct);
    Ok(wire)
}

/// Decrypt a blob produced by `encrypt_blob`. The nonce embedded at
/// the head is treated as opaque; correctness rests on the Poly1305
/// tag, not on re-deriving the nonce. Rejects wires shorter than
/// `NONCE_LEN + TAG_LEN` bytes before slicing.
pub fn decrypt_blob(mk: &EpochKey, wire: &[u8]) -> Result<Vec<u8>, CommunityCryptoError> {
    if wire.len() < MIN_WIRE_LEN {
        return Err(CommunityCryptoError::Truncated);
    }
    let cipher = ChaCha20Poly1305::new(mk.as_chacha_key());
    let nonce = Nonce::from_slice(&wire[..NONCE_LEN]);
    let ct = &wire[NONCE_LEN..];
    cipher
        .decrypt(nonce, ct)
        .map_err(|_| CommunityCryptoError::AeadFailed)
}

/// State-root publish wire envelope. ZEB-256: every publish is signed
/// by the publisher's local Ed25519 device key. Receivers verify the
/// signature, the publisher's membership status as of `payload.at`
/// (via `prior_state_at_hlc(payload.at)` — NOT the publisher's
/// current materialized status, which would be wrong for lagging-peer
/// convergence), and the per-(addr, device) replay tracker before
/// merging events.
///
/// Wire format: 4-key CBOR map (or 5-key if `epoch` is Some). All
/// field codes are 2 chars (`rc`/`pa`/`at`/`ps`/`ep`) to satisfy the
/// same-length-keys invariant at this nesting level.
///
/// ZEB-249 §10.6 (Phase A): added optional `epoch` field so receivers
/// can select the correct historical epoch key for decryption when the
/// sender's epoch differs from the receiver's current epoch. Legacy
/// messages without this field fall back to the current-then-old-key
/// trial decryption. `skip_serializing_if = "Option::is_none"` keeps
/// the wire bytes identical to v1 for publishers that haven't upgraded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommunityRootPublishPayload {
    /// Content-ID of the encrypted CommunityState blob in the shared
    /// ContentStore. Unchanged from Phase 2.
    #[serde(rename = "rc")]
    pub root_cid: ContentId,

    /// Owner address of the publishing member. Receivers use this to
    /// (a) resolve identity_pub via IdentityResolver, (b) check
    /// membership-at-publish-HLC, (c) namespace the replay tracker.
    #[serde(rename = "pa")]
    pub publisher_addr: OwnerAddr,

    /// Publisher's HLC at publish time. Carries device_id; tracker
    /// slot key is `(publisher_addr, at.device_id)`. Unchanged shape
    /// from Phase 2 — only the tracker's interpretation changed.
    #[serde(rename = "at")]
    pub at: Hlc,

    /// Ed25519 signature over canonical CBOR of
    /// `CommunityRootSignedPayload { root_cid, publisher_addr, at }`.
    #[serde(
        rename = "ps",
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub publisher_sig: [u8; 64],

    /// ZEB-249 §10.6: epoch counter at publish time. Receivers use this
    /// to select `old_epoch_keys[epoch]` when their `current_epoch`
    /// differs. `None` for legacy publishes (pre-§10.6); receivers fall
    /// back to trying current key then old keys in reverse order.
    #[serde(rename = "ep", skip_serializing_if = "Option::is_none", default)]
    pub epoch: Option<u64>,
}

impl CanonicalPayloadSealed for CommunityRootPublishPayload {}
impl CanonicalPayload for CommunityRootPublishPayload {}

/// The unsigned portion of a `CommunityRootPublishPayload` — the
/// canonical-CBOR bytes the publisher signs. Mirrors `EventPayload` vs
/// `SignedMembershipEvent`: keeping the signed sub-payload as its own
/// type means the signed bytes are unambiguous (no place to put "the
/// actual sig went here" in the encoded form).
///
/// All 3 field keys are 2 chars to satisfy the same-length-keys
/// invariant at this nesting level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommunityRootSignedPayload {
    #[serde(rename = "rc")]
    pub root_cid: ContentId,
    #[serde(rename = "pa")]
    pub publisher_addr: OwnerAddr,
    #[serde(rename = "at")]
    pub at: Hlc,
}

impl CanonicalPayloadSealed for CommunityRootSignedPayload {}
impl CanonicalPayload for CommunityRootSignedPayload {}

impl CommunityRootSignedPayload {
    /// Convert a signed sub-payload into its full wire envelope by
    /// attaching the Ed25519 signature and the publisher's current epoch
    /// (ZEB-249 §10.6: receivers use this to select the right epoch key).
    pub fn into_wire(
        self,
        publisher_sig: [u8; 64],
        epoch: Option<u64>,
    ) -> CommunityRootPublishPayload {
        CommunityRootPublishPayload {
            root_cid: self.root_cid,
            publisher_addr: self.publisher_addr,
            at: self.at,
            publisher_sig,
            epoch,
        }
    }
}

/// Convenience: extract the signed sub-payload from a full wire
/// envelope. Used by receive-side verify to reproduce the canonical
/// CBOR bytes the publisher signed.
impl From<&CommunityRootPublishPayload> for CommunityRootSignedPayload {
    fn from(w: &CommunityRootPublishPayload) -> Self {
        Self {
            root_cid: w.root_cid,
            publisher_addr: w.publisher_addr,
            at: w.at.clone(),
        }
    }
}

/// Per-event encrypted envelope. Replaces the bare ChaCha20-Poly1305
/// output of v1's membership-topic encryption with an epoch-tagged
/// container that lets receivers select the right historical key.
///
/// Wire format: 3-key or 4-key CBOR map. All keys are 2 chars to
/// satisfy the same-length-keys invariant at this nesting level.
///
/// `ratchet_generation` is reserved for a future forward-secrecy
/// extension (ZEB-249 spec §9.2). v2 readers MUST tolerate `rg`
/// present-but-null; v2 writers MUST always set `rg = None`.
///
/// See ZEB-249 spec §3.4 + §7.2.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncryptedEnvelope {
    #[serde(rename = "ep")]
    pub epoch: u64,

    #[serde(
        rename = "nc",
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub nonce: [u8; 12],

    #[serde(
        rename = "ct",
        serialize_with = "crate::owner_state_types::serialize_vec_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_vec_from_bstr"
    )]
    pub ciphertext: Vec<u8>,

    /// Reserved for ZEB-249 spec §9.2 forward-secrecy extension.
    /// Always `None` in v2 writers; `None` and `Some(_)` both decode in
    /// v2 readers (forward-compat).
    #[serde(rename = "rg", default, skip_serializing_if = "Option::is_none")]
    pub ratchet_generation: Option<u64>,
}

impl crate::owner_state_crypto::sealed::CanonicalPayloadSealed for EncryptedEnvelope {}
impl crate::owner_state_crypto::CanonicalPayload for EncryptedEnvelope {}

/// Failure modes for epoch-aware encryption/decryption.
/// See ZEB-249 spec §6.2.
#[derive(Debug, thiserror::Error)]
pub enum EpochError {
    #[error("key for epoch {0} not available locally")]
    KeyNotAvailable(u64),

    #[error("AEAD encryption failed at epoch {0}")]
    EncryptionFailed(u64),

    #[error("AEAD tag mismatch on event at epoch {0}")]
    DecryptionFailed(u64),

    #[error("rotation references stale prior_epoch {provided}, current is {current}")]
    StaleRotation { provided: u64, current: u64 },

    #[error("malformed rotation: target {target:?} included in recipient_ciphertexts")]
    MalformedRotation {
        target: crate::owner_state_types::OwnerAddr,
    },

    #[error("rotation issuer {issuer:?} lacks authority (not admin and not target)")]
    InvalidIssuer {
        issuer: crate::owner_state_types::OwnerAddr,
    },

    /// C6: `encrypt_for_topic` was called on a Space that is missing
    /// `current_epoch` or `current_epoch_key`. This can happen with
    /// partially-migrated Spaces deserialized before ZEB-249 epoch fields
    /// were populated. Replaces the previous `expect(...)` panic.
    #[error("community Space is missing current_epoch or current_epoch_key")]
    MissingEpochState,
}

/// ZEB-717 D1: domain-separation AAD for the voting Zenoh topic. The voting
/// adapter binds this via [`encrypt_for_topic_with_aad`] /
/// [`decrypt_for_topic_with_aad`]; the state-root plane uses empty AAD (the
/// 2-arg helpers). Both planes share the community epoch key, so this distinct
/// AAD makes a cross-plane ciphertext fail the AEAD tag rather than merely a
/// downstream deserialize. Versioned (`-v1`) for future rotation.
pub const VOTING_TOPIC_AAD: &[u8] = b"harmony-voting-v1";

/// Encrypt `plaintext` under the community's current epoch key,
/// wrapping the AEAD output in an `EncryptedEnvelope` that tags the
/// epoch for receiver-side key selection.
///
/// `space` MUST be a Community Space with `current_epoch` and
/// `current_epoch_key` both `Some`. Returns
/// `EpochError::MissingEpochState` if either field is absent — a
/// partially-migrated Space (e.g., deserialized before ZEB-249 fields
/// were added) can reach this helper and must not panic.
pub fn encrypt_for_topic(space: &Space, plaintext: &[u8]) -> Result<EncryptedEnvelope, EpochError> {
    // Empty AAD is byte-identical to the previous no-AAD call, so state-root
    // wire bytes and fixtures are unchanged.
    encrypt_for_topic_with_aad(space, plaintext, b"")
}

/// AAD-parameterized variant of [`encrypt_for_topic`] (ZEB-717 D1).
///
/// The voting plane binds `VOTING_TOPIC_AAD` for cryptographic domain
/// separation from the state-root plane (which passes `b""` via
/// [`encrypt_for_topic`]) — both share the same community epoch key, so
/// without a distinct AAD a cross-plane ciphertext would merely fail a
/// downstream deserialize instead of the AEAD tag. See ZEB-717 spec §3 (D1).
pub fn encrypt_for_topic_with_aad(
    space: &Space,
    plaintext: &[u8],
    aad: &[u8],
) -> Result<EncryptedEnvelope, EpochError> {
    // C6: return Err instead of panicking on missing epoch state.
    let epoch = space.current_epoch.ok_or(EpochError::MissingEpochState)?;
    let key = space
        .current_epoch_key
        .as_ref()
        .ok_or(EpochError::MissingEpochState)?;
    let cipher = ChaCha20Poly1305::new(key.as_chacha_key());

    let mut nonce_bytes = [0u8; 12];
    OsRng.fill_bytes(&mut nonce_bytes);

    let ciphertext = cipher
        .encrypt(
            Nonce::from_slice(&nonce_bytes),
            chacha20poly1305::aead::Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| EpochError::EncryptionFailed(epoch))?;

    Ok(EncryptedEnvelope {
        epoch,
        nonce: nonce_bytes,
        ciphertext,
        ratchet_generation: None,
    })
}

/// Decrypt an `EncryptedEnvelope` using the appropriate epoch key
/// from the community Space's current or old epoch keys.
///
/// Returns `EpochError::KeyNotAvailable(epoch)` if neither
/// `current_epoch_key` (when epoch matches `current_epoch`) nor
/// `old_epoch_keys[epoch]` contains the needed key.
pub fn decrypt_for_topic(
    space: &Space,
    envelope: &EncryptedEnvelope,
) -> Result<Vec<u8>, EpochError> {
    decrypt_for_topic_with_aad(space, envelope, b"")
}

/// AAD-parameterized variant of [`decrypt_for_topic`] (ZEB-717 D1). The `aad`
/// MUST match the one used at encrypt time; a mismatch fails the AEAD tag
/// (`DecryptionFailed`). Voting passes `VOTING_TOPIC_AAD`, state-root `b""`.
///
/// Note: this retains the general current-then-old key selection. The voting
/// receive path deliberately does NOT rely on the old-key branch — it gates on
/// `envelope.epoch == current_epoch` first (ZEB-717 spec §3 D3), so a kicked
/// member's retained old-epoch envelope is refused before this is reached.
pub fn decrypt_for_topic_with_aad(
    space: &Space,
    envelope: &EncryptedEnvelope,
    aad: &[u8],
) -> Result<Vec<u8>, EpochError> {
    let current_epoch = space
        .current_epoch
        .ok_or(EpochError::KeyNotAvailable(envelope.epoch))?;
    let key = if envelope.epoch == current_epoch {
        space.current_epoch_key.as_ref()
    } else {
        space.old_epoch_keys.get(&envelope.epoch)
    }
    .ok_or(EpochError::KeyNotAvailable(envelope.epoch))?;

    let cipher = ChaCha20Poly1305::new(key.as_chacha_key());
    cipher
        .decrypt(
            Nonce::from_slice(&envelope.nonce),
            chacha20poly1305::aead::Payload {
                msg: envelope.ciphertext.as_slice(),
                aad,
            },
        )
        .map_err(|_| EpochError::DecryptionFailed(envelope.epoch))
}

/// Test-only helper: build a Community `Space` with `id` at `epoch` under
/// `key`, empty `old_epoch_keys`. Integration tests seed an `OwnerState` with
/// this so the voting adapter (ZEB-717) has a live epoch key to encrypt /
/// current-epoch-only-decrypt against. Callers may set `old_epoch_keys`
/// afterward to exercise the retained-old-key rejection path.
#[cfg(any(test, feature = "test-fixtures"))]
pub fn test_community_space(id: SpaceId, epoch: u64, key: EpochKey) -> Space {
    let zero_hlc = Hlc {
        wall_ms: 0,
        logical: 0,
        device_id: "t".into(),
    };
    Space {
        id,
        kind: crate::owner_state_types::SpaceKind::Community,
        parent: None,
        community_id: None,
        name: "Test".into(),
        transport: None,
        members: vec![],
        custom_name: None,
        notification_pref: None,
        left_at: None,
        created_at: zero_hlc.clone(),
        updated_at: zero_hlc,
        content_key: None,
        prior_content_keys: vec![],
        current_epoch: Some(epoch),
        current_epoch_key: Some(key),
        old_epoch_keys: std::collections::BTreeMap::new(),
        admin_addr: Some(OwnerAddr([0xbb; 16])),
        is_invite_only: Some(false),
        shared_in_profile: false,
        pending_join_at: None,
    }
}

/// Default debounce window between a `notify_dirty` and the resulting
/// state-root publish. Mirrors `owner_state_sync::DEFAULT_DEBOUNCE_MS`
/// (250 ms) — small enough to feel near-instant to a human, large
/// enough to collapse keystroke-rate mutations into one publish.
pub const DEFAULT_DEBOUNCE_MS: u64 = 250;

/// ZEB-434 D5: delay before the boot-time unconditional root flush —
/// same value as `mint_sync::DEFAULT_BOOT_FLUSH_DELAY_MS` (500 ms,
/// long enough for the zenoh adapter to wire up, short enough to beat
/// any human interaction).
pub const COMMUNITY_BOOT_FLUSH_DELAY_MS: u64 = 500;

/// ZEB-262 Phase 4: shared per-EventId oneshot map. The
/// `CommunitySyncRegistry` owns the `Arc` and exposes
/// `register_pending_redemption` / `take_pending_redemption` /
/// `notify_pending_redemption` for the IPC side; spawned engines
/// receive a clone of the same `Arc` so their post-`Inserted` hooks
/// (`insert_local_event`, `handle_incoming_publish`) can fire any
/// matching oneshot WITHOUT needing a back-reference to the registry
/// (avoids the `Arc<Self>` / `Weak<Self>` cycle dance and the async-
/// callback-typing problem — `oneshot::Sender::send` is sync, so the
/// engine takes the lock, removes, drops the guard, then sends).
///
/// **Lock-discipline:** the map is held under a `tokio::sync::Mutex`.
/// Callers MUST drop the guard before any `.await` on the recovered
/// `Sender`. The `send(())` call is itself sync; the helpers below
/// (and the engine call sites) consistently take-then-drop-then-send
/// to keep this invariant local to the lookup-and-fire pattern.
pub type PendingRedemptionMap = std::sync::Arc<
    tokio::sync::Mutex<
        std::collections::HashMap<
            crate::community_membership::EventId,
            tokio::sync::oneshot::Sender<()>,
        >,
    >,
>;

/// ZEB-254 Task 11: callback emitted when a `JoinCountersign` targeting a
/// self-authored `PendingJoin` lands in the joiner's engine. Receives the
/// community `SpaceId` (for routing) and the community name (for the
/// `nav-updated` payload). Production wires a closure that calls
/// `app.emit("nav-updated", ...)` on the Tauri `AppHandle`; tests pass
/// `None` (no-op).
pub type NavPendingClearEmitter = std::sync::Arc<dyn Fn(SpaceId, String) + Send + Sync>;

/// Fire any oneshot registered against `event_id` in `pending`.
/// No-op if no registration exists. Lock is held only across `remove`;
/// the `send(())` happens after the guard is dropped.
async fn notify_pending_redemption_in_map(
    pending: &PendingRedemptionMap,
    event_id: &crate::community_membership::EventId,
) {
    let sender = {
        let mut g = pending.lock().await;
        g.remove(event_id)
    };
    if let Some(tx) = sender {
        // tx.send(()) returns Result<(), ()> — error means the
        // receiver was dropped (timeout fired before us). Either way
        // the oneshot is consumed; we've satisfied our notify
        // contract.
        let _ = tx.send(());
    }
}

#[derive(thiserror::Error, Debug)]
pub enum CommunitySyncError {
    #[error("crypto: {0}")]
    Crypto(#[from] CommunityCryptoError),
    #[error("CBOR encode: {0}")]
    CborEncode(String),
    #[error("CBOR decode: {0}")]
    CborDecode(String),
    #[error("content store: {0}")]
    ContentStore(#[from] crate::content_store::ContentStoreError),
    #[error("transport channel closed")]
    TransportClosed,
    #[error("persist: {0}")]
    Persist(String),
    /// ZEB-463: a persist whose target directory was missing (io
    /// `NotFound`) — the parent dir was removed out from under the write,
    /// typically by a concurrent spawn-rollback
    /// (`shutdown_engine_and_cleanup_persistence`) discarding a
    /// freshly-spawned engine. Distinct from `Persist` so the graceful-
    /// shutdown arm can CAUSALLY downgrade ONLY this case to `Ok` (the data
    /// is being intentionally discarded) while every real durability fault —
    /// disk full, permissions — still propagates loudly (ZEB-460). Qodo
    /// (PR #267) flagged the earlier dir-exists heuristic as non-causal.
    #[error("persist: directory missing: {0}")]
    PersistDirMissing(String),
    /// ZEB-732: a node-generation re-check inside
    /// `shutdown_engine_and_cleanup_persistence` failed, so the destructive
    /// `remove_dir_all` was ABORTED rather than run. `stop_engine().await`
    /// opens a window in which a concurrent `stop_node`/`start_node` can bump
    /// `NodeState.generation` and install a fresh live community for this id;
    /// deleting its dir would be data loss. Distinct from `Persist` because
    /// nothing failed on disk — the cleanup was intentionally skipped.
    ///
    /// The caller path (`cleanup_community_data`) treats this like any other
    /// cleanup miss: warn-and-continue, best-effort — identical to how a failed
    /// unlink is handled (the owner-state tombstone still blocks resurrection,
    /// and an explicit later `remove_space` retries the on-disk cleanup via the
    /// idempotent already-tombstoned path). The EARLY generation check in
    /// `cleanup_community_data_if_durable` / `clear_space_local_cache_impl`
    /// still surfaces a *pre*-cleanup generation change to the caller as `Err`;
    /// only the deep in-flight abort is swallowed here (Qodo #2: intentional,
    /// consistent with the established best-effort cleanup semantics). Carries
    /// the reason.
    #[error("cleanup aborted: {0}")]
    CleanupAborted(String),
    /// Decoded blob's `community_id` doesn't match the engine's
    /// expected community. Distinct from `CborDecode` because the
    /// wire form parsed cleanly — the failure is routing/integrity,
    /// not malformed bytes. Surfacing it as `wire_decode_failed`
    /// would misdirect operators chasing format bugs.
    #[error("misrouted blob: expected community_id {expected:?}, got {found:?}")]
    MisroutedBlob { expected: SpaceId, found: SpaceId },
    /// CAS returned `Ok(None)` for the published `root_cid` — the slot
    /// is unpopulated (fetch timed out or admit-rejected). Distinct
    /// from `ContentStore` (which carries an actual transport / disk
    /// `ContentStoreError::Io`) because the failure class is "blob
    /// not yet available," not an I/O fault. Surfacing it as `Io`
    /// would misdirect operators chasing disk / network bugs and
    /// muddy any future retry-vs-give-up logic.
    #[error("blob not found in CAS for cid {cid:?} (fetch timeout or admit-rejected)")]
    BlobNotFound { cid: ContentId },

    /// Publish was signed correctly but the publisher's membership
    /// state at the publish HLC does NOT have status `Joined`. Either
    /// they were kicked, banned, never joined, or are still pending
    /// invitation. Tracker NOT advanced — defends against the
    /// post-kick censorship attack where a kicked-but-still-keyed
    /// member tries to squat HLC slots until ZEB-249 (key rotation)
    /// lands.
    #[error(
        "publisher {addr:?} not joined at publish HLC \
         (status: {status:?}, left_at: {left_at:?})"
    )]
    PublisherNotJoined {
        addr: OwnerAddr,
        status: crate::community_membership::MemberStatus,
        /// `MemberState.left_at` field — set on both Leave and Kick
        /// events (the underlying CRDT field is overloaded). For
        /// `PublisherNotJoined` triggered by a kick this carries the
        /// kick HLC; for one triggered by a voluntary Leave-then-
        /// republish this carries the Leave HLC. `None` when the
        /// publisher was never a member.
        left_at: Option<Hlc>,
    },

    /// Ed25519 signature over `canonical_cbor(CommunityRootSignedPayload)`
    /// did not validate against the resolved identity_pub. This is
    /// the load-bearing defense against the spoofing attack: a
    /// malicious member with the `EpochKey` cannot forge a
    /// publish claiming another member's `publisher_addr` because
    /// they don't have that member's signing key. Tracker NOT
    /// advanced.
    #[error("publisher signature invalid for addr {addr:?}")]
    PublisherSigInvalid { addr: OwnerAddr },

    /// `encode_root_packet` detected an epoch change between the pre-snapshot
    /// key read and the post-snapshot key read on every retry attempt.
    /// This should be unreachable in a correct cluster (rotations are rare
    /// and bounded by the number of members); if it fires it indicates
    /// continuous rapid epoch rotation, which is a bug or an adversarial
    /// condition. ZEB-249 PR #106 R5 (CodeRabbit Critical).
    #[error(
        "encode_root_packet: epoch changed on every retry attempt (5); \
         encode aborted to prevent encrypting post-rotation snapshot \
         under pre-rotation key"
    )]
    PublishRetryExhausted,

    /// CR Critical (PR #106 R7): `live_epoch_key` was called with
    /// `crdt_state = Some(...)` (owner-state IS wired) but the Space entry
    /// or its epoch fields are absent / incomplete.  Silently falling back to
    /// the spawn-time key would reopen the §10.6 backward-secrecy gap, so
    /// this is surfaced as an error instead.
    ///
    /// `crdt_state = None` (test/legacy mode) still takes the explicit
    /// fallback path and never produces this error.
    #[error(
        "live_epoch_key: crdt_state is wired but Space/epoch is incomplete for community {0:?}; \
         refusing to fall back to spawn-time key (would reopen §10.6 backward-secrecy gap)"
    )]
    LiveEpochKeyMissing(SpaceId),
}

/// Failure modes specific to `CommunitySyncEngine::insert_local_event`.
/// Distinct enum (not a variant on `CommunitySyncError`) because local-
/// insert failures are caller-driven (bad event from IPC) rather than
/// transport / crypto class — the IPC layer needs to surface them as
/// distinct error strings to the frontend.
#[derive(thiserror::Error, Debug)]
pub enum LocalInsertError {
    /// Defense-in-depth guard at `insert_local_event` entry — caller
    /// passed an event whose embedded `community_id` does not match the
    /// engine's configured `community_id`. Without this guard the misroute
    /// would silently surface as a verify rejection (`expected_community_id`
    /// mismatch), which is harder to diagnose. Surfacing it as a distinct
    /// error class lets the IPC layer return a clear "wrong community"
    /// diagnostic to the frontend.
    #[error(
        "event community_id {got:?} doesn't match engine's configured community_id {expected:?}"
    )]
    WrongCommunity { expected: SpaceId, got: SpaceId },
    /// C5: `insert_local_event_pair` pre-validation rejected the first event
    /// before any state mutation. Neither event was inserted.
    #[error("pair rejected at first event pre-validation: {0}")]
    FirstPreValidationFailed(String),
    /// C5: `insert_local_event_pair` pre-validation rejected the second event
    /// (first passed). Neither event was inserted — atomicity preserved.
    #[error("pair rejected at second event pre-validation: {0}")]
    SecondPreValidationFailed(String),
    /// ZEB-583: a local channel-create insert was rejected by its same-lock
    /// name-uniqueness precheck — a live (non-tombstoned) channel already
    /// carries this normalized name. Returned BEFORE any state mutation
    /// (nothing was inserted). This is a LOCAL fast-fail only: remote/sync
    /// events are never prechecked, because a receive-order-dependent
    /// verify-time name gate would diverge the log across replicas. The
    /// Display text is the user-facing IPC message.
    #[error("a channel named '{display}' already exists in this community")]
    DuplicateChannelName { display: String },
    /// ZEB-712: the engine's internal task has begun (or completed) shutdown —
    /// its final flush is the last persist/publish this engine will ever run.
    /// Accepting the event would sign it into engine memory that no task will
    /// persist or publish while the IPC reports success — the silent-loss
    /// window behind the lib.rs registry-detach fences (a lifecycle IPC that
    /// passed its re-lock fence can still reach a snapshot Arc of an engine
    /// `stop_inner` has since flushed). Checked under the same `state` lock
    /// the shutdown arm sets the flag under, so an insert either lands before
    /// the flag (and is included in the final flush — durable) or gets this
    /// error; there is no silent third outcome.
    #[error("community engine is shutting down (node stopped?)")]
    EngineShuttingDown,
}

/// ZEB-583: an optional check run under the SAME `state` lock guard as the
/// append in `insert_event_with_resolved_pubs`, IMMEDIATELY before
/// `insert_event`. Binding the check and the commit to one lock acquisition
/// makes them atomic, so two concurrent local `create_channel` IPC calls
/// can't both observe "no duplicate" and both succeed (a TOCTOU the older
/// check-then-insert-in-separate-locks guard left open). Used only by local
/// IPC inserts that must fail-fast on a precondition the CRDT itself
/// deliberately does NOT enforce. Remote/sync inserts pass `None`.
enum LocalInsertPrecheck {
    /// Reject if a live (non-tombstoned) channel already has `normalized`
    /// (its name `trim().to_lowercase()`-ed) as its name. `display` is the
    /// trimmed name carried for the error message.
    UniqueLiveChannelName { normalized: String, display: String },
}

impl LocalInsertPrecheck {
    fn run(&self, state: &CommunityState, admin_addr: OwnerAddr) -> Result<(), LocalInsertError> {
        match self {
            LocalInsertPrecheck::UniqueLiveChannelName {
                normalized,
                display,
            } => {
                let materialized = state.materialized(admin_addr);
                let dup = materialized.channels.values().any(|ch| {
                    ch.deleted_at.is_none() && ch.name.trim().to_lowercase() == *normalized
                });
                if dup {
                    return Err(LocalInsertError::DuplicateChannelName {
                        display: display.clone(),
                    });
                }
                Ok(())
            }
        }
    }
}

/// Per-publisher-device latest-accepted HLC, namespaced by publisher
/// `OwnerAddr`. ZEB-256: re-keyed from `BTreeMap<String, Hlc>` so a
/// member cannot squat another member's HLC slot via shared
/// `EpochKey`. Each publisher's address gets its own per-device
/// namespace, so a malicious Alice cannot squat Bob's HLC slot even if
/// she emits a publish carrying `at.device_id == bob_dev`.
///
/// `Serialize` / `Deserialize` are derived so
/// `community_state_persist::save_replay` can canonical-CBOR-encode the
/// tracker to `replay.cbor`. The `(OwnerAddr, String)` tuple key
/// serialises as a CBOR 2-array — `BTreeMap` iteration is by key order,
/// so the encoded form is deterministic.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct CommunityRootHlcTracker {
    /// Per-(publisher_addr, device_id) latest-accepted HLC. New incoming
    /// root publishes are accepted only if STRICTLY NEWER than the
    /// recorded entry for the same `(addr, device_id)`.
    pub per_device: BTreeMap<(OwnerAddr, String), Hlc>,
}

impl CanonicalPayloadSealed for CommunityRootHlcTracker {}
impl CanonicalPayload for CommunityRootHlcTracker {}

impl CommunityRootHlcTracker {
    /// Test the candidate HLC against the per-(addr, device) latest.
    /// Returns `true` if the candidate strictly dominates the recorded
    /// entry (or there is none); `false` otherwise.
    ///
    /// Does NOT mutate — `record` is a separate step the caller invokes
    /// after the rest of the receive pipeline succeeds. The split
    /// implements the "advance-after-success" idiom that owner-state's
    /// call sites apply manually to a bare BTreeMap.
    pub fn would_accept(&self, publisher_addr: &OwnerAddr, candidate: &Hlc) -> bool {
        let key = (*publisher_addr, candidate.device_id.clone());
        match self.per_device.get(&key) {
            None => true,
            Some(prev) => candidate.is_strictly_newer_than(prev),
        }
    }

    /// Record `candidate` as the latest-accepted HLC for
    /// `(publisher_addr, candidate.device_id)`.
    ///
    /// Precondition: caller MUST have just verified `would_accept`
    /// returned `true`. We `debug_assert!` the precondition so a
    /// buggy call site surfaces in dev/test rather than silently
    /// no-opping (which would mask the bug). In release builds the
    /// insert is unconditional — at this point the caller has
    /// committed to advancing and a backward-jump indicates upstream
    /// state corruption that no amount of guarding here can repair.
    pub fn record(&mut self, publisher_addr: OwnerAddr, candidate: Hlc) {
        debug_assert!(
            self.would_accept(&publisher_addr, &candidate),
            "CommunityRootHlcTracker::record called without would_accept check; \
             backward-jump for ({:?}, {})",
            publisher_addr,
            candidate.device_id
        );
        let key = (publisher_addr, candidate.device_id.clone());
        self.per_device.insert(key, candidate);
    }
}

/// Filesystem paths for the per-community CRDT + replay-tracker
/// snapshots. Lives here rather than in `community_state_persist` so
/// the engine config can construct it without depending on the
/// persist module's types — the persist module is path-agnostic and
/// operates on whatever `&Path` the engine hands it.
#[derive(Debug, Clone)]
pub struct PersistPaths {
    pub crdt: PathBuf,
    pub replay: PathBuf,
}

/// Resolves an `OwnerAddr` -> 64-byte identity_pub at receive-side
/// `verify_event` time. Production implementation wraps Sub-A's
/// owner-device cache (Task 13's `OwnerDeviceCacheResolver`); tests
/// use a static mapping.
///
/// **Async by design.** Earlier the trait was synchronous, which forced
/// the production resolver to use `try_lock()` over the async
/// `Mutex<OwnerState>`. Lock contention then collapsed to `None`, and
/// the receive pipeline interpreted that as "unknown actor" — it had
/// already advanced the per-device replay tracker, so the dropped
/// events became unrecoverable until a strictly-newer publish arrived.
/// Making the trait async lets the production resolver wait on the
/// real lock, so contention now produces a brief await instead of
/// silently discarding a valid publish.
#[async_trait::async_trait]
pub trait IdentityResolver: Send + Sync {
    async fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]>;
}

/// One degraded-path report from an engine. Sent on the engine's
/// `error_tx` channel to a registry-level receiver, which translates
/// each report into a `community-state-sync-degraded` Tauri IPC event
/// (Task 13 wires the receiver). Decoupling the engine from the
/// `tauri::AppHandle` keeps the CRDT layer Tauri-agnostic and makes
/// the engine unit-testable without spinning up a Tauri runtime.
#[derive(Debug, Clone)]
pub struct CommunityDegradedReport {
    pub community_id: SpaceId,
    /// Short tag identifying the failure class. Stable across versions
    /// so the frontend's banner copy can switch on it. Examples:
    /// "decrypt_failed", "blob_fetch_failed", "verify_event_rejected",
    /// "wire_decode_failed", "subscriber_channel_closed".
    pub reason_tag: &'static str,
    /// Human-readable detail. Not localised; surfaced to the frontend
    /// for telemetry / debug display rather than user-facing copy.
    pub detail: String,
}

/// Membership-CRDT mutation surfaced from the engine to the IPC layer.
/// Fired on every `InsertOutcome::Inserted` — covers both the engine's
/// receive pipeline (DAG-synced events from peers) AND IPC-driven local
/// inserts via `CommunitySyncEngine::insert_local_event`.
///
/// Shipped as a flat `event` clone rather than a delta-typed payload
/// because the consumer (Phase 3's start_node delta task) needs the
/// event's `kind`, `actor`, `at`, and (for Kick) `reason` to build the
/// `community-members-changed` Tauri event payload — and shipping the
/// signed event is cheap (a few hundred bytes) and avoids duplicating
/// the per-kind switch inside the engine.
#[derive(Debug, Clone)]
pub struct CommunityMembershipDelta {
    pub community_id: SpaceId,
    pub event: SignedMembershipEvent,
}

/// ZEB-434 D1/D2: a state-root query-serve request. The queryable task
/// in event_loop sends one per inbound zenoh query; the engine's
/// single-writer task replies with a fresh wire packet (or an error
/// string for logging).
pub type RootServeRequest = tokio::sync::oneshot::Sender<Result<Vec<u8>, String>>;

/// ZEB-438: the three engine-side catch-up channels ZEB-434 added to the
/// community-engine spawn path, bundled so `spawn_engine_inner_now` /
/// `spawn_engine_with_guard` don't carry them as three loose positional
/// `Option`s. Legacy/test callers that want no catch-up wiring pass
/// [`CatchUpChannels::none()`] instead of `None, None, None` — which also
/// self-documents the intent at the call site.
///
/// These are strictly the halves the *engine* consumes; the matching
/// *adapter* halves (`root_serve_tx`, `fetch_request_rx`) stay explicit on
/// `spawn_engine_with_guard` because they're required and flow into the
/// `CommunityAdapterRequest`, not the engine.
#[derive(Default)]
pub struct CatchUpChannels {
    /// Engine half of the state-root queryable-serve channel (ZEB-434
    /// D1/D2). `Some` wires the queryable; `None` for legacy/test callers.
    pub root_serve_rx: Option<mpsc::Receiver<RootServeRequest>>,
    /// Engine→adapter root-fetch request sender (ZEB-434 D3/D4). `Some`
    /// spawns the per-community `run_root_fetch_driver`; `None` skips it.
    pub fetch_request_tx: Option<mpsc::Sender<crate::event_loop::CommunityRootFetchRequest>>,
    /// Transport-epoch re-arm watch for the root-fetch driver. `None` in
    /// legacy/test callers or the restart-race window.
    pub transport_epoch_rx: Option<tokio::sync::watch::Receiver<u64>>,
}

impl CatchUpChannels {
    /// No catch-up wiring: every channel `None`. Replaces the historical
    /// `None, None, None` positional trio at legacy/test call sites.
    pub fn none() -> Self {
        Self::default()
    }
}

#[cfg(test)]
mod catch_up_channels_tests {
    use super::CatchUpChannels;

    #[test]
    fn none_sets_every_channel_to_none() {
        let c = CatchUpChannels::none();
        assert!(c.root_serve_rx.is_none());
        assert!(c.fetch_request_tx.is_none());
        assert!(c.transport_epoch_rx.is_none());
    }
}

/// Construction-time config bag for `CommunitySyncEngine::new`. Bundles
/// the per-community key + identity, the shared CRDT + tracker arcs,
/// the wire channels, the persist paths, and the optional degraded-path
/// reporter. Bag form keeps the constructor signature manageable — the
/// owner-state engine has 9 positional args and is already at the limit.
pub struct CommunitySyncEngineConfig {
    pub community_id: SpaceId,
    pub membership_key: EpochKey,
    pub admin_addr: OwnerAddr,
    /// Whether this community requires invite-only counter-sigs on
    /// non-admin Joins. Plumbed into `VerifyContext` at receive time
    /// (Task 8 consumes this). Defaults to `false` for tests that
    /// don't exercise the invite-only path.
    pub is_invite_only: bool,
    pub device_id: String,
    /// Owner address of the local member. Embedded in every publish so
    /// receivers can verify the signature against the right
    /// identity_pub (resolved via `IdentityResolver`, NOT carried
    /// inline). Also used by `next_hlc` to namespace tracker entries.
    pub self_owner: OwnerAddr,
    /// Local Ed25519 signing key for state-root publish signing. Same
    /// handle Phase 3's `insert_local_event` already uses for membership
    /// event signing — sourced from the local `PrivateIdentity` at
    /// engine spawn time. Wrapped in `Arc` so the engine + every
    /// internal task share the same key without copying the secret.
    pub signing_key: std::sync::Arc<ed25519_dalek::SigningKey>,
    pub state: Arc<Mutex<CommunityState>>,
    pub tracker: Arc<Mutex<CommunityRootHlcTracker>>,
    pub content_store: Arc<dyn ContentStore>,
    pub publisher_tx: mpsc::Sender<Vec<u8>>,
    pub subscriber_rx: mpsc::Receiver<Vec<u8>>,
    pub paths: PersistPaths,
    pub debounce_ms: u64,
    /// Resolver for `OwnerAddr` -> 64-byte identity_pub at receive-side
    /// `verify_event` time. `None` means receive-side verify will skip
    /// every event (with a `tracing::warn`) — acceptable for Task 6/7
    /// tests that exercise the publish path only; Task 8's tests must
    /// supply a `Some(resolver)`.
    pub identity_resolver: Option<Arc<dyn IdentityResolver>>,
    /// Channel for degraded-path reports. Cloned by the registry from
    /// a single shared receiver lived in `start_node` (Task 13). `None`
    /// means degraded paths log via `tracing::warn!` only — acceptable
    /// for tests that don't assert on IPC-event emission.
    pub error_tx: Option<mpsc::Sender<CommunityDegradedReport>>,
    /// Optional sink for membership CRDT mutations. Best-effort
    /// `try_send`; a closed or full channel surfaces as a dropped delta
    /// (the IPC consumer is purely informational, so back-pressuring
    /// the engine on a stuck consumer is wrong).
    pub delta_tx: Option<mpsc::Sender<CommunityMembershipDelta>>,
    /// ZEB-262 Phase 4: shared map of pending `redeem_invite` oneshots,
    /// keyed by the joiner's bootstrap_join `EventId`. The registry's
    /// `spawn_engine` clones its own `Arc` into this field so the
    /// engine's post-`Inserted` hooks can fire matching oneshots
    /// without a back-reference to the registry. `None` for tests /
    /// pre-Phase-4 callers that don't exercise the redeem path.
    pub pending_redemptions: Option<PendingRedemptionMap>,

    /// ZEB-249 §10.6 (Phase A): live reference to the owner-state CRDT.
    /// When `Some`, `publish_root_now` reads the current epoch key from
    /// `spaces[community_id].current_epoch_key` rather than the
    /// spawn-time captured `membership_key`. `handle_incoming_publish`
    /// similarly uses the live key (with fallback to `old_epoch_keys`
    /// keyed on `payload.epoch`). `None` for tests and for call sites
    /// that haven't threaded crdt_state through yet; those fall back to
    /// the captured `membership_key`.
    ///
    /// Lock-order note: the engine NEVER holds both the community-state
    /// mutex and the owner-state mutex at the same time. In
    /// `publish_root_now`, the owner-state lock is released before the
    /// community-state snapshot is taken. In `handle_incoming_publish`,
    /// the owner-state lock is released before the community-state merge
    /// lock is acquired.
    pub crdt_state: Option<Arc<Mutex<crate::owner_state_crdt::OwnerState>>>,

    /// ZEB-254: 64-byte composite identity pub of the community admin
    /// (`X25519_pub || Ed25519_pub`), as decoded from the invite URL
    /// (`CommunityInvitePayload.admin_identity_pub`). Carried on the
    /// config for API stability; the engine no longer threads it into a
    /// verify-path consumer (ZEB-339 moved PendingJoin verification to
    /// the carried EnrollmentCert / materialized enrolled_device_keys).
    /// `None` for engines that never see an invite payload (admin's own
    /// `create_community`, boot reconcile, open communities).
    pub admin_identity_pub: Option<[u8; 64]>,

    /// ZEB-254 Task 11: callback fired when a `JoinCountersign` targeting a
    /// self-authored `PendingJoin` lands (joiner-side clear hook). Production
    /// wires a `tauri::AppHandle` closure that emits `nav-updated { pending:
    /// false }`. `None` for admin engines and for tests that don't assert on
    /// IPC events.
    pub nav_emitter: Option<NavPendingClearEmitter>,

    /// ZEB-434 D1/D2: receive half of the state-root query-serve
    /// channel. The event_loop queryable task holds the sender and
    /// forwards one `RootServeRequest` per inbound zenoh query; the
    /// engine's single-writer task replies with a freshly encoded wire
    /// packet. `None` for engines without the catch-up pull plane
    /// wired (legacy callers, most tests).
    pub root_serve_rx: Option<mpsc::Receiver<RootServeRequest>>,
}

/// Per-community state-CRDT sync engine. Owns a tokio task that
/// (Task 7+) runs the debounce timer + publisher + subscriber +
/// persistence flushes for one joined community. Construction spawns
/// the task; `shutdown().await` stops it cleanly with one final flush.
pub struct CommunitySyncEngine {
    notify_dirty: Arc<Notify>,
    /// Set by `notify_dirty()`; cleared by the task after each publish.
    /// Prevents the shutdown path from emitting a spurious publish when
    /// the `Notify` permit was left over from before the most-recent
    /// actual publish.
    has_pending_dirty: Arc<AtomicBool>,
    /// ZEB-712 closing guard. Set under the `state` lock by the internal
    /// task's shutdown arm BEFORE the final publish/persist (and
    /// belt-and-suspenders by `shutdown()` itself for paths where the arm
    /// never runs). The local-insert entry points check it under the same
    /// lock before appending, so every insert racing a shutdown resolves
    /// to exactly one of: landed before the flag → included in the final
    /// flush (durable), or landed after → `EngineShuttingDown`. Mirrors
    /// the ZEB-248 channel-log `closing` guard.
    closing: Arc<AtomicBool>,
    flush_now_tx: mpsc::Sender<tokio::sync::oneshot::Sender<Result<(), CommunitySyncError>>>,
    /// ZEB-462 B: publish-INDEPENDENT durable persist. Routes a `persist_now`
    /// request through the single-writer task (same discipline as `flush_now`)
    /// which `persist_both`s WITHOUT publishing — fences the community
    /// membership CRDT to disk on join-commit so a crash before the next
    /// debounce can't lose it.
    persist_now_tx: mpsc::Sender<tokio::sync::oneshot::Sender<Result<(), CommunitySyncError>>>,
    /// Carries `Result<(), CommunitySyncError>` so the final publish +
    /// persist errors propagate to the caller rather than being silently
    /// swallowed by `()`. Mirrors `owner_state_sync::SyncEngine`'s
    /// shape exactly.
    shutdown_tx: mpsc::Sender<tokio::sync::oneshot::Sender<Result<(), CommunitySyncError>>>,
    task: Mutex<Option<JoinHandle<()>>>,
    /// Shared CRDT handle. Retained on the engine so the registry can
    /// expose it via `state_for` for test-only inspection without
    /// reaching into `InternalCtx`. Phase 3 ships the public IPC
    /// surface; this accessor stays `#[doc(hidden)]` until then.
    state: Arc<Mutex<CommunityState>>,
    /// Shared replay-tracker handle. Retained on the engine alongside
    /// `state` so the registry can expose a test-only snapshot accessor
    /// without reaching into `InternalCtx`. ZEB-256 Task 8: the
    /// `spoofed_publish_does_not_block_real_publisher` integration
    /// test inspects per-(addr, device_id) tracker entries to assert
    /// the receiver's tracker for the real publisher is NOT clobbered
    /// by a forged publish. `#[doc(hidden)]` until production callers
    /// need a public surface.
    tracker: Arc<Mutex<CommunityRootHlcTracker>>,
    /// Per-community symmetric key. Retained on the engine so the
    /// ZEB-270 Phase 3 channel-log registry (which derives per-channel
    /// keys via `derive_channel_key(membership_key, cid, chid)`) can
    /// reach it through `engine_arc(cid).membership_key()` without
    /// re-plumbing the value through `NodeState` or the registry.
    /// `EpochKey` derives `Clone` (just a 32-byte wrapper) so the
    /// accessor returns by value.
    membership_key: EpochKey,
    /// Community identity this engine was configured with. Bound at
    /// construction so `insert_local_event` can:
    ///   1. Reject mis-routed events (caller passed an event whose
    ///      `community_id` doesn't match this engine) with a clear
    ///      `LocalInsertError::WrongCommunity` rather than letting the
    ///      mismatch surface as an opaque verify rejection.
    ///   2. Bind `VerifyContext.expected_community_id` to the engine's
    ///      configured value rather than the (caller-controlled) event
    ///      payload — without this, a malicious or buggy IPC could
    ///      bypass the cross-community routing check by setting
    ///      `event.community_id == self.community_id` falsely.
    community_id: SpaceId,
    /// Admin OwnerAddr for the community. Retained so future read-side
    /// `materialize()` callers (Phase 3 IPC) can construct a
    /// `VerifyContext` without having to thread `admin_addr` through
    /// the registry separately.
    admin_addr: OwnerAddr,
    /// Resolver retained on the engine so `insert_local_event` can
    /// build a `VerifyContext` for locally-minted events without
    /// re-plumbing it through the registry. Cloned from the config
    /// alongside the spawned task's copy.
    identity_resolver: Option<Arc<dyn IdentityResolver>>,
    /// Invite-only flag retained on the engine for the same reason as
    /// `identity_resolver` — `insert_local_event` needs it to populate
    /// `VerifyContext`.
    is_invite_only: bool,
    /// Membership-delta sink retained on the engine so
    /// `insert_local_event` can emit a delta on `Inserted` outcomes,
    /// matching the receive pipeline's behaviour.
    delta_tx: Option<mpsc::Sender<CommunityMembershipDelta>>,
    /// ZEB-262 Phase 4: shared pending-redemption map. Cloned from the
    /// config so `insert_local_event` can fire any matching oneshot on
    /// `Inserted` outcomes (mirroring the receive pipeline's
    /// `handle_incoming_publish` hook — the spawned task carries its
    /// own clone via `InternalCtx`). `None` for engines spawned
    /// without a registry, e.g. legacy unit tests.
    pending_redemptions: Option<PendingRedemptionMap>,
    /// ZEB-254 Task 10: owner address of the local member, retained so
    /// `maybe_spawn_auto_counter_sign` can check self-eligibility and
    /// sign JoinCountersign events without reaching back into NodeState.
    self_owner: crate::owner_state_types::OwnerAddr,
    /// ZEB-254 Task 10: local Ed25519 signing key, retained so the
    /// auto-counter-sign spawned task can sign JoinCountersign events.
    signing_key: std::sync::Arc<ed25519_dalek::SigningKey>,
    /// ZEB-254 Task 10: stable device identifier, used as the `device_id`
    /// field in the auto-minted JoinCountersign event's HLC.
    device_id: String,

    /// ZEB-254 Task 11: callback for the joiner-side Space-pending-clear hook.
    /// Shared between the engine struct (for `insert_local_event` path) and
    /// the spawned `InternalCtx` task (for `handle_incoming_publish` path).
    /// `None` for admin engines and tests that don't assert on IPC events.
    nav_emitter: Option<NavPendingClearEmitter>,

    /// ZEB-254 Task 11: owner-state CRDT handle, retained on the engine so
    /// the `insert_local_event` path's pending-clear hook can update
    /// `Space.pending_join_at`. Mirrors the same Arc held by `InternalCtx`.
    /// `None` for engines that were spawned without a CRDT reference (tests
    /// and pre-ZEB-249 call sites).
    crdt_state: Option<Arc<Mutex<crate::owner_state_crdt::OwnerState>>>,
}

impl CommunitySyncEngine {
    /// Construct the engine and spawn its internal task. The task
    /// runs the debounced publish loop: `notify_dirty` arms the
    /// debounce timer, the timer fires `publish_root_now`,
    /// `flush_now` forces an immediate publish, and `shutdown`
    /// performs one final dirty-only publish before the task exits.
    /// The subscriber arm is a stub pending Task 8.
    pub fn new(cfg: CommunitySyncEngineConfig) -> Self {
        let notify_dirty = Arc::new(Notify::new());
        let has_pending_dirty = Arc::new(AtomicBool::new(false));
        let closing = Arc::new(AtomicBool::new(false));
        let (flush_now_tx, flush_now_rx) = mpsc::channel(8);
        // ZEB-462 B: publish-independent persist channel (join-commit fence).
        let (persist_now_tx, persist_now_rx) = mpsc::channel(8);
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

        // Clone the state Arc into the engine BEFORE moving cfg.state
        // into the InternalCtx — both the spawned task and the engine
        // accessor share the same underlying Mutex<CommunityState>.
        let state_for_engine = Arc::clone(&cfg.state);
        // ZEB-256 Task 8: same pattern for the tracker — engine retains
        // its own Arc so `tracker_arc()` (registry-side `tracker_snapshot`)
        // can hand out a snapshot without reaching into InternalCtx.
        let tracker_for_engine = Arc::clone(&cfg.tracker);
        let community_id_for_engine = cfg.community_id;
        let admin_addr = cfg.admin_addr;
        let identity_resolver_for_engine = cfg.identity_resolver.clone();
        let is_invite_only_for_engine = cfg.is_invite_only;
        let delta_tx_for_engine = cfg.delta_tx.clone();
        let pending_redemptions_for_engine = cfg.pending_redemptions.clone();
        // ZEB-270 Phase 3 Task 4.5: retain the membership_key on the
        // engine so the channel-log registry can reach it via
        // `engine_arc(cid).membership_key()`. Cheap clone (32-byte
        // wrapper); the spawned task gets its own clone via cfg.
        let membership_key_for_engine = cfg.membership_key.clone();
        // ZEB-249 §10.6: pass crdt_state to the spawned task (InternalCtx)
        // so publish_root_now / handle_incoming_publish can read the live
        // epoch key. ZEB-254 Task 11: the engine struct also retains a clone
        // so the insert_local_event path can fire the pending-clear hook.
        let crdt_state_for_engine = cfg.crdt_state.as_ref().map(Arc::clone);
        let crdt_state_for_task = cfg.crdt_state;

        // ZEB-254 Task 10: clone self-identity fields before moving cfg
        // into InternalCtx — the engine struct retains its own copies
        // for `maybe_spawn_auto_counter_sign`.
        let self_owner_for_engine = cfg.self_owner;
        let signing_key_for_engine = Arc::clone(&cfg.signing_key);
        let device_id_for_engine = cfg.device_id.clone();
        // ZEB-254 Task 11: nav_emitter is shared between the engine struct
        // (insert_local_event path) and the InternalCtx task
        // (handle_incoming_publish path). Clone the Arc so both share the
        // same callback; `None` for admin engines and tests.
        let nav_emitter_for_engine = cfg.nav_emitter.clone();
        let nav_emitter_for_task = cfg.nav_emitter;

        let task = tokio::spawn(internal_task(InternalCtx {
            community_id: cfg.community_id,
            membership_key: cfg.membership_key,
            admin_addr: cfg.admin_addr,
            is_invite_only: cfg.is_invite_only,
            device_id: cfg.device_id,
            self_owner: cfg.self_owner,
            signing_key: cfg.signing_key,
            state: cfg.state,
            tracker: cfg.tracker,
            content_store: cfg.content_store,
            publisher_tx: cfg.publisher_tx,
            subscriber_rx: cfg.subscriber_rx,
            paths: cfg.paths,
            debounce: std::time::Duration::from_millis(cfg.debounce_ms),
            notify_dirty: Arc::clone(&notify_dirty),
            has_pending_dirty: Arc::clone(&has_pending_dirty),
            closing: Arc::clone(&closing),
            flush_now_rx,
            persist_now_rx,
            shutdown_rx,
            identity_resolver: cfg.identity_resolver,
            error_tx: cfg.error_tx,
            delta_tx: cfg.delta_tx,
            pending_redemptions: cfg.pending_redemptions,
            crdt_state: crdt_state_for_task,
            nav_emitter: nav_emitter_for_task,
            root_serve_rx: cfg.root_serve_rx,
        }));

        Self {
            notify_dirty,
            has_pending_dirty,
            closing,
            flush_now_tx,
            persist_now_tx,
            shutdown_tx,
            task: Mutex::new(Some(task)),
            state: state_for_engine,
            tracker: tracker_for_engine,
            membership_key: membership_key_for_engine,
            community_id: community_id_for_engine,
            admin_addr,
            identity_resolver: identity_resolver_for_engine,
            is_invite_only: is_invite_only_for_engine,
            delta_tx: delta_tx_for_engine,
            pending_redemptions: pending_redemptions_for_engine,
            // ZEB-254 Task 10: retain self_owner / signing_key / device_id on
            // the engine so `maybe_spawn_auto_counter_sign` can build +
            // sign JoinCountersign events without back-referencing NodeState.
            self_owner: self_owner_for_engine,
            signing_key: signing_key_for_engine,
            device_id: device_id_for_engine,
            // ZEB-254 Task 11: retain nav_emitter for the insert_local_event path.
            nav_emitter: nav_emitter_for_engine,
            // ZEB-254 Task 11: retain crdt_state for the insert_local_event pending-clear hook.
            crdt_state: crdt_state_for_engine,
        }
    }

    /// Returns a clone of the inner `CommunityState` Arc. Test-only —
    /// production callers go through Phase 3's IPC layer
    /// (`materialize()` projection over a snapshot). Kept on the engine
    /// so the registry's `state_for` doesn't need to reach into
    /// `InternalCtx`.
    #[doc(hidden)]
    pub fn state(&self) -> Arc<Mutex<CommunityState>> {
        Arc::clone(&self.state)
    }

    /// Returns a clone of the engine's `CommunityRootHlcTracker` Arc.
    /// **Test-only** — production callers don't need to inspect the
    /// per-publisher replay tracker; ZEB-256 Task 8's spoofing
    /// integration test uses this to assert that a forged publish at
    /// HLC `huge` does NOT advance the receiver's tracker for the
    /// spoofed `publisher_addr`, so the real publisher's next legit
    /// publish at HLC `Y` (with `huge > Y`) is still admitted.
    #[doc(hidden)]
    pub fn tracker_arc(&self) -> Arc<Mutex<CommunityRootHlcTracker>> {
        Arc::clone(&self.tracker)
    }

    /// Returns the admin `OwnerAddr` this engine was configured with.
    /// Retained so Phase 3's IPC handlers can rebuild a `VerifyContext`
    /// for read-side `materialize()` without re-plumbing the field
    /// through the registry. Currently unused outside that future
    /// callsite; the integration tests don't read it.
    #[doc(hidden)]
    pub fn admin_addr(&self) -> OwnerAddr {
        self.admin_addr
    }

    /// Returns whether this community is invite-only (countersign-gated) vs
    /// open (tokenless `epoch_key`-capability join). The open-join acceptor
    /// (`iroh_invite_acceptor::handle_open_join_inbound`) reads this to REJECT a
    /// tokenless `OpenJoinRequest` against an invite-only community — an
    /// `epoch_key` holder must still go through the invite/countersign gate, so
    /// admitting an open-join here would bypass that gate. Mirrors
    /// [`Self::admin_addr`] (cheap field copy retained on the engine).
    pub fn is_invite_only(&self) -> bool {
        self.is_invite_only
    }

    /// ZEB-254 Task 10: when a `PendingJoin` event is freshly inserted,
    /// check self-eligibility and — if eligible — spawn a task that
    /// signs + inserts a `JoinCountersign` event.
    ///
    /// Eligibility conditions (all must hold):
    ///   1. The freshly-inserted event is a `PendingJoin`.
    ///   2. Self is currently `Joined` in the materialized state.
    ///   3. Self's power level ≥ `POWER_THRESHOLDS.invite`.
    ///   4. No self-authored `JoinCountersign` targeting this `PendingJoin`
    ///      already exists in the log (idempotency guard).
    ///
    /// Called from BOTH the local-insert path (`insert_event_with_resolved_pubs`)
    /// and the state-root-merge path (`handle_incoming_publish` via
    /// `maybe_spawn_auto_counter_sign_for_ctx`). Spawns asynchronously so
    /// it never blocks the insert caller.
    ///
    /// The spawned task inserts the `JoinCountersign` directly into the
    /// shared `CommunityState` Mutex rather than calling `insert_local_event`
    /// — this avoids an `Arc<Self>` back-reference on the engine and keeps
    /// the plumbing cycle-free (same pattern as `PendingRedemptionMap`).
    fn maybe_spawn_auto_counter_sign(
        &self,
        pending_event: &crate::community_membership::SignedMembershipEvent,
    ) {
        // Only act on PendingJoin.
        if !matches!(
            &pending_event.kind,
            crate::community_membership::MembershipEventKind::PendingJoin { .. }
        ) {
            return;
        }

        let pending_id = pending_event.id;
        let community_id = self.community_id;
        let self_owner = self.self_owner;
        let admin_addr = self.admin_addr;
        let signing_key = Arc::clone(&self.signing_key);
        let device_id = self.device_id.clone();
        let state = Arc::clone(&self.state);
        let identity_resolver = self.identity_resolver.clone();
        let is_invite_only = self.is_invite_only;
        let notify_dirty = Arc::clone(&self.notify_dirty);
        let has_pending_dirty = Arc::clone(&self.has_pending_dirty);
        // ZEB-712 (CodeRabbit #492 R1): the spawned task mutates state
        // directly, so it carries the closing fence too.
        let closing = Arc::clone(&self.closing);
        // R3 (C4): plumb the delta channel so the auto-counter-sign task
        // can emit CommunityMembershipDelta on Inserted.
        let delta_tx = self.delta_tx.clone();

        tokio::spawn(spawn_auto_counter_sign_task(
            pending_id,
            community_id,
            self_owner,
            admin_addr,
            signing_key,
            device_id,
            state,
            identity_resolver,
            is_invite_only,
            notify_dirty,
            has_pending_dirty,
            closing,
            delta_tx,
        ));
    }

    /// Returns this engine's per-community symmetric `EpochKey`.
    /// ZEB-270 Phase 3 Task 4.5: the channel-log registry's `spawn`
    /// derives a per-channel symmetric key via
    /// `derive_channel_key(membership_key, community_id, channel_id)`.
    /// The membership key is bound at `spawn_engine` time and never
    /// changes for the engine's lifetime, so handing out a clone is
    /// safe.
    pub(crate) fn membership_key(&self) -> EpochKey {
        self.membership_key.clone()
    }

    /// Returns a `CommunityStateAtHlc` adapter wrapping this engine's
    /// shared `Arc<Mutex<CommunityState>>` plus admin_addr. ZEB-270
    /// Phase 3 Task 4.5: the channel-log engine's verify chain
    /// (`verify_channel_event`) takes a `&dyn CommunityStateAtHlc`
    /// resolved at `event.at`; this accessor produces the production
    /// adapter that materializes the live CRDT to the requested HLC.
    pub(crate) fn state_at_hlc_resolver(
        &self,
    ) -> Arc<dyn crate::community_channel_log::CommunityStateAtHlc + Send + Sync> {
        Arc::new(CommunityStateAtHlcAdapter {
            state: Arc::clone(&self.state),
            admin_addr: self.admin_addr,
        })
    }

    // ZEB-399: the channel-log engine no longer needs an identity
    // resolver — `verify_channel_event` authenticates posts against the
    // author's materialized `enrolled_device_keys` (the community
    // membership trust root), not a DM-layer owner→identity cache. The
    // former `identity_resolver()` accessor + `ChannelIdentityResolverAdapter`
    // were removed. The engine's `identity_resolver` field remains for
    // epoch-key / seal-to-owner resolution (see `CommunityRegistry::
    // identity_resolver`).

    /// Insert a locally-minted event into the community CRDT, verify it
    /// using the engine's `identity_resolver`, fire the membership-delta
    /// channel on `Inserted`, and notify the publish loop so the change
    /// reaches peers.
    ///
    /// Centralises the local-mint path so every IPC that mutates this
    /// community's CRDT (`create_community`, `redeem_invite`,
    /// `leave_community`, and Phase 4's kick / set_power / invite-only
    /// redeem) shares a single verify-then-insert-then-emit-delta path.
    /// Without this method, each IPC would grow a copy of the dance and
    /// the delta-emission rule would inevitably drift on a new variant.
    ///
    /// `Ok(InsertOutcome::Inserted)` — event landed; delta fired; publish
    /// notified. `Ok(InsertOutcome::AlreadyKnown)` — duplicate; no delta,
    /// no publish-notify (the previous insert already did both).
    /// `Ok(InsertOutcome::Rejected(VerifyError))` — verify failed at the
    /// CRDT layer (banned-stickiness, signature mismatch, invite-only
    /// missing countersig, etc.). `Err(LocalInsertError::*)` — failure
    /// BEFORE we got far enough to call insert (event mis-routed to a
    /// different community's engine, no resolver, or resolver couldn't
    /// find the actor).
    pub async fn insert_local_event(
        &self,
        event: crate::community_membership::SignedMembershipEvent,
    ) -> Result<crate::community_state_crdt::InsertOutcome, LocalInsertError> {
        // Defense in depth: reject mis-routed events at the entry point
        // with a clear error class. The VerifyContext below ALSO binds
        // expected_community_id to self.community_id (NOT event.community_id),
        // so even without this guard the receive-side check would catch
        // the mismatch — but as an opaque verify rejection rather than a
        // routing diagnostic.
        if event.community_id != self.community_id {
            return Err(LocalInsertError::WrongCommunity {
                expected: self.community_id,
                got: event.community_id,
            });
        }

        // ZEB-339 Task 9: REMOVED the resolver gate. `verify_event` now
        // derives the actor's (and countersigner's) ed25519 verify key
        // itself — from the carried EnrollmentCert (Join/PendingJoin) or
        // the actor's materialized `enrolled_device_keys` (steady-state).
        // The old `OwnerDeviceCacheResolver` lookup MISSED for remote
        // `owner_id` actors (its cache is keyed by Reticulum device-hash,
        // not owner_id), wrongly rejecting them as unresolved actors. The
        // slim VerifyContext below carries no caller-resolved pubs.
        self.insert_event_with_resolved_pubs(event, None).await
    }

    /// ZEB-339 Task 9: retained as a thin wrapper for the invite-only
    /// bootstrap callers (`redeem_invite_inner`) that previously had to
    /// pass an inline `joiner_identity_pub` because the
    /// `OwnerDeviceCacheResolver` couldn't yet resolve a first-seen
    /// joiner. `verify_event` now derives signer keys from the carried
    /// EnrollmentCert / materialized membership, so the inline pubs are
    /// no longer needed — they are accepted and discarded to keep the
    /// call sites stable. Routes straight through the shared insert body.
    pub async fn insert_local_event_with_pubs(
        &self,
        event: crate::community_membership::SignedMembershipEvent,
        _actor_identity_pub: [u8; 64],
        _countersigner_identity_pub: Option<[u8; 64]>,
    ) -> Result<crate::community_state_crdt::InsertOutcome, LocalInsertError> {
        if event.community_id != self.community_id {
            return Err(LocalInsertError::WrongCommunity {
                expected: self.community_id,
                got: event.community_id,
            });
        }
        self.insert_event_with_resolved_pubs(event, None).await
    }

    /// ZEB-583: local channel-create insert with an atomic name-uniqueness
    /// precheck. The check — reject a duplicate LIVE (non-tombstoned)
    /// normalized name — runs under the SAME `state` lock as the append, so
    /// two concurrent local `create_channel` IPC calls can't both pass it and
    /// both append a same-named channel. This is a local fast-fail (like the
    /// empty/length checks at the IPC boundary), NOT a CRDT/verify gate:
    /// `verify_event` deliberately accepts duplicate channel names because a
    /// receive-order-dependent rejection would diverge the log across replicas.
    /// `normalized_name` is the candidate's `trim().to_lowercase()`;
    /// `display_name` is the trimmed name used in the error message. A
    /// duplicate surfaces as `LocalInsertError::DuplicateChannelName`.
    pub async fn insert_local_channel_create(
        &self,
        event: crate::community_membership::SignedMembershipEvent,
        normalized_name: String,
        display_name: String,
    ) -> Result<crate::community_state_crdt::InsertOutcome, LocalInsertError> {
        if event.community_id != self.community_id {
            return Err(LocalInsertError::WrongCommunity {
                expected: self.community_id,
                got: event.community_id,
            });
        }
        self.insert_event_with_resolved_pubs(
            event,
            Some(LocalInsertPrecheck::UniqueLiveChannelName {
                normalized: normalized_name,
                display: display_name,
            }),
        )
        .await
    }

    /// Shared body for `insert_local_event` and
    /// `insert_local_event_with_pubs` — runs the verify + state mutate +
    /// post-Inserted hook chain. ZEB-339 Task 9: no longer takes
    /// caller-resolved pubs; `verify_event` derives signer keys itself.
    async fn insert_event_with_resolved_pubs(
        &self,
        event: crate::community_membership::SignedMembershipEvent,
        precheck: Option<LocalInsertPrecheck>,
    ) -> Result<crate::community_state_crdt::InsertOutcome, LocalInsertError> {
        // Bind expected_community_id to the engine's configured value,
        // NOT the (caller-controlled) event payload. Without this, a
        // malicious or buggy IPC could bypass the cross-community routing
        // check by passing an event whose community_id matches what it
        // claims. The entry-point guard above gives a clearer error class
        // for the common honest mismatch case.

        // ZEB-339 Task 9: VerifyContext carries no caller-resolved pubs;
        // verify_event resolves the signer from the carried EnrollmentCert
        // (Join/PendingJoin) or the actor's materialized
        // enrolled_device_keys (steady-state).
        let ctx = crate::community_membership::VerifyContext {
            expected_community_id: self.community_id,
            admin_addr: self.admin_addr,
            is_invite_only: self.is_invite_only,
        };

        let outcome = {
            let mut state_g = self.state.lock().await;
            // ZEB-712: closing guard — checked under the same lock the
            // shutdown arm sets the flag under. See the field docs on
            // `CommunitySyncEngine::closing` for the race semantics.
            if self.closing.load(Ordering::SeqCst) {
                return Err(LocalInsertError::EngineShuttingDown);
            }
            // ZEB-583: run the optional precheck under the SAME lock guard,
            // immediately before the append, so the check + commit are atomic
            // (closes the create_channel TOCTOU where two concurrent local IPC
            // calls both observe "no duplicate"). Local-only — remote events
            // never carry a precheck (a verify-time name gate would diverge
            // replicas).
            if let Some(check) = &precheck {
                check.run(&state_g, self.admin_addr)?;
            }
            let outcome = state_g.insert_event(event.clone(), &ctx);
            // ZEB-712 (CodeRabbit #492 R1): latch the dirty flag under the
            // SAME lock as the mutation. The `notify_dirty()` in the
            // post-insert hooks runs after this lock is released — a
            // shutdown winning that gap would read `has_pending_dirty ==
            // false`, skip the final publish, and leave the (persisted)
            // event unreplicated until a peer pulls it. The in-lock latch
            // makes "state mutated" and "publish owed" atomic.
            if matches!(
                outcome,
                crate::community_state_crdt::InsertOutcome::Inserted
            ) {
                self.has_pending_dirty.store(true, Ordering::Relaxed);
            }
            outcome
        };

        // C1 restart-recovery: when a PendingJoin returns AlreadyKnown
        // (event was already in the CRDT from disk / prior session), still
        // schedule the counter-sign eligibility check. The spawned task
        // re-checks under the lock and returns immediately if a
        // JoinCountersign already exists for this target — fully idempotent.
        if matches!(
            outcome,
            crate::community_state_crdt::InsertOutcome::AlreadyKnown
        ) && matches!(
            &event.kind,
            crate::community_membership::MembershipEventKind::PendingJoin { .. }
        ) {
            self.maybe_spawn_auto_counter_sign(&event);
        }

        if matches!(
            outcome,
            crate::community_state_crdt::InsertOutcome::Inserted
        ) {
            // ZEB-254 Task 10: auto-counter-sign PendingJoin events. Spawns
            // asynchronously — does not block the insert caller or the IPC
            // response. Must fire before notify_dirty so the JoinCountersign
            // can land in the same debounce window as the PendingJoin.
            self.maybe_spawn_auto_counter_sign(&event);

            // ZEB-254 Task 11: joiner-side pending-join clear hook. Fires
            // when a JoinCountersign targeting a self-authored PendingJoin
            // lands. No-op for non-JoinCountersign events and for admin engines.
            maybe_spawn_pending_join_clear(
                &event,
                Arc::clone(&self.state),
                self.self_owner,
                self.community_id,
                self.crdt_state.clone(),
                self.nav_emitter.clone(),
            );

            // R4-3: when this freshly-inserted event is a self-authored
            // PendingJoin, rescan the log for an already-present
            // matching JoinCountersign. Out-of-order arrival
            // (JoinCountersign syncs BEFORE the joiner's own PendingJoin
            // syncs back to their device — e.g., two-admin race where
            // admin's countersign-publish reaches the joiner before the
            // joiner's own state-root publish round-trips) would
            // otherwise leave `Space.pending_join_at` set until the
            // next-restart C3 healing pass. The rescan is idempotent
            // (the pending-clear apply checks `pending_join_at.is_none()`
            // before mutating).
            maybe_spawn_pending_clear_rescan_for_pending_join(
                &event,
                Arc::clone(&self.state),
                self.self_owner,
                self.community_id,
                self.crdt_state.clone(),
                self.nav_emitter.clone(),
            );

            // ZEB-501: wake the joiner's redeem oneshot ONLY on a real
            // JoinCountersign (its `target_event_id` == the awaited
            // `bootstrap_join.id`). The legacy ZEB-262 notify-on-`event.id`
            // was satisfied by the joiner's OWN PendingJoin self-insert
            // (its id == the registered key), so the redeem never actually
            // waited for the admin's countersign — `pending` was always
            // false. The send is sync; the lock is released before we touch
            // any other channel.
            if let Some(pending) = self.pending_redemptions.as_ref() {
                if let crate::community_membership::MembershipEventKind::JoinCountersign {
                    target_event_id,
                } = &event.kind
                {
                    notify_pending_redemption_in_map(pending, target_event_id).await;
                }
            }
            if let Some(tx) = self.delta_tx.as_ref() {
                let _ = tx.try_send(CommunityMembershipDelta {
                    community_id: event.community_id,
                    event,
                });
            }
            self.notify_dirty();
        }

        Ok(outcome)
    }

    /// Atomic pair insert: inserts `first` then `second` under a single
    /// `state` lock hold, then emits both deltas and fires `notify_dirty`
    /// once.
    ///
    /// ZEB-249 Task 6 §4.1 atomicity: the kick+rotation pair (and the
    /// leave+rotation pair) must land together. Sequential
    /// `insert_local_event` calls have a crash window between the two
    /// writes — if a crash occurs after the Kick but before the Rotation
    /// is inserted, the CRDT has a Kick with no matching Rotation and
    /// `pending_rotation_for` remains set until the self-healing observer
    /// runs. This method collapses that window to zero at the cost of a
    /// slightly longer lock hold (both `CommunityState::insert_event`
    /// calls run under the same mutex guard, then the lock is released
    /// before the async delta-emit + oneshot-notify calls).
    ///
    /// Returns `(first_outcome, second_outcome)`.
    ///
    /// - If `first` returns `Rejected`, `second` is NOT inserted (the pair is
    ///   rejected together — caller should treat this as an atomic failure).
    /// - If `first` returns `Inserted` OR `AlreadyKnown`, `second` IS inserted
    ///   (idempotent retry: a re-issued kick or rotation finds the original
    ///   event in CRDT and proceeds with the paired event regardless).
    ///
    /// When `first` is `AlreadyKnown`, the sentinel `AlreadyKnown` is
    /// NOT returned for `second` — the second insert still runs so that
    /// partially-applied pairs from a previous crash are completed.
    pub async fn insert_local_event_pair(
        &self,
        first: crate::community_membership::SignedMembershipEvent,
        second: crate::community_membership::SignedMembershipEvent,
    ) -> Result<
        (
            crate::community_state_crdt::InsertOutcome,
            crate::community_state_crdt::InsertOutcome,
        ),
        LocalInsertError,
    > {
        use crate::community_state_crdt::InsertOutcome;

        // Route check for both events.
        if first.community_id != self.community_id {
            return Err(LocalInsertError::WrongCommunity {
                expected: self.community_id,
                got: first.community_id,
            });
        }
        if second.community_id != self.community_id {
            return Err(LocalInsertError::WrongCommunity {
                expected: self.community_id,
                got: second.community_id,
            });
        }

        // ZEB-339 Task 9: REMOVED the resolver gate for both events.
        // `verify_event` (called below in the pre-validate phase and again
        // inside `insert_event`) derives signer keys from the carried
        // EnrollmentCert / materialized membership — the resolver's
        // owner_id miss no longer rejects valid remote events.
        let first_ctx = crate::community_membership::VerifyContext {
            expected_community_id: self.community_id,
            admin_addr: self.admin_addr,
            is_invite_only: self.is_invite_only,
        };
        let second_ctx = crate::community_membership::VerifyContext {
            expected_community_id: self.community_id,
            admin_addr: self.admin_addr,
            is_invite_only: self.is_invite_only,
        };

        // C5: pre-validate BOTH events before any state mutation.
        // Acquires the lock once to run both verifications, then re-acquires
        // (under the same held guard) to insert. This collapses into a single
        // lock hold — the Mutex is held across verify + insert for both.
        let (first_outcome, second_outcome) = {
            let mut state_g = self.state.lock().await;

            // ZEB-712: closing guard — same placement as the single-insert
            // path: under the state lock, before any validation or mutation.
            if self.closing.load(Ordering::SeqCst) {
                return Err(LocalInsertError::EngineShuttingDown);
            }

            // Pre-validate first: compute prior state from current log.
            let first_log: Vec<crate::community_membership::SignedMembershipEvent> =
                state_g.events.values().cloned().collect();
            let first_prior = crate::community_membership::prior_state_at_event(
                &first_log,
                &first,
                self.admin_addr,
            );
            if let Err(e) =
                crate::community_membership::verify_event(&first, &first_prior, &first_ctx)
            {
                // Pre-validation failed — no mutation occurred. Return early.
                // We return Err so the caller knows NEITHER event landed
                // (atomicity preserved: first was not inserted).
                return Err(LocalInsertError::FirstPreValidationFailed(e.to_string()));
            }

            // Pre-validate second: simulate first already landed by building
            // a temporary log that includes first.
            let mut second_log = first_log;
            second_log.push(first.clone());
            let second_prior = crate::community_membership::prior_state_at_event(
                &second_log,
                &second,
                self.admin_addr,
            );
            if let Err(e) =
                crate::community_membership::verify_event(&second, &second_prior, &second_ctx)
            {
                // Second pre-validation failed — first was NOT inserted.
                // Atomicity: caller gets a clear error; CRDT is unchanged.
                return Err(LocalInsertError::SecondPreValidationFailed(e.to_string()));
            }

            // Both pass: now insert both under the same lock hold.
            let o1 = state_g.insert_event(first.clone(), &first_ctx);
            // Only insert second if first landed (AlreadyKnown = idempotent retry OK).
            let o2 = if matches!(o1, InsertOutcome::Inserted | InsertOutcome::AlreadyKnown) {
                state_g.insert_event(second.clone(), &second_ctx)
            } else {
                InsertOutcome::AlreadyKnown // sentinel: pair rejected at first (should not happen after pre-validation)
            };
            // ZEB-712 (CodeRabbit #492 R1): latch dirty under the mutation
            // lock — same rationale as the single-insert path.
            if matches!(o1, InsertOutcome::Inserted) || matches!(o2, InsertOutcome::Inserted) {
                self.has_pending_dirty.store(true, Ordering::Relaxed);
            }
            (o1, o2)
        };

        // Post-lock: emit deltas + oneshot notifications.
        if matches!(first_outcome, InsertOutcome::Inserted) {
            if let Some(pending) = self.pending_redemptions.as_ref() {
                notify_pending_redemption_in_map(pending, &first.id).await;
            }
            if let Some(tx) = self.delta_tx.as_ref() {
                let _ = tx.try_send(CommunityMembershipDelta {
                    community_id: first.community_id,
                    event: first,
                });
            }
        }
        if matches!(second_outcome, InsertOutcome::Inserted) {
            if let Some(pending) = self.pending_redemptions.as_ref() {
                notify_pending_redemption_in_map(pending, &second.id).await;
            }
            if let Some(tx) = self.delta_tx.as_ref() {
                let _ = tx.try_send(CommunityMembershipDelta {
                    community_id: second.community_id,
                    event: second,
                });
            }
        }
        // A single dirty notification suffices for both inserts.
        // Only fire on actual fresh insertions — AlreadyKnown idempotent
        // retries must NOT republish or bump state.
        if matches!(first_outcome, InsertOutcome::Inserted)
            || matches!(second_outcome, InsertOutcome::Inserted)
        {
            self.notify_dirty();
        }

        Ok((first_outcome, second_outcome))
    }

    /// Hint that local CRDT state has mutated and a debounced publish
    /// should fire after `debounce_ms`. Non-blocking.
    pub fn notify_dirty(&self) {
        self.has_pending_dirty.store(true, Ordering::Relaxed);
        self.notify_dirty.notify_one();
    }

    /// Force an immediate publish, bypassing the debounce window.
    /// Returns when the publish has been written to the outbound
    /// channel and any persistence flush has completed.
    pub async fn flush_now(&self) -> Result<(), CommunitySyncError> {
        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        self.flush_now_tx
            .send(resp_tx)
            .await
            .map_err(|_| CommunitySyncError::TransportClosed)?;
        resp_rx
            .await
            .map_err(|_| CommunitySyncError::TransportClosed)?
    }

    /// ZEB-462 B: durably persist the community membership CRDT to disk NOW,
    /// WITHOUT publishing. Routes through the single-writer task so it cannot
    /// race the debounced persist. Used by the redeem path to fence the
    /// just-inserted membership (admin bootstrap + self Join) on join-commit,
    /// so a crash before the next debounce can't lose it. CRDT-ONLY: writes
    /// `crdt.cbor` but NOT `replay.cbor` — fencing the tracker here could
    /// durably record an unpublished `next_hlc` advance left in memory by a
    /// prior failed publish (Cursor / CodeRabbit PR #253). Does NOT publish and
    /// does NOT clear the dirty bit — a pending state-root publish still fires
    /// on the next debounce.
    pub async fn persist_now(&self) -> Result<(), CommunitySyncError> {
        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        self.persist_now_tx
            .send(resp_tx)
            .await
            .map_err(|_| CommunitySyncError::TransportClosed)?;
        resp_rx
            .await
            .map_err(|_| CommunitySyncError::TransportClosed)?
    }

    /// Stop the internal task, flushing any pending writes first. If
    /// the engine task was already gone (channel closed before our send
    /// landed), returns `Ok(())` — there was nothing to flush.
    ///
    /// Mirrors `owner_state_sync::SyncEngine::shutdown` — we DO NOT
    /// `handle.await` the JoinHandle. Awaiting from a different runtime
    /// than the spawn-runtime risks deadlocking under future tokio
    /// releases; the `resp_rx.await` already gives the flush-complete
    /// guarantee.
    pub async fn shutdown(&self) -> Result<(), CommunitySyncError> {
        // ZEB-712 (CodeRabbit #492 R1): set `closing` BEFORE attempting the
        // shutdown send. If the internal task already exited (double
        // shutdown, task death), the send fails — and a store placed after
        // it would leave a gap where a concurrent insert observes
        // `closing == false` and appends to state no task will ever
        // persist. Setting first, under the state lock, fail-closes every
        // insert from this point on. Inserts that already hold (or win)
        // the lock land before the flag and are covered by the arm's
        // final flush; the arm's own store before that flush remains as
        // defense-in-depth for the ordering guarantee.
        {
            let _state_g = self.state.lock().await;
            self.closing.store(true, Ordering::SeqCst);
        }
        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        let result = if self.shutdown_tx.send(resp_tx).await.is_ok() {
            match resp_rx.await {
                Ok(inner) => inner,
                Err(_) => Err(CommunitySyncError::TransportClosed),
            }
        } else {
            Ok(())
        };
        let _ = self.task.lock().await.take();
        result
    }
}

/// Internal context bag passed to the spawned task. Tasks 7-8 wired
/// the publish loop and receive pipeline; Task 10 added the persist
/// hooks that consume `paths`. All fields are now read by the
/// `internal_task` `select!` loop or its helpers.
struct InternalCtx {
    community_id: SpaceId,
    membership_key: EpochKey,
    admin_addr: OwnerAddr,
    is_invite_only: bool,
    device_id: String,
    self_owner: OwnerAddr,
    signing_key: std::sync::Arc<ed25519_dalek::SigningKey>,
    state: Arc<Mutex<CommunityState>>,
    tracker: Arc<Mutex<CommunityRootHlcTracker>>,
    content_store: Arc<dyn ContentStore>,
    publisher_tx: mpsc::Sender<Vec<u8>>,
    subscriber_rx: mpsc::Receiver<Vec<u8>>,
    paths: PersistPaths,
    debounce: std::time::Duration,
    notify_dirty: Arc<Notify>,
    has_pending_dirty: Arc<AtomicBool>,
    /// ZEB-712: see `CommunitySyncEngine::closing`. The shutdown arm sets
    /// it under the `state` lock before the final flush.
    closing: Arc<AtomicBool>,
    flush_now_rx: mpsc::Receiver<tokio::sync::oneshot::Sender<Result<(), CommunitySyncError>>>,
    /// ZEB-462 B: publish-independent persist request channel (join-commit fence).
    persist_now_rx: mpsc::Receiver<tokio::sync::oneshot::Sender<Result<(), CommunitySyncError>>>,
    shutdown_rx: mpsc::Receiver<tokio::sync::oneshot::Sender<Result<(), CommunitySyncError>>>,
    identity_resolver: Option<Arc<dyn IdentityResolver>>,
    error_tx: Option<mpsc::Sender<CommunityDegradedReport>>,
    delta_tx: Option<mpsc::Sender<CommunityMembershipDelta>>,
    /// ZEB-262 Phase 4: shared pending-redemption map. Used by
    /// `handle_incoming_publish` to fire any oneshot registered
    /// against an Inserted event's id. `None` skips the notify path —
    /// safe for non-redeem callers.
    pending_redemptions: Option<PendingRedemptionMap>,

    /// ZEB-249 §10.6 (Phase A): live owner-state CRDT for current epoch
    /// key lookup. `None` for tests that use the spawn-time fallback.
    /// See `CommunitySyncEngineConfig.crdt_state` for the lock-order contract.
    crdt_state: Option<Arc<Mutex<crate::owner_state_crdt::OwnerState>>>,

    /// ZEB-254 Task 11: callback for the joiner-side Space-pending-clear hook.
    /// Shared with the engine struct via `Arc` clone so both paths observe the
    /// same configured callback. `None` for admin engines and tests.
    nav_emitter: Option<NavPendingClearEmitter>,

    /// ZEB-434 D2: query-serve request channel. `internal_task` takes
    /// it out of the ctx at start (`Option<Receiver>` can't be polled
    /// inside `select!` directly). `None` disables the serve arm.
    root_serve_rx: Option<mpsc::Receiver<RootServeRequest>>,
}

// ── ZEB-254 Task 10: auto-counter-sign helper ────────────────────────────────

/// Shared async body for the auto-counter-sign spawn: checks eligibility,
/// resolves self pub, builds + signs a `JoinCountersign`, and inserts it
/// directly into the shared `CommunityState`. Called from both the engine's
/// `maybe_spawn_auto_counter_sign` method and from `handle_incoming_publish`
/// (via `maybe_spawn_auto_counter_sign_for_ctx`) so the logic lives in one
/// place.
///
/// Uses `#[allow(clippy::too_many_arguments)]` because all parameters are
/// distinct load-bearing fields — a struct wrapper would not reduce clarity
/// here, and the function is only ever called from the two hook sites.
#[allow(clippy::too_many_arguments)]
async fn spawn_auto_counter_sign_task(
    pending_id: crate::community_membership::EventId,
    community_id: crate::owner_state_types::SpaceId,
    self_owner: crate::owner_state_types::OwnerAddr,
    admin_addr: crate::owner_state_types::OwnerAddr,
    signing_key: Arc<ed25519_dalek::SigningKey>,
    device_id: String,
    state: Arc<Mutex<CommunityState>>,
    _identity_resolver: Option<Arc<dyn IdentityResolver>>,
    is_invite_only: bool,
    notify_dirty: Arc<Notify>,
    has_pending_dirty: Arc<AtomicBool>,
    // ZEB-712 (CodeRabbit #492 R1): this task deliberately bypasses
    // `insert_local_event` (no engine back-reference → no Arc cycle), so
    // it must carry the closing fence itself — otherwise it can acquire
    // the state lock after the shutdown arm's final flush and append a
    // JoinCountersign nothing will ever persist or publish. Skipping is
    // safe: eligibility idempotently re-derives on next boot (C1).
    closing: Arc<AtomicBool>,
    // ZEB-254 R3 (C4): plumb the membership-delta channel so the locally-
    // emitted JoinCountersign drives the same `community-members-changed`
    // Tauri event that any other Inserted membership event would. Without
    // this, the admin's own UI doesn't observe the local counter-sign
    // until the event round-trips back through state-root sync.
    delta_tx: Option<mpsc::Sender<CommunityMembershipDelta>>,
) {
    use crate::community_membership::{EventPayload, MemberStatus, MembershipEventKind};

    // --- Eligibility + idempotency check under the state lock. ---
    let (self_joined, power_ok, already_signed) = {
        let state_g = state.lock().await;
        let mat = state_g.materialize_now(admin_addr);

        let self_status = mat.members.get(&self_owner).map(|m| m.status);
        let joined = matches!(self_status, Some(MemberStatus::Joined));
        // ZEB-733: read the community's per-community invite tier off `mat`
        // (defaults to 0, historically a no-op) rather than the global
        // `POWER_THRESHOLDS` const — a community that raised its invite floor
        // gates local counter-signing the same way `verify_event` does.
        // Computed here, while `mat` is still in scope; only the boolean
        // escapes the block.
        let power_ok = crate::community_membership::actor_power_meets_invite_tier(&mat, self_owner);

        let signed_already = state_g.events.values().any(|e| {
            e.actor == self_owner
                && matches!(
                    &e.kind,
                    MembershipEventKind::JoinCountersign { target_event_id }
                    if *target_event_id == pending_id
                )
        });
        (joined, power_ok, signed_already)
    };

    if !self_joined || !power_ok || already_signed {
        return;
    }

    // --- Build a HLC for the new event (wall-time, logical 0, self device). ---
    let wall_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let cs_hlc = crate::owner_state_types::Hlc {
        wall_ms,
        logical: 0,
        device_id: device_id.clone(),
    };

    // --- Mint + sign the JoinCountersign event. ---
    let mut event_id_bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut event_id_bytes);

    let cs_payload = EventPayload {
        id: event_id_bytes,
        community_id,
        kind: MembershipEventKind::JoinCountersign {
            target_event_id: pending_id,
        },
        actor: self_owner,
        at: cs_hlc,
    };
    let signed_cs = match crate::community_membership::sign_event(&cs_payload, &signing_key) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                community_id = ?community_id,
                error = %e,
                "ZEB-254 auto-counter-sign: sign_event failed; skipping"
            );
            return;
        }
    };

    // --- Insert directly into CommunityState. ---
    // We bypass `insert_local_event` to avoid needing an Arc<CommunitySyncEngine>
    // back-reference (which would create a reference cycle). The insert uses
    // the same VerifyContext shape as `insert_event_with_resolved_pubs`.
    // ZEB-339: signer resolved from materialized enrolled keys (learned from
    // cert-bearing Join events); the identity_resolver is not consulted because
    // it caches Reticulum-keyed pubs and misses for owner_id actors.
    let ctx_v = crate::community_membership::VerifyContext {
        expected_community_id: community_id,
        admin_addr,
        is_invite_only,
    };

    // R3 (C4): clone before the move into insert_event so we can emit a
    // CommunityMembershipDelta on success. Cheap (signed events are
    // ~200B); only paid on the rare auto-counter-sign path.
    let signed_cs_for_delta = signed_cs.clone();

    let outcome = {
        let mut state_g = state.lock().await;

        // ZEB-712 (CodeRabbit #492 R1): same closing fence as the
        // `insert_local_event` funnel, under the same lock. See the
        // `closing` parameter docs.
        if closing.load(std::sync::atomic::Ordering::SeqCst) {
            tracing::debug!(
                community_id = ?community_id,
                target = ?pending_id,
                "ZEB-254 auto-counter-sign: engine shutting down; skipping (re-derived on next boot)"
            );
            return;
        }

        // Re-check idempotency inside the lock so a race between two
        // concurrent triggers (e.g. two PendingJoin deliveries) doesn't
        // produce a duplicate JoinCountersign.
        let already = state_g.events.values().any(|e| {
            e.actor == self_owner
                && matches!(
                    &e.kind,
                    MembershipEventKind::JoinCountersign { target_event_id }
                    if *target_event_id == pending_id
                )
        });
        if already {
            return; // already inserted by a concurrent spawn
        }

        let outcome = state_g.insert_event(signed_cs, &ctx_v);
        // ZEB-712 (CodeRabbit #492 R1): latch dirty under the mutation
        // lock — same rationale as the insert_local_event paths.
        if matches!(outcome, InsertOutcome::Inserted) {
            has_pending_dirty.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        outcome
    };

    match outcome {
        InsertOutcome::Inserted => {
            tracing::debug!(
                community_id = ?community_id,
                target = ?pending_id,
                "ZEB-254 auto-counter-sign: JoinCountersign inserted"
            );
            // R3 (C4): emit the CommunityMembershipDelta so the admin's
            // local UI/IPC consumers (community-members-changed Tauri
            // event) observe the local counter-sign immediately, without
            // waiting for the event to round-trip through state-root sync.
            // Mirrors the post-Inserted hook in
            // `insert_event_with_resolved_pubs`.
            if let Some(tx) = delta_tx.as_ref() {
                let _ = tx.try_send(CommunityMembershipDelta {
                    community_id: signed_cs_for_delta.community_id,
                    event: signed_cs_for_delta,
                });
            }
            // Dirty flag already latched under the insertion lock above
            // (ZEB-712 R1); only the task wake-up remains post-lock.
            notify_dirty.notify_one();
        }
        InsertOutcome::AlreadyKnown => {
            // Concurrent insert won the race — that's fine (idempotent).
        }
        InsertOutcome::Rejected(e) => {
            tracing::warn!(
                community_id = ?community_id,
                target = ?pending_id,
                error = ?e,
                "ZEB-254 auto-counter-sign: JoinCountersign rejected by verify_event"
            );
        }
    }
}

/// Wire `spawn_auto_counter_sign_task` into the `InternalCtx` / receive path.
/// Called from `handle_incoming_publish` after `InsertOutcome::Inserted` OR after
/// `AlreadyKnown` for a `PendingJoin` (restart-recovery — C1 bot-review finding).
/// Mirrors `CommunitySyncEngine::maybe_spawn_auto_counter_sign`.
///
/// Self-eligibility: the spawned task requires self to be currently `Joined` with
/// power ≥ `POWER_THRESHOLDS.invite`. In v1 that threshold is 0, so ANY joined
/// member qualifies — not just the admin. The power check is retained for
/// forward-compatibility with ZEB-251 per-community threshold customisation.
fn maybe_spawn_auto_counter_sign_for_ctx(
    ctx: &InternalCtx,
    pending_event: &crate::community_membership::SignedMembershipEvent,
) {
    if !matches!(
        &pending_event.kind,
        crate::community_membership::MembershipEventKind::PendingJoin { .. }
    ) {
        return;
    }

    let pending_id = pending_event.id;
    let community_id = ctx.community_id;
    let self_owner = ctx.self_owner;
    let admin_addr = ctx.admin_addr;
    let signing_key = Arc::clone(&ctx.signing_key);
    let device_id = ctx.device_id.clone();
    let state = Arc::clone(&ctx.state);
    let identity_resolver = ctx.identity_resolver.clone();
    let is_invite_only = ctx.is_invite_only;
    let notify_dirty = Arc::clone(&ctx.notify_dirty);
    let has_pending_dirty = Arc::clone(&ctx.has_pending_dirty);
    // ZEB-712 (CodeRabbit #492 R1): receive-path spawns carry the fence too.
    let closing = Arc::clone(&ctx.closing);
    // R3 (C4): receive-path delta channel for the auto-counter-sign emission.
    let delta_tx = ctx.delta_tx.clone();

    tokio::spawn(spawn_auto_counter_sign_task(
        pending_id,
        community_id,
        self_owner,
        admin_addr,
        signing_key,
        device_id,
        state,
        identity_resolver,
        is_invite_only,
        notify_dirty,
        has_pending_dirty,
        closing,
        delta_tx,
    ));
}

// ── ZEB-254 Task 11: joiner-side Space-pending-clear hook ───────────────────

/// Fires when a `JoinCountersign` is freshly `Inserted` in the joiner's
/// engine: if the target event is a self-authored `PendingJoin`, spawns a
/// task that clears `Space.pending_join_at` via `apply_space_with_
/// canonicalization` and calls the `nav_emitter` callback (which production
/// wires to `app.emit("nav-updated", { pending: false, ... })`).
///
/// Called from BOTH the `insert_local_event` engine path and the
/// `handle_incoming_publish` receive path immediately after
/// `InsertOutcome::Inserted`.
///
/// No-op when:
///   - `inserted` is not a `JoinCountersign`.
///   - `crdt_state` is `None` (test / admin engines).
///
/// The spawned task does its own eligibility check (is the target a
/// self-authored PendingJoin?) under the state lock, so late-arriving
/// out-of-order events are handled gracefully.
fn maybe_spawn_pending_join_clear(
    inserted: &crate::community_membership::SignedMembershipEvent,
    community_state: Arc<Mutex<CommunityState>>,
    self_owner: OwnerAddr,
    community_id: SpaceId,
    crdt_state: Option<Arc<Mutex<crate::owner_state_crdt::OwnerState>>>,
    nav_emitter: Option<NavPendingClearEmitter>,
) {
    use crate::community_membership::MembershipEventKind;

    // Only act on JoinCountersign.
    let target_event_id = match &inserted.kind {
        MembershipEventKind::JoinCountersign { target_event_id } => *target_event_id,
        _ => return,
    };

    // Need crdt_state to update the Space row. Without it, skip (tests /
    // admin engines don't carry it).
    let crdt_arc = match crdt_state {
        Some(a) => a,
        None => return,
    };

    tokio::spawn(async move {
        // Check eligibility: is the target a self-authored PendingJoin?
        // ZEB-254 R5-6: capture the target PendingJoin's HLC so we can
        // verify it matches the Space's `pending_join_at` before clearing.
        // Without this, a late JoinCountersign for an OLDER attempt could
        // clear a NEWER pending state and hide a still-pending redemption
        // (symmetric to R4-5 which fixed the boot-heal case).
        let target_pending_at = {
            let state_g = community_state.lock().await;
            let target = match state_g.events.get(&target_event_id) {
                Some(t) => t,
                None => {
                    // Out-of-order: target not yet in CRDT.
                    tracing::debug!(
                        community_id = ?community_id,
                        target_event_id = ?target_event_id,
                        "ZEB-254 pending-clear: target event not in CRDT (out-of-order)"
                    );
                    return;
                }
            };
            if target.actor != self_owner {
                return;
            }
            if !matches!(&target.kind, MembershipEventKind::PendingJoin { .. }) {
                return;
            }
            target.at.clone()
        }; // community_state lock released

        let wall_now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let mut owner_g = crdt_arc.lock().await;

        let existing = match owner_g.spaces.get(&community_id).cloned() {
            Some(s) => s,
            None => {
                tracing::warn!(
                    community_id = ?community_id,
                    "ZEB-254 pending-clear: Space not found in owner-state CRDT"
                );
                return;
            }
        };

        // Already cleared — nothing to do (idempotent).
        // ZEB-254 R5-6: also bail if pending_join_at points to a
        // DIFFERENT attempt than the JoinCountersign's target. Compare
        // full HLC equality — `Space.pending_join_at` is set to the
        // PendingJoin event's `at` HLC at mint time (lib.rs:
        // redeem_invite_inner), so equality on (wall_ms, logical,
        // device_id) is the canonical "same attempt" check (mirrors
        // R4-5's boot-heal full-HLC match at lib.rs:2424).
        // If the user retried redemption (new PendingJoin with newer
        // HLC) and a stale countersign for the OLDER attempt lands
        // now, we must NOT clear the newer pending marker.
        match existing.pending_join_at.as_ref() {
            None => return,
            Some(existing_at) if existing_at != &target_pending_at => {
                tracing::debug!(
                    community_id = ?community_id,
                    target_event_id = ?target_event_id,
                    existing_at = ?existing_at,
                    target_at = ?target_pending_at,
                    "ZEB-254 R5-6: pending-clear target HLC differs from Space.pending_join_at — \
                     stale countersign for older attempt, skipping clear"
                );
                return;
            }
            Some(_) => {
                // Matches — proceed to clear.
            }
        }

        // Use monotonic HLC arithmetic: if wall_now_ms is strictly after
        // existing.updated_at, take logical=0; otherwise bump logical so
        // the new HLC is strictly greater even on clock rollback or
        // same-millisecond execution (mirrors next_hlc semantics).
        let new_hlc = if wall_now_ms > existing.updated_at.wall_ms {
            crate::owner_state_types::Hlc {
                wall_ms: wall_now_ms,
                logical: 0,
                device_id: existing.updated_at.device_id.clone(),
            }
        } else {
            crate::owner_state_types::Hlc {
                wall_ms: existing.updated_at.wall_ms,
                logical: existing.updated_at.logical.saturating_add(1),
                device_id: existing.updated_at.device_id.clone(),
            }
        };

        let mut updated = existing.clone();
        updated.pending_join_at = None;
        updated.updated_at = new_hlc;

        let space_name = existing.name.clone();
        // ZEB-709 audit (C1): deliberately NOT owner-state notify_dirty'd —
        // this fn receives no engine handle. The pending_join clear is
        // re-derived by the next-restart C3 healing pass (see the fallback
        // note below), so a crash costs one restart's worth of staleness,
        // never the join itself.
        let outcome = owner_g.apply_space_with_canonicalization(updated);
        drop(owner_g);

        match outcome {
            crate::owner_state_crdt::ApplyOutcome::Inserted
            | crate::owner_state_crdt::ApplyOutcome::Merged { .. } => {
                tracing::debug!(
                    community_id = ?community_id,
                    "ZEB-254 pending-clear: Space.pending_join_at cleared"
                );
            }
            crate::owner_state_crdt::ApplyOutcome::Rejected(ref reason) => {
                tracing::warn!(
                    community_id = ?community_id,
                    reason = ?reason,
                    "ZEB-254 pending-clear: apply_space_with_canonicalization rejected"
                );
                return;
            }
        }

        if let Some(cb) = nav_emitter {
            cb(community_id, space_name);
        }
    });
}

/// R4-3 rescan helper: when a self-authored `PendingJoin` is freshly
/// `Inserted`, check the CRDT for an *already-present* `JoinCountersign`
/// whose `target_event_id` matches and — if found — fire the
/// pending-clear path. Without this, an out-of-order JoinCountersign
/// (arrives BEFORE the joiner's PendingJoin syncs to their device)
/// silently drops on the floor at the live `maybe_spawn_pending_join_clear`
/// site (the target wasn't in the log yet), and the only path to clear
/// `Space.pending_join_at` is the next-restart C3 healing pass.
///
/// Idempotency: `maybe_spawn_pending_join_clear` already checks
/// `existing.pending_join_at.is_none()` before mutating, so a double-fire
/// (live JoinCountersign hook + this rescan) is a harmless no-op.
fn maybe_spawn_pending_clear_rescan_for_pending_join(
    inserted: &crate::community_membership::SignedMembershipEvent,
    community_state: Arc<Mutex<CommunityState>>,
    self_owner: OwnerAddr,
    community_id: SpaceId,
    crdt_state: Option<Arc<Mutex<crate::owner_state_crdt::OwnerState>>>,
    nav_emitter: Option<NavPendingClearEmitter>,
) {
    use crate::community_membership::MembershipEventKind;

    // Only act when the just-inserted event is a self-authored PendingJoin.
    if inserted.actor != self_owner {
        return;
    }
    if !matches!(&inserted.kind, MembershipEventKind::PendingJoin { .. }) {
        return;
    }
    let pending_id = inserted.id;

    // Need crdt_state for the eventual Space update — same gate as
    // maybe_spawn_pending_join_clear.
    let crdt_arc = match crdt_state.clone() {
        Some(a) => a,
        None => return,
    };

    let state_for_scan = Arc::clone(&community_state);
    tokio::spawn(async move {
        // Look for a JoinCountersign already in the log whose
        // target_event_id matches our just-inserted PendingJoin.
        let matched_countersign: Option<crate::community_membership::SignedMembershipEvent> = {
            let g = state_for_scan.lock().await;
            g.events
                .values()
                .find(|e| {
                    matches!(
                        &e.kind,
                        MembershipEventKind::JoinCountersign { target_event_id }
                            if *target_event_id == pending_id
                    )
                })
                .cloned()
        };

        let cs = match matched_countersign {
            Some(c) => c,
            None => return, // No prior JoinCountersign — nothing to recover.
        };

        // Reuse the live pending-clear hook with the existing
        // countersign event as the trigger. The hook already does
        // its own eligibility re-check + idempotent owner-state apply.
        maybe_spawn_pending_join_clear(
            &cs,
            community_state,
            self_owner,
            community_id,
            Some(crdt_arc),
            nav_emitter,
        );
    });
}

// ── end ZEB-254 Task 11 helpers ───────────────────────────────────────────────

/// Internal task: `select!` loop multiplexing dirty signals, the
/// debounce wakeup, forced flushes, inbound publishes, and shutdown.
/// Mirrors `owner_state_sync::internal_task` exactly, minus the
/// persist-both invocation (Task 10) and minus the inbound-publish
/// handling beyond a stub (Task 8).
///
/// `Notified` is pinned outside the loop so its consumed-permit state
/// survives a `select!` arm-cancel — see the long-form comment in
/// `owner_state_sync::internal_task` for the full justification. We
/// re-arm with `notified.set(notify.notified())` after each fire.
async fn internal_task(mut ctx: InternalCtx) {
    use std::time::Instant;

    let mut next_wakeup: Option<Instant> = None;
    // Latched after we observe `None` on `subscriber_rx` to prevent
    // tight-looping on a closed channel; engine remains alive in
    // publish-only mode. Mirrors owner_state_sync's same latch.
    let mut inbound_closed = false;

    // ZEB-434 D2: the query-serve request channel. `Option<Receiver>`
    // can't be polled inside `select!` directly, so take it out of the
    // ctx once here; `None` (no queryable wired — legacy callers, most
    // tests) leaves the serve arm permanently disabled. Re-set to
    // `None` when the sender side closes — same hazard class as the
    // `inbound_closed` latch above: without it a closed channel would
    // yield `None` from `recv()` on every loop iteration and busy-spin
    // the task.
    let mut root_serve_rx = ctx.root_serve_rx.take();

    let notify = Arc::clone(&ctx.notify_dirty);
    let notified = notify.notified();
    tokio::pin!(notified);

    loop {
        let sleep_dur = next_wakeup
            .map(|t| t.saturating_duration_since(Instant::now()))
            .unwrap_or(std::time::Duration::from_secs(3600));

        tokio::select! {
            _ = notified.as_mut() => {
                // Sliding debounce: each notify resets the wakeup so
                // a burst of mutations collapses to one publish after
                // `debounce` from the last call in the burst.
                if ctx.has_pending_dirty.load(Ordering::Relaxed) {
                    next_wakeup = Some(Instant::now() + ctx.debounce);
                }
                notified.set(notify.notified());
            }
            _ = tokio::time::sleep(sleep_dur), if next_wakeup.is_some() => {
                next_wakeup = None;
                // `swap` rather than `store(false)` so a publish
                // failure restores the dirty bit; otherwise a
                // transient Zenoh / CAS error silently consumes the
                // signal and the next shutdown skips the retry.
                let was_dirty = ctx.has_pending_dirty.swap(false, Ordering::AcqRel);
                let pub_result = publish_root_now(&ctx).await;
                if let Err(e) = &pub_result {
                    tracing::warn!(
                        community_id = ?ctx.community_id,
                        error = %e,
                        "community publish_root_now failed"
                    );
                    if was_dirty {
                        ctx.has_pending_dirty.store(true, Ordering::Release);
                    }
                }
                // ZEB-462 B: persist the CRDT (`crdt.cbor`) UNCONDITIONALLY —
                // the membership events are validated, durable facts that must
                // survive a crash even when this publish never landed. Persist
                // the `replay.cbor` tracker advance ONLY on publish success:
                // recording next_hlc's advance after a failed publish would
                // make a restart skip the retry and leave the community
                // out-of-sync until clock-time passes the unpersisted HLC, so
                // on failure we leave the on-disk tracker un-advanced. The
                // dirty bit was restored above, so the next publish OPPORTUNITY
                // retries it — a later mutation's debounce (mutations re-arm the
                // timer via notify_dirty), flush_now, or shutdown — note that
                // restoring the bit alone does NOT re-arm this debounce timer
                // (next_wakeup was cleared), so the retry waits for one of those
                // triggers. Errors are logged + swallowed (the debounce wakeup
                // has no caller to surface a Result to; dropping the loop on a
                // transient disk error would silently disable sync).
                let persist_result = if pub_result.is_ok() {
                    persist_both(&ctx).await
                } else {
                    persist_crdt_only(&ctx).await
                };
                if let Err(e) = persist_result {
                    tracing::warn!(
                        community_id = ?ctx.community_id,
                        error = %e,
                        "community persist failed after debounce publish"
                    );
                }
            }
            Some(resp_tx) = ctx.flush_now_rx.recv() => {
                next_wakeup = None;
                let was_dirty = ctx.has_pending_dirty.swap(false, Ordering::AcqRel);
                let pub_result = publish_root_now(&ctx).await;
                if pub_result.is_err() && was_dirty {
                    ctx.has_pending_dirty.store(true, Ordering::Release);
                }
                // Persist matches the public contract: flush_now() returns
                // after both publish AND on-disk persist complete (mirrors
                // owner_state_sync::SyncEngine). ZEB-462 B: persist the CRDT
                // unconditionally (validated events are durable facts); persist
                // the replay-tracker advance only on publish success (an
                // unpublished HLC advance must not be recorded). On publish
                // failure we still persist `crdt.cbor` but surface the publish
                // error to the caller via the and()-chained Result.
                let final_result = if pub_result.is_ok() {
                    let persist_result = persist_both(&ctx).await;
                    pub_result.and(persist_result)
                } else {
                    if let Err(e) = persist_crdt_only(&ctx).await {
                        tracing::warn!(
                            community_id = ?ctx.community_id,
                            error = %e,
                            "community crdt persist failed after flush_now publish failure"
                        );
                    }
                    pub_result
                };
                let _ = resp_tx.send(final_result);
            }
            Some(resp_tx) = ctx.persist_now_rx.recv() => {
                // ZEB-462 B: publish-INDEPENDENT durable persist (join-commit
                // fence). CRDT-ONLY by design (Cursor / CodeRabbit PR #253): a
                // prior failed debounce/flush publish advances `ctx.tracker`
                // in-memory via `next_hlc` while `persist_crdt_only` deliberately
                // leaves the on-disk tracker un-advanced. `persist_both` here
                // would fence that UNPUBLISHED advance to `replay.cbor`, undoing
                // the failure split and skipping the publish retry after a
                // restart. The fence only needs the membership CRDT durable;
                // `replay.cbor` is persisted by the receive arm (accepted
                // inbound) and the publish arms (on confirmed publish), never by
                // this fence. Deliberately does NOT touch `has_pending_dirty`:
                // any pending state-root publish still fires on the next
                // debounce / flush_now.
                let persist_result = persist_crdt_only(&ctx).await;
                let _ = resp_tx.send(persist_result);
            }
            maybe_bytes = ctx.subscriber_rx.recv(), if !inbound_closed => {
                let Some(bytes) = maybe_bytes else {
                    tracing::error!(
                        community_id = ?ctx.community_id,
                        "community subscriber channel closed; sync inbound disabled"
                    );
                    // Surface as a degraded-path report so the
                    // frontend banner can flag this community as
                    // sync-disabled. Engine stays alive in publish-
                    // only mode; the latch prevents tight-looping
                    // on a closed channel.
                    report_degraded(
                        ctx.error_tx.as_ref(),
                        ctx.community_id,
                        "subscriber_channel_closed",
                        "Zenoh adapter dropped subscriber_tx; engine in publish-only mode".into(),
                    );
                    inbound_closed = true;
                    continue;
                };
                let outcome = handle_incoming_publish(&ctx, bytes).await;
                if let Some(err) = outcome.error() {
                    tracing::warn!(
                        community_id = ?ctx.community_id,
                        error = %err,
                        "community incoming publish dropped"
                    );
                    // Surface the failure-class as a degraded-path
                    // report so start_node's drain task can translate
                    // it into a `community-state-sync-degraded`
                    // Tauri event. Per the spec (§ "IPC surface →
                    // Events"), the frontend uses these to surface
                    // "this community's sync is degraded" banners.
                    report_degraded(
                        ctx.error_tx.as_ref(),
                        ctx.community_id,
                        classify_incoming_error(err),
                        format!("{err}"),
                    );
                }
                // Persist on Mutated | MutatedTrackerOnly |
                // ErrPostMutation. `crdt_mutated()` lets the
                // tracker-only branch skip the larger `crdt.cbor`
                // fsync — the CRDT is byte-identical when every
                // event in the remote blob was AlreadyKnown.
                if outcome.needs_persist() {
                    let persist_result = if outcome.crdt_mutated() {
                        persist_both(&ctx).await
                    } else {
                        persist_replay_only(&ctx).await
                    };
                    if let Err(e) = persist_result {
                        tracing::warn!(
                            community_id = ?ctx.community_id,
                            error = %e,
                            "community persist after merge failed"
                        );
                    }
                }
            }
            serve_req = async {
                match root_serve_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    // Unreachable under the `if` guard below; kept so
                    // the block type-checks for the `None` case.
                    None => std::future::pending().await,
                }
            }, if root_serve_rx.is_some() => {
                match serve_req {
                    Some(reply_tx) => {
                        // ZEB-434 D2: serve a FRESH packet through this
                        // single-writer task so publish, flush, and
                        // query-serve can never disagree about HLC
                        // state. encode advances next_hlc via the
                        // tracker — and replay-tracker persistence is
                        // part of the serve SUCCESS condition: a reply
                        // hands the querier an HLC advance this node
                        // must durably remember across restarts. The
                        // pub/sub arms can only log-and-swallow a
                        // persist failure (they can't un-publish); a
                        // query CAN be left unanswered — on Err the
                        // adapter withholds the zenoh reply and the
                        // querier backs off and retries, possibly
                        // against another responder. The CRDT itself
                        // did not change → persist_replay_only.
                        let result = match encode_root_packet(&ctx).await {
                            Ok(packet) => match persist_replay_only(&ctx).await {
                                Ok(()) => Ok(packet),
                                Err(e) => {
                                    tracing::warn!(
                                        community_id = ?ctx.community_id,
                                        error = %e,
                                        "community persist after query-serve encode failed — withholding reply"
                                    );
                                    Err(e)
                                }
                            },
                            Err(e) => {
                                tracing::warn!(
                                    community_id = ?ctx.community_id,
                                    error = %e,
                                    "community query-serve encode failed"
                                );
                                Err(e)
                            }
                        };
                        // Receiver dropped (querier gone) is fine —
                        // fire and forget.
                        let _ = reply_tx.send(result.map_err(|e| e.to_string()));
                    }
                    None => {
                        // Sender side dropped (queryable adapter gone):
                        // disable the arm permanently instead of
                        // busy-spinning on a closed channel. Mirrors
                        // the `inbound_closed` latch on subscriber_rx.
                        root_serve_rx = None;
                    }
                }
            }
            Some(resp_tx) = ctx.shutdown_rx.recv() => {
                // ZEB-712: mark closing under the `state` lock BEFORE the
                // final flush. Inserts append under this same lock and
                // check the flag first, so every insert racing this
                // shutdown either landed already (its event is in `state`
                // and the flush below persists it) or will observe the
                // flag and return `EngineShuttingDown`. Without this, an
                // insert arriving after the flush is signed into memory
                // no task will ever persist or publish — while the IPC
                // reports success.
                {
                    let _state_g = ctx.state.lock().await;
                    ctx.closing.store(true, Ordering::SeqCst);
                }
                // Final-flush only if the in-memory pending-dirty flag
                // says we owe peers a publish. Lock-relaxed is fine
                // because there's no concurrent mutator past this
                // point — we're in the shutdown branch. (Mutating IPC
                // entry points are fenced by `closing` above; the
                // flag-set is why "no concurrent mutator" now holds.)
                let was_dirty = ctx.has_pending_dirty.load(Ordering::Relaxed);
                let pub_result = if was_dirty {
                    publish_root_now(&ctx).await
                } else {
                    Ok(())
                };
                // ZEB-462 B: persist the CRDT (`crdt.cbor`) UNCONDITIONALLY —
                // a SIGKILL never reaches this arm, but a GRACEFUL shutdown
                // that cannot publish (transport already torn down, no live
                // peers) must still durably checkpoint the validated
                // membership rather than lose it. The `replay.cbor` tracker
                // advance is still gated on a SUCCESSFUL publish: recording
                // next_hlc's advance after a failed publish would, on restart
                // (in-memory `has_pending_dirty` gone), leave no signal to
                // retry. The receive-side (no publish, just a tracker advance
                // from accepted inbound) is a separate concern: persist runs
                // on every successful accept-and-merge in the subscriber arm,
                // so by shutdown the on-disk replay tracker is already current
                // for accepted publishes.
                //
                // If we never even attempted a publish (was_dirty=false) we
                // still persist_both so any receive-side updates this loop
                // accepted but didn't yet persist (only possible if a shutdown
                // raced in between accept and persist) reach disk. In practice
                // the subscriber arm calls persist before yielding back to
                // select!, so this is a belt-and-suspenders flush — cheap,
                // safe, and visible in tests.
                let final_result = if pub_result.is_ok() {
                    let persist_result = persist_both(&ctx).await;
                    pub_result.and(persist_result)
                } else {
                    // Publish failed, but the CRDT must still reach disk.
                    if let Err(e) = persist_crdt_only(&ctx).await {
                        tracing::warn!(
                            community_id = ?ctx.community_id,
                            error = %e,
                            "community crdt persist failed during shutdown after publish failure"
                        );
                    }
                    pub_result
                };
                // ZEB-463: a graceful shutdown can race a concurrent rollback
                // that removes this community's persistence directory — a
                // freshly-spawned engine rolled back via
                // shutdown_engine_and_cleanup_persistence (detached task)
                // while shutdown_all flushes the SAME engine. The final flush
                // then fails against a directory that is being intentionally
                // discarded — ENOENT usually, but other errnos too (ZEB-633
                // observed EINVAL from a rename against the dying dir). Not a
                // durability failure either way. Downgrade when the dir is
                // being discarded; a persist failure with the dir still
                // present propagates loudly per ZEB-460.
                let final_result = if shutdown_flush_lost_race_to_dir_removal(
                    &final_result,
                    ctx.paths.crdt.parent(),
                ) {
                        tracing::debug!(
                            community_id = ?ctx.community_id,
                            "community persistence dir removed during shutdown \
                             (concurrent rollback); treating the final flush as a \
                             no-op (ZEB-463)"
                        );
                        Ok(())
                    } else {
                        final_result
                    };
                // Surface persist+publish failures to the caller —
                // suppressing them mirrors the same silent-corruption
                // failure mode `owner_state_sync` explicitly rejects.
                let _ = resp_tx.send(final_result);
                return;
            }
        }
    }
}

/// ZEB-249 §10.6 (Phase A): read the live epoch key and epoch counter
/// for `community_id` from the owner-state CRDT (`crdt_state`), falling
/// back to the engine's spawn-time `membership_key` when `crdt_state`
/// is `None` or the Space is absent/incomplete.
///
/// Returns `(epoch_key, epoch_counter)`. `epoch_counter` is `None`
/// when the fallback fires (spawn-time key carries no epoch metadata).
/// Return the live epoch key and epoch number for `community_id`.
///
/// CR Critical (PR #106 R7): the `crdt_state = None` (test/legacy) and
/// `crdt_state = Some` paths are now explicitly separated:
///
/// - `None` → explicit fallback to the spawn-time `fallback` key.
/// - `Some` → the owner-state IS wired; if the Space/epoch fields are
///   missing or incomplete we surface `LiveEpochKeyMissing` rather than
///   silently using the spawn-time key, which would reopen the §10.6
///   backward-secrecy gap.
pub(crate) async fn live_epoch_key(
    community_id: SpaceId,
    crdt_state: Option<&Arc<Mutex<crate::owner_state_crdt::OwnerState>>>,
    fallback: &EpochKey,
) -> Result<(EpochKey, Option<u64>), CommunitySyncError> {
    match crdt_state {
        None => {
            // Test / legacy mode: explicit fallback to spawn-time key.
            Ok((fallback.clone(), None))
        }
        Some(cs) => {
            let guard = cs.lock().await;
            match guard.spaces.get(&community_id) {
                Some(space) => match (&space.current_epoch_key, space.current_epoch) {
                    (Some(k), Some(e)) => Ok((k.clone(), Some(e))),
                    _ => {
                        // crdt_state IS wired but Space/epoch is incomplete.
                        // Surface as error rather than silently substituting
                        // the spawn-time key (which would reopen §10.6 gap).
                        Err(CommunitySyncError::LiveEpochKeyMissing(community_id))
                    }
                },
                None => Err(CommunitySyncError::LiveEpochKeyMissing(community_id)),
            }
        }
    }
}

/// The epoch-key bytes the case-C pkarr **publisher** should publish under
/// for `community_id`: the LIVE `Space.current_epoch_key` when available
/// (so published routing records track ZEB-249 epoch rotation, matching
/// what seekers read on resolve via [`live_epoch_key`] — ZEB-596), else the
/// spawn-time `fallback` (the engine's `membership_key`).
///
/// The Err handling is deliberately the MIRROR of the seeker's: the seeker
/// (`community_contexts_for_target`) SKIPS a community when the live key is
/// missing — better to not probe than probe under a stale key — but the
/// publisher DEGRADES to the spawn-time key so it still publishes something.
/// In the common case (engine spawned at the current epoch, no mid-session
/// rotation) live == spawn-time, so this is a strict improvement over the
/// prior unconditional spawn-time key and never worse. ZEB-597.
pub(crate) async fn community_publish_epoch_key(
    community_id: SpaceId,
    crdt_state: &Arc<Mutex<crate::owner_state_crdt::OwnerState>>,
    fallback: &EpochKey,
) -> [u8; 32] {
    match live_epoch_key(community_id, Some(crdt_state), fallback).await {
        Ok((k, _epoch)) => *k.as_bytes(),
        Err(_) => *fallback.as_bytes(),
    }
}

/// Build one complete state-root wire packet: epoch-stable snapshot,
/// blob encrypt + CAS pin (put_serveable), signed payload with a
/// strictly-newer HLC, wire-envelope encrypt. Shared by the debounced
/// publish path and the ZEB-434 query-serve arm — both produce
/// byte-class-identical packets, which is what keeps "no new wire
/// format" true.
///
/// Snapshot-clone-under-brief-lock: we hold `state.lock()` only long
/// enough to `clone()` the CRDT, then drop the guard before the
/// expensive CBOR encode + AEAD + CAS hops. This keeps the lock
/// non-contended for foreground command handlers that mutate state
/// concurrently — the alternative (holding the lock through CAS.put)
/// would serialise all CRDT mutations behind one outbound publish.
///
/// Encryption split is intentional and load-bearing:
/// - `encrypt_blob` (deterministic nonce) for the on-CAS ciphertext,
///   so two devices encrypting the same `CommunityState` derive the
///   same ContentId and the CAS slot is shared (dedup, replica
///   convergence on `root_cid`).
/// - `encrypt_root_publish` (random nonce + AAD) for the wire packet,
///   so each publish is independently fresh and the AAD prefix binds
///   the ciphertext to its wire-context (replay protection lives in
///   the receiver's `RootHlcTracker`, not in nonce reuse).
///
/// Don't swap them — sharing a deterministic-nonce wire-side would
/// make every retransmit byte-identical and hide replay errors;
/// sharing a random-nonce CAS-side would defeat ContentId dedup.
async fn encode_root_packet(ctx: &InternalCtx) -> Result<Vec<u8>, CommunitySyncError> {
    use crate::owner_state_crypto::canonical_cbor_encode;
    use ed25519_dalek::Signer;

    // ZEB-249 §10.6 Phase A + PR #106 R5 (CodeRabbit Critical):
    // Recheck-and-retry pattern to close the TOCTOU race where an
    // EpochRotation lands between the live_epoch_key read and the
    // community-state snapshot. If that happens, the post-rotation
    // snapshot would be encrypted under the pre-rotation key — a
    // backward-secrecy gap. We detect the race by re-reading the
    // live epoch key after snapshotting and retrying if the epoch changed.
    //
    // Lock-order invariant preserved: owner-state lock is acquired
    // then released (inside live_epoch_key) BEFORE community-state
    // lock is acquired (inside snapshot clone). We never hold both.
    //
    // Rotation events are rare (one per Kick/Leave), so the tight loop
    // is benign in practice. Bounded at 5 iterations to prevent an
    // infinite loop in the pathological case where another actor is
    // continuously rotating (should be impossible in a correct cluster,
    // but we defend against it anyway).
    let (current_key, current_epoch, snapshot) = {
        let mut retries: u8 = 5;
        loop {
            // CR Critical (PR #106 R7): live_epoch_key now returns Result.
            // If crdt_state is Some but Space/epoch is incomplete, abort
            // immediately rather than looping with a stale key.
            let (_key_before, epoch_before) = live_epoch_key(
                ctx.community_id,
                ctx.crdt_state.as_ref(),
                &ctx.membership_key,
            )
            .await?;

            // Snapshot CRDT state under brief lock; drop guard before
            // the epoch recheck and the expensive encode + AEAD + CAS
            // hops below.
            let snap = {
                let state = ctx.state.lock().await;
                state.clone()
            };

            let (key_after, epoch_after) = live_epoch_key(
                ctx.community_id,
                ctx.crdt_state.as_ref(),
                &ctx.membership_key,
            )
            .await?;

            if epoch_before == epoch_after {
                // Epoch stable across snapshot window — safe to proceed.
                break (key_after, epoch_after, snap);
            }

            retries -= 1;
            if retries == 0 {
                return Err(CommunitySyncError::PublishRetryExhausted);
            }
            // Epoch changed between key reads — retry with fresh key.
            tracing::debug!(
                community_id = ?ctx.community_id,
                epoch_before = ?epoch_before,
                epoch_after = ?epoch_after,
                retries_left = retries,
                "encode_root_packet: epoch changed mid-encode, retrying"
            );
        }
    };

    // 1. Canonical-CBOR encode the CommunityState as the cleartext blob.
    let blob_cleartext = canonical_cbor_encode(&snapshot)
        .map_err(|e| CommunitySyncError::CborEncode(e.to_string()))?;

    // 2. Encrypt with deterministic-nonce blob AEAD so cipher_cid is
    //    reproducible across replicas (dedup + convergence).
    //    ZEB-249 §10.6: uses `current_key` (live epoch key) rather than
    //    the spawn-time `membership_key`.
    let blob_ciphertext = encrypt_blob(&current_key, &blob_cleartext)?;

    // 3. Derive structured ContentId for the encrypted blob. Flagged
    //    `encrypted: true` so the eviction policy classifies it as
    //    EncryptedDurable (priority 0 — never auto-burns).
    let root_cid = harmony_content::cid::ContentId::for_book(
        &blob_ciphertext,
        harmony_content::cid::ContentFlags {
            encrypted: true,
            ..Default::default()
        },
    )
    .map_err(|e| {
        CommunitySyncError::Crypto(CommunityCryptoError::ContentIdDerivation(e.to_string()))
    })?;

    // 4. Put into ContentStore AND mark this community-root CID serveable to
    //    peers (ZEB-395). put_serveable admits via CasOp::PutLocal exactly like
    //    put, then (production RuntimeContentStore only) records root_cid in the
    //    shared serve-allowlist so the content-serve queryable will serve it
    //    despite the encrypted flag. Registration completes before the state-
    //    root envelope announcing root_cid is returned for publish/serve, so
    //    no peer can request the CID before it is allowlisted.
    ctx.content_store
        .put_serveable(root_cid, blob_ciphertext)
        .await?;

    // 5. Build the SIGNED sub-payload with a strictly-newer HLC.
    let now = next_hlc(ctx).await;
    let signed = CommunityRootSignedPayload {
        root_cid,
        publisher_addr: ctx.self_owner,
        at: now,
    };

    // 6. Sign the canonical CBOR of the signed sub-payload. Ed25519
    //    sign is microseconds, fine on the runtime thread (no
    //    spawn_blocking).
    let signed_bytes = canonical_cbor_encode(&signed)
        .map_err(|e| CommunitySyncError::CborEncode(e.to_string()))?;
    let publisher_sig = ctx.signing_key.sign(&signed_bytes).to_bytes();

    // 7. Wrap into the full wire envelope, including the current epoch
    //    tag so receivers can select the matching key.
    //    ZEB-249 §10.6: `current_epoch` is `Some` when crdt_state is
    //    available; `None` for test-fallback (legacy behaviour).
    let payload = signed.into_wire(publisher_sig, current_epoch);
    let payload_bytes = canonical_cbor_encode(&payload)
        .map_err(|e| CommunitySyncError::CborEncode(e.to_string()))?;

    // 8. Encrypt with random-nonce root AEAD (every publish is fresh).
    //    ZEB-249 §10.6: uses `current_key` (live epoch key).
    let wire = encrypt_root_publish(&current_key, &payload_bytes)?;

    Ok(wire)
}

/// Snapshot the local CRDT, encrypt it, write to CAS, build a
/// `CommunityRootPublishPayload`, AEAD-wrap it for the wire, and ship
/// it on `publisher_tx`.
///
/// Delegates encoding to [`encode_root_packet`], which is also shared
/// by the ZEB-434 query-serve arm so both paths produce byte-class-
/// identical packets without duplicating crypto/HLC logic.
async fn publish_root_now(ctx: &InternalCtx) -> Result<(), CommunitySyncError> {
    let wire = encode_root_packet(ctx).await?;
    // Ship the encoded packet onto the outbound channel — Zenoh adapter
    // forwards.
    ctx.publisher_tx
        .send(wire)
        .await
        .map_err(|_| CommunitySyncError::TransportClosed)
}

/// Build an HLC that is strictly newer than every prior HLC published
/// by this device. The strict-newer guarantee is structural, not
/// probabilistic: at least one of `(wall_ms, logical)` lex-increases
/// on every call.
///
/// - If `now`'s wall clock advanced past the previous wall, `wall_ms`
///   bumps and `logical` resets to 0.
/// - If wall is the same or moved BACKWARDS (NTP correction, monotonic
///   clock drift), we pin `effective_wall = max(now, prev_wall)` and
///   `logical = prev.logical + 1`.
///
/// We route through `tracker.record(...)` rather than direct
/// `per_device.insert(...)` so a backward-jump (would_accept fails)
/// trips the `debug_assert!` in dev/test. Direct insert would silently
/// smooth over a system-clock anomaly that the receiver-side replay
/// tracker would otherwise reject — surfacing the bug at the publisher
/// is strictly cheaper than chasing a "why is my publish being
/// dropped" report from a peer.
async fn next_hlc(ctx: &InternalCtx) -> Hlc {
    use std::time::{SystemTime, UNIX_EPOCH};
    let wall_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let mut tracker = ctx.tracker.lock().await;
    let key = (ctx.self_owner, ctx.device_id.clone());
    let prev = tracker.per_device.get(&key).cloned();
    // Three branches:
    //   (a) No prev → first publish for this device.
    //   (b) Wall advanced past prev_wall → reset logical to 0.
    //   (c) Same or earlier wall → bump logical, but if logical
    //       saturates at u32::MAX manufacture a wall_ms advance to
    //       keep producing strictly-newer HLCs. Otherwise the
    //       resulting HLC would equal prev exactly, and `record()`
    //       would panic via debug_assert (we'd also be silently
    //       republishing the same logical clock, which receivers
    //       reject as replay).
    let now = match prev.as_ref() {
        None => Hlc {
            wall_ms,
            logical: 0,
            device_id: ctx.device_id.clone(),
        },
        Some(p) if wall_ms > p.wall_ms => Hlc {
            wall_ms,
            logical: 0,
            device_id: ctx.device_id.clone(),
        },
        Some(p) if p.logical == u32::MAX => Hlc {
            // Saturation escape: bump wall (vanishingly unlikely in
            // production — 4B publishes within one wall-millisecond —
            // but the alternative is debug-mode panic).
            wall_ms: p.wall_ms.saturating_add(1),
            logical: 0,
            device_id: ctx.device_id.clone(),
        },
        Some(p) => Hlc {
            wall_ms: p.wall_ms,
            logical: p.logical + 1,
            device_id: ctx.device_id.clone(),
        },
    };
    tracker.record(ctx.self_owner, now.clone());
    now
}

/// Outcome of processing one inbound state-root publish. Mirrors the
/// `IncomingOutcome` enum from `owner_state_sync` — the variants
/// distinguish where the failure happened so the caller persists only
/// when local state actually changed (Task 10's persist hook).
///
/// Community-state introduces a tri-state on the success side
/// (`Mutated` vs `MutatedTrackerOnly`) because the receiver may accept
/// a publish whose blob carries only events we already have in our
/// log (`AlreadyKnown`). The tracker still advances — we don't want
/// next-boot to re-fetch the same blob — but the CRDT itself is
/// byte-identical, so Task 10 can skip the larger `crdt.cbor` fsync
/// and only flush `replay.cbor`.
#[derive(Debug)]
enum IncomingOutcome {
    /// `would_accept` rejected the wire HLC at the early replay-check
    /// (step 2). No state change. Don't persist.
    Duplicate,
    /// Tracker advanced AND ≥ 1 new event was Inserted into the CRDT.
    /// Persist both `crdt.cbor` and `replay.cbor`.
    Mutated,
    /// Tracker advanced but every event in the remote blob was already
    /// in our log (`AlreadyKnown`). The CRDT is byte-identical; only
    /// `replay.cbor` needs to flush. Distinguishing this from `Mutated`
    /// lets Task 10's persist path skip the larger `crdt.cbor` fsync
    /// when a peer re-broadcasts the same event set with an advanced
    /// clock.
    MutatedTrackerOnly,
    /// Failure occurred BEFORE the tracker advanced (decrypt-root,
    /// payload decode, blob fetch, blob decrypt, blob decode,
    /// misrouted-blob check). No state change. Don't persist.
    ErrPreMutation(CommunitySyncError),
    /// Failure occurred AFTER the tracker advanced. Tracker is in-
    /// memory dirty; persist defensively so a restart doesn't replay
    /// the same publish.
    ///
    /// **No constructor as of Task 6.** Under the verify-on-receive
    /// pipeline every fallible step before tracker advance returns
    /// `ErrPreMutation`; the only post-advance fallible step is the
    /// per-event merge loop, but `Rejected` outcomes there surface as
    /// `verify_event_rejected` reports rather than this variant. The
    /// variant is kept for forward-compatibility — future
    /// post-advance fallible operations (e.g. an additional integrity
    /// gate over the merged events) should use it. `#[allow(dead_code)]`
    /// suppresses the never-constructed lint without erasing the
    /// shape from the API.
    #[allow(dead_code)]
    ErrPostMutation(CommunitySyncError),
}

impl IncomingOutcome {
    /// Whether the disk needs flushing. Task 10's `persist_both` is
    /// the broad case (CRDT + replay); for `MutatedTrackerOnly`
    /// callers can use `persist_replay_only` to skip the CRDT fsync.
    fn needs_persist(&self) -> bool {
        matches!(
            self,
            Self::Mutated | Self::MutatedTrackerOnly | Self::ErrPostMutation(_)
        )
    }

    /// Whether the CRDT itself changed (≥ 1 event Inserted). Used by
    /// the subscriber arm to decide between `persist_both` and
    /// `persist_replay_only`. `ErrPostMutation` is deliberately
    /// excluded — under the Task 6 verify-on-receive pipeline, the
    /// only path that advances the tracker AND fails afterward is a
    /// failed `state.insert_event` call, but `Rejected` outcomes from
    /// `insert_event` surface as in-place rejection reports rather
    /// than IncomingOutcome::ErrPostMutation. Reachable
    /// ErrPostMutation paths after Task 6 are zero, but we keep the
    /// variant for forward-compatibility with future steps that mutate
    /// the tracker before a fallible operation. Including ErrPostMutation
    /// here would force an unnecessary `crdt.cbor` fsync. Future
    /// ErrPostMutation paths that DO mutate the CRDT mid-loop must
    /// surface a separate outcome variant rather than overload this one.
    fn crdt_mutated(&self) -> bool {
        matches!(self, Self::Mutated)
    }

    fn error(&self) -> Option<&CommunitySyncError> {
        match self {
            Self::ErrPreMutation(e) | Self::ErrPostMutation(e) => Some(e),
            Self::Duplicate | Self::Mutated | Self::MutatedTrackerOnly => None,
        }
    }
}

/// Process one inbound state-root publish. See `IncomingOutcome` for
/// the return-value semantics.
///
/// Pipeline (ZEB-256 verify-on-receive, Task 6):
/// 1. Decrypt the wire packet (random-nonce + AAD).
/// 2. Decode `CommunityRootPublishPayload`.
/// 3. **Membership-at-HLC gate** — materialize the local log; if
///    `payload.publisher_addr`'s status is not `Joined`, reject with
///    `PublisherNotJoined`. Tracker NOT advanced.
/// 4. **Identity-pub resolution** — `IdentityResolver::resolve` for
///    `payload.publisher_addr`; an unresolved publisher is rejected.
///    Tracker NOT advanced.
/// 5. **Ed25519 sig verification** — verify `payload.publisher_sig`
///    against the resolved identity_pub (Ed25519 half = bytes [32..64])
///    over `canonical_cbor(CommunityRootSignedPayload::from(&payload))`.
///    Failure → `PublisherSigInvalid`. Tracker NOT advanced.
/// 6. Replay-check via `tracker.would_accept(&payload.publisher_addr,
///    &payload.at)` — early-exit `Duplicate`.
/// 7. Fetch the encrypted blob from CAS (cache miss → `ErrPreMutation`).
/// 8. Decrypt the blob (deterministic-nonce).
/// 9. Decode `CommunityState`.
/// 10. Misrouted-blob check: `remote.community_id == ctx.community_id`.
/// 11. Pre-resolve identity_pubs for every inner event OUTSIDE the
///     state lock (Phase A — avoids the lock-order hazard with
///     owner-state).
/// 12. Lock state. RE-CHECK membership-at-HLC under the lock — closes
///     the TOCTOU race where a concurrent `insert_local_event()` call
///     lands a Leave/Kick between step 2's snapshot and the merge.
///     If the publisher is no longer Joined per the current local
///     state, drop the lock and return `ErrPreMutation::PublisherNotJoined`
///     WITHOUT advancing the tracker.
/// 13. Otherwise, merge each event under the same state lock
///     (skip-if-known; call `state.insert_event` with a fresh
///     `VerifyContext`; surface `Rejected` outcomes as
///     `CommunityDegradedReport` on `error_tx`). Drop the state lock.
/// 14. Advance `tracker` keyed on `payload.publisher_addr` — single
///     mutation point. Happens AFTER the state merge so a TOCTOU-race
///     rejection at step 12 leaves the tracker untouched.
///
/// **Cheapest-first rejection order.** Membership-at-HLC at step 2 is
/// a local state lookup (free) used as a DoS pre-filter; identity
/// resolution is an in-memory cache hit; sig-verify is microseconds
/// but unnecessary for a publisher we know is no longer Joined.
/// Surfacing each class as a distinct error variant lets the frontend
/// banner discriminate "this peer lost membership" (informational)
/// from "someone forged a publish claiming to be this peer"
/// (security-relevant). Step 12's re-check is the AUTHORITATIVE gate
/// w.r.t. local state mutations — step 2's read-then-drop snapshot
/// can race with `insert_local_event()`.
///
/// **Censorship-defense invariant.** None of the rejection gates
/// advance the tracker — only the `record` at step 14, which runs
/// only after the state merge succeeds. A kicked-but-still-keyed
/// member trying to squat HLC slots fails either the cheap step-2
/// gate or the authoritative step-12 re-check before tracker.record
/// runs; per-publisher namespacing on the tracker key
/// `(publisher_addr, device_id)` further isolates the per-addr
/// HLC space so an attacker can't claim Alice's slot via shared
/// `EpochKey`.
///
/// **Divergence from `owner_state_sync::handle_incoming_publish`:**
/// owner-state advances the tracker IMMEDIATELY after the replay-check
/// (so blob-fetch / decrypt / decode failures land as
/// `ErrPostMutation`). We delay the advance until step 14 — AFTER the
/// blob has been fetched, decrypted, decoded, AND merged under the
/// re-checked membership lock. Two reasons: (a) a misrouted blob
/// (foreign community's state surfaced under our CID) means the
/// publisher's HLC carries no useful information for OUR replay
/// tracker, so advancing it would let a correctly-routed re-publish
/// at the same HLC be silently dropped; (b) the post-merge tracker
/// advance is required for TOCTOU defense — see step 12.
/// owner-state has neither concern because there's only one owner-
/// CRDT per identity AND no concurrent `insert_local_event` IPC
/// ZEB-339 Task 9: verify a state-root publish's signature against the
/// publisher's materialized enrolled device keys.
///
/// Replaces the old resolver-based publisher-auth (Reticulum
/// `IdentityResolver::resolve` + `address_hash` binding check). The
/// publisher now signs the state-root with their ENROLLED device key
/// (device #2); the receiver verifies `payload.publisher_sig` over
/// `canonical_cbor(CommunityRootSignedPayload::from(&payload))` against
/// ANY key in the publisher's `MemberState.enrolled_device_keys` set —
/// the same set materialized by the membership-at-HLC gate from the
/// publisher's EnrollmentCert-bearing Join.
///
/// `member_state` is the publisher's materialized `MemberState` as of
/// `payload.at` (the membership gate already confirmed `Joined`). A
/// `None`/empty enrolled-key set OR no matching key yields
/// `PublisherSigInvalid` — the same observable outcome as the legacy
/// path (a publish unauthenticated under the claimed addr).
///
/// Pure + sync so it can be unit-tested directly without engine setup.
fn verify_publisher_sig(
    payload: &CommunityRootPublishPayload,
    member_state: &crate::community_membership::MemberState,
) -> Result<(), CommunitySyncError> {
    use crate::owner_state_crypto::canonical_cbor_encode;

    let signed_bytes = canonical_cbor_encode(&CommunityRootSignedPayload::from(payload))
        .map_err(|e| CommunitySyncError::CborEncode(e.to_string()))?;
    let sig = ed25519_dalek::Signature::from_bytes(&payload.publisher_sig);

    for key_bytes in &member_state.enrolled_device_keys {
        if let Ok(vk) = ed25519_dalek::VerifyingKey::from_bytes(key_bytes) {
            // verify_strict: rejects non-canonical S + small-order R,
            // matching `community_membership::verify_signature`'s posture
            // for signed wire payloads.
            if vk.verify_strict(&signed_bytes, &sig).is_ok() {
                return Ok(());
            }
        }
    }

    Err(CommunitySyncError::PublisherSigInvalid {
        addr: payload.publisher_addr,
    })
}

/// path.
async fn handle_incoming_publish(ctx: &InternalCtx, wire: Vec<u8>) -> IncomingOutcome {
    use crate::community_membership::MemberStatus;

    // ZEB-249 §10.6 Phase A: snapshot live epoch key state BEFORE any
    // lock on community state.  We collect (current_key, old_keys_rev)
    // here so the decrypt trial loop below doesn't need to re-acquire
    // any mutex.  Lock-order: owner-state lock released before community-
    // state lock is ever taken.
    let (outer_current_key, outer_old_keys_rev) = if let Some(cs) = ctx.crdt_state.as_ref() {
        let guard = cs.lock().await;
        if let Some(space) = guard.spaces.get(&ctx.community_id) {
            let cur = space
                .current_epoch_key
                .clone()
                .unwrap_or_else(|| ctx.membership_key.clone());
            // Collect old epoch keys in reverse order (newest first) so we
            // try the most-likely match before the most-stale.
            let old_rev: Vec<EpochKey> = {
                let mut v: Vec<(u64, EpochKey)> = space
                    .old_epoch_keys
                    .iter()
                    .map(|(e, k)| (*e, k.clone()))
                    .collect();
                v.sort_by_key(|x| std::cmp::Reverse(x.0));
                v.into_iter().map(|(_, k)| k).collect()
            };
            (cur, old_rev)
        } else {
            (ctx.membership_key.clone(), vec![])
        }
    } else {
        (ctx.membership_key.clone(), vec![])
    };

    // 1. Decrypt root publish.  ZEB-249 §10.6: try current epoch key
    //    first, then old keys in reverse order (newest first) for the
    //    brief transition window where a sender may use a key that's one
    //    epoch behind our current view.
    //
    //    CR Major (PR #106 R6): capture WHICH key succeeded so blob
    //    decryption is bound to the same key. A packet rewrapped under
    //    any accepted old root key MUST NOT be acceptable with blob under
    //    a different old key — that would break the root→blob epoch
    //    binding. `root_key_used` is a reference into `outer_current_key`
    //    or `outer_old_keys_rev`; the borrow checker ensures it stays
    //    valid for the lifetime of both Vec values.
    let (payload_bytes, root_key_used): (Vec<u8>, &EpochKey) = {
        if let Ok(b) = decrypt_root_publish(&outer_current_key, &wire) {
            (b, &outer_current_key)
        } else {
            let mut found: Option<(Vec<u8>, &EpochKey)> = None;
            for old_key in &outer_old_keys_rev {
                if let Ok(b) = decrypt_root_publish(old_key, &wire) {
                    found = Some((b, old_key));
                    break;
                }
            }
            match found {
                Some(v) => v,
                None => {
                    return IncomingOutcome::ErrPreMutation(CommunitySyncError::Crypto(
                        CommunityCryptoError::AeadFailed,
                    ))
                }
            }
        }
    };
    let payload: CommunityRootPublishPayload = match canonical_cbor_decode(&payload_bytes) {
        Ok(p) => p,
        Err(e) => {
            return IncomingOutcome::ErrPreMutation(CommunitySyncError::CborDecode(e.to_string()))
        }
    };

    // 2. Membership-at-HLC gate (DoS pre-filter — NOT the authoritative
    //    gate). Cheapest of the three new gates: a local materialize
    //    over our trusted log + a single map lookup. Run BEFORE
    //    sig-verify because a stale-membership rejection is
    //    informational and we shouldn't pay sig-verify cost for a
    //    publish we'll reject anyway. The check is over our locally-
    //    trusted state, so there's no integrity risk in trusting it
    //    pre-sig.
    //
    //    Gate-ordering note: this gate AND step 3 (resolver lookup)
    //    intentionally consume the unauthenticated `publisher_addr`
    //    before the authoritative sig-verify in step 4. This is by
    //    design — they're cheapest-first DoS pre-filters that bound
    //    work on adversarial inbound traffic, not security gates. The
    //    *security* invariant is enforced at step 4: a forged
    //    publisher_addr that happens to be a current member can pass
    //    steps 2+3 but cannot survive step 4 unless the attacker also
    //    forged a valid Ed25519 signature. The tracker never advances
    //    on rejection at any step.
    //
    //    TOCTOU note: this gate snapshots `state.events` then drops
    //    the lock so subsequent async work doesn't block the engine.
    //    A concurrent `insert_local_event()` may land a Leave/Kick
    //    for `publisher_addr` in the window between this snapshot
    //    and the merge phase. The authoritative re-check happens at
    //    step 12 (under the state lock, immediately before the merge)
    //    so a publish admitted here can still be rejected post-race
    //    without advancing the tracker.
    //
    //    Spec compliance: materialize the prefix of our local log
    //    strictly before `payload.at` via `prior_state_at_hlc` and
    //    check the publisher's status as of THAT HLC. This matches
    //    ZEB-256 § 5 step 3 verbatim. Convergence safety: a lagging
    //    peer that learned a later Leave/Kick before receiving an
    //    earlier (valid) publish from the same publisher must not
    //    permanently reject the earlier publish — the prior-state-at-
    //    publish-HLC view shows the publisher as `Joined` for any
    //    publish strictly preceding the membership change in HLC
    //    order, so the gate admits it.
    //
    //    Bootstrap caveat: the gate evaluates membership pre-decrypt,
    //    so any membership change carrying the publisher's authorizing
    //    Join INSIDE the encrypted blob is rejected. Three cases
    //    historically tracked under ZEB-260:
    //      Case A — invite-only joiner with empty CRDT receiving
    //        admin's first publish-back. FIXED in 2026-05 by plumbing
    //        admin's signed bootstrap through the invite URL
    //        (CommunityInvitePayload.admin_bootstrap +
    //        admin_identity_pub) and inserting it during
    //        redeem_invite_inner before the unicast send.
    //      Case B — open-community brand-new joiner whose self-Join
    //        is only inside their own publish blob. DEFERRED.
    //      Case C — self-Re-Join after Leave. DEFERRED.
    //    Cases B+C share the same root cause but require a gate
    //    redesign (blob pre-decrypt or self-publisher-bootstrap)
    //    rather than a side-channel; deferred until a real production
    //    blocker emerges. See
    //    docs/specs/2026-05-08-zeb-260-invite-only-cold-cache-design.md
    //    for the Case A fix design.
    // ZEB-339 Task 9: the membership gate materializes the publisher's
    // `MemberState` (incl. `enrolled_device_keys`); we retain it for the
    // sig-verify step below so we don't re-materialize.
    // ZEB-558: the gate yields (publisher_member_state, deferred_open_bootstrap).
    // For an OPEN community + entirely-unknown publisher we DEFER the reject:
    // the publisher's self-Join lives only inside the (not-yet-fetched) blob,
    // so we validate it post-decode via `bootstrap_admit_open_publisher`.
    let (publisher_member_state, deferred_open_bootstrap, deferred_invite_bootstrap): (
        Option<crate::community_membership::MemberState>,
        bool,
        bool,
    ) = {
        let state = ctx.state.lock().await;
        let events: Vec<SignedMembershipEvent> = state.events.values().cloned().collect();
        drop(state);
        let materialized =
            crate::community_membership::prior_state_at_hlc(&events, &payload.at, ctx.admin_addr);
        let member_state = materialized.members.get(&payload.publisher_addr).cloned();
        let status_now = member_state.as_ref().map(|s| s.status);
        if matches!(status_now, Some(MemberStatus::Joined)) {
            (member_state, false, false)
        } else if !ctx.is_invite_only && member_state.is_none() {
            // OPEN + entirely-unknown publisher → defer. We do NOT run the
            // prior-state publisher-sig check below (no enrolled keys yet);
            // `bootstrap_admit_open_publisher` (post-decode) supplies them.
            (None, true, false)
        } else if ctx.is_invite_only && member_state.is_none() {
            // ZEB-526: INVITE-ONLY + entirely-unknown publisher → defer (do NOT
            // reject pre-decode). The publisher's self-authorizing PendingJoin
            // (admin-signed InviteToken + EnrollmentCert) lives only inside the
            // not-yet-fetched blob; `bootstrap_admit_invite_only_publisher`
            // (post-decode) validates it and supplies the publisher's enrolled
            // keys for the root publisher-sig check. The authoritative merge then
            // inserts that PendingJoin (firing the admin's auto-counter-sign),
            // giving the invite-only join the zenoh-publish convergence FALLBACK
            // it lacked when the single iroh first-contact dial doesn't land
            // (offline party, cross-WAN flake, or the plain `redeem_invite` path).
            // The publisher is admitted ONLY as PendingJoin — their non-membership
            // events fail the per-event `verify_event` in the merge, and their
            // own root stays gated until the counter-sign makes them Joined.
            (None, false, true)
        } else {
            // known-but-Left/Banned, or a known-PendingJoin re-publishing
            // (we already hold their PendingJoin — no salvage needed; their
            // root is unauthorized until counter-signed) → strict reject
            // (unchanged). `member_state` is `Some` here (every `None` case is
            // handled by the two defer arms above). We collapse the status onto
            // `MemberStatus::Left` only via the `unwrap_or` below, which is now
            // unreachable but kept for type-safety; the security invariant
            // ("not Joined → reject") is unchanged.
            return IncomingOutcome::ErrPreMutation(CommunitySyncError::PublisherNotJoined {
                addr: payload.publisher_addr,
                status: status_now.unwrap_or(MemberStatus::Left),
                left_at: member_state.and_then(|s| s.left_at),
            });
        }
    };

    // 3+4. ZEB-339 Task 9: verify `payload.publisher_sig` against the
    //    publisher's MATERIALIZED enrolled device keys (from their
    //    EnrollmentCert-bearing Join), NOT a Reticulum
    //    `IdentityResolver` lookup + `address_hash` binding. The
    //    membership gate above already confirmed the publisher is
    //    `Joined` at `payload.at` and handed us their `MemberState`;
    //    `verify_publisher_sig` iterates the `enrolled_device_keys` set
    //    and accepts the publish iff some key `verify_strict`-validates
    //    the signature over `canonical_cbor(CommunityRootSignedPayload)`.
    //    No matching key (or empty set) → `PublisherSigInvalid`. Tracker
    //    NOT advanced.
    //
    //    This removes the old resolver gate entirely from the publish
    //    path: a remote member whose `owner_id` actor the
    //    `OwnerDeviceCacheResolver` could not resolve (cache keyed by
    //    Reticulum device-hash, not owner_id) is now authenticated
    //    directly from trusted local membership state.
    // ZEB-558: for the deferred open-bootstrap case we have no enrolled keys
    // yet — the publisher-sig check runs post-decode against keys derived from
    // the in-blob self-Join. For all other (known-Joined) publishers, verify
    // now against their materialized enrolled keys exactly as before.
    if !deferred_open_bootstrap && !deferred_invite_bootstrap {
        let pms = publisher_member_state
            .as_ref()
            .expect("non-deferred publisher ⇒ Some(member_state)");
        if let Err(e) = verify_publisher_sig(&payload, pms) {
            return IncomingOutcome::ErrPreMutation(e);
        }
    }

    // 5. Replay-protect via per-(addr, device) RootHlcTracker.
    //    Read-only — the `record` step happens at step 14 (after the
    //    state-merge under the TOCTOU re-check), so dedup and the
    //    single-mutation-point are now decoupled into separate steps.
    //    Keyed on the trusted `payload.publisher_addr` (sig-verify
    //    above proved the addr).
    //
    //    Note: a publish that passes here can still be rejected at
    //    step 12's re-check (concurrent local Leave/Kick) and
    //    therefore NOT advance the tracker. That's the intended
    //    semantic — re-receive of the same publish later will hit
    //    the same step-12 rejection until either the publisher's
    //    membership re-Joins (out-of-band) or the publisher republishes
    //    at a strictly newer HLC.
    {
        let tracker = ctx.tracker.lock().await;
        if !tracker.would_accept(&payload.publisher_addr, &payload.at) {
            return IncomingOutcome::Duplicate;
        }
    }

    // 6. Fetch the encrypted blob from CAS. Cache-miss is a pre-mutation
    //    failure — the publish carries a CID we couldn't resolve in
    //    time; CRDT eventual consistency lets the next state-root from
    //    any peer recover.
    let blob_ciphertext = match ctx.content_store.get(&payload.root_cid).await {
        Ok(Some(b)) => b,
        Ok(None) => {
            return IncomingOutcome::ErrPreMutation(CommunitySyncError::BlobNotFound {
                cid: payload.root_cid,
            });
        }
        Err(e) => return IncomingOutcome::ErrPreMutation(CommunitySyncError::ContentStore(e)),
    };

    // 7. Decrypt blob (deterministic-nonce).
    //    CR Major (PR #106 R6): blob decryption MUST use the SAME epoch
    //    key that successfully decrypted the root wire packet. Trying
    //    independent key sets for root and blob would allow an attacker
    //    to rewrap the outer layer under any accepted old key while
    //    substituting a blob encrypted under a different old key —
    //    breaking the root→blob epoch binding contract.
    let blob_cleartext = match decrypt_blob(root_key_used, &blob_ciphertext) {
        Ok(b) => b,
        Err(e) => return IncomingOutcome::ErrPreMutation(CommunitySyncError::Crypto(e)),
    };

    // 8. Decode CommunityState.
    let remote: CommunityState = match canonical_cbor_decode(&blob_cleartext) {
        Ok(s) => s,
        Err(e) => {
            return IncomingOutcome::ErrPreMutation(CommunitySyncError::CborDecode(e.to_string()))
        }
    };

    // 8b. Reject misrouted blob: blob's community_id must match the
    //     engine's expected community_id. Without this, a
    //     ContentStore-collision (vanishingly unlikely with SHA-256
    //     but cheap to gate) or buggy callsite could surface a
    //     foreign community's events under our key.
    if remote.community_id != ctx.community_id {
        return IncomingOutcome::ErrPreMutation(CommunitySyncError::MisroutedBlob {
            expected: ctx.community_id,
            found: remote.community_id,
        });
    }

    // Phase A: order the inner events for replay. ZEB-339 Task 9: the
    // old per-event resolver pre-resolution (and the owner_id-miss
    // skip-and-log that wrongly dropped valid remote events) is GONE —
    // `verify_event` (called by `insert_event` in Phase B) derives every
    // signer's ed25519 key itself, from the carried EnrollmentCert
    // (Join/PendingJoin) or the actor's materialized
    // `enrolled_device_keys` (steady-state). No identity_resolver round
    // trip remains in the merge path, so the prior lock-order hazard with
    // owner-state is also eliminated for this phase.
    //
    // **Replay order matters.** `BTreeMap<EventId, _>::into_values()`
    // walks in `EventId` byte order, but `insert_event` authorizes
    // each candidate against `prior_state_at_event` — i.e., everything
    // already in the local log that strictly precedes the candidate by
    // `event_sort_key`. If two events arrive in the same blob and the
    // later-by-replay-order event is processed first, its earlier
    // predecessor (still pending in our queue) is missing from
    // prior_state, and a valid event can land as `Rejected`. Sort
    // explicitly by `event_sort_key` so we merge in the same order
    // `materialize` would replay.
    let mut resolved: Vec<SignedMembershipEvent> = remote.events.into_values().collect();
    resolved.sort_by(|a, b| {
        crate::community_membership::event_sort_key(a)
            .cmp(&crate::community_membership::event_sort_key(b))
    });

    // ZEB-558: deferred open-bootstrap admission. The publisher was unknown at
    // the gate; validate the open self-Join (and any DeviceAnnounce/Leave) they
    // carry in this blob, bounded to strictly-before the root HLC (`payload.at`)
    // so admission uses the same pre-root membership window as the known-
    // publisher path. This derives their enrolled keys and confirms they are
    // Joined-at-root; we then verify the root publisher_sig against those keys.
    // The authoritative merge below re-validates and inserts the events.
    if deferred_open_bootstrap {
        match crate::community_membership::bootstrap_admit_open_publisher(
            &resolved,
            payload.publisher_addr,
            ctx.admin_addr,
            ctx.community_id,
            &payload.at,
        ) {
            Some(bootstrap_member_state) => {
                if let Err(e) = verify_publisher_sig(&payload, &bootstrap_member_state) {
                    return IncomingOutcome::ErrPreMutation(e);
                }
            }
            None => {
                // No signature-valid open self-Join for the publisher in this
                // blob → the publish is unauthorized; reject as before.
                return IncomingOutcome::ErrPreMutation(CommunitySyncError::PublisherNotJoined {
                    addr: payload.publisher_addr,
                    status: MemberStatus::Left,
                    left_at: None,
                });
            }
        }
    }

    // ZEB-526: deferred INVITE-ONLY-bootstrap admission. The publisher was
    // unknown at the gate; validate the self-authorizing PendingJoin (admin-
    // signed InviteToken + EnrollmentCert) they carry in this blob, bounded to
    // strictly-before the root HLC. Unlike the open sibling this admits a
    // PendingJoin (not Joined) publisher and includes the admin's own bootstrap
    // events in the authorization window so the InviteToken's admin-signer key
    // resolves. It derives the publisher's enrolled keys, against which we verify
    // the root publisher_sig; the authoritative merge below then inserts the
    // PendingJoin, firing the admin's auto-counter-sign so the join converges.
    if deferred_invite_bootstrap {
        match crate::community_membership::bootstrap_admit_invite_only_publisher(
            &resolved,
            payload.publisher_addr,
            ctx.admin_addr,
            ctx.community_id,
            &payload.at,
        ) {
            Some(bootstrap_member_state) => {
                if let Err(e) = verify_publisher_sig(&payload, &bootstrap_member_state) {
                    return IncomingOutcome::ErrPreMutation(e);
                }
            }
            None => {
                // No signature-valid self-authorizing PendingJoin for the
                // publisher in this blob → the publish is unauthorized; reject.
                return IncomingOutcome::ErrPreMutation(CommunitySyncError::PublisherNotJoined {
                    addr: payload.publisher_addr,
                    status: MemberStatus::Left,
                    left_at: None,
                });
            }
        }
    }

    // Phase B: lock community state once, run insert_event for each
    // event, collect rejections for out-of-lock reporting.
    let mut inserted_any = false;
    let mut rejection_reports: Vec<crate::community_membership::VerifyError> = Vec::new();
    // Buffer inserted-event clones for delta emission AFTER the state
    // lock is released. Same lock-discipline rationale as
    // `rejection_reports`: holding the state mutex across an
    // `mpsc::Sender::try_send` is technically non-blocking but keeping
    // the emit lock-free preserves the "no channel ops while holding
    // state" invariant the rest of this module follows.
    let mut inserted_events: Vec<SignedMembershipEvent> = Vec::new();
    // Restart-recovery: PendingJoins that returned AlreadyKnown still need
    // the auto-counter-sign and pending-clear hooks, but must NOT drive
    // delta emission or notify_pending_redemption_in_map (those should only
    // fire for genuinely new CRDT insertions). A separate vec keeps the
    // two concerns apart.
    let mut pending_joins_for_recheck: Vec<SignedMembershipEvent> = Vec::new();
    {
        let mut state = ctx.state.lock().await;

        // 12. TOCTOU re-check: a concurrent `insert_local_event()`
        //     call (the IPC path used by `redeem_invite`,
        //     `leave_community`, Phase 4 `kick`, etc.) may have
        //     landed an event between step 2's snapshot and now,
        //     including a Leave/Kick that would have failed the
        //     gate. Re-evaluate `prior_state_at_hlc(payload.at)` over
        //     the CURRENT events (held under the state lock so no
        //     further concurrent inserts can land before we merge).
        //     A negative outcome here returns ErrPreMutation WITHOUT
        //     advancing the tracker — the cheapest-first gate at
        //     step 2 was a pre-filter; THIS is the authoritative
        //     security check w.r.t. local state mutations.
        //     CodeRabbit PR #88 round 3 finding.
        //     ZEB-558 (CodeRabbit #336): the deferred open-bootstrap case runs
        //     this re-check too. The publisher is normally unknown here (prior
        //     state None) and admitted by THIS merge, but a concurrent local
        //     insert could have landed their Join AND a Leave/Kick between
        //     step 2's snapshot and now — skipping the re-check would let a
        //     now-Left/Banned publisher pass root authorization and advance the
        //     replay tracker. So we keep the check and only widen the accepted
        //     prior-state for the deferred path to include `None` (the expected
        //     unknown-publisher case) alongside `Joined`.
        {
            let events_now: Vec<SignedMembershipEvent> = state.events.values().cloned().collect();
            let mat_now = crate::community_membership::prior_state_at_hlc(
                &events_now,
                &payload.at,
                ctx.admin_addr,
            );
            let pub_state = mat_now.members.get(&payload.publisher_addr).cloned();
            let pub_status = pub_state.as_ref().map(|s| s.status);
            // Known-publisher path: must be Joined. Deferred bootstrap paths:
            // `None` is the normal unknown-publisher case (their self-authorizing
            // Join/PendingJoin is in `resolved`, admitted by this merge), so
            // accept None OR Joined. ONLY the invite-only deferred path
            // additionally accepts PendingJoin — that is the invite publisher's
            // admitted-but-uncountersigned state, which has no analogue in an
            // open community (open joins mint an immediate Join). Keeping the
            // PendingJoin allowance out of the open arm avoids widening the open
            // path's authorization surface. Still reject Left/Banned surfaced by
            // a concurrent insert.
            let authorized = if deferred_invite_bootstrap {
                matches!(
                    pub_status,
                    None | Some(MemberStatus::Joined) | Some(MemberStatus::PendingJoin)
                )
            } else if deferred_open_bootstrap {
                matches!(pub_status, None | Some(MemberStatus::Joined))
            } else {
                matches!(pub_status, Some(MemberStatus::Joined))
            };
            if !authorized {
                // Drop the lock before returning so error reporting
                // and the caller's persist machinery aren't serialized
                // behind an unrelated state lock release.
                drop(state);
                return IncomingOutcome::ErrPreMutation(CommunitySyncError::PublisherNotJoined {
                    addr: payload.publisher_addr,
                    // Same fallback semantics as step 2 — see the
                    // longer comment there for the rationale.
                    status: pub_status.unwrap_or(MemberStatus::Left),
                    left_at: pub_state.and_then(|s| s.left_at),
                });
            }
        }

        for event in resolved {
            if state.events.contains_key(&event.id) {
                // C1 restart-recovery: even though we've already seen this
                // event, check whether a self-authored JoinCountersign for
                // it needs to be emitted. This handles the case where the
                // engine restarted and reloaded a PendingJoin from disk
                // (returned AlreadyKnown on the reconcile insert) before
                // the auto-counter-sign hook had a chance to fire. We
                // collect the event for post-lock processing; we don't
                // flip `inserted_any` since the CRDT is unchanged.
                // NOTE: push to pending_joins_for_recheck, NOT inserted_events,
                // so hooks (auto-countersign, pending-clear) fire without
                // triggering delta emission or notify_pending_redemption.
                if matches!(
                    &event.kind,
                    crate::community_membership::MembershipEventKind::PendingJoin { .. }
                ) {
                    pending_joins_for_recheck.push(event);
                }
                continue;
            }

            // ZEB-339 Task 9: VerifyContext carries no caller-resolved
            // pubs; verify_event derives signer keys from the carried
            // cert / materialized membership.
            let ctx_v = VerifyContext {
                expected_community_id: ctx.community_id,
                admin_addr: ctx.admin_addr,
                is_invite_only: ctx.is_invite_only,
            };
            // Clone before `insert_event` consumes the event so we can
            // surface the delta if the outcome is `Inserted`. The clone
            // is cheap (a few hundred bytes of signed event) and only
            // paid on the merge path; duplicates short-circuit above.
            let event_clone = event.clone();
            match state.insert_event(event, &ctx_v) {
                InsertOutcome::Inserted => {
                    inserted_any = true;
                    inserted_events.push(event_clone);
                }
                InsertOutcome::AlreadyKnown => {
                    // Don't flip inserted_any — the CRDT is unchanged;
                    // without this, every duplicate Zenoh fanout echo would
                    // trigger a disk-persist on the Mutated arm at Task 10.
                    //
                    // C1 restart-recovery: if this is a PendingJoin that
                    // returned AlreadyKnown (event was already in the CRDT
                    // from a prior session / disk-reload), still schedule the
                    // counter-sign eligibility check. The check is idempotent
                    // — `spawn_auto_counter_sign_task` re-checks under the
                    // lock and returns immediately if a JoinCountersign
                    // already exists for this target.
                    // NOTE: push to pending_joins_for_recheck, NOT inserted_events.
                    if matches!(
                        &event_clone.kind,
                        crate::community_membership::MembershipEventKind::PendingJoin { .. }
                    ) {
                        pending_joins_for_recheck.push(event_clone);
                    }
                }
                InsertOutcome::Rejected(verr) => {
                    tracing::warn!(
                        community_id = ?ctx.community_id,
                        error = ?verr,
                        "skipping incoming event: verify_event rejected"
                    );
                    // Buffer rejection for out-of-lock reporting (Phase
                    // C below). Holding the state lock across the
                    // degraded-channel send would block local mutators
                    // when the channel is back-pressured.
                    rejection_reports.push(verr);
                }
            }
        }
    } // state lock released here

    // ZEB-254 Task 10: auto-counter-sign any freshly-Inserted PendingJoin
    // events. Spawn is fire-and-forget — doesn't block tracker advance or
    // delta emission. The spawned task re-acquires the state lock internally
    // for the final idempotency check + insert.
    // Also run for restart-recovery PendingJoins (pending_joins_for_recheck)
    // which returned AlreadyKnown but may still need a JoinCountersign.
    for event in inserted_events
        .iter()
        .chain(pending_joins_for_recheck.iter())
    {
        maybe_spawn_auto_counter_sign_for_ctx(ctx, event);
    }

    // ZEB-254 Task 11: joiner-side pending-join clear hook for every
    // freshly-Inserted JoinCountersign. Spawn is fire-and-forget — doesn't
    // block tracker advance or delta emission.
    //
    // R4-3: ALSO fire the rescan helper for every freshly-Inserted
    // PendingJoin so an out-of-order JoinCountersign already present in
    // the log triggers the same pending-clear path. Both helpers are
    // no-ops for the non-matching event kind.
    for event in &inserted_events {
        maybe_spawn_pending_join_clear(
            event,
            Arc::clone(&ctx.state),
            ctx.self_owner,
            ctx.community_id,
            ctx.crdt_state.clone(),
            ctx.nav_emitter.clone(),
        );
        maybe_spawn_pending_clear_rescan_for_pending_join(
            event,
            Arc::clone(&ctx.state),
            ctx.self_owner,
            ctx.community_id,
            ctx.crdt_state.clone(),
            ctx.nav_emitter.clone(),
        );
    }

    // 14. Advance the replay tracker — the SINGLE state-mutation
    //     point for tracker progress. Happens AFTER Phase B's
    //     state-merge (and AFTER the TOCTOU re-check inside Phase B)
    //     so a concurrent local Leave/Kick that races the gate
    //     leaves the tracker untouched, preserving the
    //     "tracker NOT advanced on any rejection" invariant under
    //     concurrent IPC mutations.
    {
        let mut tracker = ctx.tracker.lock().await;
        tracker.record(payload.publisher_addr, payload.at.clone());
    }

    // ZEB-501: wake the joiner's redeem oneshot ONLY on a real
    // JoinCountersign's `target_event_id` (see the matching note at the
    // local-insert hook). The legacy ZEB-262 notify-on-`event.id` is
    // removed — harmless here in practice (the joiner's own PendingJoin
    // echo arrives `AlreadyKnown`, not `Inserted`), but dropped for
    // symmetry so only a countersign ever wakes the redeem.
    if let Some(pending) = ctx.pending_redemptions.as_ref() {
        for event in &inserted_events {
            if let crate::community_membership::MembershipEventKind::JoinCountersign {
                target_event_id,
            } = &event.kind
            {
                notify_pending_redemption_in_map(pending, target_event_id).await;
            }
        }
    }

    // Phase C-pre: emit membership-delta for every inserted event
    // outside the state lock. `try_send` is fire-and-forget — a closed
    // or full channel drops the delta rather than back-pressuring the
    // engine (the IPC consumer is purely informational).
    if let Some(tx) = ctx.delta_tx.as_ref() {
        for event in inserted_events {
            let _ = tx.try_send(CommunityMembershipDelta {
                community_id: ctx.community_id,
                event,
            });
        }
    }

    // Phase C: emit rejection reports outside the state lock.
    // verify_event rejections at receive time are the most useful
    // signal for the frontend banner (forged sigs, insufficient power,
    // banned-actor replays etc). One bad event does not block valid
    // ones in the same publish — defense-in-depth at both layers
    // (Phase 1 spec §"Defense-in-depth").
    for verr in rejection_reports {
        report_degraded(
            ctx.error_tx.as_ref(),
            ctx.community_id,
            "verify_event_rejected",
            format!("{verr:?}"),
        );
    }

    // The tracker advanced (step 14) regardless of whether any event
    // was Inserted. Differentiate so Task 10 can persist the smaller
    // replay.cbor file alone when the CRDT is unchanged. See the
    // `IncomingOutcome` doc comments for the full rationale.
    if inserted_any {
        IncomingOutcome::Mutated
    } else {
        IncomingOutcome::MutatedTrackerOnly
    }
}

/// Snapshot the CRDT and replay tracker to disk.
///
/// **Lock and runtime discipline:** snapshot both Arcs under their
/// respective async locks (briefly), drop the guards, then offload
/// the actual `save_crdt` / `save_replay` calls to
/// `tokio::task::spawn_blocking`. Without spawn_blocking the sync
/// `std::fs::write` + `std::fs::rename` calls would park the tokio
/// worker thread for the full disk-write cost on every debounce
/// wakeup and every merge cycle.
///
/// Both saves are atomic-rename-via-tempfile, so a partial save can't
/// corrupt the live file. Failures bubble up as
/// `CommunitySyncError::Persist` so the shutdown arm can surface them
/// to the caller; the wakeup / merge arms log + continue.
/// Map a per-community save (`save_crdt`/`save_replay`) `PersistError` to a
/// `CommunitySyncError`, routing the io-`NotFound` case — a missing parent
/// directory, i.e. the ZEB-463 rollback race — to `PersistDirMissing` so the
/// shutdown arm can CAUSALLY downgrade it. `write_atomic` always
/// `create_dir_all`s its parent first, so the only way a save fails with
/// `NotFound` is a concurrent `remove_dir_all` deleting the dir mid-write.
/// Every other failure becomes a plain `Persist`.
fn map_persist_err(e: crate::community_state_persist::PersistError) -> CommunitySyncError {
    use crate::community_state_persist::PersistError;
    match &e {
        PersistError::Io(io) if io.kind() == std::io::ErrorKind::NotFound => {
            CommunitySyncError::PersistDirMissing(e.to_string())
        }
        _ => CommunitySyncError::Persist(e.to_string()),
    }
}

/// ZEB-463 + ZEB-633: decide whether a graceful-shutdown final-flush failure
/// is the benign "lost the race with a concurrent rollback that removed this
/// community's persistence directory" case.
///
/// Two-layer decision:
///
/// 1. CAUSAL (ZEB-463): `PersistDirMissing` — the variant `map_persist_err`
///    produces ONLY when the underlying io error was `NotFound` — is the race
///    by construction. Always downgraded, no dir check needed.
/// 2. DIR-GONE (ZEB-633): the same race surfaces as OTHER errnos too —
///    observed live: `Persist("io: Invalid argument (os error 22)")` when
///    `write_atomic`'s rename ran against a directory `remove_dir_all` was
///    concurrently tearing down (macOS/APFS). For a `Persist` failure, if the
///    community dir is GONE at check time the flush is moot whatever the
///    errno: the ONLY removers of that dir are intentional discards (rollback
///    / leave-cleanup), so there is nothing left to be durable into. This is
///    deliberately narrower than the post-hoc heuristic Qodo flagged on
///    PR #267 — a `Persist` fault with the dir STILL PRESENT (the real-disk-
///    fault case ZEB-460 protects) still propagates, as does every non-persist
///    error and `Ok`.
fn shutdown_flush_lost_race_to_dir_removal(
    result: &Result<(), CommunitySyncError>,
    community_dir: Option<&std::path::Path>,
) -> bool {
    match result {
        Err(CommunitySyncError::PersistDirMissing(_)) => true,
        Err(CommunitySyncError::Persist(_)) => {
            // `try_exists`, not `exists` (Qodo + CodeRabbit, PR #397 R1):
            // downgrade ONLY on a CONFIRMED absence — `Ok(false)`. A probe
            // error (`Err`: permission, transient FS fault) or a present dir
            // (`Ok(true)`) propagates the persist failure, so an unreadable
            // dir can never masquerade as the intentional-discard race.
            matches!(community_dir.map(|dir| dir.try_exists()), Some(Ok(false)))
        }
        _ => false,
    }
}

async fn persist_both(ctx: &InternalCtx) -> Result<(), CommunitySyncError> {
    // Snapshot under locks — clones are cheap (CRDT is a BTreeMap of
    // signed events, tracker is a small per-device map), and far
    // cheaper than holding a lock across blocking I/O.
    let state_snap = ctx.state.lock().await.clone();
    let tracker_snap = ctx.tracker.lock().await.clone();
    let crdt_path = ctx.paths.crdt.clone();
    let replay_path = ctx.paths.replay.clone();
    tokio::task::spawn_blocking(move || -> Result<(), CommunitySyncError> {
        save_crdt(&crdt_path, &state_snap).map_err(map_persist_err)?;
        save_replay(&replay_path, &tracker_snap).map_err(map_persist_err)?;
        Ok(())
    })
    .await
    .map_err(|join_err| {
        CommunitySyncError::Persist(format!("spawn_blocking join failed: {join_err}"))
    })??;
    Ok(())
}

/// Replay-only persist for the `MutatedTrackerOnly` case — every event
/// in the remote blob was `AlreadyKnown` but the tracker advanced. The
/// CRDT is byte-identical, so re-fsyncing `crdt.cbor` would be wasted
/// I/O on every duplicate-but-clock-advanced publish. Only `replay.cbor`
/// rewrites here.
///
/// Same lock + runtime discipline as `persist_both`: snapshot under
/// the tracker lock, drop the guard, run the disk write in
/// `spawn_blocking`.
async fn persist_replay_only(ctx: &InternalCtx) -> Result<(), CommunitySyncError> {
    let tracker_snap = ctx.tracker.lock().await.clone();
    let replay_path = ctx.paths.replay.clone();
    tokio::task::spawn_blocking(move || -> Result<(), CommunitySyncError> {
        save_replay(&replay_path, &tracker_snap).map_err(map_persist_err)
    })
    .await
    .map_err(|join_err| {
        CommunitySyncError::Persist(format!("spawn_blocking join failed: {join_err}"))
    })??;
    Ok(())
}

/// CRDT-only persist for the publish-FAILURE case (ZEB-462 B). The
/// community membership CRDT is a monotonic accumulation of VALIDATED
/// events — durable facts that must survive a crash regardless of whether
/// the outbound state-root publish landed. The publish-gated arms
/// (debounce / flush_now / shutdown) therefore persist `crdt.cbor`
/// unconditionally and only withhold the `replay.cbor` tracker advance
/// when the publish failed — that advance (from `encode_root_packet`'s
/// `next_hlc`) is meaningless until peers actually receive it, so leaving
/// it un-persisted lets the next boot retry the publish from the same HLC.
///
/// Without this, a joiner whose co-located publish never durably lands
/// would never write `crdt.cbor` until a GRACEFUL shutdown — a SIGKILL
/// then loses the entire membership (admin + self), which materializes as
/// the publish gate's synthetic `Left`/`left_at: None` for every
/// publisher after restart (ZEB-462 B).
///
/// Same lock + runtime discipline as `persist_both`: snapshot the CRDT
/// under its lock, drop the guard, run the disk write in `spawn_blocking`.
async fn persist_crdt_only(ctx: &InternalCtx) -> Result<(), CommunitySyncError> {
    let state_snap = ctx.state.lock().await.clone();
    let crdt_path = ctx.paths.crdt.clone();
    tokio::task::spawn_blocking(move || -> Result<(), CommunitySyncError> {
        save_crdt(&crdt_path, &state_snap).map_err(map_persist_err)
    })
    .await
    .map_err(|join_err| {
        CommunitySyncError::Persist(format!("spawn_blocking join failed: {join_err}"))
    })??;
    Ok(())
}

/// Send a `CommunityDegradedReport` if `error_tx` is wired.
///
/// **Fire-and-forget semantics.** Uses `try_send` so a full degraded
/// channel falls back to dropping the report rather than back-
/// pressuring the engine's `select!` loop. The engine is already
/// degraded by the time we emit — adding a tokio task stall on a full
/// channel would compound the degradation. A dropped report is logged
/// at debug level for diagnostics; the next degraded event from the
/// same community will re-trigger the frontend banner.
fn report_degraded(
    error_tx: Option<&mpsc::Sender<CommunityDegradedReport>>,
    community_id: SpaceId,
    reason_tag: &'static str,
    detail: String,
) {
    if let Some(tx) = error_tx {
        match tx.try_send(CommunityDegradedReport {
            community_id,
            reason_tag,
            detail,
        }) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Full(report)) => {
                tracing::debug!(
                    community_id = ?report.community_id,
                    reason_tag = report.reason_tag,
                    "community degraded report dropped: channel full"
                );
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(report)) => {
                tracing::warn!(
                    community_id = ?report.community_id,
                    reason_tag = report.reason_tag,
                    "community degraded report dropped: channel closed (drain task gone)"
                );
            }
        }
    }
}

/// Translate a `CommunitySyncError` into a stable short reason-tag for
/// `CommunityDegradedReport`. Stable across versions so the frontend's
/// banner copy can switch on the tag without parsing free-form
/// `detail` strings; new variants get appended over time as new
/// failure classes surface.
fn classify_incoming_error(err: &CommunitySyncError) -> &'static str {
    match err {
        CommunitySyncError::Crypto(_) => "decrypt_failed",
        CommunitySyncError::CborEncode(_) | CommunitySyncError::CborDecode(_) => {
            "wire_decode_failed"
        }
        CommunitySyncError::ContentStore(_) => "blob_fetch_failed",
        CommunitySyncError::BlobNotFound { .. } => "blob_not_found",
        CommunitySyncError::TransportClosed => "transport_closed",
        CommunitySyncError::Persist(_) => "persist_failed",
        CommunitySyncError::PersistDirMissing(_) => "persist_failed",
        CommunitySyncError::MisroutedBlob { .. } => "misrouted_blob",
        CommunitySyncError::PublisherNotJoined { .. } => "publisher_not_joined",
        CommunitySyncError::PublisherSigInvalid { .. } => "publisher_sig_invalid",
        CommunitySyncError::PublishRetryExhausted => "publish_retry_exhausted",
        CommunitySyncError::LiveEpochKeyMissing(_) => "live_epoch_key_missing",
        // ZEB-732: local cleanup-abort; never produced by the incoming-sync
        // path this classifier serves, but the match must stay exhaustive.
        CommunitySyncError::CleanupAborted(_) => "cleanup_aborted",
    }
}

/// ZEB-618: sidecar dir for a community's root-fetch resync stamp —
/// the community's own engine dir (same layout `paths_for`/PersistPaths
/// derives). Free fn so the config-derivation unit test can pin the
/// layout without a registry instance.
fn community_root_resync_dir(identity_dir: &std::path::Path, id: &SpaceId) -> std::path::PathBuf {
    identity_dir.join("communities").join(hex::encode(id.0))
}

/// ZEB-732: a process-unique suffix for the detach-then-delete temp name in
/// [`CommunitySyncRegistry::shutdown_engine_and_cleanup_persistence`]. The
/// monotonic `SEQ` guarantees uniqueness within a run; the wall-clock nanos
/// disambiguate across process restarts, so a temp dir orphaned by a crash
/// mid-delete cannot collide with a fresh detach (a collision would make the
/// `rename` fail and leave the community dir un-deleted).
fn unique_detach_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos}.{seq}")
}

/// Test-only re-export of `classify_incoming_error`. Lets the unit
/// test pin the reason_tag → variant mapping without exposing the
/// internal function as part of the public API.
#[doc(hidden)]
pub fn classify_incoming_error_for_test(err: &CommunitySyncError) -> &'static str {
    classify_incoming_error(err)
}

/// Construction-time config for `CommunitySyncRegistry::new`. The
/// registry clones the relevant pieces into every spawned engine's
/// `CommunitySyncEngineConfig` (content_store + identity_resolver +
/// device_id + debounce_ms + error_tx). The persist paths are derived
/// per-community from `identity_dir` via `paths_for`.
pub struct CommunityRegistryConfig {
    /// This device's stable ID, used as the publisher key in every
    /// engine's HLC and replay tracker.
    pub device_id: String,
    /// Shared CAS handle. Cloned (Arc bump) into every engine.
    pub content_store: Arc<dyn ContentStore>,
    /// Resolver for `OwnerAddr` -> 64-byte identity_pub at receive-side
    /// `verify_event` time. Production wires Sub-A's owner-device
    /// cache via `OwnerDeviceCacheResolver` (Task 13); test stubs
    /// implement the trait directly.
    pub identity_resolver: Arc<dyn IdentityResolver>,
    /// Filesystem root under which per-community subdirectories live
    /// (`identity_dir/communities/{id_hex}/`). The registry derives
    /// each engine's `PersistPaths` from this.
    pub identity_dir: PathBuf,
    /// Debounce window between local mutations and the resulting
    /// state-root publish. See `DEFAULT_DEBOUNCE_MS`.
    pub debounce_ms: u64,
    /// Optional degraded-path channel. When `Some`, the registry
    /// clones the sender into every engine's `CommunitySyncEngineConfig`,
    /// and the receiver-side (owned by start_node — Task 13) translates
    /// `CommunityDegradedReport`s into `community-state-sync-degraded`
    /// Tauri events. `None` for tests that don't assert on IPC events.
    pub error_tx: Option<mpsc::Sender<CommunityDegradedReport>>,
    /// Optional membership-delta channel. When `Some`, the registry
    /// clones the sender into every engine's `CommunitySyncEngineConfig`,
    /// and the receiver-side (owned by start_node — Phase 3 Task 8)
    /// translates `CommunityMembershipDelta`s into
    /// `community-members-changed` Tauri events. `None` for tests that
    /// don't assert on IPC events.
    pub delta_tx: Option<mpsc::Sender<CommunityMembershipDelta>>,

    /// Owner address of the local member. Cloned into every engine's
    /// `CommunitySyncEngineConfig.self_owner`. Stable across all
    /// communities for a single node: one identity, one address.
    ///
    /// ZEB-256 (Task 6 verify-on-receive): a peer's `publisher_addr`
    /// is now load-bearing. The receive-side membership-at-HLC gate
    /// and sig-verify both key off it. Plumbed here so every
    /// `spawn_engine` call gets a consistent self-identity rather
    /// than a per-call argument that could drift.
    pub self_owner: OwnerAddr,

    /// ZEB-339 Task 9: ENROLLED device signing key (device #2), shared
    /// across every spawned engine. Wrapped in `Arc` so engine spawns are
    /// cheap (Arc bump, no secret-byte copy). Production sources this from
    /// the loaded enrollment (`community_signing_key_arc`), NOT the
    /// Reticulum identity key — the engine uses it for the state-root
    /// publisher signature and the auto-emitted JoinCountersign, both of
    /// which are verified against the signer's materialized
    /// `enrolled_device_keys`.
    pub signing_key: Arc<ed25519_dalek::SigningKey>,

    /// ZEB-249 §10.6 (Phase A): optional reference to the owner-state
    /// CRDT. When `Some`, every spawned engine receives a clone of this
    /// Arc so `publish_root_now` / `handle_incoming_publish` can read
    /// the live epoch key. `None` for tests that use the spawn-time key.
    pub crdt_state: Option<Arc<Mutex<crate::owner_state_crdt::OwnerState>>>,

    /// ZEB-254 Task 11: optional callback cloned into every spawned engine's
    /// `CommunitySyncEngineConfig.nav_emitter`. Production passes a closure
    /// that calls `app.emit("nav-updated", { pending: false, ... })` on the
    /// Tauri `AppHandle`. `None` for tests and for registries that don't
    /// handle joiner-side pending-clear events.
    pub nav_emitter: Option<NavPendingClearEmitter>,

    /// ZEB-618: presence-driven reachability kick for each engine's
    /// root-fetch driver (ZEB-599 D1 parity with the channel-log
    /// drivers). Cloned per spawned driver. `None` for tests/callers
    /// without presence.
    pub presence_resync_rx: Option<tokio::sync::watch::Receiver<u64>>,
}

/// Multi-community engine lifecycle manager. Owns
/// `BTreeMap<SpaceId, Arc<CommunitySyncEngine>>` under a `tokio::Mutex`
/// — `BTreeMap` rather than `HashMap` so `known_ids()` returns a stable
/// ordering (Task 12 diffs against the owner-state membership snapshot,
/// and a stable order keeps that diff readable in logs).
///
/// All public methods are async because they take the engines map
/// under the tokio Mutex; callers should not assume any of them are
/// cheap. `spawn_engine` in particular performs disk I/O
/// (`load_crdt` + `load_replay`) under the lock.
pub struct CommunitySyncRegistry {
    cfg: Arc<CommunityRegistryConfig>,
    engines: tokio::sync::Mutex<BTreeMap<SpaceId, Arc<CommunitySyncEngine>>>,
    /// ZEB-249 §10.6 (Phase A): optional owner-state CRDT reference,
    /// cloned from `CommunityRegistryConfig.crdt_state` at construction.
    /// Passed into every `spawn_engine_inner_now` call so engines can
    /// read the live epoch key. `None` for tests.
    crdt_state: Option<Arc<Mutex<crate::owner_state_crdt::OwnerState>>>,
    /// ZEB-262 Phase 4: per-`EventId` oneshots that fire when the
    /// matching `SignedMembershipEvent` is Inserted into ANY engine in
    /// this registry. The `redeem_invite` IPC registers a oneshot
    /// keyed on its minted `bootstrap_join.id` BEFORE sending the
    /// `CommunityInvitePacket`, then awaits the oneshot with timeout.
    /// The engine's post-Inserted hook (`insert_local_event` for the
    /// local-mint path, `handle_incoming_publish` for the receive
    /// path) calls into the shared map and fires any matching
    /// oneshot.
    ///
    /// **Plumb shape:** the engine doesn't hold a back-reference to
    /// the registry. Instead, the registry shares this `Arc<Mutex<…>>`
    /// directly with each spawned engine via
    /// `CommunitySyncEngineConfig.pending_redemptions`. This avoids
    /// an `Arc<Self>`/`Weak<Self>` cycle and keeps the notify path
    /// sync (`oneshot::Sender::send` is sync — the map lock is held
    /// only across the `remove`).
    ///
    /// **Lock-discipline:** the map is held under a `tokio::sync::Mutex`.
    /// The helpers below (and the engine call sites) ALL take the lock,
    /// extract / mutate, drop the guard, then operate on the recovered
    /// `Sender`. Never hold the guard across an `.await` of the
    /// recovered sender.
    pending_redemptions: PendingRedemptionMap,

    /// ZEB-254 Task 11: optional nav-updated callback cloned from
    /// `CommunityRegistryConfig.nav_emitter` at construction. Passed
    /// to every spawned engine so each can fire the joiner-side
    /// pending-clear hook independently. `None` for tests.
    nav_emitter: Option<NavPendingClearEmitter>,

    /// ZEB-434 D3/D4: per-community shutdown senders for the spawned
    /// `run_root_fetch_driver` tasks. Inserted when a spawn wires a
    /// `fetch_request_tx`; removed + flipped by `stop_engine` /
    /// `shutdown_all` so the driver ends with its engine.
    ///
    /// **Lock-discipline:** never held together with the `engines`
    /// lock — every site acquires them strictly sequentially (engines
    /// guard dropped first). `watch::Sender::send` is sync, so no
    /// `.await` happens with the guard alive.
    root_fetch_shutdowns:
        tokio::sync::Mutex<std::collections::HashMap<SpaceId, tokio::sync::watch::Sender<bool>>>,
}

// ── CommunitySyncSpawnGuard (ZEB-274) ─────────────────────────────────────────

/// RAII rollback guard for a freshly-spawned community-sync engine.
/// Held by an IPC handler across the critical section between
/// `spawn_engine_with_guard` and the durable `apply_space` commit.
///
/// Drop without explicit `commit()` or `abort()` triggers a
/// `Handle::try_current()` safety-net that calls
/// `rollback_fresh_spawn` (only if THIS call freshly created the
/// engine — concurrent-redeem race losers per ZEB-260 PR #90 round-5
/// are no-ops on Drop). ZEB-436: the rollback removes the persistence
/// dir only when this spawn also created it; a dir that predates the
/// spawn (orphan re-adoption, rejoin-after-leave) is preserved.
///
/// **No-runtime fallback:** unlike ZEB-271's `CommunityTransactionGuard`
/// (whose `abort_transaction_internal` is sync map cleanup),
/// the rollback is fundamentally async
/// (`engine.shutdown().await` flushes pending writes). When `Drop`
/// runs without a tokio runtime, we log a warn and accept the leak —
/// `reconcile_from_state` at next `start_node` will detect the
/// inconsistency and clean up. See spec §10.2.
pub struct CommunitySyncSpawnGuard {
    registry: std::sync::Arc<CommunitySyncRegistry>,
    community_id: SpaceId,
    /// Set ONCE by `spawn_engine_with_guard` before it returns. True
    /// iff this call freshly created the engine (vs. the idempotent
    /// no-op path that found an existing engine). Only fresh
    /// creations carry the rollback obligation. Plain `bool` (not
    /// `AtomicBool`): no concurrent mutation — set ONCE before
    /// `spawn_engine_with_guard` returns the guard to the caller, then
    /// only read by Drop.
    freshly_created: bool,
    /// ZEB-436: true when the per-community `crdt.cbor` already existed
    /// on disk BEFORE this spawn. `freshly_created` tracks ENGINE
    /// freshness (registry map insertion); this tracks DATA freshness.
    /// A fresh engine over pre-existing data — orphan re-adoption
    /// (ZEB-427), rejoin-after-leave — must roll back by stopping the
    /// engine WITHOUT deleting the dir: it holds the user's entire
    /// community history, which the unconditional cleanup here used to
    /// destroy. Plain `bool`, same single-writer discipline as
    /// `freshly_created` (set once by `spawn_engine_with_guard`, then
    /// only read by `abort`/`Drop`).
    preserve_persistence: bool,
    /// Set to `true` by `commit()` to bypass `Drop`'s rollback path.
    /// `AtomicBool` (mirrors ZEB-271) for Acquire/Release ordering
    /// across the Drop visibility boundary.
    completed: std::sync::atomic::AtomicBool,
    /// Use-once flag (CR round 4 finding #2): set by
    /// `spawn_engine_with_guard` via `compare_exchange(false, true)`.
    /// If a second call sees `true`, it returns Err immediately. This
    /// prevents the failure mode where a guard is reused and its
    /// `freshly_created` field gets overwritten — silently disabling
    /// the rollback obligation from the first spawn. Non-resettable.
    used: std::sync::atomic::AtomicBool,
}

impl CommunitySyncSpawnGuard {
    /// Release the rollback obligation. The engine remains alive.
    /// Called by the IPC handler after `apply_space` succeeds (after
    /// the durable commit point). Sync — no `.await` needed because
    /// there is no work to do beyond setting the flag. Consumes self
    /// so Drop never runs after commit.
    pub fn commit(self) {
        self.completed
            .store(true, std::sync::atomic::Ordering::Release);
        // self drops here; Drop sees completed=true and runs no cleanup.
    }

    /// Explicit rollback. Runs `rollback_fresh_spawn` if
    /// `freshly_created` (engine stop, plus dir cleanup ONLY when this
    /// spawn also created the persistence dir — ZEB-436). Sets
    /// `completed = true` so `Drop` is a no-op. Sync entry point but
    /// spawns the async cleanup as a tokio task internally (mirrors
    /// ZEB-271 `CommunityTransactionGuard::abort` shape). If no tokio
    /// runtime is present, logs a warn and accepts the leak (per spec
    /// §10.2 — no sync alternative for `engine.shutdown().await`).
    pub fn abort(self) {
        if self.freshly_created {
            match tokio::runtime::Handle::try_current() {
                Ok(handle) => {
                    let registry = std::sync::Arc::clone(&self.registry);
                    let community_id = self.community_id;
                    let preserve_persistence = self.preserve_persistence;
                    handle.spawn(async move {
                        if let Err(e) = registry
                            .rollback_fresh_spawn(&community_id, preserve_persistence)
                            .await
                        {
                            tracing::warn!(
                                community_id = ?community_id,
                                error = %e,
                                "CommunitySyncSpawnGuard::abort cleanup failed \
                                 (engine + persist dir may leak; \
                                 reconcile_from_state will recover at next start_node)"
                            );
                        }
                    });
                }
                Err(_) => {
                    tracing::warn!(
                        community_id = ?self.community_id,
                        "CommunitySyncSpawnGuard::abort called without runtime; \
                         cannot run async cleanup. Engine + persist dir will leak \
                         until reconcile_from_state at next start_node."
                    );
                }
            }
        }
        self.completed
            .store(true, std::sync::atomic::Ordering::Release);
        // self drops here; Drop sees completed=true.
    }
}

impl Drop for CommunitySyncSpawnGuard {
    fn drop(&mut self) {
        if !self.completed.load(std::sync::atomic::Ordering::Acquire) && self.freshly_created {
            tracing::warn!(
                community_id = ?self.community_id,
                "CommunitySyncSpawnGuard dropped without commit/abort — \
                 running safety net (spec §5.1)"
            );
            match tokio::runtime::Handle::try_current() {
                Ok(handle) => {
                    let registry = std::sync::Arc::clone(&self.registry);
                    let community_id = self.community_id;
                    let preserve_persistence = self.preserve_persistence;
                    handle.spawn(async move {
                        if let Err(e) = registry
                            .rollback_fresh_spawn(&community_id, preserve_persistence)
                            .await
                        {
                            tracing::warn!(
                                community_id = ?community_id,
                                error = %e,
                                "CommunitySyncSpawnGuard Drop cleanup failed \
                                 (engine + persist dir may leak; \
                                 reconcile_from_state will recover at next start_node)"
                            );
                        }
                    });
                }
                Err(_) => {
                    tracing::warn!(
                        community_id = ?self.community_id,
                        "CommunitySyncSpawnGuard dropped without runtime; \
                         cannot run async cleanup. Engine + persist dir will leak \
                         until reconcile_from_state at next start_node (spec §10.2)."
                    );
                }
            }
        }
    }
}

impl CommunitySyncRegistry {
    pub fn new(cfg: CommunityRegistryConfig) -> Self {
        let crdt_state = cfg.crdt_state.as_ref().map(Arc::clone);
        let nav_emitter = cfg.nav_emitter.clone();
        Self {
            cfg: Arc::new(cfg),
            engines: tokio::sync::Mutex::new(BTreeMap::new()),
            crdt_state,
            pending_redemptions: std::sync::Arc::new(tokio::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            nav_emitter,
            root_fetch_shutdowns: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Open a spawn-rollback guard. Returns immediately, performs no I/O.
    /// Caller then calls `spawn_engine_with_guard(&mut guard, ...)` to
    /// perform the actual spawn — the guard captures the freshness flag
    /// internally. If the caller fails before `commit()`, `Drop` runs
    /// `shutdown_engine_and_cleanup_persistence`. See spec §3.1, §3.2.
    ///
    /// `begin_spawn_guard` is sync (no I/O, no lock acquisition) — the
    /// guard is created with `freshly_created = false` (set later by
    /// `spawn_engine_with_guard` if the spawn was the fresh one).
    pub fn begin_spawn_guard(
        self: &std::sync::Arc<Self>,
        community_id: SpaceId,
    ) -> CommunitySyncSpawnGuard {
        CommunitySyncSpawnGuard {
            registry: std::sync::Arc::clone(self),
            community_id,
            freshly_created: false,
            preserve_persistence: false,
            completed: std::sync::atomic::AtomicBool::new(false),
            used: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Atomic spawn + adapter dispatch with RAII rollback. Replaces the
    /// old `spawn_engine` public surface. Internalizes the freshness flag
    /// (formerly the `Result<bool, _>` return) into the guard so concurrent
    /// callers can't race on rollback obligation. See spec §3.2, §5.3.
    ///
    /// Sequence (atomic from caller's perspective):
    ///   1. `spawn_engine_inner_now` builds the engine, inserts into the
    ///      map. Returns `bool` for freshly-created.
    ///   2. If freshly created, `community_adapter_tx.try_send(...)` to
    ///      dispatch the adapter request to event_loop.
    ///   3. If try_send fails AND freshly created, immediately `.await`
    ///      `rollback_fresh_spawn` to undo the spawn (ZEB-436:
    ///      preservation-aware — a persistence dir that predates the
    ///      spawn survives). Returns Err. Guard's `freshly_created` flag
    ///      is NEVER set to true (so Drop is a no-op).
    ///   4. On full success, set `guard.freshly_created = true` (or false
    ///      for the idempotent path) and return Ok(engine).
    ///
    /// **ZEB-434**: mirrors `spawn_engine_inner_now`'s catch-up params —
    /// `root_serve_rx` / `fetch_request_tx` / `transport_epoch_rx` are
    /// the engine/driver halves forwarded to the inner spawn (legacy/
    /// test callers pass `CatchUpChannels::none()`); `root_serve_tx` /
    /// `fetch_request_rx` are the adapter halves packed into the
    /// `CommunityAdapterRequest`. On the idempotent path all five are
    /// dropped alongside the pub/sub adapter halves (the existing
    /// engine + adapter already own their live channels).
    #[allow(clippy::too_many_arguments)]
    pub async fn spawn_engine_with_guard(
        self: &std::sync::Arc<Self>,
        guard: &mut CommunitySyncSpawnGuard,
        community_id: SpaceId,
        membership_key: EpochKey,
        admin_addr: OwnerAddr,
        is_invite_only: bool,
        publisher_tx: mpsc::Sender<Vec<u8>>,
        subscriber_rx: mpsc::Receiver<Vec<u8>>,
        publisher_rx: mpsc::Receiver<Vec<u8>>,
        subscriber_tx: mpsc::Sender<Vec<u8>>,
        community_adapter_tx: mpsc::Sender<crate::event_loop::CommunityAdapterRequest>,
        catch_up: CatchUpChannels,
        root_serve_tx: mpsc::Sender<RootServeRequest>,
        fetch_request_rx: mpsc::Receiver<crate::event_loop::CommunityRootFetchRequest>,
    ) -> Result<std::sync::Arc<CommunitySyncEngine>, CommunitySyncError> {
        // Defensive: guard must be for the same registry instance AND the
        // same community_id. Programming errors otherwise — the IPC handler
        // should always pair them. Both checks are RUNTIME (not
        // debug_assert) so release builds also reject mismatched pairs;
        // otherwise a wrong-guard call would silently succeed and the
        // guard's later Drop would tear down the wrong registry/community.
        // CR round 2: community_id check. CR round 3: registry-Arc check
        // — without it, a guard from registry A could be used with
        // registry B's spawn_engine_with_guard, and the guard's Drop
        // would later tear down community_id from registry A.
        if !std::sync::Arc::ptr_eq(&guard.registry, self) {
            return Err(CommunitySyncError::Persist(format!(
                "spawn_engine_with_guard guard/registry mismatch — programming error \
                 (guard from a different CommunitySyncRegistry instance; \
                 community_id = {community_id:?})"
            )));
        }
        if guard.community_id != community_id {
            return Err(CommunitySyncError::Persist(format!(
                "spawn_engine_with_guard guard/community_id mismatch — programming error \
                 (guard for {:?}, called with {:?})",
                guard.community_id, community_id
            )));
        }

        // CR round 4 finding #2: use-once enforcement. compare_exchange
        // returns Err if `used` was already true → reject the call.
        // Without this, a second call on the same guard would overwrite
        // freshly_created and silently disable the rollback obligation
        // from the first spawn. Non-resettable: the guard is exhausted
        // after one attempt regardless of success/failure. Callers must
        // create a new guard via begin_spawn_guard for retries.
        if guard
            .used
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::Acquire,
                std::sync::atomic::Ordering::Relaxed,
            )
            .is_err()
        {
            return Err(CommunitySyncError::Persist(format!(
                "spawn_engine_with_guard called twice on the same guard — \
                 programming error (community_id = {community_id:?}). \
                 Guards are use-once; create a fresh one via begin_spawn_guard."
            )));
        }

        // ZEB-436: capture DATA freshness before the spawn can create
        // anything on disk. PR #229 R1 (Qodo + CodeRabbit): the marker is
        // the community DIR, not `crdt.cbor` — `channels/...` holds
        // durable history that can outlive a quarantined `crdt.cbor` —
        // and a probe failure defaults to PRESERVE: when freshness can't
        // be determined, a leaked dir (reconcile_from_state recovers at
        // next start_node) beats deleting a user's history.
        let community_dir = self
            .cfg
            .identity_dir
            .join("communities")
            .join(hex::encode(community_id.0));
        let preexisting_persistence = match tokio::fs::try_exists(&community_dir).await {
            Ok(exists) => exists,
            Err(e) => {
                tracing::warn!(
                    community_id = ?community_id,
                    path = ?community_dir,
                    error = %e,
                    "pre-spawn persistence probe failed; preserving persistence on rollback"
                );
                true
            }
        };

        // Step 1: spawn the engine via the inner helper.
        let freshly_created = self
            .spawn_engine_inner_now(
                community_id,
                membership_key,
                admin_addr,
                is_invite_only,
                publisher_tx,
                subscriber_rx,
                catch_up,
            )
            .await?;

        // CR round 4 finding #3: ARM the guard NOW (before any further
        // failure-prone work) so the Drop safety net catches any later
        // failure in this function. Set-early pattern: we'd rather have
        // Drop run a redundant teardown than leave an orphaned engine.
        // This also fixes the engine_arc-vanished race below — without
        // arming-early, the rare engine_arc lookup-fail path would leave
        // the engine in the registry without cleanup.
        guard.freshly_created = freshly_created;
        guard.preserve_persistence = preexisting_persistence;

        // Step 2: if fresh, dispatch the adapter request.
        if freshly_created {
            if let Err(send_err) =
                community_adapter_tx.try_send(crate::event_loop::CommunityAdapterRequest {
                    id_hex: hex::encode(community_id.0),
                    publisher_rx,
                    subscriber_tx,
                    root_serve_tx,
                    fetch_request_rx,
                })
            {
                // Step 3: try_send failed → undo the spawn inline.
                // ZEB-436: preservation-aware — never delete a dir this
                // spawn didn't create.
                match self
                    .rollback_fresh_spawn(&community_id, preexisting_persistence)
                    .await
                {
                    Ok(_) => {
                        // Inline cleanup succeeded → DISARM the guard
                        // (Drop would otherwise run a redundant teardown
                        // that finds nothing and logs spurious warns).
                        guard.freshly_created = false;
                    }
                    Err(stop_err) => {
                        // Inline cleanup ALSO failed → leave guard armed
                        // (freshly_created stays true) so the Drop safety
                        // net retries the cleanup. CR round 4 finding #3.
                        tracing::warn!(
                            community_id = ?community_id,
                            error = %stop_err,
                            "spawn_engine_with_guard: cleanup after adapter try_send failure also failed — \
                             leaving guard armed for Drop retry; if Drop also fails, reconcile recovers at next start_node"
                        );
                    }
                }
                return Err(CommunitySyncError::Persist(format!(
                    "community_adapter_tx.try_send failed: {send_err}"
                )));
            }
        }
        // ELSE: engine pre-existed (idempotent path); the publisher_rx +
        // subscriber_tx + community_adapter_tx args are dropped (the
        // existing engine + adapter already own their channels).
        // freshly_created was set false above; guard's Drop is a no-op.

        // (Set-early arm above already bound guard.freshly_created;
        // CR round 4 finding #3 made this safe against engine_arc-vanished.)

        // Recover the engine handle for the caller. The inner helper
        // doesn't return it directly to preserve the existing return-type
        // shape on the inner; we look it up from the registry.
        //
        // The lookup re-acquires the engines lock asynchronously. In the
        // freshly_created path, the engine was just inserted under the
        // same lock — the only way the lookup would miss is if a
        // concurrent caller invoked `shutdown_engine_and_cleanup_persistence`
        // (only called by rollback paths) between insert and lookup. The
        // `ok_or_else` below handles this rare case with a clear error.
        // In the idempotent path, the existing engine is what we return.
        let engine = self.engine_arc(&community_id).await.ok_or_else(|| {
            CommunitySyncError::Persist(format!(
                "engine vanished immediately after spawn_engine_inner_now \
                 (community_id = {community_id:?}) — registry race or programming error"
            ))
        })?;
        Ok(engine)
    }

    /// Register a oneshot to fire when a `JoinCountersign` whose
    /// `target_event_id == event_id` is Inserted into any engine in this
    /// registry — i.e. when the PendingJoin identified by `event_id` is
    /// counter-signed by an admin.
    ///
    /// ZEB-501: previously this fired on the Insert of the event whose own
    /// id == `event_id`, which the joiner's own PendingJoin self-insert
    /// satisfied synchronously — so the redeem never actually waited for the
    /// counter-sign and always reported `pending == false`. The insert hook
    /// now notifies only on `JoinCountersign.target_event_id`.
    ///
    /// Replaces any existing oneshot for the same `event_id` (the prior
    /// sender is dropped, which the prior caller's `.await` on the receiver
    /// surfaces as `Err(RecvError)` — interpret as "redemption superseded").
    /// v1 doesn't deduplicate registrations because the caller pattern (one
    /// `redeem_invite` IPC = one fresh `event_id`) keeps the map naturally
    /// sparse.
    pub async fn register_pending_redemption(
        &self,
        event_id: crate::community_membership::EventId,
        sender: tokio::sync::oneshot::Sender<()>,
    ) {
        let mut g = self.pending_redemptions.lock().await;
        g.insert(event_id, sender);
        // guard dropped at end of scope
    }

    /// Remove the oneshot for `event_id` without firing it. Called by
    /// the IPC's timeout path so a late-arriving counter-signed Join
    /// doesn't try to send to a dead receiver.
    pub async fn take_pending_redemption(
        &self,
        event_id: &crate::community_membership::EventId,
    ) -> Option<tokio::sync::oneshot::Sender<()>> {
        let mut g = self.pending_redemptions.lock().await;
        g.remove(event_id)
    }

    /// If a oneshot is registered for `event_id`, take it out of the
    /// map and fire it. No-op if no registration exists. The engine's
    /// insert hooks call this directly via the shared `Arc` (see
    /// `notify_pending_redemption_in_map`); this method exists for
    /// IPC-side / direct callers (and is the symmetric companion to
    /// `register_pending_redemption` / `take_pending_redemption`).
    ///
    /// **Lock-discipline:** the map lock is held only across the
    /// `remove` call. The `send(())` on `oneshot::Sender` is sync, so
    /// no `.await` happens with the guard alive.
    pub async fn notify_pending_redemption(&self, event_id: &crate::community_membership::EventId) {
        notify_pending_redemption_in_map(&self.pending_redemptions, event_id).await;
    }

    /// ZEB-258 rollback primitive: stop the engine task for
    /// `community_id` (drops adapter + Zenoh subscriber), then remove
    /// its per-community persistence directory.
    ///
    /// **Idempotent:** unknown `community_id` returns `Ok(())` — both
    /// `stop_engine` (no-op on missing) and the `if dir.exists()` guard
    /// flow through cleanly. Used by `create_community_inner` /
    /// `redeem_invite` rollback paths so the per-community
    /// `crdt.cbor` + `replay.cbor` don't accumulate on the disk after
    /// a partially-failed creation.
    ///
    /// **Caller responsibility:** ensure no other thread holds an
    /// `Arc<CommunitySyncEngine>` from this registry. Typical use is
    /// "I just spawned this; no one else has a handle yet." If a
    /// handle has leaked elsewhere, those holders see `TransportClosed`
    /// once teardown completes.
    /// ZEB-436: rollback for a freshly-spawned engine. Always stops the
    /// engine; removes the persistence dir ONLY when the spawn itself
    /// created it (`preserve_persistence == false`). A fresh ENGINE over
    /// PRE-EXISTING data — orphan re-adoption (ZEB-427), rejoin-after-
    /// leave — must roll back to exactly the pre-spawn disk state: the
    /// dir holds the user's entire community history, which is precisely
    /// what the repair attempt exists to recover.
    pub(crate) async fn rollback_fresh_spawn(
        &self,
        community_id: &SpaceId,
        preserve_persistence: bool,
    ) -> Result<(), CommunitySyncError> {
        if preserve_persistence {
            self.stop_engine(community_id).await
        } else {
            // No NodeState generation to guard here — this rolls back a
            // freshly-spawned engine synchronously (ZEB-732 gen_check is a
            // no-op for the spawn-rollback path).
            self.shutdown_engine_and_cleanup_persistence(community_id, || Ok(()))
                .await
        }
    }

    /// `gen_check` (ZEB-732): re-validates that the destructive delete is
    /// still safe, invoked AFTER `stop_engine().await` and IMMEDIATELY before
    /// `remove_dir_all`. Callers that snapshot a `NodeState.generation` pass a
    /// closure comparing the live generation against their snapshot; a mismatch
    /// aborts the delete (a concurrent stop/start installed a fresh live
    /// community whose dir this would otherwise destroy). Callers with no
    /// generation to guard (e.g. `rollback_fresh_spawn`, tests) pass
    /// `|| Ok(())`. The closure runs synchronously and is the last checkpoint
    /// before the delete — everything from here to `remove_dir_all` is
    /// non-awaiting, so no task can bump generation after it passes.
    pub async fn shutdown_engine_and_cleanup_persistence(
        &self,
        community_id: &SpaceId,
        gen_check: impl FnOnce() -> Result<(), String> + Send,
    ) -> Result<(), CommunitySyncError> {
        // Phase 1: stop the engine task (idempotent on missing).
        self.stop_engine(community_id).await?;

        // ZEB-732: re-check the node generation AFTER stop_engine's await
        // (its `e.shutdown().await` is the TOCTOU window) and BEFORE the
        // destructive delete below. Abort — do NOT delete — on a mismatch.
        gen_check().map_err(CommunitySyncError::CleanupAborted)?;

        // Phase 2: remove the per-community persistence directory via
        // DETACH-THEN-DELETE (Qodo #1 / CodeRabbit).
        //
        // The recursive delete is async (`tokio::fs::remove_dir_all(..).await`),
        // so a concurrent `stop_node`→`start_node` could recreate
        // `communities/<id>` DURING that await and have the fresh dir destroyed
        // — the generation re-check above is a necessary fence but not a
        // sufficient one across the yield. Close it by synchronously renaming
        // the dir OUT of the canonical path (an atomic metadata op, with no
        // `.await` between the gen_check and the rename) before the awaited
        // delete: any `communities/<id>` a racing start_node then creates is an
        // INDEPENDENT directory this delete never targets. We remove the
        // detached snapshot instead. The recursive unlink stays async (no
        // worker parked on `std::fs::remove_dir_all`).
        let communities = self.cfg.identity_dir.join("communities");
        let dir = communities.join(hex::encode(community_id.0));
        if dir.exists() {
            let detached = communities.join(format!(
                "{}.deleting.{}",
                hex::encode(community_id.0),
                unique_detach_suffix()
            ));
            std::fs::rename(&dir, &detached)
                .map_err(|e| CommunitySyncError::Persist(format!("detach {dir:?}: {e}")))?;
            tokio::fs::remove_dir_all(&detached).await.map_err(|e| {
                CommunitySyncError::Persist(format!("remove_dir_all {detached:?}: {e}"))
            })?;
        }
        Ok(())
    }

    /// Derive per-community persist paths under
    /// `identity_dir/communities/{id_hex}/{crdt|replay}.cbor`.
    ///
    /// `id_hex` is lowercase, zero-padded, 32 chars (one byte per pair)
    /// for the 16-byte `SpaceId`. Stable across boots — the registry
    /// owns the layout convention so multiple `CommunitySyncEngine`
    /// instances writing concurrently can never collide on the same
    /// directory.
    fn paths_for(&self, community_id: SpaceId) -> PersistPaths {
        // hex::encode is the codebase convention for OwnerAddr/SpaceId/
        // device_id rendering — see event_loop.rs, dm_outbox.rs, etc.
        let id_hex = hex::encode(community_id.0);
        let dir = self.cfg.identity_dir.join("communities").join(&id_hex);
        PersistPaths {
            crdt: dir.join("crdt.cbor"),
            replay: dir.join("replay.cbor"),
        }
    }

    /// Spawn a new `CommunitySyncEngine` for `community_id` and insert
    /// it into the registry's map. Loads any prior `crdt.cbor` +
    /// `replay.cbor` from disk first so the engine starts from the
    /// last persisted snapshot rather than empty state.
    ///
    /// **Idempotency:** re-spawning an already-known community is a
    /// no-op (returns `Ok(())`), NOT an error. This tolerates duplicate
    /// add events from owner-state mutations — Phase 3+'s subscription
    /// scan can fire the same `Membership::Joined` delta twice without
    /// the registry double-spawning or surfacing a spurious failure.
    ///
    /// **Lock scope:** disk I/O (`load_crdt` + `load_replay`) runs
    /// BEFORE acquiring the engines map lock to avoid parking a tokio
    /// worker thread behind synchronous `std::fs::read` calls. The
    /// idempotency check then re-runs under the lock so two concurrent
    /// spawns for the same community still resolve to a single engine
    /// (the second one's pre-loaded state is discarded — cheap, since
    /// the file is what survives anyway). The `CommunitySyncEngine::new`
    /// call (which itself does a `tokio::spawn` for the internal task)
    /// stays under the lock so the insert + spawn pair is atomic vs
    /// other spawn races.
    /// Returns `Ok(true)` when this call freshly created the engine,
    /// `Ok(false)` when an engine for `community_id` was already present
    /// (the no-op idempotent path). Callers that need the atomic
    /// "did I create or did I find existing?" signal — particularly
    /// `redeem_invite_inner`'s rollback guards (ZEB-260 PR #90 round-5,
    /// CodeRabbit) — MUST use this return value rather than a separate
    /// `engine_arc(...).is_some()` pre-check, which is racy under
    /// concurrent redeems. The `bool` is set under the same engines-map
    /// lock that performs the `contains_key` check + insert, so the
    /// flag and the engine state are mutually consistent.
    ///
    /// **ZEB-274**: this is the inner helper. Public IPC callers should
    /// use `spawn_engine_with_guard` to get atomic spawn, adapter
    /// dispatch, and rollback guard. Boot-time `start_node` reconcile
    /// (lib.rs:1747) is allowed to call this directly because it has no
    /// rollback obligation (boot reconcile recovers state, doesn't
    /// introduce new state). Integration tests under `src-tauri/tests/`
    /// also call this directly because they don't exercise the
    /// IPC-handler RAII surface; the method stays `pub` (not
    /// `pub(crate)`) so those tests compile against the public API.
    ///
    /// **ZEB-434** (ZEB-438: now bundled into `catch_up`, destructured
    /// below): `root_serve_rx` is the engine half of the
    /// queryable-serve channel (threaded into the engine config);
    /// `fetch_request_tx` is the driver half of the fetch-request
    /// channel — when `Some`, a `run_root_fetch_driver` task is spawned
    /// after successful insertion (with `transport_epoch_rx` as its
    /// re-arm watch; `None` in legacy/test callers or the restart-race
    /// window, where the generation fence prevents the spawn from
    /// completing anyway).
    /// Legacy/test callers pass [`CatchUpChannels::none()`].
    // ZEB-438: bundling the catch-up trio into `CatchUpChannels` drops this
    // from 9 to 7 explicit params, but a method still counts `self`, so 8
    // exceeds clippy's threshold of 7 — the allow stays (same rationale as
    // `spawn_engine_with_guard`). The win here is a self-documenting bundle
    // and a single update site for the catch-up channels, not lint removal.
    #[allow(clippy::too_many_arguments)]
    pub async fn spawn_engine_inner_now(
        &self,
        community_id: SpaceId,
        membership_key: EpochKey,
        admin_addr: OwnerAddr,
        is_invite_only: bool,
        publisher_tx: mpsc::Sender<Vec<u8>>,
        subscriber_rx: mpsc::Receiver<Vec<u8>>,
        catch_up: CatchUpChannels,
    ) -> Result<bool, CommunitySyncError> {
        // ZEB-438: unbundle the engine-side catch-up channels (previously
        // three loose positional `Option`s on this signature).
        let CatchUpChannels {
            root_serve_rx,
            fetch_request_tx,
            transport_epoch_rx,
        } = catch_up;

        // Phase 1: blocking disk I/O off the runtime entirely. Both
        // load_crdt and load_replay call std::fs::read, so even with
        // the registry mutex released they'd block the tokio worker
        // (one worker per spawn_engine call, multiplied across the
        // boot-time scan of every joined community). spawn_blocking
        // offloads to the dedicated blocking pool — mirrors
        // owner_state_sync's persist path (line 376). Doing this
        // outside the engines lock is also harmless on a re-spawn
        // race: the second caller's loaded state is dropped at the
        // idempotency check below.
        let paths = self.paths_for(community_id);
        let paths_for_io = paths.clone();
        let (initial_state, initial_tracker) = tokio::task::spawn_blocking(
            move || -> Result<(CommunityState, CommunityRootHlcTracker), CommunitySyncError> {
                let state = load_crdt(&paths_for_io.crdt, community_id)
                    .map_err(|e| CommunitySyncError::Persist(e.to_string()))?;
                let tracker = load_replay(&paths_for_io.replay)
                    .map_err(|e| CommunitySyncError::Persist(e.to_string()))?;
                Ok((state, tracker))
            },
        )
        .await
        .map_err(|join_err| {
            CommunitySyncError::Persist(format!("spawn_blocking join failed: {join_err}"))
        })??;

        // Phase 2: take the engines lock, re-check idempotency, build
        // and insert the engine. Lock is held across CommunitySyncEngine::new
        // (which spawns the internal task) so a concurrent spawn for
        // the same community can't race past the contains_key check.
        let mut engines = self.engines.lock().await;
        if engines.contains_key(&community_id) {
            // Idempotent — re-spawn is a no-op rather than an error
            // so the registry tolerates duplicate add events from
            // owner-state mutations. ZEB-260 PR #90 round-5: returning
            // `false` here gives concurrent callers an atomic signal
            // distinguishing "I was the one who created the engine"
            // from "I found one already running."
            return Ok(false);
        }

        let state = Arc::new(Mutex::new(initial_state));
        let tracker = Arc::new(Mutex::new(initial_tracker));

        let engine = Arc::new(CommunitySyncEngine::new(CommunitySyncEngineConfig {
            community_id,
            membership_key,
            admin_addr,
            is_invite_only,
            device_id: self.cfg.device_id.clone(),
            // ZEB-256 Task 6: self_owner + signing_key are sourced
            // from the registry config so every engine in a registry
            // shares one local identity. Task 4's TEMP placeholder
            // (admin_addr + dummy [0x42; 32] key) is gone — the
            // membership-at-HLC + sig-verify gates require the real
            // values for verify-on-receive to admit our publishes.
            self_owner: self.cfg.self_owner,
            signing_key: Arc::clone(&self.cfg.signing_key),
            state,
            tracker,
            content_store: Arc::clone(&self.cfg.content_store),
            publisher_tx,
            subscriber_rx,
            paths,
            debounce_ms: self.cfg.debounce_ms,
            identity_resolver: Some(Arc::clone(&self.cfg.identity_resolver)),
            error_tx: self.cfg.error_tx.clone(),
            delta_tx: self.cfg.delta_tx.clone(),
            // ZEB-262 Phase 4: hand the engine a clone of the shared
            // pending-redemption map. The engine's post-Inserted hooks
            // (`insert_local_event`, `handle_incoming_publish`) fire
            // any matching oneshot. The registry retains its own
            // `Arc` for IPC-side `register_pending_redemption` /
            // `take_pending_redemption` / `notify_pending_redemption`.
            pending_redemptions: Some(std::sync::Arc::clone(&self.pending_redemptions)),
            // ZEB-249 §10.6 (Phase A): pass the live owner-state CRDT
            // so the engine can read current_epoch_key dynamically.
            crdt_state: self.crdt_state.as_ref().map(Arc::clone),
            // ZEB-339 / ZEB-496: the admin identity pub has no remaining
            // verify-path consumer; the engine ignores this field, so the
            // registry no longer pays a spawn-time resolver lookup to
            // populate it.
            admin_identity_pub: None,
            // ZEB-254 Task 11: clone the nav_emitter from the registry so
            // the engine can fire nav-updated on joiner-side countersign.
            // `None` for test registries that don't supply one.
            nav_emitter: self.nav_emitter.clone(),
            // ZEB-434 D1/D2: engine half of the queryable-serve
            // channel; the event_loop queryable task holds the
            // matching sender. `None` for legacy/test callers.
            root_serve_rx,
        }));

        engines.insert(community_id, engine);

        // ZEB-434 D3/D4: spawn the per-community root-fetch driver.
        // It paces state-root pull queries (spawn-time query, backoff
        // while unanswered, transport-epoch re-arm) and forwards each
        // attempt to the zenoh adapter via `fetch_request_tx`; replies
        // ingest through the engine's normal inbound path, so the
        // driver only ever sees reply counts.
        //
        // Lock order: `engines` may be held while taking
        // `root_fetch_shutdowns` (spawn path only); the reverse never
        // happens — stop_engine/shutdown_all take them strictly
        // sequentially. Inserting the shutdown sender under the
        // engines guard makes engine+driver-entry visibility atomic:
        // a concurrent stop_engine that removes the engine AFTER our
        // guard drops is guaranteed to find (and flip) this entry. If
        // it flips before the driver task below first polls, the
        // driver reads `borrow() == true` (watch retains the last
        // value across sender drop) and exits immediately.
        let driver_shutdown_rx = if let Some(fetch_tx) = fetch_request_tx {
            let (driver_shutdown_tx, driver_shutdown_rx) = tokio::sync::watch::channel(false);
            self.root_fetch_shutdowns
                .lock()
                .await
                .insert(community_id, driver_shutdown_tx);
            Some((fetch_tx, driver_shutdown_rx))
        } else {
            None
        };

        // Release the engines guard now that both inserts are done.
        // The driver spawn below intentionally runs outside the lock.
        drop(engines);

        if let Some((fetch_tx, driver_shutdown_rx)) = driver_shutdown_rx {
            let request_root = move || {
                let fetch_tx = fetch_tx.clone();
                async move {
                    let (report_tx, report_rx) = tokio::sync::oneshot::channel();
                    if fetch_tx
                        .send(crate::event_loop::CommunityRootFetchRequest { report: report_tx })
                        .await
                        .is_err()
                    {
                        // Adapter bridge closed for good.
                        return crate::channel_backfill::RootFetch::EngineGone;
                    }
                    match report_rx.await {
                        Ok(n) if n > 0 => crate::channel_backfill::RootFetch::Answered,
                        // Zero replies = no responder (a community-root
                        // responder always has a root to serve); aborted
                        // query (sender dropped) is transient — both
                        // back off and retry.
                        Ok(_) | Err(_) => crate::channel_backfill::RootFetch::NoReply,
                    }
                }
            };
            // ZEB-618: restart-aware anti-entropy floor for THIS community's
            // root fetch (ZEB-599 parity with the channel-log backfill
            // drivers). Sidecar lives in the community's own engine dir. The
            // interval is a SINGLE jittered draw shared between the persisted
            // first-deadline and the driver arg — so the two can never
            // diverge (mirrors the mail-root pair built by Task 5). Any
            // missing piece degrades to the legacy interval-from-spawn floor.
            let (root_interval_ms, root_persist) = {
                let dir = community_root_resync_dir(&self.cfg.identity_dir, &community_id);
                // Async create_dir_all (ZEB-467: no blocking fs on the spawn
                // path). The community dir is created lazily on first CRDT
                // persist, so ensure it exists now — otherwise the floor's
                // `save` callback (which does NOT create_dir_all) has nowhere
                // to write and forfeits restart-awareness every cycle.
                match tokio::fs::create_dir_all(&dir).await {
                    Ok(()) => {
                        let interval_ms = crate::channel_backfill::periodic_resync_interval_ms();
                        let now_ms = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0);
                        let last =
                            crate::community_channel_log::ChannelBackfillState::load_async(&dir)
                                .await
                                .map(|s| s.last_full_reconcile_ms);
                        let first_deadline_ms = crate::channel_backfill::first_resync_deadline(
                            last,
                            interval_ms,
                            now_ms,
                        );
                        let persist_dir = dir.clone();
                        (
                            interval_ms,
                            Some(crate::channel_backfill::ResyncPersist {
                                first_deadline_ms,
                                on_full_reconcile: std::sync::Arc::new(move |fired_at_ms| {
                                    let dir = persist_dir.clone();
                                    // Tiny sidecar write off the driver task
                                    // (same shape as the mail-root / channel-log
                                    // ZEB-599 callbacks).
                                    tokio::task::spawn_blocking(move || {
                                        if let Err(e) =
                                            crate::community_channel_log::ChannelBackfillState::save(
                                                &dir,
                                                fired_at_ms,
                                            )
                                        {
                                            tracing::debug!(error = %e, "community-root resync persist failed (hint only)");
                                        }
                                    });
                                }),
                            }),
                        )
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, "community-root resync dir create failed; legacy floor");
                        (crate::channel_backfill::periodic_resync_interval_ms(), None)
                    }
                }
            };
            tokio::spawn(crate::channel_backfill::run_root_fetch_driver(
                crate::channel_backfill::RootFetchLatch::new(),
                request_root,
                driver_shutdown_rx,
                transport_epoch_rx,
                // ZEB-618: presence kick — the registry-level receiver cloned
                // per spawned driver (same watch the channel-log drivers get).
                // `None` when the caller wired no presence source.
                self.cfg.presence_resync_rx.clone(),
                // ZEB-425: anti-entropy floor — re-arm the community root
                // fetch ~hourly (jittered per driver to avoid a startup
                // thundering herd) even with no epoch bump (router-only
                // gateways / late queryables / same-zid reconnects). ZEB-618:
                // the interval is the single draw shared with `root_persist`.
                Some(root_interval_ms),
                || {
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0)
                },
                // ZEB-618: restart-aware persisted floor (Some in production,
                // None when the sidecar dir couldn't be prepared).
                root_persist,
            ));
        }

        // ZEB-260 PR #90 round-5: `true` means "this call freshly
        // created the engine" — the atomic create flag for
        // redeem_invite_inner's rollback guards.
        Ok(true)
    }

    /// `true` if an engine is currently spawned for `community_id`.
    /// Snapshot read — the answer can change immediately after the
    /// caller drops the future, so callers should not assume it for
    /// invariant checks.
    pub async fn has_engine(&self, community_id: &SpaceId) -> bool {
        self.engines.lock().await.contains_key(community_id)
    }

    /// Remove `community_id`'s engine from the map and await its
    /// shutdown. Idempotent: if no engine is registered, returns
    /// `Ok(())` (mirrors `spawn_engine`'s no-op-on-already-present
    /// behavior — the registry treats stop-of-unknown the same way).
    pub async fn stop_engine(&self, community_id: &SpaceId) -> Result<(), CommunitySyncError> {
        let engine = {
            let mut engines = self.engines.lock().await;
            engines.remove(community_id)
        };
        // ZEB-434: stop the community's root-fetch driver (if one was
        // spawned). Strictly AFTER the engines guard dropped above —
        // the two locks are never held together. Flip before awaiting
        // the engine's shutdown so the driver stops issuing fetch
        // requests while the engine drains.
        if let Some(tx) = self.root_fetch_shutdowns.lock().await.remove(community_id) {
            let _ = tx.send(true);
        }
        match engine {
            Some(e) => e.shutdown().await,
            None => Ok(()),
        }
    }

    /// Drain every spawned engine, awaiting each one's shutdown in turn.
    /// Surfaces the LAST error encountered after attempting to shut down
    /// all engines — bailing on the first error would leak the engines
    /// after it, which is the worse failure mode (their tasks would
    /// outlive the registry and continue publishing). One log line per
    /// failure preserves enough detail for post-mortem.
    pub async fn shutdown_all(&self) -> Result<(), CommunitySyncError> {
        let engines: Vec<Arc<CommunitySyncEngine>> = {
            let mut e = self.engines.lock().await;
            std::mem::take(&mut *e).into_values().collect()
        };
        // ZEB-434: stop every root-fetch driver. Sequential with — never
        // nested inside — the engines lock above; flipped before the
        // engine shutdowns below so no driver issues fetch requests
        // while its engine drains.
        let shutdowns: Vec<tokio::sync::watch::Sender<bool>> = {
            let mut g = self.root_fetch_shutdowns.lock().await;
            std::mem::take(&mut *g).into_values().collect()
        };
        for tx in shutdowns {
            let _ = tx.send(true);
        }
        let mut last_err: Option<CommunitySyncError> = None;
        for e in engines {
            if let Err(err) = e.shutdown().await {
                tracing::warn!(error = %err, "engine shutdown failed during shutdown_all");
                last_err = Some(err);
            }
        }
        match last_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Snapshot of currently-spawned community IDs. Used by Task 12's
    /// owner-state subscription scan to compute add/remove deltas
    /// against the membership snapshot. `BTreeMap` iteration order is
    /// stable (sorted by `SpaceId`), so the returned `Vec` is
    /// deterministic — useful for logging and for any caller that
    /// wants to bisect by index.
    pub async fn known_ids(&self) -> Vec<SpaceId> {
        self.engines.lock().await.keys().cloned().collect()
    }

    /// Returns a clone of the engine's `CommunityState` Arc for a
    /// community, if an engine is spawned for it. **Test-only** —
    /// production callers go through Phase 3's IPC layer; this surface
    /// is gated as `#[doc(hidden)]` and exists so the integration test
    /// in `tests/community_sync_integration.rs` can inspect post-
    /// merge CRDT state without reaching into private fields.
    #[doc(hidden)]
    pub async fn state_for(&self, community_id: &SpaceId) -> Option<Arc<Mutex<CommunityState>>> {
        self.engines
            .lock()
            .await
            .get(community_id)
            .map(|e| e.state())
    }

    /// Returns a clone of the engine's `CommunityRootHlcTracker` for a
    /// community, if an engine is spawned for it. **Test-only** —
    /// gated as `#[doc(hidden)]` and exists so the ZEB-256 Task 8
    /// `spoofed_publish_does_not_block_real_publisher` integration
    /// test can inspect per-(addr, device_id) tracker entries without
    /// reaching into private engine fields. Returns the snapshot
    /// (cloned out from under the engine's mutex) rather than the live
    /// Arc so callers don't accidentally hold the lock across awaits.
    #[doc(hidden)]
    pub async fn tracker_snapshot(
        &self,
        community_id: &SpaceId,
    ) -> Option<CommunityRootHlcTracker> {
        let engine = self.engines.lock().await.get(community_id).cloned()?;
        let tracker = engine.tracker_arc();
        let snap = tracker.lock().await.clone();
        Some(snap)
    }

    /// Returns a clone of the `Arc<CommunitySyncEngine>` for
    /// `community_id`, if an engine is spawned. Used by Phase 3 IPC
    /// handlers (`create_community`, Phase 4 `redeem_invite`) that need
    /// to call `engine.insert_local_event(...)` after spawning the
    /// engine + dispatching the adapter request. Mirrors `state_for`'s
    /// shape but returns the engine handle rather than just the inner
    /// state — the engine surface is what fires `notify_dirty` and
    /// drives the debounced state-root publish.
    pub async fn engine_arc(&self, community_id: &SpaceId) -> Option<Arc<CommunitySyncEngine>> {
        self.engines.lock().await.get(community_id).cloned()
    }

    /// ZEB-714: snapshot of every spawned community's id. Used by the
    /// periodic recovery-liveness tick to run the self-heal observer
    /// over idle communities — a time-driven recovery execution (spec
    /// §4.1 now-floor) produces no CRDT delta, so without a tick the
    /// delta-driven observer would never see the Executed phase and
    /// never synthesize the finality-gated rotation.
    pub async fn spawned_community_ids(&self) -> Vec<SpaceId> {
        self.engines.lock().await.keys().copied().collect()
    }

    /// ZEB-249 Task 6: returns a clone of the registry's shared
    /// `IdentityResolver`. IPC handlers that need to look up members'
    /// 64-byte identity pubs (e.g., to derive X25519 pubkeys for
    /// EpochRotation/EpochCatchup `seal_to_owner` calls) can call this
    /// without reaching into the engine internals.
    pub fn identity_resolver(&self) -> Arc<dyn IdentityResolver> {
        Arc::clone(&self.cfg.identity_resolver)
    }

    /// Force the engine for `community_id` to publish its current CRDT
    /// state immediately, bypassing the debounce window. Returns
    /// `Err(CommunitySyncError::TransportClosed)` if no engine is
    /// registered for the community (callers can treat this as a
    /// no-op-on-unknown by ignoring that variant). **Test-only** —
    /// Phase 3 IPC will ship the public flush surface; this accessor
    /// is gated as `#[doc(hidden)]` to keep the integration test
    /// contract minimal until then.
    #[doc(hidden)]
    pub async fn flush_now(&self, community_id: &SpaceId) -> Result<(), CommunitySyncError> {
        // Clone the Arc<Engine> out from under the map lock so we don't
        // hold the registry mutex through the engine's flush_now (which
        // awaits a oneshot reply from the engine task). Holding the
        // outer lock across that wait would serialise every other
        // registry operation behind one engine's publish window.
        let engine = {
            let engines = self.engines.lock().await;
            engines.get(community_id).cloned()
        };
        match engine {
            Some(e) => e.flush_now().await,
            None => Err(CommunitySyncError::TransportClosed),
        }
    }

    /// ZEB-462 B: durably fence the community membership CRDT for
    /// `community_id` to disk NOW, publish-independent, if an engine is
    /// spawned. Used by the redeem path's join-commit durable fence (mirrors
    /// `fence_owner_state_flush` for owner-state). A missing engine returns
    /// `Ok(())` — the caller treats it as a best-effort fence (the next boot's
    /// debounce/scan recovers), NOT a hard join failure.
    pub async fn persist_now(&self, community_id: &SpaceId) -> Result<(), CommunitySyncError> {
        // Clone the Arc<Engine> out from under the map lock (same rationale as
        // `flush_now`): don't hold the registry mutex across the engine's
        // oneshot reply wait.
        let engine = {
            let engines = self.engines.lock().await;
            engines.get(community_id).cloned()
        };
        match engine {
            Some(e) => e.persist_now().await,
            None => Ok(()),
        }
    }
}

/// Identity resolver backed by Sub-A's owner-device cache. The cache
/// maps OwnerAddr → DeviceIdentityHash → identity_pub bytes via
/// RegisterDevice events; this resolver picks the FIRST recorded
/// identity_pub for the queried owner.
///
/// Semantic note on OwnerAddr ↔ DeviceIdentityHash: community_membership's
/// `event.actor: OwnerAddr` carries the SAME 16 bytes as a
/// `DeviceIdentityHash` — both are `SHA256(X25519_pub || Ed25519_pub)[:16]`
/// of the signing identity. The Phase 1 `verify_signature` enforces this
/// via `Identity::from_public_bytes(actor_identity_pub).address_hash ==
/// event.actor.0`, so the resolver must look up identity_pub by treating
/// `event.actor` as a device-hash key.
///
/// The cache stores one `OwnerDeviceEntry` per OWNER (master OwnerAddr),
/// each entry carrying a parallel-vec `(devices: Vec<DeviceIdentityHash>,
/// device_identity_pubs: Vec<Option<[u8; 64]>>)`. To resolve an
/// event-actor → identity_pub, we must iterate ALL owner entries and
/// binary-search each entry's `devices` vec for the target hash. The
/// existing `crate::dm_outbox::lookup_pubkey_for_device` helper
/// (`dm_outbox.rs:1575`) does exactly this — `OwnerDeviceCacheResolver`
/// is a thin wrapper around it that adapts the OwnerAddr ↔
/// DeviceIdentityHash newtype boundary.
pub struct OwnerDeviceCacheResolver {
    cache: Arc<Mutex<crate::owner_state_crdt::OwnerState>>,
    /// Self's owner address — when an incoming `resolve` call asks for
    /// our own owner, return `self_identity_pub` directly without
    /// hitting the cache. The cache lookup treats `OwnerAddr.0` as a
    /// `DeviceIdentityHash`, which works for peers (the peer's device
    /// is keyed by its address-as-hash in the cache layout) but fails
    /// for self because our `address_hash != our local signing device
    /// hash` in general — so own-authored events would otherwise
    /// resolve to `None` and fail local verification as an unresolved
    /// actor. CodeRabbit MAJOR finding on PR #87 round 2 (and the
    /// "Known production-path concern" callout from the PR body).
    self_owner: OwnerAddr,
    /// Self's 64-byte identity public bytes — what `insert_local_event`
    /// needs to verify own-authored events. Same value the local
    /// `PrivateIdentity` would surface via `to_public_bytes()`; in
    /// production this is `NodeState.dm_identity_pub_64`.
    self_identity_pub: [u8; 64],
}

impl OwnerDeviceCacheResolver {
    pub fn new(
        cache: Arc<Mutex<crate::owner_state_crdt::OwnerState>>,
        self_owner: OwnerAddr,
        self_identity_pub: [u8; 64],
    ) -> Self {
        Self {
            cache,
            self_owner,
            self_identity_pub,
        }
    }
}

#[async_trait::async_trait]
impl IdentityResolver for OwnerDeviceCacheResolver {
    async fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
        // Self short-circuit: own-authored events use the local
        // identity_pub directly. We know who we are without consulting
        // the cache — and the cache lookup wouldn't find us anyway
        // (see struct doc).
        if *addr == self.self_owner {
            return Some(self.self_identity_pub);
        }
        use crate::dm_outbox::lookup_pubkey_for_device;
        use crate::owner_state_types::DeviceIdentityHash;
        // Async trait fn — the resolver waits on the real Mutex rather
        // than collapsing contention to None. The lookup itself is
        // O(devices) per owner-entry, so the lock is short-held; the
        // owner-state critical sections that run concurrently
        // (SyncEngine snapshot+publish, dm_outbox drain, Phase 3 IPC
        // handlers) interleave normally.
        let cache = self.cache.lock().await;
        // OwnerAddr and DeviceIdentityHash are bytes-compatible newtypes
        // (both wrap [u8; 16]). Reinterpret without copying.
        let device_hash = DeviceIdentityHash(addr.0);
        lookup_pubkey_for_device(&cache.owner_device_cache, device_hash)
    }
}

/// ZEB-298+ZEB-312 PR 2 Task 1: production wiring for the voting engine's
/// identity resolver. The voting engine's `VotingIdentityResolver` trait
/// has the same `OwnerAddr → Option<[u8; 64]>` shape as channel-log's
/// `IdentityResolver`, so this impl delegates to the existing lookup —
/// no separate cache or different semantics. The 64-byte composite is
/// `X25519_pub || Ed25519_pub`, which `verify_voting_event` feeds to
/// `harmony_identity::Identity::from_public_bytes` for signature
/// verification + actor-address-binding check.
#[async_trait::async_trait]
impl crate::community_voting_core::VotingIdentityResolver for OwnerDeviceCacheResolver {
    async fn resolve(&self, owner: &OwnerAddr) -> Option<[u8; 64]> {
        <Self as IdentityResolver>::resolve(self, owner).await
    }
}

// ── ZEB-270 Phase 3 Task 4.5: production adapters for the channel-log
//    verify chain ────────────────────────────────────────────────────────

/// Wraps the live per-community `Arc<Mutex<CommunityState>>` to
/// satisfy the channel-log `CommunityStateAtHlc` trait. Materialises
/// the CRDT to "now" and projects the requested HLC slice via the
/// existing `prior_state_at_hlc` helper.
///
/// Construction is cheap (two Arc bumps + a copy of `admin_addr`); the
/// expensive work happens only on a verify-chain call from the
/// channel-log engine's receive loop.
///
/// **Lock posture.** `state.lock().await` runs against the same
/// `tokio::sync::Mutex` the per-community sync engine uses for its
/// CRDT writes. Holding the lock briefly across the `materialize`
/// projection is acceptable — the engine's publish path also holds
/// the lock across `materialized()` calls (a similar O(events) walk),
/// and the verify chain runs on the channel-log receive task, NOT on
/// the engine's task, so they never block each other.
pub struct CommunityStateAtHlcAdapter {
    pub state: Arc<Mutex<crate::community_state_crdt::CommunityState>>,
    pub admin_addr: OwnerAddr,
}

#[async_trait::async_trait]
impl crate::community_channel_log::CommunityStateAtHlc for CommunityStateAtHlcAdapter {
    async fn snapshot_at(
        &self,
        channel_id: &crate::community_membership::ChannelId,
        author: &crate::owner_state_types::OwnerAddr,
        at: &crate::owner_state_types::Hlc,
    ) -> crate::community_channel_log::CommunityStateSnapshot {
        // Snapshot the event log under the lock, then drop the guard
        // before materialising. `prior_state_at_hlc` is a pure function
        // of (events, target_hlc, admin_addr) — it doesn't touch the
        // shared CommunityState, so we can release the lock first to
        // avoid blocking the engine's writer for the duration of the
        // O(events) replay.
        //
        // Critical: ONE lock acquisition + ONE materialization for both
        // the channel-config lookup and the author-power lookup. The
        // previous shape (two trait methods, two lock acquisitions, two
        // materializations) admitted a torn read where a CRDT update
        // landing between the two awaits could let verify_channel_event
        // admit a post on a state that never coexisted at one HLC.
        let state = self.state.lock().await;
        let events: Vec<crate::community_membership::SignedMembershipEvent> =
            state.events.values().cloned().collect();
        drop(state);
        let materialized =
            crate::community_membership::prior_state_at_hlc(&events, at, self.admin_addr);

        let channel = materialized.channels.get(channel_id).cloned();
        // Member must be Joined at `at` (Left/Banned/Invited drop the
        // post). `power_levels` defaults to 0 for unset entries; for
        // Joined non-listed members the lookup is `Some(0)`. For
        // never-Joined / Left / Banned we return `None` so verify
        // surfaces NotAuthorized rather than a misleading 0.
        let author_power =
            materialized
                .members
                .get(author)
                .and_then(|member| match member.status {
                    crate::community_membership::MemberStatus::Joined => {
                        Some(materialized.power_levels.get(author).copied().unwrap_or(0))
                    }
                    _ => None,
                });

        // ZEB-399: surface the author's enrolled device keys from the SAME
        // materialization, so `verify_channel_event` can authenticate the
        // post signature against the community membership trust root (the
        // same `enrolled_device_keys` that `verify_publisher_sig` uses for
        // root publishes) instead of a DM-layer owner→identity cache.
        let author_enrolled_keys = materialized
            .members
            .get(author)
            .map(|member| member.enrolled_device_keys.iter().copied().collect())
            .unwrap_or_default();

        crate::community_channel_log::CommunityStateSnapshot {
            channel,
            author_power,
            author_enrolled_keys,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content_store::{ContentStore, RuntimeContentStore};

    /// NopResolver: minimal `IdentityResolver` for fixture builds. None
    /// of the spawn-rollback-guard tests exercise verify-on-receive, so
    /// the resolver never resolves anything.
    struct NopResolver;

    #[async_trait::async_trait]
    impl IdentityResolver for NopResolver {
        async fn resolve(&self, _: &OwnerAddr) -> Option<[u8; 64]> {
            None
        }
    }

    /// ZEB-618: the per-community resync sidecar path derives from
    /// `identity_dir` exactly like `paths_for`/PersistPaths does.
    #[test]
    fn community_root_resync_dir_matches_engine_layout() {
        let sid = SpaceId([0x77; 16]);
        let dir = community_root_resync_dir(std::path::Path::new("/tmp/idroot"), &sid);
        assert_eq!(
            dir,
            std::path::PathBuf::from(format!("/tmp/idroot/communities/{}", hex::encode(sid.0)))
        );
    }

    // ---- ZEB-339 Task 9: publisher-auth via materialized enrolled keys ----

    /// Build a signed `CommunityRootPublishPayload` for `publisher_addr`,
    /// signed with `device_key` (the publisher's enrolled device key).
    fn build_signed_publish(
        publisher_addr: OwnerAddr,
        device_key: &ed25519_dalek::SigningKey,
    ) -> CommunityRootPublishPayload {
        use crate::owner_state_crypto::canonical_cbor_encode;
        use ed25519_dalek::Signer as _;
        let root_cid = harmony_content::cid::ContentId::for_book(
            b"zeb-339-publisher-auth-test-blob",
            harmony_content::cid::ContentFlags {
                encrypted: true,
                ..Default::default()
            },
        )
        .expect("cid");
        let signed = CommunityRootSignedPayload {
            root_cid,
            publisher_addr,
            at: crate::owner_state_types::Hlc {
                wall_ms: 1_700_000_000_000,
                logical: 0,
                device_id: "dev-1".to_string(),
            },
        };
        let signed_bytes = canonical_cbor_encode(&signed).expect("encode");
        let sig = device_key.sign(&signed_bytes).to_bytes();
        signed.into_wire(sig, None)
    }

    /// A `MemberState` for `owner` whose `enrolled_device_keys` carries
    /// exactly the keys in `keys`.
    fn joined_member_with_keys(
        keys: std::collections::BTreeSet<[u8; 32]>,
    ) -> crate::community_membership::MemberState {
        crate::community_membership::MemberState {
            status: crate::community_membership::MemberStatus::Joined,
            joined_at: crate::owner_state_types::Hlc {
                wall_ms: 1_699_000_000_000,
                logical: 0,
                device_id: "dev-1".to_string(),
            },
            left_at: None,
            enrolled_device_keys: keys,
            revoked_device_keys: std::collections::BTreeSet::new(),
        }
    }

    #[test]
    fn verify_publisher_sig_accepts_enrolled_device_key() {
        let owner = crate::community_membership::mint_test_owner(0x11);
        let payload = build_signed_publish(owner.owner, &owner.device_key);
        let member =
            joined_member_with_keys(crate::community_membership::test_enrolled_keys(&owner));

        assert!(
            verify_publisher_sig(&payload, &member).is_ok(),
            "publish signed with the member's enrolled device key must verify"
        );
    }

    #[test]
    fn verify_publisher_sig_rejects_non_enrolled_key() {
        let owner = crate::community_membership::mint_test_owner(0x12);
        // Sign with a DIFFERENT key that is NOT in the enrolled set.
        let rogue_key = ed25519_dalek::SigningKey::from_bytes(&[0x77; 32]);
        let payload = build_signed_publish(owner.owner, &rogue_key);
        // Member's materialized enrolled set holds only the legit device key.
        let member =
            joined_member_with_keys(crate::community_membership::test_enrolled_keys(&owner));

        match verify_publisher_sig(&payload, &member) {
            Err(CommunitySyncError::PublisherSigInvalid { addr }) => {
                assert_eq!(addr, owner.owner);
            }
            other => panic!("expected PublisherSigInvalid, got {other:?}"),
        }
    }

    #[test]
    fn verify_publisher_sig_rejects_empty_enrolled_set() {
        let owner = crate::community_membership::mint_test_owner(0x13);
        // Even a correctly-signed publish must be rejected if the member
        // has no enrolled device keys materialized.
        let payload = build_signed_publish(owner.owner, &owner.device_key);
        let member = joined_member_with_keys(std::collections::BTreeSet::new());

        match verify_publisher_sig(&payload, &member) {
            Err(CommunitySyncError::PublisherSigInvalid { addr }) => {
                assert_eq!(addr, owner.owner);
            }
            other => panic!("expected PublisherSigInvalid for empty set, got {other:?}"),
        }
    }

    #[test]
    fn verify_publisher_sig_rejects_tampered_payload() {
        let owner = crate::community_membership::mint_test_owner(0x14);
        let mut payload = build_signed_publish(owner.owner, &owner.device_key);
        // Tamper with the signed-over HLC after signing: the recomputed
        // canonical bytes no longer match the signature.
        payload.at.wall_ms += 1;
        let member =
            joined_member_with_keys(crate::community_membership::test_enrolled_keys(&owner));

        assert!(
            matches!(
                verify_publisher_sig(&payload, &member),
                Err(CommunitySyncError::PublisherSigInvalid { .. })
            ),
            "a publish whose signed bytes were tampered must not verify"
        );
    }

    /// Test fixture for ZEB-274 spawn-rollback-guard tests. Owns the
    /// registry under `Arc` (matches production shape — guards hold
    /// `Arc<CommunitySyncRegistry>`), the per-fixture tempdir, the
    /// arguments needed for `spawn_engine_with_guard` calls, and the
    /// `community_adapter_tx` half of the adapter-request bridge. The
    /// tests do not consume the dispatched adapter requests, so the
    /// fixture also owns the receiver to keep the channel alive
    /// (drop-on-fixture-drop is fine — the registry's Sender clones go
    /// with the fixture).
    struct GuardTestFixture {
        registry: std::sync::Arc<CommunitySyncRegistry>,
        identity_dir: std::path::PathBuf,
        membership_key: EpochKey,
        admin_addr: OwnerAddr,
        community_adapter_tx: mpsc::Sender<crate::event_loop::CommunityAdapterRequest>,
        // Held to keep the adapter-request channel alive (and the
        // tempdir) for the lifetime of the test. Suppress unused-field
        // warnings: these are owned for their drop side effects only.
        #[allow(dead_code)]
        community_adapter_rx: mpsc::Receiver<crate::event_loop::CommunityAdapterRequest>,
        #[allow(dead_code)]
        tempdir: tempfile::TempDir,
    }

    /// Build a fresh `GuardTestFixture` rooted at a tempdir. Each test
    /// gets its own tempdir + registry; tests do not share fixtures.
    async fn build_test_fixture() -> GuardTestFixture {
        let (cas_op_tx, _cas_op_rx) = mpsc::channel(8);
        let cs: std::sync::Arc<dyn ContentStore> = std::sync::Arc::new(RuntimeContentStore::new(
            cas_op_tx,
            std::time::Duration::from_millis(1000),
        ));

        let tempdir = tempfile::tempdir().expect("tempdir");
        let identity_dir = tempdir.path().to_path_buf();

        let registry = std::sync::Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
            device_id: "dev".into(),
            content_store: cs,
            identity_resolver: std::sync::Arc::new(NopResolver),
            identity_dir: identity_dir.clone(),
            debounce_ms: DEFAULT_DEBOUNCE_MS,
            error_tx: None,
            delta_tx: None,
            self_owner: OwnerAddr([0x01; 16]),
            signing_key: std::sync::Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0x42; 32])),
            crdt_state: None,
            nav_emitter: None,
            presence_resync_rx: None,
        }));

        // Adapter-request bridge: `spawn_engine_with_guard` will
        // try_send into this. Buffer of 64 is plenty for the per-test
        // single-spawn case. The receiver is held by the fixture to
        // keep the channel alive.
        let (community_adapter_tx, community_adapter_rx) = mpsc::channel(64);

        GuardTestFixture {
            registry,
            identity_dir,
            membership_key: EpochKey::new([0xa1; 32]),
            admin_addr: OwnerAddr([0xb1; 16]),
            community_adapter_tx,
            community_adapter_rx,
            tempdir,
        }
    }

    /// ZEB-434: adapter-half stand-ins for guard tests. The matching
    /// engine/driver halves are dropped immediately — these fixtures
    /// never spawn the zenoh adapter, and the spawn itself gets
    /// [`CatchUpChannels::none()`] for the engine-side catch-up params.
    fn dummy_root_serve_tx() -> mpsc::Sender<RootServeRequest> {
        mpsc::channel(8).0
    }

    /// See [`dummy_root_serve_tx`].
    fn dummy_fetch_request_rx() -> mpsc::Receiver<crate::event_loop::CommunityRootFetchRequest> {
        mpsc::channel(4).1
    }

    // ── ZEB-274 spawn-rollback-guard tests ─────────────────────────

    /// Spec §7.1 #1: spawn engine, commit guard, verify engine present
    /// + persistence dir present after guard drops.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guard_commit_releases_rollback() {
        let fix = build_test_fixture().await;
        let community_id = SpaceId([0xc1; 16]);

        let (pub_tx, pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let (sub_tx, sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

        let mut guard = std::sync::Arc::clone(&fix.registry).begin_spawn_guard(community_id);
        let engine = std::sync::Arc::clone(&fix.registry)
            .spawn_engine_with_guard(
                &mut guard,
                community_id,
                fix.membership_key.clone(),
                fix.admin_addr,
                false,
                pub_tx,
                sub_rx,
                pub_rx,
                sub_tx,
                fix.community_adapter_tx.clone(),
                CatchUpChannels::none(),
                dummy_root_serve_tx(),
                dummy_fetch_request_rx(),
            )
            .await
            .expect("spawn_engine_with_guard");

        guard.commit();
        // engine handle still valid
        drop(engine);

        // Engine must still be in the registry (commit released the
        // rollback obligation). The persistence-dir-still-exists
        // assertion the spec mentions is dropped here because a fresh
        // engine with no inserted events / no flush never writes the
        // dir on disk, and the test fixture intentionally doesn't
        // connect a CAS event-loop to drive a publish (`flush_now`
        // returns ContentStore::Io). The engine-presence check fully
        // captures "commit did not run cleanup" — Test 2 verifies the
        // negative direction (engine absent after rollback).
        assert!(
            fix.registry.has_engine(&community_id).await,
            "engine must remain after commit"
        );
    }

    /// ZEB-463 + ZEB-633 regression: a graceful-shutdown final flush that
    /// fails because a concurrent rollback removed the community's
    /// persistence directory must be treated as a benign no-op (the data is
    /// being discarded on purpose), while EVERY other failure still
    /// propagates.
    ///
    /// The race itself (a `remove_dir_all` interleaving inside
    /// `write_atomic`, which `create_dir_all`s its parent first) is a true
    /// TOCTOU and not deterministically reproducible, so we test the exact
    /// decision predicate the shutdown arm applies: (1) the causal
    /// `PersistDirMissing` variant (Qodo PR #267) downgrades unconditionally;
    /// (2) ZEB-633: a plain `Persist` fault downgrades ONLY when the
    /// community dir is gone at check time (the race surfaced live as EINVAL,
    /// not ENOENT, on macOS) — with the dir present it still propagates.
    #[test]
    fn shutdown_flush_dir_removal_predicate_downgrades_race_only() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let existing = td.path().to_path_buf();
        let missing = td.path().join("removed-by-rollback");

        // (1) The causal PersistDirMissing variant downgrades regardless of
        // the dir's current state (it may have been racily recreated).
        assert!(shutdown_flush_lost_race_to_dir_removal(
            &Err(CommunitySyncError::PersistDirMissing(
                "io: No such file or directory".into()
            )),
            Some(&existing),
        ));
        // (2) ZEB-633: a Persist fault with the dir GONE is the same race
        // wearing a different errno (observed: EINVAL from a rename against
        // the dying dir) — downgrade.
        assert!(shutdown_flush_lost_race_to_dir_removal(
            &Err(CommunitySyncError::Persist(
                "io: Invalid argument (os error 22)".into()
            )),
            Some(&missing),
        ));
        // A real persist disk fault (e.g. ENOSPC) with the dir still present
        // must still propagate (ZEB-460).
        assert!(!shutdown_flush_lost_race_to_dir_removal(
            &Err(CommunitySyncError::Persist("io: disk full".into())),
            Some(&existing),
        ));
        // Unknown dir (no parent) is conservative: propagate.
        assert!(!shutdown_flush_lost_race_to_dir_removal(
            &Err(CommunitySyncError::Persist("io: disk full".into())),
            None,
        ));
        // Success and unrelated errors are never downgraded.
        assert!(!shutdown_flush_lost_race_to_dir_removal(
            &Ok(()),
            Some(&missing)
        ));
        assert!(!shutdown_flush_lost_race_to_dir_removal(
            &Err(CommunitySyncError::TransportClosed),
            Some(&missing),
        ));
    }

    /// ZEB-463 (Qodo PR #267): `map_persist_err` routes ONLY io `NotFound` to
    /// `PersistDirMissing` — the causal signal the shutdown arm keys on —
    /// while every other failure stays a plain `Persist` so real durability
    /// faults still propagate.
    #[test]
    fn map_persist_err_routes_only_not_found_to_dir_missing() {
        use crate::community_state_persist::PersistError;
        // io NotFound → PersistDirMissing
        assert!(matches!(
            map_persist_err(PersistError::Io(std::io::Error::from(
                std::io::ErrorKind::NotFound
            ))),
            CommunitySyncError::PersistDirMissing(_)
        ));
        // Another io kind (e.g. PermissionDenied) → plain Persist
        assert!(matches!(
            map_persist_err(PersistError::Io(std::io::Error::from(
                std::io::ErrorKind::PermissionDenied
            ))),
            CommunitySyncError::Persist(_)
        ));
        // A non-io PersistError → plain Persist
        assert!(matches!(
            map_persist_err(PersistError::CborEncode("bad".into())),
            CommunitySyncError::Persist(_)
        ));
    }

    /// Spec §7.1 #2: spawn engine, drop guard without commit, verify
    /// engine absent + persistence dir absent.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guard_drop_without_commit_tears_down_fresh() {
        let fix = build_test_fixture().await;
        let community_id = SpaceId([0xc2; 16]);

        let (pub_tx, pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let (sub_tx, sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

        {
            let mut guard = std::sync::Arc::clone(&fix.registry).begin_spawn_guard(community_id);
            let _engine = std::sync::Arc::clone(&fix.registry)
                .spawn_engine_with_guard(
                    &mut guard,
                    community_id,
                    fix.membership_key.clone(),
                    fix.admin_addr,
                    false,
                    pub_tx,
                    sub_rx,
                    pub_rx,
                    sub_tx,
                    fix.community_adapter_tx.clone(),
                    CatchUpChannels::none(),
                    dummy_root_serve_tx(),
                    dummy_fetch_request_rx(),
                )
                .await
                .expect("spawn_engine_with_guard");
            // guard drops here without commit → Drop spawns cleanup task
        }

        // Poll up to 2s for the cleanup task to BOTH remove the engine
        // from the registry map AND remove the per-community persistence
        // dir from disk. shutdown_engine_and_cleanup_persistence runs
        // these in sequence: stop_engine first (engine.shutdown().await
        // + map remove) THEN tokio::fs::remove_dir_all. CI's slower disk
        // I/O exposed a race where has_engine() became false (after
        // stop_engine) but the dir was still present (remove_dir_all
        // not yet completed). The fix: wait for BOTH conditions.
        let dir = fix
            .identity_dir
            .join("communities")
            .join(hex::encode(community_id.0));
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(2000);
        while (fix.registry.has_engine(&community_id).await || dir.exists())
            && std::time::Instant::now() < deadline
        {
            tokio::task::yield_now().await;
        }
        assert!(
            !fix.registry.has_engine(&community_id).await,
            "engine must be torn down after guard drops without commit"
        );
        assert!(
            !dir.exists(),
            "persistence dir must be removed after guard drops"
        );
    }

    /// ZEB-436: a guard rollback must NOT delete a persistence dir that
    /// predates the spawn. `freshly_created` tracks ENGINE freshness
    /// (registry map insertion) — an orphan re-adoption (ZEB-427) or a
    /// rejoin-after-leave (ZEB-427 Half B retains dirs of left
    /// communities) freshly spawns an engine over a dir holding the
    /// user's entire community history. Pre-fix, the rollback's
    /// unconditional `remove_dir_all` destroyed exactly the data the
    /// repair existed to recover. Counterpart to
    /// `guard_drop_without_commit_tears_down_fresh`, which pins that a
    /// dir CREATED by the failed spawn is still removed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guard_drop_preserves_preexisting_persistence_zeb436() {
        let fix = build_test_fixture().await;
        let community_id = SpaceId([0xc6; 16]);

        // Pre-existing persistence: a crdt.cbor written BEFORE the spawn
        // (what an orphaned community dir looks like on disk).
        let dir = fix
            .identity_dir
            .join("communities")
            .join(hex::encode(community_id.0));
        std::fs::create_dir_all(&dir).expect("create community dir");
        let crdt_path = dir.join("crdt.cbor");
        let preexisting = CommunityState::new(community_id);
        crate::community_state_persist::save_crdt(&crdt_path, &preexisting)
            .expect("seed pre-existing crdt.cbor");

        let (pub_tx, pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let (sub_tx, sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        {
            let mut guard = std::sync::Arc::clone(&fix.registry).begin_spawn_guard(community_id);
            let _engine = std::sync::Arc::clone(&fix.registry)
                .spawn_engine_with_guard(
                    &mut guard,
                    community_id,
                    fix.membership_key.clone(),
                    fix.admin_addr,
                    false,
                    pub_tx,
                    sub_rx,
                    pub_rx,
                    sub_tx,
                    fix.community_adapter_tx.clone(),
                    CatchUpChannels::none(),
                    dummy_root_serve_tx(),
                    dummy_fetch_request_rx(),
                )
                .await
                .expect("spawn_engine_with_guard");
            // guard drops here without commit → rollback
        }

        // Wait for the rollback to remove the engine, then hold the line:
        // the (pre-fix) deletion is sequenced immediately after the engine
        // stop, so a short grace period gives the bug a real chance to
        // manifest before the survival assertion.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(5000);
        while fix.registry.has_engine(&community_id).await && std::time::Instant::now() < deadline {
            tokio::task::yield_now().await;
        }
        assert!(
            !fix.registry.has_engine(&community_id).await,
            "rollback must still stop the freshly-spawned engine"
        );
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(
            crdt_path.exists(),
            "rollback must NOT delete a pre-existing persistence dir \
             (the user's community history)"
        );
    }

    // ── ZEB-462 B: community membership CRDT crash-durability ──────────

    /// Build a minimal signed membership event to seed an engine's in-memory
    /// CRDT. Bypasses verify (inserted directly into `state.events`) — these
    /// tests exercise the persist round-trip, not the verify path.
    fn seed_membership_event(community_id: SpaceId, actor: OwnerAddr) -> SignedMembershipEvent {
        let payload = crate::community_membership::EventPayload {
            id: [0xee; 16],
            community_id,
            kind: crate::community_membership::MembershipEventKind::Join,
            actor,
            at: crate::owner_state_types::Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "seed-dev".to_string(),
            },
        };
        let sk = ed25519_dalek::SigningKey::from_bytes(&[0x42; 32]);
        crate::community_membership::sign_event(&payload, &sk).expect("sign_event")
    }

    /// `persist_now` durably fences the in-memory CRDT to disk WITHOUT
    /// publishing (the join-commit fence). A SIGKILL right after a join must
    /// not lose membership the engine holds only in memory.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn persist_now_fences_crdt_without_publishing_zeb462() {
        let fix = build_test_fixture().await;
        let community_id = SpaceId([0xd1; 16]);
        let community_dir = fix
            .identity_dir
            .join("communities")
            .join(hex::encode(community_id.0));
        let crdt_path = community_dir.join("crdt.cbor");
        let replay_path = community_dir.join("replay.cbor");

        let (pub_tx, pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let (sub_tx, sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let mut guard = std::sync::Arc::clone(&fix.registry).begin_spawn_guard(community_id);
        let engine = std::sync::Arc::clone(&fix.registry)
            .spawn_engine_with_guard(
                &mut guard,
                community_id,
                fix.membership_key.clone(),
                fix.admin_addr,
                false,
                pub_tx,
                sub_rx,
                pub_rx,
                sub_tx,
                fix.community_adapter_tx.clone(),
                CatchUpChannels::none(),
                dummy_root_serve_tx(),
                dummy_fetch_request_rx(),
            )
            .await
            .expect("spawn_engine_with_guard");
        guard.commit();

        let event = seed_membership_event(community_id, fix.admin_addr);
        let eid = event.id;
        engine.state().lock().await.events.insert(eid, event);

        // Publish-independent durable fence (the join-commit path). No publish
        // has run, yet the CRDT must be on disk after this returns.
        engine.persist_now().await.expect("persist_now");

        let loaded = load_crdt(&crdt_path, community_id).expect("load_crdt after persist_now");
        assert!(
            loaded.events.contains_key(&eid),
            "persist_now must durably write the in-memory membership event"
        );
        // CRDT-ONLY regression (Cursor / CodeRabbit PR #253): persist_now must
        // NOT write replay.cbor — fencing the tracker here could durably record
        // an unpublished next_hlc advance left in memory by a failed publish.
        // `persist_both` would create replay.cbor; `persist_crdt_only` does not.
        assert!(
            !replay_path.exists(),
            "persist_now must be CRDT-only — it must not fence the replay tracker"
        );

        engine.shutdown().await.ok();
    }

    /// A flush whose PUBLISH fails must still persist the CRDT — validated
    /// membership events are durable facts. Pre-fix the persist was gated on
    /// publish success, so a co-located joiner whose publish never landed
    /// never wrote `crdt.cbor`, and a SIGKILL lost the entire membership
    /// (admin + self → the gate's synthetic `Left` for every publisher).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn flush_persists_crdt_even_when_publish_fails_zeb462() {
        let mut fix = build_test_fixture().await;
        let community_id = SpaceId([0xd2; 16]);
        let crdt_path = fix
            .identity_dir
            .join("communities")
            .join(hex::encode(community_id.0))
            .join("crdt.cbor");

        let (pub_tx, pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let (sub_tx, sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let mut guard = std::sync::Arc::clone(&fix.registry).begin_spawn_guard(community_id);
        let engine = std::sync::Arc::clone(&fix.registry)
            .spawn_engine_with_guard(
                &mut guard,
                community_id,
                fix.membership_key.clone(),
                fix.admin_addr,
                false,
                pub_tx,
                sub_rx,
                pub_rx,
                sub_tx,
                fix.community_adapter_tx.clone(),
                CatchUpChannels::none(),
                dummy_root_serve_tx(),
                dummy_fetch_request_rx(),
            )
            .await
            .expect("spawn_engine_with_guard");
        guard.commit();

        // Drop the adapter request that owns `pub_rx` so the engine's outbound
        // publish channel has no receiver — models the co-located "publish
        // never lands" case. (The fixture's CAS receiver is also dropped, so
        // the encode step fails first regardless; this makes the intent
        // explicit + robust to encode-path changes.)
        let adapter_req = fix
            .community_adapter_rx
            .recv()
            .await
            .expect("adapter request");
        drop(adapter_req);

        let event = seed_membership_event(community_id, fix.admin_addr);
        let eid = event.id;
        engine.state().lock().await.events.insert(eid, event);

        // flush_now publishes (FAILS — no CAS / no receiver) then, with the
        // fix, still persists the CRDT. The Result surfaces the publish error.
        let flush_result = engine.flush_now().await;
        assert!(
            flush_result.is_err(),
            "publish must fail with the adapter receiver + CAS dropped"
        );

        let loaded =
            load_crdt(&crdt_path, community_id).expect("load_crdt after failed-publish flush");
        assert!(
            loaded.events.contains_key(&eid),
            "ZEB-462 B: CRDT must persist even when the publish failed"
        );

        engine.shutdown().await.ok();
    }

    /// Cursor PR #253 R2: when the join-commit `persist_now` fence FAILS, it
    /// must re-arm the engine's dirty bit (mirroring `fence_owner_state_flush`)
    /// so the next debounce / shutdown retries the persist. Without the re-arm,
    /// a prior in-flight debounce that cleared the dirty bit (publish ok,
    /// `persist_both` failed) followed by a failed fence leaves `crdt.cbor`
    /// stale with nothing armed, and a SIGKILL loses the membership.
    ///
    /// Rig the failure deterministically: shut the engine down first so the
    /// fence's `persist_now()` cannot be serviced (it returns `TransportClosed`,
    /// or at worst times out) — no timing race. Both failure branches of the
    /// fence call `notify_dirty`, so the dirty bit must be set afterward
    /// regardless of which one fires.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fence_rearms_debounce_when_persist_fails_zeb462() {
        use std::sync::atomic::Ordering;

        let fix = build_test_fixture().await;
        let community_id = SpaceId([0xd3; 16]);

        let (pub_tx, pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let (sub_tx, sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let mut guard = std::sync::Arc::clone(&fix.registry).begin_spawn_guard(community_id);
        let engine = std::sync::Arc::clone(&fix.registry)
            .spawn_engine_with_guard(
                &mut guard,
                community_id,
                fix.membership_key.clone(),
                fix.admin_addr,
                false,
                pub_tx,
                sub_rx,
                pub_rx,
                sub_tx,
                fix.community_adapter_tx.clone(),
                CatchUpChannels::none(),
                dummy_root_serve_tx(),
                dummy_fetch_request_rx(),
            )
            .await
            .expect("spawn_engine_with_guard");
        guard.commit();

        // Stop the single-writer task so the fence's `persist_now` can never be
        // serviced — a deterministic stand-in for a wedged / failed persist.
        engine.shutdown().await.ok();
        // Establish the precondition explicitly: a clear dirty bit, so a
        // post-fence `true` can only have come from the fence's re-arm.
        engine.has_pending_dirty.store(false, Ordering::Relaxed);

        // The real production helper, run against a dead engine: `persist_now`
        // fails, so the fence must re-arm the debounce.
        crate::fence_community_crdt_persist(
            &engine,
            std::time::Duration::from_secs(5),
            "zeb462_fence_test",
            "deadbeefdeadbeefdeadbeefdeadbeef",
        )
        .await;

        assert!(
            engine.has_pending_dirty.load(Ordering::Relaxed),
            "a failed join-commit fence must re-arm the dirty bit so the next \
             debounce / shutdown retries the membership persist (Cursor PR #253 R2)"
        );
    }

    /// ZEB-436 PR #229 R1 (Qodo): the pre-existence marker must be the
    /// community DIR, not `crdt.cbor` — durable channel-log history
    /// lives under `channels/...` and can exist while `crdt.cbor` is
    /// absent (e.g. quarantined after a corrupt decode). Pre-fix such a
    /// dir probed as "fresh" and a rollback deleted the user's channel
    /// history.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guard_preserves_dir_with_channel_history_but_no_crdt_zeb436() {
        let fix = build_test_fixture().await;
        let community_id = SpaceId([0xc7; 16]);

        let dir = fix
            .identity_dir
            .join("communities")
            .join(hex::encode(community_id.0));
        let channel_dir = dir.join("channels").join("00ff");
        std::fs::create_dir_all(&channel_dir).expect("create channel dir");
        let manifest = channel_dir.join("manifest.cbor");
        std::fs::write(&manifest, b"durable channel history").expect("seed manifest");

        let (pub_tx, pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let (sub_tx, sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        {
            let mut guard = std::sync::Arc::clone(&fix.registry).begin_spawn_guard(community_id);
            let _engine = std::sync::Arc::clone(&fix.registry)
                .spawn_engine_with_guard(
                    &mut guard,
                    community_id,
                    fix.membership_key.clone(),
                    fix.admin_addr,
                    false,
                    pub_tx,
                    sub_rx,
                    pub_rx,
                    sub_tx,
                    fix.community_adapter_tx.clone(),
                    CatchUpChannels::none(),
                    dummy_root_serve_tx(),
                    dummy_fetch_request_rx(),
                )
                .await
                .expect("spawn_engine_with_guard");
            // guard drops here without commit → rollback
        }

        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(5000);
        while fix.registry.has_engine(&community_id).await && std::time::Instant::now() < deadline {
            tokio::task::yield_now().await;
        }
        assert!(
            !fix.registry.has_engine(&community_id).await,
            "rollback must still stop the freshly-spawned engine"
        );
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(
            manifest.exists(),
            "rollback must NOT delete a pre-existing community dir even \
             when crdt.cbor is absent — channels/ holds durable history"
        );
    }

    /// Spec §7.1 #3: open guard A, spawn engine; open guard B for the
    /// same community (idempotent — sees existing engine), drop B
    /// without commit; verify engine still present (B's guard didn't
    /// tear down because freshly_created = false).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guard_drop_idempotent_call_is_noop() {
        let fix = build_test_fixture().await;
        let community_id = SpaceId([0xc3; 16]);

        let (pub_tx_a, pub_rx_a) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let (sub_tx_a, sub_rx_a) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

        // Caller A: spawns the engine fresh.
        let mut guard_a = std::sync::Arc::clone(&fix.registry).begin_spawn_guard(community_id);
        let _engine_a = std::sync::Arc::clone(&fix.registry)
            .spawn_engine_with_guard(
                &mut guard_a,
                community_id,
                fix.membership_key.clone(),
                fix.admin_addr,
                false,
                pub_tx_a,
                sub_rx_a,
                pub_rx_a,
                sub_tx_a,
                fix.community_adapter_tx.clone(),
                CatchUpChannels::none(),
                dummy_root_serve_tx(),
                dummy_fetch_request_rx(),
            )
            .await
            .expect("spawn_engine_with_guard A");
        guard_a.commit();

        // Caller B: spawns idempotently (engine pre-existing).
        let (pub_tx_b, pub_rx_b) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let (sub_tx_b, sub_rx_b) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        {
            let mut guard_b = std::sync::Arc::clone(&fix.registry).begin_spawn_guard(community_id);
            let _engine_b = std::sync::Arc::clone(&fix.registry)
                .spawn_engine_with_guard(
                    &mut guard_b,
                    community_id,
                    fix.membership_key.clone(),
                    fix.admin_addr,
                    false,
                    pub_tx_b,
                    sub_rx_b,
                    pub_rx_b,
                    sub_tx_b,
                    fix.community_adapter_tx.clone(),
                    CatchUpChannels::none(),
                    dummy_root_serve_tx(),
                    dummy_fetch_request_rx(),
                )
                .await
                .expect("spawn_engine_with_guard B (idempotent)");
            // guard_b drops here without commit. freshly_created = false → Drop is no-op.
        }

        // Engine must STILL be present (B's guard didn't tear down A's engine).
        assert!(
            fix.registry.has_engine(&community_id).await,
            "engine must remain after idempotent caller B's guard drops uncommitted"
        );
    }

    /// Spec §7.1 #4: spawn engine, abort guard, verify engine absent.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guard_explicit_abort_tears_down() {
        let fix = build_test_fixture().await;
        let community_id = SpaceId([0xc4; 16]);

        let (pub_tx, pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let (sub_tx, sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

        let mut guard = std::sync::Arc::clone(&fix.registry).begin_spawn_guard(community_id);
        let _engine = std::sync::Arc::clone(&fix.registry)
            .spawn_engine_with_guard(
                &mut guard,
                community_id,
                fix.membership_key.clone(),
                fix.admin_addr,
                false,
                pub_tx,
                sub_rx,
                pub_rx,
                sub_tx,
                fix.community_adapter_tx.clone(),
                CatchUpChannels::none(),
                dummy_root_serve_tx(),
                dummy_fetch_request_rx(),
            )
            .await
            .expect("spawn_engine_with_guard");

        guard.abort();

        // Poll up to 2s for abort's spawned cleanup task. Deadline
        // bumped from 500ms to 2s for CI's slower disk I/O (the
        // shutdown path includes engine.shutdown().await which can
        // flush pending writes — multi-ms on CI runners).
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(2000);
        while fix.registry.has_engine(&community_id).await && std::time::Instant::now() < deadline {
            tokio::task::yield_now().await;
        }
        assert!(
            !fix.registry.has_engine(&community_id).await,
            "engine must be torn down after explicit abort"
        );
    }

    /// Spec §7.1 #5: drop guard from a non-tokio thread; verify the
    /// no-runtime path runs (logs warn) and the engine remains
    /// (acknowledged leak per spec §10.2 — reconcile recovers at next
    /// start_node).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guard_drop_no_runtime_logs_and_leaks() {
        let fix = build_test_fixture().await;
        let community_id = SpaceId([0xc5; 16]);

        let (pub_tx, pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let (sub_tx, sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

        let mut guard = std::sync::Arc::clone(&fix.registry).begin_spawn_guard(community_id);
        let _engine = std::sync::Arc::clone(&fix.registry)
            .spawn_engine_with_guard(
                &mut guard,
                community_id,
                fix.membership_key.clone(),
                fix.admin_addr,
                false,
                pub_tx,
                sub_rx,
                pub_rx,
                sub_tx,
                fix.community_adapter_tx.clone(),
                CatchUpChannels::none(),
                dummy_root_serve_tx(),
                dummy_fetch_request_rx(),
            )
            .await
            .expect("spawn_engine_with_guard");

        // Move the guard into a synchronous (non-tokio) thread and
        // drop it there. The Drop impl's Handle::try_current() must
        // return Err and take the no-runtime fallback (log + leak).
        let drop_thread = std::thread::spawn(move || {
            // Verify no runtime is reachable from this thread.
            assert!(
                tokio::runtime::Handle::try_current().is_err(),
                "bare std::thread must not have a runtime handle"
            );
            drop(guard);
        });
        drop_thread.join().expect("drop thread");

        // Engine MUST still be present (no-runtime path can't tear down).
        // This is the acknowledged leak per spec §10.2.
        assert!(
            fix.registry.has_engine(&community_id).await,
            "engine must remain after no-runtime Drop (leak acknowledged per spec §10.2)"
        );

        // Cleanup for test isolation: tear down explicitly via the registry.
        fix.registry
            .shutdown_engine_and_cleanup_persistence(&community_id, || Ok(()))
            .await
            .expect("explicit cleanup for test isolation");
    }

    /// ZEB-732: `shutdown_engine_and_cleanup_persistence` must ABORT the
    /// `remove_dir_all` when `gen_check` reports a changed node generation —
    /// the persistence dir must survive (a concurrent stop/start installed a
    /// fresh live community for this id during `stop_engine().await`). The
    /// same call with a passing `gen_check` then deletes the dir, proving the
    /// guard is the only thing holding the delete back.
    #[tokio::test]
    async fn shutdown_cleanup_aborts_delete_on_gen_check_mismatch() {
        let fix = build_test_fixture().await;
        let community_id = SpaceId([0x73; 16]);
        // No engine is registered for this id → `stop_engine` is a no-op, so
        // the test isolates the `gen_check` gate in front of `remove_dir_all`.
        let dir = fix
            .identity_dir
            .join("communities")
            .join(hex::encode(community_id.0));
        std::fs::create_dir_all(&dir).expect("create community dir");
        std::fs::write(dir.join("crdt.cbor"), b"x").expect("seed dir");

        // gen_check reports a mismatch → CleanupAborted, dir preserved.
        let aborted = fix
            .registry
            .shutdown_engine_and_cleanup_persistence(&community_id, || {
                Err("node generation changed".to_string())
            })
            .await;
        assert!(
            matches!(aborted, Err(CommunitySyncError::CleanupAborted(_))),
            "a failed gen_check must surface as CleanupAborted, got {aborted:?}"
        );
        assert!(
            dir.exists(),
            "aborted cleanup must NOT delete the community dir"
        );

        // gen_check passes → the dir IS deleted (proves the guard was the only
        // thing preventing the delete).
        fix.registry
            .shutdown_engine_and_cleanup_persistence(&community_id, || Ok(()))
            .await
            .expect("cleanup with passing gen_check succeeds");
        assert!(
            !dir.exists(),
            "passing gen_check must delete the community dir"
        );
        // ZEB-732: detach-then-delete must not leave a `.deleting` temp behind.
        let leftover: Vec<String> = std::fs::read_dir(fix.identity_dir.join("communities"))
            .expect("read communities dir")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".deleting"))
            .collect();
        assert!(
            leftover.is_empty(),
            "detach-then-delete must remove its temp dir, found: {leftover:?}"
        );
    }

    /// CodeRabbit round 2: regression test for the adapter-dispatch-failure
    /// rollback path inside spawn_engine_with_guard (spec §5.3 step 3).
    /// Force community_adapter_tx.try_send to fail by closing the receiver
    /// BEFORE calling spawn_engine_with_guard. The function should:
    ///   1. Spawn the engine via spawn_engine_inner_now (succeeds)
    ///   2. Try to dispatch the adapter request → fails (channel closed)
    ///   3. Tear down the freshly spawned engine inline before returning Err
    ///   4. Leave guard.freshly_created = false so Drop is a no-op
    ///
    /// Asserts: spawn returns Err; engine is NOT in the registry (rolled
    /// back inline); persistence dir is NOT created (rolled back inline).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guard_try_send_failure_rolls_back_without_arming_guard() {
        let mut fix = build_test_fixture().await;
        let community_id = SpaceId([0xc6; 16]);

        // Drop the receiver to close the adapter channel. Subsequent
        // try_send calls on the surviving Sender will return Closed.
        // Take ownership of the rx field by replacing with a dummy,
        // then explicitly drop the original (binding to `_`-prefixed
        // local would extend the lifetime to the end of the enclosing
        // scope, defeating the close).
        let (_dummy_tx, dummy_rx) = mpsc::channel(1);
        drop(std::mem::replace(&mut fix.community_adapter_rx, dummy_rx));

        let (pub_tx, pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let (sub_tx, sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

        let mut guard = std::sync::Arc::clone(&fix.registry).begin_spawn_guard(community_id);
        let result = std::sync::Arc::clone(&fix.registry)
            .spawn_engine_with_guard(
                &mut guard,
                community_id,
                fix.membership_key.clone(),
                fix.admin_addr,
                false,
                pub_tx,
                sub_rx,
                pub_rx,
                sub_tx,
                fix.community_adapter_tx.clone(),
                CatchUpChannels::none(),
                dummy_root_serve_tx(),
                dummy_fetch_request_rx(),
            )
            .await;

        assert!(
            result.is_err(),
            "spawn_engine_with_guard must return Err when adapter try_send fails"
        );

        // Engine MUST NOT be in the registry (rolled back inline by
        // spawn_engine_with_guard's step 3 — NOT by the guard's Drop,
        // which would only run if freshly_created=true; we're verifying
        // the inline rollback path).
        assert!(
            !fix.registry.has_engine(&community_id).await,
            "engine must NOT be in the registry after adapter try_send failure \
             (spawn_engine_with_guard rolled back inline)"
        );

        // Persistence dir MUST NOT exist either.
        let dir = fix
            .identity_dir
            .join("communities")
            .join(hex::encode(community_id.0));
        assert!(
            !dir.exists(),
            "persistence dir must NOT exist after adapter try_send failure \
             (rolled back inline before guard arming)"
        );

        // The guard drops at scope exit. Its Drop should be a no-op:
        // CR round 4's set-early-arm pattern initially sets
        // freshly_created=true (so Drop catches any later failure),
        // but the successful inline cleanup branch resets it to false
        // before returning Err. So the guard arrives at Drop with
        // freshly_created=false → no redundant teardown.
    }

    /// CR round 4 finding #2: regression test for the use-once flag.
    /// Calling spawn_engine_with_guard a second time on the same guard
    /// must return Err immediately — without this check, the second
    /// call would overwrite freshly_created and silently disable the
    /// rollback obligation from the first spawn.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guard_use_once_rejects_second_spawn_call() {
        let fix = build_test_fixture().await;
        let community_id = SpaceId([0xc7; 16]);

        let (pub_tx_a, pub_rx_a) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let (sub_tx_a, sub_rx_a) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

        let mut guard = std::sync::Arc::clone(&fix.registry).begin_spawn_guard(community_id);

        // First call: succeeds. Engine is spawned + adapter dispatched.
        std::sync::Arc::clone(&fix.registry)
            .spawn_engine_with_guard(
                &mut guard,
                community_id,
                fix.membership_key.clone(),
                fix.admin_addr,
                false,
                pub_tx_a,
                sub_rx_a,
                pub_rx_a,
                sub_tx_a,
                fix.community_adapter_tx.clone(),
                CatchUpChannels::none(),
                dummy_root_serve_tx(),
                dummy_fetch_request_rx(),
            )
            .await
            .expect("first spawn_engine_with_guard call");

        // Second call on the SAME guard: must Err with use-once message.
        let (pub_tx_b, pub_rx_b) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let (sub_tx_b, sub_rx_b) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let result = std::sync::Arc::clone(&fix.registry)
            .spawn_engine_with_guard(
                &mut guard,
                community_id,
                fix.membership_key.clone(),
                fix.admin_addr,
                false,
                pub_tx_b,
                sub_rx_b,
                pub_rx_b,
                sub_tx_b,
                fix.community_adapter_tx.clone(),
                CatchUpChannels::none(),
                dummy_root_serve_tx(),
                dummy_fetch_request_rx(),
            )
            .await;

        // Arc<CommunitySyncEngine> doesn't implement Debug, so use
        // match instead of expect_err.
        let err = match result {
            Ok(_) => panic!("second spawn_engine_with_guard call must Err, got Ok"),
            Err(e) => e,
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("called twice on the same guard"),
            "second-call Err must mention use-once violation; got: {msg}"
        );

        // Commit the guard to release rollback obligation cleanly.
        guard.commit();

        // Engine must still be present (the second call's Err didn't
        // tear it down — only the rejection message matters).
        assert!(
            fix.registry.has_engine(&community_id).await,
            "engine from the first call must remain after the second use-once-rejected call"
        );
    }

    /// CR round 5: regression test for the Arc::ptr_eq cross-registry
    /// rejection branch added in CR round 3. Two separate
    /// CommunitySyncRegistry instances; guard from registry A is passed
    /// to registry B's spawn_engine_with_guard. Must Err immediately,
    /// before any spawn side effects.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guard_wrong_registry_rejected() {
        let fix_a = build_test_fixture().await;
        let fix_b = build_test_fixture().await;
        let community_id = SpaceId([0xc8; 16]);

        // Open guard on registry A; pass it to registry B.
        let mut guard = std::sync::Arc::clone(&fix_a.registry).begin_spawn_guard(community_id);

        let (pub_tx, pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let (sub_tx, sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

        let result = std::sync::Arc::clone(&fix_b.registry)
            .spawn_engine_with_guard(
                &mut guard,
                community_id,
                fix_b.membership_key.clone(),
                fix_b.admin_addr,
                false,
                pub_tx,
                sub_rx,
                pub_rx,
                sub_tx,
                fix_b.community_adapter_tx.clone(),
                CatchUpChannels::none(),
                dummy_root_serve_tx(),
                dummy_fetch_request_rx(),
            )
            .await;

        let err = match result {
            Ok(_) => panic!("cross-registry call must Err, got Ok"),
            Err(e) => e,
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("guard/registry mismatch")
                || msg.contains("different CommunitySyncRegistry"),
            "Err must mention registry mismatch; got: {msg}"
        );

        // Engine must NOT be in EITHER registry (rejection happened
        // before any spawn side effects).
        assert!(
            !fix_a.registry.has_engine(&community_id).await,
            "registry A must have no engine (no spawn happened)"
        );
        assert!(
            !fix_b.registry.has_engine(&community_id).await,
            "registry B must have no engine (rejected before spawn)"
        );

        // Commit guard A to release its rollback obligation cleanly
        // (otherwise Drop runs on a registry-A guard that nothing was
        // spawned on — harmless but spammy log).
        guard.commit();
    }

    /// CR round 5: regression test for the community_id mismatch
    /// rejection branch added in CR round 2. Same registry; guard
    /// opened for community X but spawn called with community Y.
    /// Must Err immediately, before any spawn side effects.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guard_wrong_community_id_rejected() {
        let fix = build_test_fixture().await;
        let community_x = SpaceId([0xc9; 16]);
        let community_y = SpaceId([0xca; 16]);

        // Guard for X; call with Y.
        let mut guard = std::sync::Arc::clone(&fix.registry).begin_spawn_guard(community_x);

        let (pub_tx, pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let (sub_tx, sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

        let result = std::sync::Arc::clone(&fix.registry)
            .spawn_engine_with_guard(
                &mut guard,
                community_y, // mismatch — guard is for X
                fix.membership_key.clone(),
                fix.admin_addr,
                false,
                pub_tx,
                sub_rx,
                pub_rx,
                sub_tx,
                fix.community_adapter_tx.clone(),
                CatchUpChannels::none(),
                dummy_root_serve_tx(),
                dummy_fetch_request_rx(),
            )
            .await;

        let err = match result {
            Ok(_) => panic!("community_id mismatch call must Err, got Ok"),
            Err(e) => e,
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("guard/community_id mismatch"),
            "Err must mention community_id mismatch; got: {msg}"
        );

        // No engine in registry for EITHER community.
        assert!(
            !fix.registry.has_engine(&community_x).await,
            "no engine for community X (guard was for X but never spawned)"
        );
        assert!(
            !fix.registry.has_engine(&community_y).await,
            "no engine for community Y (rejected before spawn)"
        );

        // Commit guard for X to release rollback obligation cleanly.
        guard.commit();
    }

    // ── ZEB-434 D2: query-serve arm ─────────────────────────────────

    /// In-memory CAS servicer shared by both engines, mirroring
    /// `community_channel_config_integration::spawn_shared_cas` so blobs
    /// engine A `put_serveable`s are visible to engine B's `GetOrFetch`.
    fn spawn_shared_cas() -> mpsc::Sender<crate::content_store::CasOp> {
        use crate::content_store::CasOp;
        let cas: Arc<Mutex<std::collections::HashMap<harmony_content::cid::ContentId, Vec<u8>>>> =
            Arc::new(Mutex::new(std::collections::HashMap::new()));
        let (cas_op_tx, mut cas_op_rx) = mpsc::channel::<CasOp>(64);
        let cas_for_servicer = Arc::clone(&cas);
        tokio::spawn(async move {
            while let Some(op) = cas_op_rx.recv().await {
                match op {
                    CasOp::PutLocal {
                        cid, blob, reply, ..
                    } => {
                        cas_for_servicer.lock().await.insert(cid, blob);
                        if let Some(r) = reply {
                            let _ = r.send(Ok(()));
                        }
                    }
                    CasOp::GetOrFetch {
                        cid,
                        timeout: _,
                        reply,
                    } => {
                        let v = cas_for_servicer.lock().await.get(&cid).cloned();
                        let _ = reply.send(Ok(v));
                    }
                    CasOp::GetLocal { cid, reply } => {
                        let v = cas_for_servicer.lock().await.get(&cid).cloned();
                        let _ = reply.send(v);
                    }
                    CasOp::AllowServeSubtree { reply, .. } => {
                        // Not exercised by these state-sync fixtures.
                        let _ = reply.send(Ok(0));
                    }
                }
            }
        });
        cas_op_tx
    }

    /// ZEB-434 D2: the engine's query-serve arm replies with a fresh
    /// root packet that a PEER engine ingests through its FULL inbound
    /// verification pipeline (decrypt, membership-at-HLC gate,
    /// publisher-sig verify, replay guard, merge, materialize).
    ///
    /// Fixture mirrors
    /// `community_channel_config_integration::alice_creates_channel_bob_materializes_via_state_sync`
    /// (same shared-CAS stub, same membership_key wiring, same
    /// EnrollmentCert-bearing bootstrap Join + OOB cold-cache seed into
    /// the receiver), but builds the engines directly via
    /// `CommunitySyncEngine::new` so engine A's config can carry
    /// `root_serve_rx: Some(..)`. Engine A's `publisher_rx` is drained
    /// and DISCARDED — the packet reaching B travels exclusively
    /// through the query-serve reply, proving the serve path works
    /// without any pub/sub traffic from A.
    #[tokio::test]
    async fn query_serve_arm_replies_packet_that_peer_engine_ingests() {
        use crate::community_membership::{
            mint_test_owner, sign_event, ChannelId, ChannelKind, EventPayload, MembershipEventKind,
            SignedMembershipEvent,
        };
        use crate::community_state_crdt::InsertOutcome;

        let alice = mint_test_owner(0xAA);
        let bob = mint_test_owner(0xBB);
        let alice_addr = alice.owner;
        let bob_addr = bob.owner;
        let alice_sk = Arc::new(alice.device_key.clone());
        let bob_sk = Arc::new(bob.device_key.clone());

        // ZEB-339: signer resolution uses the carried EnrollmentCert
        // (Join) / materialized enrolled keys (steady-state), not the
        // resolver — NopResolver matches the integration fixture's
        // unused-pub resolver.
        let resolver: Arc<dyn IdentityResolver> = Arc::new(NopResolver);

        let cas_op_tx = spawn_shared_cas();
        let cs_a: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
            cas_op_tx.clone(),
            std::time::Duration::from_secs(2),
        ));
        let cs_b: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
            cas_op_tx,
            std::time::Duration::from_secs(2),
        ));

        let dir_a = tempfile::tempdir().expect("dir a");
        let dir_b = tempfile::tempdir().expect("dir b");

        let community_id = SpaceId([0x3A; 16]);
        let membership_key = EpochKey::new([0x55; 32]);

        // Engine A (admin): query-serve channel wired. Its publisher_rx
        // is drained + discarded below — no pub/sub traffic reaches B.
        let (serve_tx, serve_rx) = mpsc::channel::<RootServeRequest>(4);
        let (a_pub_tx, mut a_pub_rx) = mpsc::channel::<Vec<u8>>(64);
        let (_a_sub_tx_held, a_sub_rx) = mpsc::channel::<Vec<u8>>(64);
        tokio::spawn(async move { while a_pub_rx.recv().await.is_some() {} });

        let engine_a = CommunitySyncEngine::new(CommunitySyncEngineConfig {
            community_id,
            membership_key: membership_key.clone(),
            admin_addr: alice_addr,
            is_invite_only: false,
            device_id: "alice-dev".into(),
            self_owner: alice_addr,
            signing_key: Arc::clone(&alice_sk),
            state: Arc::new(Mutex::new(CommunityState::new(community_id))),
            tracker: Arc::new(Mutex::new(CommunityRootHlcTracker::default())),
            content_store: cs_a,
            publisher_tx: a_pub_tx,
            subscriber_rx: a_sub_rx,
            paths: PersistPaths {
                crdt: dir_a.path().join("crdt.cbor"),
                replay: dir_a.path().join("replay.cbor"),
            },
            debounce_ms: DEFAULT_DEBOUNCE_MS,
            identity_resolver: Some(Arc::clone(&resolver)),
            error_tx: None,
            delta_tx: None,
            pending_redemptions: None,
            crdt_state: None,
            admin_identity_pub: None,
            nav_emitter: None,
            root_serve_rx: Some(serve_rx),
        });

        // Engine B (member, same community/key, no serve channel). We
        // hold b_sub_tx to inject the served packet and b_pub_rx so B's
        // own debounced publishes don't latch transport_closed.
        let (b_pub_tx, _b_pub_rx_held) = mpsc::channel::<Vec<u8>>(64);
        let (b_sub_tx, b_sub_rx) = mpsc::channel::<Vec<u8>>(64);
        let b_state = Arc::new(Mutex::new(CommunityState::new(community_id)));

        let engine_b = CommunitySyncEngine::new(CommunitySyncEngineConfig {
            community_id,
            membership_key,
            admin_addr: alice_addr,
            is_invite_only: false,
            device_id: "bob-dev".into(),
            self_owner: bob_addr,
            signing_key: Arc::clone(&bob_sk),
            state: Arc::clone(&b_state),
            tracker: Arc::new(Mutex::new(CommunityRootHlcTracker::default())),
            content_store: cs_b,
            publisher_tx: b_pub_tx,
            subscriber_rx: b_sub_rx,
            paths: PersistPaths {
                crdt: dir_b.path().join("crdt.cbor"),
                replay: dir_b.path().join("replay.cbor"),
            },
            debounce_ms: DEFAULT_DEBOUNCE_MS,
            identity_resolver: Some(resolver),
            error_tx: None,
            delta_tx: None,
            pending_redemptions: None,
            crdt_state: None,
            admin_identity_pub: None,
            nav_emitter: None,
            root_serve_rx: None,
        });

        // Alice's EnrollmentCert-bearing bootstrap Join (admin power 100).
        let alice_join_at = Hlc {
            wall_ms: 100_000,
            logical: 0,
            device_id: "alice-dev".into(),
        };
        let alice_join_payload = EventPayload {
            id: [0x10; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: alice_addr,
            at: alice_join_at.clone(),
        };
        let alice_join = SignedMembershipEvent {
            enrollment: Some(alice.cert.clone()),
            ..sign_event(&alice_join_payload, alice_sk.as_ref()).expect("sign join")
        };
        let outcome = engine_a
            .insert_local_event(alice_join.clone())
            .await
            .expect("alice bootstrap insert");
        assert_eq!(outcome, InsertOutcome::Inserted);

        // ZEB-256 cold-cache simulation (mirrors the integration
        // fixture): OOB-seed Alice's bootstrap Join into B so B's
        // membership-at-HLC gate admits the served packet. Production
        // wires this via the invite URL's admin_bootstrap field.
        let outcome = engine_b
            .insert_local_event(alice_join)
            .await
            .expect("bob OOB-seeds Alice's bootstrap Join");
        assert_eq!(outcome, InsertOutcome::Inserted);

        // Alice's ChannelCreate at (bootstrap.wall, bootstrap.logical+1).
        let ch_id = ChannelId([0x42; 16]);
        let alice_create_payload = EventPayload {
            id: [0x11; 16],
            community_id,
            kind: MembershipEventKind::ChannelCreate {
                channel_id: ch_id,
                name: "general".into(),
                write_power: 0,
                kind: ChannelKind::Text,
            },
            actor: alice_addr,
            at: Hlc {
                wall_ms: alice_join_at.wall_ms,
                logical: alice_join_at.logical + 1,
                device_id: alice_join_at.device_id.clone(),
            },
        };
        let alice_create =
            sign_event(&alice_create_payload, alice_sk.as_ref()).expect("sign channel create");
        let outcome = engine_a
            .insert_local_event(alice_create)
            .await
            .expect("alice channel-create insert");
        assert_eq!(outcome, InsertOutcome::Inserted);

        // Drive the query-serve arm: one oneshot in, one fresh packet out.
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        serve_tx.send(reply_tx).await.expect("send serve request");
        let packet = reply_rx.await.expect("engine replied").expect("encode ok");

        // Feed the served packet into B's inbound pipeline — full
        // verification (decrypt, replay guard, membership check) must
        // pass for the channel to materialize.
        b_sub_tx.send(packet).await.expect("inject packet into B");

        let mut materialized = false;
        for _ in 0..40 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let mat = {
                let g = b_state.lock().await;
                g.materialize_now(alice_addr)
            };
            if let Some(info) = mat.channels.get(&ch_id) {
                assert_eq!(info.name, "general");
                assert_eq!(info.write_power, 0);
                assert!(info.deleted_at.is_none());
                materialized = true;
                break;
            }
        }
        assert!(
            materialized,
            "channel from the query-serve packet must materialize on engine B"
        );
    }

    /// PR #230 review (Qodo + CodeRabbit): replay-tracker persistence
    /// is part of the query-serve SUCCESS condition. encode advances
    /// `next_hlc`, so a served packet implies an HLC this node must
    /// durably remember — if `persist_replay_only` fails, the engine
    /// must reply `Err` (the adapter then withholds the zenoh reply and
    /// the querier retries) instead of handing out the packet.
    ///
    /// Fault injection: a DIRECTORY pre-created at the replay path
    /// makes the tracker save fail deterministically while everything
    /// else (encode, CAS put, signing) succeeds.
    #[tokio::test]
    async fn query_serve_arm_withholds_packet_when_replay_persist_fails() {
        use crate::community_membership::mint_test_owner;

        let alice = mint_test_owner(0xAD);
        let alice_addr = alice.owner;
        let alice_sk = Arc::new(alice.device_key.clone());
        let resolver: Arc<dyn IdentityResolver> = Arc::new(NopResolver);

        let cas_op_tx = spawn_shared_cas();
        let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
            cas_op_tx,
            std::time::Duration::from_secs(2),
        ));

        let dir = tempfile::tempdir().expect("dir");
        let replay_path = dir.path().join("replay.cbor");
        std::fs::create_dir(&replay_path).expect("pre-create dir at replay path");

        let community_id = SpaceId([0x3B; 16]);
        let (serve_tx, serve_rx) = mpsc::channel::<RootServeRequest>(4);
        let (pub_tx, mut pub_rx) = mpsc::channel::<Vec<u8>>(64);
        let (_sub_tx_held, sub_rx) = mpsc::channel::<Vec<u8>>(64);
        tokio::spawn(async move { while pub_rx.recv().await.is_some() {} });

        let _engine = CommunitySyncEngine::new(CommunitySyncEngineConfig {
            community_id,
            membership_key: EpochKey::new([0x66; 32]),
            admin_addr: alice_addr,
            is_invite_only: false,
            device_id: "alice-dev".into(),
            self_owner: alice_addr,
            signing_key: alice_sk,
            state: Arc::new(Mutex::new(CommunityState::new(community_id))),
            tracker: Arc::new(Mutex::new(CommunityRootHlcTracker::default())),
            content_store: cs,
            publisher_tx: pub_tx,
            subscriber_rx: sub_rx,
            paths: PersistPaths {
                crdt: dir.path().join("crdt.cbor"),
                replay: replay_path,
            },
            debounce_ms: DEFAULT_DEBOUNCE_MS,
            identity_resolver: Some(resolver),
            error_tx: None,
            delta_tx: None,
            pending_redemptions: None,
            crdt_state: None,
            admin_identity_pub: None,
            nav_emitter: None,
            root_serve_rx: Some(serve_rx),
        });

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        serve_tx.send(reply_tx).await.expect("send serve request");
        let served = reply_rx.await.expect("engine replied");
        let err = served.expect_err("persist failure must withhold the packet");
        assert!(
            err.to_lowercase().contains("persist"),
            "error must come from the persist step (not encode): {err}"
        );

        // The arm survives the failure (warn + Err, not task death):
        // a second request still gets a reply.
        let (reply_tx2, reply_rx2) = tokio::sync::oneshot::channel();
        serve_tx
            .send(reply_tx2)
            .await
            .expect("second serve request");
        assert!(reply_rx2.await.expect("engine replied again").is_err());
    }

    /// ZEB-434 Task 10: end-to-end repro pin for the live bug — a
    /// channel created while a member was offline stayed invisible to
    /// them indefinitely (the root publish fired into the void; no
    /// pull path existed).
    ///
    /// Fixture mirrors `query_serve_arm_replies_packet_that_peer_engine_ingests`
    /// (two engines, shared CAS, EnrollmentCert bootstrap Join OOB-seeded
    /// into B, A's publisher drained into the void = "B was offline for
    /// the publish"). The deltas over that test:
    ///
    /// 1. The pull is driven BY [`run_root_fetch_driver`] through a
    ///    request closure that bridges to A's query-serve channel and
    ///    forwards the reply into B's subscriber inbound — exactly what
    ///    the production adapter's fetch task + queryable do across
    ///    zenoh, with the wire collapsed.
    /// 2. Idempotency: a SECOND fetch through the same bridge must be
    ///    AlreadyKnown-only on B — no event growth, channel map
    ///    unchanged. The equality assertion is guarded against a
    ///    too-short settle window by first polling B's replay tracker
    ///    until its `(alice, alice-dev)` high-water mark advances past
    ///    the first packet's HLC (the second packet carries a strictly
    ///    newer HLC from A's fresh encode, and the tracker records it
    ///    at step 14, AFTER the merge) — proving the second packet was
    ///    fully processed before we compare counts.
    #[tokio::test]
    async fn offline_created_channel_heals_via_root_fetch_pull() {
        use crate::channel_backfill::{run_root_fetch_driver, RootFetch, RootFetchLatch};
        use crate::community_membership::{
            mint_test_owner, sign_event, ChannelId, ChannelKind, EventPayload, MembershipEventKind,
            SignedMembershipEvent,
        };
        use crate::community_state_crdt::InsertOutcome;

        let alice = mint_test_owner(0xAA);
        let bob = mint_test_owner(0xBB);
        let alice_addr = alice.owner;
        let bob_addr = bob.owner;
        let alice_sk = Arc::new(alice.device_key.clone());
        let bob_sk = Arc::new(bob.device_key.clone());

        let resolver: Arc<dyn IdentityResolver> = Arc::new(NopResolver);

        let cas_op_tx = spawn_shared_cas();
        let cs_a: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
            cas_op_tx.clone(),
            std::time::Duration::from_secs(2),
        ));
        let cs_b: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
            cas_op_tx,
            std::time::Duration::from_secs(2),
        ));

        let dir_a = tempfile::tempdir().expect("dir a");
        let dir_b = tempfile::tempdir().expect("dir b");

        let community_id = SpaceId([0x3B; 16]);
        let membership_key = EpochKey::new([0x55; 32]);

        // Engine A (admin): query-serve channel wired. A's publisher_rx
        // is drained + DISCARDED — its root publish for the
        // ChannelCreate "fires into the void" exactly like the live
        // repro where B was offline. No pub/sub bytes can reach B: B's
        // subscriber_tx is held by this test and fed ONLY by the
        // root-fetch bridge below.
        let (serve_tx, serve_rx) = mpsc::channel::<RootServeRequest>(4);
        let (a_pub_tx, mut a_pub_rx) = mpsc::channel::<Vec<u8>>(64);
        let (_a_sub_tx_held, a_sub_rx) = mpsc::channel::<Vec<u8>>(64);
        tokio::spawn(async move { while a_pub_rx.recv().await.is_some() {} });

        let engine_a = CommunitySyncEngine::new(CommunitySyncEngineConfig {
            community_id,
            membership_key: membership_key.clone(),
            admin_addr: alice_addr,
            is_invite_only: false,
            device_id: "alice-dev".into(),
            self_owner: alice_addr,
            signing_key: Arc::clone(&alice_sk),
            state: Arc::new(Mutex::new(CommunityState::new(community_id))),
            tracker: Arc::new(Mutex::new(CommunityRootHlcTracker::default())),
            content_store: cs_a,
            publisher_tx: a_pub_tx,
            subscriber_rx: a_sub_rx,
            paths: PersistPaths {
                crdt: dir_a.path().join("crdt.cbor"),
                replay: dir_a.path().join("replay.cbor"),
            },
            debounce_ms: DEFAULT_DEBOUNCE_MS,
            identity_resolver: Some(Arc::clone(&resolver)),
            error_tx: None,
            delta_tx: None,
            pending_redemptions: None,
            crdt_state: None,
            admin_identity_pub: None,
            nav_emitter: None,
            root_serve_rx: Some(serve_rx),
        });

        // Engine B (member): we hold its tracker Arc so the test can
        // observe the per-(addr, device) replay high-water mark — the
        // settle guard for the idempotency phase.
        let (b_pub_tx, _b_pub_rx_held) = mpsc::channel::<Vec<u8>>(64);
        let (b_sub_tx, b_sub_rx) = mpsc::channel::<Vec<u8>>(64);
        let b_state = Arc::new(Mutex::new(CommunityState::new(community_id)));
        let b_tracker = Arc::new(Mutex::new(CommunityRootHlcTracker::default()));

        let engine_b = CommunitySyncEngine::new(CommunitySyncEngineConfig {
            community_id,
            membership_key,
            admin_addr: alice_addr,
            is_invite_only: false,
            device_id: "bob-dev".into(),
            self_owner: bob_addr,
            signing_key: Arc::clone(&bob_sk),
            state: Arc::clone(&b_state),
            tracker: Arc::clone(&b_tracker),
            content_store: cs_b,
            publisher_tx: b_pub_tx,
            subscriber_rx: b_sub_rx,
            paths: PersistPaths {
                crdt: dir_b.path().join("crdt.cbor"),
                replay: dir_b.path().join("replay.cbor"),
            },
            debounce_ms: DEFAULT_DEBOUNCE_MS,
            identity_resolver: Some(resolver),
            error_tx: None,
            delta_tx: None,
            pending_redemptions: None,
            crdt_state: None,
            admin_identity_pub: None,
            nav_emitter: None,
            root_serve_rx: None,
        });

        // Alice's EnrollmentCert-bearing bootstrap Join (admin power 100).
        let alice_join_at = Hlc {
            wall_ms: 100_000,
            logical: 0,
            device_id: "alice-dev".into(),
        };
        let alice_join_payload = EventPayload {
            id: [0x20; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: alice_addr,
            at: alice_join_at.clone(),
        };
        let alice_join = SignedMembershipEvent {
            enrollment: Some(alice.cert.clone()),
            ..sign_event(&alice_join_payload, alice_sk.as_ref()).expect("sign join")
        };
        let outcome = engine_a
            .insert_local_event(alice_join.clone())
            .await
            .expect("alice bootstrap insert");
        assert_eq!(outcome, InsertOutcome::Inserted);

        // ZEB-256 cold-cache simulation: OOB-seed Alice's bootstrap
        // Join into B so B's membership-at-HLC gate admits the served
        // packet. Production wires this via the invite URL's
        // admin_bootstrap field.
        let outcome = engine_b
            .insert_local_event(alice_join)
            .await
            .expect("bob OOB-seeds Alice's bootstrap Join");
        assert_eq!(outcome, InsertOutcome::Inserted);

        // THE REPRO MOMENT: Alice creates a channel while Bob is
        // "offline". The debounced root publish for this insert lands
        // in the drained a_pub_rx void — Bob never sees it via pub/sub.
        let ch_id = ChannelId([0x43; 16]);
        let alice_create_payload = EventPayload {
            id: [0x21; 16],
            community_id,
            kind: MembershipEventKind::ChannelCreate {
                channel_id: ch_id,
                name: "created-while-offline".into(),
                write_power: 0,
                kind: ChannelKind::Text,
            },
            actor: alice_addr,
            at: Hlc {
                wall_ms: alice_join_at.wall_ms,
                logical: alice_join_at.logical + 1,
                device_id: alice_join_at.device_id.clone(),
            },
        };
        let alice_create =
            sign_event(&alice_create_payload, alice_sk.as_ref()).expect("sign channel create");
        let outcome = engine_a
            .insert_local_event(alice_create)
            .await
            .expect("alice channel-create insert");
        assert_eq!(outcome, InsertOutcome::Inserted);

        // The bridge closure — production's queryable + fetch task
        // collapsed onto one in-process hop: send a RootServeRequest
        // oneshot into A's serve channel, await the fresh packet,
        // forward it into B's inbound subscriber channel.
        let request_root = {
            let serve_tx = serve_tx.clone();
            let b_sub_tx = b_sub_tx.clone();
            move || {
                let serve_tx = serve_tx.clone();
                let b_sub_tx = b_sub_tx.clone();
                async move {
                    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                    if serve_tx.send(reply_tx).await.is_err() {
                        return RootFetch::EngineGone;
                    }
                    match reply_rx.await {
                        Ok(Ok(packet)) => {
                            if b_sub_tx.send(packet).await.is_err() {
                                return RootFetch::EngineGone;
                            }
                            RootFetch::Answered
                        }
                        _ => RootFetch::NoReply,
                    }
                }
            }
        };

        // Bob "comes online": the root-fetch driver pulls. epoch_rx =
        // None → the driver returns once the latch is satisfied. The
        // 30 s timeout turns a serve-arm regression (NoReply → real
        // backoff sleeps forever) into a test FAILURE instead of a hang.
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            run_root_fetch_driver(
                RootFetchLatch::new(),
                request_root.clone(),
                shutdown_rx,
                None,
                // no presence watch
                None,
                // resync disabled (epoch None too): the driver must return
                // on Idle once satisfied, as this test's 30 s timeout
                // assumes.
                None,
                // Real wall clock is fine — the driver satisfies on the
                // first request; no backoff sleeps on the happy path.
                || {
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0)
                },
                // no persist sink
                None,
            ),
        )
        .await
        .expect("root-fetch driver must be Answered and return within 30s");

        // The healed state: the offline-created channel materializes on
        // B through its FULL inbound verification pipeline.
        let mut materialized = false;
        for _ in 0..40 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let mat = {
                let g = b_state.lock().await;
                g.materialize_now(alice_addr)
            };
            if let Some(info) = mat.channels.get(&ch_id) {
                assert_eq!(info.name, "created-while-offline");
                assert_eq!(info.write_power, 0);
                assert!(info.deleted_at.is_none());
                materialized = true;
                break;
            }
        }
        assert!(
            materialized,
            "offline-created channel must heal onto engine B via the root-fetch pull"
        );

        // Baseline for the idempotency phase. The tracker records the
        // packet HLC at step 14, AFTER the merge that materialize
        // observed above — poll briefly for it.
        let tracker_key = (alice_addr, "alice-dev".to_string());
        let mut first_hlc: Option<Hlc> = None;
        for _ in 0..40 {
            let entry = {
                let g = b_tracker.lock().await;
                g.per_device.get(&tracker_key).cloned()
            };
            if let Some(h) = entry {
                first_hlc = Some(h);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let first_hlc =
            first_hlc.expect("B's replay tracker must record A's packet HLC after first ingest");
        let (events_before, channels_before) = {
            let g = b_state.lock().await;
            (g.events.len(), g.materialize_now(alice_addr).channels.len())
        };
        assert_eq!(
            events_before, 2,
            "B holds exactly the bootstrap Join + the pulled ChannelCreate"
        );

        // Idempotency: a SECOND fetch through the same bridge must be
        // a no-op on B's CRDT (every event AlreadyKnown).
        let outcome = request_root().await;
        assert_eq!(outcome, RootFetch::Answered);

        // Settle guard: A's fresh encode stamps a strictly-newer HLC,
        // and B records it only after the full receive pipeline ran —
        // so a tracker advance PROVES the second packet was processed,
        // making the unchanged-count assertion below meaningful (a
        // too-short wait can't false-pass).
        let mut second_processed = false;
        for _ in 0..40 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let entry = {
                let g = b_tracker.lock().await;
                g.per_device.get(&tracker_key).cloned()
            };
            if let Some(h) = entry {
                if h.is_strictly_newer_than(&first_hlc) {
                    second_processed = true;
                    break;
                }
            }
        }
        assert!(
            second_processed,
            "B's replay tracker must advance past the first packet's HLC — \
             the second served packet carries a fresh, strictly-newer HLC"
        );

        let (events_after, channels_after) = {
            let g = b_state.lock().await;
            (g.events.len(), g.materialize_now(alice_addr).channels.len())
        };
        assert_eq!(
            events_before, events_after,
            "second root fetch must be AlreadyKnown-only — no event growth"
        );
        assert_eq!(
            channels_before, channels_after,
            "second root fetch must not change the materialized channel map"
        );
        let mat = {
            let g = b_state.lock().await;
            g.materialize_now(alice_addr)
        };
        let info = mat
            .channels
            .get(&ch_id)
            .expect("channel still present after idempotent re-fetch");
        assert_eq!(info.name, "created-while-offline");
    }

    // ── ZEB-712: engine closing guard ────────────────────────────────────
    //
    // The registry-detach fences in lib.rs shrink but cannot close the
    // TOCTOU: a lifecycle IPC that passed its re-lock fence can still call
    // `insert_local_event` on a snapshot Arc AFTER `stop_inner` →
    // `shutdown_all()` ran the engine's final flush — the event is signed
    // and accepted into engine memory that no task will ever persist or
    // publish, while the IPC reports success. These tests pin the closure:
    // an insert either lands BEFORE the closing flag (and is included in
    // the shutdown arm's final flush — durable) or errors. The silent
    // third outcome is what ZEB-712 removes.

    /// Minimal single-engine fixture for the closing-guard tests: direct
    /// `CommunitySyncEngine::new` (same shape as the query-serve fixture
    /// above), publisher_rx drained so debounced publishes never latch
    /// transport_closed, NopResolver (signer resolution uses the carried
    /// EnrollmentCert / materialized membership). Returns the subscriber
    /// sender alongside the engine — callers bind it (`_sub_tx`) so the
    /// channel stays open for the test's duration; dropping it would make
    /// the engine latch "subscriber channel closed; sync inbound disabled"
    /// error noise (Qodo, PR #492 — same class as the PR #307 precedent).
    fn closing_guard_engine(
        dir: &tempfile::TempDir,
        community_id: SpaceId,
        alice: &crate::community_membership::TestOwner,
    ) -> (CommunitySyncEngine, mpsc::Sender<Vec<u8>>) {
        let (pub_tx, mut pub_rx) = mpsc::channel::<Vec<u8>>(64);
        let (sub_tx, sub_rx) = mpsc::channel::<Vec<u8>>(64);
        tokio::spawn(async move { while pub_rx.recv().await.is_some() {} });
        let cas_op_tx = spawn_shared_cas();
        let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
            cas_op_tx,
            std::time::Duration::from_secs(2),
        ));
        let engine = CommunitySyncEngine::new(CommunitySyncEngineConfig {
            community_id,
            membership_key: EpochKey::new([0x55; 32]),
            admin_addr: alice.owner,
            is_invite_only: false,
            device_id: "alice-dev".into(),
            self_owner: alice.owner,
            signing_key: Arc::new(alice.device_key.clone()),
            state: Arc::new(Mutex::new(CommunityState::new(community_id))),
            tracker: Arc::new(Mutex::new(CommunityRootHlcTracker::default())),
            content_store: cs,
            publisher_tx: pub_tx,
            subscriber_rx: sub_rx,
            paths: PersistPaths {
                crdt: dir.path().join("crdt.cbor"),
                replay: dir.path().join("replay.cbor"),
            },
            debounce_ms: DEFAULT_DEBOUNCE_MS,
            identity_resolver: Some(Arc::new(NopResolver)),
            error_tx: None,
            delta_tx: None,
            pending_redemptions: None,
            crdt_state: None,
            admin_identity_pub: None,
            nav_emitter: None,
            root_serve_rx: None,
        });
        (engine, sub_tx)
    }

    /// Alice's EnrollmentCert-bearing bootstrap Join (the same construction
    /// every fixture in this module uses for the admin bootstrap).
    fn closing_guard_bootstrap_join(
        community_id: SpaceId,
        alice: &crate::community_membership::TestOwner,
        event_id: [u8; 16],
        logical: u32,
    ) -> crate::community_membership::SignedMembershipEvent {
        use crate::community_membership::{
            sign_event, EventPayload, MembershipEventKind, SignedMembershipEvent,
        };
        let payload = EventPayload {
            id: event_id,
            community_id,
            kind: MembershipEventKind::Join,
            actor: alice.owner,
            at: Hlc {
                wall_ms: 100_000,
                logical,
                device_id: "alice-dev".into(),
            },
        };
        SignedMembershipEvent {
            enrollment: Some(alice.cert.clone()),
            ..sign_event(&payload, &alice.device_key).expect("sign join")
        }
    }

    /// Alice's steady-state ChannelCreate (verifies via her materialized
    /// enrolled key from the bootstrap Join — no cert carried).
    fn closing_guard_channel_create(
        community_id: SpaceId,
        alice: &crate::community_membership::TestOwner,
        event_id: [u8; 16],
        channel_byte: u8,
        logical: u32,
    ) -> crate::community_membership::SignedMembershipEvent {
        use crate::community_membership::{
            sign_event, ChannelId, ChannelKind, EventPayload, MembershipEventKind,
        };
        let payload = EventPayload {
            id: event_id,
            community_id,
            kind: MembershipEventKind::ChannelCreate {
                channel_id: ChannelId([channel_byte; 16]),
                name: format!("chan-{channel_byte:02x}"),
                write_power: 0,
                kind: ChannelKind::Text,
            },
            actor: alice.owner,
            at: Hlc {
                wall_ms: 100_000,
                logical,
                device_id: "alice-dev".into(),
            },
        };
        sign_event(&payload, &alice.device_key).expect("sign channel create")
    }

    /// R1: an insert AFTER `shutdown()` must surface an error — pre-fix it
    /// returns `Ok(Inserted)` into engine memory nothing will ever persist
    /// or publish (the exact ZEB-712 silent-loss bug).
    #[tokio::test]
    async fn insert_after_shutdown_errs_engine_shutting_down() {
        use crate::community_membership::mint_test_owner;
        use crate::community_state_crdt::InsertOutcome;

        let alice = mint_test_owner(0xAA);
        let community_id = SpaceId([0x71; 16]);
        let dir = tempfile::tempdir().expect("tempdir");
        let (engine, _sub_tx) = closing_guard_engine(&dir, community_id, &alice);

        // Prove the fixture: the same construction inserts fine pre-shutdown.
        let join = closing_guard_bootstrap_join(community_id, &alice, [0x10; 16], 0);
        let outcome = engine
            .insert_local_event(join)
            .await
            .expect("pre-shutdown bootstrap insert");
        assert_eq!(outcome, InsertOutcome::Inserted);

        engine.shutdown().await.expect("shutdown");

        // Post-shutdown: the SAME kind of construction that succeeded above
        // must now be refused, not silently accepted.
        let create = closing_guard_channel_create(community_id, &alice, [0x11; 16], 0x42, 1);
        let err = engine
            .insert_local_event(create)
            .await
            .expect_err("insert after shutdown must error — Ok here means the event was signed into memory that will never persist or publish");
        assert!(
            err.to_string().contains("shutting down"),
            "expected the engine-shutting-down error, got: {err}"
        );
    }

    /// R1 companion: `insert_local_event_pair` (the second local-insert
    /// entry point, its own C5-atomic lock block) gets the same guard.
    #[tokio::test]
    async fn insert_pair_after_shutdown_errs_engine_shutting_down() {
        use crate::community_membership::mint_test_owner;
        use crate::community_state_crdt::InsertOutcome;

        let alice = mint_test_owner(0xAB);
        let community_id = SpaceId([0x72; 16]);
        let dir = tempfile::tempdir().expect("tempdir");
        let (engine, _sub_tx) = closing_guard_engine(&dir, community_id, &alice);

        let join = closing_guard_bootstrap_join(community_id, &alice, [0x10; 16], 0);
        let outcome = engine
            .insert_local_event(join)
            .await
            .expect("pre-shutdown bootstrap insert");
        assert_eq!(outcome, InsertOutcome::Inserted);

        engine.shutdown().await.expect("shutdown");

        let first = closing_guard_channel_create(community_id, &alice, [0x11; 16], 0x42, 1);
        let second = closing_guard_channel_create(community_id, &alice, [0x12; 16], 0x43, 2);
        let err = engine
            .insert_local_event_pair(first, second)
            .await
            .expect_err("pair insert after shutdown must error");
        assert!(
            err.to_string().contains("shutting down"),
            "expected the engine-shutting-down error, got: {err}"
        );
    }

    /// R2 (pin): an insert that lands BEFORE shutdown is included in the
    /// shutdown arm's final flush — the durable half of the closing-guard
    /// semantics. Guards against a future "fix" that rejects instead of
    /// flushing the already-accepted side of the race.
    #[tokio::test]
    async fn insert_before_shutdown_included_in_final_flush() {
        use crate::community_membership::mint_test_owner;
        use crate::community_state_crdt::InsertOutcome;

        let alice = mint_test_owner(0xAC);
        let community_id = SpaceId([0x73; 16]);
        let dir = tempfile::tempdir().expect("tempdir");
        let crdt_path = dir.path().join("crdt.cbor");
        let (engine, _sub_tx) = closing_guard_engine(&dir, community_id, &alice);

        let join = closing_guard_bootstrap_join(community_id, &alice, [0x10; 16], 0);
        let join_id = join.id;
        let outcome = engine
            .insert_local_event(join)
            .await
            .expect("pre-shutdown bootstrap insert");
        assert_eq!(outcome, InsertOutcome::Inserted);

        engine.shutdown().await.expect("shutdown");

        let loaded = load_crdt(&crdt_path, community_id).expect("load persisted crdt");
        assert!(
            loaded.events.contains_key(&join_id),
            "pre-shutdown insert must reach disk via the shutdown arm's final flush"
        );
    }

    /// CR-1 (#492): the auto-counter-sign task bypasses `insert_local_event`
    /// (deliberate direct state mutation to avoid an engine back-reference),
    /// so it needs the SAME closing fence — without it, a task spawned or
    /// scheduled around shutdown can acquire the state lock after the final
    /// flush and append a JoinCountersign nothing will ever persist or
    /// publish. Dropping it is safe: countersign eligibility idempotently
    /// re-derives on next boot (C1 restart-recovery).
    ///
    /// The fixture stages alice's bootstrap Join + bob's PendingJoin by
    /// direct log insertion (NOT `insert_local_event`) so no competing
    /// auto-counter-sign spawn fires before the shutdown — the only
    /// countersign attempt is the explicit post-shutdown one under test.
    #[tokio::test]
    async fn auto_counter_sign_after_shutdown_inserts_nothing() {
        use crate::community_membership::{
            mint_test_owner, sign_event, EventPayload, MembershipEventKind, SignedMembershipEvent,
        };

        let alice = mint_test_owner(0xAD);
        let bob = mint_test_owner(0xBE);
        let community_id = SpaceId([0x74; 16]);
        let dir = tempfile::tempdir().expect("tempdir");
        let (engine, _sub_tx) = closing_guard_engine(&dir, community_id, &alice);

        let alice_join = closing_guard_bootstrap_join(community_id, &alice, [0x10; 16], 0);
        let bob_pending_payload = EventPayload {
            id: [0x20; 16],
            community_id,
            kind: MembershipEventKind::PendingJoin {
                invite_token: crate::community_invite::InviteToken {
                    inviter: alice.owner,
                    invitee_hint: None,
                    minted_at: Hlc {
                        wall_ms: 99_000,
                        logical: 0,
                        device_id: "alice-dev".into(),
                    },
                    expires_at: None,
                    sig: [0u8; 64],
                },
            },
            actor: bob.owner,
            at: Hlc {
                wall_ms: 100_500,
                logical: 0,
                device_id: "bob-dev".into(),
            },
        };
        let bob_pending = SignedMembershipEvent {
            enrollment: Some(bob.cert.clone()),
            ..sign_event(&bob_pending_payload, &bob.device_key).expect("sign pending join")
        };
        let pending_id = bob_pending.id;
        let state = engine.state();
        {
            let mut g = state.lock().await;
            g.events.insert(alice_join.id, alice_join);
            g.events.insert(bob_pending.id, bob_pending.clone());
        }

        engine.shutdown().await.expect("shutdown");

        engine.maybe_spawn_auto_counter_sign(&bob_pending);
        // Let the spawned task run to completion (its awaits are all
        // uncontended lock acquisitions; a bounded yield budget is ample).
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }

        let g = state.lock().await;
        let countersigned = g.events.values().any(|e| {
            matches!(
                &e.kind,
                MembershipEventKind::JoinCountersign { target_event_id }
                if *target_event_id == pending_id
            )
        });
        assert!(
            !countersigned,
            "auto-counter-sign must not insert after shutdown — a JoinCountersign \
             here was signed into engine memory nothing will ever persist or publish"
        );
    }
}

#[cfg(test)]
mod envelope_tests {
    use super::*;
    use crate::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
    use crate::owner_state_types::{EpochKey, Hlc, OwnerAddr, Space, SpaceId, SpaceKind};

    #[test]
    fn encrypted_envelope_round_trip() {
        let env = EncryptedEnvelope {
            epoch: 5,
            nonce: [0x10; 12],
            ciphertext: vec![0x20; 32],
            ratchet_generation: None,
        };
        let bytes = canonical_cbor_encode(&env).expect("encode");
        let decoded: EncryptedEnvelope = canonical_cbor_decode(&bytes).expect("decode");
        assert_eq!(
            decoded, env,
            "EncryptedEnvelope round-trip must preserve all fields"
        );
    }

    #[test]
    fn encrypted_envelope_with_ratchet_generation() {
        let env = EncryptedEnvelope {
            epoch: 5,
            nonce: [0x10; 12],
            ciphertext: vec![0x20; 32],
            ratchet_generation: Some(42),
        };
        let bytes = canonical_cbor_encode(&env).expect("encode");
        let decoded: EncryptedEnvelope = canonical_cbor_decode(&bytes).expect("decode");
        assert_eq!(decoded, env);
        assert_eq!(decoded.ratchet_generation, Some(42));
    }

    /// Build a minimal Community-kind Space with the given epoch and key.
    /// Uses placeholder values for fields not relevant to epoch crypto.
    fn build_test_community_space(epoch: u64, key: EpochKey) -> Space {
        super::test_community_space(SpaceId([0xaa; 16]), epoch, key)
    }

    /// ZEB-597: the case-C publisher must key on the LIVE current_epoch_key
    /// (so published records track epoch rotation, matching what seekers read
    /// via `live_epoch_key` in ZEB-596) — not the spawn-time fallback.
    #[tokio::test]
    async fn community_publish_epoch_key_prefers_live_over_fallback() {
        let cid = SpaceId([0xaa; 16]);
        let live = [0x77u8; 32];
        let fallback = EpochKey::new([0x42u8; 32]);
        let mut os = crate::owner_state_crdt::OwnerState::default();
        os.spaces
            .insert(cid, build_test_community_space(3, EpochKey::new(live)));
        let crdt = Arc::new(Mutex::new(os));
        let key = community_publish_epoch_key(cid, &crdt, &fallback).await;
        assert_eq!(
            key, live,
            "must publish under the LIVE epoch key, not the spawn-time fallback"
        );
    }

    /// ZEB-597: re-reading after the Space's current_epoch_key rotates must
    /// yield the NEW key — the regression guard for the spawn-time-key bug
    /// (a captured spawn-time key would stay frozen at the old value here).
    #[tokio::test]
    async fn community_publish_epoch_key_tracks_rotation() {
        let cid = SpaceId([0xaa; 16]);
        let fallback = EpochKey::new([0x42u8; 32]);
        let mut os = crate::owner_state_crdt::OwnerState::default();
        os.spaces.insert(
            cid,
            build_test_community_space(3, EpochKey::new([0x77u8; 32])),
        );
        let crdt = Arc::new(Mutex::new(os));
        assert_eq!(
            community_publish_epoch_key(cid, &crdt, &fallback).await,
            [0x77u8; 32]
        );
        // Rotate the live key (a ZEB-249 epoch advance).
        {
            let mut g = crdt.lock().await;
            g.spaces
                .get_mut(&cid)
                .expect("space present")
                .current_epoch_key = Some(EpochKey::new([0x99u8; 32]));
        }
        assert_eq!(
            community_publish_epoch_key(cid, &crdt, &fallback).await,
            [0x99u8; 32],
            "publisher key must advance with the live epoch key after rotation"
        );
    }

    /// ZEB-597: when the live key is unavailable (Space absent / incomplete),
    /// degrade to the spawn-time fallback so the publisher still publishes
    /// SOMETHING — strict improvement over the prior unconditional spawn-time
    /// key, never worse. (Contrast the seeker — ZEB-596 — which SKIPS the
    /// community in this case rather than probe a stale key.)
    #[tokio::test]
    async fn community_publish_epoch_key_falls_back_when_space_missing() {
        let cid = SpaceId([0xaa; 16]);
        let fallback = EpochKey::new([0x42u8; 32]);
        let crdt = Arc::new(Mutex::new(crate::owner_state_crdt::OwnerState::default()));
        let key = community_publish_epoch_key(cid, &crdt, &fallback).await;
        assert_eq!(
            key, [0x42u8; 32],
            "missing Space must degrade to the spawn-time fallback key"
        );
    }

    #[test]
    fn encrypt_decrypt_round_trip_current_epoch() {
        let key = EpochKey::new([0xab; 32]);
        let space = build_test_community_space(0, key);

        let plaintext = b"hello world from epoch 0";
        let envelope = encrypt_for_topic(&space, plaintext).expect("encrypt");
        assert_eq!(envelope.epoch, 0);
        let decrypted = decrypt_for_topic(&space, &envelope).expect("decrypt");
        assert_eq!(decrypted.as_slice(), plaintext.as_ref());
    }

    // ZEB-717 D1: AAD domain separation between the voting plane and the
    // state-root plane, which share the same community epoch key.
    const VOTING_AAD_FIXTURE: &[u8] = b"harmony-voting-v1";

    #[test]
    fn aad_round_trip_matches() {
        let space = build_test_community_space(0, EpochKey::new([0xab; 32]));
        let pt = b"voting-plaintext";
        let env = encrypt_for_topic_with_aad(&space, pt, VOTING_AAD_FIXTURE).expect("encrypt");
        assert_eq!(env.epoch, 0);
        let out = decrypt_for_topic_with_aad(&space, &env, VOTING_AAD_FIXTURE).expect("decrypt");
        assert_eq!(out.as_slice(), pt.as_ref());
    }

    #[test]
    fn aad_mismatch_rejects() {
        let space = build_test_community_space(0, EpochKey::new([0xab; 32]));
        let env = encrypt_for_topic_with_aad(&space, b"x", VOTING_AAD_FIXTURE).expect("encrypt");
        // Wrong AAD (empty = the state-root domain) must fail the AEAD tag.
        let err = decrypt_for_topic_with_aad(&space, &env, b"").unwrap_err();
        assert!(
            matches!(err, EpochError::DecryptionFailed(_)),
            "cross-plane AAD mismatch must fail the tag, got {err:?}"
        );
        // The 2-arg state-root decrypt (empty AAD) must also refuse a voting envelope.
        assert!(decrypt_for_topic(&space, &env).is_err());
    }

    #[test]
    fn empty_aad_is_byte_compatible_state_root() {
        // The 2-arg path must be indistinguishable from with_aad(.., b"") — this
        // is what keeps state-root wire bytes/fixtures unchanged by ZEB-717.
        let space = build_test_community_space(0, EpochKey::new([0xab; 32]));
        let env = encrypt_for_topic(&space, b"state-root").expect("encrypt");
        assert_eq!(
            decrypt_for_topic(&space, &env)
                .expect("2-arg decrypt")
                .as_slice(),
            b"state-root".as_ref()
        );
        assert_eq!(
            decrypt_for_topic_with_aad(&space, &env, b"")
                .expect("with_aad(b\"\") decrypt")
                .as_slice(),
            b"state-root".as_ref()
        );
    }

    #[test]
    fn decrypt_with_old_epoch_key_succeeds() {
        let old_key = EpochKey::new([0xcc; 32]);
        let new_key = EpochKey::new([0xdd; 32]);
        let mut space = build_test_community_space(1, new_key);
        space.old_epoch_keys.insert(0, old_key.clone());

        // Encrypt with old_key at epoch=0; decrypt under new state with old in old_epoch_keys.
        let nonce = [0x11u8; 12];
        let plaintext = b"old epoch message";
        let cipher = ChaCha20Poly1305::new(old_key.as_chacha_key());
        let ciphertext = cipher
            .encrypt(Nonce::from_slice(&nonce), plaintext.as_ref())
            .expect("encrypt");
        let envelope = EncryptedEnvelope {
            epoch: 0,
            nonce,
            ciphertext,
            ratchet_generation: None,
        };
        let decrypted = decrypt_for_topic(&space, &envelope).expect("decrypt old epoch");
        assert_eq!(decrypted.as_slice(), plaintext.as_ref());
    }

    #[test]
    fn decrypt_missing_epoch_returns_key_not_available() {
        let key = EpochKey::new([0xab; 32]);
        let space = build_test_community_space(0, key);
        let envelope = EncryptedEnvelope {
            epoch: 999,
            nonce: [0; 12],
            ciphertext: vec![0; 16],
            ratchet_generation: None,
        };
        let err = decrypt_for_topic(&space, &envelope).expect_err("must fail");
        assert!(
            matches!(err, EpochError::KeyNotAvailable(999)),
            "expected KeyNotAvailable(999), got {err:?}"
        );
    }

    /// C6: encrypt_for_topic must return Err(MissingEpochState) when
    /// current_epoch is None (partially-migrated Space). Previously panicked.
    #[test]
    fn encrypt_for_topic_returns_error_on_missing_epoch() {
        let zero_hlc = Hlc {
            wall_ms: 0,
            logical: 0,
            device_id: "t".into(),
        };
        let space = Space {
            id: SpaceId([0xaa; 16]),
            kind: SpaceKind::Community,
            parent: None,
            community_id: None,
            name: "Test".into(),
            transport: None,
            members: vec![],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: zero_hlc.clone(),
            updated_at: zero_hlc,
            content_key: None,
            prior_content_keys: vec![],
            current_epoch: None,     // missing!
            current_epoch_key: None, // missing!
            old_epoch_keys: std::collections::BTreeMap::new(),
            admin_addr: Some(OwnerAddr([0xbb; 16])),
            is_invite_only: Some(false),
            shared_in_profile: false,
            pending_join_at: None,
        };
        let err = encrypt_for_topic(&space, b"test payload")
            .expect_err("encrypt_for_topic must return Err on missing epoch state, not panic");
        assert!(
            matches!(err, EpochError::MissingEpochState),
            "expected MissingEpochState, got {err:?}"
        );
    }

    /// C6 (variant): current_epoch is Some but current_epoch_key is None.
    #[test]
    fn encrypt_for_topic_returns_error_on_missing_epoch_key() {
        let zero_hlc = Hlc {
            wall_ms: 0,
            logical: 0,
            device_id: "t".into(),
        };
        let space = Space {
            id: SpaceId([0xaa; 16]),
            kind: SpaceKind::Community,
            parent: None,
            community_id: None,
            name: "Test".into(),
            transport: None,
            members: vec![],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: zero_hlc.clone(),
            updated_at: zero_hlc,
            content_key: None,
            prior_content_keys: vec![],
            current_epoch: Some(0),  // set
            current_epoch_key: None, // missing!
            old_epoch_keys: std::collections::BTreeMap::new(),
            admin_addr: Some(OwnerAddr([0xbb; 16])),
            is_invite_only: Some(false),
            shared_in_profile: false,
            pending_join_at: None,
        };
        let err = encrypt_for_topic(&space, b"test payload")
            .expect_err("encrypt_for_topic must return Err on missing epoch key, not panic");
        assert!(
            matches!(err, EpochError::MissingEpochState),
            "expected MissingEpochState, got {err:?}"
        );
    }
}
