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
//! flush_now → force-publish, shutdown → final flush); the subscriber
//! arm is still a stub (Task 8 fills in handle_incoming_publish).
//! Subsequent tasks add persistence flushes and the registry.

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

use crate::community_state_crdt::CommunityState;
use crate::content_store::ContentStore;
use crate::owner_state_crypto::{sealed::CanonicalPayloadSealed, CanonicalPayload};
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
}

/// Per-publisher-device latest-accepted HLC. Mirrors owner_state_sync's
/// in-memory replay tracker shape, but keyed externally by community_id
/// (one tracker instance per joined community).
#[derive(Debug, Default, Clone)]
pub struct CommunityRootHlcTracker {
    /// Per-publisher-device latest-accepted HLC. New incoming root
    /// publishes are accepted only if STRICTLY NEWER per their
    /// device_id key.
    pub per_device: BTreeMap<String, Hlc>,
}

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
/// snapshots. Task 10 replaces this in-module type with a re-export
/// from `community_state_persist`; kept inline for now so the engine
/// scaffold can land before the persist layer.
#[derive(Debug, Clone)]
pub struct PersistPaths {
    pub crdt: PathBuf,
    pub replay: PathBuf,
}

/// Resolves an `OwnerAddr` -> 64-byte identity_pub at receive-side
/// `verify_event` time. Production implementation wraps Sub-A's
/// owner-device cache (Task 13's `OwnerDeviceCacheResolver`); tests
/// use a static mapping. The trait is declared at Task 6 so the
/// `CommunitySyncEngineConfig::identity_resolver` field can reference
/// it; concrete implementations (other than test stubs) land in later
/// tasks.
pub trait IdentityResolver: Send + Sync {
    fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]>;
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
        }
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

/// Internal context bag passed to the spawned task. Task 7's loop
/// reads most fields; `paths`, `is_invite_only`, `admin_addr`,
/// `identity_resolver`, and `error_tx` are still unread pending
/// Tasks 8/10/13 (persist hook, verify-on-receive, degraded-path
/// reporter). `#[allow(dead_code)]` stays until those tasks land —
/// removing fields just because the current task doesn't read them
/// would force a churn cycle when the next task adds them back.
#[allow(dead_code)]
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
                // Persistence is unimplemented until Task 10 — there
                // is no on-disk state at all yet, so each publish is
                // memory-only on the publisher side.
            }
            Some(resp_tx) = ctx.flush_now_rx.recv() => {
                next_wakeup = None;
                let was_dirty = ctx.has_pending_dirty.swap(false, Ordering::AcqRel);
                let pub_result = publish_root_now(&ctx).await;
                if pub_result.is_err() && was_dirty {
                    ctx.has_pending_dirty.store(true, Ordering::Release);
                }
                let _ = resp_tx.send(pub_result);
            }
            maybe_bytes = ctx.subscriber_rx.recv(), if !inbound_closed => {
                let Some(_bytes) = maybe_bytes else {
                    tracing::error!(
                        community_id = ?ctx.community_id,
                        "community subscriber channel closed; sync inbound disabled"
                    );
                    inbound_closed = true;
                    continue;
                };
                // Task 8 fills in handle_incoming_publish. For now we
                // drop the bytes — the engine stays alive in
                // publish-only mode.
            }
            Some(resp_tx) = ctx.shutdown_rx.recv() => {
                let pub_result = if ctx.has_pending_dirty.load(Ordering::Relaxed) {
                    publish_root_now(&ctx).await
                } else {
                    Ok(())
                };
                let _ = resp_tx.send(pub_result);
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
