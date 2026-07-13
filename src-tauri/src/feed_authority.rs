//! ZEB-678 S1: the per-feed authority record that owner-anchors a vine feed.
//!
//! A vine feed is keyed on a device's `#3` node address `N` (the Zenoh topic
//! `harmony/vines/{N}`). This record makes that feed owner-anchored in place:
//! its `n_sig` binds `feed_id (N)` to an `owner_id (O)`, `device_id (D)`, and
//! the enrolled `#2` `publisher_key (K)` under N's `#3` key (signed once), and
//! the chokepoint-verified enrollment proves `K`/`D` are enrolled under `O`.
//! An optional revocation marks the publisher device revoked. Followers pin
//! the binding on first valid sight (first-write-wins) and treat `revoked` as
//! monotonic-true (§4). JSON on the wire — vines are `serde_json`, not CBOR.
//!
//! Because the crate cert types (`EnrollmentCert`/`RevocationCert`) are
//! CBOR-native and do NOT round-trip through JSON, they ride the record as
//! canonical-CBOR-hex blobs (the same `signer_certs_cbor` idiom the butler /
//! relay frames use), decoded at verify time.
//!
//! S1 is data + verify only: no publish/engine wiring, no `revoke_device`
//! hook, no reactions or signing migration — those are S2/S3.

use harmony_owner::certs::{EnrollmentCert, RevocationCert};
use serde::{Deserialize, Serialize};

/// ZEB-678 §3.1 — the per-feed record that owner-anchors a vine feed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedAuthorityRecord {
    /// Hex node address `N` — equals `hex(hash(n_identity_pub))` and the feed topic.
    pub feed_id: String,
    /// Hex 16-byte harmony-owner `owner_id` (O).
    pub owner_id: String,
    /// Hex 16-byte `EnrollmentCert.device_id` (D).
    pub device_id: String,
    /// Hex 32-byte enrolled `#2` ed25519 key (K).
    pub publisher_key: String,
    /// Hex 64-byte `#3` pubkey (X25519(32) || Ed25519(32)) whose hash is `feed_id`.
    pub n_identity_pub: String,
    /// CBOR-hex of the `EnrollmentCert` proving `publisher_key`/`device_id`
    /// are enrolled under `owner_id`.
    pub enrollment_cbor_hex: String,
    /// CBOR-hex of `Vec<EnrollmentCert>` (quorum signer bundle); "" ⇒ master-issued.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub signer_certs_cbor_hex: String,
    /// CBOR-hex of the `RevocationCert`; present ⇒ the publisher device is revoked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revocation_cbor_hex: Option<String>,
    /// LWW clock (HLC wall_ms).
    pub updated_at: u64,
    /// Hex 64-byte `#3` signature over `authority_binding_bytes`.
    pub n_sig: String,
}

/// CBOR-hex encode a single `EnrollmentCert` for the `enrollment_cbor_hex` field.
pub fn encode_cert(cert: &EnrollmentCert) -> String {
    let mut buf = Vec::new();
    ciborium::into_writer(cert, &mut buf).expect("cbor-encode enrollment cert");
    hex::encode(buf)
}

/// CBOR-hex encode a quorum signer bundle for `signer_certs_cbor_hex`
/// (empty ⇒ `""`, which serde omits).
pub fn encode_certs(certs: &[EnrollmentCert]) -> String {
    if certs.is_empty() {
        return String::new();
    }
    let mut buf = Vec::new();
    ciborium::into_writer(&certs, &mut buf).expect("cbor-encode signer certs");
    hex::encode(buf)
}

/// CBOR-hex encode a `RevocationCert` for `revocation_cbor_hex`.
pub fn encode_revocation(rev: &RevocationCert) -> String {
    let mut buf = Vec::new();
    ciborium::into_writer(rev, &mut buf).expect("cbor-encode revocation cert");
    hex::encode(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enrollment_verify::quorum_fixtures::{
        mint_quorum_revocation, mint_quorum_world, WORLD_NOW,
    };

    fn sample(
        revocation: Option<RevocationCert>,
        signer_certs: Vec<EnrollmentCert>,
    ) -> FeedAuthorityRecord {
        let world = mint_quorum_world(0x80);
        FeedAuthorityRecord {
            feed_id: "aa".into(),
            owner_id: hex::encode(world.owner_id),
            device_id: hex::encode(world.a_cert.device_id),
            publisher_key: hex::encode(world.a_cert.device_pubkeys.classical.ed25519_verify),
            n_identity_pub: "bb".into(),
            enrollment_cbor_hex: encode_cert(&world.a_cert),
            signer_certs_cbor_hex: encode_certs(&signer_certs),
            revocation_cbor_hex: revocation.as_ref().map(encode_revocation),
            updated_at: 1_700_000_000_000,
            n_sig: "cc".into(),
        }
    }

    #[test]
    fn serde_omits_empty_signer_certs_and_revocation() {
        let json = serde_json::to_string(&sample(None, Vec::new())).unwrap();
        assert!(
            !json.contains("signerCertsCborHex"),
            "empty bundle must be omitted: {json}"
        );
        assert!(
            !json.contains("revocationCborHex"),
            "None revocation must be omitted: {json}"
        );
        assert!(
            json.contains("feedId") && json.contains("nSig") && json.contains("enrollmentCborHex"),
            "camelCase keys: {json}"
        );
    }

    #[test]
    fn serde_includes_populated_optional_fields() {
        let world = mint_quorum_world(0x84);
        let rev = mint_quorum_revocation(&world, world.c_quorum_cert.device_id, WORLD_NOW);
        let json = serde_json::to_string(&sample(Some(rev), world.bundle.clone())).unwrap();
        assert!(
            json.contains("signerCertsCborHex"),
            "populated bundle present: {json}"
        );
        assert!(
            json.contains("revocationCborHex"),
            "Some revocation present: {json}"
        );
    }

    #[test]
    fn json_round_trips() {
        let rec = sample(None, Vec::new());
        let json = serde_json::to_string(&rec).unwrap();
        let back: FeedAuthorityRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(serde_json::to_string(&back).unwrap(), json);
    }
}
