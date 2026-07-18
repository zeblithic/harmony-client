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

/// CBOR-hex encode any serializable value (the shared encoder for the cert
/// blobs). Returns a recoverable error rather than panicking so the S2 publish
/// path can surface an encode failure instead of aborting.
fn encode_cbor_hex<T: serde::Serialize>(value: &T, what: &str) -> Result<String, String> {
    let mut buf = Vec::new();
    ciborium::into_writer(value, &mut buf)
        .map_err(|e| format!("authority {what} cbor-encode failed: {e}"))?;
    Ok(hex::encode(buf))
}

/// CBOR-hex encode a single `EnrollmentCert` for the `enrollment_cbor_hex` field.
pub fn encode_cert(cert: &EnrollmentCert) -> Result<String, String> {
    encode_cbor_hex(cert, "enrollment")
}

/// CBOR-hex encode a quorum signer bundle for `signer_certs_cbor_hex`
/// (empty ⇒ `""`, which serde omits).
pub fn encode_certs(certs: &[EnrollmentCert]) -> Result<String, String> {
    if certs.is_empty() {
        return Ok(String::new());
    }
    encode_cbor_hex(&certs, "signer_certs")
}

/// CBOR-hex encode a `RevocationCert` for `revocation_cbor_hex`.
pub fn encode_revocation(rev: &RevocationCert) -> Result<String, String> {
    encode_cbor_hex(rev, "revocation")
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

/// ZEB-678 S2: build a device's own *active* `FeedAuthorityRecord` for its
/// feed `N` (no revocation). `node_identity` is the feed's `#3` key (hashes to
/// `N` and produces `n_sig`); `sk` is the enrolled `#2` device key; `cert` is
/// this device's own enrollment. `signer_certs` is the quorum signer bundle a
/// quorum-issued `cert` needs to verify (`own_cert_bundle`); master-issued
/// self-publish passes an empty slice, which serde omits from the wire
/// (ZEB-682). Errors if the `#2` signing key does not match the enrolled
/// `publisher_key`, so a record that could not verify is never published.
pub fn build_active_authority(
    node_identity: &harmony_identity::PrivateIdentity,
    sk: &ed25519_dalek::SigningKey,
    cert: &EnrollmentCert,
    signer_certs: &[EnrollmentCert],
    updated_at_ms: u64,
) -> Result<FeedAuthorityRecord, String> {
    let publisher_key = sk.verifying_key().to_bytes();
    if cert.device_pubkeys.classical.ed25519_verify != publisher_key {
        return Err("enrolled publisher key does not match the #2 signing key".to_string());
    }
    let mut rec = FeedAuthorityRecord {
        feed_id: String::new(), // set by sign_authority_binding
        owner_id: hex::encode(cert.owner_id),
        device_id: hex::encode(cert.device_id),
        publisher_key: hex::encode(publisher_key),
        n_identity_pub: String::new(), // set by sign_authority_binding
        enrollment_cbor_hex: encode_cert(cert)?,
        signer_certs_cbor_hex: encode_certs(signer_certs)?,
        revocation_cbor_hex: None,
        updated_at: updated_at_ms,
        n_sig: String::new(), // set by sign_authority_binding
    };
    sign_authority_binding(node_identity, &mut rec);
    Ok(rec)
}

/// ZEB-678 S3: turn a device's stamped *active* `FeedAuthorityRecord` (its
/// fleet-net `feed_binding`) into a *revoked* one by appending `revocation` and
/// bumping the LWW clock. No re-signing: `n_sig` covers only the immutable
/// binding (§3.1, [`authority_binding_bytes`]), so the original signature stays
/// valid and every follower still accepts the record — now flagged revoked.
/// Returns `(feed_id, canonical_json)` ready to publish to
/// `harmony/vines/{feed_id}/authority`.
///
/// Rejects a `revocation` whose `target` is not the feed's `device_id`: such a
/// record is dropped by every follower at [`verify_authority`] step 3, so we
/// never emit it.
pub fn build_revoked_authority(
    active_binding_json: &str,
    revocation: &RevocationCert,
    now_ms: u64,
) -> Result<(String, String), String> {
    let mut rec: FeedAuthorityRecord = serde_json::from_str(active_binding_json)
        .map_err(|e| format!("feed_binding parse failed: {e}"))?;
    // Authenticate the binding BEFORE trusting `feed_id` (the publish topic) or
    // republishing. `feed_binding` is self-authored by the target device into its
    // fleet-net row, so a tampered/corrupt one must not steer the seed-holder to
    // publish to a bogus topic (or a record every follower drops) while we log a
    // successful cut-off. `verify_binding` proves `feed_id == hash(n_identity_pub)`
    // under a valid `n_sig`, so a device can only ever republish a binding for the
    // feed it actually owns. (ZEB-678 S3 review — Qodo/CodeRabbit.)
    verify_binding(&rec)?;
    let target_hex = hex::encode(revocation.target);
    if target_hex != rec.device_id {
        return Err(format!(
            "revocation target {target_hex} does not match feed device_id {}",
            rec.device_id
        ));
    }
    rec.revocation_cbor_hex = Some(encode_revocation(revocation)?);
    rec.updated_at = now_ms.max(rec.updated_at.saturating_add(1));
    let json = serde_json::to_string(&rec).map_err(|e| format!("serialize failed: {e}"))?;
    Ok((rec.feed_id.clone(), json))
}

/// Verify the `#3` binding: `n_identity_pub` hashes to `feed_id`, and `n_sig`
/// is a strict Ed25519 signature over the binding bytes. Mirrors
/// `vine_signing::verify_signed`. Crate-private — it checks ONLY the binding,
/// not enrollment/revocation; `verify_authority` is the full public check.
pub(crate) fn verify_binding(r: &FeedAuthorityRecord) -> Result<(), String> {
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

/// Upper bound on a single CBOR-hex cert blob. `verify_authority` decodes
/// attacker-controlled strings once this is wired to peer-writable ingest
/// (S2), so cap the input before allocating/parsing to bound hostile work. A
/// cert is a few hundred bytes and a signer bundle a few KiB — 64 KiB is
/// generous and still bounds abuse.
const MAX_AUTHORITY_CBOR_BYTES: usize = 64 * 1024;

/// Hex-decode with a size cap checked BEFORE `hex::decode` allocates.
fn bounded_hex_decode(hexs: &str, what: &str) -> Result<Vec<u8>, String> {
    if hexs.len() > MAX_AUTHORITY_CBOR_BYTES * 2 {
        return Err(format!(
            "authority {what} cbor too large ({} hex chars > {} cap)",
            hexs.len(),
            MAX_AUTHORITY_CBOR_BYTES * 2
        ));
    }
    hex::decode(hexs).map_err(|e| format!("authority {what} not hex: {e}"))
}

pub(crate) fn decode_cert(hexs: &str) -> Result<EnrollmentCert, String> {
    let bytes = bounded_hex_decode(hexs, "enrollment")?;
    let mut cur = std::io::Cursor::new(&bytes);
    let cert = ciborium::from_reader(&mut cur)
        .map_err(|_| "authority enrollment cbor invalid".to_string())?;
    if cur.position() as usize != bytes.len() {
        return Err("authority enrollment cbor has trailing bytes".to_string());
    }
    Ok(cert)
}

pub(crate) fn decode_certs(hexs: &str) -> Result<Vec<EnrollmentCert>, String> {
    if hexs.is_empty() {
        return Ok(Vec::new());
    }
    let bytes = bounded_hex_decode(hexs, "signer_certs")?;
    let mut cur = std::io::Cursor::new(&bytes);
    let certs = ciborium::from_reader(&mut cur)
        .map_err(|_| "authority signer_certs cbor invalid".to_string())?;
    if cur.position() as usize != bytes.len() {
        return Err("authority signer_certs cbor has trailing bytes".to_string());
    }
    Ok(certs)
}

fn decode_revocation(hexs: &str) -> Result<RevocationCert, String> {
    let bytes = bounded_hex_decode(hexs, "revocation")?;
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
///
/// `now_secs` is the **verifier-controlled** wall clock (Unix seconds) supplied
/// by the ingest boundary — NOT `r.updated_at`, which is unauthenticated (it is
/// excluded from `n_sig`), so deriving the enrollment-validity clock from it
/// would let a peer backdate `updated_at` to revive an expired/backdated cert.
/// The revocation check keeps `rev.issued_at`, which IS authenticated inside
/// the `RevocationCert`.
pub fn verify_authority(
    r: &FeedAuthorityRecord,
    now_secs: u64,
) -> Result<VerifiedAuthority, String> {
    verify_binding(r)?;
    let owner_id = decode_hex16(&r.owner_id, "owner_id")?;
    let device_id = decode_hex16(&r.device_id, "device_id")?;
    let publisher_key = decode_hex32(&r.publisher_key, "publisher_key")?;
    let enrollment = decode_cert(&r.enrollment_cbor_hex)?;
    let signer_certs = decode_certs(&r.signer_certs_cbor_hex)?;

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
///
/// ZEB-683 investigated an authenticated "supersession" path for a same-device
/// `#2` rotation and found it UNREPRESENTABLE: `EnrollmentCert::verify`
/// enforces `device_id == hash(device_pubkeys)`, so a record pairing the
/// pinned `device_id` with a different `#2` key can never pass
/// [`verify_authority`]. A `#2` key change therefore mints a NEW device
/// identity — which is device replacement, and a replaced device's feed does
/// not survive by design (spec §2; feed continuity across devices is the §11
/// "canonical owner feed" follow-up). What CAN change for the same device is
/// cert metadata only (renewal `issued_at`/`expires_at`, Master↔Quorum
/// re-issue over the same keys) — same binding, so it lands as
/// [`IngestOutcome::BenignRefresh`] and each record re-verifies its own
/// embedded cert. First-write-wins stays absolute.
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
    /// verified revocation sets `revoked` true forever (never cleared). There
    /// is deliberately NO repin path — see the [`PinnedAuthority`] doc for the
    /// ZEB-683 finding (a same-device `#2` change is unrepresentable; cert
    /// renewals keep the binding and land as `BenignRefresh`).
    ///
    /// `now_secs` is the verifier-controlled wall clock forwarded to
    /// [`verify_authority`] for the enrollment-validity check — never derived
    /// from the record's unauthenticated `updated_at`.
    pub fn ingest(&mut self, r: &FeedAuthorityRecord, now_secs: u64) -> IngestOutcome {
        let verified = match verify_authority(r, now_secs) {
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
            enrollment_cbor_hex: encode_cert(cert).expect("encode enrollment"),
            signer_certs_cbor_hex: encode_certs(&signer_certs).expect("encode signer certs"),
            revocation_cbor_hex: revocation
                .as_ref()
                .map(|r| encode_revocation(r).expect("encode revocation")),
            updated_at: updated_at_secs * 1000,
            n_sig: String::new(),
        };
        sign_authority_binding(n, &mut rec);
        rec
    }
    use crate::enrollment_verify::quorum_fixtures::{
        mint_quorum_revocation, mint_quorum_world, SIGNER_ISSUED_AT, WORLD_NOW,
    };

    #[test]
    fn build_revoked_authority_appends_cert_and_still_verifies_as_revoked() {
        use harmony_owner::certs::RevocationReason;
        let world = mint_quorum_world(0xA0);
        let n = gen_identity();
        // The stamped active feed_binding: master-issued (empty bundle), no revocation.
        let active = record_for(&world, &world.a_cert, Vec::new(), None, WORLD_NOW, &n);
        let active_json = serde_json::to_string(&active).unwrap();
        let expected_feed = active.feed_id.clone();

        let rev = RevocationCert::sign_master(
            &world.master_sk,
            world.master_bundle.clone(),
            world.a_cert.device_id,
            WORLD_NOW,
            RevocationReason::Lost,
        )
        .unwrap();
        let now_ms = (WORLD_NOW + 5) * 1000;

        let (feed_id, json) = build_revoked_authority(&active_json, &rev, now_ms).unwrap();
        assert_eq!(feed_id, expected_feed);

        let parsed: FeedAuthorityRecord = serde_json::from_str(&json).unwrap();
        assert!(parsed.revocation_cbor_hex.is_some(), "revocation appended");
        assert!(parsed.updated_at >= now_ms, "updated_at bumped forward");
        // n_sig untouched → the binding still verifies, now flagged revoked.
        let v = verify_authority(&parsed, WORLD_NOW).expect("revoked authority verifies");
        assert!(v.revoked, "device must be marked revoked");
        assert_eq!(v.device_id, world.a_cert.device_id);
    }

    #[test]
    fn build_revoked_authority_rejects_target_that_is_not_the_feed_device() {
        use harmony_owner::certs::RevocationReason;
        let world = mint_quorum_world(0xA1);
        let n = gen_identity();
        let active = record_for(&world, &world.a_cert, Vec::new(), None, WORLD_NOW, &n);
        let active_json = serde_json::to_string(&active).unwrap();
        // Revocation targets a DIFFERENT device (C), not the feed's device (A).
        let rev = RevocationCert::sign_master(
            &world.master_sk,
            world.master_bundle.clone(),
            world.c_quorum_cert.device_id,
            WORLD_NOW,
            RevocationReason::Lost,
        )
        .unwrap();
        let err = build_revoked_authority(&active_json, &rev, WORLD_NOW * 1000).unwrap_err();
        assert!(err.contains("target"), "target mismatch is rejected: {err}");
    }

    #[test]
    fn build_revoked_authority_rejects_unparseable_binding() {
        use harmony_owner::certs::RevocationReason;
        let world = mint_quorum_world(0xA2);
        let rev = RevocationCert::sign_master(
            &world.master_sk,
            world.master_bundle.clone(),
            world.a_cert.device_id,
            WORLD_NOW,
            RevocationReason::Lost,
        )
        .unwrap();
        assert!(build_revoked_authority("not json", &rev, 1_000).is_err());
    }

    #[test]
    fn build_revoked_authority_rejects_tampered_binding() {
        use harmony_owner::certs::RevocationReason;
        let world = mint_quorum_world(0xA3);
        let n = gen_identity();
        // A validly-signed binding whose feed_id is then tampered so it no longer
        // hashes from n_identity_pub — a corrupt/hostile self-stamped feed_binding.
        let mut active = record_for(&world, &world.a_cert, Vec::new(), None, WORLD_NOW, &n);
        active.feed_id = "deadbeef".repeat(8);
        let active_json = serde_json::to_string(&active).unwrap();
        let rev = RevocationCert::sign_master(
            &world.master_sk,
            world.master_bundle.clone(),
            world.a_cert.device_id,
            WORLD_NOW,
            RevocationReason::Lost,
        )
        .unwrap();
        // Must reject on binding verification, before it would publish to the
        // tampered feed_id topic.
        assert!(build_revoked_authority(&active_json, &rev, WORLD_NOW * 1000).is_err());
    }

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
            enrollment_cbor_hex: encode_cert(&world.a_cert).expect("encode enrollment"),
            signer_certs_cbor_hex: encode_certs(&signer_certs).expect("encode signer certs"),
            revocation_cbor_hex: revocation
                .as_ref()
                .map(|r| encode_revocation(r).expect("encode revocation")),
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
        let vm = verify_authority(&m, WORLD_NOW).expect("master authority verifies");
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
        verify_authority(&q, WORLD_NOW).expect("quorum authority verifies with bundle");
    }

    #[test]
    fn verify_authority_rejects_owner_mismatch() {
        let world = mint_quorum_world(0x98);
        let n = gen_identity();
        let mut rec = record_for(&world, &world.a_cert, Vec::new(), None, WORLD_NOW, &n);
        rec.owner_id = hex::encode([0xEEu8; 16]);
        sign_authority_binding(&n, &mut rec); // rebind valid; owner claim now foreign
        assert!(verify_authority(&rec, WORLD_NOW).is_err());
    }

    #[test]
    fn verify_authority_rejects_publisher_key_and_device_id_mismatch() {
        let world = mint_quorum_world(0x9C);
        let n = gen_identity();
        let mut pk = record_for(&world, &world.a_cert, Vec::new(), None, WORLD_NOW, &n);
        pk.publisher_key = hex::encode([0x01u8; 32]);
        sign_authority_binding(&n, &mut pk);
        assert!(
            verify_authority(&pk, WORLD_NOW).is_err(),
            "publisher_key mismatch rejected"
        );

        let n2 = gen_identity();
        let mut did = record_for(&world, &world.a_cert, Vec::new(), None, WORLD_NOW, &n2);
        did.device_id = hex::encode([0x02u8; 16]);
        sign_authority_binding(&n2, &mut did);
        assert!(
            verify_authority(&did, WORLD_NOW).is_err(),
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
            verify_authority(&rec, WORLD_NOW).is_err(),
            "expired enrollment rejected"
        );
    }

    #[test]
    fn verify_authority_uses_verifier_clock_not_updated_at() {
        // Regression (Qodo #1): `updated_at` is excluded from `n_sig`, so a peer
        // can set it freely. It must NOT drive the enrollment-validity clock —
        // otherwise a backdated `updated_at` inside an expired cert's old window
        // would revive it. The same record is accepted at a clock inside the
        // window and rejected at the real now, proving the clock is the
        // verifier's parameter, not the record's field.
        let world = mint_quorum_world(0xB8);
        let d_sk = ed25519_dalek::SigningKey::from_bytes(&[0xF1; 32]);
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
        // Backdate `updated_at` to sit inside the (now-expired) validity window.
        let rec = record_for(&world, &expiring, Vec::new(), None, issued + 10, &n);

        // Positive control: valid when the verifier's own clock is in-window.
        verify_authority(&rec, issued + 10).expect("valid when verifier clock is in-window");
        // Security: rejected at the real now, regardless of backdated updated_at.
        assert!(
            verify_authority(&rec, WORLD_NOW).is_err(),
            "backdated updated_at must not revive an expired enrollment"
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
        let v = verify_authority(&ok, WORLD_NOW).expect("valid revocation verifies");
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
            verify_authority(&bad, WORLD_NOW).is_err(),
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
        assert_eq!(cache.ingest(&a, WORLD_NOW), IngestOutcome::Pinned);
        // Same feed (same #3 `n`) but a DIFFERENT device ⇒ dropped by first-write-wins.
        let b = record_for(&world, &world.b_cert, Vec::new(), None, WORLD_NOW + 1, &n);
        assert_eq!(b.feed_id, a.feed_id, "same #3 identity ⇒ same feed_id");
        assert!(matches!(
            cache.ingest(&b, WORLD_NOW),
            IngestOutcome::Dropped(_)
        ));
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
        assert_eq!(cache.ingest(&active, WORLD_NOW), IngestOutcome::Pinned);
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
        assert_eq!(
            cache.ingest(&revoked_rec, WORLD_NOW),
            IngestOutcome::RevokedSet
        );
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
        cache.ingest(&newer, WORLD_NOW);
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
        cache.ingest(&older, WORLD_NOW);
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
        assert_eq!(cache.ingest(&a, WORLD_NOW), IngestOutcome::Pinned);
        let refresh = record_for(&world, &world.a_cert, Vec::new(), None, WORLD_NOW + 100, &n);
        assert_eq!(
            cache.ingest(&refresh, WORLD_NOW),
            IngestOutcome::BenignRefresh
        );
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
        assert!(matches!(
            cache.ingest(&bad, WORLD_NOW),
            IngestOutcome::Dropped(_)
        ));
        assert!(cache.get(&bad.feed_id).is_none());
    }

    #[test]
    fn build_active_authority_produces_verifiable_record() {
        let world = mint_quorum_world(0xD2);
        let n = gen_identity();
        // world.a_sk is the enrolled #2 key matching world.a_cert (master-issued).
        let rec = build_active_authority(&n, &world.a_sk, &world.a_cert, &[], WORLD_NOW * 1000)
            .expect("builds");
        assert_eq!(rec.feed_id, hex::encode(n.public_identity().address_hash));
        assert!(
            rec.revocation_cbor_hex.is_none(),
            "active binding, no revocation"
        );
        assert!(
            rec.signer_certs_cbor_hex.is_empty(),
            "master-issued: empty bundle"
        );
        let v = verify_authority(&rec, WORLD_NOW).expect("self-built record verifies");
        assert!(!v.revoked);
        assert_eq!(v.publisher_key, world.a_sk.verifying_key().to_bytes());

        // A #2 signing key that is not the enrolled key is rejected at build.
        let wrong = ed25519_dalek::SigningKey::from_bytes(&[0xEE; 32]);
        assert!(
            build_active_authority(&n, &wrong, &world.a_cert, &[], WORLD_NOW * 1000).is_err(),
            "mismatched publisher key rejected"
        );
    }

    // ── ZEB-682: quorum self-publish ─────────────────────────────────────

    #[test]
    fn build_active_authority_quorum_bundle_verifies_zeb682() {
        let world = mint_quorum_world(0xC0);
        let n = gen_identity();
        // With the signer bundle threaded, a quorum-issued device's own record
        // verifies end-to-end — this is the ZEB-682 migration gap.
        let rec = build_active_authority(
            &n,
            &world.c_sk,
            &world.c_quorum_cert,
            &world.bundle,
            WORLD_NOW * 1000,
        )
        .expect("quorum self-publish builds");
        assert!(!rec.signer_certs_cbor_hex.is_empty(), "bundle on the wire");
        let v = verify_authority(&rec, WORLD_NOW).expect("quorum record verifies");
        assert_eq!(v.publisher_key, world.c_sk.verifying_key().to_bytes());

        // The pre-ZEB-682 shape — quorum cert, empty bundle — builds but can
        // NEVER verify; the publish path's self-check keeps it off the wire.
        let bare =
            build_active_authority(&n, &world.c_sk, &world.c_quorum_cert, &[], WORLD_NOW * 1000)
                .expect("builds without bundle");
        assert!(
            verify_authority(&bare, WORLD_NOW).is_err(),
            "quorum cert with empty bundle must not verify"
        );
    }

    // ── ZEB-683: rotation/renewal cache semantics ────────────────────────
    //
    // ZEB-683 investigated an authenticated repin ("supersession") for a
    // same-device #2 rotation. Finding: `EnrollmentCert::verify` enforces
    // `device_id == hash(device_pubkeys)`, so "same device_id, new #2 key"
    // is unrepresentable — a key change is a NEW device (= replacement, new
    // feed per spec §2). The two tests below pin both halves: the key-change
    // record can never verify, and a cert RENEWAL (same keys, newer
    // issued_at) keeps the binding and lands as a benign refresh.

    /// Master-sign a cert pairing `device_id` with a DIFFERENT `#2` key —
    /// structurally forgeable at sign time, but `verify` rejects it
    /// (`device_id != hash(device_pubkeys)`).
    fn same_device_new_key_cert(
        world: &crate::enrollment_verify::quorum_fixtures::QuorumWorld,
        device_id: [u8; 16],
        fill: u8,
        issued_at: u64,
    ) -> EnrollmentCert {
        use harmony_owner::pubkey_bundle::{ClassicalKeys, PubKeyBundle};
        let sk = ed25519_dalek::SigningKey::from_bytes(&[fill; 32]);
        let bundle = PubKeyBundle {
            classical: ClassicalKeys {
                ed25519_verify: sk.verifying_key().to_bytes(),
                x25519_pub: [0u8; 32],
            },
            post_quantum: None,
        };
        EnrollmentCert::sign_master(
            &world.master_sk,
            world.master_bundle.clone(),
            device_id,
            bundle,
            issued_at,
            None,
        )
        .expect("sign_master does not enforce the hash; verify does")
    }

    #[test]
    fn cache_rejects_same_device_key_change_record_zeb683() {
        let world = mint_quorum_world(0xC4);
        let n = gen_identity();
        let mut cache = FeedAuthorityCache::default();
        let old = record_for(&world, &world.a_cert, Vec::new(), None, WORLD_NOW, &n);
        assert_eq!(cache.ingest(&old, WORLD_NOW), IngestOutcome::Pinned);

        // A record pairing the pinned device_id with a fresh #2 key under a
        // strictly newer master-issued cert: verification kills it BEFORE any
        // binding comparison — the repin path ZEB-683 asked for cannot exist.
        let cert =
            same_device_new_key_cert(&world, world.a_cert.device_id, 0x20, SIGNER_ISSUED_AT + 100);
        let rebind = record_for(&world, &cert, Vec::new(), None, WORLD_NOW + 5, &n);
        assert!(matches!(
            cache.ingest(&rebind, WORLD_NOW),
            IngestOutcome::Dropped(ref m) if m.contains("invalid")
        ));
        let pin = cache.get(&old.feed_id).expect("pin untouched");
        assert_eq!(
            pin.publisher_key,
            world.a_cert.device_pubkeys.classical.ed25519_verify
        );
    }

    #[test]
    fn cache_cert_renewal_same_binding_is_benign_refresh_zeb683() {
        let world = mint_quorum_world(0xC8);
        let n = gen_identity();
        let mut cache = FeedAuthorityCache::default();
        let old = record_for(&world, &world.a_cert, Vec::new(), None, WORLD_NOW, &n);
        assert_eq!(cache.ingest(&old, WORLD_NOW), IngestOutcome::Pinned);

        // Renewal: same keys (same device_id), newer issued_at. The binding is
        // unchanged, so the record verifies and refreshes the clock — this is
        // how a long-lived feed keeps a fresh embedded cert on the wire.
        let renewed = EnrollmentCert::sign_master(
            &world.master_sk,
            world.master_bundle.clone(),
            world.a_cert.device_id,
            world.a_cert.device_pubkeys.clone(),
            SIGNER_ISSUED_AT + 100,
            None,
        )
        .expect("renewed cert");
        let refresh = record_for(&world, &renewed, Vec::new(), None, WORLD_NOW + 5, &n);
        assert_eq!(
            cache.ingest(&refresh, WORLD_NOW),
            IngestOutcome::BenignRefresh
        );
        let pin = cache.get(&old.feed_id).expect("pinned");
        assert_eq!(
            pin.publisher_key, world.a_cert.device_pubkeys.classical.ed25519_verify,
            "binding unchanged"
        );
        assert_eq!(pin.updated_at, (WORLD_NOW + 5) * 1000, "clock advanced");
    }
}
