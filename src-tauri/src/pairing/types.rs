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
    /// ZEB-677 S4: a seedless (master-less) inviter has opened a quorum
    /// enrollment request and is waiting for an armed sibling to co-sign so
    /// it can assemble the K=2 `EnrollmentCert`. Bounded by the SM's 120 s
    /// ceremony deadline; on timeout/error it transitions to `Failed`.
    /// Serializes as `awaitingQuorumCosign` (unit variant, camelCase tag).
    AwaitingQuorumCosign,
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
        /// ZEB-510 step 2: the sender's iroh transport endpoint, observed
        /// first-hand over the SAS-authenticated channel so each device can seed
        /// a dial route to its fleet sibling before fleet-net converges. Hex of
        /// the 32-byte iroh node_id. `#[serde(default)]` keeps pre-step-2 peers
        /// decodable (they omit it; the receiver tolerates `None`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        iroh_node_id_hex: Option<String>,
        /// ZEB-510 step 2: the sender's iroh home-relay URL (may be empty even
        /// when `iroh_node_id_hex` is present, if the relay is not yet known).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        iroh_home_relay: Option<String>,
    },
    Enroll {
        enrollment_cert_cbor_hex: String,
        owner_state_cbor_hex: String,
        joiner_advisory_display_name: String,
        /// ZEB-492: CBOR-of-`FleetKeyMaterial`, hex-encoded — the owner's fleet
        /// KeyTree sealed to the joiner so a cert-only device can build the
        /// fleet engines and act as a butler. `#[serde(default)]` keeps the
        /// payload backward/forward-compatible (pre-ZEB-492 inviters omit it).
        #[serde(default)]
        fleet_keytree_cbor_hex: Option<String>,
        /// ZEB-668 S5: CBOR-of-`Vec<FleetKeyMaterial>`, hex-encoded — the
        /// multi-epoch set (epoch-0 + current) for joiners enrolled after a
        /// fleet epoch bump. `#[serde(default)]` keeps pre-S5 payloads
        /// decodable; pre-S5 joiners ignore the unknown field and fall back
        /// to the single-material field above (epoch-0 only — such a build
        /// cannot follow a bumped fleet anyway, release-noted).
        #[serde(default)]
        fleet_keytree_set_cbor_hex: Option<String>,
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
    fn confirm_carries_iroh_endpoint_and_omits_when_absent() {
        // Present: round-trips through CBOR.
        let with = EncryptedPayload::Confirm {
            sas_digits: "123456".into(),
            iroh_node_id_hex: Some("ab".repeat(32)),
            iroh_home_relay: Some("https://relay.example/".into()),
        };
        let mut buf = Vec::new();
        ciborium::into_writer(&with, &mut buf).unwrap();
        let back: EncryptedPayload = ciborium::from_reader(&buf[..]).unwrap();
        match back {
            EncryptedPayload::Confirm {
                sas_digits,
                iroh_node_id_hex,
                iroh_home_relay,
            } => {
                assert_eq!(sas_digits, "123456");
                assert_eq!(iroh_node_id_hex.as_deref(), Some("ab".repeat(32).as_str()));
                assert_eq!(iroh_home_relay.as_deref(), Some("https://relay.example/"));
            }
            _ => panic!("expected Confirm"),
        }

        // Absent: `skip_serializing_if` omits the endpoint keys from the wire,
        // and `#[serde(default)]` fills them as None on decode — this IS the
        // back-compat guarantee (a pre-step-2 peer's Confirm never carries them).
        let without = EncryptedPayload::Confirm {
            sas_digits: "654321".into(),
            iroh_node_id_hex: None,
            iroh_home_relay: None,
        };
        let mut buf2 = Vec::new();
        ciborium::into_writer(&without, &mut buf2).unwrap();
        let back2: EncryptedPayload = ciborium::from_reader(&buf2[..]).unwrap();
        match back2 {
            EncryptedPayload::Confirm {
                sas_digits,
                iroh_node_id_hex,
                iroh_home_relay,
            } => {
                assert_eq!(sas_digits, "654321");
                assert!(iroh_node_id_hex.is_none());
                assert!(iroh_home_relay.is_none());
            }
            _ => panic!("expected Confirm"),
        }
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
            iroh_node_id_hex: None,
            iroh_home_relay: None,
        };
        let mut bytes = Vec::new();
        ciborium::into_writer(&p, &mut bytes).unwrap();
        let back: EncryptedPayload = ciborium::from_reader(bytes.as_slice()).unwrap();
        match back {
            EncryptedPayload::Confirm { sas_digits, .. } => assert_eq!(sas_digits, "012845"),
            _ => panic!("wrong variant"),
        }
    }

    /// ZEB-690 (item 3): pin the wire back-compat contract that
    /// `#[serde(default, skip_serializing_if = "Option::is_none")]` provides —
    /// a None-Confirm emits NO iroh keys (byte-identical to old wire), and a
    /// hand-built old-style map decodes to None iroh fields. Distinct from the
    /// round-trip above, which only proves current-serializer↔deserializer symmetry.
    #[test]
    fn confirm_none_fields_omit_iroh_keys_and_old_wire_decodes() {
        use ciborium::value::Value;

        // (a) Forward: a None-Confirm serializes WITHOUT the iroh keys.
        let p = EncryptedPayload::Confirm {
            sas_digits: "012845".to_string(),
            iroh_node_id_hex: None,
            iroh_home_relay: None,
        };
        let mut bytes = Vec::new();
        ciborium::into_writer(&p, &mut bytes).unwrap();
        let v: Value = ciborium::from_reader(bytes.as_slice()).unwrap();
        let keys: Vec<String> = match &v {
            Value::Map(entries) => entries
                .iter()
                .filter_map(|(k, _)| match k {
                    Value::Text(s) => Some(s.clone()),
                    _ => None,
                })
                .collect(),
            _ => panic!("expected CBOR map"),
        };
        assert!(keys.contains(&"kind".to_string()));
        assert!(keys.contains(&"sasDigits".to_string()));
        assert!(
            !keys.contains(&"irohNodeIdHex".to_string()),
            "None iroh_node_id_hex must be skipped: {keys:?}"
        );
        assert!(
            !keys.contains(&"irohHomeRelay".to_string()),
            "None iroh_home_relay must be skipped: {keys:?}"
        );

        // (b) Backward: hand-built old-style wire (kind + sasDigits ONLY) decodes
        // to a Confirm with both iroh fields None.
        let old = Value::Map(vec![
            (Value::Text("kind".into()), Value::Text("confirm".into())),
            (
                Value::Text("sasDigits".into()),
                Value::Text("012845".into()),
            ),
        ]);
        let mut old_bytes = Vec::new();
        ciborium::into_writer(&old, &mut old_bytes).unwrap();
        let decoded: EncryptedPayload = ciborium::from_reader(old_bytes.as_slice()).unwrap();
        match decoded {
            EncryptedPayload::Confirm {
                sas_digits,
                iroh_node_id_hex,
                iroh_home_relay,
            } => {
                assert_eq!(sas_digits, "012845");
                assert!(iroh_node_id_hex.is_none());
                assert!(iroh_home_relay.is_none());
            }
            _ => panic!("expected Confirm"),
        }
    }
}
