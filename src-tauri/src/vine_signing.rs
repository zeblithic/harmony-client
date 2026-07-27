//! ZEB-673: creator/reactor signing for vine wire records.
//!
//! Extends the ZEB-670 tombstone scheme (`vine_tombstone.rs`) to the two
//! remaining vine wire records: `VineDescriptorPayload` (signed by the
//! creator) and `VineReactionPayload` (signed by the reactor). Same
//! identity binding as `community_membership::verify_signature`: the
//! record carries the signer's 64-byte identity pub (X25519(32) ||
//! Ed25519(32)); receivers require
//! `hex(Identity::from_public_bytes(pub).address_hash)` to equal the
//! claimed address, then `verify_strict` over the canonical bytes.
//!
//! Canonical bytes are LENGTH-PREFIXED, not `|`-separated like the
//! tombstone's: descriptors carry free text (`title`, `creator_name`,
//! `original_creator_name`) where a separator could be injected. Each
//! field is written in a fixed order as `u32-LE byte-length ‖ bytes`
//! (`Option` as a presence byte then the value, `u64` as 8-byte LE,
//! `bool` as one byte), so the encoding is injective by construction.
//! `owner_state_crypto::canonical_cbor_encode` was rejected because its
//! documented contract requires all field names of a struct to share one
//! encoded length — these payloads violate it wholesale.
//!
//! Migration posture (see ZEB-673 design comment): signatures exist
//! only on the wire. Disk rows (`DescriptorOnDisk`/`ReactionOnDisk`)
//! never retain them — verification happens once at ingest, the same
//! posture as `TombstoneOnDisk` — so records rebuilt from disk (and
//! pre-ZEB-673 wire records) carry `None` in the `Option` signature
//! fields. WIRE arrivals without a valid signature are rejected at
//! cache admission (strict on wire); legacy cached rows age out via the
//! existing 90-day / 5000-descriptor bounds.

//! ZEB-671 extends the same scheme to `VineFollowListPayload` (signed by
//! the list OWNER, published on `harmony/vines/{owner}/follows`). Unlike
//! descriptors/reactions there is no unsigned legacy for follow lists —
//! the record type is born strict.

use crate::{VineDescriptorPayload, VineFollowListPayload, VineReactionPayload};
// ZEB-678 S2: `sk.sign(..)` on the raw enrolled `#2` key comes from the
// `ed25519_dalek::Signer` trait (the `#3` path signs via `PrivateIdentity`).
use ed25519_dalek::Signer as _;

/// Domain-separation prefix + version for descriptor canonical bytes.
const DESCRIPTOR_DOMAIN: &str = "harmony-vine-descriptor-v1";
/// Domain-separation prefix + version for reaction canonical bytes.
const REACTION_DOMAIN: &str = "harmony-vine-reaction-v1";
/// Domain-separation prefix + version for follow-list canonical bytes.
const FOLLOW_LIST_DOMAIN: &str = "harmony-vine-follows-v1";

pub(crate) fn push_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

/// `None` and `Some("")` must encode differently: a presence byte
/// precedes the value, so `None` = `[0]` and `Some("")` = `[1, 0,0,0,0]`.
fn push_opt_str(out: &mut Vec<u8>, s: &Option<String>) {
    match s {
        None => out.push(0),
        Some(v) => {
            out.push(1);
            push_str(out, v);
        }
    }
}

pub(crate) fn push_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn push_bool(out: &mut Vec<u8>, v: bool) {
    out.push(u8::from(v));
}

/// Deterministic byte string a descriptor signature covers. Fixed field
/// order; covers every SEMANTIC field and never `identity_pub`/`sig`.
pub fn descriptor_canonical_bytes(d: &VineDescriptorPayload) -> Vec<u8> {
    let mut out = Vec::with_capacity(256);
    push_str(&mut out, DESCRIPTOR_DOMAIN);
    push_str(&mut out, &d.id);
    push_str(&mut out, &d.creator_address);
    push_str(&mut out, &d.creator_name);
    push_u64(&mut out, d.created_at);
    push_str(&mut out, &d.video_cid);
    push_opt_str(&mut out, &d.title);
    push_opt_str(&mut out, &d.reshare_of);
    push_opt_str(&mut out, &d.original_creator_address);
    push_opt_str(&mut out, &d.original_creator_name);
    out
}

/// Deterministic byte string a reaction signature covers.
pub fn reaction_canonical_bytes(r: &VineReactionPayload) -> Vec<u8> {
    let mut out = Vec::with_capacity(128);
    push_str(&mut out, REACTION_DOMAIN);
    push_str(&mut out, &r.vine_id);
    push_str(&mut out, &r.reactor_address);
    push_str(&mut out, &r.reactor_name);
    push_bool(&mut out, r.liked);
    push_u64(&mut out, r.timestamp);
    out
}

/// Deterministic byte string a follow-list signature covers: domain ‖
/// owner ‖ updated_at ‖ u32-LE entry count ‖ each address
/// length-prefixed. The count prefix pins the list boundary the same way
/// per-field length prefixes pin string boundaries (`["ab"]` vs
/// `["a","b"]` cannot collide).
pub fn follow_list_canonical_bytes(p: &VineFollowListPayload) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 + p.follows.len() * 40);
    push_str(&mut out, FOLLOW_LIST_DOMAIN);
    push_str(&mut out, &p.owner_address);
    push_u64(&mut out, p.updated_at);
    out.extend_from_slice(&(p.follows.len() as u32).to_le_bytes());
    for addr in &p.follows {
        push_str(&mut out, addr);
    }
    out
}

/// The address a signing identity derives — the value receivers bind
/// signatures against. Publish paths compare this to the address they
/// are about to embed and refuse to sign on divergence (Greptile PR
/// #446: a mismatch would "succeed" locally while every receiver
/// rejects the record).
pub fn signer_address(private: &harmony_identity::PrivateIdentity) -> String {
    hex::encode(private.public_identity().address_hash)
}

/// Sign a descriptor in place with the local owner identity, setting
/// `identity_pub` + `sig`. The caller must have set `creator_address`
/// to `signer_address(private)` — receivers reject mismatches in
/// `verify_descriptor`.
pub fn sign_descriptor(private: &harmony_identity::PrivateIdentity, d: &mut VineDescriptorPayload) {
    let bytes = descriptor_canonical_bytes(d);
    d.sig = Some(hex::encode(private.sign(&bytes)));
    d.identity_pub = Some(hex::encode(private.public_identity().to_public_bytes()));
}

/// Sign a reaction in place; `reactor_address` must match `private`.
pub fn sign_reaction(private: &harmony_identity::PrivateIdentity, r: &mut VineReactionPayload) {
    let bytes = reaction_canonical_bytes(r);
    r.sig = Some(hex::encode(private.sign(&bytes)));
    r.identity_pub = Some(hex::encode(private.public_identity().to_public_bytes()));
}

/// Sign a follow list in place; `owner_address` must match `private`.
pub fn sign_follow_list(
    private: &harmony_identity::PrivateIdentity,
    p: &mut VineFollowListPayload,
) {
    let bytes = follow_list_canonical_bytes(p);
    p.sig = Some(hex::encode(private.sign(&bytes)));
    p.identity_pub = Some(hex::encode(private.public_identity().to_public_bytes()));
}

/// The address a hex-encoded 64-byte identity pub derives (`SHA256(pub)[:16]`,
/// hex-encoded). Extracted from `verify_signed`'s pubkey→address binding
/// (ZEB-811 Task 2) so `pkarr_vines::verify_vines_record` can reuse the exact
/// same derivation instead of duplicating it.
pub(crate) fn address_for_identity_pub_hex(identity_pub_hex: &str) -> Result<String, String> {
    let pub_vec =
        hex::decode(identity_pub_hex).map_err(|e| format!("identity pub is not hex: {e}"))?;
    let identity = harmony_identity::Identity::from_public_bytes(&pub_vec)
        .map_err(|_| "identity pub invalid".to_string())?;
    Ok(hex::encode(identity.address_hash))
}

/// Shared verification core: pubkey→address binding, then strict
/// Ed25519 over the canonical bytes (`verify_strict` rejects
/// non-canonical S values / small-order R points — RFC 8032 strict
/// subset, mirroring `vine_tombstone::verify_tombstone`).
pub(crate) fn verify_signed(
    identity_pub: Option<&str>,
    sig: Option<&str>,
    claimed_address: &str,
    canonical: &[u8],
    what: &str,
) -> Result<(), String> {
    let identity_pub = identity_pub.ok_or_else(|| format!("{what} is unsigned"))?;
    let sig = sig.ok_or_else(|| format!("{what} is unsigned"))?;
    let pub_vec =
        hex::decode(identity_pub).map_err(|e| format!("{what} identity pub is not hex: {e}"))?;
    let identity = harmony_identity::Identity::from_public_bytes(&pub_vec)
        .map_err(|_| format!("{what} identity pub invalid"))?;
    if address_for_identity_pub_hex(identity_pub).as_deref() != Ok(claimed_address) {
        return Err(format!("{what} pubkey does not match claimed address"));
    }
    let sig_bytes: [u8; 64] = hex::decode(sig)
        .map_err(|e| format!("{what} sig is not hex: {e}"))?
        .try_into()
        .map_err(|_| format!("{what} sig must be 64 bytes"))?;
    let sig = ed25519_dalek::Signature::from_bytes(&sig_bytes);
    identity
        .verifying_key
        .verify_strict(canonical, &sig)
        .map_err(|_| format!("{what} signature invalid"))
}

/// Verify a received descriptor: signer's pubkey must derive the
/// payload's `creator_address`, signature must cover the canonical
/// bytes. `Err` for unsigned (legacy wire) records — strict on wire.
pub fn verify_descriptor(d: &VineDescriptorPayload) -> Result<(), String> {
    verify_signed(
        d.identity_pub.as_deref(),
        d.sig.as_deref(),
        &d.creator_address,
        &descriptor_canonical_bytes(d),
        "descriptor",
    )
}

/// Verify a received reaction: the signer is the REACTOR (the payload's
/// `reactor_address`), not the vine creator whose topic carries it.
pub fn verify_reaction(r: &VineReactionPayload) -> Result<(), String> {
    verify_signed(
        r.identity_pub.as_deref(),
        r.sig.as_deref(),
        &r.reactor_address,
        &reaction_canonical_bytes(r),
        "reaction",
    )
}

/// Verify a received follow list: the signer is the list OWNER (the
/// payload's `owner_address` — receivers additionally bind it to the
/// topic's owner segment at cache admission).
pub fn verify_follow_list(p: &VineFollowListPayload) -> Result<(), String> {
    verify_signed(
        p.identity_pub.as_deref(),
        p.sig.as_deref(),
        &p.owner_address,
        &follow_list_canonical_bytes(p),
        "follow list",
    )
}

// ── ZEB-678 S2: enrolled `#2` device-key signing (`-v2` domain) ──────────
//
// The migration re-signs the SAME canonical field set as the `#3` builders
// under bumped `-v2` domain constants, producing a `device_sig` that
// receivers verify against the feed's authority `publisher_key`. The domain
// bump gives clean protocol separation from `#3`-signed bytes;
// `creator_address`/`owner_address` is already inside the signed bytes, so a
// `device_sig` cannot be replayed onto another feed.

/// `-v2` domain: descriptor signed by the enrolled `#2` device key.
const DESCRIPTOR_DOMAIN_V2: &str = "harmony-vine-descriptor-v2";
/// `-v2` domain: reaction signed by the enrolled `#2` device key.
const REACTION_DOMAIN_V2: &str = "harmony-vine-reaction-v2";
/// `-v2` domain: follow list signed by the enrolled `#2` device key.
const FOLLOW_LIST_DOMAIN_V2: &str = "harmony-vine-follows-v2";

/// Same field set as [`descriptor_canonical_bytes`], under the `-v2` domain.
pub fn descriptor_canonical_bytes_v2(d: &VineDescriptorPayload) -> Vec<u8> {
    let mut out = Vec::with_capacity(256);
    push_str(&mut out, DESCRIPTOR_DOMAIN_V2);
    push_str(&mut out, &d.id);
    push_str(&mut out, &d.creator_address);
    push_str(&mut out, &d.creator_name);
    push_u64(&mut out, d.created_at);
    push_str(&mut out, &d.video_cid);
    push_opt_str(&mut out, &d.title);
    push_opt_str(&mut out, &d.reshare_of);
    push_opt_str(&mut out, &d.original_creator_address);
    push_opt_str(&mut out, &d.original_creator_name);
    out
}

/// Same field set as [`reaction_canonical_bytes`], under the `-v2` domain.
pub fn reaction_canonical_bytes_v2(r: &VineReactionPayload) -> Vec<u8> {
    let mut out = Vec::with_capacity(128);
    push_str(&mut out, REACTION_DOMAIN_V2);
    push_str(&mut out, &r.vine_id);
    push_str(&mut out, &r.reactor_address);
    push_str(&mut out, &r.reactor_name);
    push_bool(&mut out, r.liked);
    push_u64(&mut out, r.timestamp);
    out
}

/// Same field set as [`follow_list_canonical_bytes`], under the `-v2` domain.
pub fn follow_list_canonical_bytes_v2(p: &VineFollowListPayload) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 + p.follows.len() * 40);
    push_str(&mut out, FOLLOW_LIST_DOMAIN_V2);
    push_str(&mut out, &p.owner_address);
    push_u64(&mut out, p.updated_at);
    out.extend_from_slice(&(p.follows.len() as u32).to_le_bytes());
    for addr in &p.follows {
        push_str(&mut out, addr);
    }
    out
}

/// Sign a descriptor with the enrolled `#2` device key, setting `device_sig`.
/// The legacy `#3` `identity_pub`/`sig` are left untouched — a migrated record
/// leaves them `None`.
pub fn sign_descriptor_v2(sk: &ed25519_dalek::SigningKey, d: &mut VineDescriptorPayload) {
    let bytes = descriptor_canonical_bytes_v2(d);
    d.device_sig = Some(hex::encode(sk.sign(&bytes).to_bytes()));
}

/// Sign a reaction with the enrolled `#2` device key, setting `device_sig`.
/// The caller sets the owner-anchoring fields (`owner_id`/`enrollment_cbor_hex`
/// /`signer_certs_cbor_hex`) so the reaction self-verifies cross-actor.
pub fn sign_reaction_v2(sk: &ed25519_dalek::SigningKey, r: &mut VineReactionPayload) {
    let bytes = reaction_canonical_bytes_v2(r);
    r.device_sig = Some(hex::encode(sk.sign(&bytes).to_bytes()));
}

/// Sign a follow list with the enrolled `#2` device key, setting `device_sig`.
pub fn sign_follow_list_v2(sk: &ed25519_dalek::SigningKey, p: &mut VineFollowListPayload) {
    let bytes = follow_list_canonical_bytes_v2(p);
    p.device_sig = Some(hex::encode(sk.sign(&bytes).to_bytes()));
}

/// Shared `#2` verification core: a hex `device_sig` checked with
/// `verify_strict` against a feed's authority `publisher_key` (RFC 8032 strict
/// subset, same posture as [`verify_signed`]).
pub(crate) fn verify_device_sig(
    device_sig: Option<&str>,
    publisher_key: &[u8; 32],
    canonical: &[u8],
    what: &str,
) -> Result<(), String> {
    let sig = device_sig.ok_or_else(|| format!("{what} has no device signature"))?;
    // ZEB-678 S2 (review-fix, Qodo security): bound the attacker-controlled hex
    // before decoding — length-check then decode into a fixed buffer, so an
    // oversized-but-valid-hex string can't force a large allocation on the
    // network-ingest path (mirrors feed_authority's capped decoders).
    if sig.len() != 128 {
        return Err(format!(
            "{what} device_sig must be 128 hex chars (64 bytes)"
        ));
    }
    let mut sig_bytes = [0u8; 64];
    hex::decode_to_slice(sig, &mut sig_bytes)
        .map_err(|e| format!("{what} device_sig is not hex: {e}"))?;
    let vk = ed25519_dalek::VerifyingKey::from_bytes(publisher_key)
        .map_err(|_| format!("{what} publisher key invalid"))?;
    vk.verify_strict(canonical, &ed25519_dalek::Signature::from_bytes(&sig_bytes))
        .map_err(|_| format!("{what} device signature invalid"))
}

/// Verify a descriptor's `#2` `device_sig` against the feed authority
/// `publisher_key`.
pub fn verify_descriptor_v2(
    d: &VineDescriptorPayload,
    publisher_key: &[u8; 32],
) -> Result<(), String> {
    verify_device_sig(
        d.device_sig.as_deref(),
        publisher_key,
        &descriptor_canonical_bytes_v2(d),
        "descriptor",
    )
}

/// Verify a follow list's `#2` `device_sig` against the feed authority
/// `publisher_key`.
pub fn verify_follow_list_v2(
    p: &VineFollowListPayload,
    publisher_key: &[u8; 32],
) -> Result<(), String> {
    verify_device_sig(
        p.device_sig.as_deref(),
        publisher_key,
        &follow_list_canonical_bytes_v2(p),
        "follow list",
    )
}

/// Verify a reaction STANDALONE (cross-actor): recover the reactor's enrolled
/// `#2` key from its carried enrollment via the chokepoint, then check
/// `device_sig` against it. `now_secs` is verifier-controlled (supplied by the
/// ingest boundary), never derived from a record field.
pub fn verify_reaction_v2(r: &VineReactionPayload, now_secs: u64) -> Result<(), String> {
    let owner_hex = r.owner_id.as_deref().ok_or("reaction missing owner_id")?;
    // Bound the hex before decoding (Qodo security) — fixed-size owner id.
    if owner_hex.len() != 32 {
        return Err("reaction owner_id must be 32 hex chars (16 bytes)".to_string());
    }
    let mut owner_id = [0u8; 16];
    hex::decode_to_slice(owner_hex, &mut owner_id)
        .map_err(|e| format!("reaction owner_id is not hex: {e}"))?;
    let enrollment = crate::feed_authority::decode_cert(
        r.enrollment_cbor_hex
            .as_deref()
            .ok_or("reaction missing enrollment")?,
    )?;
    let signer_certs = crate::feed_authority::decode_certs(&r.signer_certs_cbor_hex)?;
    let verified = crate::enrollment_verify::verify_enrollment_any_issuer(
        &enrollment,
        &signer_certs,
        Some(&owner_id),
        now_secs,
    )
    .map_err(|e| format!("reaction enrollment invalid: {e}"))?;
    verify_device_sig(
        r.device_sig.as_deref(),
        &verified.device_ed25519,
        &reaction_canonical_bytes_v2(r),
        "reaction",
    )
}

// ── Test/fixture-only identity-minting helpers (ZEB-811 Task 2) ──────────
//
// Shared with `pkarr_vines.rs`'s record-verification test so it doesn't
// duplicate key-minting code. `test_identity` mirrors this module's own
// pre-existing test helper (moved here so it's reachable outside `mod
// tests`); `identity_pub_64`/`identity_signing_key` bridge a
// `PrivateIdentity` to the raw types `PkarrRoutingRecord::sign_new` needs.
// `PrivateIdentity` keeps its Ed25519 `SigningKey` field private (no public
// accessor), so `identity_signing_key` recovers it from
// `to_private_bytes()`'s `[X25519 secret(32) ‖ Ed25519 secret(32)]` layout.

#[cfg(any(test, feature = "test-fixtures"))]
pub(crate) fn test_identity() -> harmony_identity::PrivateIdentity {
    harmony_identity::PrivateIdentity::generate(&mut rand::rngs::OsRng)
}

#[cfg(any(test, feature = "test-fixtures"))]
pub(crate) fn identity_pub_64(identity: &harmony_identity::PrivateIdentity) -> [u8; 64] {
    identity.public_identity().to_public_bytes()
}

#[cfg(any(test, feature = "test-fixtures"))]
pub(crate) fn identity_signing_key(
    identity: &harmony_identity::PrivateIdentity,
) -> ed25519_dalek::SigningKey {
    let priv_bytes = identity.to_private_bytes();
    let ed_seed: [u8; 32] = priv_bytes[32..]
        .try_into()
        .expect("to_private_bytes returns 64 bytes");
    ed25519_dalek::SigningKey::from_bytes(&ed_seed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mutation applied to a signed payload to prove the signature
    /// covers the mutated field.
    type Tamper<T> = Box<dyn Fn(&mut T)>;

    fn addr_of(private: &harmony_identity::PrivateIdentity) -> String {
        hex::encode(private.public_identity().address_hash)
    }

    fn descriptor_for(private: &harmony_identity::PrivateIdentity) -> VineDescriptorPayload {
        VineDescriptorPayload {
            id: "vine-abc-1".into(),
            creator_address: addr_of(private),
            creator_name: "Alice".into(),
            created_at: 1_700_000_000,
            video_cid: "cafe01".into(),
            title: Some("hello world".into()),
            reshare_of: None,
            original_creator_address: None,
            original_creator_name: None,
            identity_pub: None,
            sig: None,
            device_sig: None,
        }
    }

    fn reaction_for(private: &harmony_identity::PrivateIdentity) -> VineReactionPayload {
        VineReactionPayload {
            vine_id: "vine-abc-1".into(),
            reactor_address: addr_of(private),
            reactor_name: "Bob".into(),
            liked: true,
            timestamp: 1_700_000_100,
            identity_pub: None,
            sig: None,
            owner_id: None,
            enrollment_cbor_hex: None,
            signer_certs_cbor_hex: String::new(),
            device_sig: None,
        }
    }

    #[test]
    fn descriptor_sign_verify_roundtrip() {
        let id = test_identity();
        let mut d = descriptor_for(&id);
        sign_descriptor(&id, &mut d);
        assert!(verify_descriptor(&d).is_ok());
    }

    #[test]
    fn reaction_sign_verify_roundtrip() {
        let id = test_identity();
        let mut r = reaction_for(&id);
        sign_reaction(&id, &mut r);
        assert!(verify_reaction(&r).is_ok());
    }

    #[test]
    fn unsigned_records_are_rejected() {
        let id = test_identity();
        let d = descriptor_for(&id);
        assert!(verify_descriptor(&d).unwrap_err().contains("unsigned"));
        let r = reaction_for(&id);
        assert!(verify_reaction(&r).unwrap_err().contains("unsigned"));
    }

    #[test]
    fn descriptor_tamper_any_semantic_field_invalidates() {
        let id = test_identity();
        let base = {
            let mut d = descriptor_for(&id);
            sign_descriptor(&id, &mut d);
            d
        };
        let tampers: Vec<Tamper<VineDescriptorPayload>> = vec![
            Box::new(|d| d.id = "vine-abc-2".into()),
            Box::new(|d| d.creator_name = "Mallory".into()),
            Box::new(|d| d.created_at += 1),
            Box::new(|d| d.video_cid = "beef02".into()),
            Box::new(|d| d.title = Some("evil".into())),
            Box::new(|d| d.title = None),
            Box::new(|d| d.reshare_of = Some("vine-x".into())),
            Box::new(|d| d.original_creator_address = Some("aa".into())),
            Box::new(|d| d.original_creator_name = Some("Carol".into())),
        ];
        for (i, tamper) in tampers.iter().enumerate() {
            let mut d = base.clone();
            tamper(&mut d);
            let err = verify_descriptor(&d).unwrap_err();
            assert!(err.contains("signature invalid"), "tamper #{i} got {err:?}");
        }
    }

    #[test]
    fn reaction_tamper_any_semantic_field_invalidates() {
        let id = test_identity();
        let base = {
            let mut r = reaction_for(&id);
            sign_reaction(&id, &mut r);
            r
        };
        let tampers: Vec<Tamper<VineReactionPayload>> = vec![
            Box::new(|r| r.vine_id = "vine-other".into()),
            Box::new(|r| r.reactor_name = "Mallory".into()),
            Box::new(|r| r.liked = !r.liked),
            Box::new(|r| r.timestamp += 1),
        ];
        for (i, tamper) in tampers.iter().enumerate() {
            let mut r = base.clone();
            tamper(&mut r);
            let err = verify_reaction(&r).unwrap_err();
            assert!(err.contains("signature invalid"), "tamper #{i} got {err:?}");
        }
    }

    #[test]
    fn forged_signer_with_victims_pubkey_is_rejected() {
        // Attacker ships the victim's real pubkey (address binding
        // passes) but can only sign with their own key.
        let victim = test_identity();
        let attacker = test_identity();
        let mut d = descriptor_for(&victim);
        let bytes = descriptor_canonical_bytes(&d);
        d.identity_pub = Some(hex::encode(victim.public_identity().to_public_bytes()));
        d.sig = Some(hex::encode(attacker.sign(&bytes)));
        assert!(verify_descriptor(&d)
            .unwrap_err()
            .contains("signature invalid"));
    }

    #[test]
    fn pubkey_address_mismatch_is_rejected() {
        // Signed by `other` end to end, but claiming `id`'s address:
        // fails the pubkey→address binding gate before sig check.
        let id = test_identity();
        let other = test_identity();
        let mut d = descriptor_for(&id);
        sign_descriptor(&other, &mut d);
        assert!(verify_descriptor(&d)
            .unwrap_err()
            .contains("does not match"));
    }

    #[test]
    fn canonical_none_differs_from_some_empty() {
        let id = test_identity();
        let mut a = descriptor_for(&id);
        a.title = None;
        let mut b = descriptor_for(&id);
        b.title = Some(String::new());
        assert_ne!(
            descriptor_canonical_bytes(&a),
            descriptor_canonical_bytes(&b)
        );
    }

    #[test]
    fn canonical_adjacent_field_shift_differs() {
        // id="ab"/creator_address="c…" vs id="a"/creator_address="bc…"
        // must not collide — the length prefix pins each boundary.
        let id = test_identity();
        let mut a = descriptor_for(&id);
        a.id = "ab".into();
        a.creator_address = format!("c{}", a.creator_address);
        let mut b = descriptor_for(&id);
        b.id = "a".into();
        b.creator_address = format!("bc{}", b.creator_address);
        assert_ne!(
            descriptor_canonical_bytes(&a),
            descriptor_canonical_bytes(&b)
        );

        // Adjacent Options: Some shifting between neighboring fields.
        let mut c = descriptor_for(&id);
        c.title = Some("x".into());
        c.reshare_of = None;
        let mut d = descriptor_for(&id);
        d.title = None;
        d.reshare_of = Some("x".into());
        assert_ne!(
            descriptor_canonical_bytes(&c),
            descriptor_canonical_bytes(&d)
        );
    }

    #[test]
    fn free_text_with_separators_roundtrips() {
        let id = test_identity();
        let mut d = descriptor_for(&id);
        d.title = Some("pipes | and\nnewlines | and 🎬 emoji".into());
        d.creator_name = "we|ird\u{202e}name".into();
        sign_descriptor(&id, &mut d);
        assert!(verify_descriptor(&d).is_ok());
    }

    #[test]
    fn serde_sig_fields_absent_when_none_camel_case_when_some() {
        let id = test_identity();
        let unsigned = descriptor_for(&id);
        let json = serde_json::to_value(&unsigned).unwrap();
        assert!(json.get("identityPub").is_none());
        assert!(json.get("sig").is_none());

        let mut signed = descriptor_for(&id);
        sign_descriptor(&id, &mut signed);
        let json = serde_json::to_value(&signed).unwrap();
        assert!(json.get("identityPub").is_some());
        assert!(json.get("sig").is_some());
    }

    fn follow_list_for(private: &harmony_identity::PrivateIdentity) -> VineFollowListPayload {
        VineFollowListPayload {
            owner_address: addr_of(private),
            follows: vec!["aa".repeat(16), "bb".repeat(16), "cc".repeat(16)],
            updated_at: 1_700_000_200,
            identity_pub: None,
            sig: None,
            device_sig: None,
        }
    }

    #[test]
    fn follow_list_sign_verify_roundtrip() {
        let id = test_identity();
        let mut p = follow_list_for(&id);
        sign_follow_list(&id, &mut p);
        assert!(verify_follow_list(&p).is_ok());

        // Empty list (the opt-out retraction shape) must also roundtrip.
        let mut empty = follow_list_for(&id);
        empty.follows = vec![];
        sign_follow_list(&id, &mut empty);
        assert!(verify_follow_list(&empty).is_ok());
    }

    #[test]
    fn follow_list_unsigned_is_rejected() {
        let id = test_identity();
        let p = follow_list_for(&id);
        assert!(verify_follow_list(&p).unwrap_err().contains("unsigned"));
    }

    #[test]
    fn follow_list_tamper_any_semantic_field_invalidates() {
        let id = test_identity();
        let base = {
            let mut p = follow_list_for(&id);
            sign_follow_list(&id, &mut p);
            p
        };
        let tampers: Vec<Tamper<VineFollowListPayload>> = vec![
            Box::new(|p| p.updated_at += 1),
            Box::new(|p| p.follows.push("dd".repeat(16))),
            Box::new(|p| {
                p.follows.pop();
            }),
            Box::new(|p| p.follows.swap(0, 1)),
            Box::new(|p| p.follows[0] = "ee".repeat(16)),
            Box::new(|p| p.follows.clear()),
        ];
        for (i, tamper) in tampers.iter().enumerate() {
            let mut p = base.clone();
            tamper(&mut p);
            let err = verify_follow_list(&p).unwrap_err();
            assert!(err.contains("signature invalid"), "tamper #{i} got {err:?}");
        }
    }

    #[test]
    fn follow_list_pubkey_address_mismatch_is_rejected() {
        let id = test_identity();
        let other = test_identity();
        let mut p = follow_list_for(&id);
        sign_follow_list(&other, &mut p);
        assert!(verify_follow_list(&p)
            .unwrap_err()
            .contains("does not match"));
    }

    #[test]
    fn follow_list_canonical_entry_boundaries_are_pinned() {
        // ["ab"] vs ["a","b"]: same concatenated bytes, different shape —
        // the count prefix + per-entry length prefixes must distinguish.
        let id = test_identity();
        let mut a = follow_list_for(&id);
        a.follows = vec!["ab".into()];
        let mut b = follow_list_for(&id);
        b.follows = vec!["a".into(), "b".into()];
        assert_ne!(
            follow_list_canonical_bytes(&a),
            follow_list_canonical_bytes(&b)
        );

        // Shift across an entry boundary: ["ab","c"] vs ["a","bc"].
        let mut c = follow_list_for(&id);
        c.follows = vec!["ab".into(), "c".into()];
        let mut d = follow_list_for(&id);
        d.follows = vec!["a".into(), "bc".into()];
        assert_ne!(
            follow_list_canonical_bytes(&c),
            follow_list_canonical_bytes(&d)
        );
    }

    #[test]
    fn follow_list_serde_camel_case_pin() {
        let id = test_identity();
        let mut p = follow_list_for(&id);
        sign_follow_list(&id, &mut p);
        let json = serde_json::to_value(&p).unwrap();
        assert!(json.get("ownerAddress").is_some());
        assert!(json.get("follows").is_some());
        assert!(json.get("updatedAt").is_some());
        assert!(json.get("identityPub").is_some());
        assert!(json.get("sig").is_some());

        // Unsigned construction shape omits the sig fields entirely.
        let unsigned = follow_list_for(&id);
        let json = serde_json::to_value(&unsigned).unwrap();
        assert!(json.get("identityPub").is_none());
        assert!(json.get("sig").is_none());

        // And a sig-less JSON parses (pre-sign/disk shape) but verify
        // rejects it — strict on wire.
        let bare = serde_json::json!({
            "ownerAddress": "aabb",
            "follows": ["ccdd"],
            "updatedAt": 1_700_000_300u64
        });
        let parsed: VineFollowListPayload = serde_json::from_value(bare).unwrap();
        assert!(verify_follow_list(&parsed)
            .unwrap_err()
            .contains("unsigned"));
    }

    #[test]
    fn legacy_json_without_sig_fields_parses() {
        // Pre-ZEB-673 disk/wire shape: no identityPub/sig keys at all.
        let legacy = serde_json::json!({
            "id": "vine-legacy-1",
            "creatorAddress": "aabb",
            "creatorName": "Old Peer",
            "createdAt": 1_700_000_000u64,
            "videoCid": "cafe01"
        });
        let d: VineDescriptorPayload = serde_json::from_value(legacy).unwrap();
        assert!(d.identity_pub.is_none());
        assert!(d.sig.is_none());

        let legacy_r = serde_json::json!({
            "vineId": "vine-legacy-1",
            "reactorAddress": "ccdd",
            "reactorName": "Old Reactor",
            "liked": true,
            "timestamp": 1_700_000_100u64
        });
        let r: VineReactionPayload = serde_json::from_value(legacy_r).unwrap();
        assert!(r.identity_pub.is_none());
        assert!(r.sig.is_none());
    }

    // ── ZEB-678 S2: enrolled `#2` device-key (`-v2`) signing ─────────────

    #[test]
    fn descriptor_v2_sign_verify_roundtrip_and_wrong_key_rejected() {
        let sk = ed25519_dalek::SigningKey::from_bytes(&[0x11; 32]);
        let pk = sk.verifying_key().to_bytes();
        let id = test_identity();
        let mut d = descriptor_for(&id);
        assert!(d.device_sig.is_none());
        sign_descriptor_v2(&sk, &mut d);
        assert!(d.device_sig.is_some(), "device_sig set");
        // v2 signing leaves the legacy `#3` fields untouched (migrated record).
        assert!(d.sig.is_none() && d.identity_pub.is_none());
        verify_descriptor_v2(&d, &pk).expect("valid #2 signature");

        let wrong = ed25519_dalek::SigningKey::from_bytes(&[0x22; 32])
            .verifying_key()
            .to_bytes();
        assert!(
            verify_descriptor_v2(&d, &wrong).is_err(),
            "wrong publisher key rejected"
        );
        // A `#3`-only descriptor (no device_sig) fails the v2 path.
        let d3 = descriptor_for(&id);
        assert!(verify_descriptor_v2(&d3, &pk)
            .unwrap_err()
            .contains("no device signature"));
    }

    #[test]
    fn follow_list_v2_sign_verify_and_device_sig_omitted_when_none() {
        let sk = ed25519_dalek::SigningKey::from_bytes(&[0x33; 32]);
        let pk = sk.verifying_key().to_bytes();
        let id = test_identity();
        let mut p = follow_list_for(&id);
        let before = serde_json::to_value(&p).unwrap();
        assert!(
            before.get("deviceSig").is_none(),
            "deviceSig omitted when None"
        );
        sign_follow_list_v2(&sk, &mut p);
        verify_follow_list_v2(&p, &pk).expect("valid #2 follow-list signature");
        assert!(serde_json::to_value(&p).unwrap().get("deviceSig").is_some());
        let wrong = ed25519_dalek::SigningKey::from_bytes(&[0x44; 32])
            .verifying_key()
            .to_bytes();
        assert!(verify_follow_list_v2(&p, &wrong).is_err());
    }

    // NOTE: `verify_reaction_v2` intentionally verifies ONLY the `#2` layer
    // (enrollment + device_sig); it does NOT bind `reactor_address` — the `#2`
    // device cannot be tied to a `#3` node address standalone. That binding is
    // enforced by the ingest path (`VineFeedCache::on_reaction_sample` always
    // runs `verify_reaction` first), so this isolation test uses an unrelated
    // `reactor_address` by design.
    #[test]
    fn reaction_v2_self_verifies_master_and_quorum() {
        use crate::enrollment_verify::quorum_fixtures::{mint_quorum_world, WORLD_NOW};
        let world = mint_quorum_world(0xC0);

        // Master-issued reactor: `#2` key = a_sk, cert = a_cert, empty bundle.
        let mut r = reaction_for(&test_identity());
        r.owner_id = Some(hex::encode(world.owner_id));
        r.enrollment_cbor_hex = Some(crate::feed_authority::encode_cert(&world.a_cert).unwrap());
        r.signer_certs_cbor_hex = String::new();
        sign_reaction_v2(&world.a_sk, &mut r);
        verify_reaction_v2(&r, WORLD_NOW).expect("master-issued reaction self-verifies");

        // A device_sig NOT from the enrolled key is rejected.
        let mut bad = r.clone();
        sign_reaction_v2(
            &ed25519_dalek::SigningKey::from_bytes(&[0x99; 32]),
            &mut bad,
        );
        assert!(
            verify_reaction_v2(&bad, WORLD_NOW).is_err(),
            "device_sig not from the enrolled key is rejected"
        );

        // Quorum-issued reactor: `#2` key = c_sk, cert = c_quorum_cert,
        // signer bundle = [a_cert, b_cert].
        let mut rq = reaction_for(&test_identity());
        rq.owner_id = Some(hex::encode(world.owner_id));
        rq.enrollment_cbor_hex =
            Some(crate::feed_authority::encode_cert(&world.c_quorum_cert).unwrap());
        rq.signer_certs_cbor_hex = crate::feed_authority::encode_certs(&world.bundle).unwrap();
        sign_reaction_v2(&world.c_sk, &mut rq);
        verify_reaction_v2(&rq, WORLD_NOW).expect("quorum-issued reaction self-verifies");

        // Owner-id mismatch is rejected (enrollment is not under that owner).
        let mut wrong_owner = r.clone();
        wrong_owner.owner_id = Some(hex::encode([0x55u8; 16]));
        assert!(verify_reaction_v2(&wrong_owner, WORLD_NOW).is_err());
    }

    /// Qodo security: the attacker-controlled `device_sig` hex is length-bounded
    /// BEFORE decoding, so an oversized-but-valid-hex string is rejected on the
    /// length gate rather than allocating first.
    #[test]
    fn verify_device_sig_length_bounded_before_decode() {
        let pk = [0u8; 32];
        let oversized = "a".repeat(10_000);
        let err = verify_device_sig(Some(&oversized), &pk, b"canonical", "test").unwrap_err();
        assert!(
            err.contains("128 hex chars"),
            "oversized rejected by length: {err}"
        );

        // A correctly-sized (but not matching) sig passes the length gate and
        // fails later at verify — proving the gate rejects only on length.
        let right_len = "a".repeat(128);
        let err2 = verify_device_sig(Some(&right_len), &pk, b"canonical", "test").unwrap_err();
        assert!(
            !err2.contains("128 hex chars"),
            "128-char sig clears the length gate: {err2}"
        );
    }
}
