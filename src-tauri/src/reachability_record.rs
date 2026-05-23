//! ZEB-321 Phase 1: ReachabilityAnnounce CRDT event payload.
//!
//! See `docs/specs/2026-05-22-zeb-321-cross-wan-connectivity-design.md` §5.

use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

use crate::owner_state_crypto::{
    canonical_cbor_encode, sealed::CanonicalPayloadSealed, CanonicalPayload, CryptoError,
};
use crate::owner_state_types::{deserialize_bytes_from_bstr, serialize_bytes_as_bstr};

/// Payload of a `MembershipEventKind::ReachabilityAnnounce` variant.
/// All 5 field keys are 2 chars to satisfy the same-length-keys invariant
/// at this nesting level. Encoded inside the membership envelope's `vl`
/// (variant value) slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReachabilityAnnouncePayload {
    /// Iroh NodeId (Ed25519 public key, 32 bytes). Distinct from
    /// harmony identity key — bound to it via `identity_signature`.
    #[serde(
        rename = "nd",
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub iroh_node_id: [u8; 32],

    /// Home DERP relay URL (Phase 1: an n0-hosted relay).
    #[serde(rename = "rl")]
    pub home_relay_url: String,

    /// Direct-traversal hint addresses (publicly routable if any; may
    /// be empty Vec).
    #[serde(rename = "da")]
    pub direct_addresses: Vec<SocketAddr>,

    /// Wall-clock milliseconds when this record was authored.
    #[serde(rename = "ts")]
    pub announced_at_ms: u64,

    /// Inner Ed25519 signature by the device's HARMONY identity key
    /// over canonical CBOR of (nd, rl, da, ts, actor, hlc). Binds the
    /// Iroh NodeId to the harmony identity. 64 bytes.
    #[serde(
        rename = "sg",
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub identity_signature: [u8; 64],
}

impl CanonicalPayloadSealed for ReachabilityAnnouncePayload {}
impl CanonicalPayload for ReachabilityAnnouncePayload {}

/// Convenience: canonical-encode for hashing / signing.
pub fn canonical_payload_bytes(p: &ReachabilityAnnouncePayload) -> Result<Vec<u8>, CryptoError> {
    canonical_cbor_encode(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_payload() -> ReachabilityAnnouncePayload {
        ReachabilityAnnouncePayload {
            iroh_node_id: [0xAB; 32],
            home_relay_url: "https://derp.example/".into(),
            direct_addresses: vec![],
            announced_at_ms: 1_700_000_000_000,
            identity_signature: [0xCD; 64],
        }
    }

    #[test]
    fn roundtrip_cbor() {
        let p = fixture_payload();
        let bytes = canonical_payload_bytes(&p).expect("encode");
        let decoded: ReachabilityAnnouncePayload =
            ciborium::de::from_reader(&bytes[..]).expect("decode");
        assert_eq!(decoded, p);
    }

    #[test]
    fn payload_keys_are_2_chars() {
        // Same-length-keys CBOR invariant — see EventPayload doc in community_membership.rs.
        let p = fixture_payload();
        let bytes = canonical_payload_bytes(&p).expect("encode");
        let val: ciborium::Value = ciborium::de::from_reader(&bytes[..]).expect("decode");
        let map = val.as_map().expect("payload is map");
        for (k, _) in map {
            let s = k.as_text().expect("key is text");
            assert_eq!(
                s.chars().count(),
                2,
                "ReachabilityAnnouncePayload key {s:?} violates 2-char invariant"
            );
        }
    }
}
