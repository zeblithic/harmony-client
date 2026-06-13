//! ZEB-458 P4 Phase B: CommunityRelayAnnounce CRDT event payload.
//!
//! A community member opts in to volunteer as a sealed relay; opting in
//! publishes this signed advertisement into the community-state membership
//! CRDT, mirroring `ReachabilityAnnounce` (see `reachability_record.rs`).
//! Consumers read the fresh, capped advertiser set via `CommunityRelayResolver`.

use ed25519_dalek::Signer;
use serde::{Deserialize, Serialize};

use crate::owner_state_crypto::{
    canonical_cbor_encode, sealed::CanonicalPayloadSealed, CanonicalPayload, CryptoError,
};
use crate::owner_state_types::{
    deserialize_bytes_from_bstr, serialize_bytes_as_bstr, Hlc, OwnerAddr,
};
use crate::reachability_record::InnerSigError;

/// Advertisement refresh cadence (~7.5 min) — reuse the butler-set cadence.
pub const COMMUNITY_RELAY_AD_REFRESH_MS: u64 = crate::butler_deposit::BUTLER_SET_REFRESH_MS;
/// Freshness window (~15 min) — reuse the butler-set freshness window.
pub const COMMUNITY_RELAY_AD_FRESHNESS_MS: u64 = crate::butler_deposit::BUTLER_SET_FRESHNESS_MS;
/// Max advertisers a consumer reads per community (bounds fan-out, D37).
pub const COMMUNITY_RELAY_ADVERTISERS_MAX: usize = 4;

/// The relay device's dialing coordinates (one advertised volunteer device).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommunityRelayEntry {
    #[serde(
        rename = "rd",
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub relay_device_id: [u8; 16],
    #[serde(
        rename = "ep",
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub iroh_endpoint_id: [u8; 32],
    #[serde(
        rename = "vk",
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub relay_device_ed25519_verify: [u8; 32],
    #[serde(rename = "hr")]
    pub home_relay: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommunityRelayAnnouncePayload {
    #[serde(rename = "rl")]
    pub relay: CommunityRelayEntry,
    #[serde(rename = "aa")]
    pub ad_at: u64,
    #[serde(
        rename = "sg",
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub identity_signature: [u8; 64],
}

impl CanonicalPayloadSealed for CommunityRelayAnnouncePayload {}
impl CanonicalPayload for CommunityRelayAnnouncePayload {}

pub fn canonical_payload_bytes(p: &CommunityRelayAnnouncePayload) -> Result<Vec<u8>, CryptoError> {
    canonical_cbor_encode(p)
}

pub fn inner_signed_bytes(
    relay: &CommunityRelayEntry,
    ad_at: u64,
    actor: &OwnerAddr,
    hlc: &Hlc,
) -> Result<Vec<u8>, CryptoError> {
    #[derive(Serialize)]
    struct InnerSigInput<'a> {
        #[serde(rename = "rd", serialize_with = "serialize_bytes_as_bstr")]
        rd: &'a [u8; 16],
        #[serde(rename = "ep", serialize_with = "serialize_bytes_as_bstr")]
        ep: &'a [u8; 32],
        #[serde(rename = "vk", serialize_with = "serialize_bytes_as_bstr")]
        vk: &'a [u8; 32],
        #[serde(rename = "hr")]
        hr: &'a str,
        #[serde(rename = "aa")]
        aa: u64,
        #[serde(rename = "ac")]
        ac: &'a OwnerAddr,
        #[serde(rename = "hl")]
        hl: &'a Hlc,
    }
    let input = InnerSigInput {
        rd: &relay.relay_device_id,
        ep: &relay.iroh_endpoint_id,
        vk: &relay.relay_device_ed25519_verify,
        hr: &relay.home_relay,
        aa: ad_at,
        ac: actor,
        hl: hlc,
    };
    let mut buf = Vec::new();
    ciborium::into_writer(&input, &mut buf).map_err(|e| CryptoError::CborEncode(format!("{e}")))?;
    Ok(buf)
}

pub fn build_signed_community_relay_announce(
    relay: CommunityRelayEntry,
    ad_at: u64,
    actor: &OwnerAddr,
    hlc: &Hlc,
    signing_key: &ed25519_dalek::SigningKey,
) -> Result<CommunityRelayAnnouncePayload, CryptoError> {
    let inner = inner_signed_bytes(&relay, ad_at, actor, hlc)?;
    let sig = signing_key.sign(&inner).to_bytes();
    Ok(CommunityRelayAnnouncePayload {
        relay,
        ad_at,
        identity_signature: sig,
    })
}

pub fn verify_inner_signature(
    p: &CommunityRelayAnnouncePayload,
    actor: &OwnerAddr,
    hlc: &Hlc,
    actor_ed25519_pub: &ed25519_dalek::VerifyingKey,
) -> Result<(), InnerSigError> {
    let bytes =
        inner_signed_bytes(&p.relay, p.ad_at, actor, hlc).map_err(|_| InnerSigError::Encode)?;
    let sig = ed25519_dalek::Signature::from_bytes(&p.identity_signature);
    actor_ed25519_pub
        .verify_strict(&bytes, &sig)
        .map_err(|_| InnerSigError::Invalid)
}

pub fn fresh_relay_entry(
    p: &CommunityRelayAnnouncePayload,
    now_ms: u64,
) -> Option<CommunityRelayEntry> {
    if p.ad_at == 0
        || p.ad_at > now_ms.saturating_add(COMMUNITY_RELAY_AD_FRESHNESS_MS)
        || now_ms.saturating_sub(p.ad_at) > COMMUNITY_RELAY_AD_FRESHNESS_MS
    {
        return None;
    }
    Some(p.relay.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state_types::{Hlc, OwnerAddr};

    fn fixture_hlc() -> Hlc {
        Hlc {
            wall_ms: 1_700_000_000_000,
            logical: 0,
            device_id: "fix".into(),
        }
    }
    fn fixture_entry() -> CommunityRelayEntry {
        CommunityRelayEntry {
            relay_device_id: [0x11; 16],
            iroh_endpoint_id: [0x22; 32],
            relay_device_ed25519_verify: [0x33; 32],
            home_relay: "https://derp.example/".into(),
        }
    }

    #[test]
    fn payload_round_trips_canonical_cbor() {
        let p = CommunityRelayAnnouncePayload {
            relay: fixture_entry(),
            ad_at: 1_700_000_000_000,
            identity_signature: [0xCD; 64],
        };
        let bytes = canonical_payload_bytes(&p).expect("encode");
        let back: CommunityRelayAnnouncePayload =
            ciborium::de::from_reader(&bytes[..]).expect("decode");
        assert_eq!(back, p);
    }

    #[test]
    fn payload_top_level_keys_are_2_chars() {
        let p = CommunityRelayAnnouncePayload {
            relay: fixture_entry(),
            ad_at: 1,
            identity_signature: [0; 64],
        };
        let bytes = canonical_payload_bytes(&p).expect("encode");
        let val: ciborium::Value = ciborium::de::from_reader(&bytes[..]).expect("decode");
        for (k, _) in val.as_map().expect("map") {
            assert_eq!(k.as_text().expect("text").chars().count(), 2);
        }
    }

    #[test]
    fn inner_sig_round_trips_and_rejects_actor_or_hlc_mutation() {
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&[0x42; 32]);
        let vk = signing_key.verifying_key();
        let actor = OwnerAddr([0xAA; 16]);
        let hlc = fixture_hlc();
        let p = build_signed_community_relay_announce(
            fixture_entry(),
            1_700_000_000_000,
            &actor,
            &hlc,
            &signing_key,
        )
        .expect("build");
        verify_inner_signature(&p, &actor, &hlc, &vk).expect("verify");
        assert!(verify_inner_signature(&p, &OwnerAddr([0xBB; 16]), &hlc, &vk).is_err());
        let wrong_hlc = Hlc {
            wall_ms: hlc.wall_ms + 1,
            ..hlc.clone()
        };
        assert!(verify_inner_signature(&p, &actor, &wrong_hlc, &vk).is_err());
    }

    #[test]
    fn inner_sig_rejects_tampered_relay_entry() {
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&[0x42; 32]);
        let vk = signing_key.verifying_key();
        let actor = OwnerAddr([0xAA; 16]);
        let hlc = fixture_hlc();
        let mut p = build_signed_community_relay_announce(
            fixture_entry(),
            1_700_000_000_000,
            &actor,
            &hlc,
            &signing_key,
        )
        .expect("build");
        p.relay.iroh_endpoint_id[0] ^= 0xFF;
        assert!(verify_inner_signature(&p, &actor, &hlc, &vk).is_err());
    }

    #[test]
    fn fresh_relay_entry_filters_stale_zero_and_future() {
        let p = CommunityRelayAnnouncePayload {
            relay: fixture_entry(),
            ad_at: 1_700_000_000_000,
            identity_signature: [0; 64],
        };
        let w = COMMUNITY_RELAY_AD_FRESHNESS_MS;
        let t = p.ad_at;
        assert!(fresh_relay_entry(&p, t).is_some());
        assert!(fresh_relay_entry(&p, t + w).is_some());
        assert!(fresh_relay_entry(&p, t - w).is_some());
        assert!(fresh_relay_entry(&p, t + w + 1).is_none());
        assert!(fresh_relay_entry(&p, t - w - 1).is_none());
        let mut zero = p.clone();
        zero.ad_at = 0;
        assert!(fresh_relay_entry(&zero, 1).is_none());
    }

    #[test]
    fn payload_wire_bytes_pinned() {
        let p = CommunityRelayAnnouncePayload {
            relay: CommunityRelayEntry {
                relay_device_id: [0x11; 16],
                iroh_endpoint_id: [0x22; 32],
                relay_device_ed25519_verify: [0x33; 32],
                home_relay: "https://derp.example/".into(),
            },
            ad_at: 1_700_000_000_000,
            identity_signature: [0xCD; 64],
        };
        let hex = hex::encode(canonical_payload_bytes(&p).expect("encode"));
        eprintln!("community_relay_announce payload hex: {hex}");
        assert_eq!(hex, "a362726ca462726450111111111111111111111111111111116265705820222222222222222222222222222222222222222222222222222222222222222262766b582033333333333333333333333333333333333333333333333333333333333333336268727568747470733a2f2f646572702e6578616d706c652f6261611b0000018bcfe568006273675840cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd",
            "CommunityRelayAnnouncePayload wire format changed — re-pin deliberately");
    }
}
