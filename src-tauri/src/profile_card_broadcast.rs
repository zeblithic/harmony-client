//! ZEB-341 — owner_id-keyed, EnrollmentCert-verified profile card broadcast.
//! Sibling to `profile_broadcast.rs`. Carries a peer's display name + status,
//! bound to their harmony-owner `owner_id` via the ZEB-339 cert model.
//! Spec: docs/specs/2026-05-30-zeb-341-profile-cards-design.md

use crate::owner_state_crypto::{
    canonical_cbor_encode, sealed::CanonicalPayloadSealed, CanonicalPayload, CryptoError,
};
use crate::owner_state_types::Hlc;
use ed25519_dalek::{Signer, SigningKey};
use harmony_owner::certs::EnrollmentCert;
use serde::{Deserialize, Serialize};

pub const PROFILE_CARD_TOPIC_PREFIX: &str = "harmony/discovery/profile/owner/";
pub const MAX_DISPLAY_NAME_BYTES: usize = 64;
pub const MAX_STATUS_TEXT_BYTES: usize = 128;
#[allow(dead_code)]
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
    #[error("canonical CBOR encode failed: {0}")]
    Encode(#[from] CryptoError),
}

/// Build + Ed25519-sign a card over canonical CBOR with `signature` zeroed.
/// `signer` MUST be the enrolled device key (pub ==
/// `enrollment.device_pubkeys.classical.ed25519_verify`).
pub fn sign_card(
    signer: &SigningKey,
    owner_id: [u8; 16],
    display_name: String,
    status_text: String,
    enrollment: EnrollmentCert,
    shared_at: Hlc,
) -> Result<ProfileCardBroadcast, CardError> {
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
        enrollment,
        shared_at,
        signature: [0u8; 64],
    };
    let bytes = canonical_cbor_encode(&card)?;
    card.signature = signer.sign(&bytes).to_bytes();
    Ok(card)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_card_round_trips_and_signature_verifies_under_device_key() {
        use ed25519_dalek::Verifier;
        let owner = crate::community_membership::mint_test_owner(0x41);
        let card = sign_card(
            &owner.device_key,
            owner.owner.0,
            "Jake (Koya Dev)".into(),
            "building".into(),
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
                owner.cert,
                hlc
            ),
            Err(CardError::StatusTextTooLong)
        ));
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
}
