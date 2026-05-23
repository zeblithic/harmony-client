//! ZEB-321 Phase 1: ReachabilityAnnounce CRDT event payload.
//!
//! See `docs/specs/2026-05-22-zeb-321-cross-wan-connectivity-design.md` §5.

use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

use crate::owner_state_crypto::{
    canonical_cbor_encode, sealed::CanonicalPayloadSealed, CanonicalPayload, CryptoError,
};
use crate::owner_state_types::{
    deserialize_bytes_from_bstr, serialize_bytes_as_bstr, Hlc, OwnerAddr,
};

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

/// Canonical byte string the inner identity signature covers:
/// CBOR(canonical) of (nd, rl, da, ts, ac, hl). The actor + hlc are pulled
/// from the surrounding membership envelope; they're NOT part of the
/// payload struct itself but are bound into the signature so a replay
/// attacker can't lift a `ReachabilityAnnouncePayload` from one envelope
/// and re-attach it under a different actor or HLC.
///
/// All 6 field keys are 2 chars to satisfy the same-length-keys
/// invariant at this nesting level. This sig-input map MUST be kept
/// distinct from `ReachabilityAnnouncePayload`'s wire shape (which has
/// only the 5 self-contained fields nd/rl/da/ts/sg); confusing them
/// would let a peer replay the inner-sig bytes verbatim.
///
/// Encodes via raw `ciborium::into_writer` (not `canonical_cbor_encode`)
/// because the input struct holds references — and `CanonicalPayload`'s
/// sealed-trait API can't be impl'd for borrowed `&T`. The encoding
/// shape is still deterministic given all field serde impls are.
pub fn inner_signed_bytes(
    iroh_node_id: &[u8; 32],
    home_relay_url: &str,
    direct_addresses: &[SocketAddr],
    announced_at_ms: u64,
    actor: &OwnerAddr,
    hlc: &Hlc,
) -> Result<Vec<u8>, CryptoError> {
    #[derive(Serialize)]
    struct InnerSigInput<'a> {
        #[serde(rename = "nd", serialize_with = "serialize_bytes_as_bstr")]
        nd: &'a [u8; 32],
        #[serde(rename = "rl")]
        rl: &'a str,
        #[serde(rename = "da")]
        da: &'a [SocketAddr],
        #[serde(rename = "ts")]
        ts: u64,
        #[serde(rename = "ac")]
        ac: &'a OwnerAddr,
        #[serde(rename = "hl")]
        hl: &'a Hlc,
    }
    let input = InnerSigInput {
        nd: iroh_node_id,
        rl: home_relay_url,
        da: direct_addresses,
        ts: announced_at_ms,
        ac: actor,
        hl: hlc,
    };
    let mut buf = Vec::new();
    ciborium::into_writer(&input, &mut buf).map_err(|e| CryptoError::CborEncode(format!("{e}")))?;
    Ok(buf)
}

/// Sign a fresh `ReachabilityAnnouncePayload` using the device's harmony
/// identity signing key. Caller is responsible for ensuring `actor`
/// matches the identity (`identity.identity.address_hash`).
pub fn build_signed_payload(
    iroh_node_id: [u8; 32],
    home_relay_url: String,
    direct_addresses: Vec<SocketAddr>,
    announced_at_ms: u64,
    actor: &OwnerAddr,
    hlc: &Hlc,
    identity: &harmony_identity::PrivateIdentity,
) -> Result<ReachabilityAnnouncePayload, CryptoError> {
    let inner = inner_signed_bytes(
        &iroh_node_id,
        &home_relay_url,
        &direct_addresses,
        announced_at_ms,
        actor,
        hlc,
    )?;
    let sig = identity.sign(&inner);
    Ok(ReachabilityAnnouncePayload {
        iroh_node_id,
        home_relay_url,
        direct_addresses,
        announced_at_ms,
        identity_signature: sig,
    })
}

/// Verify the inner identity signature against the given Ed25519
/// verifying key — the 32-byte Ed25519 half of the 64-byte
/// `harmony_identity::Identity::to_public_bytes()`.
pub fn verify_inner_signature(
    p: &ReachabilityAnnouncePayload,
    actor: &OwnerAddr,
    hlc: &Hlc,
    actor_ed25519_pub: &ed25519_dalek::VerifyingKey,
) -> Result<(), InnerSigError> {
    let bytes = inner_signed_bytes(
        &p.iroh_node_id,
        &p.home_relay_url,
        &p.direct_addresses,
        p.announced_at_ms,
        actor,
        hlc,
    )
    .map_err(|_| InnerSigError::Encode)?;
    let sig = ed25519_dalek::Signature::from_bytes(&p.identity_signature);
    actor_ed25519_pub
        .verify_strict(&bytes, &sig)
        .map_err(|_| InnerSigError::Invalid)
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum InnerSigError {
    #[error("inner reachability signature failed to encode")]
    Encode,
    #[error("inner reachability signature invalid")]
    Invalid,
}

#[cfg(test)]
mod tests {
    use super::*;
    use harmony_identity::PrivateIdentity;

    fn fixture_hlc() -> Hlc {
        Hlc {
            wall_ms: 1_700_000_000_000,
            logical: 0,
            device_id: "fix".into(),
        }
    }

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

    #[test]
    fn inner_sig_roundtrip_with_real_identity() {
        let identity = PrivateIdentity::generate(&mut rand::thread_rng());
        let actor = OwnerAddr(identity.identity.address_hash);
        let hlc = fixture_hlc();
        let p = build_signed_payload(
            [0xAB; 32],
            "https://derp.example/".into(),
            vec![],
            1_700_000_000_000,
            &actor,
            &hlc,
            &identity,
        )
        .expect("sign");

        // PrivateIdentity::to_public_bytes() returns 64 bytes:
        // X25519_pub (32) || Ed25519_pub (32).
        let pub_bytes = identity.identity.to_public_bytes();
        let ed_pub: [u8; 32] = pub_bytes[32..].try_into().unwrap();
        let verifying = ed25519_dalek::VerifyingKey::from_bytes(&ed_pub).unwrap();

        verify_inner_signature(&p, &actor, &hlc, &verifying).expect("verify");
    }

    #[test]
    fn inner_sig_rejects_tampered_node_id() {
        let identity = PrivateIdentity::generate(&mut rand::thread_rng());
        let actor = OwnerAddr(identity.identity.address_hash);
        let hlc = fixture_hlc();
        let mut p = build_signed_payload(
            [0xAB; 32],
            "https://derp.example/".into(),
            vec![],
            1_700_000_000_000,
            &actor,
            &hlc,
            &identity,
        )
        .expect("sign");
        p.iroh_node_id[0] ^= 0xFF;

        let pub_bytes = identity.identity.to_public_bytes();
        let ed_pub: [u8; 32] = pub_bytes[32..].try_into().unwrap();
        let verifying = ed25519_dalek::VerifyingKey::from_bytes(&ed_pub).unwrap();

        assert_eq!(
            verify_inner_signature(&p, &actor, &hlc, &verifying),
            Err(InnerSigError::Invalid)
        );
    }
}
