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

/// Domain-separation prefix + version for the authority binding bytes.
const AUTHORITY_DOMAIN: &str = "harmony-vine-authority-v1";

/// Length-prefixed bytes the `n_sig` covers — ONLY the immutable binding
/// fields (§3.1). `updated_at`/revocation are authenticated separately, so a
/// benign clock refresh or an appended revocation never invalidates `n_sig`.
pub fn authority_binding_bytes(r: &FeedAuthorityRecord) -> Vec<u8> {
    let mut out = Vec::with_capacity(256);
    crate::vine_signing::push_str(&mut out, AUTHORITY_DOMAIN);
    crate::vine_signing::push_str(&mut out, &r.feed_id);
    crate::vine_signing::push_str(&mut out, &r.owner_id);
    crate::vine_signing::push_str(&mut out, &r.device_id);
    crate::vine_signing::push_str(&mut out, &r.publisher_key);
    out
}

/// Set `feed_id` (= the `#3` address), `n_identity_pub`, and `n_sig` in place.
/// Mirrors `vine_signing::sign_descriptor`; the `#3` key is used exactly once
/// per feed, to establish this binding.
pub fn sign_authority_binding(
    private: &harmony_identity::PrivateIdentity,
    r: &mut FeedAuthorityRecord,
) {
    r.feed_id = crate::vine_signing::signer_address(private);
    r.n_identity_pub = hex::encode(private.public_identity().to_public_bytes());
    let bytes = authority_binding_bytes(r);
    r.n_sig = hex::encode(private.sign(&bytes));
}

/// Verify the `#3` binding: `n_identity_pub` hashes to `feed_id`, and `n_sig`
/// is a strict Ed25519 signature over the binding bytes. Mirrors
/// `vine_signing::verify_signed`.
pub fn verify_binding(r: &FeedAuthorityRecord) -> Result<(), String> {
    let pub_vec = hex::decode(&r.n_identity_pub)
        .map_err(|e| format!("authority n_identity_pub not hex: {e}"))?;
    let identity = harmony_identity::Identity::from_public_bytes(&pub_vec)
        .map_err(|_| "authority n_identity_pub invalid".to_string())?;
    if hex::encode(identity.address_hash) != r.feed_id {
        return Err("authority n_identity_pub does not match feed_id".to_string());
    }
    let sig_bytes: [u8; 64] = hex::decode(&r.n_sig)
        .map_err(|e| format!("authority n_sig not hex: {e}"))?
        .try_into()
        .map_err(|_| "authority n_sig must be 64 bytes".to_string())?;
    let sig = ed25519_dalek::Signature::from_bytes(&sig_bytes);
    identity
        .verifying_key
        .verify_strict(&authority_binding_bytes(r), &sig)
        .map_err(|_| "authority binding signature invalid".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gen_identity() -> harmony_identity::PrivateIdentity {
        harmony_identity::PrivateIdentity::generate(&mut rand::rngs::OsRng)
    }

    /// Build a record for `cert` under `world`, signed by `#3` identity `n`.
    /// Reused across the binding, verify, and cache tests — same-feed tests
    /// thread the SAME `n` so `feed_id` stays constant.
    fn record_for(
        world: &crate::enrollment_verify::quorum_fixtures::QuorumWorld,
        cert: &EnrollmentCert,
        signer_certs: Vec<EnrollmentCert>,
        revocation: Option<RevocationCert>,
        updated_at_secs: u64,
        n: &harmony_identity::PrivateIdentity,
    ) -> FeedAuthorityRecord {
        let mut rec = FeedAuthorityRecord {
            feed_id: String::new(),
            owner_id: hex::encode(world.owner_id),
            device_id: hex::encode(cert.device_id),
            publisher_key: hex::encode(cert.device_pubkeys.classical.ed25519_verify),
            n_identity_pub: String::new(),
            enrollment_cbor_hex: encode_cert(cert),
            signer_certs_cbor_hex: encode_certs(&signer_certs),
            revocation_cbor_hex: revocation.as_ref().map(encode_revocation),
            updated_at: updated_at_secs * 1000,
            n_sig: String::new(),
        };
        sign_authority_binding(n, &mut rec);
        rec
    }
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

    #[test]
    fn binding_signs_and_verifies() {
        let world = mint_quorum_world(0x88);
        let n = gen_identity();
        let rec = record_for(&world, &world.a_cert, Vec::new(), None, WORLD_NOW, &n);
        assert_eq!(rec.feed_id, hex::encode(n.public_identity().address_hash));
        verify_binding(&rec).expect("valid binding verifies");
    }

    #[test]
    fn binding_rejects_wrong_feed_id() {
        let world = mint_quorum_world(0x8C);
        let n = gen_identity();
        let mut rec = record_for(&world, &world.a_cert, Vec::new(), None, WORLD_NOW, &n);
        rec.feed_id = "00".repeat(20); // no longer matches hash(n_identity_pub)
        assert!(verify_binding(&rec).is_err());
    }

    #[test]
    fn binding_rejects_tampered_bound_field() {
        let world = mint_quorum_world(0x90);
        let n = gen_identity();
        let mut rec = record_for(&world, &world.a_cert, Vec::new(), None, WORLD_NOW, &n);
        rec.owner_id = "11".repeat(16); // covered by n_sig ⇒ signature no longer matches
        assert!(verify_binding(&rec).is_err());
    }
}
