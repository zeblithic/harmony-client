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
use crate::owner_state_types::{Hlc, MembershipKey, OwnerAddr, SpaceId};

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
/// `MembershipKey`. Random 12-byte nonce prepended to the ciphertext;
/// receiver splits and verifies via ChaCha20-Poly1305 AAD binding.
///
/// Random nonce is correct here (every publish is a distinct wire
/// packet — we WANT freshness; replay protection is the receiver's
/// `RootHlcTracker`, not nonce reuse).
pub fn encrypt_root_publish(
    mk: &MembershipKey,
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
pub fn decrypt_root_publish(
    mk: &MembershipKey,
    wire: &[u8],
) -> Result<Vec<u8>, CommunityCryptoError> {
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
pub fn encrypt_blob(mk: &MembershipKey, plaintext: &[u8]) -> Result<Vec<u8>, CommunityCryptoError> {
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
pub fn decrypt_blob(mk: &MembershipKey, wire: &[u8]) -> Result<Vec<u8>, CommunityCryptoError> {
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

/// State-root publish payload for a community. Sent over
/// `harmony/community/{id_hex}/state-root-v1` after AEAD-encryption
/// via `encrypt_root_publish`. Receivers fetch `root_cid` from CAS
/// to retrieve the encrypted CommunityState blob, then decrypt with
/// `decrypt_blob`.
///
/// Wire format: 2-key CBOR map. Both field codes are 2 chars
/// (`rc` + `at`) to satisfy the same-length-keys invariant at this
/// nesting level. The HLC `at` is the publisher's monotonic counter
/// — receivers' RootHlcTrackers reject anything not strictly newer
/// per (publisher_device_id, hlc) (replay protection; mirrors
/// `crate::owner_state_types::RootPublishPayload`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommunityRootPublishPayload {
    /// Content-ID of the encrypted CommunityState blob in the
    /// shared ContentStore.
    #[serde(rename = "rc")]
    pub root_cid: ContentId,
    /// Publisher's HLC at publish time. Monotonically increasing per
    /// device_id; receivers track per-device latest-seen.
    #[serde(rename = "at")]
    pub at: Hlc,
}

impl CanonicalPayloadSealed for CommunityRootPublishPayload {}
impl CanonicalPayload for CommunityRootPublishPayload {}

/// Default debounce window between a `notify_dirty` and the resulting
/// state-root publish. Mirrors `owner_state_sync::DEFAULT_DEBOUNCE_MS`
/// (250 ms) — small enough to feel near-instant to a human, large
/// enough to collapse keystroke-rate mutations into one publish.
pub const DEFAULT_DEBOUNCE_MS: u64 = 250;

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
    /// Engine config has `identity_resolver: None`, so receive-side
    /// `verify_event` can't resolve identity_pubs. Distinct error
    /// class because the cause is configuration (Sub-A's owner-device
    /// cache wasn't wired in), not transport or crypto failure.
    #[error("no identity resolver configured — Phase 2 receive-side verify needs one")]
    MissingIdentityResolver,
}

/// Per-publisher-device latest-accepted HLC. Mirrors owner_state_sync's
/// in-memory replay tracker shape, but keyed externally by community_id
/// (one tracker instance per joined community).
///
/// `Serialize` / `Deserialize` are derived so Task 10's
/// `community_state_persist::save_replay` can canonical-CBOR-encode the
/// tracker to `replay.cbor`. The single `per_device` field is a
/// `BTreeMap<String, Hlc>` — both inner types already round-trip through
/// canonical CBOR, so no custom field renames are needed.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct CommunityRootHlcTracker {
    /// Per-publisher-device latest-accepted HLC. New incoming root
    /// publishes are accepted only if STRICTLY NEWER per their
    /// device_id key.
    pub per_device: BTreeMap<String, Hlc>,
}

impl CanonicalPayloadSealed for CommunityRootHlcTracker {}
impl CanonicalPayload for CommunityRootHlcTracker {}

impl CommunityRootHlcTracker {
    /// Test the candidate HLC against the per-device latest. Returns
    /// `true` if the candidate strictly dominates the recorded entry
    /// (or there is none); `false` otherwise.
    ///
    /// Does NOT mutate — `record` is a separate step the caller invokes
    /// after the rest of the receive pipeline succeeds. The split
    /// implements the "advance-after-success" idiom that owner-state's
    /// call sites apply manually to a bare BTreeMap.
    pub fn would_accept(&self, candidate: &Hlc) -> bool {
        match self.per_device.get(&candidate.device_id) {
            None => true,
            Some(prev) => candidate.is_strictly_newer_than(prev),
        }
    }

    /// Record `candidate` as the latest-accepted HLC for its device.
    ///
    /// Precondition: caller MUST have just verified `would_accept`
    /// returned `true`. We `debug_assert!` the precondition so a
    /// buggy call site surfaces in dev/test rather than silently
    /// no-opping (which would mask the bug). In release builds the
    /// insert is unconditional — at this point the caller has
    /// committed to advancing and a backward-jump indicates upstream
    /// state corruption that no amount of guarding here can repair.
    pub fn record(&mut self, candidate: Hlc) {
        debug_assert!(
            self.would_accept(&candidate),
            "CommunityRootHlcTracker::record called without would_accept check; backward-jump for device {}",
            candidate.device_id
        );
        let device_id = candidate.device_id.clone();
        self.per_device.insert(device_id, candidate);
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

/// Construction-time config bag for `CommunitySyncEngine::new`. Bundles
/// the per-community key + identity, the shared CRDT + tracker arcs,
/// the wire channels, the persist paths, and the optional degraded-path
/// reporter. Bag form keeps the constructor signature manageable — the
/// owner-state engine has 9 positional args and is already at the limit.
pub struct CommunitySyncEngineConfig {
    pub community_id: SpaceId,
    pub membership_key: MembershipKey,
    pub admin_addr: OwnerAddr,
    /// Whether this community requires invite-only counter-sigs on
    /// non-admin Joins. Plumbed into `VerifyContext` at receive time
    /// (Task 8 consumes this). Defaults to `false` for tests that
    /// don't exercise the invite-only path.
    pub is_invite_only: bool,
    pub device_id: String,
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
    /// Admin OwnerAddr for the community. Retained so future read-side
    /// `materialize()` callers (Phase 3 IPC) can construct a
    /// `VerifyContext` without having to thread `admin_addr` through
    /// the registry separately.
    admin_addr: OwnerAddr,
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
        let admin_addr = cfg.admin_addr;

        let task = tokio::spawn(internal_task(InternalCtx {
            community_id: cfg.community_id,
            membership_key: cfg.membership_key,
            admin_addr: cfg.admin_addr,
            is_invite_only: cfg.is_invite_only,
            device_id: cfg.device_id,
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
        }));

        Self {
            notify_dirty,
            has_pending_dirty,
            flush_now_tx,
            shutdown_tx,
            task: Mutex::new(Some(task)),
            state: state_for_engine,
            admin_addr,
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

    /// Returns the admin `OwnerAddr` this engine was configured with.
    /// Retained so Phase 3's IPC handlers can rebuild a `VerifyContext`
    /// for read-side `materialize()` without re-plumbing the field
    /// through the registry. Currently unused outside that future
    /// callsite; the integration tests don't read it.
    #[doc(hidden)]
    pub fn admin_addr(&self) -> OwnerAddr {
        self.admin_addr
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
    membership_key: MembershipKey,
    admin_addr: OwnerAddr,
    is_invite_only: bool,
    device_id: String,
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
                    )
                    .await;
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
                    )
                    .await;
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
                let pub_result = if ctx.has_pending_dirty.load(Ordering::Relaxed) {
                    publish_root_now(&ctx).await
                } else {
                    Ok(())
                };
                // Always flush on shutdown — we can't cheaply tell
                // from outside whether the CRDT mutated since the
                // last persist, and losing the final disk flush
                // silently corrupts next-boot replay (the in-memory
                // tracker would advance again on a re-broadcast and
                // miss whatever we accepted in this session).
                let persist_result = persist_both(&ctx).await;
                // `pub_result.and(persist_result)` returns Ok only if
                // BOTH steps succeeded; otherwise the first Err
                // surfaces. Persist failures must reach the caller —
                // suppressing them mirrors the same silent-corruption
                // failure mode `owner_state_sync` explicitly rejects.
                let _ = resp_tx.send(pub_result.and(persist_result));
                return;
            }
        }
    }
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
    let blob_ciphertext = encrypt_blob(&ctx.membership_key, &blob_cleartext)?;

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

    // 5. Build state-root payload with a strictly-newer HLC.
    let now = next_hlc(ctx).await;
    let payload = CommunityRootPublishPayload { root_cid, at: now };
    let payload_bytes = canonical_cbor_encode(&payload)
        .map_err(|e| CommunitySyncError::CborEncode(e.to_string()))?;

    // 6. Encrypt with random-nonce root AEAD (every publish is fresh).
    let wire = encrypt_root_publish(&ctx.membership_key, &payload_bytes)?;

    // 7. Send onto outbound channel — Zenoh adapter (Task 11) forwards.
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
    // Read once — both the logical-counter computation and the
    // effective-wall pin need the same `prev`. `saturating_add` on
    // logical bounds pathological wraparound under sustained backward
    // NTP correction at the cost of a stuck publisher (which the
    // receiver will reject) — preferable to silent replay.
    let prev = tracker.per_device.get(&ctx.device_id).cloned();
    let (logical, prev_wall) = match prev.as_ref() {
        Some(p) if p.wall_ms == wall_ms => (p.logical.saturating_add(1), p.wall_ms),
        Some(p) if p.wall_ms > wall_ms => (p.logical.saturating_add(1), p.wall_ms),
        Some(p) => (0, p.wall_ms),
        None => (0, 0),
    };
    let effective_wall = std::cmp::max(wall_ms, prev_wall);
    let now = Hlc {
        wall_ms: effective_wall,
        logical,
        device_id: ctx.device_id.clone(),
    };
    tracker.record(now.clone());
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
    /// `persist_replay_only`.
    fn crdt_mutated(&self) -> bool {
        matches!(self, Self::Mutated | Self::ErrPostMutation(_))
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
/// Pipeline:
/// 1. Decrypt the wire packet (random-nonce + AAD).
/// 2. Decode `CommunityRootPublishPayload`.
/// 3. Replay-check via `tracker.would_accept` (early-exit Duplicate).
/// 4. Fetch the encrypted blob from CAS (cache miss → ErrPreMutation).
/// 5. Decrypt the blob (deterministic-nonce).
/// 6. Decode `CommunityState`.
/// 7. Misrouted-blob check: `remote.community_id == ctx.community_id`.
/// 8. Advance `tracker` — single mutation point. Subsequent failures
///    are ErrPostMutation so the caller persists tracker advance.
/// 9. For each event: skip-if-known; resolve actor + countersigner
///    identity_pubs (skip-on-error); call `state.insert_event` with a
///    fresh `VerifyContext`; surface `Rejected` outcomes as
///    `CommunityDegradedReport` on `error_tx`.
///
/// **Divergence from `owner_state_sync::handle_incoming_publish`:**
/// owner-state advances the tracker IMMEDIATELY after the replay-check
/// (so blob-fetch / decrypt / decode failures land as
/// `ErrPostMutation`). We delay the advance until step 8 — AFTER the
/// blob has been fetched, decrypted, decoded, AND passed the
/// misrouted-blob check. The rationale is asymmetric trust: a
/// misrouted blob (foreign community's state surfaced under our
/// CID) means the publisher's HLC carries no useful information for
/// OUR replay tracker, so advancing it would let a correctly-routed
/// re-publish at the same HLC be silently dropped. owner-state
/// doesn't have this concern because there's only one owner-CRDT
/// per identity.
async fn handle_incoming_publish(ctx: &InternalCtx, wire: Vec<u8>) -> IncomingOutcome {
    // 1. Decrypt root publish.
    let payload_bytes = match decrypt_root_publish(&ctx.membership_key, &wire) {
        Ok(b) => b,
        Err(e) => return IncomingOutcome::ErrPreMutation(CommunitySyncError::Crypto(e)),
    };
    let payload: CommunityRootPublishPayload = match canonical_cbor_decode(&payload_bytes) {
        Ok(p) => p,
        Err(e) => {
            return IncomingOutcome::ErrPreMutation(CommunitySyncError::CborDecode(e.to_string()))
        }
    };

    // 2. Replay-protect via per-community RootHlcTracker. Read-only —
    //    the `record` step happens after the rest of the receive
    //    pipeline succeeds (single state-mutation point at step 8).
    {
        let tracker = ctx.tracker.lock().await;
        if !tracker.would_accept(&payload.at) {
            return IncomingOutcome::Duplicate;
        }
    }

    // 3. Fetch the encrypted blob from CAS. Cache-miss is a pre-mutation
    //    failure — the publish carries a CID we couldn't resolve in
    //    time; CRDT eventual consistency lets the next state-root from
    //    any peer recover.
    let blob_ciphertext = match ctx.content_store.get(&payload.root_cid).await {
        Ok(Some(b)) => b,
        Ok(None) => {
            return IncomingOutcome::ErrPreMutation(CommunitySyncError::ContentStore(
                crate::content_store::ContentStoreError::Io(format!(
                    "missing root blob for cid {:?} (fetch timeout or admit-rejected)",
                    payload.root_cid
                )),
            ));
        }
        Err(e) => return IncomingOutcome::ErrPreMutation(CommunitySyncError::ContentStore(e)),
    };

    // 4. Decrypt blob (deterministic-nonce).
    let blob_cleartext = match decrypt_blob(&ctx.membership_key, &blob_ciphertext) {
        Ok(b) => b,
        Err(e) => return IncomingOutcome::ErrPreMutation(CommunitySyncError::Crypto(e)),
    };

    // 5. Decode CommunityState.
    let remote: CommunityState = match canonical_cbor_decode(&blob_cleartext) {
        Ok(s) => s,
        Err(e) => {
            return IncomingOutcome::ErrPreMutation(CommunitySyncError::CborDecode(e.to_string()))
        }
    };

    // 5b. Reject misrouted blob: blob's community_id must match the
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

    // 6. Advance the replay tracker BEFORE merging events. This is
    //    the single state-mutation point — if any subsequent step
    //    fails, we mark the outcome ErrPostMutation so the caller
    //    persists tracker advance to disk (preventing replay on
    //    next-boot).
    {
        let mut tracker = ctx.tracker.lock().await;
        tracker.record(payload.at.clone());
    }

    // 7. Merge events. Each event must re-verify against B's
    //    prior_state_at_event — we don't trust A's verification.
    let Some(resolver) = ctx.identity_resolver.as_deref() else {
        // Phase 2's receive-side verify needs an identity resolver
        // to map event.actor → identity_pub. None means we can't
        // verify any incoming event. Tracker already advanced;
        // surface as ErrPostMutation so persist hook still runs.
        return IncomingOutcome::ErrPostMutation(CommunitySyncError::MissingIdentityResolver);
    };

    // Phase A: pre-resolve identity_pubs OUTSIDE the community state
    // lock. The resolver awaits owner_state's mutex; holding community
    // state at the same time would create a lock-order hazard with
    // Phase 3 IPC handlers that lock owner_state then community_state.
    // Skip-on-error logs + drops events with unknown actor / cs
    // identity_pubs; mirrors decrypt_inbox_entries (DM transport).
    let mut resolved: Vec<(SignedMembershipEvent, [u8; 64], Option<[u8; 64]>)> = Vec::new();
    for event in remote.events.into_values() {
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
    {
        let mut state = ctx.state.lock().await;
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
            match state.insert_event(event, &ctx_v) {
                InsertOutcome::Inserted => {
                    inserted_any = true;
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
        )
        .await;
    }

    // The tracker advanced (step 6) regardless of whether any event
    // was Inserted. Differentiate so Task 10 can persist the smaller
    // replay.cbor file alone when the CRDT is unchanged. See the
    // `IncomingOutcome` doc comments for the full rationale.
    if inserted_any {
        IncomingOutcome::Mutated
    } else {
        IncomingOutcome::MutatedTrackerOnly
    }
}

/// Snapshot the CRDT and replay tracker to disk. Locks held briefly:
/// state lock for `save_crdt`, then dropped before re-locking the
/// tracker for `save_replay`. The interleave matters — holding both
/// locks across both saves would force every concurrent CRDT mutation
/// (foreground command handlers) to wait through two fsyncs.
///
/// Both saves are atomic-rename-via-tempfile, so a partial save can't
/// corrupt the live file. Failures bubble up as
/// `CommunitySyncError::Persist` so the shutdown arm can surface them
/// to the caller; the wakeup / merge arms log + continue.
async fn persist_both(ctx: &InternalCtx) -> Result<(), CommunitySyncError> {
    let state = ctx.state.lock().await;
    save_crdt(&ctx.paths.crdt, &state).map_err(|e| CommunitySyncError::Persist(e.to_string()))?;
    drop(state);
    let tracker = ctx.tracker.lock().await;
    save_replay(&ctx.paths.replay, &tracker)
        .map_err(|e| CommunitySyncError::Persist(e.to_string()))?;
    Ok(())
}

/// Replay-only persist for the `MutatedTrackerOnly` case — every event
/// in the remote blob was `AlreadyKnown` but the tracker advanced. The
/// CRDT is byte-identical, so re-fsyncing `crdt.cbor` would be wasted
/// I/O on every duplicate-but-clock-advanced publish. Only `replay.cbor`
/// rewrites here.
async fn persist_replay_only(ctx: &InternalCtx) -> Result<(), CommunitySyncError> {
    let tracker = ctx.tracker.lock().await;
    save_replay(&ctx.paths.replay, &tracker)
        .map_err(|e| CommunitySyncError::Persist(e.to_string()))?;
    Ok(())
}

/// Send a `CommunityDegradedReport` if `error_tx` is wired. Helper for
/// the three emit sites in `internal_task` and `handle_incoming_publish`
/// — they all `if let Some(tx) ... let _ = tx.send(...)` the same shape,
/// and Task 13 will add a fourth site for start_node-level reporting.
async fn report_degraded(
    error_tx: Option<&mpsc::Sender<CommunityDegradedReport>>,
    community_id: SpaceId,
    reason_tag: &'static str,
    detail: String,
) {
    if let Some(tx) = error_tx {
        let _ = tx
            .send(CommunityDegradedReport {
                community_id,
                reason_tag,
                detail,
            })
            .await;
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
        CommunitySyncError::TransportClosed => "transport_closed",
        CommunitySyncError::Persist(_) => "persist_failed",
        CommunitySyncError::MisroutedBlob { .. } => "misrouted_blob",
        CommunitySyncError::MissingIdentityResolver => "missing_identity_resolver",
    }
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
}

impl CommunitySyncRegistry {
    pub fn new(cfg: CommunityRegistryConfig) -> Self {
        Self {
            cfg: Arc::new(cfg),
            engines: tokio::sync::Mutex::new(BTreeMap::new()),
        }
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
    /// **Lock scope:** the engines map lock is held across the disk
    /// I/O (`load_crdt` + `load_replay`), the `CommunitySyncEngine::new`
    /// call, AND the `tokio::spawn` of the engine's internal task that
    /// `new` performs. Concurrent `spawn_engine` calls for distinct
    /// communities will serialise on this lock — acceptable because
    /// spawn is rare (once per Joined event), and holding the lock
    /// through engine construction is the only way to keep the
    /// contains-key check race-free against another spawn for the
    /// same community.
    pub async fn spawn_engine(
        &self,
        community_id: SpaceId,
        membership_key: MembershipKey,
        admin_addr: OwnerAddr,
        is_invite_only: bool,
        publisher_tx: mpsc::Sender<Vec<u8>>,
        subscriber_rx: mpsc::Receiver<Vec<u8>>,
    ) -> Result<(), CommunitySyncError> {
        let mut engines = self.engines.lock().await;
        if engines.contains_key(&community_id) {
            // Idempotent — re-spawn is a no-op rather than an error
            // so the registry tolerates duplicate add events from
            // owner-state mutations.
            return Ok(());
        }

        let paths = self.paths_for(community_id);
        let initial_state = load_crdt(&paths.crdt, community_id)
            .map_err(|e| CommunitySyncError::Persist(e.to_string()))?;
        let initial_tracker =
            load_replay(&paths.replay).map_err(|e| CommunitySyncError::Persist(e.to_string()))?;

        let state = Arc::new(Mutex::new(initial_state));
        let tracker = Arc::new(Mutex::new(initial_tracker));

        let engine = Arc::new(CommunitySyncEngine::new(CommunitySyncEngineConfig {
            community_id,
            membership_key,
            admin_addr,
            is_invite_only,
            device_id: self.cfg.device_id.clone(),
            state,
            tracker,
            content_store: Arc::clone(&self.cfg.content_store),
            publisher_tx,
            subscriber_rx,
            paths,
            debounce_ms: self.cfg.debounce_ms,
            identity_resolver: Some(Arc::clone(&self.cfg.identity_resolver)),
            error_tx: self.cfg.error_tx.clone(),
        }));

        engines.insert(community_id, engine);
        Ok(())
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
}

impl OwnerDeviceCacheResolver {
    pub fn new(cache: Arc<Mutex<crate::owner_state_crdt::OwnerState>>) -> Self {
        Self { cache }
    }
}

#[async_trait::async_trait]
impl IdentityResolver for OwnerDeviceCacheResolver {
    async fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
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
