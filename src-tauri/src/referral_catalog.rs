//! ZEB-375 (Friends Phase 2a): referral-catalog wire types + codecs for the
//! `harmony/friend-pex/v1` awareness sub-protocol. Strict CBOR, single-char map
//! keys, bounded decode with trailing-byte rejection — mirrors
//! `iroh_friend_acceptor`'s handshake codec so the two sub-protocols share
//! one wire discipline.

use serde::{Deserialize, Serialize};

use crate::friend_graph::deserialize_capped_display;
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
}
