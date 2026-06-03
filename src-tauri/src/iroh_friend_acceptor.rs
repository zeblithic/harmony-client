//! ZEB-370 Phase 1 (Tasks 7-8): the `harmony/friend/v1` friend-link control
//! protocol — wire types, length-prefixed codec, point-to-point enrolled-device
//! authentication, and the inbound handshake acceptor.
//!
//! ## Identity & auth model (spec §3)
//!
//! A friend link is authenticated by the requester's **device-#2 Ed25519
//! signature** plus their **`EnrollmentCert`** (the ZEB-339 model), applied
//! point-to-point (no `SignedMembershipEvent` wrapper). The verifier:
//!   1. runs `cert.verify()` (master→device chain + `owner_id` binding),
//!   2. requires `cert.issuer == Master` (Quorum certs can't be fully verified
//!      here — mirrors `community_membership::enrolled_key_from_cert`),
//!   3. checks `cert.owner_id == claimed owner_id`, and
//!   4. returns `cert.device_pubkeys.classical.ed25519_verify` (the device key
//!      the handshake signature is verified against).
//!
//! This 4-step core is [`verify_enrolled_device`].
//!
//! Friends are keyed on the master `owner_id`; a friend's `master_ed25519` is
//! extracted from their cert's `EnrollmentIssuer::Master { master_pubkey }`.
//!
//! ## Wire protocol
//!
//! Both directions use `[u32 LE length-prefix][canonical-ish CBOR body]` over an
//! iroh bi-stream on the `harmony/friend/v1` ALPN, mirroring
//! `iroh_invite_acceptor`'s framing. Bodies are encoded with `ciborium`
//! (`into_writer`/`from_reader`); decode bounds the body at
//! [`FRIEND_MAX_PACKET_LEN`].
//!
//! Requester → acceptor: [`FriendLinkRequest`].
//! Acceptor → requester: [`FriendLinkAccepted`].

use crate::owner_state_types::{deserialize_bytes_from_bstr, serialize_bytes_as_bstr, OwnerAddr};
use harmony_owner::certs::{EnrollmentCert, EnrollmentIssuer};
use serde::{Deserialize, Serialize};

/// Maximum bytes the acceptor reads per friend-handshake packet. The wire shape
/// is `[u32 LE length-prefix][body]`; any prefix exceeding this is rejected to
/// defend against memory-exhaustion by an adversarial dialer. 256 KiB matches
/// `iroh_invite_acceptor::HANDSHAKE_MAX_PACKET_LEN` and is far larger than any
/// legitimate request (an `EnrollmentCert` + two `[u8;64]` sigs fit in single-
/// digit KB).
pub const FRIEND_MAX_PACKET_LEN: usize = 256 * 1024;

/// A friend-link request: "I am owner `from_addr`; here is my proof (cert +
/// device-#2 signature) and the friend-token signature I am redeeming; please
/// add me and reply with your own proof."
///
/// `sig` is the requester's device-#2 Ed25519 signature over
/// [`friend_request_sig_preimage`]`(from_addr, token_sig)`. `token_sig` binds
/// the request to a specific minted friend token (the ZEB-367 `InviteToken.sig`
/// the inviter published Case-A), so an acceptor can `unregister_friend_token`
/// the consumed one-shot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FriendLinkRequest {
    /// The requester's master `OwnerAddr` (their `owner_id`). MUST equal
    /// `enrollment.owner_id` (checked by `verify_enrolled_device`).
    pub from_addr: OwnerAddr,
    /// The requester's advertised display name (UX hint). `None` when unset.
    pub display: Option<String>,
    /// The friend-token signature being redeemed (the inviter's published
    /// `InviteToken.sig`). Bound into the request preimage; lets the acceptor
    /// unregister the consumed Case-A one-shot. Stored as a CBOR bstr(64).
    #[serde(
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub token_sig: [u8; 64],
    /// The requester's Master `EnrollmentCert` (their owner→device-#2 binding).
    pub enrollment: EnrollmentCert,
    /// Requester's device-#2 Ed25519 signature over
    /// `friend_request_sig_preimage(from_addr, token_sig)`. Stored as a CBOR
    /// bstr(64).
    #[serde(
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub sig: [u8; 64],
}

/// The acceptor's reply: "accepted; here is my own proof so you can add me back
/// (the mutual link)."
///
/// `sig` is the acceptor's device-#2 Ed25519 signature over
/// [`friend_accept_sig_preimage`]`(from_addr, token_sig)`, where `token_sig` is
/// the same token signature from the originating request (binding the accept to
/// the request it answers).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FriendLinkAccepted {
    /// The accepter's master `OwnerAddr` (their `owner_id`). MUST equal
    /// `enrollment.owner_id`.
    pub from_addr: OwnerAddr,
    /// The accepter's advertised display name (UX hint). `None` when unset.
    pub display: Option<String>,
    /// The accepter's Master `EnrollmentCert`.
    pub enrollment: EnrollmentCert,
    /// Accepter's device-#2 Ed25519 signature over
    /// `friend_accept_sig_preimage(from_addr, token_sig)`. Stored as a CBOR
    /// bstr(64).
    #[serde(
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub sig: [u8; 64],
}

/// Canonical preimage bytes the requester's device-#2 key signs for a
/// [`FriendLinkRequest`]. A small CBOR-encoded tuple `("hfr1", from_addr,
/// token_sig)` — the `"hfr1"` domain tag makes a friend-request signature
/// unmistakable for any other Ed25519 signature this device produces.
pub fn friend_request_sig_preimage(from_addr: OwnerAddr, token_sig: &[u8; 64]) -> Vec<u8> {
    sig_preimage("hfr1", from_addr, token_sig)
}

/// Canonical preimage bytes the accepter's device-#2 key signs for a
/// [`FriendLinkAccepted`]. Domain-separated from the request preimage by the
/// `"hfa1"` tag so a request signature can never be replayed as an accept.
pub fn friend_accept_sig_preimage(from_addr: OwnerAddr, token_sig: &[u8; 64]) -> Vec<u8> {
    sig_preimage("hfa1", from_addr, token_sig)
}

/// Shared preimage builder. The `[u8;64]` is wrapped via `serde_bytes` so it
/// encodes as a CBOR bstr (not a 64-element array), keeping the preimage compact
/// and stable.
fn sig_preimage(domain: &'static str, from_addr: OwnerAddr, token_sig: &[u8; 64]) -> Vec<u8> {
    #[derive(Serialize)]
    struct Preimage<'a> {
        domain: &'a str,
        from_addr: OwnerAddr,
        #[serde(with = "serde_bytes")]
        token_sig: &'a [u8; 64],
    }
    let mut out = Vec::new();
    // Infallible for this fixed-shape value; an encode error would be a logic
    // bug, so surface it loudly rather than silently signing empty bytes.
    ciborium::into_writer(
        &Preimage {
            domain,
            from_addr,
            token_sig,
        },
        &mut out,
    )
    .expect("friend sig preimage always encodes");
    out
}

/// Errors raised while encoding/decoding or authenticating a friend handshake.
#[derive(Debug, thiserror::Error)]
pub enum FriendHandshakeError {
    #[error("CBOR encode failed: {0}")]
    Encode(String),
    #[error("CBOR decode failed: {0}")]
    Decode(String),
    /// The body exceeds [`FRIEND_MAX_PACKET_LEN`]. Bounds work on hostile input.
    #[error("friend packet exceeds size limit: len={len} max={max}")]
    TooLarge { len: usize, max: usize },
    /// `cert.verify()` failed, or the cert's issuer is not `Master`.
    #[error("enrollment cert invalid (verify failed or non-Master issuer)")]
    EnrollmentCertInvalid,
    /// `cert.owner_id` does not equal the claimed owner address.
    #[error("enrollment owner mismatch: cert binds a different owner_id")]
    EnrollmentOwnerMismatch,
    /// The handshake signature did not verify against the enrolled device key.
    #[error("handshake signature invalid")]
    SignatureInvalid,
    /// Applying the resulting `FriendEntry` to the CRDT was rejected (e.g. a
    /// stale HLC or a key↔master-key invariant failure).
    #[error("friend-graph apply rejected: {0}")]
    ApplyRejected(String),
}

/// Encode a [`FriendLinkRequest`] to CBOR bytes (no length prefix). The caller
/// frames it with a `u32 LE` length prefix on the wire.
pub fn encode_friend_request(req: &FriendLinkRequest) -> Result<Vec<u8>, FriendHandshakeError> {
    let mut out = Vec::new();
    ciborium::into_writer(req, &mut out)
        .map_err(|e| FriendHandshakeError::Encode(e.to_string()))?;
    Ok(out)
}

/// Decode a [`FriendLinkRequest`] from CBOR bytes, bounding the input at
/// [`FRIEND_MAX_PACKET_LEN`] before decoding.
pub fn decode_friend_request(bytes: &[u8]) -> Result<FriendLinkRequest, FriendHandshakeError> {
    if bytes.len() > FRIEND_MAX_PACKET_LEN {
        return Err(FriendHandshakeError::TooLarge {
            len: bytes.len(),
            max: FRIEND_MAX_PACKET_LEN,
        });
    }
    ciborium::from_reader(bytes).map_err(|e| FriendHandshakeError::Decode(e.to_string()))
}

/// Encode a [`FriendLinkAccepted`] to CBOR bytes (no length prefix).
pub fn encode_friend_accepted(acc: &FriendLinkAccepted) -> Result<Vec<u8>, FriendHandshakeError> {
    let mut out = Vec::new();
    ciborium::into_writer(acc, &mut out)
        .map_err(|e| FriendHandshakeError::Encode(e.to_string()))?;
    Ok(out)
}

/// Decode a [`FriendLinkAccepted`] from CBOR bytes, bounding the input at
/// [`FRIEND_MAX_PACKET_LEN`] before decoding.
pub fn decode_friend_accepted(bytes: &[u8]) -> Result<FriendLinkAccepted, FriendHandshakeError> {
    if bytes.len() > FRIEND_MAX_PACKET_LEN {
        return Err(FriendHandshakeError::TooLarge {
            len: bytes.len(),
            max: FRIEND_MAX_PACKET_LEN,
        });
    }
    ciborium::from_reader(bytes).map_err(|e| FriendHandshakeError::Decode(e.to_string()))
}

/// Point-to-point enrolled-device authentication: the 4-step core of
/// `community_membership::enrolled_key_from_cert`, applied without the
/// `SignedMembershipEvent` wrapper.
///
/// Verifies `cert`, requires a `Master` issuer, binds `cert.owner_id ==
/// claimed_owner.0`, and returns the enrolled device-#2 Ed25519 verify key the
/// handshake signature must be checked against.
pub fn verify_enrolled_device(
    cert: &EnrollmentCert,
    claimed_owner: OwnerAddr,
) -> Result<[u8; 32], FriendHandshakeError> {
    cert.verify()
        .map_err(|_| FriendHandshakeError::EnrollmentCertInvalid)?;
    // Reject non-Master issuers: cert.verify() only structurally-checks Quorum
    // certs (it cannot verify the quorum signatures without an OwnerState walk-
    // back), so accepting one here would admit unverified signatures. Mirrors
    // enrolled_key_from_cert.
    if !matches!(cert.issuer, EnrollmentIssuer::Master { .. }) {
        return Err(FriendHandshakeError::EnrollmentCertInvalid);
    }
    if cert.owner_id != claimed_owner.0 {
        return Err(FriendHandshakeError::EnrollmentOwnerMismatch);
    }
    Ok(cert.device_pubkeys.classical.ed25519_verify)
}

/// Extract the friend's master Ed25519 verify key from a verified Master
/// `EnrollmentCert`'s issuer. Used to populate `FriendEntry.master_ed25519` (the
/// friend-graph key anchor). Returns `EnrollmentCertInvalid` if the issuer is
/// not `Master` — callers always run `verify_enrolled_device` first, so this is
/// belt-and-suspenders.
pub fn master_ed25519_from_cert(cert: &EnrollmentCert) -> Result<[u8; 32], FriendHandshakeError> {
    match &cert.issuer {
        EnrollmentIssuer::Master { master_pubkey } => Ok(master_pubkey.classical.ed25519_verify),
        _ => Err(FriendHandshakeError::EnrollmentCertInvalid),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_membership::mint_test_owner;
    use ed25519_dalek::Signer;

    /// Build a signed, well-formed `FriendLinkRequest` from a test owner.
    fn signed_request(owner_seed: u8, token_sig: [u8; 64]) -> (FriendLinkRequest, [u8; 32]) {
        let owner = mint_test_owner(owner_seed);
        let device_key = owner.cert.device_pubkeys.classical.ed25519_verify;
        let preimage = friend_request_sig_preimage(owner.owner, &token_sig);
        let sig = owner.device_key.sign(&preimage).to_bytes();
        let req = FriendLinkRequest {
            from_addr: owner.owner,
            display: Some("alice".into()),
            token_sig,
            enrollment: owner.cert,
            sig,
        };
        (req, device_key)
    }

    #[test]
    fn friend_request_round_trips() {
        let (req, _) = signed_request(0x21, [9u8; 64]);
        let bytes = encode_friend_request(&req).expect("encode");
        let back = decode_friend_request(&bytes).expect("decode");
        assert_eq!(req, back);
    }

    #[test]
    fn friend_accepted_round_trips() {
        let owner = mint_test_owner(0x22);
        let acc = FriendLinkAccepted {
            from_addr: owner.owner,
            display: None,
            enrollment: owner.cert,
            sig: [4u8; 64],
        };
        let bytes = encode_friend_accepted(&acc).expect("encode");
        let back = decode_friend_accepted(&bytes).expect("decode");
        assert_eq!(acc, back);
    }

    #[test]
    fn decode_rejects_oversized_request() {
        let huge = vec![0u8; FRIEND_MAX_PACKET_LEN + 1];
        match decode_friend_request(&huge) {
            Err(FriendHandshakeError::TooLarge { .. }) => {}
            other => panic!("expected TooLarge, got {other:?}"),
        }
        match decode_friend_accepted(&huge) {
            Err(FriendHandshakeError::TooLarge { .. }) => {}
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn verify_enrolled_device_accepts_valid_cert() {
        let owner = mint_test_owner(0x31);
        let device_key = verify_enrolled_device(&owner.cert, owner.owner).expect("valid");
        assert_eq!(
            device_key,
            owner.cert.device_pubkeys.classical.ed25519_verify
        );
    }

    #[test]
    fn verify_enrolled_device_rejects_wrong_owner() {
        let owner = mint_test_owner(0x32);
        let other = mint_test_owner(0x33);
        // Cert is owner's, but we claim it belongs to `other` → owner mismatch.
        match verify_enrolled_device(&owner.cert, other.owner) {
            Err(FriendHandshakeError::EnrollmentOwnerMismatch) => {}
            other => panic!("expected EnrollmentOwnerMismatch, got {other:?}"),
        }
    }

    #[test]
    fn verify_enrolled_device_rejects_tampered_cert() {
        let owner = mint_test_owner(0x34);
        let mut cert = owner.cert.clone();
        // Structurally tamper: flip issued_at so the master signature no longer
        // covers the payload → cert.verify() fails.
        cert.issued_at ^= 0xFFFF;
        match verify_enrolled_device(&cert, owner.owner) {
            Err(FriendHandshakeError::EnrollmentCertInvalid) => {}
            other => panic!("expected EnrollmentCertInvalid, got {other:?}"),
        }
    }

    #[test]
    fn verify_enrolled_device_rejects_non_master_issuer() {
        let owner = mint_test_owner(0x35);
        let mut cert = owner.cert.clone();
        // Swap a Quorum issuer in. cert.verify() will structurally pass the
        // device-id check but verify_enrolled_device must reject the non-Master
        // issuer before trusting it.
        cert.issuer = EnrollmentIssuer::Quorum {
            signers: vec![[1u8; 16], [2u8; 16]],
            signatures: vec![vec![0u8; 64], vec![0u8; 64]],
        };
        match verify_enrolled_device(&cert, owner.owner) {
            Err(FriendHandshakeError::EnrollmentCertInvalid) => {}
            other => panic!("expected EnrollmentCertInvalid, got {other:?}"),
        }
    }

    #[test]
    fn request_signature_verifies_against_enrolled_key_and_tamper_fails() {
        use ed25519_dalek::{Signature, VerifyingKey};
        let token_sig = [7u8; 64];
        let (req, device_key) = signed_request(0x36, token_sig);

        // The enrolled device key resolved from the cert must verify the sig
        // over the request preimage.
        let resolved = verify_enrolled_device(&req.enrollment, req.from_addr).expect("valid cert");
        assert_eq!(resolved, device_key);
        let vk = VerifyingKey::from_bytes(&resolved).expect("vk");
        let preimage = friend_request_sig_preimage(req.from_addr, &req.token_sig);
        vk.verify_strict(&preimage, &Signature::from_bytes(&req.sig))
            .expect("untampered sig verifies");

        // A tampered sig (or a preimage over a different token_sig) must fail.
        let bad_preimage = friend_request_sig_preimage(req.from_addr, &[0u8; 64]);
        assert!(vk
            .verify_strict(&bad_preimage, &Signature::from_bytes(&req.sig))
            .is_err());
    }

    #[test]
    fn master_ed25519_from_cert_matches_owner_id() {
        let owner = mint_test_owner(0x37);
        let master = master_ed25519_from_cert(&owner.cert).expect("master cert");
        // The friend-graph key invariant: owner_id derived from this master key
        // equals the cert's owner_id.
        assert_eq!(
            crate::friend_graph::owner_id_from_master_ed25519(&master),
            owner.owner
        );
    }
}
