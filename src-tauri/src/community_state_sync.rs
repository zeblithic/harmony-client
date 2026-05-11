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
        .encrypt(Nonce::from_slice(&nonce_bytes), plaintext)
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
            envelope.ciphertext.as_slice(),
        )
        .map_err(|_| EpochError::DecryptionFailed(envelope.epoch))
}

/// Default debounce window between a `notify_dirty` and the resulting
/// state-root publish. Mirrors `owner_state_sync::DEFAULT_DEBOUNCE_MS`
/// (250 ms) — small enough to feel near-instant to a human, large
/// enough to collapse keystroke-rate mutations into one publish.
pub const DEFAULT_DEBOUNCE_MS: u64 = 250;

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
    /// Engine config has `identity_resolver: None`, so receive-side
    /// `verify_event` can't resolve identity_pubs. Distinct error
    /// class because the cause is configuration (Sub-A's owner-device
    /// cache wasn't wired in), not transport or crypto failure.
    #[error("no identity resolver configured — Phase 2 receive-side verify needs one")]
    MissingIdentityResolver,

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

    /// `IdentityResolver` returned `None` for `publisher_addr`. Cold
    /// cache (the publisher's identity_pub hasn't propagated to our
    /// owner-state cache yet) or the addr was never a member.
    /// Transient when caused by cold cache; persistent when caused by
    /// a wholly-fabricated addr — both surface the same way at this
    /// layer. Tracker NOT advanced; next publish after cache
    /// propagation succeeds.
    #[error(
        "publisher {addr:?} identity not in resolver — \
         cache cold or addr not yet propagated"
    )]
    UnknownPublisher { addr: OwnerAddr },

    /// Ed25519 signature over `canonical_cbor(CommunityRootSignedPayload)`
    /// did not validate against the resolved identity_pub. This is
    /// the load-bearing defense against the spoofing attack: a
    /// malicious member with the `EpochKey` cannot forge a
    /// publish claiming another member's `publisher_addr` because
    /// they don't have that member's signing key. Tracker NOT
    /// advanced.
    #[error("publisher signature invalid for addr {addr:?}")]
    PublisherSigInvalid { addr: OwnerAddr },
}

/// Failure modes specific to `CommunitySyncEngine::insert_local_event`.
/// Distinct enum (not a variant on `CommunitySyncError`) because local-
/// insert failures are caller-driven (bad event from IPC) rather than
/// transport / crypto class — the IPC layer needs to surface them as
/// distinct error strings to the frontend.
#[derive(thiserror::Error, Debug)]
pub enum LocalInsertError {
    #[error("identity_resolver not configured — engine cannot verify local events")]
    MissingIdentityResolver,
    #[error("actor identity not in resolver: {0:?}")]
    UnknownActor(OwnerAddr),
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
    flush_now_tx: mpsc::Sender<tokio::sync::oneshot::Sender<Result<(), CommunitySyncError>>>,
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
        let (flush_now_tx, flush_now_rx) = mpsc::channel(8);
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
        // epoch key. The engine struct itself doesn't need the reference —
        // only the task does.
        let crdt_state_for_task = cfg.crdt_state;

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
            flush_now_rx,
            shutdown_rx,
            identity_resolver: cfg.identity_resolver,
            error_tx: cfg.error_tx,
            delta_tx: cfg.delta_tx,
            pending_redemptions: cfg.pending_redemptions,
            crdt_state: crdt_state_for_task,
        }));

        Self {
            notify_dirty,
            has_pending_dirty,
            flush_now_tx,
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

    /// Returns a `ChannelIdentityResolver` adapter wrapping this
    /// engine's `IdentityResolver`. ZEB-270 Phase 3 Task 4.5: the
    /// channel-log engine's verify chain needs an OwnerAddr →
    /// 64-byte composite resolver with the same shape as the existing
    /// `IdentityResolver` (Phase 2 already shares a single
    /// `OwnerDeviceCacheResolver` across all community engines via
    /// `CommunityRegistryConfig.identity_resolver`).
    ///
    /// Returns `None` if the engine was constructed without an
    /// identity_resolver (legacy unit tests pre-Task 8). Production
    /// engines always have one (registry config carries the resolver
    /// and clones it into every spawned engine — see `spawn_engine`).
    pub(crate) fn identity_resolver(
        &self,
    ) -> Option<Arc<dyn crate::community_channel_log::ChannelIdentityResolver + Send + Sync>> {
        self.identity_resolver
            .as_ref()
            .map(|inner| -> Arc<dyn crate::community_channel_log::ChannelIdentityResolver + Send + Sync> {
                Arc::new(ChannelIdentityResolverAdapter {
                    inner: Arc::clone(inner),
                })
            })
    }

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

        let resolver = self
            .identity_resolver
            .as_ref()
            .ok_or(LocalInsertError::MissingIdentityResolver)?;

        let actor_pub = resolver
            .resolve(&event.actor)
            .await
            .ok_or(LocalInsertError::UnknownActor(event.actor))?;

        let countersigner_pub = if let Some(cs) = event.countersig.as_ref() {
            resolver.resolve(&cs.signer).await
        } else {
            None
        };

        self.insert_event_with_resolved_pubs(event, actor_pub, countersigner_pub)
            .await
    }

    /// Variant of `insert_local_event` that accepts already-verified
    /// actor + countersigner identity pubs, bypassing `IdentityResolver`.
    ///
    /// ZEB-262 Phase 4: the invite-only receive path already proves the
    /// joiner's identity_pub via the inline `joiner_identity_pub` field
    /// (Path B app-sig binding) BEFORE this insert runs — but the joiner
    /// is not yet in the receiver's `OwnerDeviceCache`, so the production
    /// `OwnerDeviceCacheResolver` would return `None` and surface
    /// `UnknownActor`. That's the bootstrap-by-design case the inline
    /// pub is for. Pass it through directly here.
    ///
    /// The CRDT-layer verify (`VerifyContext.actor_identity_pub`) is
    /// unchanged — same code path, same expected_community_id binding,
    /// same admin / invite-only checks. Only resolver lookup is skipped.
    pub async fn insert_local_event_with_pubs(
        &self,
        event: crate::community_membership::SignedMembershipEvent,
        actor_identity_pub: [u8; 64],
        countersigner_identity_pub: Option<[u8; 64]>,
    ) -> Result<crate::community_state_crdt::InsertOutcome, LocalInsertError> {
        if event.community_id != self.community_id {
            return Err(LocalInsertError::WrongCommunity {
                expected: self.community_id,
                got: event.community_id,
            });
        }
        self.insert_event_with_resolved_pubs(event, actor_identity_pub, countersigner_identity_pub)
            .await
    }

    /// Shared body for `insert_local_event` and
    /// `insert_local_event_with_pubs` — runs the verify + state mutate +
    /// post-Inserted hook chain with already-resolved pubs.
    async fn insert_event_with_resolved_pubs(
        &self,
        event: crate::community_membership::SignedMembershipEvent,
        actor_pub: [u8; 64],
        countersigner_pub: Option<[u8; 64]>,
    ) -> Result<crate::community_state_crdt::InsertOutcome, LocalInsertError> {
        // Bind expected_community_id to the engine's configured value,
        // NOT the (caller-controlled) event payload. Without this, a
        // malicious or buggy IPC could bypass the cross-community routing
        // check by passing an event whose community_id matches what it
        // claims. The entry-point guard above gives a clearer error class
        // for the common honest mismatch case.
        let ctx = crate::community_membership::VerifyContext {
            expected_community_id: self.community_id,
            admin_addr: self.admin_addr,
            is_invite_only: self.is_invite_only,
            actor_identity_pub: &actor_pub,
            countersigner_identity_pub: countersigner_pub.as_ref(),
        };

        let outcome = {
            let mut state_g = self.state.lock().await;
            state_g.insert_event(event.clone(), &ctx)
        };

        if matches!(
            outcome,
            crate::community_state_crdt::InsertOutcome::Inserted
        ) {
            // ZEB-262 Phase 4: fire any redeem_invite oneshot waiting
            // on this event id BEFORE the delta-emit / notify_dirty
            // hooks. The send is sync; the lock is released before
            // we touch any other channel. Order doesn't affect
            // correctness — the oneshot, the delta channel, and the
            // publish-debounce all observe the same Inserted event —
            // but firing the oneshot first keeps the redeem_invite
            // IPC's wait-then-decode latency minimal.
            if let Some(pending) = self.pending_redemptions.as_ref() {
                notify_pending_redemption_in_map(pending, &event.id).await;
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

        let resolver = self
            .identity_resolver
            .as_ref()
            .ok_or(LocalInsertError::MissingIdentityResolver)?;

        // Resolve actor pubs for both events. Both resolves happen
        // BEFORE the lock — the lock hold is kept as short as possible.
        let first_actor_pub = resolver
            .resolve(&first.actor)
            .await
            .ok_or(LocalInsertError::UnknownActor(first.actor))?;
        let first_countersigner_pub = if let Some(cs) = first.countersig.as_ref() {
            resolver.resolve(&cs.signer).await
        } else {
            None
        };

        let second_actor_pub = resolver
            .resolve(&second.actor)
            .await
            .ok_or(LocalInsertError::UnknownActor(second.actor))?;
        let second_countersigner_pub = if let Some(cs) = second.countersig.as_ref() {
            resolver.resolve(&cs.signer).await
        } else {
            None
        };

        let first_ctx = crate::community_membership::VerifyContext {
            expected_community_id: self.community_id,
            admin_addr: self.admin_addr,
            is_invite_only: self.is_invite_only,
            actor_identity_pub: &first_actor_pub,
            countersigner_identity_pub: first_countersigner_pub.as_ref(),
        };
        let second_ctx = crate::community_membership::VerifyContext {
            expected_community_id: self.community_id,
            admin_addr: self.admin_addr,
            is_invite_only: self.is_invite_only,
            actor_identity_pub: &second_actor_pub,
            countersigner_identity_pub: second_countersigner_pub.as_ref(),
        };

        // C5: pre-validate BOTH events before any state mutation.
        // Acquires the lock once to run both verifications, then re-acquires
        // (under the same held guard) to insert. This collapses into a single
        // lock hold — the Mutex is held across verify + insert for both.
        let (first_outcome, second_outcome) = {
            let mut state_g = self.state.lock().await;

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
        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        let result = if self.shutdown_tx.send(resp_tx).await.is_ok() {
            resp_rx
                .await
                .map_err(|_| CommunitySyncError::TransportClosed)?
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
    flush_now_rx: mpsc::Receiver<tokio::sync::oneshot::Sender<Result<(), CommunitySyncError>>>,
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
}

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
                // Only persist after a SUCCESSFUL publish. Persisting
                // on failure would record next_hlc's tracker advance
                // even though peers never received the publish — a
                // restart would skip the retry and leave the community
                // out-of-sync until clock-time advances past the
                // unpersisted HLC. Errors here are logged + swallowed
                // (debounce wakeup has no caller to surface a Result
                // to; dropping the loop on a transient disk error
                // would silently disable sync for this community).
                if pub_result.is_ok() {
                    if let Err(e) = persist_both(&ctx).await {
                        tracing::warn!(
                            community_id = ?ctx.community_id,
                            error = %e,
                            "community persist_both failed after debounce publish"
                        );
                    }
                }
            }
            Some(resp_tx) = ctx.flush_now_rx.recv() => {
                next_wakeup = None;
                let was_dirty = ctx.has_pending_dirty.swap(false, Ordering::AcqRel);
                let pub_result = publish_root_now(&ctx).await;
                if pub_result.is_err() && was_dirty {
                    ctx.has_pending_dirty.store(true, Ordering::Release);
                }
                // Persist matches the public contract: flush_now()
                // returns after both publish AND on-disk persist
                // complete (mirrors owner_state_sync::SyncEngine). Only
                // persist on publish success to avoid recording an
                // unpublished HLC advance; on persist failure surface
                // the error to the caller via and()-chained Result.
                let final_result = if pub_result.is_ok() {
                    let persist_result = persist_both(&ctx).await;
                    pub_result.and(persist_result)
                } else {
                    pub_result
                };
                let _ = resp_tx.send(final_result);
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
            Some(resp_tx) = ctx.shutdown_rx.recv() => {
                // Final-flush only if the in-memory pending-dirty flag
                // says we owe peers a publish. Lock-relaxed is fine
                // because there's no concurrent mutator past this
                // point — we're in the shutdown branch.
                let was_dirty = ctx.has_pending_dirty.load(Ordering::Relaxed);
                let pub_result = if was_dirty {
                    publish_root_now(&ctx).await
                } else {
                    Ok(())
                };
                // Persist gating mirrors the debounce / flush_now arms:
                // only checkpoint state to disk after a SUCCESSFUL
                // publish. Persisting after a failed publish would
                // record next_hlc's tracker advance even though peers
                // never received the final root — on restart the in-
                // memory `has_pending_dirty` is gone, so there's no
                // signal to retry. The receive-side (no publish, just
                // a tracker advance from accepted inbound) is a
                // separate concern: persist runs on every successful
                // accept-and-merge in the subscriber arm, so by the
                // time we reach shutdown the on-disk replay tracker is
                // already up to date for accepted publishes.
                //
                // If we never even attempted a publish (was_dirty=false)
                // we still flush so any receive-side updates this loop
                // accepted but didn't yet persist (only possible if a
                // shutdown raced in between accept and persist) reach
                // disk. In practice the subscriber arm calls persist
                // before yielding back to select!, so this is a belt-
                // and-suspenders flush — cheap, safe, and visible in
                // tests.
                let final_result = if pub_result.is_ok() {
                    let persist_result = persist_both(&ctx).await;
                    pub_result.and(persist_result)
                } else {
                    pub_result
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
async fn live_epoch_key(
    community_id: SpaceId,
    crdt_state: Option<&Arc<Mutex<crate::owner_state_crdt::OwnerState>>>,
    fallback: &EpochKey,
) -> (EpochKey, Option<u64>) {
    if let Some(cs) = crdt_state {
        let guard = cs.lock().await;
        if let Some(space) = guard.spaces.get(&community_id) {
            if let (Some(k), Some(e)) = (space.current_epoch_key.clone(), space.current_epoch) {
                return (k, Some(e));
            }
        }
    }
    (fallback.clone(), None)
}

/// Snapshot the local CRDT, encrypt it, write to CAS, build a
/// `CommunityRootPublishPayload`, AEAD-wrap it for the wire, and ship
/// it on `publisher_tx`.
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
async fn publish_root_now(ctx: &InternalCtx) -> Result<(), CommunitySyncError> {
    use crate::owner_state_crypto::canonical_cbor_encode;
    use ed25519_dalek::Signer;

    // ZEB-249 §10.6 Phase A: read the live epoch key BEFORE snapshotting
    // community state so we never hold both locks at the same time
    // (lock-order: owner-state THEN community-state is forbidden; we
    // release owner-state lock here before acquiring community-state).
    let (current_key, current_epoch) = live_epoch_key(
        ctx.community_id,
        ctx.crdt_state.as_ref(),
        &ctx.membership_key,
    )
    .await;

    // Snapshot CRDT state under brief lock; drop guard before the
    // expensive encode + AEAD + CAS hops below.
    let snapshot = {
        let state = ctx.state.lock().await;
        state.clone()
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

    // 4. Put into ContentStore (routes through CasOp::PutLocal).
    ctx.content_store.put(root_cid, blob_ciphertext).await?;

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

    // 9. Send onto outbound channel — Zenoh adapter (Task 11) forwards.
    ctx.publisher_tx
        .send(wire)
        .await
        .map_err(|_| CommunitySyncError::TransportClosed)?;

    Ok(())
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
///    `payload.publisher_addr`; `None` → `UnknownPublisher`. Tracker
///    NOT advanced.
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
/// path.
async fn handle_incoming_publish(ctx: &InternalCtx, wire: Vec<u8>) -> IncomingOutcome {
    use crate::community_membership::MemberStatus;
    use crate::owner_state_crypto::canonical_cbor_encode;

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
                v.sort_by(|a, b| b.0.cmp(&a.0));
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
    let payload_bytes = {
        let mut result = decrypt_root_publish(&outer_current_key, &wire);
        if result.is_err() {
            for old_key in &outer_old_keys_rev {
                result = decrypt_root_publish(old_key, &wire);
                if result.is_ok() {
                    break;
                }
            }
        }
        match result {
            Ok(b) => b,
            Err(e) => return IncomingOutcome::ErrPreMutation(CommunitySyncError::Crypto(e)),
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
    {
        let state = ctx.state.lock().await;
        let events: Vec<SignedMembershipEvent> = state.events.values().cloned().collect();
        drop(state);
        let materialized =
            crate::community_membership::prior_state_at_hlc(&events, &payload.at, ctx.admin_addr);
        let member_state = materialized.members.get(&payload.publisher_addr).cloned();
        let status_now = member_state.as_ref().map(|s| s.status);
        let is_joined = matches!(status_now, Some(MemberStatus::Joined));
        if !is_joined {
            return IncomingOutcome::ErrPreMutation(CommunitySyncError::PublisherNotJoined {
                addr: payload.publisher_addr,
                // `None` from `members.get` is the catch-all "not
                // currently Joined per our prior-state-at-publish-HLC
                // view": the publisher was never a member, has not
                // yet had their Join propagate to us (cold cache /
                // out-of-band-bootstrap path), or `publisher_addr`
                // is fabricated. We collapse all three onto
                // `MemberStatus::Left` rather than introduce a fourth
                // variant — the error is diagnostic-only and the
                // security invariant ("not Joined → reject") is
                // unchanged. Frontend code that branches on this
                // field MUST treat `Left` + `left_at: None` as the
                // "never joined / unknown" case (genuine Leaves
                // always carry a `left_at`).
                status: status_now.unwrap_or(MemberStatus::Left),
                left_at: member_state.and_then(|s| s.left_at),
            });
        }
    }

    // 3. Resolve `publisher_addr` → identity_pub via `IdentityResolver`.
    //    Distinct from MissingIdentityResolver (config error) vs
    //    UnknownPublisher (resolver returned None for this addr — cold
    //    cache or fabricated addr). Tracker NOT advanced on either.
    let resolver = match ctx.identity_resolver.as_deref() {
        Some(r) => r,
        None => {
            return IncomingOutcome::ErrPreMutation(CommunitySyncError::MissingIdentityResolver);
        }
    };
    let publisher_pub = match resolver.resolve(&payload.publisher_addr).await {
        Some(p) => p,
        None => {
            return IncomingOutcome::ErrPreMutation(CommunitySyncError::UnknownPublisher {
                addr: payload.publisher_addr,
            });
        }
    };

    // 4. Verify Ed25519 signature over canonical CBOR of
    //    `CommunityRootSignedPayload::from(&payload)`.
    //
    //    Key→address binding: before consuming the resolved key we
    //    re-derive `address_hash` from the 64-byte identity public
    //    bytes and reject if it does not equal `payload.publisher_addr`.
    //    This is defense-in-depth against a buggy / stale resolver
    //    handing us the wrong identity for `publisher_addr` — without
    //    this check, a valid signature from key X would be accepted
    //    under a falsely-claimed address Y. Mirrors the binding step
    //    in `community_membership::verify_signature` (line 446) and
    //    `verify_countersig` (line 522), which use the same
    //    `harmony_identity::Identity::from_public_bytes` derivation.
    //
    //    Use `verify_strict` (not `verify`): strict mode rejects
    //    signatures with non-canonical S values and small-order R
    //    points (RFC 8032 strict subset), matching
    //    `community_membership::verify_signature` and
    //    `dm_envelope`'s posture for signed wire payloads.
    {
        let signed_bytes = match canonical_cbor_encode(&CommunityRootSignedPayload::from(&payload))
        {
            Ok(b) => b,
            Err(e) => {
                return IncomingOutcome::ErrPreMutation(CommunitySyncError::CborEncode(
                    e.to_string(),
                ));
            }
        };
        let identity = match harmony_identity::Identity::from_public_bytes(&publisher_pub) {
            Ok(i) if i.address_hash == payload.publisher_addr.0 => i,
            // Either the identity bytes are malformed OR the resolver
            // returned a key whose `address_hash` does not match the
            // claimed publisher_addr. Both cases collapse to the same
            // observable outcome: this publish is unauthenticated under
            // the claimed addr. Surface as `PublisherSigInvalid` so the
            // existing degraded-report path handles it (rather than
            // adding a separate "key did not bind to addr" variant —
            // the security invariant is identical).
            _ => {
                return IncomingOutcome::ErrPreMutation(CommunitySyncError::PublisherSigInvalid {
                    addr: payload.publisher_addr,
                });
            }
        };
        let sig = ed25519_dalek::Signature::from_bytes(&payload.publisher_sig);
        if identity
            .verifying_key
            .verify_strict(&signed_bytes, &sig)
            .is_err()
        {
            return IncomingOutcome::ErrPreMutation(CommunitySyncError::PublisherSigInvalid {
                addr: payload.publisher_addr,
            });
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
    //    ZEB-249 §10.6: use the epoch key selected from payload.epoch.
    //    If payload.epoch matches outer_current_key's epoch, we use that.
    //    Otherwise we try to find the key for that epoch in our old_epoch_keys.
    //    Fall back to the same trial-decrypt order used for the outer wire.
    let blob_cleartext = {
        // Try the outer key we used to decrypt the wire (most likely match).
        let mut result = decrypt_blob(&outer_current_key, &blob_ciphertext);
        if result.is_err() {
            for old_key in &outer_old_keys_rev {
                result = decrypt_blob(old_key, &blob_ciphertext);
                if result.is_ok() {
                    break;
                }
            }
        }
        match result {
            Ok(b) => b,
            Err(e) => return IncomingOutcome::ErrPreMutation(CommunitySyncError::Crypto(e)),
        }
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

    // Phase A: pre-resolve identity_pubs OUTSIDE the community state
    // lock. The resolver awaits owner_state's mutex; holding community
    // state at the same time would create a lock-order hazard with
    // Phase 3 IPC handlers that lock owner_state then community_state.
    // Skip-on-error logs + drops events with unknown actor / cs
    // identity_pubs; mirrors decrypt_inbox_entries (DM transport).
    //
    // **Replay order matters.** `BTreeMap<EventId, _>::into_values()`
    // walks in `EventId` byte order, but `insert_event` authorizes
    // each candidate against `prior_state_at_event` — i.e., everything
    // already in the local log that strictly precedes the candidate by
    // `event_sort_key`. If two events arrive in the same blob and the
    // later-by-replay-order event is processed first, its earlier
    // predecessor (still pending in our pre-resolve queue) is missing
    // from prior_state, and a valid event can land as `Rejected`. Sort
    // explicitly by `event_sort_key` so we merge in the same order
    // `materialize` would replay.
    let mut events_in_replay_order: Vec<SignedMembershipEvent> =
        remote.events.into_values().collect();
    events_in_replay_order.sort_by(|a, b| {
        crate::community_membership::event_sort_key(a)
            .cmp(&crate::community_membership::event_sort_key(b))
    });

    let mut resolved: Vec<(SignedMembershipEvent, [u8; 64], Option<[u8; 64]>)> = Vec::new();
    for event in events_in_replay_order {
        let actor_pub = match resolver.resolve(&event.actor).await {
            Some(p) => p,
            None => {
                tracing::warn!(
                    community_id = ?ctx.community_id,
                    actor = ?event.actor,
                    "skipping incoming event: unknown actor identity_pub"
                );
                continue;
            }
        };
        let cs_pub: Option<[u8; 64]> = match event.countersig.as_ref() {
            None => None,
            Some(cs) => match resolver.resolve(&cs.signer).await {
                Some(p) => Some(p),
                None => {
                    tracing::warn!(
                        community_id = ?ctx.community_id,
                        signer = ?cs.signer,
                        "skipping incoming event: unknown countersigner identity_pub"
                    );
                    continue;
                }
            },
        };
        resolved.push((event, actor_pub, cs_pub));
    }

    // Phase B: lock community state once, run insert_event for each
    // resolved event, collect rejections for out-of-lock reporting.
    let mut inserted_any = false;
    let mut rejection_reports: Vec<crate::community_membership::VerifyError> = Vec::new();
    // Buffer inserted-event clones for delta emission AFTER the state
    // lock is released. Same lock-discipline rationale as
    // `rejection_reports`: holding the state mutex across an
    // `mpsc::Sender::try_send` is technically non-blocking but keeping
    // the emit lock-free preserves the "no channel ops while holding
    // state" invariant the rest of this module follows.
    let mut inserted_events: Vec<SignedMembershipEvent> = Vec::new();
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
        {
            let events_now: Vec<SignedMembershipEvent> = state.events.values().cloned().collect();
            let mat_now = crate::community_membership::prior_state_at_hlc(
                &events_now,
                &payload.at,
                ctx.admin_addr,
            );
            let pub_state = mat_now.members.get(&payload.publisher_addr).cloned();
            let pub_status = pub_state.as_ref().map(|s| s.status);
            if !matches!(pub_status, Some(MemberStatus::Joined)) {
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

        for (event, actor_pub, cs_pub_owned) in resolved {
            if state.events.contains_key(&event.id) {
                continue;
            }
            // Inline `Option::as_ref` because rustc can't always infer
            // the right `AsRef` impl on `[u8; 64]`.
            let cs_pub_ref: Option<&[u8; 64]> = match &cs_pub_owned {
                Some(p) => Some(p),
                None => None,
            };
            let ctx_v = VerifyContext {
                expected_community_id: ctx.community_id,
                admin_addr: ctx.admin_addr,
                is_invite_only: ctx.is_invite_only,
                actor_identity_pub: &actor_pub,
                countersigner_identity_pub: cs_pub_ref,
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
                    // Skip — already in our log. Don't flip inserted_any
                    // because the CRDT is unchanged; without this, every
                    // duplicate Zenoh fanout echo would trigger a
                    // disk-persist on the Mutated arm at Task 10.
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

    // ZEB-262 Phase 4: fire any redeem_invite oneshots waiting on
    // events that were Inserted in this merge cycle. The notify path
    // shares the lock-and-fire helper with `insert_local_event`; the
    // map lock is taken once per inserted id (each call is independent)
    // and dropped before any `.await` (the helper's send is sync). We
    // borrow `&event.id` here so the subsequent delta-emit loop can
    // still consume `inserted_events` by value.
    if let Some(pending) = ctx.pending_redemptions.as_ref() {
        for event in &inserted_events {
            notify_pending_redemption_in_map(pending, &event.id).await;
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
async fn persist_both(ctx: &InternalCtx) -> Result<(), CommunitySyncError> {
    // Snapshot under locks — clones are cheap (CRDT is a BTreeMap of
    // signed events, tracker is a small per-device map), and far
    // cheaper than holding a lock across blocking I/O.
    let state_snap = ctx.state.lock().await.clone();
    let tracker_snap = ctx.tracker.lock().await.clone();
    let crdt_path = ctx.paths.crdt.clone();
    let replay_path = ctx.paths.replay.clone();
    tokio::task::spawn_blocking(move || -> Result<(), CommunitySyncError> {
        save_crdt(&crdt_path, &state_snap)
            .map_err(|e| CommunitySyncError::Persist(e.to_string()))?;
        save_replay(&replay_path, &tracker_snap)
            .map_err(|e| CommunitySyncError::Persist(e.to_string()))?;
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
        save_replay(&replay_path, &tracker_snap)
            .map_err(|e| CommunitySyncError::Persist(e.to_string()))
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
        CommunitySyncError::MisroutedBlob { .. } => "misrouted_blob",
        CommunitySyncError::MissingIdentityResolver => "missing_identity_resolver",
        CommunitySyncError::PublisherNotJoined { .. } => "publisher_not_joined",
        CommunitySyncError::UnknownPublisher { .. } => "publisher_unknown",
        CommunitySyncError::PublisherSigInvalid { .. } => "publisher_sig_invalid",
    }
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

    /// Local Ed25519 signing key, shared across every spawned engine.
    /// Wrapped in `Arc` so engine spawns are cheap (Arc bump, no
    /// secret-byte copy). Sourced from the local `PrivateIdentity` at
    /// `start_node` time; identical handle to the one Phase 3's
    /// `insert_local_event` uses for membership-event signing.
    pub signing_key: Arc<ed25519_dalek::SigningKey>,

    /// ZEB-249 §10.6 (Phase A): optional reference to the owner-state
    /// CRDT. When `Some`, every spawned engine receives a clone of this
    /// Arc so `publish_root_now` / `handle_incoming_publish` can read
    /// the live epoch key. `None` for tests that use the spawn-time key.
    pub crdt_state: Option<Arc<Mutex<crate::owner_state_crdt::OwnerState>>>,
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
}

// ── CommunitySyncSpawnGuard (ZEB-274) ─────────────────────────────────────────

/// RAII rollback guard for a freshly-spawned community-sync engine.
/// Held by an IPC handler across the critical section between
/// `spawn_engine_with_guard` and the durable `apply_space` commit.
///
/// Drop without explicit `commit()` or `abort()` triggers a
/// `Handle::try_current()` safety-net that calls
/// `shutdown_engine_and_cleanup_persistence` (only if THIS call
/// freshly created the engine — concurrent-redeem race losers per
/// ZEB-260 PR #90 round-5 are no-ops on Drop).
///
/// **No-runtime fallback:** unlike ZEB-271's `CommunityTransactionGuard`
/// (whose `abort_transaction_internal` is sync map cleanup),
/// `shutdown_engine_and_cleanup_persistence` is fundamentally async
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

    /// Explicit rollback. Calls `shutdown_engine_and_cleanup_persistence`
    /// if `freshly_created`. Sets `completed = true` so `Drop` is a
    /// no-op. Sync entry point but spawns the async cleanup as a tokio
    /// task internally (mirrors ZEB-271 `CommunityTransactionGuard::abort`
    /// shape). If no tokio runtime is present, logs a warn and accepts
    /// the leak (per spec §10.2 — no sync alternative for
    /// `engine.shutdown().await`).
    pub fn abort(self) {
        if self.freshly_created {
            match tokio::runtime::Handle::try_current() {
                Ok(handle) => {
                    let registry = std::sync::Arc::clone(&self.registry);
                    let community_id = self.community_id;
                    handle.spawn(async move {
                        if let Err(e) = registry
                            .shutdown_engine_and_cleanup_persistence(&community_id)
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
                    handle.spawn(async move {
                        if let Err(e) = registry
                            .shutdown_engine_and_cleanup_persistence(&community_id)
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
        Self {
            cfg: Arc::new(cfg),
            engines: tokio::sync::Mutex::new(BTreeMap::new()),
            crdt_state,
            pending_redemptions: std::sync::Arc::new(tokio::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
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
    ///      `shutdown_engine_and_cleanup_persistence` to undo the spawn.
    ///      Returns Err. Guard's `freshly_created` flag is NEVER set to
    ///      true (so Drop is a no-op).
    ///   4. On full success, set `guard.freshly_created = true` (or false
    ///      for the idempotent path) and return Ok(engine).
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

        // Step 1: spawn the engine via the inner helper.
        let freshly_created = self
            .spawn_engine_inner_now(
                community_id,
                membership_key,
                admin_addr,
                is_invite_only,
                publisher_tx,
                subscriber_rx,
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

        // Step 2: if fresh, dispatch the adapter request.
        if freshly_created {
            if let Err(send_err) =
                community_adapter_tx.try_send(crate::event_loop::CommunityAdapterRequest {
                    id_hex: hex::encode(community_id.0),
                    publisher_rx,
                    subscriber_tx,
                })
            {
                // Step 3: try_send failed → undo the spawn inline.
                match self
                    .shutdown_engine_and_cleanup_persistence(&community_id)
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

    /// Register a oneshot to fire when the `SignedMembershipEvent` with
    /// `event_id` is Inserted into any engine in this registry. Replaces
    /// any existing oneshot for the same `event_id` (the prior sender is
    /// dropped, which the prior caller's `.await` on the receiver
    /// surfaces as `Err(RecvError)` — interpret as "redemption
    /// superseded"). v1 doesn't deduplicate registrations because the
    /// caller pattern (one `redeem_invite` IPC = one fresh `event_id`)
    /// keeps the map naturally sparse.
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
    pub async fn shutdown_engine_and_cleanup_persistence(
        &self,
        community_id: &SpaceId,
    ) -> Result<(), CommunitySyncError> {
        // Phase 1: stop the engine task (idempotent on missing).
        self.stop_engine(community_id).await?;

        // Phase 2: remove the per-community persistence directory.
        // `tokio::fs::remove_dir_all` so we don't park a worker thread
        // on the synchronous `std::fs::remove_dir_all`.
        let dir = self
            .cfg
            .identity_dir
            .join("communities")
            .join(hex::encode(community_id.0));
        if dir.exists() {
            tokio::fs::remove_dir_all(&dir)
                .await
                .map_err(|e| CommunitySyncError::Persist(format!("remove_dir_all {dir:?}: {e}")))?;
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
    pub async fn spawn_engine_inner_now(
        &self,
        community_id: SpaceId,
        membership_key: EpochKey,
        admin_addr: OwnerAddr,
        is_invite_only: bool,
        publisher_tx: mpsc::Sender<Vec<u8>>,
        subscriber_rx: mpsc::Receiver<Vec<u8>>,
    ) -> Result<bool, CommunitySyncError> {
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
        }));

        engines.insert(community_id, engine);
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
    /// resolve to `None` and fail `LocalInsertError::UnknownActor`.
    /// CodeRabbit MAJOR finding on PR #87 round 2 (and the
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

        crate::community_channel_log::CommunityStateSnapshot {
            channel,
            author_power,
        }
    }
}

/// Adapts a generic `Arc<dyn IdentityResolver>` (the resolver shape
/// already used by the per-community sync engine) to the channel-log
/// `ChannelIdentityResolver` trait. Both expose the same `OwnerAddr →
/// 64-byte composite (X25519 || Ed25519)` lookup; this wrapper exists
/// only because the two traits live in different modules and can't
/// directly impl each other without one depending on the other's crate.
pub struct ChannelIdentityResolverAdapter {
    pub inner: Arc<dyn IdentityResolver>,
}

#[async_trait::async_trait]
impl crate::community_channel_log::ChannelIdentityResolver for ChannelIdentityResolverAdapter {
    async fn resolve(&self, addr: &crate::owner_state_types::OwnerAddr) -> Option<[u8; 64]> {
        self.inner.resolve(addr).await
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
            .shutdown_engine_and_cleanup_persistence(&community_id)
            .await
            .expect("explicit cleanup for test isolation");
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
        let zero_hlc = Hlc {
            wall_ms: 0,
            logical: 0,
            device_id: "t".into(),
        };
        Space {
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
            current_epoch: Some(epoch),
            current_epoch_key: Some(key),
            old_epoch_keys: std::collections::BTreeMap::new(),
            admin_addr: Some(OwnerAddr([0xbb; 16])),
            is_invite_only: Some(false),
        }
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
        };
        let err = encrypt_for_topic(&space, b"test payload")
            .expect_err("encrypt_for_topic must return Err on missing epoch key, not panic");
        assert!(
            matches!(err, EpochError::MissingEpochState),
            "expected MissingEpochState, got {err:?}"
        );
    }
}
