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

fn decode_hex16(s: &str, what: &str) -> Result<[u8; 16], String> {
    hex::decode(s)
        .map_err(|e| format!("authority {what} not hex: {e}"))?
        .try_into()
        .map_err(|_| format!("authority {what} must be 16 bytes"))
}

fn decode_hex32(s: &str, what: &str) -> Result<[u8; 32], String> {
    hex::decode(s)
        .map_err(|e| format!("authority {what} not hex: {e}"))?
        .try_into()
        .map_err(|_| format!("authority {what} must be 32 bytes"))
}

fn decode_cert(hexs: &str) -> Result<EnrollmentCert, String> {
    let bytes = hex::decode(hexs).map_err(|e| format!("authority enrollment not hex: {e}"))?;
    let mut cur = std::io::Cursor::new(&bytes);
    let cert = ciborium::from_reader(&mut cur)
        .map_err(|_| "authority enrollment cbor invalid".to_string())?;
    if cur.position() as usize != bytes.len() {
        return Err("authority enrollment cbor has trailing bytes".to_string());
    }
    Ok(cert)
}

fn decode_certs(hexs: &str) -> Result<Vec<EnrollmentCert>, String> {
    if hexs.is_empty() {
        return Ok(Vec::new());
    }
    let bytes = hex::decode(hexs).map_err(|e| format!("authority signer_certs not hex: {e}"))?;
    let mut cur = std::io::Cursor::new(&bytes);
    let certs = ciborium::from_reader(&mut cur)
        .map_err(|_| "authority signer_certs cbor invalid".to_string())?;
    if cur.position() as usize != bytes.len() {
        return Err("authority signer_certs cbor has trailing bytes".to_string());
    }
    Ok(certs)
}

fn decode_revocation(hexs: &str) -> Result<RevocationCert, String> {
    let bytes = hex::decode(hexs).map_err(|e| format!("authority revocation not hex: {e}"))?;
    let mut cur = std::io::Cursor::new(&bytes);
    let rev = ciborium::from_reader(&mut cur)
        .map_err(|_| "authority revocation cbor invalid".to_string())?;
    if cur.position() as usize != bytes.len() {
        return Err("authority revocation cbor has trailing bytes".to_string());
    }
    Ok(rev)
}

/// The verified core of an authority record: the pinned identity and whether
/// this record carries a valid revocation of that device.
#[derive(Debug, Clone, Copy)]
pub struct VerifiedAuthority {
    pub device_id: [u8; 16],
    pub publisher_key: [u8; 32],
    pub revoked: bool,
}

/// Full §4 verification: (1) `#3` binding, (2) `#2` enrollment through the
/// chokepoint against the claimed owner with `publisher_key`/`device_id`
/// cross-checks, (3) optional revocation (verified at its own `issued_at`,
/// target must equal `device_id`).
pub fn verify_authority(r: &FeedAuthorityRecord) -> Result<VerifiedAuthority, String> {
    verify_binding(r)?;
    let owner_id = decode_hex16(&r.owner_id, "owner_id")?;
    let device_id = decode_hex16(&r.device_id, "device_id")?;
    let publisher_key = decode_hex32(&r.publisher_key, "publisher_key")?;
    let enrollment = decode_cert(&r.enrollment_cbor_hex)?;
    let signer_certs = decode_certs(&r.signer_certs_cbor_hex)?;

    let now_secs = r.updated_at / 1000;
    let verified = crate::enrollment_verify::verify_enrollment_any_issuer(
        &enrollment,
        &signer_certs,
        Some(&owner_id),
        now_secs,
    )
    .map_err(|e| format!("authority enrollment invalid: {e}"))?;
    if verified.device_ed25519 != publisher_key {
        return Err("authority publisher_key does not match enrollment device key".to_string());
    }
    if enrollment.device_id != device_id {
        return Err("authority device_id does not match enrollment".to_string());
    }

    let revoked = match &r.revocation_cbor_hex {
        None => false,
        Some(hexs) => {
            let rev = decode_revocation(hexs)?;
            crate::enrollment_verify::verify_revocation_any_issuer(
                &rev,
                &enrollment,
                &signer_certs,
                rev.issued_at,
            )
            .map_err(|e| format!("authority revocation invalid: {e}"))?;
            if rev.target != device_id {
                return Err("authority revocation target does not match device_id".to_string());
            }
            true
        }
    };
    Ok(VerifiedAuthority {
        device_id,
        publisher_key,
        revoked,
    })
}

/// The pinned per-feed state a follower keeps. The binding is set once
/// (first-write-wins); `revoked` is monotonic-true.
#[derive(Debug, Clone)]
pub struct PinnedAuthority {
    pub device_id: [u8; 16],
    pub publisher_key: [u8; 32],
    pub n_identity_pub: String,
    pub revoked: bool,
    pub updated_at: u64,
}

/// Outcome of feeding one authority record into the cache.
#[derive(Debug, PartialEq)]
pub enum IngestOutcome {
    /// First valid record for this feed — binding pinned.
    Pinned,
    /// A verified revocation flipped `revoked` false → true.
    RevokedSet,
    /// Agreeing record with a newer clock — clock advanced, nothing else.
    BenignRefresh,
    /// Invalid, a rebinding attempt, or a stale/no-op record.
    Dropped(String),
}

/// In-memory `feed_id → PinnedAuthority` cache (§4 step 4). No disk, no engine
/// wiring — S2/S3 add those.
#[derive(Debug, Default)]
pub struct FeedAuthorityCache {
    feeds: std::collections::HashMap<String, PinnedAuthority>,
}

impl FeedAuthorityCache {
    pub fn get(&self, feed_id: &str) -> Option<&PinnedAuthority> {
        self.feeds.get(feed_id)
    }

    /// Verify and merge a record. The active binding is first-write-wins; a
    /// verified revocation sets `revoked` true forever (never cleared).
    pub fn ingest(&mut self, r: &FeedAuthorityRecord) -> IngestOutcome {
        let verified = match verify_authority(r) {
            Ok(v) => v,
            Err(e) => return IngestOutcome::Dropped(format!("invalid: {e}")),
        };
        match self.feeds.get_mut(&r.feed_id) {
            None => {
                self.feeds.insert(
                    r.feed_id.clone(),
                    PinnedAuthority {
                        device_id: verified.device_id,
                        publisher_key: verified.publisher_key,
                        n_identity_pub: r.n_identity_pub.clone(),
                        revoked: verified.revoked,
                        updated_at: r.updated_at,
                    },
                );
                IngestOutcome::Pinned
            }
            Some(pinned) => {
                // First-write-wins: the binding never changes.
                if pinned.device_id != verified.device_id
                    || pinned.publisher_key != verified.publisher_key
                {
                    return IngestOutcome::Dropped(
                        "binding mismatch (first-write-wins)".to_string(),
                    );
                }
                // Sticky revoked: a verified revocation flips it true, forever.
                if verified.revoked && !pinned.revoked {
                    pinned.revoked = true;
                    pinned.updated_at = pinned.updated_at.max(r.updated_at);
                    return IngestOutcome::RevokedSet;
                }
                // Benign refresh: advancing clock on an agreeing record. Never clears revoked.
                if r.updated_at > pinned.updated_at {
                    pinned.updated_at = r.updated_at;
                    IngestOutcome::BenignRefresh
                } else {
                    IngestOutcome::Dropped("stale (no clock advance)".to_string())
                }
            }
        }
    }
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

    // --- Task 3: verify_authority (chokepoint-backed) ---

    #[test]
    fn verify_authority_accepts_master_and_quorum() {
        let world = mint_quorum_world(0x94);
        let n = gen_identity();
        let m = record_for(&world, &world.a_cert, Vec::new(), None, WORLD_NOW, &n);
        let vm = verify_authority(&m).expect("master authority verifies");
        assert_eq!(vm.device_id, world.a_cert.device_id);
        assert!(!vm.revoked);

        let n2 = gen_identity();
        let q = record_for(
            &world,
            &world.c_quorum_cert,
            world.bundle.clone(),
            None,
            WORLD_NOW,
            &n2,
        );
        verify_authority(&q).expect("quorum authority verifies with bundle");
    }

    #[test]
    fn verify_authority_rejects_owner_mismatch() {
        let world = mint_quorum_world(0x98);
        let n = gen_identity();
        let mut rec = record_for(&world, &world.a_cert, Vec::new(), None, WORLD_NOW, &n);
        rec.owner_id = hex::encode([0xEEu8; 16]);
        sign_authority_binding(&n, &mut rec); // rebind valid; owner claim now foreign
        assert!(verify_authority(&rec).is_err());
    }

    #[test]
    fn verify_authority_rejects_publisher_key_and_device_id_mismatch() {
        let world = mint_quorum_world(0x9C);
        let n = gen_identity();
        let mut pk = record_for(&world, &world.a_cert, Vec::new(), None, WORLD_NOW, &n);
        pk.publisher_key = hex::encode([0x01u8; 32]);
        sign_authority_binding(&n, &mut pk);
        assert!(
            verify_authority(&pk).is_err(),
            "publisher_key mismatch rejected"
        );

        let n2 = gen_identity();
        let mut did = record_for(&world, &world.a_cert, Vec::new(), None, WORLD_NOW, &n2);
        did.device_id = hex::encode([0x02u8; 16]);
        sign_authority_binding(&n2, &mut did);
        assert!(
            verify_authority(&did).is_err(),
            "device_id mismatch rejected"
        );
    }

    #[test]
    fn verify_authority_rejects_expired_enrollment() {
        let world = mint_quorum_world(0xA0);
        let d_sk = ed25519_dalek::SigningKey::from_bytes(&[0xF0; 32]);
        let d_bundle = harmony_owner::pubkey_bundle::PubKeyBundle {
            classical: harmony_owner::pubkey_bundle::ClassicalKeys {
                ed25519_verify: d_sk.verifying_key().to_bytes(),
                x25519_pub: [0u8; 32],
            },
            post_quantum: None,
        };
        let d_id = d_bundle.identity_hash();
        let issued = crate::enrollment_verify::quorum_fixtures::SIGNER_ISSUED_AT;
        let expiring = EnrollmentCert::sign_master(
            &world.master_sk,
            world.master_bundle.clone(),
            d_id,
            d_bundle,
            issued,
            Some(issued + 50), // expires long before WORLD_NOW
        )
        .unwrap();
        let n = gen_identity();
        let rec = record_for(&world, &expiring, Vec::new(), None, WORLD_NOW, &n);
        assert!(
            verify_authority(&rec).is_err(),
            "expired enrollment rejected"
        );
    }

    #[test]
    fn verify_authority_accepts_revocation_and_rejects_target_mismatch() {
        let world = mint_quorum_world(0xA4);
        let n = gen_identity();
        let good_rev = mint_quorum_revocation(&world, world.a_cert.device_id, WORLD_NOW);
        let ok = record_for(
            &world,
            &world.a_cert,
            world.bundle.clone(),
            Some(good_rev),
            WORLD_NOW,
            &n,
        );
        let v = verify_authority(&ok).expect("valid revocation verifies");
        assert!(v.revoked, "revocation sets revoked");

        let n2 = gen_identity();
        let wrong_rev = mint_quorum_revocation(&world, world.c_quorum_cert.device_id, WORLD_NOW);
        let bad = record_for(
            &world,
            &world.a_cert,
            world.bundle.clone(),
            Some(wrong_rev),
            WORLD_NOW,
            &n2,
        );
        assert!(
            verify_authority(&bad).is_err(),
            "revocation targeting a different device rejected"
        );
    }

    // --- Task 4: FeedAuthorityCache ---

    #[test]
    fn cache_pins_binding_first_write_wins() {
        let world = mint_quorum_world(0xA8);
        let n = gen_identity();
        let mut cache = FeedAuthorityCache::default();
        let a = record_for(&world, &world.a_cert, Vec::new(), None, WORLD_NOW, &n);
        assert_eq!(cache.ingest(&a), IngestOutcome::Pinned);
        // Same feed (same #3 `n`) but a DIFFERENT device ⇒ dropped by first-write-wins.
        let b = record_for(&world, &world.b_cert, Vec::new(), None, WORLD_NOW + 1, &n);
        assert_eq!(b.feed_id, a.feed_id, "same #3 identity ⇒ same feed_id");
        assert!(matches!(cache.ingest(&b), IngestOutcome::Dropped(_)));
        assert_eq!(
            cache.get(&a.feed_id).unwrap().device_id,
            world.a_cert.device_id
        );
    }

    #[test]
    fn cache_revocation_is_sticky_and_rollback_proof() {
        let world = mint_quorum_world(0xAC);
        let n = gen_identity();
        let mut cache = FeedAuthorityCache::default();
        let active = record_for(
            &world,
            &world.a_cert,
            world.bundle.clone(),
            None,
            WORLD_NOW,
            &n,
        );
        assert_eq!(cache.ingest(&active), IngestOutcome::Pinned);
        assert!(!cache.get(&active.feed_id).unwrap().revoked);

        let rev = mint_quorum_revocation(&world, world.a_cert.device_id, WORLD_NOW);
        let revoked_rec = record_for(
            &world,
            &world.a_cert,
            world.bundle.clone(),
            Some(rev),
            WORLD_NOW + 10,
            &n,
        );
        assert_eq!(cache.ingest(&revoked_rec), IngestOutcome::RevokedSet);
        assert!(cache.get(&active.feed_id).unwrap().revoked);

        // A newer clean record must NOT clear it.
        let newer = record_for(
            &world,
            &world.a_cert,
            world.bundle.clone(),
            None,
            WORLD_NOW + 20,
            &n,
        );
        cache.ingest(&newer);
        assert!(
            cache.get(&active.feed_id).unwrap().revoked,
            "revoked stays sticky after a newer clean record"
        );

        // An older (rollback) clean record likewise cannot clear it.
        let older = record_for(
            &world,
            &world.a_cert,
            world.bundle.clone(),
            None,
            WORLD_NOW - 5,
            &n,
        );
        cache.ingest(&older);
        assert!(
            cache.get(&active.feed_id).unwrap().revoked,
            "rollback cannot un-revoke"
        );
    }

    #[test]
    fn cache_benign_refresh_advances_clock() {
        let world = mint_quorum_world(0xB0);
        let n = gen_identity();
        let mut cache = FeedAuthorityCache::default();
        let a = record_for(&world, &world.a_cert, Vec::new(), None, WORLD_NOW, &n);
        assert_eq!(cache.ingest(&a), IngestOutcome::Pinned);
        let refresh = record_for(&world, &world.a_cert, Vec::new(), None, WORLD_NOW + 100, &n);
        assert_eq!(cache.ingest(&refresh), IngestOutcome::BenignRefresh);
        assert_eq!(
            cache.get(&a.feed_id).unwrap().updated_at,
            (WORLD_NOW + 100) * 1000
        );
    }

    #[test]
    fn cache_drops_invalid_record() {
        let world = mint_quorum_world(0xB4);
        let n = gen_identity();
        let mut cache = FeedAuthorityCache::default();
        let mut bad = record_for(&world, &world.a_cert, Vec::new(), None, WORLD_NOW, &n);
        bad.n_sig = "00".repeat(64); // invalid signature
        assert!(matches!(cache.ingest(&bad), IngestOutcome::Dropped(_)));
        assert!(cache.get(&bad.feed_id).is_none());
    }
}
