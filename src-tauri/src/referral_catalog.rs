//! ZEB-375 (Friends Phase 2a): referral-catalog wire types + codecs for the
//! `harmony/friend-pex/v1` awareness sub-protocol. Strict CBOR, single-char map
//! keys, bounded decode with trailing-byte rejection — mirrors
//! `iroh_friend_acceptor`'s handshake codec so the two sub-protocols share
//! one wire discipline.

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::friend_graph::deserialize_capped_display;
use crate::iroh_friend_acceptor::verify_enrolled_device;
use crate::owner_state_types::{
    deserialize_bytes_from_bstr, serialize_bytes_as_bstr, Hlc, OwnerAddr,
};
// EnrollmentCert: imported via the SAME path `iroh_friend_acceptor.rs` uses.
use harmony_owner::certs::EnrollmentCert;

/// Same bound as the handshake codec — a friend-PEX body never exceeds it.
pub const PEX_MAX_PACKET_LEN: usize = 256 * 1024;
/// Hard cap on entries served in one catalog (truncation is logged, never silent).
pub const MAX_REFERRAL_ENTRIES: usize = 256;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ReferralCodecError {
    #[error("referral packet too large: {len} > {max}")]
    TooLarge { len: usize, max: usize },
    #[error("referral encode failed: {0}")]
    Encode(String),
    #[error("referral decode failed: {0}")]
    Decode(String),
    #[error("trailing bytes after referral CBOR")]
    TrailingBytes,
}

/// A single referrable peer the catalog author could introduce: the peer's
/// master `OwnerAddr` plus an optional display-name hint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferralEntry {
    #[serde(rename = "o")]
    pub peer_owner: OwnerAddr,
    /// Display hint, capped at `MAX_FRIEND_DISPLAY_LEN` at the WIRE boundary
    /// (oversized → hard decode error, not truncation), same as `FriendEntry`.
    #[serde(rename = "n", default, deserialize_with = "deserialize_capped_display")]
    pub display: Option<String>,
}

/// A signed catalog of an author's referrable friends, addressed to a specific
/// subject (the requester it answers). `sig` is the author's device-#2 Ed25519
/// signature over [`referral_catalog_sig_preimage`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferralCatalog {
    #[serde(rename = "a")]
    pub author: OwnerAddr,
    #[serde(rename = "e")]
    pub entries: Vec<ReferralEntry>,
    #[serde(rename = "t")]
    pub at: Hlc,
    #[serde(rename = "c")]
    pub enrollment: EnrollmentCert,
    #[serde(
        rename = "s",
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub sig: [u8; 64],
}

/// A request to browse a friend's referral catalog. `sig` is the requester's
/// device-#2 Ed25519 signature over [`catalog_request_sig_preimage`]; `to_addr`
/// binds the request to a specific server so a captured request cannot be
/// re-aimed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogRequest {
    #[serde(rename = "a")]
    pub from_addr: OwnerAddr,
    #[serde(rename = "d")]
    pub to_addr: OwnerAddr,
    #[serde(rename = "c")]
    pub enrollment: EnrollmentCert,
    #[serde(
        rename = "s",
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub sig: [u8; 64],
}

/// Decode a single CBOR item, bounding the input at [`PEX_MAX_PACKET_LEN`] and
/// rejecting any trailing bytes. Mirrors `iroh_friend_acceptor::decode_strict`
/// (which is private to that module) so the two sub-protocols share one wire
/// discipline.
fn decode_strict<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, ReferralCodecError> {
    if bytes.len() > PEX_MAX_PACKET_LEN {
        return Err(ReferralCodecError::TooLarge {
            len: bytes.len(),
            max: PEX_MAX_PACKET_LEN,
        });
    }
    let mut cursor = std::io::Cursor::new(bytes);
    let v = ciborium::from_reader(&mut cursor)
        .map_err(|e| ReferralCodecError::Decode(e.to_string()))?;
    if cursor.position() as usize != bytes.len() {
        return Err(ReferralCodecError::TrailingBytes);
    }
    Ok(v)
}

/// Encode a [`CatalogRequest`] to CBOR bytes (no length prefix).
pub fn encode_catalog_request(req: &CatalogRequest) -> Result<Vec<u8>, ReferralCodecError> {
    let mut out = Vec::new();
    ciborium::into_writer(req, &mut out).map_err(|e| ReferralCodecError::Encode(e.to_string()))?;
    Ok(out)
}

/// Decode a [`CatalogRequest`] from CBOR bytes (bounded, strict).
pub fn decode_catalog_request(bytes: &[u8]) -> Result<CatalogRequest, ReferralCodecError> {
    decode_strict(bytes)
}

/// Encode a [`ReferralCatalog`] to CBOR bytes (no length prefix).
pub fn encode_referral_catalog(cat: &ReferralCatalog) -> Result<Vec<u8>, ReferralCodecError> {
    let mut out = Vec::new();
    ciborium::into_writer(cat, &mut out).map_err(|e| ReferralCodecError::Encode(e.to_string()))?;
    Ok(out)
}

/// Decode a [`ReferralCatalog`] from CBOR bytes (bounded, strict).
pub fn decode_referral_catalog(bytes: &[u8]) -> Result<ReferralCatalog, ReferralCodecError> {
    decode_strict(bytes)
}

/// Bytes R's device-#2 key signs for a [`CatalogRequest`]. `"hcr1"` domain tag +
/// requester + target (binding `to_addr` blocks re-aiming a captured request).
pub fn catalog_request_sig_preimage(from_addr: OwnerAddr, to_addr: OwnerAddr) -> Vec<u8> {
    #[derive(Serialize)]
    struct P {
        d: &'static str,
        a: OwnerAddr,
        t: OwnerAddr,
    }
    let mut out = Vec::new();
    ciborium::into_writer(
        &P {
            d: "hcr1",
            a: from_addr,
            t: to_addr,
        },
        &mut out,
    )
    .expect("fixed-shape encode is infallible");
    out
}

/// Bytes F's device-#2 key signs for a [`ReferralCatalog`]. `"hrc1"` domain tag +
/// author + subject (binding `subject` blocks re-showing a catalog to another
/// requester) + the served entries + clock.
pub fn referral_catalog_sig_preimage(
    author: OwnerAddr,
    subject: OwnerAddr,
    entries: &[ReferralEntry],
    at: &Hlc,
) -> Vec<u8> {
    #[derive(Serialize)]
    struct P<'a> {
        d: &'static str,
        a: OwnerAddr,
        s: OwnerAddr,
        e: &'a [ReferralEntry],
        t: &'a Hlc,
    }
    let mut out = Vec::new();
    ciborium::into_writer(
        &P {
            d: "hrc1",
            a: author,
            s: subject,
            e: entries,
            t: at,
        },
        &mut out,
    )
    .expect("fixed-shape encode is infallible");
    out
}

/// Failure modes when authenticating a [`CatalogRequest`] or verifying a
/// [`ReferralCatalog`]. The target/identity checks (`WrongTarget`,
/// `AuthorMismatch`) are reported *before* the cert/signature checks (`Auth`,
/// `SignatureInvalid`) so a mis-addressed or mis-attributed message is rejected
/// without spending a signature verification.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ReferralAuthError {
    /// The enrollment cert failed `verify_enrolled_device` (bad cert, non-Master
    /// issuer, or `cert.owner_id` != the claimed owner).
    #[error("referral enrollment cert authentication failed")]
    Auth,
    /// The catalog's `author` did not match the author the verifier expected.
    #[error("referral catalog author mismatch")]
    AuthorMismatch,
    /// The request's `to_addr` did not match this server's own owner address.
    #[error("catalog request addressed to a different owner")]
    WrongTarget,
    /// The device-#2 Ed25519 signature did not verify over the canonical preimage.
    #[error("referral signature invalid")]
    SignatureInvalid,
}

/// Build a device-#2-signed [`CatalogRequest`] from R to F. Signs
/// [`catalog_request_sig_preimage`] (domain `"hcr1"`, binds `from`+`to`).
pub fn sign_catalog_request(
    device2: &SigningKey,
    from_addr: OwnerAddr,
    to_addr: OwnerAddr,
    enrollment: EnrollmentCert,
) -> CatalogRequest {
    let preimage = catalog_request_sig_preimage(from_addr, to_addr);
    let sig = device2.sign(&preimage).to_bytes();
    CatalogRequest {
        from_addr,
        to_addr,
        enrollment,
        sig,
    }
}

/// Authenticate an inbound [`CatalogRequest`] against this server's own owner.
///
/// Order is security-load-bearing:
/// 1. `req.to_addr == self_owner` (else [`ReferralAuthError::WrongTarget`]) — a
///    captured request cannot be re-aimed at a different server.
/// 2. `verify_enrolled_device(&req.enrollment, req.from_addr)` → device key
///    (cert err mapped to [`ReferralAuthError::Auth`]; also enforces
///    `cert.owner_id == from_addr`).
/// 3. `verify_strict` of `req.sig` over [`catalog_request_sig_preimage`] against
///    that device key (err → [`ReferralAuthError::SignatureInvalid`]).
pub fn authenticate_catalog_request(
    req: &CatalogRequest,
    self_owner: OwnerAddr,
) -> Result<(), ReferralAuthError> {
    if req.to_addr != self_owner {
        return Err(ReferralAuthError::WrongTarget);
    }
    let device_key = verify_enrolled_device(&req.enrollment, req.from_addr)
        .map_err(|_| ReferralAuthError::Auth)?;
    let vk =
        VerifyingKey::from_bytes(&device_key).map_err(|_| ReferralAuthError::SignatureInvalid)?;
    let preimage = catalog_request_sig_preimage(req.from_addr, req.to_addr);
    vk.verify_strict(&preimage, &Signature::from_bytes(&req.sig))
        .map_err(|_| ReferralAuthError::SignatureInvalid)?;
    Ok(())
}

/// Build a device-#2-signed [`ReferralCatalog`] from `author` for `subject`.
/// Signs [`referral_catalog_sig_preimage`] (domain `"hrc1"`, binds author +
/// subject + entries + clock).
pub fn sign_referral_catalog(
    device2: &SigningKey,
    author: OwnerAddr,
    subject: OwnerAddr,
    entries: Vec<ReferralEntry>,
    at: Hlc,
    enrollment: EnrollmentCert,
) -> ReferralCatalog {
    let preimage = referral_catalog_sig_preimage(author, subject, &entries, &at);
    let sig = device2.sign(&preimage).to_bytes();
    ReferralCatalog {
        author,
        entries,
        at,
        enrollment,
        sig,
    }
}

/// Verify a [`ReferralCatalog`] is the author we expected, signed by their
/// enrolled device, and addressed to the subject we expected.
///
/// Order is security-load-bearing:
/// 1. `cat.author == expected_author` (else [`ReferralAuthError::AuthorMismatch`]).
/// 2. `verify_enrolled_device(&cat.enrollment, cat.author)` → device key (cert
///    err → [`ReferralAuthError::Auth`]; also enforces `cert.owner_id ==
///    author`, so a swapped-in cert for a different owner is rejected here).
/// 3. `verify_strict` of `cat.sig` over `referral_catalog_sig_preimage(author,
///    expected_subject, entries, at)` (err → [`ReferralAuthError::SignatureInvalid`]).
///    Binding `expected_subject` into the preimage means a catalog signed for
///    one requester will not verify when replayed to another.
pub fn verify_referral_catalog(
    cat: &ReferralCatalog,
    expected_author: OwnerAddr,
    expected_subject: OwnerAddr,
) -> Result<(), ReferralAuthError> {
    if cat.author != expected_author {
        return Err(ReferralAuthError::AuthorMismatch);
    }
    let device_key =
        verify_enrolled_device(&cat.enrollment, cat.author).map_err(|_| ReferralAuthError::Auth)?;
    let vk =
        VerifyingKey::from_bytes(&device_key).map_err(|_| ReferralAuthError::SignatureInvalid)?;
    let preimage =
        referral_catalog_sig_preimage(cat.author, expected_subject, &cat.entries, &cat.at);
    vk.verify_strict(&preimage, &Signature::from_bytes(&cat.sig))
        .map_err(|_| ReferralAuthError::SignatureInvalid)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_membership::mint_test_owner;
    use crate::owner_state_types::{Hlc, OwnerAddr};

    /// Deterministic HLC for fixtures.
    fn hlc(n: u64) -> Hlc {
        Hlc {
            wall_ms: n,
            logical: 0,
            device_id: "test-device".to_string(),
        }
    }

    /// Deterministic `CatalogRequest` fixture.
    fn sample_request() -> CatalogRequest {
        CatalogRequest {
            from_addr: OwnerAddr([0x11; 16]),
            to_addr: OwnerAddr([0x22; 16]),
            enrollment: mint_test_owner(0x42).cert,
            sig: [9u8; 64],
        }
    }

    /// Deterministic `ReferralCatalog` fixture.
    fn sample_catalog() -> ReferralCatalog {
        ReferralCatalog {
            author: OwnerAddr([0x11; 16]),
            entries: vec![ReferralEntry {
                peer_owner: OwnerAddr([0x33; 16]),
                display: Some("bob".to_string()),
            }],
            at: hlc(7),
            enrollment: mint_test_owner(0x42).cert,
            sig: [9u8; 64],
        }
    }

    #[test]
    fn catalog_request_round_trips() {
        let req = sample_request();
        let bytes = encode_catalog_request(&req).expect("encode");
        assert_eq!(decode_catalog_request(&bytes).expect("decode"), req);
    }

    #[test]
    fn referral_catalog_round_trips() {
        let cat = sample_catalog();
        let bytes = encode_referral_catalog(&cat).expect("encode");
        assert_eq!(decode_referral_catalog(&bytes).expect("decode"), cat);
    }

    #[test]
    fn decode_rejects_trailing_bytes() {
        let mut bytes = encode_catalog_request(&sample_request()).unwrap();
        bytes.push(0x00);
        assert!(decode_catalog_request(&bytes).is_err());
    }

    #[test]
    fn decode_rejects_oversize() {
        let huge = vec![0u8; PEX_MAX_PACKET_LEN + 1];
        assert!(decode_referral_catalog(&huge).is_err());
    }

    #[test]
    fn display_is_capped_on_decode() {
        let mut cat = sample_catalog();
        cat.entries[0].display = Some("x".repeat(crate::friend_graph::MAX_FRIEND_DISPLAY_LEN + 1));
        let bytes = encode_referral_catalog(&cat).unwrap();
        assert!(decode_referral_catalog(&bytes).is_err());
    }

    #[test]
    fn request_and_catalog_preimages_are_domain_separated() {
        let a = OwnerAddr([1u8; 16]);
        let b = OwnerAddr([2u8; 16]);
        let req_pre = catalog_request_sig_preimage(a, b);
        let cat_pre = referral_catalog_sig_preimage(a, b, &[], &hlc(7));
        assert_ne!(req_pre, cat_pre, "distinct domains must never collide");
    }

    /// One deterministic referral entry for the signed-catalog fixtures.
    fn one_entry() -> Vec<ReferralEntry> {
        vec![ReferralEntry {
            peer_owner: OwnerAddr([0x33; 16]),
            display: Some("bob".to_string()),
        }]
    }

    #[test]
    fn signed_catalog_verifies_and_tamper_is_rejected() {
        let f = mint_test_owner(0x11);
        let subject = OwnerAddr([0x22; 16]);
        let entries = one_entry();
        let cat = sign_referral_catalog(
            &f.device_key,
            f.owner,
            subject,
            entries.clone(),
            hlc(7),
            f.cert.clone(),
        );

        // Happy path: correct author + subject verifies.
        assert!(verify_referral_catalog(&cat, f.owner, subject).is_ok());

        // Wrong subject → the signature no longer covers the expected preimage.
        assert!(verify_referral_catalog(&cat, f.owner, OwnerAddr([0x99; 16])).is_err());

        // Wrong expected author → AuthorMismatch (checked before crypto).
        assert!(verify_referral_catalog(&cat, OwnerAddr([0x88; 16]), subject).is_err());

        // Tampered entry display → signature over the entries no longer matches.
        let mut tampered = cat.clone();
        tampered.entries[0].display = Some("eve".to_string());
        assert!(verify_referral_catalog(&tampered, f.owner, subject).is_err());
    }

    #[test]
    fn catalog_with_mismatched_cert_owner_is_rejected() {
        let f = mint_test_owner(0x11);
        let g = mint_test_owner(0x12);
        let subject = OwnerAddr([2; 16]);
        let mut cat = sign_referral_catalog(
            &f.device_key,
            f.owner,
            subject,
            one_entry(),
            hlc(7),
            f.cert.clone(),
        );
        // Swap in G's cert: now cert.owner_id (G) != author (F).
        cat.enrollment = g.cert.clone();
        assert!(verify_referral_catalog(&cat, f.owner, subject).is_err());
    }

    #[test]
    fn catalog_request_with_mismatched_cert_owner_is_rejected() {
        // A request signed by R but carrying a DIFFERENT owner's (S's) cert must
        // fail: verify_enrolled_device checks cert.owner_id == from_addr.
        let r = mint_test_owner(0x21);
        let s = mint_test_owner(0x22);
        let f_owner = OwnerAddr([0x42; 16]);
        let mut req = sign_catalog_request(&r.device_key, r.owner, f_owner, r.cert.clone());
        req.enrollment = s.cert.clone();
        assert!(authenticate_catalog_request(&req, f_owner).is_err());
    }

    #[test]
    fn catalog_request_auth_enforces_to_addr_and_sig() {
        let r = mint_test_owner(0x21);
        let f_owner = OwnerAddr([0x42; 16]);
        let req = sign_catalog_request(&r.device_key, r.owner, f_owner, r.cert.clone());

        // Happy path: addressed to f_owner, valid sig.
        assert!(authenticate_catalog_request(&req, f_owner).is_ok());

        // Wrong target server → WrongTarget (checked before crypto).
        assert!(authenticate_catalog_request(&req, OwnerAddr([0x43; 16])).is_err());

        // Flipped signature bit → SignatureInvalid.
        let mut bad = req.clone();
        bad.sig[0] ^= 1;
        assert!(authenticate_catalog_request(&bad, f_owner).is_err());
    }
}
