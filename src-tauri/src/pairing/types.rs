use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PairingRole {
    Inviter,
    Joiner,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredPeer {
    pub session_id: Uuid,
    pub role: PairingRole,
    pub display_name: String,
    /// Set only when the peer is an Inviter — the owner identity hash.
    pub owner_id_if_inviter: Option<String>, // 32-hex
    pub ephemeral_pubkey_hex: String,
    /// Set only when the peer is a Joiner — the ed25519 verifying key the
    /// Inviter must sign the EnrollmentCert against. Different curve, different
    /// key from `ephemeral_pubkey_hex` (which is X25519 for SAS / session key).
    pub joiner_ed25519_verify_hex: Option<String>, // 64-hex (32 bytes)
    pub seen_at_unix: u64,
}

// `rename_all` renames variant names (e.g. Discovering → "discovering").
// `rename_all_fields` renames fields within struct variants to camelCase.
// Both are needed: serde does NOT cascade container `rename_all` to fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum PairingState {
    Idle,
    Discovering {
        role: PairingRole,
        ephemeral_pubkey_hex: String,
        session_id: Uuid,
    },
    Discovered {
        peers: Vec<DiscoveredPeer>,
    },
    Handshaking {
        peer_session_id: Uuid,
        sas_digits: String, // exactly 6 chars
    },
    WaitingPeerConfirm {
        peer_session_id: Uuid,
    },
    Enrolling,
    Complete {
        device_id_hex: String, // 32-hex
    },
    Failed {
        reason: String,
    },
}

/// Wire messages exchanged on `harmony/pairing/v2/lan/<session-id>/<phase>`.
/// DISCOVER and SELECT are plaintext (needed for discovery + selection).
/// CONFIRM and ENROLL are encrypted under the derived session_key.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "phase",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum PairingWireMessage {
    Discover {
        session_id: Uuid,
        role: PairingRole,
        ephemeral_pubkey_hex: String,
        display_name: String,
        owner_id_if_inviter: Option<String>,
        /// Joiner publishes its ed25519 verifying key here so the Inviter can
        /// sign the EnrollmentCert against it. Inviter omits (None).
        joiner_ed25519_verify_hex: Option<String>,
    },
    /// Sent when the local user clicks the peer's row.
    Select {
        my_session_id: Uuid,
        peer_session_id: Uuid,
    },
    /// Encrypted-payload envelope. Inner bytes are XChaCha20-Poly1305
    /// ciphertext; the inner plaintext is `EncryptedPayload` encoded with
    /// CBOR via ciborium (matches the wire encoding used everywhere else
    /// in this crate; the roundtrip tests below verify it).
    Encrypted {
        my_session_id: Uuid,
        peer_session_id: Uuid,
        nonce_hex: String, // 24 bytes hex
        ciphertext_hex: String,
    },
    Cancel {
        my_session_id: Uuid,
        peer_session_id: Option<Uuid>,
        reason: String,
    },
}

/// Plaintext payload that gets encrypted into `PairingWireMessage::Encrypted`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum EncryptedPayload {
    Confirm {
        sas_digits: String,
    },
    Enroll {
        enrollment_cert_cbor_hex: String,
        owner_state_cbor_hex: String,
        joiner_advisory_display_name: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_state_serde_camel_case() {
        let s = PairingState::Discovering {
            role: PairingRole::Inviter,
            ephemeral_pubkey_hex: "deadbeef".to_string(),
            session_id: Uuid::nil(),
        };
        let j = serde_json::to_string(&s).unwrap();
        assert!(j.contains("\"discovering\""));
        assert!(j.contains("ephemeralPubkeyHex"));
        assert!(j.contains("sessionId"));
    }

    #[test]
    fn role_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&PairingRole::Inviter).unwrap(),
            "\"inviter\""
        );
        assert_eq!(
            serde_json::to_string(&PairingRole::Joiner).unwrap(),
            "\"joiner\""
        );
    }

    #[test]
    fn wire_message_roundtrips() {
        let m = PairingWireMessage::Discover {
            session_id: Uuid::nil(),
            role: PairingRole::Joiner,
            ephemeral_pubkey_hex: "00".repeat(32),
            display_name: "AVALON".to_string(),
            owner_id_if_inviter: None,
            joiner_ed25519_verify_hex: None,
        };
        let mut bytes = Vec::new();
        ciborium::into_writer(&m, &mut bytes).unwrap();
        let back: PairingWireMessage = ciborium::from_reader(bytes.as_slice()).unwrap();
        assert!(matches!(back, PairingWireMessage::Discover { .. }));
    }

    #[test]
    fn encrypted_payload_roundtrips() {
        let p = EncryptedPayload::Confirm {
            sas_digits: "012845".to_string(),
        };
        let mut bytes = Vec::new();
        ciborium::into_writer(&p, &mut bytes).unwrap();
        let back: EncryptedPayload = ciborium::from_reader(bytes.as_slice()).unwrap();
        match back {
            EncryptedPayload::Confirm { sas_digits } => assert_eq!(sas_digits, "012845"),
            _ => panic!("wrong variant"),
        }
    }
}
