//! ZEB-341 — owner_id-keyed, EnrollmentCert-verified profile card broadcast.
//! Sibling to `profile_broadcast.rs`. Carries a peer's display name + status,
//! bound to their harmony-owner `owner_id` via the ZEB-339 cert model.
//! Spec: docs/specs/2026-05-30-zeb-341-profile-cards-design.md

use crate::owner_state_crypto::{
    canonical_cbor_encode, sealed::CanonicalPayloadSealed, CanonicalPayload, CryptoError,
};
use crate::owner_state_types::Hlc;
use ed25519_dalek::{Signer, SigningKey};
use harmony_owner::certs::{EnrollmentCert, EnrollmentIssuer};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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
    #[error("canonical CBOR encode failed: {0}")]
    Encode(#[from] CryptoError),
}

/// Build + Ed25519-sign a card over canonical CBOR with `signature` zeroed.
/// `signer` MUST be the enrolled device key (pub ==
/// `enrollment.device_pubkeys.classical.ed25519_verify`).
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
pub fn verify_card(card: &ProfileCardBroadcast, now_ms: u64) -> Result<[u8; 16], CardVerifyError> {
    if card.display_name.len() > MAX_DISPLAY_NAME_BYTES {
        return Err(CardVerifyError::DisplayNameTooLong);
    }
    if card.status_text.len() > MAX_STATUS_TEXT_BYTES {
        return Err(CardVerifyError::StatusTextTooLong);
    }
    card.enrollment
        .verify(now_ms)
        .map_err(|_| CardVerifyError::EnrollmentCertInvalid)?;
    // Reject non-Master issuers: the profile card path cannot fully verify
    // Quorum signatures, and cert.verify() only structurally-checks them
    // (mirrors enrolled_key_from_cert in community_membership.rs).
    if !matches!(card.enrollment.issuer, EnrollmentIssuer::Master { .. }) {
        return Err(CardVerifyError::EnrollmentCertInvalid);
    }
    if card.enrollment.owner_id != card.owner_id {
        return Err(CardVerifyError::EnrollmentOwnerMismatch);
    }
    let device_ed25519 = card.enrollment.device_pubkeys.classical.ed25519_verify;
    let vk = ed25519_dalek::VerifyingKey::from_bytes(&device_ed25519)
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
}

impl ProfileCardCache {
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
        let mut g = self.slots.lock().await;
        if let Some(slot) = g.get_mut(&sub) {
            if card.owner_id != slot.expected_owner {
                return;
            }
            let is_newer = match &slot.latest {
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
    }

    /// Snapshot the latest verified card for a subscription as the frontend DTO.
    pub async fn get_cached(&self, sub: SubscriptionId) -> Option<DiscoveredCardInfo> {
        let g = self.slots.lock().await;
        let slot = g.get(&sub)?;
        let c = slot.latest.as_ref()?;
        Some(DiscoveredCardInfo {
            owner_id_hex: hex::encode(c.owner_id),
            display_name: c.display_name.clone(),
            status_text: c.status_text.clone(),
            avatar_cid: c.avatar_cid.map(hex::encode),
            profile_page_root: c.profile_page_root.map(hex::encode),
        })
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
        let latest: std::sync::Arc<Mutex<Option<CardWire>>> = std::sync::Arc::new(Mutex::new(None));
        let task = {
            let latest_for_task = std::sync::Arc::clone(&latest);
            let sink_for_task = std::sync::Arc::clone(&sink);
            tokio::spawn(async move {
                let mut iv = tokio::time::interval(refresh);
                iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                iv.tick().await; // consume immediate first tick
                loop {
                    iv.tick().await;
                    let snapshot = latest_for_task.lock().await.clone();
                    if let Some((topic, bytes)) = snapshot {
                        if let Err(e) = sink_for_task.publish(topic, bytes).await {
                            tracing::warn!(error = %e, "profile card refresh publish failed");
                        }
                    }
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

    /// Abort the refresh task. Idempotent. (Mirrors ProfileBroadcastPublisher::shutdown.)
    pub async fn shutdown(&self) {
        if let Some(h) = self.task.lock().await.take() {
            h.abort();
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
    fn verify_card_rejects_non_master_issuer() {
        use harmony_owner::{
            certs::{EnrollmentCert, EnrollmentIssuer},
            pubkey_bundle::{ClassicalKeys, PubKeyBundle},
        };
        // Build a structurally-valid Quorum cert that passes cert.verify() but
        // should be rejected by verify_card (only Master certs accepted).
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
        // Build a card with the Quorum cert (sign with device key; owner_id matches cert)
        let card = sign_card(
            &device_sk,
            owner_id,
            "n".into(),
            "".into(),
            None,
            None,
            quorum_cert,
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
}
