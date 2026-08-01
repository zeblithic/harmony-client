//! Sub-D Phase 4 (ZEB-281) — ProfileMembershipBroadcast primitive.
//!
//! Privacy-preserving Zenoh-broadcast protocol where users curate a
//! per-community opt-in subset of their memberships, and peers viewing
//! a profile see only the communities the owner has explicitly shared.
//!
//! See `docs/specs/2026-05-12-zeb-281-sub-d-phase-4-profile-membership-broadcast-design.md`.

use crate::owner_state_crypto::{
    canonical_cbor_encode, sealed::CanonicalPayloadSealed, CanonicalPayload,
};
use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};
use ed25519_dalek::{Signature, Signer};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Hard cap on number of community IDs per broadcast. 200 SpaceIds × 32
/// bytes + framing + sig ≈ 6.5 KB worst-case canonical payload. Spec §4.1.
pub const MAX_SHARED_COMMUNITIES: usize = 200;

/// Topic-name prefix; full topic is `{PREFIX}{owner_addr_hex}/memberships`
/// where `owner_addr_hex` is the lowercase 32-char hex encoding of the
/// 16-byte `OwnerAddr`. Spec §4.1.
///
/// Distinct from `harmony/discovery/library/announce` (Phase 2) and
/// `harmony/discovery/library/{addr}/communities` (Phase 1). Distinct
/// from `harmony/announce/{cid_hex}` (storage tier content-availability).
pub const PROFILE_DISCOVERY_TOPIC_PREFIX: &str = "harmony/discovery/profile/";

/// Wire-size sanity bound on a single broadcast payload before CBOR
/// decode. Defends against adversarial-size frames on the
/// `harmony/discovery/profile/{...}/memberships` topic. The structural
/// worst case (200 SpaceIds × 32 bytes raw + identity_pub + signature +
/// Hlc framing) is ≈6.5 KB; 8 KB is generous headroom. Note that
/// `Hlc.device_id: String` is unbounded, so this is a sanity gate, not
/// a tight proof — adversarial peers with absurd device_id strings get
/// dropped here.
// Referenced by `event_loop.rs` profile-broadcast subscriber pool in Task 5.
#[allow(dead_code)]
pub const MAX_BROADCAST_WIRE_BYTES: usize = 8_192;

/// Build a broadcast topic key for the given OwnerAddr.
pub fn broadcast_topic_for(addr: &OwnerAddr) -> String {
    format!(
        "{PROFILE_DISCOVERY_TOPIC_PREFIX}{}/memberships",
        hex::encode(addr.0)
    )
}

/// Sub-D Phase 4 wire type. Spec §4.1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileMembershipBroadcast {
    /// 64-byte identity bundle (X25519_pub(32) || Ed25519_pub(32)) of
    /// the owner publishing this broadcast. Spec §4.1.
    #[serde(
        rename = "ai",
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub owner_identity_pub: [u8; 64],

    /// Sorted, strictly-increasing (no duplicates) subset of the owner's
    /// joined community SpaceIds opted to share publicly. MAY be empty
    /// (rotation case — see Publisher state machine). Hard cap:
    /// `MAX_SHARED_COMMUNITIES = 200`. Spec §4.1.
    #[serde(rename = "cs")]
    pub community_ids: Vec<SpaceId>,

    /// Hybrid Logical Clock — recipients prefer newer broadcasts over
    /// older ones; publisher rotates stale state by bumping the HLC.
    /// Spec §4.1.
    #[serde(rename = "sa")]
    pub shared_at: Hlc,

    /// Ed25519 sig over canonical CBOR with `signature` zeroed. Same
    /// idiom as `LibraryAnnounce` (Phase 2) and `LibraryDirectoryEntry`
    /// admin sig (Phase 1). Spec §4.1.
    #[serde(
        rename = "sg",
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub signature: [u8; 64],
}

/// Marker so `canonical_cbor_encode` can sign over this struct.
impl CanonicalPayloadSealed for ProfileMembershipBroadcast {}
impl CanonicalPayload for ProfileMembershipBroadcast {}

/// Verification errors. Spec §4.3.
#[derive(Debug, thiserror::Error)]
pub enum BroadcastVerifyError {
    #[error("community_ids exceeds {MAX_SHARED_COMMUNITIES} entries")]
    TooManyCommunities,
    #[error("community_ids must be strictly increasing (sorted + deduped)")]
    CommunityIdsNotSortedDeduped,
    #[error("malformed owner identity_pub: {0}")]
    InvalidIdentityPub(String),
    #[error("Ed25519 signature verification failed")]
    SignatureInvalid,
    #[error("canonical CBOR encode failed: {0}")]
    Encode(#[from] crate::owner_state_crypto::CryptoError),
}

/// Build + Ed25519-sign a broadcast over canonical CBOR with `signature`
/// zeroed. Called by `ProfileBroadcastPublisher::maybe_publish` (production
/// publish path, see Task 2) and by unit tests. The caller is responsible
/// for HLC monotonicity (the publisher's state machine enforces it; tests
/// can pass arbitrary HLCs).
///
/// `signer.verifying_key().as_bytes()` MUST be the Ed25519 half (bytes
/// 32-63) of `owner_identity_pub`, otherwise the caller has a key/identity
/// mismatch (sig will verify but identity parse may not).
pub fn sign_broadcast(
    signer: &ed25519_dalek::SigningKey,
    owner_identity_pub: [u8; 64],
    community_ids: Vec<SpaceId>,
    shared_at: Hlc,
) -> Result<ProfileMembershipBroadcast, BroadcastVerifyError> {
    let mut broadcast = ProfileMembershipBroadcast {
        owner_identity_pub,
        community_ids,
        shared_at,
        signature: [0u8; 64],
    };
    let bytes = canonical_cbor_encode(&broadcast)?;
    let sig = signer.sign(&bytes);
    broadcast.signature = sig.to_bytes();
    Ok(broadcast)
}

/// Verify a broadcast end-to-end. Returns the derived OwnerAddr on
/// success — callers compare it against the topic owner for the
/// attribution check (subscriber-side, in `process_sample`). Spec §6.
pub fn verify_broadcast(
    broadcast: &ProfileMembershipBroadcast,
) -> Result<OwnerAddr, BroadcastVerifyError> {
    // (1) Bounds
    if broadcast.community_ids.len() > MAX_SHARED_COMMUNITIES {
        return Err(BroadcastVerifyError::TooManyCommunities);
    }
    // (2) Strictly increasing (sorted + deduped)
    if !broadcast.community_ids.windows(2).all(|w| w[0] < w[1]) {
        return Err(BroadcastVerifyError::CommunityIdsNotSortedDeduped);
    }
    // (3) Parse identity_pub — also rejects malformed point bytes.
    let identity = harmony_identity::Identity::from_public_bytes(&broadcast.owner_identity_pub)
        .map_err(|e| BroadcastVerifyError::InvalidIdentityPub(format!("{e:?}")))?;
    // (4) Verify sig over canonical CBOR with signature field zeroed.
    let mut for_sig = broadcast.clone();
    for_sig.signature = [0u8; 64];
    let signed_bytes = canonical_cbor_encode(&for_sig)?;
    let sig = Signature::from_bytes(&broadcast.signature);
    identity
        .verifying_key
        .verify_strict(&signed_bytes, &sig)
        .map_err(|_| BroadcastVerifyError::SignatureInvalid)?;
    Ok(OwnerAddr(identity.address_hash))
}

use std::sync::Arc;

use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;

/// Async sink for outbound broadcasts. Production wraps the event-loop
/// `publish_tx`; tests can use `MockSink` to assert the published payload
/// without spinning up Zenoh.
#[async_trait::async_trait]
pub trait ProfileBroadcastPublishSink: Send + Sync + 'static {
    async fn publish(&self, topic: String, payload: Vec<u8>) -> Result<(), String>;
}

/// Read-side handle to the owner-state opted-in set + HLC source.
/// Production wraps `Arc<Mutex<OwnerState>>` + the existing HLC tracker;
/// tests can use `MockSource` to inject fixed sets.
#[async_trait::async_trait]
pub trait ProfileBroadcastSource: Send + Sync + 'static {
    /// Return the SORTED, deduped current opted-in community SpaceIds.
    /// (Walks `OwnerState.spaces` for `kind == Community &&
    /// shared_in_profile == true`.)
    async fn current_shared_set(&self) -> Vec<SpaceId>;
    /// Return the next HLC to stamp on the next publish.
    async fn next_hlc(&self) -> Hlc;
}

/// Publisher debounce window. Spec §5.
pub const PUBLISHER_DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(2);

/// Publisher periodic refresh interval. Spec §5.
pub const PUBLISHER_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(600);

/// Tracks publish state across debounce/refresh ticks.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PublishedSnapshot {
    set: Vec<SpaceId>, // sorted, deduped
    hlc: Hlc,
}

/// Sub-D Phase 4 publisher. Spec §5.
pub struct ProfileBroadcastPublisher {
    notify: Arc<Notify>,
    /// Most recent published snapshot. `None` until first publish.
    ///
    /// ZEB-188: this field is the test-visible mirror of the `Arc` the
    /// background task writes through (`last_published_for_task`); its only
    /// reader is `last_published_for_test`. Gated to that same predicate so a
    /// bare (`--no-default-features`) `-D warnings` build doesn't see it as
    /// dead code — the live snapshot always lives in the task's own clone.
    #[cfg(any(test, feature = "test-fixtures"))]
    last_published: Arc<Mutex<Option<PublishedSnapshot>>>,
    /// Background task driving debounce + refresh. Aborted on `shutdown()`.
    task: Mutex<Option<JoinHandle<()>>>,
}

impl ProfileBroadcastPublisher {
    /// Spawn the publisher background task. `signer` Ed25519-signs each
    /// outbound payload; `identity_pub` is the 64-byte bundle stamped on
    /// every broadcast (and from which subscribers derive the broadcaster
    /// OwnerAddr for the attribution check). `source` reads the current
    /// opted-in set + HLC; `sink` publishes the bytes.
    pub fn spawn(
        signer: ed25519_dalek::SigningKey,
        identity_pub: [u8; 64],
        source: Arc<dyn ProfileBroadcastSource>,
        sink: Arc<dyn ProfileBroadcastPublishSink>,
        // Test seam: lets unit tests provide a faster debounce/refresh.
        // Production always uses the constants above.
        debounce: std::time::Duration,
        refresh: std::time::Duration,
    ) -> Arc<Self> {
        let notify = Arc::new(Notify::new());
        let last_published: Arc<Mutex<Option<PublishedSnapshot>>> = Arc::new(Mutex::new(None));

        let task = {
            let notify_for_task = Arc::clone(&notify);
            let last_published_for_task = Arc::clone(&last_published);
            let identity_pub_for_task = identity_pub;
            tokio::spawn(async move {
                let mut refresh_interval = tokio::time::interval(refresh);
                refresh_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                // First tick fires immediately; consume it.
                refresh_interval.tick().await;
                loop {
                    tokio::select! {
                        _ = notify_for_task.notified() => {
                            // Debounce — drain further notifies that
                            // arrive within `debounce`.
                            let deadline = tokio::time::Instant::now() + debounce;
                            loop {
                                tokio::select! {
                                    _ = notify_for_task.notified() => continue,
                                    _ = tokio::time::sleep_until(deadline) => break,
                                }
                            }
                            Self::maybe_publish(
                                &signer,
                                identity_pub_for_task,
                                source.as_ref(),
                                sink.as_ref(),
                                last_published_for_task.as_ref(),
                                false, // is_refresh
                            ).await;
                        }
                        _ = refresh_interval.tick() => {
                            Self::maybe_publish(
                                &signer,
                                identity_pub_for_task,
                                source.as_ref(),
                                sink.as_ref(),
                                last_published_for_task.as_ref(),
                                true, // is_refresh
                            ).await;
                        }
                    }
                }
            })
        };

        Arc::new(Self {
            notify,
            #[cfg(any(test, feature = "test-fixtures"))]
            last_published,
            task: Mutex::new(Some(task)),
        })
    }

    /// IPC handler calls this after mutating `Space.shared_in_profile`.
    pub fn notify_dirty(&self) {
        self.notify.notify_one();
    }

    /// Abort the background task. Idempotent.
    pub async fn shutdown(&self) {
        if let Some(h) = self.task.lock().await.take() {
            h.abort();
        }
    }

    /// Test seam: read the most recently published snapshot.
    #[cfg(any(test, feature = "test-fixtures"))]
    pub async fn last_published_for_test(&self) -> Option<(Vec<SpaceId>, Hlc)> {
        self.last_published
            .lock()
            .await
            .as_ref()
            .map(|s| (s.set.clone(), s.hlc.clone()))
    }

    async fn maybe_publish(
        signer: &ed25519_dalek::SigningKey,
        identity_pub: [u8; 64],
        source: &dyn ProfileBroadcastSource,
        sink: &dyn ProfileBroadcastPublishSink,
        last_published: &Mutex<Option<PublishedSnapshot>>,
        is_refresh: bool,
    ) {
        let current = source.current_shared_set().await;

        // Hard cap defence-in-depth: `verify_broadcast` rejects payloads
        // carrying more than `MAX_SHARED_COMMUNITIES` ids, so publishing
        // an oversize set would silently disappear at every peer. Skip
        // (no truncation, no `last_published` update) and surface a
        // warning so the issue isn't invisible.
        if current.len() > MAX_SHARED_COMMUNITIES {
            tracing::warn!(
                len = current.len(),
                max = MAX_SHARED_COMMUNITIES,
                "profile broadcast skipped: opted-in set exceeds maximum; \
                 every peer would reject this payload at verify_broadcast"
            );
            return;
        }

        // Lock held across `sink.publish().await` is safe here — single-writer
        // task (the spawned publisher), only other consumer
        // (`last_published_for_test`) is read-only.
        let mut guard = last_published.lock().await;
        let last_snapshot = guard.as_ref().cloned();

        // Privacy invariant: NO broadcast before first opt-in.
        let has_ever_published = last_snapshot.is_some();
        if !has_ever_published && current.is_empty() {
            return;
        }

        // Skip-when-unchanged for debounce path; refresh always re-publishes
        // the current set as long as we've ever published before.
        if !is_refresh {
            if let Some(snap) = &last_snapshot {
                if snap.set == current {
                    return;
                }
            }
        }
        // Refresh-arm: NO `!has_ever_published` early-return here. After
        // process restart, the in-memory `last_published` is `None` (it's
        // intentionally not persisted), but `Space.shared_in_profile` is
        // — so a user with persisted opt-ins must re-broadcast on the
        // first refresh tick without needing an explicit `notify_dirty()`.
        // The privacy invariant (no broadcast before first opt-in) is
        // already guarded above by the `!has_ever_published &&
        // current.is_empty()` early-return: when there's no persisted
        // opt-in set, `current` is empty and the publisher stays silent.

        // Rotation: when refresh tick fires AFTER N→0, the prior snapshot
        // is empty. Don't republish empty over empty.
        if is_refresh {
            if let Some(snap) = &last_snapshot {
                if snap.set.is_empty() && current.is_empty() {
                    return;
                }
            }
        }

        let hlc = source.next_hlc().await;
        let broadcast = match sign_broadcast(signer, identity_pub, current.clone(), hlc.clone()) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = ?e, "profile broadcast sign failed");
                return;
            }
        };
        let bytes = match canonical_cbor_encode(&broadcast) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = ?e, "profile broadcast encode failed");
                return;
            }
        };
        let addr = match harmony_identity::Identity::from_public_bytes(&identity_pub) {
            Ok(id) => OwnerAddr(id.address_hash),
            Err(e) => {
                tracing::warn!(error = ?e, "profile broadcast identity derive failed");
                return;
            }
        };
        let topic = broadcast_topic_for(&addr);
        if let Err(e) = sink.publish(topic, bytes).await {
            tracing::warn!(error = %e, "profile broadcast publish failed");
            return;
        }
        *guard = Some(PublishedSnapshot { set: current, hlc });
    }
}

/// Production-side `ProfileBroadcastSource` that reads the owner-state
/// CRDT + uses the existing HLC tracker. Wired in start_node (Task 5).
pub struct OwnerStateBroadcastSource {
    pub crdt_state: Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
    pub hlc_tracker: Arc<tokio::sync::Mutex<harmony_crdt_sync::ReplayTracker<String, Hlc>>>,
    pub device_id: String,
    /// ZEB-790: node-wide bounded-adoption floor (see `hlc_adopt_floor` module
    /// docs). Applied at `next_hlc`.
    pub adopt_floor: crate::hlc_adopt_floor::HlcAdoptFloor,
}

#[async_trait::async_trait]
impl ProfileBroadcastSource for OwnerStateBroadcastSource {
    async fn current_shared_set(&self) -> Vec<SpaceId> {
        let g = self.crdt_state.lock().await;
        // NOTE: For a Community Space, the community's SpaceId IS `s.id`
        // — `s.community_id` is a back-pointer from a Channel Space to
        // its parent Community Space and is validated to be `None` on
        // Community Spaces themselves (owner_state_types.rs:1769-1775).
        // Using `s.community_id` here would filter every Community out.
        let mut ids: Vec<SpaceId> = g
            .spaces
            .values()
            .filter(|s| {
                // `left_at.is_none()` guards the privacy invariant from
                // `leave_community`: the Community Space is intentionally
                // retained (history, re-join, etc.) but MUST NOT continue
                // to be advertised in the profile broadcast. Without this
                // filter, a stale `shared_in_profile=true` on a left
                // community would leak past departure.
                matches!(s.kind, crate::owner_state_types::SpaceKind::Community)
                    && s.shared_in_profile
                    && s.left_at.is_none()
            })
            .map(|s| s.id)
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }

    async fn next_hlc(&self) -> Hlc {
        // Mirror the bump-pattern used by `owner_state_sync::next_hlc`
        // and other community-related publishes (SystemTime-based wall
        // clock + saturating logical counter).
        use std::time::{SystemTime, UNIX_EPOCH};
        let now_ms = self.adopt_floor.merged_now(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        );
        let mut tracker = self.hlc_tracker.lock().await;
        // Was a fourth open-coded copy of the same tick rule (bump logical
        // unless the wall reading advanced). It now delegates to the core
        // kernel like every other minting path (ZEB-759).
        let tick = harmony_crdt_sync::HlcTick::next(
            tracker
                .accepted_from(&self.device_id)
                .map(harmony_crdt_sync::HlcTick::from),
            now_ms,
        );
        let next = Hlc {
            wall_ms: tick.wall_ms,
            logical: tick.logical,
            device_id: self.device_id.clone(),
        };
        tracker.observe_local(next.clone());
        next
    }
}

/// Production-side `ProfileBroadcastPublishSink` that forwards to the
/// event-loop's `publish_tx` channel.
pub struct EventLoopPublishSink {
    pub publish_tx: tokio::sync::mpsc::Sender<crate::event_loop::PublishRequest>,
}

#[async_trait::async_trait]
impl ProfileBroadcastPublishSink for EventLoopPublishSink {
    async fn publish(&self, topic: String, payload: Vec<u8>) -> Result<(), String> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.publish_tx
            .send(crate::event_loop::PublishRequest {
                key_expr: topic,
                payload,
                reply: reply_tx,
            })
            .await
            .map_err(|e| format!("publish_tx send: {e}"))?;
        reply_rx
            .await
            .map_err(|e| format!("publish_tx reply dropped: {e}"))?
    }
}

/// Subscription identifier handed to the frontend by `subscribe_peer_profile`.
/// Plain u64 (monotonic, allocated by an AtomicU64 in NodeState). One
/// subscription_id maps to one Zenoh subscriber task; multiple subscriptions
/// to the same peer are allowed (one per open ProfilePopover).
pub type SubscriptionId = u64;

/// Snapshot of the latest verified broadcast for a subscription.
/// Wire keys are camelCase to match `DiscoveredLibraryInfo` (Phase 2).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredProfileInfo {
    /// Hex-encoded 16-byte OwnerAddr (32 hex chars).
    pub owner_addr: String,
    /// Hex-encoded SpaceIds the peer opted to share (32 hex chars each).
    pub community_ids: Vec<String>,
    /// `shared_at.wall_ms` as base-10 string. Display only — callers MUST
    /// NOT use this for HLC ordering decisions.
    pub shared_at: String,
}

/// Per-subscription cache entry. Holds the highest-HLC verified broadcast
/// observed so far + the peer addr the subscription targets (for attribution).
#[derive(Debug, Clone)]
struct CachedSubscription {
    peer_addr: OwnerAddr,
    /// Most recent verified broadcast. `None` until first valid sample
    /// arrives (covers the "loading" UI state).
    latest: Option<ProfileMembershipBroadcast>,
}

/// In-process cache of verified peer profile broadcasts. Spec §6.
#[derive(Debug, Default)]
pub struct ProfileBroadcastCache {
    by_sub: tokio::sync::Mutex<HashMap<SubscriptionId, CachedSubscription>>,
}

#[derive(Debug, thiserror::Error)]
pub enum CacheOnSampleError {
    #[error("subscription not found: {0}")]
    SubscriptionNotFound(SubscriptionId),
    #[error("verify failed: {0}")]
    VerifyFailed(#[from] BroadcastVerifyError),
    #[error("attribution mismatch: topic owner={topic_owner:?}, derived={derived:?}")]
    AttributionMismatch {
        topic_owner: OwnerAddr,
        derived: OwnerAddr,
    },
    #[error("replay: incoming HLC not strictly newer than cached")]
    Replay,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheOnSampleOutcome {
    /// First broadcast for this subscription — emit event.
    InsertedFirst,
    /// Newer broadcast replaced an older one — emit event.
    Replaced,
}

impl ProfileBroadcastCache {
    /// Register a subscription with the cache. Idempotent — re-inserting
    /// the same subscription_id replaces the prior entry.
    pub async fn register(&self, sub: SubscriptionId, peer_addr: OwnerAddr) {
        self.by_sub.lock().await.insert(
            sub,
            CachedSubscription {
                peer_addr,
                latest: None,
            },
        );
    }

    /// Drop a subscription from the cache. Idempotent — missing sub is OK.
    pub async fn drop_subscription(&self, sub: SubscriptionId) {
        self.by_sub.lock().await.remove(&sub);
    }

    /// Process an incoming raw payload for a subscription. Spec §6.
    pub async fn on_sample(
        &self,
        sub: SubscriptionId,
        broadcast: ProfileMembershipBroadcast,
    ) -> Result<CacheOnSampleOutcome, CacheOnSampleError> {
        // (1) Verify
        let derived = verify_broadcast(&broadcast)?;
        // (2) Attribution check + replay defense — atomic against the map.
        let mut g = self.by_sub.lock().await;
        let entry = g
            .get_mut(&sub)
            .ok_or(CacheOnSampleError::SubscriptionNotFound(sub))?;
        if derived != entry.peer_addr {
            return Err(CacheOnSampleError::AttributionMismatch {
                topic_owner: entry.peer_addr,
                derived,
            });
        }
        if let Some(prev) = &entry.latest {
            // Strict greater-than: equal HLC is also rejected (idempotent
            // duplicate). Use the canonical `Hlc::is_strictly_newer_than`
            // which compares on the full (wall_ms, logical, device_id)
            // tuple. Comparing only (wall_ms, logical) — as an earlier
            // revision did — would incorrectly reject broadcasts from a
            // different bound device of the same owner whose wall+logical
            // happen to tie a previously-cached sample.
            if !broadcast.shared_at.is_strictly_newer_than(&prev.shared_at) {
                return Err(CacheOnSampleError::Replay);
            }
        }
        let was_none = entry.latest.is_none();
        entry.latest = Some(broadcast);
        Ok(if was_none {
            CacheOnSampleOutcome::InsertedFirst
        } else {
            CacheOnSampleOutcome::Replaced
        })
    }

    /// Snapshot the latest verified broadcast for a subscription as the
    /// frontend DTO.
    pub async fn get_cached(&self, sub: SubscriptionId) -> Option<DiscoveredProfileInfo> {
        let g = self.by_sub.lock().await;
        let entry = g.get(&sub)?;
        let b = entry.latest.as_ref()?;
        Some(DiscoveredProfileInfo {
            owner_addr: hex::encode(entry.peer_addr.0),
            community_ids: b.community_ids.iter().map(|s| hex::encode(s.0)).collect(),
            shared_at: b.shared_at.wall_ms.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    fn fixture_hlc(wall_ms: u64) -> Hlc {
        Hlc {
            wall_ms,
            logical: 0,
            device_id: "fix".into(),
        }
    }

    /// Mirrors the `build_test_identity_pub` pattern in
    /// `library_directory::tests`: derive a SigningKey from a 32-byte
    /// seed and synthesize a 64-byte identity bundle whose Ed25519
    /// half matches the signer's verifying key. The X25519 half is a
    /// constant prefix — `Identity::from_public_bytes` doesn't
    /// validate the X25519 half (only `VerifyingKey::from_bytes`
    /// validates the Ed25519 half), so any 32 bytes work. Distinct
    /// prefix from library_directory's `[0x11; 32]` keeps test
    /// identities orthogonal even if seeds collide.
    fn build_identity(seed: [u8; 32]) -> (SigningKey, [u8; 64]) {
        let signing = SigningKey::from_bytes(&seed);
        let verifying = signing.verifying_key().to_bytes();
        let mut bundle = [0u8; 64];
        bundle[..32].copy_from_slice(&[0x33; 32]);
        bundle[32..].copy_from_slice(&verifying);
        (signing, bundle)
    }

    fn fixture_space_id(byte: u8) -> SpaceId {
        SpaceId([byte; 16])
    }

    #[test]
    fn verify_broadcast_valid_returns_owner_addr() {
        let (signer, identity_pub) = build_identity([1u8; 32]);
        let cs = vec![
            fixture_space_id(1),
            fixture_space_id(2),
            fixture_space_id(3),
        ];
        let b = sign_broadcast(&signer, identity_pub, cs, fixture_hlc(100)).unwrap();
        let addr = verify_broadcast(&b).unwrap();
        // Derived addr matches Identity::from_public_bytes
        let expected = harmony_identity::Identity::from_public_bytes(&identity_pub)
            .unwrap()
            .address_hash;
        assert_eq!(addr.0, expected);
    }

    #[test]
    fn verify_broadcast_tampered_signature_rejected() {
        let (signer, identity_pub) = build_identity([2u8; 32]);
        let cs = vec![fixture_space_id(1)];
        let mut b = sign_broadcast(&signer, identity_pub, cs, fixture_hlc(100)).unwrap();
        b.signature[0] ^= 0xff;
        assert!(matches!(
            verify_broadcast(&b),
            Err(BroadcastVerifyError::SignatureInvalid)
        ));
    }

    #[test]
    fn verify_broadcast_tampered_payload_rejected() {
        let (signer, identity_pub) = build_identity([3u8; 32]);
        let cs = vec![fixture_space_id(1), fixture_space_id(2)];
        let mut b = sign_broadcast(&signer, identity_pub, cs, fixture_hlc(100)).unwrap();
        // XOR the LAST byte of community_ids[0] — keeps it sorted (still
        // < community_ids[1] = [2; 16]) and unique, so the bounds + sort
        // + dedup checks pass, but the canonical-CBOR bytes change so
        // the sig now mismatches.
        b.community_ids[0].0[15] ^= 0x01;
        assert!(matches!(
            verify_broadcast(&b),
            Err(BroadcastVerifyError::SignatureInvalid)
        ));
    }

    #[test]
    fn verify_broadcast_too_many_communities_rejected() {
        let (signer, identity_pub) = build_identity([4u8; 32]);
        let cs: Vec<SpaceId> = (0..(MAX_SHARED_COMMUNITIES + 1) as u16)
            .map(|i| {
                let mut bytes = [0u8; 16];
                bytes[0..2].copy_from_slice(&i.to_be_bytes());
                SpaceId(bytes)
            })
            .collect();
        let b = sign_broadcast(&signer, identity_pub, cs, fixture_hlc(100)).unwrap();
        assert!(matches!(
            verify_broadcast(&b),
            Err(BroadcastVerifyError::TooManyCommunities)
        ));
    }

    #[test]
    fn verify_broadcast_unsorted_community_ids_rejected() {
        let (signer, identity_pub) = build_identity([5u8; 32]);
        // [B, A] — out of order
        let cs = vec![fixture_space_id(2), fixture_space_id(1)];
        let b = sign_broadcast(&signer, identity_pub, cs, fixture_hlc(100)).unwrap();
        assert!(matches!(
            verify_broadcast(&b),
            Err(BroadcastVerifyError::CommunityIdsNotSortedDeduped)
        ));
    }

    #[test]
    fn verify_broadcast_duplicate_community_ids_rejected() {
        let (signer, identity_pub) = build_identity([6u8; 32]);
        // [A, A] — duplicate
        let cs = vec![fixture_space_id(1), fixture_space_id(1)];
        let b = sign_broadcast(&signer, identity_pub, cs, fixture_hlc(100)).unwrap();
        assert!(matches!(
            verify_broadcast(&b),
            Err(BroadcastVerifyError::CommunityIdsNotSortedDeduped)
        ));
    }

    #[test]
    fn verify_broadcast_malformed_identity_pub_rejected() {
        // Ed25519 half `[0x7F; 32]` doesn't decompress under
        // ed25519-dalek 2.x / curve25519-dalek 4.x — same fixture used
        // by `library_directory::tests::malformed_identity_pub_rejected`.
        // (All-zero bytes decompress to a valid Edwards low-order point,
        // so they don't trigger this rejection path — use 0x7F instead.)
        let mut bad_identity_pub = [0u8; 64];
        bad_identity_pub[32..].copy_from_slice(&[0x7F; 32]);
        // Bypass sign_broadcast (which requires a real signer); build
        // a broadcast manually with a sig that will never be reached
        // because identity parse fails first.
        let b = ProfileMembershipBroadcast {
            owner_identity_pub: bad_identity_pub,
            community_ids: vec![fixture_space_id(1)],
            shared_at: fixture_hlc(100),
            signature: [0u8; 64],
        };
        assert!(matches!(
            verify_broadcast(&b),
            Err(BroadcastVerifyError::InvalidIdentityPub(_))
        ));
    }

    #[test]
    fn verify_broadcast_empty_community_ids_accepted() {
        let (signer, identity_pub) = build_identity([8u8; 32]);
        let b = sign_broadcast(&signer, identity_pub, vec![], fixture_hlc(100)).unwrap();
        let addr = verify_broadcast(&b).unwrap();
        let expected = harmony_identity::Identity::from_public_bytes(&identity_pub)
            .unwrap()
            .address_hash;
        assert_eq!(addr.0, expected);
    }

    use std::sync::Arc as StdArc;
    use tokio::sync::Mutex as TokioMutex;

    /// Regression: ensures `OwnerStateBroadcastSource::current_shared_set`
    /// extracts the Community Space's own `id` (the community's SpaceId)
    /// and NOT `s.community_id` (which is a back-pointer from Channels to
    /// their parent Community, and validates as `None` on Community
    /// Spaces themselves — see owner_state_types.rs:1769-1775). The
    /// MockSource above returns hard-coded SpaceIds so it doesn't
    /// exercise this code path; this test wires the real production
    /// source against a synthetic `OwnerState`.
    #[tokio::test]
    async fn owner_state_source_returns_opted_in_community_space_ids() {
        use crate::owner_state_crdt::OwnerState;
        use crate::owner_state_types::{DmContentKey, EpochKey, OwnerAddr, Space, SpaceKind};

        let zero_hlc = Hlc {
            wall_ms: 0,
            logical: 0,
            device_id: "t".into(),
        };
        // Community A — opted in. Its id MUST appear in the result.
        let community_opted_in = Space {
            id: SpaceId([0xa1; 16]),
            kind: SpaceKind::Community,
            parent: None,
            community_id: None, // back-pointer is None on Community itself
            name: "Opted-in community".into(),
            transport: None,
            members: vec![],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: zero_hlc.clone(),
            updated_at: zero_hlc.clone(),
            content_key: None,
            prior_content_keys: vec![],
            current_epoch: Some(0),
            current_epoch_key: Some(EpochKey::new([0xab; 32])),
            old_epoch_keys: std::collections::BTreeMap::new(),
            admin_addr: Some(OwnerAddr([0xbb; 16])),
            is_invite_only: Some(false),
            shared_in_profile: true,
            pending_join_at: None,
        };
        // Community B — opted OUT. Must NOT appear.
        let community_opted_out = Space {
            id: SpaceId([0xa2; 16]),
            kind: SpaceKind::Community,
            parent: None,
            community_id: None,
            name: "Opted-out community".into(),
            transport: None,
            members: vec![],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: zero_hlc.clone(),
            updated_at: zero_hlc.clone(),
            content_key: None,
            prior_content_keys: vec![],
            current_epoch: Some(0),
            current_epoch_key: Some(EpochKey::new([0xcd; 32])),
            old_epoch_keys: std::collections::BTreeMap::new(),
            admin_addr: Some(OwnerAddr([0xbb; 16])),
            is_invite_only: Some(false),
            shared_in_profile: false,
            pending_join_at: None,
        };
        // DM — shared_in_profile=true defensively, kind != Community
        // so it MUST be filtered out regardless of the flag.
        let dm = Space {
            id: SpaceId([0xd0; 16]),
            kind: SpaceKind::Dm,
            parent: None,
            community_id: None,
            name: "Some DM".into(),
            transport: None,
            members: vec![OwnerAddr([1u8; 16]), OwnerAddr([2u8; 16])],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: zero_hlc.clone(),
            updated_at: zero_hlc.clone(),
            content_key: Some(DmContentKey::new([0xaa; 32])),
            prior_content_keys: vec![],
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: true,
            pending_join_at: None,
        };
        // Channel — kind != Community, with community_id back-pointer
        // to community_opted_in. Previous bug would have included this
        // community's id via the back-pointer instead of via the
        // Community Space's own id. MUST be filtered out (wrong kind).
        let channel = Space {
            id: SpaceId([0xc0; 16]),
            kind: SpaceKind::Channel,
            parent: Some(community_opted_in.id),
            community_id: Some(community_opted_in.id),
            name: "Some channel".into(),
            transport: None,
            members: vec![],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: zero_hlc.clone(),
            updated_at: zero_hlc,
            content_key: None,
            prior_content_keys: vec![],
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            pending_join_at: None,
        };

        let mut state = OwnerState::default();
        state
            .spaces
            .insert(community_opted_in.id, community_opted_in.clone());
        state
            .spaces
            .insert(community_opted_out.id, community_opted_out.clone());
        state.spaces.insert(dm.id, dm.clone());
        state.spaces.insert(channel.id, channel.clone());

        let crdt_state = StdArc::new(TokioMutex::new(state));
        let hlc_tracker = StdArc::new(TokioMutex::new(harmony_crdt_sync::ReplayTracker::new(
            "test".to_string(),
        )));
        let src = OwnerStateBroadcastSource {
            crdt_state,
            hlc_tracker,
            device_id: "test".into(),
            adopt_floor: crate::hlc_adopt_floor::HlcAdoptFloor::new(),
        };

        let got = src.current_shared_set().await;
        assert_eq!(
            got,
            vec![community_opted_in.id],
            "current_shared_set must return ONLY the opted-in Community \
             Space's own id (not community_id back-pointer, not the \
             opted-out community, not the DM, not the channel); got {got:?}"
        );
    }

    /// Regression: a Community Space with `shared_in_profile=true` AND
    /// `left_at=Some(_)` MUST be excluded from the broadcast set. The
    /// `leave_community` IPC intentionally retains the Space (so history,
    /// re-join, etc. stay coherent) but the privacy promise of "no
    /// advertising memberships you no longer hold" requires filtering
    /// here. Without the `left_at.is_none()` guard, a stale opt-in
    /// would keep leaking past departure.
    #[tokio::test]
    async fn owner_state_source_excludes_left_communities() {
        use crate::owner_state_crdt::OwnerState;
        use crate::owner_state_types::{EpochKey, OwnerAddr, Space, SpaceKind};

        let zero_hlc = Hlc {
            wall_ms: 0,
            logical: 0,
            device_id: "t".into(),
        };
        let left_hlc = Hlc {
            wall_ms: 1_000,
            logical: 0,
            device_id: "t".into(),
        };

        // Community A — opted-in AND still joined. MUST appear.
        let still_joined = Space {
            id: SpaceId([0xa1; 16]),
            kind: SpaceKind::Community,
            parent: None,
            community_id: None,
            name: "Still joined".into(),
            transport: None,
            members: vec![],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: zero_hlc.clone(),
            updated_at: zero_hlc.clone(),
            content_key: None,
            prior_content_keys: vec![],
            current_epoch: Some(0),
            current_epoch_key: Some(EpochKey::new([0xab; 32])),
            old_epoch_keys: std::collections::BTreeMap::new(),
            admin_addr: Some(OwnerAddr([0xbb; 16])),
            is_invite_only: Some(false),
            shared_in_profile: true,
            pending_join_at: None,
        };
        // Community B — was opted-in but the user has since left. The
        // shared_in_profile flag is still true (not auto-cleared by
        // leave_community), but the broadcast source MUST suppress it
        // because `left_at` is Some.
        let left_but_still_flagged = Space {
            id: SpaceId([0xa2; 16]),
            kind: SpaceKind::Community,
            parent: None,
            community_id: None,
            name: "Left community".into(),
            transport: None,
            members: vec![],
            custom_name: None,
            notification_pref: None,
            left_at: Some(left_hlc),
            created_at: zero_hlc.clone(),
            updated_at: zero_hlc.clone(),
            content_key: None,
            prior_content_keys: vec![],
            current_epoch: Some(0),
            current_epoch_key: Some(EpochKey::new([0xcd; 32])),
            old_epoch_keys: std::collections::BTreeMap::new(),
            admin_addr: Some(OwnerAddr([0xbb; 16])),
            is_invite_only: Some(false),
            shared_in_profile: true,
            pending_join_at: None,
        };

        let mut state = OwnerState::default();
        state.spaces.insert(still_joined.id, still_joined.clone());
        state
            .spaces
            .insert(left_but_still_flagged.id, left_but_still_flagged.clone());

        let crdt_state = StdArc::new(TokioMutex::new(state));
        let hlc_tracker = StdArc::new(TokioMutex::new(harmony_crdt_sync::ReplayTracker::new(
            "test".to_string(),
        )));
        let src = OwnerStateBroadcastSource {
            crdt_state,
            hlc_tracker,
            device_id: "test".into(),
            adopt_floor: crate::hlc_adopt_floor::HlcAdoptFloor::new(),
        };

        let got = src.current_shared_set().await;
        assert_eq!(
            got,
            vec![still_joined.id],
            "current_shared_set must exclude communities with left_at=Some(_) \
             even when shared_in_profile is still true; got {got:?}"
        );
    }

    // Type aliases keep clippy::type_complexity happy and make the
    // MockSink / mock_publisher_setup signatures readable.
    type SharedPublished = StdArc<TokioMutex<Vec<(String, Vec<u8>)>>>;
    type SharedSpaceIds = StdArc<TokioMutex<Vec<SpaceId>>>;

    struct MockSink {
        published: SharedPublished,
    }

    #[async_trait::async_trait]
    impl ProfileBroadcastPublishSink for MockSink {
        async fn publish(&self, topic: String, payload: Vec<u8>) -> Result<(), String> {
            self.published.lock().await.push((topic, payload));
            Ok(())
        }
    }

    struct MockSource {
        set: StdArc<TokioMutex<Vec<SpaceId>>>,
        hlc: StdArc<TokioMutex<Hlc>>,
    }

    #[async_trait::async_trait]
    impl ProfileBroadcastSource for MockSource {
        async fn current_shared_set(&self) -> Vec<SpaceId> {
            self.set.lock().await.clone()
        }
        async fn next_hlc(&self) -> Hlc {
            let mut g = self.hlc.lock().await;
            g.logical = g.logical.saturating_add(1);
            g.clone()
        }
    }

    fn mock_publisher_setup() -> (
        SharedSpaceIds,
        SharedPublished,
        Arc<ProfileBroadcastPublisher>,
    ) {
        let (signer, identity_pub) = build_identity([42u8; 32]);
        let set = StdArc::new(TokioMutex::new(Vec::new()));
        let hlc = StdArc::new(TokioMutex::new(Hlc {
            wall_ms: 1_000,
            logical: 0,
            device_id: "test".into(),
        }));
        let published = StdArc::new(TokioMutex::new(Vec::new()));
        let source = Arc::new(MockSource {
            set: StdArc::clone(&set),
            hlc: StdArc::clone(&hlc),
        });
        let sink = Arc::new(MockSink {
            published: StdArc::clone(&published),
        });
        let publisher = ProfileBroadcastPublisher::spawn(
            signer,
            identity_pub,
            source,
            sink,
            // Fast debounce + fast refresh for tests.
            std::time::Duration::from_millis(20),
            std::time::Duration::from_millis(100),
        );
        (set, published, publisher)
    }

    #[tokio::test]
    async fn publisher_no_broadcast_before_first_optin() {
        let (_set, published, publisher) = mock_publisher_setup();
        // Empty set — notify the publisher anyway.
        publisher.notify_dirty();
        // Wait past the debounce + a refresh tick.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let pubs = published.lock().await;
        assert!(
            pubs.is_empty(),
            "publisher must NOT publish before first opt-in (privacy invariant); \
             published: {pubs:?}"
        );
        drop(pubs);
        publisher.shutdown().await;
    }

    #[tokio::test]
    async fn publisher_debounce_coalesces_rapid_toggles() {
        let (set, published, publisher) = mock_publisher_setup();
        // Seed an opted-in set so privacy gate is open.
        *set.lock().await = vec![fixture_space_id(1)];
        publisher.notify_dirty();
        // Rapid-fire 5 toggles within the 20ms debounce.
        for _ in 0..5 {
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            publisher.notify_dirty();
        }
        // Wait past debounce.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        // Shut down BEFORE asserting to eliminate the race against the
        // 100ms refresh tick on contended CI runners — once the task is
        // aborted no further publishes can happen, so the assert runs
        // against a quiesced publisher.
        publisher.shutdown().await;
        let pubs = published.lock().await;
        assert_eq!(
            pubs.len(),
            1,
            "5 rapid toggles must coalesce to 1 broadcast; got {}",
            pubs.len()
        );
    }

    #[tokio::test]
    async fn publisher_rotation_publishes_empty_then_stops_refresh() {
        let (set, published, publisher) = mock_publisher_setup();
        // First opt-in → publish.
        *set.lock().await = vec![fixture_space_id(1)];
        publisher.notify_dirty();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        // N→0 rotation → publish empty.
        *set.lock().await = vec![];
        publisher.notify_dirty();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        // Wait several refresh intervals — should NOT republish empty.
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;

        let pubs = published.lock().await;
        assert_eq!(
            pubs.len(),
            2,
            "expected 2 publishes (first opt-in + N→0 rotation); got {} entries: {pubs:?}",
            pubs.len()
        );
        // Decode the second payload and assert community_ids == [].
        let second_payload = &pubs[1].1;
        let decoded: ProfileMembershipBroadcast =
            ciborium::from_reader(&second_payload[..]).expect("decode rotation payload");
        assert!(
            decoded.community_ids.is_empty(),
            "rotation publish must carry empty community_ids; got {:?}",
            decoded.community_ids
        );
        // And its HLC must be strictly newer than the first.
        let first_payload = &pubs[0].1;
        let first_decoded: ProfileMembershipBroadcast =
            ciborium::from_reader(&first_payload[..]).expect("decode first payload");
        assert!(
            decoded.shared_at.wall_ms > first_decoded.shared_at.wall_ms
                || (decoded.shared_at.wall_ms == first_decoded.shared_at.wall_ms
                    && decoded.shared_at.logical > first_decoded.shared_at.logical),
            "rotation HLC must be strictly newer; first={:?} second={:?}",
            first_decoded.shared_at,
            decoded.shared_at
        );
        drop(pubs);
        publisher.shutdown().await;
    }

    /// Regression: simulates a process restart where the user has
    /// `Space.shared_in_profile=true` persisted in the CRDT but no
    /// in-memory `last_published` yet (it's not persisted across
    /// restarts). Without an explicit `notify_dirty()` call, the
    /// first refresh-interval tick MUST publish the persisted opt-in
    /// set. A prior revision returned early on refresh-tick whenever
    /// `!has_ever_published`, so users with persisted opt-ins went
    /// silent until they re-toggled — the privacy invariant is still
    /// preserved by the upstream `current.is_empty()` early-return.
    #[tokio::test]
    async fn publisher_persisted_optin_rebroadcasts_after_restart_without_notify() {
        let (set, published, publisher) = mock_publisher_setup();
        // Persisted opt-in set BEFORE any notify_dirty() — represents
        // state recovered from on-disk CRDT after process restart.
        *set.lock().await = vec![fixture_space_id(1)];
        // Do NOT call publisher.notify_dirty(). Wait past one refresh
        // interval (test setup uses 100ms refresh) and confirm an
        // initial publish happened anyway.
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        publisher.shutdown().await;
        let pubs = published.lock().await;
        assert!(
            !pubs.is_empty(),
            "persisted opt-in must produce an initial broadcast on the \
             first refresh tick (no notify_dirty required); got {pubs:?}"
        );
        let first_payload = &pubs[0].1;
        let decoded: ProfileMembershipBroadcast =
            ciborium::from_reader(&first_payload[..]).expect("decode first payload");
        assert_eq!(
            decoded.community_ids,
            vec![fixture_space_id(1)],
            "first broadcast on restart must carry the persisted opt-in set"
        );
    }

    /// Regression: an opted-in set larger than `MAX_SHARED_COMMUNITIES`
    /// must NOT be signed + published. Every peer would reject such a
    /// payload at `verify_broadcast`, so publishing it makes the
    /// profile silently disappear from the network. The publisher
    /// should skip + warn instead, leaving `last_published`
    /// untouched.
    #[tokio::test]
    async fn publisher_skips_publish_when_set_exceeds_max() {
        let (set, published, publisher) = mock_publisher_setup();
        // Build MAX_SHARED_COMMUNITIES + 1 distinct SpaceIds. `SpaceId`
        // is `[u8; 16]`, so encode a u16 index across the first two
        // bytes to keep all ids distinct (we need 201, which exceeds
        // the 256 range of a single byte).
        let oversize: Vec<SpaceId> = (0u16..=(MAX_SHARED_COMMUNITIES as u16))
            .map(|i| {
                let mut bytes = [0u8; 16];
                bytes[0] = (i & 0xff) as u8;
                bytes[1] = ((i >> 8) & 0xff) as u8;
                SpaceId(bytes)
            })
            .collect();
        assert_eq!(oversize.len(), MAX_SHARED_COMMUNITIES + 1);
        *set.lock().await = oversize;
        publisher.notify_dirty();
        // Wait past debounce + a refresh tick.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        publisher.shutdown().await;
        let pubs = published.lock().await;
        assert!(
            pubs.is_empty(),
            "publisher must NOT publish an oversize set; got {} entries",
            pubs.len()
        );
        // last_published must remain None — no snapshot recorded.
        assert!(
            publisher.last_published_for_test().await.is_none(),
            "last_published must remain None when oversize publish is skipped"
        );
    }

    #[tokio::test]
    async fn state_replay_old_hlc_rejected() {
        let (signer, identity_pub) = build_identity([100u8; 32]);
        let peer_addr = OwnerAddr(
            harmony_identity::Identity::from_public_bytes(&identity_pub)
                .unwrap()
                .address_hash,
        );
        let cache = ProfileBroadcastCache::default();
        cache.register(1, peer_addr).await;

        let newer = sign_broadcast(
            &signer,
            identity_pub,
            vec![fixture_space_id(1)],
            fixture_hlc(200),
        )
        .unwrap();
        let older = sign_broadcast(
            &signer,
            identity_pub,
            vec![fixture_space_id(2)],
            fixture_hlc(100),
        )
        .unwrap();

        // Land the newer first.
        assert_eq!(
            cache.on_sample(1, newer.clone()).await.unwrap(),
            CacheOnSampleOutcome::InsertedFirst
        );
        // Older arrives second → Replay.
        assert!(matches!(
            cache.on_sample(1, older).await,
            Err(CacheOnSampleError::Replay)
        ));
        // Cached state unchanged: still the newer.
        let snap = cache.get_cached(1).await.unwrap();
        assert_eq!(snap.shared_at, "200");
    }

    #[tokio::test]
    async fn state_replay_newer_hlc_accepted() {
        let (signer, identity_pub) = build_identity([101u8; 32]);
        let peer_addr = OwnerAddr(
            harmony_identity::Identity::from_public_bytes(&identity_pub)
                .unwrap()
                .address_hash,
        );
        let cache = ProfileBroadcastCache::default();
        cache.register(2, peer_addr).await;

        let older = sign_broadcast(
            &signer,
            identity_pub,
            vec![fixture_space_id(1)],
            fixture_hlc(100),
        )
        .unwrap();
        let newer = sign_broadcast(
            &signer,
            identity_pub,
            vec![fixture_space_id(2)],
            fixture_hlc(200),
        )
        .unwrap();

        assert_eq!(
            cache.on_sample(2, older).await.unwrap(),
            CacheOnSampleOutcome::InsertedFirst
        );
        assert_eq!(
            cache.on_sample(2, newer).await.unwrap(),
            CacheOnSampleOutcome::Replaced
        );
        // Cached state updated to the newer.
        let snap = cache.get_cached(2).await.unwrap();
        assert_eq!(snap.shared_at, "200");
        assert_eq!(snap.community_ids, vec![hex::encode([2u8; 16])]);
    }
}
