//! ZEB-350 Voice V2 presence: ephemeral signed+sealed beacons + the live
//! roster. Beacons ride a dedicated Zenoh topic (never the CRDT); the seal
//! under `ChannelKey` gates non-members, and the device-#2 signature +
//! materialized-membership check (Task 7) prevents intra-member spoofing.

use crate::community_membership::ChannelId;
use crate::owner_state_types::{Hlc, SpaceId};
use serde::{Deserialize, Serialize};

/// The unsigned presence claim. Canonical CBOR, 2-char keys (same-length
/// invariant for deterministic encoding).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoicePresenceBeacon {
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
    #[serde(rename = "mu")]
    pub muted: bool,
    #[serde(rename = "jh")]
    pub joined_hlc: Hlc,
    #[serde(rename = "sq")]
    pub seq: u64,
    #[serde(rename = "lf", default, skip_serializing_if = "is_false")]
    pub left: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Beacon + detached device-#2 signature over `canonical_cbor_encode(beacon)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedVoicePresenceBeacon {
    #[serde(rename = "bc")]
    pub beacon: VoicePresenceBeacon,
    #[serde(
        rename = "sg",
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub sig: [u8; 64],
}

// Register both beacon types as `CanonicalPayload` so `canonical_cbor_encode`
// (the sealed-trait encoder) can sign/seal them. Mirrors the way
// `owner_state_crdt::OwnerState` registers its impl outside `owner_state_types`.
impl crate::owner_state_crypto::sealed::CanonicalPayloadSealed for VoicePresenceBeacon {}
impl crate::owner_state_crypto::CanonicalPayload for VoicePresenceBeacon {}
impl crate::owner_state_crypto::sealed::CanonicalPayloadSealed for SignedVoicePresenceBeacon {}
impl crate::owner_state_crypto::CanonicalPayload for SignedVoicePresenceBeacon {}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BeaconError {
    #[error("beacon CBOR encode failed")]
    Encode,
    #[error("beacon signature invalid")]
    BadSig,
}

use crate::community_channel_log::ChannelKey;
use crate::owner_state_crypto::canonical_cbor_encode;
use crate::voice_crypto::{decrypt_voice_packet, encrypt_voice_packet, VOICE_PRESENCE_AAD};

/// Sign a beacon with the device-#2 ed25519 key. The signature covers the
/// canonical CBOR of the unsigned beacon (sig field excluded by construction).
pub fn sign_presence_beacon(
    beacon: VoicePresenceBeacon,
    signing_key: &ed25519_dalek::SigningKey,
) -> Result<SignedVoicePresenceBeacon, BeaconError> {
    use ed25519_dalek::Signer;
    let bytes = canonical_cbor_encode(&beacon).map_err(|_| BeaconError::Encode)?;
    let sig = signing_key.sign(&bytes).to_bytes();
    Ok(SignedVoicePresenceBeacon { beacon, sig })
}

/// Verify the detached signature against the verifying key embedded in
/// `beacon.device`. This proves the holder of `device`'s private key signed
/// it; Task 7 additionally checks `device ∈ owner.enrolled_device_keys`.
pub fn verify_presence_beacon_sig(signed: &SignedVoicePresenceBeacon) -> Result<(), BeaconError> {
    let bytes = canonical_cbor_encode(&signed.beacon).map_err(|_| BeaconError::Encode)?;
    let vk = ed25519_dalek::VerifyingKey::from_bytes(&signed.beacon.device)
        .map_err(|_| BeaconError::BadSig)?;
    let sig = ed25519_dalek::Signature::from_bytes(&signed.sig);
    vk.verify_strict(&bytes, &sig)
        .map_err(|_| BeaconError::BadSig)
}

/// Seal a signed beacon under the channel key for transport. Output framing
/// matches the voice media packet (`[12B nonce][ct+tag]`), distinct AAD.
pub fn seal_presence_beacon(
    key: &ChannelKey,
    community: &SpaceId,
    channel: &ChannelId,
    signed: &SignedVoicePresenceBeacon,
) -> Result<Vec<u8>, BeaconError> {
    let plain = canonical_cbor_encode(signed).map_err(|_| BeaconError::Encode)?;
    encrypt_voice_packet(key, community, channel, VOICE_PRESENCE_AAD, &plain)
        .map_err(|_| BeaconError::Encode)
}

/// Open + decode a sealed beacon. Returns `None` on any failure (drop).
pub fn open_presence_beacon(
    key: &ChannelKey,
    community: &SpaceId,
    channel: &ChannelId,
    packet: &[u8],
) -> Option<SignedVoicePresenceBeacon> {
    let plain = decrypt_voice_packet(key, community, channel, VOICE_PRESENCE_AAD, packet).ok()?;
    ciborium::from_reader(plain.as_slice()).ok()
}

/// Deterministic-nonce seal for wire-format fixtures. NEVER call from
/// production — a fixed nonce with a reused key is catastrophic nonce reuse.
#[cfg(any(test, feature = "test-fixtures"))]
#[doc(hidden)]
pub fn seal_presence_beacon_with_nonce(
    key: &ChannelKey,
    community: &SpaceId,
    channel: &ChannelId,
    signed: &SignedVoicePresenceBeacon,
    nonce: [u8; 12],
) -> Result<Vec<u8>, BeaconError> {
    let plain = canonical_cbor_encode(signed).map_err(|_| BeaconError::Encode)?;
    crate::voice_crypto::encrypt_voice_packet_with_nonce(
        key,
        community,
        channel,
        VOICE_PRESENCE_AAD,
        &plain,
        nonce,
    )
    .map_err(|_| BeaconError::Encode)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    fn beacon(seq: u64) -> VoicePresenceBeacon {
        VoicePresenceBeacon {
            owner: [0xa1; 16],
            device: [0u8; 32], // overwritten by sign helper's caller in real use
            muted: true,
            joined_hlc: Hlc {
                wall_ms: 1000,
                logical: 0,
                device_id: "aa".repeat(32),
            },
            seq,
            left: false,
        }
    }

    #[test]
    fn sign_then_verify_round_trips() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let mut b = beacon(1);
        b.device = sk.verifying_key().to_bytes();
        let signed = sign_presence_beacon(b.clone(), &sk).unwrap();
        assert_eq!(signed.beacon, b);
        verify_presence_beacon_sig(&signed).expect("valid sig");
    }

    #[test]
    fn tampered_beacon_fails_verify() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let mut b = beacon(1);
        b.device = sk.verifying_key().to_bytes();
        let mut signed = sign_presence_beacon(b, &sk).unwrap();
        signed.beacon.muted = false; // tamper after signing
        assert_eq!(
            verify_presence_beacon_sig(&signed),
            Err(BeaconError::BadSig)
        );
    }

    #[test]
    fn signature_must_match_embedded_device_key() {
        let signer = SigningKey::from_bytes(&[7u8; 32]);
        let mut b = beacon(1);
        b.device = SigningKey::from_bytes(&[9u8; 32])
            .verifying_key()
            .to_bytes(); // different device
        let signed = sign_presence_beacon(b, &signer).unwrap();
        assert_eq!(
            verify_presence_beacon_sig(&signed),
            Err(BeaconError::BadSig)
        );
    }

    #[test]
    fn seal_open_round_trips_and_wrong_key_drops() {
        use crate::community_channel_log::derive_channel_key;
        use crate::owner_state_types::EpochKey;
        let sk = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let mut b = beacon(3);
        b.device = sk.verifying_key().to_bytes();
        let signed = sign_presence_beacon(b, &sk).unwrap();
        let (c, ch) = (SpaceId([0xc0; 16]), ChannelId([0xc1; 16]));
        let key = derive_channel_key(&EpochKey::new([0x11; 32]), &c, &ch);
        let sealed = seal_presence_beacon(&key, &c, &ch, &signed).unwrap();
        assert_eq!(open_presence_beacon(&key, &c, &ch, &sealed), Some(signed));
        let other = derive_channel_key(&EpochKey::new([0x22; 32]), &c, &ch);
        assert_eq!(open_presence_beacon(&other, &c, &ch, &sealed), None);
    }
}
