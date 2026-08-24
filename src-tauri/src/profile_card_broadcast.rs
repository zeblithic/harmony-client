//! ZEB-341 — owner_id-keyed, EnrollmentCert-verified profile card broadcast.
//! Sibling to `profile_broadcast.rs`. Carries a peer's display name + status,
//! bound to their harmony-owner `owner_id` via the ZEB-339 cert model.
//! Spec: docs/specs/2026-05-30-zeb-341-profile-cards-design.md

use crate::owner_state_crypto::{
    canonical_cbor_encode, sealed::CanonicalPayloadSealed, CanonicalPayload, CryptoError,
};
use crate::owner_state_types::Hlc;
use crate::persistent_card_store::{PersistedCard, PersistentCardStore};
use ed25519_dalek::{Signer, SigningKey};
use harmony_owner::certs::EnrollmentCert;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use tokio::sync::Mutex;

pub use crate::profile_broadcast::SubscriptionId;

pub const PROFILE_CARD_TOPIC_PREFIX: &str = "harmony/discovery/profile/owner/";
pub const MAX_DISPLAY_NAME_BYTES: usize = 64;
pub const MAX_STATUS_TEXT_BYTES: usize = 128;
pub const MAX_CARD_WIRE_BYTES: usize = 4_096;

/// Build a broadcast topic key for the given owner_id.
pub fn card_topic_for(owner_id: &[u8; 16]) -> String {
    format!("{PROFILE_CARD_TOPIC_PREFIX}{}/card", hex::encode(owner_id))
}

/// ZEB-341 wire type. Spec §4.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileCardBroadcast {
    #[serde(
        rename = "oi",
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub owner_id: [u8; 16],
    #[serde(rename = "dn")]
    pub display_name: String,
    #[serde(rename = "st")]
    pub status_text: String,
    #[serde(
        rename = "av",
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "crate::owner_state_types::serialize_optional_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_optional_bytes_from_bstr"
    )]
    pub avatar_cid: Option<[u8; 32]>,
    #[serde(
        rename = "pp",
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "crate::owner_state_types::serialize_optional_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_optional_bytes_from_bstr"
    )]
    pub profile_page_root: Option<[u8; 32]>,
    #[serde(rename = "en")]
    pub enrollment: EnrollmentCert,
    #[serde(rename = "sa")]
    pub shared_at: Hlc,
    #[serde(
        rename = "sg",
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub signature: [u8; 64],
    /// ZEB-677: Master-issued signer certs backing a Quorum-issued
    /// `enrollment`. Empty for Master-issued certs (key omitted on the wire;
    /// old peers ignore it). Unlike the deposit frames, this field sits
    /// INSIDE the card's whole-struct signature — the presenting side must
    /// populate it before `sign_card`, and old cards (empty default) still
    /// verify because signer and verifier encode identically.
    #[serde(rename = "eb", default, skip_serializing_if = "Vec::is_empty")]
    pub signer_certs: Vec<EnrollmentCert>,
}

impl CanonicalPayloadSealed for ProfileCardBroadcast {}
impl CanonicalPayload for ProfileCardBroadcast {}

/// Errors from `sign_card`.
#[derive(Debug, thiserror::Error)]
pub enum CardError {
    #[error("display_name exceeds {MAX_DISPLAY_NAME_BYTES} bytes")]
    DisplayNameTooLong,
    #[error("status_text exceeds {MAX_STATUS_TEXT_BYTES} bytes")]
    StatusTextTooLong,
    #[error("cert.owner_id does not match requested owner_id")]
    EnrollmentOwnerMismatch,
    #[error("signer does not match enrollment device ed25519 key")]
    SignerKeyMismatch,
    /// ZEB-677 (Qodo PR #458): a Quorum-issued cert signed without its
    /// signer-cert bundle would produce a card EVERY verifier rejects —
    /// fail at sign time instead of emitting an unverifiable card.
    #[error("quorum-issued cert requires a signer bundle (use sign_card_with_bundle)")]
    QuorumCertRequiresBundle,
    #[error("canonical CBOR encode failed: {0}")]
    Encode(#[from] CryptoError),
}

/// Build + Ed25519-sign a card over canonical CBOR with `signature` zeroed.
/// `signer` MUST be the enrolled device key (pub ==
/// `enrollment.device_pubkeys.classical.ed25519_verify`). Master-issued
/// certs only — a Quorum-issued cert without its bundle would sign
/// successfully but fail EVERY peer's verification, so this fails fast
/// with [`CardError::QuorumCertRequiresBundle`]; use
/// [`sign_card_with_bundle`] instead (ZEB-677).
#[allow(clippy::too_many_arguments)]
pub fn sign_card(
    signer: &SigningKey,
    owner_id: [u8; 16],
    display_name: String,
    status_text: String,
    avatar_cid: Option<[u8; 32]>,
    profile_page_root: Option<[u8; 32]>,
    enrollment: EnrollmentCert,
    shared_at: Hlc,
) -> Result<ProfileCardBroadcast, CardError> {
    if matches!(
        enrollment.issuer,
        harmony_owner::certs::EnrollmentIssuer::Quorum { .. }
    ) {
        return Err(CardError::QuorumCertRequiresBundle);
    }
    sign_card_with_bundle(
        signer,
        owner_id,
        display_name,
        status_text,
        avatar_cid,
        profile_page_root,
        enrollment,
        Vec::new(),
        shared_at,
    )
}

/// [`sign_card`] with a signer-cert bundle for Quorum-issued `enrollment`
/// certs (ZEB-677). The bundle sits inside the card's whole-struct
/// signature, so it is bound at sign time.
#[allow(clippy::too_many_arguments)]
pub fn sign_card_with_bundle(
    signer: &SigningKey,
    owner_id: [u8; 16],
    display_name: String,
    status_text: String,
    avatar_cid: Option<[u8; 32]>,
    profile_page_root: Option<[u8; 32]>,
    enrollment: EnrollmentCert,
    signer_certs: Vec<EnrollmentCert>,
    shared_at: Hlc,
) -> Result<ProfileCardBroadcast, CardError> {
    // Fail-fast on cert/owner + signer/key binding so we never emit a card that
    // can't verify at peers (verify_card enforces the same bindings).
    if enrollment.owner_id != owner_id {
        return Err(CardError::EnrollmentOwnerMismatch);
    }
    if signer.verifying_key().to_bytes() != enrollment.device_pubkeys.classical.ed25519_verify {
        return Err(CardError::SignerKeyMismatch);
    }
    if display_name.len() > MAX_DISPLAY_NAME_BYTES {
        return Err(CardError::DisplayNameTooLong);
    }
    if status_text.len() > MAX_STATUS_TEXT_BYTES {
        return Err(CardError::StatusTextTooLong);
    }
    let mut card = ProfileCardBroadcast {
        owner_id,
        display_name,
        status_text,
        avatar_cid,
        profile_page_root,
        enrollment,
        shared_at,
        signature: [0u8; 64],
        signer_certs,
    };
    let bytes = canonical_cbor_encode(&card)?;
    card.signature = signer.sign(&bytes).to_bytes();
    Ok(card)
}

/// Errors from `verify_card`.
#[derive(Debug, thiserror::Error)]
pub enum CardVerifyError {
    #[error("display_name exceeds {MAX_DISPLAY_NAME_BYTES} bytes")]
    DisplayNameTooLong,
    #[error("status_text exceeds {MAX_STATUS_TEXT_BYTES} bytes")]
    StatusTextTooLong,
    #[error("enrollment cert invalid")]
    EnrollmentCertInvalid,
    #[error("cert.owner_id does not match card.owner_id")]
    EnrollmentOwnerMismatch,
    #[error("Ed25519 signature verification failed")]
    SignatureInvalid,
    #[error("shared_at.wall_ms is implausibly far in the receiver's future")]
    SharedAtTooFarInFuture,
    #[error("canonical CBOR encode failed: {0}")]
    Encode(#[from] CryptoError),
}

/// Verify a card end-to-end. Returns the bound `owner_id` on success.
///
/// Subscriber-side attribution (returned owner_id == topic owner_id) is the
/// CALLER's responsibility (event-loop pool, a later task).
///
/// NOTE: the signed bytes are `canonical_cbor_encode(card_with_sig_zeroed)` over
/// the WHOLE struct — same construction as `sign_card`. Do NOT re-encode the
/// embedded EnrollmentCert via `harmony_owner::cbor::to_canonical`; that yields
/// different bytes (sorted keys) and the signature would not verify.
pub fn verify_card(
    card: &ProfileCardBroadcast,
    now_secs: u64,
) -> Result<[u8; 16], CardVerifyError> {
    if card.display_name.len() > MAX_DISPLAY_NAME_BYTES {
        return Err(CardVerifyError::DisplayNameTooLong);
    }
    if card.status_text.len() > MAX_STATUS_TEXT_BYTES {
        return Err(CardVerifyError::StatusTextTooLong);
    }
    // ZEB-849 (C4): reject an implausibly future-dated shared_at before it can
    // out-HLC every honest card. Control tier — a card pins IDENTITY fields
    // (name/avatar/profile-page), so the tight 5-min bound is correct.
    if crate::clock_trust::wall_exceeds_forward_skew_secs_logged(
        card.shared_at.wall_ms,
        now_secs,
        crate::clock_trust::MAX_FORWARD_SKEW_MS,
        "profile_card.shared_at",
    ) {
        return Err(CardVerifyError::SharedAtTooFarInFuture);
    }
    // ZEB-677: chokepoint verification — Master certs self-contained; Quorum
    // certs against the card's signer-cert bundle (depth-1). No-bundle
    // quorum certs still fail closed.
    let verified = crate::enrollment_verify::verify_enrollment_any_issuer(
        &card.enrollment,
        &card.signer_certs,
        Some(&card.owner_id),
        now_secs,
    )
    .map_err(|e| match e {
        crate::enrollment_verify::EnrollmentVerifyError::OwnerMismatch => {
            CardVerifyError::EnrollmentOwnerMismatch
        }
        _ => CardVerifyError::EnrollmentCertInvalid,
    })?;
    let vk = ed25519_dalek::VerifyingKey::from_bytes(&verified.device_ed25519)
        .map_err(|_| CardVerifyError::SignatureInvalid)?;
    let mut for_sig = card.clone();
    for_sig.signature = [0u8; 64];
    let bytes = canonical_cbor_encode(&for_sig)?;
    vk.verify_strict(
        &bytes,
        &ed25519_dalek::Signature::from_bytes(&card.signature),
    )
    .map_err(|_| CardVerifyError::SignatureInvalid)?;
    Ok(card.owner_id)
}

/// ZEB-921: display name from cached self-card wire bytes (`CardWire.1`).
/// Decode-only — the publisher cache is written exclusively by our own
/// publish path with bytes we just signed (`publish_now`), so signature /
/// cert verification would add plumbing without a new guarantee. `None`
/// on decode failure (defensive; self-produced bytes always decode).
pub fn decode_card_display_name(bytes: &[u8]) -> Option<String> {
    ciborium::de::from_reader::<ProfileCardBroadcast, _>(bytes)
        .ok()
        .map(|c| c.display_name)
}

/// Snapshot of the latest verified profile card broadcast for a subscription.
/// Wire keys are camelCase to match the frontend DTO convention.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredCardInfo {
    #[serde(rename = "ownerIdHex")]
    pub owner_id_hex: String,
    #[serde(rename = "displayName")]
    pub display_name: String,
    #[serde(rename = "statusText")]
    pub status_text: String,
    #[serde(rename = "avatarCid", skip_serializing_if = "Option::is_none")]
    pub avatar_cid: Option<String>,
    #[serde(rename = "profilePageRoot", skip_serializing_if = "Option::is_none")]
    pub profile_page_root: Option<String>,
}

/// Per-subscription cached card entry. Holds the highest-HLC verified card
/// observed so far + the expected owner_id the subscription targets.
#[derive(Debug, Clone)]
struct CachedCard {
    owner_id: [u8; 16],
    display_name: String,
    status_text: String,
    avatar_cid: Option<[u8; 32]>,
    profile_page_root: Option<[u8; 32]>,
    shared_at: Hlc,
}

impl CachedCard {
    fn to_discovered(&self) -> DiscoveredCardInfo {
        DiscoveredCardInfo {
            owner_id_hex: hex::encode(self.owner_id),
            display_name: self.display_name.clone(),
            status_text: self.status_text.clone(),
            avatar_cid: self.avatar_cid.map(hex::encode),
            profile_page_root: self.profile_page_root.map(hex::encode),
        }
    }
}

/// Per-slot state: (expected_owner, latest cached card).
#[derive(Debug, Default, Clone)]
struct CardSlot {
    expected_owner: [u8; 16],
    latest: Option<CachedCard>,
}

/// In-process cache of verified peer profile card broadcasts. Spec §7.
///
/// Newer-HLC-wins: uses `Hlc::is_strictly_newer_than` for ordering (lexicographic
/// on `(wall_ms, logical, device_id)`). `Hlc` does not derive `PartialOrd`.
#[derive(Default)]
pub struct ProfileCardCache {
    slots: Mutex<HashMap<SubscriptionId, CardSlot>>,
    /// ZEB-839: durable last-known-card store. Written through on every
    /// verified newer card and consulted as a fallback when a live slot has
    /// no card yet (offline peer / fresh restart). `None`/unset until
    /// `set_store` runs at `start_node` (the cache is constructed before the
    /// owner identity is guaranteed loaded).
    store: OnceLock<Arc<PersistentCardStore>>,
}

impl ProfileCardCache {
    /// ZEB-839: attach the durable card store. Idempotent — a second call is a
    /// no-op (the store is owner-scoped and set once per node start).
    pub fn set_store(&self, store: Arc<PersistentCardStore>) {
        let _ = self.store.set(store);
    }

    /// Register a subscription. Idempotent — a pre-existing entry for the
    /// same `sub` is left untouched (avoids evicting a cached card on re-register).
    pub async fn register(&self, sub: SubscriptionId, expected_owner: [u8; 16]) {
        self.slots.lock().await.entry(sub).or_insert(CardSlot {
            expected_owner,
            latest: None,
        });
    }

    /// Drop a subscription from the cache. Idempotent — missing sub is OK.
    pub async fn drop_subscription(&self, sub: SubscriptionId) {
        self.slots.lock().await.remove(&sub);
    }

    /// Insert a VERIFIED card (caller already ran `verify_card` + attribution).
    ///
    /// Silently ignores the card if:
    /// - the subscription slot does not exist,
    /// - `card.owner_id` != the slot's expected owner (defense-in-depth), or
    /// - the card is not strictly newer than the cached entry (replay guard).
    pub async fn insert_verified(&self, sub: SubscriptionId, card: &ProfileCardBroadcast) {
        let is_newer;
        {
            let mut g = self.slots.lock().await;
            let Some(slot) = g.get_mut(&sub) else {
                return;
            };
            if card.owner_id != slot.expected_owner {
                return;
            }
            is_newer = match &slot.latest {
                Some(e) => card.shared_at.is_strictly_newer_than(&e.shared_at),
                None => true,
            };
            if is_newer {
                slot.latest = Some(CachedCard {
                    owner_id: card.owner_id,
                    display_name: card.display_name.clone(),
                    status_text: card.status_text.clone(),
                    avatar_cid: card.avatar_cid,
                    profile_page_root: card.profile_page_root,
                    shared_at: card.shared_at.clone(),
                });
            }
        }
        // ZEB-839 write-through: mirror the verified newer card into the durable
        // store (owner-keyed, newer-HLC-wins) and flush off the async hot path.
        // The store applies its own newer-wins check across ALL slots for this
        // owner, so a stale sample from one slot can't clobber a newer one held
        // under another slot.
        if is_newer {
            if let Some(store) = self.store.get() {
                let persisted = PersistedCard::from_broadcast(card);
                if store.upsert(&persisted) {
                    let store = Arc::clone(store);
                    tokio::task::spawn_blocking(move || {
                        if let Err(e) = store.persist() {
                            tracing::warn!(error = %e, "ZEB-839: profile-card store flush failed");
                        }
                    });
                }
            }
        }
    }

    /// Snapshot the newest verified card for a subscription as the frontend DTO.
    ///
    /// Returns whichever of {live slot, durable store} is HLC-newer (live wins
    /// ties). The store aggregates the newest verified card for this owner
    /// across ALL subscriptions (write-through, newer-HLC-wins), so it covers:
    /// a slot with no card yet (offline peer / fresh restart), AND a slot that
    /// lost a best-effort Zenoh sample a *different* subscription received
    /// (e.g. the two `MemberCardService` instances). A missing slot (never
    /// subscribed) has no owner to key on, so it stays `None`.
    pub async fn get_cached(&self, sub: SubscriptionId) -> Option<DiscoveredCardInfo> {
        let (expected_owner, live) = {
            let g = self.slots.lock().await;
            let slot = g.get(&sub)?;
            (slot.expected_owner, slot.latest.clone())
        };
        let stored = self.store.get().and_then(|s| s.get(&expected_owner));
        match (live, stored) {
            (Some(live), Some(stored)) => Some(
                if stored.shared_at.is_strictly_newer_than(&live.shared_at) {
                    stored.to_discovered()
                } else {
                    live.to_discovered()
                },
            ),
            (Some(live), None) => Some(live.to_discovered()),
            (None, Some(stored)) => Some(stored.to_discovered()),
            (None, None) => None,
        }
    }

    /// Map of `owner_id` → newest verified card's display name, across all
    /// subscription slots (newest by HLC wins; a tie keeps the first seen).
    /// Used by the Network Health snapshot to label peers in ONE pass —
    /// O(slots) under a single lock, vs O(peers × slots) per-peer lookups.
    pub async fn display_names_by_owner(&self) -> std::collections::HashMap<[u8; 16], String> {
        let g = self.slots.lock().await;
        let mut best: std::collections::HashMap<[u8; 16], &CachedCard> =
            std::collections::HashMap::new();
        for slot in g.values() {
            let Some(c) = slot.latest.as_ref() else {
                continue;
            };
            match best.get(&c.owner_id) {
                // Keep the existing entry unless this card is strictly newer.
                Some(existing) if !c.shared_at.is_strictly_newer_than(&existing.shared_at) => {}
                _ => {
                    best.insert(c.owner_id, c);
                }
            }
        }
        let mut result: std::collections::HashMap<[u8; 16], String> = best
            .into_iter()
            .map(|(owner, c)| (owner, c.display_name.clone()))
            .collect();
        // ZEB-839: union last-known names from the durable store for any owner
        // not covered by a live slot (offline peers / fresh restart). Live wins.
        if let Some(store) = self.store.get() {
            for (owner, name) in store.display_names_by_owner() {
                result.entry(owner).or_insert(name);
            }
        }
        result
    }
}

/// Sign a card, canonical-CBOR-encode it, and publish to its owner_id topic.
/// Returns the (topic, bytes) actually published so callers can cache them
/// for periodic refresh.
#[allow(clippy::too_many_arguments)]
pub async fn publish_card_once(
    signer: &SigningKey,
    owner_id: [u8; 16],
    display_name: String,
    status_text: String,
    avatar_cid: Option<[u8; 32]>,
    profile_page_root: Option<[u8; 32]>,
    enrollment: EnrollmentCert,
    shared_at: Hlc,
    sink: &dyn crate::profile_broadcast::ProfileBroadcastPublishSink,
) -> Result<(String, Vec<u8>), String> {
    let card = sign_card(
        signer,
        owner_id,
        display_name,
        status_text,
        avatar_cid,
        profile_page_root,
        enrollment,
        shared_at,
    )
    .map_err(|e| e.to_string())?;
    let bytes = canonical_cbor_encode(&card).map_err(|e| e.to_string())?;
    let topic = card_topic_for(&owner_id);
    sink.publish(topic.clone(), bytes.clone()).await?;
    Ok((topic, bytes))
}

/// Re-publish the last self-card every `refresh` so peers that subscribe
/// AFTER the user's last profile-save still receive it (the subscriber side
/// uses live declare_subscriber with no retained value). Name/status are NOT
/// persisted backend-side, so this caches the already-signed bytes rather
/// than re-reading a source. HLC is baked in at sign time; re-publishing the
/// same bytes is idempotent at peers (equal-HLC -> no-op via newer-wins).
pub const PROFILE_CARD_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(600);

/// ZEB-568: short initial-burst schedule. A peer who has JUST subscribed (e.g.
/// a member who just joined) races the steady 600s refresh — without an early
/// re-publish their roster shows "Name unavailable" until the next 600s tick.
/// On spawn we re-emit the cached card at these offsets (absolute, measured
/// from spawn) BEFORE entering the steady `interval(refresh)` loop, so a late
/// subscriber converges in seconds rather than minutes. Re-publishing identical
/// bytes is idempotent at peers (equal-HLC -> no-op via newer-wins). Test builds
/// override these to keep the burst-cadence test fast (see `BOOT_BURST_OFFSETS`).
#[cfg(not(test))]
const BOOT_BURST_OFFSETS: &[std::time::Duration] = &[
    std::time::Duration::from_secs(3),
    std::time::Duration::from_secs(10),
    std::time::Duration::from_secs(30),
];
#[cfg(test)]
const BOOT_BURST_OFFSETS: &[std::time::Duration] = &[
    std::time::Duration::from_millis(10),
    std::time::Duration::from_millis(20),
    std::time::Duration::from_millis(30),
];

/// A published card on the wire: its Zenoh topic and the signed canonical-CBOR
/// bytes. Cached so the refresh task can re-emit the exact same payload.
type CardWire = (String, Vec<u8>);

/// Caches the last signed self-card and re-publishes it on an interval so that
/// peers who subscribe after the user's last profile-save still receive it.
pub struct ProfileCardPublisher {
    latest: std::sync::Arc<Mutex<Option<CardWire>>>, // (topic, signed CBOR)
    sink: std::sync::Arc<dyn crate::profile_broadcast::ProfileBroadcastPublishSink>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl ProfileCardPublisher {
    pub fn spawn(
        sink: std::sync::Arc<dyn crate::profile_broadcast::ProfileBroadcastPublishSink>,
        refresh: std::time::Duration,
    ) -> std::sync::Arc<Self> {
        Self::spawn_inner(sink, refresh, true)
    }

    /// Test-only: spawn WITHOUT the initial boot burst. Lets a unit test that
    /// asserts exact publish counts (`republish_cached_reemits_and_is_noop_when_empty`)
    /// be deterministic — the burst runs at 10/20/30ms in test builds and would
    /// otherwise add background re-publishes mid-assertion. The burst path itself
    /// is covered by `card_publisher_initial_burst_republishes_quickly`.
    #[cfg(test)]
    fn spawn_no_burst(
        sink: std::sync::Arc<dyn crate::profile_broadcast::ProfileBroadcastPublishSink>,
        refresh: std::time::Duration,
    ) -> std::sync::Arc<Self> {
        Self::spawn_inner(sink, refresh, false)
    }

    fn spawn_inner(
        sink: std::sync::Arc<dyn crate::profile_broadcast::ProfileBroadcastPublishSink>,
        refresh: std::time::Duration,
        run_boot_burst: bool,
    ) -> std::sync::Arc<Self> {
        let latest: std::sync::Arc<Mutex<Option<CardWire>>> = std::sync::Arc::new(Mutex::new(None));
        let task = {
            let latest_for_task = std::sync::Arc::clone(&latest);
            let sink_for_task = std::sync::Arc::clone(&sink);
            tokio::spawn(async move {
                // ZEB-568: initial-burst schedule — re-publish the cached card a
                // few times soon after spawn so a peer that subscribes right
                // after we boot (or after a profile-save) converges in seconds
                // instead of waiting up to a full `refresh` (600s). Replaces the
                // old "consume the immediate first tick then wait a full refresh"
                // behavior. No-op while nothing is cached yet.
                //
                // BOOT_BURST_OFFSETS are absolute offsets FROM SPAWN, so sleep
                // only the delta to the next offset (a plain `sleep(*offset)`
                // each iteration would make them cumulative: 3/13/43s, not the
                // documented 3/10/30s — see ZEB-568 review).
                if run_boot_burst {
                    let mut prev = std::time::Duration::ZERO;
                    for offset in BOOT_BURST_OFFSETS {
                        tokio::time::sleep(offset.saturating_sub(prev)).await;
                        prev = *offset;
                        republish_snapshot(&latest_for_task, sink_for_task.as_ref()).await;
                    }
                }
                // Steady-state: re-publish every `refresh` for any peer that
                // missed all of the above. Cadence (period) UNCHANGED (600s); it
                // simply starts after the short burst completes.
                let mut iv = tokio::time::interval(refresh);
                iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                iv.tick().await; // consume immediate first tick (burst covered boot)
                loop {
                    iv.tick().await;
                    republish_snapshot(&latest_for_task, sink_for_task.as_ref()).await;
                }
            })
        };
        std::sync::Arc::new(Self {
            latest,
            sink,
            task: Mutex::new(Some(task)),
        })
    }

    /// Publish a freshly-signed card NOW and remember it for periodic refresh.
    pub async fn publish_now(&self, topic: String, bytes: Vec<u8>) -> Result<(), String> {
        *self.latest.lock().await = Some((topic.clone(), bytes.clone()));
        self.sink.publish(topic, bytes).await
    }

    /// ZEB-568: re-emit the cached `(topic, bytes)` self-card without
    /// re-signing. Used by the eager-republish receiver task when a new peer /
    /// member is observed, so a freshly-subscribed member receives our card in
    /// seconds rather than waiting up to a full 600s `refresh`. No-op (returns
    /// `Ok(())`) when nothing is cached yet (no profile-save / boot publish has
    /// happened). Re-publishing identical bytes is idempotent at peers
    /// (equal-HLC -> no-op via newer-wins).
    pub async fn republish_cached(&self) -> Result<(), String> {
        let snapshot = self.latest.lock().await.clone();
        match snapshot {
            Some((topic, bytes)) => self.sink.publish(topic, bytes).await,
            None => Ok(()),
        }
    }

    /// ZEB-884: a cloned handle to the cached `latest` self-card, so a Zenoh
    /// **queryable** declared elsewhere (where the shared session lives) can
    /// answer a late subscriber's query-on-subscribe `get` with the exact cached
    /// signed bytes — no re-sign, no separate cache. `None` until a card has been
    /// published (a node that never published has no card to serve).
    pub fn latest_handle(&self) -> std::sync::Arc<Mutex<Option<CardWire>>> {
        std::sync::Arc::clone(&self.latest)
    }

    /// Abort the refresh task. Idempotent. (Mirrors ProfileBroadcastPublisher::shutdown.)
    pub async fn shutdown(&self) {
        if let Some(h) = self.task.lock().await.take() {
            h.abort();
        }
    }
}

/// Re-publish the currently-cached self-card via `sink`, logging (but not
/// propagating) a publish failure. Shared by the background task's initial
/// burst and steady-state refresh. No-op while nothing is cached yet.
async fn republish_snapshot(
    latest: &Mutex<Option<CardWire>>,
    sink: &dyn crate::profile_broadcast::ProfileBroadcastPublishSink,
) {
    let snapshot = latest.lock().await.clone();
    if let Some((topic, bytes)) = snapshot {
        if let Err(e) = sink.publish(topic, bytes).await {
            tracing::warn!(error = %e, "profile card refresh publish failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct CapturingSink {
        out: std::sync::Arc<Mutex<Vec<CardWire>>>,
    }

    #[async_trait::async_trait]
    impl crate::profile_broadcast::ProfileBroadcastPublishSink for CapturingSink {
        async fn publish(&self, topic: String, payload: Vec<u8>) -> Result<(), String> {
            self.out.lock().await.push((topic, payload));
            Ok(())
        }
    }

    #[tokio::test]
    async fn publish_card_once_emits_a_card_that_verifies() {
        let owner = crate::community_membership::mint_test_owner(0x70);
        let out = std::sync::Arc::new(Mutex::new(Vec::<(String, Vec<u8>)>::new()));
        let sink = CapturingSink { out: out.clone() };
        let (topic, _bytes) = publish_card_once(
            &owner.device_key,
            owner.owner.0,
            "Pat".into(),
            "afk".into(),
            None,
            None,
            owner.cert.clone(),
            Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "d".into(),
            },
            &sink,
        )
        .await
        .expect("publish");
        assert_eq!(topic, card_topic_for(&owner.owner.0));
        let g = out.lock().await;
        assert_eq!(g.len(), 1);
        let decoded: ProfileCardBroadcast = ciborium::de::from_reader(&g[0].1[..]).unwrap();
        assert_eq!(verify_card(&decoded, 0).unwrap(), owner.owner.0);
    }

    #[tokio::test]
    async fn card_publisher_publishes_now_and_refreshes() {
        let owner = crate::community_membership::mint_test_owner(0x71);
        let out = std::sync::Arc::new(Mutex::new(Vec::<(String, Vec<u8>)>::new()));
        let sink = std::sync::Arc::new(CapturingSink { out: out.clone() });
        let pubr = ProfileCardPublisher::spawn(sink.clone(), std::time::Duration::from_millis(40));
        let card = sign_card(
            &owner.device_key,
            owner.owner.0,
            "Al".into(),
            "".into(),
            None,
            None,
            owner.cert.clone(),
            Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "d".into(),
            },
        )
        .unwrap();
        let bytes = canonical_cbor_encode(&card).unwrap();
        pubr.publish_now(card_topic_for(&owner.owner.0), bytes)
            .await
            .unwrap();
        assert_eq!(out.lock().await.len(), 1, "publish_now emits immediately");
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        pubr.shutdown().await;
        let n = out.lock().await.len();
        assert!(n >= 2, "refresh re-published at least once (got {n})");
    }

    /// ZEB-568: the initial-burst schedule re-publishes the cached card several
    /// times soon after spawn (BOOT_BURST_OFFSETS = 10/20/30ms in test builds),
    /// so a late subscriber converges in well under a full `refresh`. With a
    /// long refresh (1s) the steady loop can't fire inside the test window —
    /// so the extra publishes MUST come from the burst.
    #[tokio::test]
    async fn card_publisher_initial_burst_republishes_quickly() {
        let owner = crate::community_membership::mint_test_owner(0x72);
        let out = std::sync::Arc::new(Mutex::new(Vec::<(String, Vec<u8>)>::new()));
        let sink = std::sync::Arc::new(CapturingSink { out: out.clone() });
        // refresh is 1s: far longer than the whole burst (30ms) + our 80ms wait,
        // so any count > 1 is attributable to the burst, not the steady loop.
        let pubr = ProfileCardPublisher::spawn(sink.clone(), std::time::Duration::from_secs(1));
        let card = sign_card(
            &owner.device_key,
            owner.owner.0,
            "Bz".into(),
            "".into(),
            None,
            None,
            owner.cert.clone(),
            Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "d".into(),
            },
        )
        .unwrap();
        let bytes = canonical_cbor_encode(&card).unwrap();
        pubr.publish_now(card_topic_for(&owner.owner.0), bytes)
            .await
            .unwrap();
        assert_eq!(out.lock().await.len(), 1, "publish_now emits immediately");
        // Wait past all three burst offsets (10/20/30ms) with margin.
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        pubr.shutdown().await;
        let n = out.lock().await.len();
        // 1 publish_now + 3 burst re-publishes; assert at least 3 total so the
        // test is robust to scheduler jitter on the last offset.
        assert!(
            n >= 3,
            "initial burst re-published the cached card multiple times soon after spawn (got {n})"
        );
    }

    /// ZEB-884: `latest_handle()` is a live `Arc::clone` view of the cached
    /// self-card — `None` before any publish, then observing the exact
    /// `(topic, bytes)` that `publish_now` stored. This is what the publisher-side
    /// queryable reads to answer a query-on-subscribe without re-signing.
    #[tokio::test]
    async fn latest_handle_observes_published_card() {
        let owner = crate::community_membership::mint_test_owner(0x74);
        let out = std::sync::Arc::new(Mutex::new(Vec::<(String, Vec<u8>)>::new()));
        let sink = std::sync::Arc::new(CapturingSink { out: out.clone() });
        // spawn_no_burst + long refresh: `latest` is only written by publish_now,
        // so background republishes can't affect it, but keep the window quiet.
        let pubr = ProfileCardPublisher::spawn_no_burst(
            sink.clone(),
            std::time::Duration::from_secs(3600),
        );
        let handle = pubr.latest_handle();
        assert!(
            handle.lock().await.is_none(),
            "nothing published yet -> handle observes None"
        );
        let card = sign_card(
            &owner.device_key,
            owner.owner.0,
            "Qy".into(),
            "".into(),
            None,
            None,
            owner.cert.clone(),
            Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "d".into(),
            },
        )
        .unwrap();
        let bytes = canonical_cbor_encode(&card).unwrap();
        let topic = card_topic_for(&owner.owner.0);
        pubr.publish_now(topic.clone(), bytes.clone())
            .await
            .unwrap();
        assert_eq!(
            handle.lock().await.clone(),
            Some((topic, bytes)),
            "the same Arc handle now observes the published (topic, bytes)"
        );
    }

    /// ZEB-568: republish_cached re-emits the last cached card after a
    /// publish_now, and is a no-op (Ok, zero publishes) when nothing is cached.
    #[tokio::test]
    async fn republish_cached_reemits_and_is_noop_when_empty() {
        let owner = crate::community_membership::mint_test_owner(0x73);
        let out = std::sync::Arc::new(Mutex::new(Vec::<(String, Vec<u8>)>::new()));
        let sink = std::sync::Arc::new(CapturingSink { out: out.clone() });
        // spawn_no_burst + long refresh so neither the boot burst nor the steady
        // loop can add publishes inside this test's window — every captured
        // publish here is explicit, making the exact-count assertions
        // deterministic. (The burst path is covered by
        // `card_publisher_initial_burst_republishes_quickly`.)
        let pubr = ProfileCardPublisher::spawn_no_burst(
            sink.clone(),
            std::time::Duration::from_secs(3600),
        );

        // Nothing cached yet -> no-op, Ok, zero publishes.
        pubr.republish_cached().await.expect("noop ok");
        assert_eq!(
            out.lock().await.len(),
            0,
            "republish_cached is a no-op before anything is cached"
        );

        let card = sign_card(
            &owner.device_key,
            owner.owner.0,
            "Re".into(),
            "".into(),
            None,
            None,
            owner.cert.clone(),
            Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "d".into(),
            },
        )
        .unwrap();
        let bytes = canonical_cbor_encode(&card).unwrap();
        let topic = card_topic_for(&owner.owner.0);
        pubr.publish_now(topic.clone(), bytes.clone())
            .await
            .unwrap();
        assert_eq!(out.lock().await.len(), 1, "publish_now emits once");

        // republish_cached re-emits the SAME (topic, bytes).
        pubr.republish_cached().await.expect("republish ok");
        let g = out.lock().await;
        assert_eq!(g.len(), 2, "republish_cached re-emitted the cached card");
        assert_eq!(g[1].0, topic, "re-emitted to the same topic");
        assert_eq!(g[1].1, bytes, "re-emitted byte-identical card");
    }

    #[tokio::test]
    async fn card_cache_register_insert_get_roundtrip() {
        let cache = ProfileCardCache::default();
        let owner = crate::community_membership::mint_test_owner(0x60);
        cache.register(1, owner.owner.0).await;
        assert_eq!(cache.get_cached(1).await, None, "no broadcast yet -> None");
        let card = sign_card(
            &owner.device_key,
            owner.owner.0,
            "Cy".into(),
            "yo".into(),
            None,
            None,
            owner.cert.clone(),
            Hlc {
                wall_ms: 5,
                logical: 0,
                device_id: "d".into(),
            },
        )
        .unwrap();
        cache.insert_verified(1, &card).await;
        let got = cache.get_cached(1).await.expect("cached");
        assert_eq!(got.display_name, "Cy");
        assert_eq!(got.status_text, "yo");
        assert_eq!(got.owner_id_hex, hex::encode(owner.owner.0));
        cache.drop_subscription(1).await;
        assert_eq!(cache.get_cached(1).await, None);
    }

    #[tokio::test]
    async fn card_cache_newer_hlc_wins() {
        let cache = ProfileCardCache::default();
        let owner = crate::community_membership::mint_test_owner(0x61);
        cache.register(2, owner.owner.0).await;
        let older = sign_card(
            &owner.device_key,
            owner.owner.0,
            "old".into(),
            "".into(),
            None,
            None,
            owner.cert.clone(),
            Hlc {
                wall_ms: 10,
                logical: 0,
                device_id: "d".into(),
            },
        )
        .unwrap();
        let newer = sign_card(
            &owner.device_key,
            owner.owner.0,
            "new".into(),
            "".into(),
            None,
            None,
            owner.cert.clone(),
            Hlc {
                wall_ms: 20,
                logical: 0,
                device_id: "d".into(),
            },
        )
        .unwrap();
        cache.insert_verified(2, &newer).await;
        cache.insert_verified(2, &older).await; // stale -> ignored
        assert_eq!(cache.get_cached(2).await.unwrap().display_name, "new");
    }

    #[tokio::test]
    async fn card_cache_insert_ignores_owner_id_mismatch() {
        // defense-in-depth: a card whose owner_id != the slot's expected owner is ignored
        let cache = ProfileCardCache::default();
        let a = crate::community_membership::mint_test_owner(0x62);
        let b = crate::community_membership::mint_test_owner(0x63);
        cache.register(3, a.owner.0).await; // slot expects owner A
        let b_card = sign_card(
            &b.device_key,
            b.owner.0,
            "B".into(),
            "".into(),
            None,
            None,
            b.cert.clone(),
            Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "d".into(),
            },
        )
        .unwrap();
        cache.insert_verified(3, &b_card).await; // owner_id == B != expected A -> ignored
        assert_eq!(cache.get_cached(3).await, None);
    }

    #[tokio::test]
    async fn display_names_by_owner_maps_owner_to_newest_name() {
        let cache = ProfileCardCache::default();
        let a = crate::community_membership::mint_test_owner(0x71);
        cache.register(7, a.owner.0).await;
        let card = sign_card(
            &a.device_key,
            a.owner.0,
            "Alice".into(),
            "".into(),
            None,
            None,
            a.cert.clone(),
            Hlc {
                wall_ms: 5,
                logical: 0,
                device_id: "d".into(),
            },
        )
        .unwrap();
        cache.insert_verified(7, &card).await;
        let names = cache.display_names_by_owner().await;
        assert_eq!(names.get(&a.owner.0), Some(&"Alice".to_string()));
        // Unknown owner → absent from the map.
        let b = crate::community_membership::mint_test_owner(0x72);
        assert_eq!(names.get(&b.owner.0), None);
    }

    // ---- ZEB-839: durable-store fallback + write-through ----

    fn seeded_store(
        dir: &std::path::Path,
        owner_hex: &str,
        card: &ProfileCardBroadcast,
    ) -> std::sync::Arc<crate::persistent_card_store::PersistentCardStore> {
        let store = std::sync::Arc::new(
            crate::persistent_card_store::PersistentCardStore::load_for_owner(crate::device_dataset_file::test_cipher(), dir, owner_hex),
        );
        store.upsert(&crate::persistent_card_store::PersistedCard::from_broadcast(card));
        store
    }

    #[tokio::test]
    async fn store_fallback_serves_last_known_when_slot_has_no_live_card() {
        // The core offline/restart fix: a registered subscription with no live
        // card resolves to the last-known card from the durable store.
        let dir = tempfile::tempdir().unwrap();
        let owner = crate::community_membership::mint_test_owner(0x80);
        let card = sign_card(
            &owner.device_key,
            owner.owner.0,
            "Offline Olly".into(),
            "brb".into(),
            None,
            None,
            owner.cert.clone(),
            Hlc {
                wall_ms: 5,
                logical: 0,
                device_id: "d".into(),
            },
        )
        .unwrap();
        let store = seeded_store(dir.path(), "owner80", &card);

        let cache = ProfileCardCache::default();
        cache.set_store(store);
        cache.register(1, owner.owner.0).await; // subscribed, but peer is offline (no live card)
        let got = cache
            .get_cached(1)
            .await
            .expect("store fallback serves last-known");
        assert_eq!(got.display_name, "Offline Olly");
        assert_eq!(got.status_text, "brb");
        assert_eq!(got.owner_id_hex, hex::encode(owner.owner.0));
    }

    #[tokio::test]
    async fn store_fallback_absent_without_registered_slot() {
        // No slot → no owner to key on → None even though the store has a card.
        let dir = tempfile::tempdir().unwrap();
        let owner = crate::community_membership::mint_test_owner(0x81);
        let card = sign_card(
            &owner.device_key,
            owner.owner.0,
            "Ghost".into(),
            "".into(),
            None,
            None,
            owner.cert.clone(),
            Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "d".into(),
            },
        )
        .unwrap();
        let store = seeded_store(dir.path(), "owner81", &card);
        let cache = ProfileCardCache::default();
        cache.set_store(store);
        assert_eq!(cache.get_cached(999).await, None, "no slot -> no fallback");
    }

    #[tokio::test]
    async fn live_card_wins_over_store_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let owner = crate::community_membership::mint_test_owner(0x82);
        let stale = sign_card(
            &owner.device_key,
            owner.owner.0,
            "Stale Name".into(),
            "".into(),
            None,
            None,
            owner.cert.clone(),
            Hlc {
                wall_ms: 5,
                logical: 0,
                device_id: "d".into(),
            },
        )
        .unwrap();
        let store = seeded_store(dir.path(), "owner82", &stale);
        let cache = ProfileCardCache::default();
        cache.set_store(store);
        cache.register(1, owner.owner.0).await;
        let live = sign_card(
            &owner.device_key,
            owner.owner.0,
            "Live Name".into(),
            "".into(),
            None,
            None,
            owner.cert.clone(),
            Hlc {
                wall_ms: 10,
                logical: 0,
                device_id: "d".into(),
            },
        )
        .unwrap();
        cache.insert_verified(1, &live).await;
        assert_eq!(cache.get_cached(1).await.unwrap().display_name, "Live Name");
    }

    #[tokio::test]
    async fn get_cached_prefers_store_when_slot_lost_a_newer_sample() {
        // Zenoh pub/sub is best-effort: this subscription's slot can hold an
        // older card while the durable store — fed by ANOTHER subscription's
        // write-through — holds a newer one for the same owner. get_cached must
        // surface the newer (Qodo #1 on PR #574).
        let dir = tempfile::tempdir().unwrap();
        let owner = crate::community_membership::mint_test_owner(0x86);
        let store = std::sync::Arc::new(
            crate::persistent_card_store::PersistentCardStore::load_for_owner(
                crate::device_dataset_file::test_cipher(),
                dir.path(),
                "owner86",
            ),
        );
        let cache = ProfileCardCache::default();
        cache.set_store(std::sync::Arc::clone(&store));
        cache.register(1, owner.owner.0).await;
        // This subscription receives only the older card.
        let old = sign_card(
            &owner.device_key,
            owner.owner.0,
            "Old".into(),
            "".into(),
            None,
            None,
            owner.cert.clone(),
            Hlc {
                wall_ms: 5,
                logical: 0,
                device_id: "d".into(),
            },
        )
        .unwrap();
        cache.insert_verified(1, &old).await;
        // A different subscription's write-through lands a newer card this slot missed.
        let newer = crate::persistent_card_store::PersistedCard::from_broadcast(
            &sign_card(
                &owner.device_key,
                owner.owner.0,
                "New".into(),
                "".into(),
                None,
                None,
                owner.cert.clone(),
                Hlc {
                    wall_ms: 20,
                    logical: 0,
                    device_id: "d".into(),
                },
            )
            .unwrap(),
        );
        store.upsert(&newer);
        assert_eq!(
            cache.get_cached(1).await.unwrap().display_name,
            "New",
            "store's newer card surfaces despite the older live slot"
        );
    }

    #[tokio::test]
    async fn insert_verified_writes_through_to_store() {
        // A verified newer card is mirrored into the durable store's in-memory
        // map synchronously (the disk flush is offloaded and not asserted here).
        let dir = tempfile::tempdir().unwrap();
        let owner = crate::community_membership::mint_test_owner(0x83);
        let store = std::sync::Arc::new(
            crate::persistent_card_store::PersistentCardStore::load_for_owner(
                crate::device_dataset_file::test_cipher(),
                dir.path(),
                "owner83",
            ),
        );
        let cache = ProfileCardCache::default();
        cache.set_store(std::sync::Arc::clone(&store));
        cache.register(1, owner.owner.0).await;
        let card = sign_card(
            &owner.device_key,
            owner.owner.0,
            "Written".into(),
            "".into(),
            None,
            None,
            owner.cert.clone(),
            Hlc {
                wall_ms: 7,
                logical: 0,
                device_id: "d".into(),
            },
        )
        .unwrap();
        cache.insert_verified(1, &card).await;
        assert_eq!(
            store.get(&owner.owner.0).unwrap().display_name,
            "Written",
            "write-through populated the durable store"
        );
    }

    #[tokio::test]
    async fn display_names_by_owner_unions_store_for_offline_owner() {
        let dir = tempfile::tempdir().unwrap();
        let offline = crate::community_membership::mint_test_owner(0x84);
        let offcard = sign_card(
            &offline.device_key,
            offline.owner.0,
            "Offline".into(),
            "".into(),
            None,
            None,
            offline.cert.clone(),
            Hlc {
                wall_ms: 3,
                logical: 0,
                device_id: "d".into(),
            },
        )
        .unwrap();
        let store = seeded_store(dir.path(), "ownerX", &offcard);
        let cache = ProfileCardCache::default();
        cache.set_store(store);
        // A different owner known live.
        let live = crate::community_membership::mint_test_owner(0x85);
        cache.register(1, live.owner.0).await;
        let livecard = sign_card(
            &live.device_key,
            live.owner.0,
            "LiveOne".into(),
            "".into(),
            None,
            None,
            live.cert.clone(),
            Hlc {
                wall_ms: 3,
                logical: 0,
                device_id: "d".into(),
            },
        )
        .unwrap();
        cache.insert_verified(1, &livecard).await;
        let names = cache.display_names_by_owner().await;
        assert_eq!(
            names.get(&live.owner.0),
            Some(&"LiveOne".to_string()),
            "live owner present"
        );
        assert_eq!(
            names.get(&offline.owner.0),
            Some(&"Offline".to_string()),
            "offline owner unioned from the durable store"
        );
    }

    #[test]
    fn sign_card_round_trips_and_signature_verifies_under_device_key() {
        use ed25519_dalek::Verifier;
        let owner = crate::community_membership::mint_test_owner(0x41);
        let card = sign_card(
            &owner.device_key,
            owner.owner.0,
            "Jake (Koya Dev)".into(),
            "building".into(),
            None,
            None,
            owner.cert.clone(),
            Hlc {
                wall_ms: 1_000,
                logical: 0,
                device_id: "dev".into(),
            },
        )
        .expect("sign");
        assert_eq!(card.owner_id, owner.owner.0);
        assert_eq!(card.display_name, "Jake (Koya Dev)");
        let mut for_sig = card.clone();
        for_sig.signature = [0u8; 64];
        let bytes = crate::owner_state_crypto::canonical_cbor_encode(&for_sig).unwrap();
        owner
            .device_key
            .verifying_key()
            .verify(
                &bytes,
                &ed25519_dalek::Signature::from_bytes(&card.signature),
            )
            .expect("sig verifies");
    }

    #[test]
    fn sign_card_rejects_overlong_name_and_status() {
        let owner = crate::community_membership::mint_test_owner(0x42);
        let hlc = Hlc {
            wall_ms: 1,
            logical: 0,
            device_id: "d".into(),
        };
        let long = "x".repeat(MAX_DISPLAY_NAME_BYTES + 1);
        assert!(matches!(
            sign_card(
                &owner.device_key,
                owner.owner.0,
                long,
                "ok".into(),
                None,
                None,
                owner.cert.clone(),
                hlc.clone()
            ),
            Err(CardError::DisplayNameTooLong)
        ));
        let longstatus = "y".repeat(MAX_STATUS_TEXT_BYTES + 1);
        assert!(matches!(
            sign_card(
                &owner.device_key,
                owner.owner.0,
                "ok".into(),
                longstatus,
                None,
                None,
                owner.cert,
                hlc
            ),
            Err(CardError::StatusTextTooLong)
        ));
    }

    #[test]
    fn sign_card_rejects_owner_cert_mismatch() {
        let a = crate::community_membership::mint_test_owner(0x43);
        // A's signer + A's cert, but a DIFFERENT requested owner_id.
        assert!(matches!(
            sign_card(
                &a.device_key,
                [0xFFu8; 16],
                "n".into(),
                "".into(),
                None,
                None,
                a.cert.clone(),
                Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "d".into(),
                },
            ),
            Err(CardError::EnrollmentOwnerMismatch)
        ));
    }

    #[test]
    fn sign_card_rejects_signer_key_mismatch() {
        let a = crate::community_membership::mint_test_owner(0x44);
        let b = crate::community_membership::mint_test_owner(0x45);
        // B's device_key against A's owner_id + A's cert → signer ≠ enrolled key.
        assert!(matches!(
            sign_card(
                &b.device_key,
                a.owner.0,
                "n".into(),
                "".into(),
                None,
                None,
                a.cert.clone(),
                Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "d".into(),
                },
            ),
            Err(CardError::SignerKeyMismatch)
        ));
    }

    #[test]
    fn sign_verify_round_trips_with_avatar() {
        let owner = crate::community_membership::mint_test_owner(0x5A);
        let avatar = Some([0xABu8; 32]);
        let card = sign_card(
            &owner.device_key,
            owner.owner.0,
            "Ann".into(),
            "hi".into(),
            avatar,
            None,
            owner.cert.clone(),
            Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "d".into(),
            },
        )
        .expect("sign");
        assert_eq!(card.avatar_cid, avatar);
        assert_eq!(verify_card(&card, 0).expect("verify"), owner.owner.0);
    }

    #[test]
    fn no_avatar_card_is_byte_identical_to_pre_field_encoding() {
        let owner = crate::community_membership::mint_test_owner(0x5B);
        let card = ProfileCardBroadcast {
            signer_certs: Vec::new(),
            owner_id: owner.owner.0,
            display_name: "Bo".into(),
            status_text: "".into(),
            avatar_cid: None,
            profile_page_root: None,
            enrollment: owner.cert,
            shared_at: Hlc {
                wall_ms: 9,
                logical: 1,
                device_id: "x".into(),
            },
            signature: [0u8; 64],
        };
        let bytes = crate::owner_state_crypto::canonical_cbor_encode(&card).expect("encode");
        assert_eq!(bytes[0], 0xA6, "no-avatar card must stay a 6-entry map");
    }

    #[test]
    fn no_optional_cids_card_stays_six_entry_map() {
        // ZEB-345: a card with BOTH avatar_cid and profile_page_root None must
        // remain a 6-entry map (0xA6) — proving the new `pp` field is additive
        // and byte-identical to a pre-ZEB-345 card when absent.
        let owner = crate::community_membership::mint_test_owner(0x5C);
        let card = sign_card(
            &owner.device_key,
            owner.owner.0,
            "Bo".into(),
            "".into(),
            None,
            None,
            owner.cert.clone(),
            Hlc {
                wall_ms: 9,
                logical: 1,
                device_id: "x".into(),
            },
        )
        .expect("sign");
        let mut for_sig = card.clone();
        for_sig.signature = [0u8; 64];
        let bytes = crate::owner_state_crypto::canonical_cbor_encode(&for_sig).expect("encode");
        assert_eq!(
            bytes[0], 0xA6,
            "card with no avatar and no profile_page_root must stay a 6-entry map"
        );
    }

    #[test]
    fn sign_verify_round_trips_with_profile_page_root() {
        let owner = crate::community_membership::mint_test_owner(0x5D);
        let page_root = Some([7u8; 32]);
        let card = sign_card(
            &owner.device_key,
            owner.owner.0,
            "Pp".into(),
            "doc".into(),
            None,
            page_root,
            owner.cert.clone(),
            Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "d".into(),
            },
        )
        .expect("sign");
        assert_eq!(card.profile_page_root, page_root);
        // encode -> decode preserves the field (same pattern as avatar round-trip)
        let bytes = canonical_cbor_encode(&card).expect("encode");
        let decoded: ProfileCardBroadcast =
            ciborium::de::from_reader(&bytes[..]).expect("decode struct");
        assert_eq!(decoded.profile_page_root, page_root);
        assert_eq!(verify_card(&decoded, 0).expect("verify"), owner.owner.0);
    }

    #[test]
    fn card_topic_for_is_owner_id_hex() {
        let owner_id = [0xABu8; 16];
        assert_eq!(
            card_topic_for(&owner_id),
            format!(
                "harmony/discovery/profile/owner/{}/card",
                hex::encode(owner_id)
            )
        );
    }

    #[test]
    fn verify_card_accepts_a_well_formed_card() {
        let owner = crate::community_membership::mint_test_owner(0x50);
        let card = sign_card(
            &owner.device_key,
            owner.owner.0,
            "Ann".into(),
            "hi".into(),
            None,
            None,
            owner.cert.clone(),
            Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "d".into(),
            },
        )
        .unwrap();
        assert_eq!(verify_card(&card, 0).unwrap(), owner.owner.0);
    }

    #[test]
    fn verify_card_rejects_owner_mismatch() {
        let x = crate::community_membership::mint_test_owner(0x51);
        let y = crate::community_membership::mint_test_owner(0x52);
        let mut card = sign_card(
            &y.device_key,
            y.owner.0,
            "n".into(),
            "".into(),
            None,
            None,
            y.cert.clone(),
            Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "d".into(),
            },
        )
        .unwrap();
        card.owner_id = x.owner.0; // owner_id now != cert.owner_id
        assert!(matches!(
            verify_card(&card, 0),
            Err(CardVerifyError::EnrollmentOwnerMismatch)
        ));
    }

    #[test]
    fn verify_card_rejects_tampered_signature() {
        let owner = crate::community_membership::mint_test_owner(0x53);
        let mut card = sign_card(
            &owner.device_key,
            owner.owner.0,
            "n".into(),
            "".into(),
            None,
            None,
            owner.cert.clone(),
            Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "d".into(),
            },
        )
        .unwrap();
        card.signature[0] ^= 0x01;
        assert!(matches!(
            verify_card(&card, 0),
            Err(CardVerifyError::SignatureInvalid)
        ));
    }

    #[test]
    fn verify_card_rejects_oversize_fields() {
        let owner = crate::community_membership::mint_test_owner(0x54);
        let mut card = sign_card(
            &owner.device_key,
            owner.owner.0,
            "n".into(),
            "".into(),
            None,
            None,
            owner.cert.clone(),
            Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "d".into(),
            },
        )
        .unwrap();
        card.display_name = "z".repeat(MAX_DISPLAY_NAME_BYTES + 1);
        assert!(matches!(
            verify_card(&card, 0),
            Err(CardVerifyError::DisplayNameTooLong)
        ));
    }

    #[test]
    fn verify_card_rejects_quorum_without_bundle() {
        use harmony_owner::{
            certs::{EnrollmentCert, EnrollmentIssuer},
            pubkey_bundle::{ClassicalKeys, PubKeyBundle},
        };
        // Build a structurally-valid Quorum cert that passes cert.verify()'s
        // structural branch. Without a signer-cert bundle on the card, the
        // quorum part signatures cannot be verified — must be rejected
        // (ZEB-677: the old blanket non-Master rejection narrowed to the
        // no-bundle case; see verify_card_accepts_quorum_with_bundle).
        let device_sk = ed25519_dalek::SigningKey::from_bytes(&[0xAAu8; 32]);
        let device_bundle = PubKeyBundle {
            classical: ClassicalKeys {
                ed25519_verify: device_sk.verifying_key().to_bytes(),
                x25519_pub: [0u8; 32],
            },
            post_quantum: None,
        };
        let device_id = device_bundle.identity_hash();
        let owner_id = [0xBBu8; 16];
        let quorum_cert = EnrollmentCert {
            version: 1,
            owner_id,
            device_id,
            device_pubkeys: device_bundle,
            issued_at: 1_700_000_000,
            expires_at: None,
            issuer: EnrollmentIssuer::Quorum {
                signers: vec![[1u8; 16], [2u8; 16]],
                signatures: vec![vec![0u8; 64], vec![0u8; 64]],
            },
            signature: vec![],
        };
        quorum_cert
            .verify(0)
            .expect("quorum cert passes structural verify");
        // sign_card refuses a quorum cert outright — an empty-bundle quorum
        // card would fail EVERY peer's verification (Qodo PR #458).
        assert!(matches!(
            sign_card(
                &device_sk,
                owner_id,
                "n".into(),
                "".into(),
                None,
                None,
                quorum_cert.clone(),
                Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "d".into(),
                },
            ),
            Err(CardError::QuorumCertRequiresBundle)
        ));
        // A no-bundle quorum card built EXPLICITLY (hostile peer / future
        // bug) must still fail verification closed.
        let card = sign_card_with_bundle(
            &device_sk,
            owner_id,
            "n".into(),
            "".into(),
            None,
            None,
            quorum_cert,
            Vec::new(),
            Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "d".into(),
            },
        )
        .unwrap();
        assert!(matches!(
            verify_card(&card, 0),
            Err(CardVerifyError::EnrollmentCertInvalid)
        ));
    }

    /// ZEB-677: a card whose enrollment is a genuine Quorum-issued cert
    /// verifies when it carries the Master-issued signer-cert bundle
    /// (signed into the card, so it is tamper-bound).
    #[test]
    fn verify_card_accepts_quorum_with_bundle() {
        use crate::enrollment_verify::quorum_fixtures::{mint_quorum_world, WORLD_NOW};
        let world = mint_quorum_world(0xA8);
        let card = sign_card_with_bundle(
            &world.c_sk,
            world.owner_id,
            "quorum device".into(),
            "".into(),
            None,
            None,
            world.c_quorum_cert.clone(),
            world.bundle.clone(),
            Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "d".into(),
            },
        )
        .expect("sign card with quorum cert + bundle");
        let owner = verify_card(&card, WORLD_NOW).expect("quorum card with bundle verifies");
        assert_eq!(owner, world.owner_id);

        // Stripping the bundle breaks BOTH the quorum verification and the
        // card signature (the bundle is inside the signed bytes).
        let mut stripped = card.clone();
        stripped.signer_certs = Vec::new();
        assert!(verify_card(&stripped, WORLD_NOW).is_err());

        // Serde: absent key decodes to empty (old encoders), populated
        // bundle round-trips.
        let bytes = canonical_cbor_encode(&card).expect("encode");
        let back: ProfileCardBroadcast =
            crate::owner_state_crypto::canonical_cbor_decode(&bytes).expect("decode");
        assert_eq!(back.signer_certs, world.bundle);
        assert_eq!(card, back);
    }

    #[test]
    fn verify_card_rejects_expired_cert() {
        use harmony_owner::pubkey_bundle::{ClassicalKeys, PubKeyBundle};

        // Build a card with an enrollment cert that expires at 2_000.
        let master_sk = ed25519_dalek::SigningKey::from_bytes(&[0x55u8; 32]);
        let master_bundle = PubKeyBundle {
            classical: ClassicalKeys {
                ed25519_verify: master_sk.verifying_key().to_bytes(),
                x25519_pub: [0u8; 32],
            },
            post_quantum: None,
        };
        let owner_id = master_bundle.identity_hash();
        let device_sk = ed25519_dalek::SigningKey::from_bytes(&[0x55u8 ^ 0xFFu8; 32]);
        let device_bundle = PubKeyBundle {
            classical: ClassicalKeys {
                ed25519_verify: device_sk.verifying_key().to_bytes(),
                x25519_pub: [0u8; 32],
            },
            post_quantum: None,
        };
        let device_id = device_bundle.identity_hash();
        // issued_at = 1_000, expires_at = Some(2_000)
        let cert = harmony_owner::certs::EnrollmentCert::sign_master(
            &master_sk,
            master_bundle,
            device_id,
            device_bundle,
            1_000,
            Some(2_000),
        )
        .expect("sign_master");

        let card = sign_card(
            &device_sk,
            owner_id,
            "Exp".into(),
            "".into(),
            None,
            None,
            cert,
            Hlc {
                wall_ms: 1_500,
                logical: 0,
                device_id: "d".into(),
            },
        )
        .expect("sign");

        // now > expiry → rejected
        assert!(matches!(
            verify_card(&card, 2_001),
            Err(CardVerifyError::EnrollmentCertInvalid)
        ));
        // before expiry → ok
        assert!(verify_card(&card, 1_500).is_ok());
    }

    #[test]
    fn verify_card_rejects_future_dated_shared_at() {
        // C4: a card whose shared_at.wall_ms is beyond now + MAX_FORWARD_SKEW_MS
        // must never verify — otherwise it out-HLCs every honest card forever.
        let owner = crate::community_membership::mint_test_owner(0x71);
        const NOW_S: u64 = 1_700_000_000;
        let now_ms = NOW_S * 1000;
        let one_year_ms = 365 * 86_400 * 1000;
        let poison = sign_card(
            &owner.device_key,
            owner.owner.0,
            "Mallory".into(),
            "".into(),
            None,
            None,
            owner.cert.clone(),
            Hlc {
                wall_ms: now_ms + one_year_ms,
                logical: 0,
                device_id: "d".into(),
            },
        )
        .expect("sign");
        assert!(matches!(
            verify_card(&poison, NOW_S),
            Err(CardVerifyError::SharedAtTooFarInFuture)
        ));
    }

    #[test]
    fn verify_card_accepts_in_range_shared_at_at_the_inclusive_ceiling() {
        let owner = crate::community_membership::mint_test_owner(0x72);
        const NOW_S: u64 = 1_700_000_000;
        let now_ms = NOW_S * 1000;
        // Present, and exactly at the inclusive ceiling, both verify.
        for wall_ms in [now_ms, now_ms + crate::clock_trust::MAX_FORWARD_SKEW_MS] {
            let card = sign_card(
                &owner.device_key,
                owner.owner.0,
                "Ann".into(),
                "".into(),
                None,
                None,
                owner.cert.clone(),
                Hlc {
                    wall_ms,
                    logical: 0,
                    device_id: "d".into(),
                },
            )
            .expect("sign");
            assert_eq!(
                verify_card(&card, NOW_S).expect("in-range verifies"),
                owner.owner.0
            );
        }
        // Past the ceiling is rejected. The seconds→ms conversion is compensated
        // by +999 (the shared helper's floored-conversion fail-open margin), so
        // the first rejected sample is one ms past the compensated ceiling; the
        // exact ±1 ms boundary is pinned in `clock_trust`'s own test.
        let over = sign_card(
            &owner.device_key,
            owner.owner.0,
            "Ann".into(),
            "".into(),
            None,
            None,
            owner.cert.clone(),
            Hlc {
                wall_ms: now_ms + crate::clock_trust::MAX_FORWARD_SKEW_MS + 1000,
                logical: 0,
                device_id: "d".into(),
            },
        )
        .expect("sign");
        assert!(matches!(
            verify_card(&over, NOW_S),
            Err(CardVerifyError::SharedAtTooFarInFuture)
        ));
    }

    #[test]
    fn verify_card_zero_now_is_apply_all_for_shared_at() {
        // now_secs == 0 is the unreadable-local-clock sentinel (wall_now_secs()
        // .unwrap_or(0)); a bad LOCAL clock must never reject an honest card, so the
        // bound disables itself. (This is also why every legacy verify_card(&card, 0)
        // test keeps passing.)
        let owner = crate::community_membership::mint_test_owner(0x73);
        let far_future = sign_card(
            &owner.device_key,
            owner.owner.0,
            "Ann".into(),
            "".into(),
            None,
            None,
            owner.cert.clone(),
            Hlc {
                wall_ms: u64::MAX / 2,
                logical: 0,
                device_id: "d".into(),
            },
        )
        .expect("sign");
        assert_eq!(
            verify_card(&far_future, 0).expect("apply-all"),
            owner.owner.0
        );
    }

    /// ZEB-921: the owner-state observable decodes the display name from the
    /// exact bytes the publisher caches (and the ZEB-884 queryable serves).
    #[test]
    fn decode_card_display_name_roundtrips_signed_bytes() {
        let owner = crate::community_membership::mint_test_owner(0x74);
        let card = sign_card(
            &owner.device_key,
            owner.owner.0,
            "Zeb921Probe".into(),
            "".into(),
            None,
            None,
            owner.cert.clone(),
            Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "d".into(),
            },
        )
        .unwrap();
        let bytes = canonical_cbor_encode(&card).unwrap();
        assert_eq!(
            decode_card_display_name(&bytes).as_deref(),
            Some("Zeb921Probe")
        );
    }

    #[test]
    fn decode_card_display_name_garbage_is_none() {
        assert_eq!(decode_card_display_name(b"not cbor at all"), None);
        assert_eq!(decode_card_display_name(&[]), None);
    }
}
