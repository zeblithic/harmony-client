//! ZEB-537 community presence: ephemeral signed+sealed liveness beacons,
//! generalizing the per-call `voice_presence` (ZEB-350) pattern to a
//! per-community scope. Beacons ride a dedicated Zenoh topic (never the CRDT);
//! the seal under the per-community presence key (`derive_presence_key`) gates
//! non-members, and the device-#2 signature + materialized-membership check
//! prevents intra-member spoofing.
//!
//! Presence here is community-scoped (not per-channel): the seal binds only the
//! community, via a sentinel all-zero `ChannelId` passed to the audited
//! `encrypt_voice_packet` / `decrypt_voice_packet` AEAD seam, under the distinct
//! `COMMUNITY_PRESENCE_AAD` domain.

use crate::community_channel_log::ChannelKey;
use crate::community_membership::ChannelId;
use crate::owner_state_crypto::canonical_cbor_encode;
use crate::owner_state_types::{Hlc, SpaceId};
use crate::voice_crypto::{decrypt_voice_packet, encrypt_voice_packet, COMMUNITY_PRESENCE_AAD};
use serde::{Deserialize, Serialize};

/// Presence has no channel, so the AEAD seam (which is `(community, channel)`
/// scoped for voice) is bound with this all-zero sentinel channel. The
/// `COMMUNITY_PRESENCE_AAD` domain already separates community presence from
/// every voice/channel-log artifact, so the sentinel only needs to be stable.
const PRESENCE_SENTINEL_CHANNEL: ChannelId = ChannelId([0u8; 16]);

/// The unsigned community-presence claim. Canonical CBOR, 2-char keys
/// (same-length invariant for deterministic encoding).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresenceBeacon {
    #[serde(
        rename = "ow",
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub owner: [u8; 16],
    #[serde(
        rename = "dv",
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub device: [u8; 32],
    #[serde(rename = "sh")]
    pub started_hlc: Hlc,
    #[serde(rename = "sq")]
    pub seq: u64,
}

/// Beacon + detached device-#2 signature over `canonical_cbor_encode(beacon)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedPresenceBeacon {
    #[serde(rename = "bc")]
    pub beacon: PresenceBeacon,
    #[serde(
        rename = "sg",
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub sig: [u8; 64],
}

// Register both beacon types as `CanonicalPayload` so `canonical_cbor_encode`
// (the sealed-trait encoder) can sign/seal them. Mirrors `voice_presence`.
impl crate::owner_state_crypto::sealed::CanonicalPayloadSealed for PresenceBeacon {}
impl crate::owner_state_crypto::CanonicalPayload for PresenceBeacon {}
impl crate::owner_state_crypto::sealed::CanonicalPayloadSealed for SignedPresenceBeacon {}
impl crate::owner_state_crypto::CanonicalPayload for SignedPresenceBeacon {}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BeaconError {
    #[error("beacon CBOR encode failed")]
    Encode,
    #[error("beacon signature invalid")]
    BadSig,
    /// `session.put` failed — a transport/runtime fault, distinct from an
    /// encode/seal fault, so callers can diagnose (and one day retry) network
    /// failures without conflating them with CBOR bugs.
    #[error("beacon transport publish failed")]
    Publish,
}

/// Sign a beacon with the device-#2 ed25519 key. The signature covers the
/// canonical CBOR of the unsigned beacon (sig field excluded by construction).
pub fn sign_presence_beacon(
    beacon: PresenceBeacon,
    signing_key: &ed25519_dalek::SigningKey,
) -> Result<SignedPresenceBeacon, BeaconError> {
    use ed25519_dalek::Signer;
    let bytes = canonical_cbor_encode(&beacon).map_err(|_| BeaconError::Encode)?;
    let sig = signing_key.sign(&bytes).to_bytes();
    Ok(SignedPresenceBeacon { beacon, sig })
}

/// Verify the detached signature against the verifying key embedded in
/// `beacon.device`. This proves the holder of `device`'s private key signed it;
/// the membership check additionally requires `device ∈ owner.enrolled_device_keys`.
pub fn verify_presence_beacon_sig(signed: &SignedPresenceBeacon) -> Result<(), BeaconError> {
    let bytes = canonical_cbor_encode(&signed.beacon).map_err(|_| BeaconError::Encode)?;
    let vk = ed25519_dalek::VerifyingKey::from_bytes(&signed.beacon.device)
        .map_err(|_| BeaconError::BadSig)?;
    let sig = ed25519_dalek::Signature::from_bytes(&signed.sig);
    vk.verify_strict(&bytes, &sig)
        .map_err(|_| BeaconError::BadSig)
}

/// Seal a signed beacon under the per-community presence key for transport.
/// Output framing matches the voice media packet (`[12B nonce][ct+tag]`) but
/// binds the sentinel channel + `COMMUNITY_PRESENCE_AAD` so it can only ever be
/// opened as a community-presence beacon for `community`.
pub fn seal_presence_beacon(
    key: &ChannelKey,
    community: &SpaceId,
    signed: &SignedPresenceBeacon,
) -> Result<Vec<u8>, BeaconError> {
    let plain = canonical_cbor_encode(signed).map_err(|_| BeaconError::Encode)?;
    encrypt_voice_packet(
        key,
        community,
        &PRESENCE_SENTINEL_CHANNEL,
        COMMUNITY_PRESENCE_AAD,
        &plain,
    )
    .map_err(|_| BeaconError::Encode)
}

/// Open + decode a sealed beacon. Returns `None` on any failure (drop).
pub fn open_presence_beacon(
    key: &ChannelKey,
    community: &SpaceId,
    packet: &[u8],
) -> Option<SignedPresenceBeacon> {
    let plain = decrypt_voice_packet(
        key,
        community,
        &PRESENCE_SENTINEL_CHANNEL,
        COMMUNITY_PRESENCE_AAD,
        packet,
    )
    .ok()?;
    ciborium::from_reader(plain.as_slice()).ok()
}

/// Deterministic-nonce seal for wire-format fixtures. NEVER call from
/// production — a fixed nonce with a reused key is catastrophic nonce reuse.
#[cfg(any(test, feature = "test-fixtures"))]
#[doc(hidden)]
pub fn seal_presence_beacon_with_nonce(
    key: &ChannelKey,
    community: &SpaceId,
    signed: &SignedPresenceBeacon,
    nonce: [u8; 12],
) -> Result<Vec<u8>, BeaconError> {
    let plain = canonical_cbor_encode(signed).map_err(|_| BeaconError::Encode)?;
    crate::voice_crypto::encrypt_voice_packet_with_nonce(
        key,
        community,
        &PRESENCE_SENTINEL_CHANNEL,
        COMMUNITY_PRESENCE_AAD,
        &plain,
        nonce,
    )
    .map_err(|_| BeaconError::Encode)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_channel_log::derive_presence_key;
    use crate::owner_state_types::EpochKey;
    use ed25519_dalek::SigningKey;

    fn beacon(seq: u64) -> PresenceBeacon {
        PresenceBeacon {
            owner: [0xa1; 16],
            device: [0u8; 32], // overwritten by the signer in real use
            started_hlc: Hlc {
                wall_ms: 1000,
                logical: 0,
                device_id: "aa".repeat(32),
            },
            seq,
        }
    }

    #[test]
    fn sign_then_verify_ok() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let mut b = beacon(1);
        b.device = sk.verifying_key().to_bytes();
        let signed = sign_presence_beacon(b.clone(), &sk).unwrap();
        assert_eq!(signed.beacon, b);
        verify_presence_beacon_sig(&signed).expect("valid sig");
    }

    #[test]
    fn tampered_sig_rejected() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let mut b = beacon(1);
        b.device = sk.verifying_key().to_bytes();
        let mut signed = sign_presence_beacon(b, &sk).unwrap();
        signed.beacon.seq = 99; // tamper after signing
        assert_eq!(
            verify_presence_beacon_sig(&signed),
            Err(BeaconError::BadSig)
        );
    }

    #[test]
    fn seal_open_roundtrip_and_wrong_key_drops() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let mut b = beacon(3);
        b.device = sk.verifying_key().to_bytes();
        let signed = sign_presence_beacon(b, &sk).unwrap();
        let c = SpaceId([0xc0; 16]);
        let key = derive_presence_key(&EpochKey::new([0x11; 32]), &c);
        let sealed = seal_presence_beacon(&key, &c, &signed).unwrap();
        assert_eq!(open_presence_beacon(&key, &c, &sealed), Some(signed));
        let other = derive_presence_key(&EpochKey::new([0x22; 32]), &c);
        assert_eq!(open_presence_beacon(&other, &c, &sealed), None);
    }
}
