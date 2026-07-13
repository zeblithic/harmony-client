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
    if hex::encode(identity.address_hash) != claimed_address {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A mutation applied to a signed payload to prove the signature
    /// covers the mutated field.
    type Tamper<T> = Box<dyn Fn(&mut T)>;

    fn test_identity() -> harmony_identity::PrivateIdentity {
        harmony_identity::PrivateIdentity::generate(&mut rand::rngs::OsRng)
    }

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
}
