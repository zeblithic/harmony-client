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
// Called from `ProfileBroadcastPublisher::maybe_publish` in Task 2; until
// then only `#[cfg(test)]` callers exist, so the non-test build sees this
// as unused.
#[allow(dead_code)]
pub(crate) fn sign_broadcast(
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
}
