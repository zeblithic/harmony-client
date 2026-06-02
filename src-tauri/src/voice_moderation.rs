//! ZEB-358 community voice moderation: power-gated server-mute + remove-from-
//! voice. A device-#2-signed, ChannelKey-sealed directive rides a dedicated
//! Zenoh control topic (never the CRDT); honest clients enforce it receiver-
//! side (drop the target's audio + hide/flag them). Mute and kick are the same
//! time-boxed directive. Mirrors `voice_presence.rs`.

use crate::community_membership::ChannelId;
use crate::owner_state_types::{Hlc, SpaceId};
use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};

/// Liveness TTL: a directive stays effective this long after last receipt.
pub const ENFORCE_TTL_MS: u64 = 12_000;
/// Issuer re-publishes each active directive this often (< ENFORCE_TTL_MS).
pub const RE_ASSERT_INTERVAL_MS: u64 = 4_000;
/// Default moderator-chosen duration (5 min), enforced issuer-side.
pub const DEFAULT_MODERATION_MS: u64 = 300_000;
/// Minimum power to moderate (reuses the existing `kick` threshold).
pub const MOD_POWER: u8 = 50;

/// What a directive asserts about the target owner. `serde_repr` encodes each
/// variant as its bare u8 discriminant (mirrors `ChannelKind`) and rejects
/// unknown discriminants on decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr)]
#[repr(u8)]
pub enum ModAction {
    Mute = 0,
    Unmute = 1,
    Kick = 2,
    Unkick = 3,
}

impl ModAction {
    /// True for the mute class {Mute, Unmute}; false for the kick class.
    pub fn is_mute_class(self) -> bool {
        matches!(self, ModAction::Mute | ModAction::Unmute)
    }
    /// True for the "positive" directives that turn enforcement ON.
    pub fn enforces(self) -> bool {
        matches!(self, ModAction::Mute | ModAction::Kick)
    }
}

/// Unsigned directive. Canonical CBOR, 2-char keys (same-length invariant).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceModerationDirective {
    #[serde(
        rename = "ao",
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub actor_owner: [u8; 16],
    #[serde(
        rename = "ad",
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub actor_device: [u8; 32],
    #[serde(
        rename = "to",
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub target_owner: [u8; 16],
    #[serde(rename = "ac")]
    pub action: ModAction,
    #[serde(rename = "ih")]
    pub issued_hlc: Hlc,
    #[serde(rename = "sq")]
    pub seq: u64,
}

/// Directive + detached device-#2 signature over `canonical_cbor_encode(directive)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedVoiceModerationDirective {
    #[serde(rename = "dr")]
    pub directive: VoiceModerationDirective,
    #[serde(
        rename = "sg",
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub sig: [u8; 64],
}

impl crate::owner_state_crypto::sealed::CanonicalPayloadSealed for ModAction {}
impl crate::owner_state_crypto::CanonicalPayload for ModAction {}
impl crate::owner_state_crypto::sealed::CanonicalPayloadSealed for VoiceModerationDirective {}
impl crate::owner_state_crypto::CanonicalPayload for VoiceModerationDirective {}
impl crate::owner_state_crypto::sealed::CanonicalPayloadSealed for SignedVoiceModerationDirective {}
impl crate::owner_state_crypto::CanonicalPayload for SignedVoiceModerationDirective {}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ModError {
    /// Canonical-CBOR encoding of the directive failed (a serialization bug).
    #[error("directive CBOR encode failed")]
    Encode,
    /// AEAD seal under the channel key failed — a crypto/runtime fault, kept
    /// distinct from `Encode` so a seal failure can be diagnosed separately.
    #[error("directive seal failed")]
    Seal,
    #[error("directive signature invalid")]
    BadSig,
    #[error("signer is not an enrolled, joined member")]
    NotMember,
    #[error("signer lacks moderation power over target")]
    NotAuthorized,
    /// `session.put` failed — a transport/runtime fault, distinct from an
    /// encode/seal fault, so callers can diagnose network failures separately.
    #[error("directive transport publish failed")]
    Publish,
}

use crate::community_channel_log::ChannelKey;
use crate::owner_state_crypto::canonical_cbor_encode;
use crate::voice_crypto::{decrypt_voice_packet, encrypt_voice_packet, VOICE_MODERATION_AAD};

/// Sign a directive with the device-#2 ed25519 key. The signature covers the
/// canonical CBOR of the unsigned directive (the `sig` field is excluded by
/// construction). Mirrors `voice_presence::sign_presence_beacon`.
pub fn sign_directive(
    directive: VoiceModerationDirective,
    signing_key: &ed25519_dalek::SigningKey,
) -> Result<SignedVoiceModerationDirective, ModError> {
    use ed25519_dalek::Signer;
    let bytes = canonical_cbor_encode(&directive).map_err(|_| ModError::Encode)?;
    let sig = signing_key.sign(&bytes).to_bytes();
    Ok(SignedVoiceModerationDirective { directive, sig })
}

/// Verify the detached signature against the verifying key embedded in
/// `directive.actor_device`. This proves the holder of that device key signed
/// it; authority (device ∈ actor's enrolled keys + power) is checked separately
/// in `verify_directive_authority` (Task 3).
pub fn verify_directive_sig(signed: &SignedVoiceModerationDirective) -> Result<(), ModError> {
    let bytes = canonical_cbor_encode(&signed.directive).map_err(|_| ModError::Encode)?;
    let vk = ed25519_dalek::VerifyingKey::from_bytes(&signed.directive.actor_device)
        .map_err(|_| ModError::BadSig)?;
    let sig = ed25519_dalek::Signature::from_bytes(&signed.sig);
    vk.verify_strict(&bytes, &sig).map_err(|_| ModError::BadSig)
}

/// Seal a signed directive under the channel key for transport. Output framing
/// matches the voice media/presence packet (`[12B nonce][ct+tag]`), distinct
/// AAD (`VOICE_MODERATION_AAD ‖ community ‖ channel`).
pub fn seal_directive(
    key: &ChannelKey,
    community: &SpaceId,
    channel: &ChannelId,
    signed: &SignedVoiceModerationDirective,
) -> Result<Vec<u8>, ModError> {
    let plain = canonical_cbor_encode(signed).map_err(|_| ModError::Encode)?;
    encrypt_voice_packet(key, community, channel, VOICE_MODERATION_AAD, &plain)
        .map_err(|_| ModError::Seal)
}

/// Open + decode a sealed directive. Returns `None` on any failure (drop) —
/// wrong key, wrong (community, channel) scope, tampered ciphertext, or bad CBOR.
pub fn open_directive(
    key: &ChannelKey,
    community: &SpaceId,
    channel: &ChannelId,
    packet: &[u8],
) -> Option<SignedVoiceModerationDirective> {
    let plain = decrypt_voice_packet(key, community, channel, VOICE_MODERATION_AAD, packet).ok()?;
    ciborium::from_reader(plain.as_slice()).ok()
}

#[cfg(any(test, feature = "test-fixtures"))]
#[doc(hidden)]
pub fn seal_directive_with_nonce(
    key: &ChannelKey,
    community: &SpaceId,
    channel: &ChannelId,
    signed: &SignedVoiceModerationDirective,
    nonce: [u8; 12],
) -> Result<Vec<u8>, ModError> {
    let plain = canonical_cbor_encode(signed).map_err(|_| ModError::Encode)?;
    crate::voice_crypto::encrypt_voice_packet_with_nonce(
        key,
        community,
        channel,
        VOICE_MODERATION_AAD,
        &plain,
        nonce,
    )
    .map_err(|_| ModError::Seal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_channel_log::derive_channel_key;
    use crate::owner_state_types::EpochKey;

    fn key() -> ChannelKey {
        derive_channel_key(
            &EpochKey::new([9u8; 32]),
            &SpaceId([1u8; 16]),
            &ChannelId([2u8; 16]),
        )
    }
    fn sk() -> ed25519_dalek::SigningKey {
        ed25519_dalek::SigningKey::from_bytes(&[3u8; 32])
    }
    const C: SpaceId = SpaceId([1u8; 16]);
    const CH: ChannelId = ChannelId([2u8; 16]);

    fn directive(action: ModAction, vk: [u8; 32]) -> VoiceModerationDirective {
        VoiceModerationDirective {
            actor_owner: [0xAA; 16],
            actor_device: vk,
            target_owner: [0xBB; 16],
            action,
            issued_hlc: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "x".into(),
            },
            seq: 1,
        }
    }

    #[test]
    fn mod_action_discriminant_and_roundtrip() {
        for a in [
            ModAction::Mute,
            ModAction::Unmute,
            ModAction::Kick,
            ModAction::Unkick,
        ] {
            let b = canonical_cbor_encode(&a).unwrap();
            let back: ModAction = ciborium::from_reader(b.as_slice()).unwrap();
            assert_eq!(a, back);
        }
        assert_eq!(
            canonical_cbor_encode(&ModAction::Kick).unwrap(),
            canonical_cbor_encode(&2u8).unwrap()
        );
    }

    #[test]
    fn sign_then_verify_ok_and_tamper_fails() {
        let signing = sk();
        let vk = signing.verifying_key().to_bytes();
        let signed = sign_directive(directive(ModAction::Mute, vk), &signing).unwrap();
        assert!(verify_directive_sig(&signed).is_ok());
        let mut bad = signed.clone();
        bad.directive.action = ModAction::Unmute;
        assert_eq!(verify_directive_sig(&bad), Err(ModError::BadSig));
    }

    #[test]
    fn seal_open_roundtrip_and_wrong_channel_drops() {
        let signing = sk();
        let vk = signing.verifying_key().to_bytes();
        let signed = sign_directive(directive(ModAction::Kick, vk), &signing).unwrap();
        let sealed = seal_directive(&key(), &C, &CH, &signed).unwrap();
        let opened = open_directive(&key(), &C, &CH, &sealed).unwrap();
        assert_eq!(opened, signed);
        let other_ch = ChannelId([0xEE; 16]);
        assert!(open_directive(&key(), &C, &other_ch, &sealed).is_none());
    }
}
