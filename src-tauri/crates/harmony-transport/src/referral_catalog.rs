//! ZEB-375 (Friends Phase 2a): referral-catalog wire types + codecs for the
//! `harmony/friend-pex/v1` awareness sub-protocol. Strict CBOR, single-char map
//! keys, bounded decode with trailing-byte rejection — mirrors
//! `iroh_friend_acceptor`'s handshake codec so the two sub-protocols share
//! one wire discipline.

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::friend_graph::{deserialize_capped_display, FriendGraph, FriendStatus};
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
    #[error("too many referral entries: {len} > {max}")]
    TooManyEntries { len: usize, max: usize },
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
    /// ZEB-677: Master-issued signer certs backing a Quorum-issued
    /// `enrollment`. Empty for Master-issued certs; see
    /// `FriendLinkRequest.signer_certs`.
    #[serde(rename = "b", default, skip_serializing_if = "Vec::is_empty")]
    pub signer_certs: Vec<EnrollmentCert>,
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
    /// ZEB-677: Master-issued signer certs backing a Quorum-issued
    /// `enrollment`. Empty for Master-issued certs; see
    /// `FriendLinkRequest.signer_certs`.
    #[serde(rename = "b", default, skip_serializing_if = "Vec::is_empty")]
    pub signer_certs: Vec<EnrollmentCert>,
}

/// Decode a single CBOR item, bounding the input at [`PEX_MAX_PACKET_LEN`] and
/// rejecting any trailing bytes. Mirrors `iroh_friend_acceptor::decode_strict`
/// (which is private to that module) so the two sub-protocols share one wire
/// discipline.
pub(crate) fn decode_strict<T: serde::de::DeserializeOwned>(
    bytes: &[u8],
) -> Result<T, ReferralCodecError> {
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

/// Decode a [`ReferralCatalog`] from CBOR bytes (bounded, strict). Enforces the
/// logical [`MAX_REFERRAL_ENTRIES`] cap on the decoded entry count: a malicious
/// friend could pack many more entries than the serve-side truncation allows
/// while still fitting under [`PEX_MAX_PACKET_LEN`], so we reject any catalog
/// over the cap rather than ingest it.
pub fn decode_referral_catalog(bytes: &[u8]) -> Result<ReferralCatalog, ReferralCodecError> {
    let cat: ReferralCatalog = decode_strict(bytes)?;
    if cat.entries.len() > MAX_REFERRAL_ENTRIES {
        return Err(ReferralCodecError::TooManyEntries {
            len: cat.entries.len(),
            max: MAX_REFERRAL_ENTRIES,
        });
    }
    Ok(cat)
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
        // ZEB-677: bundle threading for quorum-certed requesters lands with
        // the ceremony slices (S4); every self-cert today is Master-issued.
        signer_certs: Vec::new(),
    }
}

/// Authenticate an inbound [`CatalogRequest`] against this server's own owner.
///
/// Order is security-load-bearing:
/// 1. `req.to_addr == self_owner` (else [`ReferralAuthError::WrongTarget`]) — a
///    captured request cannot be re-aimed at a different server.
/// 2. `verify_enrolled_device(&req.enrollment, &req.signer_certs,
///    req.from_addr)` → device key (cert err mapped to
///    [`ReferralAuthError::Auth`]; also enforces `cert.owner_id == from_addr`).
/// 3. `verify_strict` of `req.sig` over [`catalog_request_sig_preimage`] against
///    that device key (err → [`ReferralAuthError::SignatureInvalid`]).
pub fn authenticate_catalog_request(
    req: &CatalogRequest,
    self_owner: OwnerAddr,
    // ZEB-680 §1: consulted (via the inner `verify_enrolled_device`) against the
    // requester's owner + verified device key.
    revoked: &crate::revoked_device_projection::RevokedDeviceProjection,
    now_secs: u64,
) -> Result<(), ReferralAuthError> {
    if req.to_addr != self_owner {
        return Err(ReferralAuthError::WrongTarget);
    }
    let verified = verify_enrolled_device(
        &req.enrollment,
        &req.signer_certs,
        req.from_addr,
        revoked,
        now_secs,
    )
    .map_err(|_| ReferralAuthError::Auth)?;
    let vk = VerifyingKey::from_bytes(&verified.device_ed25519)
        .map_err(|_| ReferralAuthError::SignatureInvalid)?;
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
        // ZEB-677: see sign_catalog_request — threading lands with S4.
        signer_certs: Vec::new(),
    }
}

/// Verify a [`ReferralCatalog`] is the author we expected, signed by their
/// enrolled device, and addressed to the subject we expected.
///
/// Order is security-load-bearing:
/// 1. `cat.author == expected_author` (else [`ReferralAuthError::AuthorMismatch`]).
/// 2. `verify_enrolled_device(&cat.enrollment, &cat.signer_certs, cat.author)`
///    → device key (cert err → [`ReferralAuthError::Auth`]; also enforces
///    `cert.owner_id == author`, so a swapped-in cert for a different owner is
///    rejected here).
/// 3. `verify_strict` of `cat.sig` over `referral_catalog_sig_preimage(author,
///    expected_subject, entries, at)` (err → [`ReferralAuthError::SignatureInvalid`]).
///    Binding `expected_subject` into the preimage means a catalog signed for
///    one requester will not verify when replayed to another.
pub fn verify_referral_catalog(
    cat: &ReferralCatalog,
    expected_author: OwnerAddr,
    expected_subject: OwnerAddr,
    // ZEB-680 §1: consulted (via the inner `verify_enrolled_device`) against the
    // catalog author's owner + verified device key.
    revoked: &crate::revoked_device_projection::RevokedDeviceProjection,
    now_secs: u64,
) -> Result<(), ReferralAuthError> {
    if cat.author != expected_author {
        return Err(ReferralAuthError::AuthorMismatch);
    }
    let verified = verify_enrolled_device(
        &cat.enrollment,
        &cat.signer_certs,
        cat.author,
        revoked,
        now_secs,
    )
    .map_err(|_| ReferralAuthError::Auth)?;
    let vk = VerifyingKey::from_bytes(&verified.device_ed25519)
        .map_err(|_| ReferralAuthError::SignatureInvalid)?;
    let preimage =
        referral_catalog_sig_preimage(cat.author, expected_subject, &cat.entries, &cat.at);
    vk.verify_strict(&preimage, &Signature::from_bytes(&cat.sig))
        .map_err(|_| ReferralAuthError::SignatureInvalid)?;
    Ok(())
}

/// Project a [`FriendGraph`] into the referral entries we are willing to serve:
/// only `Active` friends the user has explicitly marked `referrable`. Yielded in
/// the graph's deterministic `BTreeMap` key order and capped at
/// [`MAX_REFERRAL_ENTRIES`]; any overflow beyond the cap is *logged* (never
/// silently dropped). `Pending`/`Revoked` and non-`referrable` friends are
/// excluded — the catalog is a sharer-side opt-in surface, not the full graph.
pub fn collect_referrable_entries(fg: &FriendGraph) -> Vec<ReferralEntry> {
    let mut out = Vec::new();
    let mut dropped = 0usize;
    for (owner, e) in fg.friends.iter() {
        if e.status == FriendStatus::Active && e.referrable {
            if out.len() < MAX_REFERRAL_ENTRIES {
                out.push(ReferralEntry {
                    peer_owner: *owner,
                    display: e.display.clone(),
                });
            } else {
                dropped += 1;
            }
        }
    }
    if dropped > 0 {
        tracing::warn!(
            dropped,
            cap = MAX_REFERRAL_ENTRIES,
            "referral catalog truncated"
        );
    }
    out
}

/// Build a device-#2-signed [`ReferralCatalog`] addressed to `subject` from the
/// caller's own `Active`+`referrable` friends. Convenience wrapper over
/// [`collect_referrable_entries`] + [`sign_referral_catalog`].
pub fn build_referral_catalog(
    fg: &FriendGraph,
    subject: OwnerAddr,
    self_owner: OwnerAddr,
    self_enrollment: EnrollmentCert,
    device2: &SigningKey,
    at: Hlc,
) -> ReferralCatalog {
    sign_referral_catalog(
        device2,
        self_owner,
        subject,
        collect_referrable_entries(fg),
        at,
        self_enrollment,
    )
}

/// A single referral entry projected for the requester's browse view. `display`
/// is the author's name hint for the referred peer; `already_friend` is `true`
/// when the requester's OWN friend graph already holds that peer as an `Active`
/// or `Pending` link (so the UI can mark it "already friends" and Phase 2b can
/// suppress a redundant introduction). Owner-state types stay backend-only; the
/// peer is surfaced as a hex string.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReferralView {
    /// The referred peer's 16-byte master `owner_id`, hex-encoded.
    pub owner_id_hex: String,
    /// The author's display-name hint for this peer, if any.
    pub display: Option<String>,
    /// Whether the requester already has this peer as an Active/Pending friend.
    pub already_friend: bool,
}

/// Project a verified [`ReferralCatalog`] into the requester-facing browse view,
/// cross-referencing each entry against the requester's OWN friend graph so the
/// UI can flag peers we already know. Pure over the catalog + graph so it's
/// unit-testable without a NodeState harness. Order mirrors the catalog's
/// `entries` order (which the author produced in deterministic `BTreeMap` key
/// order).
pub fn project_referrals(cat: &ReferralCatalog, fg: &FriendGraph) -> Vec<ReferralView> {
    cat.entries
        .iter()
        .map(|entry| ReferralView {
            owner_id_hex: hex::encode(entry.peer_owner.0),
            display: entry.display.clone(),
            already_friend: fg
                .friends
                .get(&entry.peer_owner)
                .map(|e| matches!(e.status, FriendStatus::Active | FriendStatus::Pending))
                .unwrap_or(false),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enrollment_verify::quorum_fixtures::mint_test_owner;
    use crate::friend_graph::{FriendEntry, FriendOrigin};
    use crate::owner_state_types::{Hlc, OwnerAddr};

    /// ZEB-680: an empty revoked-device projection for verifier call sites that
    /// don't exercise revocation (it revokes nothing).
    fn no_revocations() -> crate::revoked_device_projection::RevokedDeviceProjection {
        crate::revoked_device_projection::RevokedDeviceProjection::new()
    }

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
            signer_certs: Vec::new(),
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
            signer_certs: Vec::new(),
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
    fn decode_rejects_too_many_entries() {
        // A malicious friend packs MORE entries than the logical cap while
        // still fitting under PEX_MAX_PACKET_LEN. Encoding has no cap, so the
        // fixture builds fine — but decode must reject it.
        let entries: Vec<ReferralEntry> = (0..=MAX_REFERRAL_ENTRIES)
            .map(|i| ReferralEntry {
                // Distinct peer_owner per entry. The count exceeds 256, so a
                // single index byte would collide (i=256 with i=0); spread the
                // index across 2 bytes so every entry is unique.
                peer_owner: {
                    let mut key = [0u8; 16];
                    key[0] = (i & 0xff) as u8;
                    key[1] = (i >> 8) as u8;
                    OwnerAddr(key)
                },
                display: None,
            })
            .collect();
        assert_eq!(entries.len(), MAX_REFERRAL_ENTRIES + 1);
        let cat = ReferralCatalog {
            author: OwnerAddr([0x11; 16]),
            entries,
            at: hlc(7),
            enrollment: mint_test_owner(0x42).cert,
            signer_certs: Vec::new(),
            sig: [9u8; 64],
        };
        let bytes = encode_referral_catalog(&cat).expect("encode has no cap");
        assert_eq!(
            decode_referral_catalog(&bytes),
            Err(ReferralCodecError::TooManyEntries {
                len: MAX_REFERRAL_ENTRIES + 1,
                max: MAX_REFERRAL_ENTRIES,
            }),
        );
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
        assert!(verify_referral_catalog(&cat, f.owner, subject, &no_revocations(), 0).is_ok());

        // Wrong subject → the signature no longer covers the expected preimage.
        assert!(verify_referral_catalog(
            &cat,
            f.owner,
            OwnerAddr([0x99; 16]),
            &no_revocations(),
            0
        )
        .is_err());

        // Wrong expected author → AuthorMismatch (checked before crypto).
        assert!(verify_referral_catalog(
            &cat,
            OwnerAddr([0x88; 16]),
            subject,
            &no_revocations(),
            0
        )
        .is_err());

        // Tampered entry display → signature over the entries no longer matches.
        let mut tampered = cat.clone();
        tampered.entries[0].display = Some("eve".to_string());
        assert!(
            verify_referral_catalog(&tampered, f.owner, subject, &no_revocations(), 0).is_err()
        );
    }

    #[test]
    fn verify_referral_catalog_rejects_expired_cert() {
        use harmony_owner::certs::EnrollmentCert;
        use harmony_owner::pubkey_bundle::{ClassicalKeys, PubKeyBundle};

        // Build an owner with a cert that expires at 2_000, issued at 1_000.
        let seed = 0x15u8;
        let master_sk = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]);
        let master_bundle = PubKeyBundle {
            classical: ClassicalKeys {
                ed25519_verify: master_sk.verifying_key().to_bytes(),
                x25519_pub: [0u8; 32],
            },
            post_quantum: None,
        };
        let owner_id = master_bundle.identity_hash();
        let device_sk = ed25519_dalek::SigningKey::from_bytes(&[seed ^ 0xFF; 32]);
        let device_bundle = PubKeyBundle {
            classical: ClassicalKeys {
                ed25519_verify: device_sk.verifying_key().to_bytes(),
                x25519_pub: [0u8; 32],
            },
            post_quantum: None,
        };
        let device_id = device_bundle.identity_hash();
        let cert = EnrollmentCert::sign_master(
            &master_sk,
            master_bundle,
            device_id,
            device_bundle,
            1_000,
            Some(2_000),
        )
        .expect("sign_master");
        let author = OwnerAddr(owner_id);
        let subject = OwnerAddr([0x22; 16]);

        // Sign a catalog using this cert (valid structure, expires_at = 2_000).
        let cat = sign_referral_catalog(&device_sk, author, subject, vec![], hlc(1_000), cert);

        // Expired: now_ms = 2_001 > expires_at = 2_000 → Err(Auth).
        assert!(
            matches!(
                verify_referral_catalog(&cat, author, subject, &no_revocations(), 2_001),
                Err(ReferralAuthError::Auth)
            ),
            "a catalog with an expired cert must be rejected"
        );
        // Before expiry (now_ms = 1_500): must succeed.
        assert!(
            verify_referral_catalog(&cat, author, subject, &no_revocations(), 1_500).is_ok(),
            "a catalog with a non-expired cert must be accepted"
        );
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
        assert!(verify_referral_catalog(&cat, f.owner, subject, &no_revocations(), 0).is_err());
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
        assert!(authenticate_catalog_request(&req, f_owner, &no_revocations(), 0).is_err());
    }

    #[test]
    fn catalog_request_auth_enforces_to_addr_and_sig() {
        let r = mint_test_owner(0x21);
        let f_owner = OwnerAddr([0x42; 16]);
        let req = sign_catalog_request(&r.device_key, r.owner, f_owner, r.cert.clone());

        // Happy path: addressed to f_owner, valid sig.
        assert!(authenticate_catalog_request(&req, f_owner, &no_revocations(), 0).is_ok());

        // Wrong target server → WrongTarget (checked before crypto).
        assert!(
            authenticate_catalog_request(&req, OwnerAddr([0x43; 16]), &no_revocations(), 0)
                .is_err()
        );

        // Flipped signature bit → SignatureInvalid.
        let mut bad = req.clone();
        bad.sig[0] ^= 1;
        assert!(authenticate_catalog_request(&bad, f_owner, &no_revocations(), 0).is_err());
    }

    /// ZEB-680 §1 (T3 regression pin): `authenticate_catalog_request` consults the
    /// revoked-device projection through the inner `verify_enrolled_device`. A
    /// request from a revoked requester fails auth (`DeviceRevoked` maps to
    /// `ReferralAuthError::Auth`); the SAME request with an empty projection
    /// authenticates. The only difference is the projection, so the rejection can
    /// only come from the revocation consult — pinning the per-site enforcement.
    #[test]
    fn authenticate_catalog_request_rejects_revoked_requester() {
        let r = mint_test_owner(0x21);
        let f_owner = OwnerAddr([0x42; 16]);
        let req = sign_catalog_request(&r.device_key, r.owner, f_owner, r.cert.clone());
        // Empty projection revokes nothing: authenticates.
        assert!(authenticate_catalog_request(&req, f_owner, &no_revocations(), 0).is_ok());
        // Seed the requester's enrolled device key against its own owner.
        let revoked = crate::revoked_device_projection::RevokedDeviceProjection::new();
        let keys: std::collections::BTreeSet<[u8; 32]> =
            std::iter::once(r.cert.device_pubkeys.classical.ed25519_verify).collect();
        revoked.union_from_members(std::iter::once((r.owner, &keys)));
        let err = authenticate_catalog_request(&req, f_owner, &revoked, 0).unwrap_err();
        assert!(
            matches!(err, ReferralAuthError::Auth),
            "expected Auth (from DeviceRevoked), got {err:?}"
        );
    }

    /// ZEB-680 §1 (T3 regression pin): `verify_referral_catalog` — the separable
    /// author-verify seam (referral_catalog.rs :329) — consults the projection for
    /// the catalog AUTHOR's cert. Seeding the author's device key rejects the
    /// catalog; the same catalog with an empty projection verifies.
    #[test]
    fn verify_referral_catalog_rejects_revoked_author() {
        let f = mint_test_owner(0x11);
        let subject = OwnerAddr([0x22; 16]);
        let cat = sign_referral_catalog(
            &f.device_key,
            f.owner,
            subject,
            one_entry(),
            hlc(7),
            f.cert.clone(),
        );
        // Empty projection revokes nothing: verifies.
        assert!(verify_referral_catalog(&cat, f.owner, subject, &no_revocations(), 0).is_ok());
        // Seed the AUTHOR's enrolled device key against the author owner.
        let revoked = crate::revoked_device_projection::RevokedDeviceProjection::new();
        let keys: std::collections::BTreeSet<[u8; 32]> =
            std::iter::once(f.cert.device_pubkeys.classical.ed25519_verify).collect();
        revoked.union_from_members(std::iter::once((f.owner, &keys)));
        let err = verify_referral_catalog(&cat, f.owner, subject, &revoked, 0).unwrap_err();
        assert!(
            matches!(err, ReferralAuthError::Auth),
            "expected Auth (from DeviceRevoked), got {err:?}"
        );
    }

    /// Build a FULL valid `FriendEntry` with deterministic field values, varying
    /// only the lifecycle/opt-in/label the referral-collection logic keys on.
    fn entry(status: FriendStatus, referrable: bool, display: Option<&str>) -> FriendEntry {
        FriendEntry {
            master_ed25519: [0x11; 32],
            display: display.map(str::to_string),
            status,
            established_via: FriendOrigin::Token,
            referrable,
            learned_at: hlc(1),
            sealed_secret: None,
        }
    }

    #[test]
    fn collect_referrable_includes_only_active_referrable() {
        let mut fg = FriendGraph::default();
        fg.friends.insert(
            OwnerAddr([1; 16]),
            entry(FriendStatus::Active, true, Some("yes")),
        );
        fg.friends.insert(
            OwnerAddr([2; 16]),
            entry(FriendStatus::Active, false, Some("not-referrable")),
        );
        fg.friends.insert(
            OwnerAddr([3; 16]),
            entry(FriendStatus::Pending, true, Some("pending")),
        );
        fg.friends.insert(
            OwnerAddr([4; 16]),
            entry(FriendStatus::Revoked, true, Some("revoked")),
        );

        let out = collect_referrable_entries(&fg);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].peer_owner, OwnerAddr([1; 16]));
    }

    #[test]
    fn collect_referrable_is_capped() {
        let mut fg = FriendGraph::default();
        for i in 0..(MAX_REFERRAL_ENTRIES + 5) {
            // Distinct 16-byte keys (more than 256 entries → use 2 bytes of index).
            let mut key = [0u8; 16];
            key[0] = (i & 0xff) as u8;
            key[1] = (i >> 8) as u8;
            fg.friends
                .insert(OwnerAddr(key), entry(FriendStatus::Active, true, None));
        }
        assert_eq!(collect_referrable_entries(&fg).len(), MAX_REFERRAL_ENTRIES);
    }

    #[test]
    fn built_catalog_is_signed_for_subject() {
        let f = mint_test_owner(0x11);
        let subject = OwnerAddr([0x22; 16]);
        let mut fg = FriendGraph::default();
        fg.friends.insert(
            OwnerAddr([1; 16]),
            entry(FriendStatus::Active, true, Some("friend")),
        );

        let cat =
            build_referral_catalog(&fg, subject, f.owner, f.cert.clone(), &f.device_key, hlc(7));

        assert_eq!(cat.entries.len(), 1);
        assert!(verify_referral_catalog(&cat, f.owner, subject, &no_revocations(), 0).is_ok());
    }

    #[test]
    fn project_referrals_marks_already_friends() {
        // Our local graph already has [1;16] as an Active friend.
        let mut fg = FriendGraph::default();
        fg.friends.insert(
            OwnerAddr([1; 16]),
            entry(FriendStatus::Active, false, Some("local-known")),
        );

        // The friend's catalog offers two entries: one we already know, one new.
        let cat = ReferralCatalog {
            author: OwnerAddr([0xaa; 16]),
            entries: vec![
                ReferralEntry {
                    peer_owner: OwnerAddr([1; 16]),
                    display: Some("known".to_string()),
                },
                ReferralEntry {
                    peer_owner: OwnerAddr([2; 16]),
                    display: Some("new".to_string()),
                },
            ],
            at: hlc(7),
            enrollment: mint_test_owner(0x42).cert,
            signer_certs: Vec::new(),
            sig: [9u8; 64],
        };

        let views = project_referrals(&cat, &fg);
        assert_eq!(views.len(), 2);
        assert!(views[0].already_friend);
        assert!(!views[1].already_friend);
        assert_eq!(views[0].owner_id_hex, hex::encode([1u8; 16]));
    }
}
