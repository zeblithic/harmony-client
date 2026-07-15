//! ZEB-376 (Friends Phase 2b): active-introduction wire types + codecs for the
//! `harmony/friend-pex/v1` sub-protocol. Same strict-CBOR / single-char-key /
//! device-#2-signed / address-bound discipline as `referral_catalog.rs` (2a);
//! the two sub-protocols share one wire codec (`referral_catalog::decode_strict`,
//! `PEX_MAX_PACKET_LEN`) so their framing can never diverge.

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::iroh_friend_acceptor::verify_enrolled_device;
use crate::owner_state_types::{
    deserialize_bytes_from_bstr, serialize_bytes_as_bstr, Hlc, OwnerAddr,
};
use crate::reachability_record::ReachabilityAnnouncePayload;
use crate::referral_catalog::{decode_catalog_request, decode_strict, ReferralCodecError};
use harmony_owner::certs::EnrollmentCert;

/// Failure modes when authenticating an [`IntroduceRequest`] (on F) or verifying
/// an [`Introduction`] (on X). Target/identity checks are reported BEFORE
/// cert/signature checks so a mis-addressed message is rejected without spending
/// a signature verification (mirrors `referral_catalog::ReferralAuthError`).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum IntroAuthError {
    /// `to_addr` did not match this node's own owner address (re-aim guard).
    #[error("intro message addressed to a different owner")]
    WrongTarget,
    /// The voucher on an [`Introduction`] was not the voucher we expected.
    #[error("introduction voucher mismatch")]
    VoucherMismatch,
    /// The requested `target`/`subject` did not match the expectation.
    #[error("introduction subject/target mismatch")]
    SubjectMismatch,
    /// An enrollment cert failed `verify_enrolled_device`.
    #[error("intro enrollment cert authentication failed")]
    Auth,
    /// The device-#2 signature did not verify over the canonical preimage.
    #[error("intro signature invalid")]
    SignatureInvalid,
    /// The relayed reachability's inner identity signature / freshness failed.
    #[error("intro reachability record failed verification")]
    ReachabilityInvalid,
}

/// You → F: "introduce me to `target`; here is my device-#2 cert and my current
/// reachability so `target` can dial me." `to_addr` binds the broker (re-aim
/// guard); `sig` is the requester's device-#2 signature over
/// [`introduce_request_sig_preimage`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntroduceRequest {
    #[serde(rename = "a")]
    pub from_addr: OwnerAddr,
    #[serde(rename = "d")]
    pub to_addr: OwnerAddr,
    #[serde(rename = "x")]
    pub target: OwnerAddr,
    #[serde(rename = "r")]
    pub reachability: ReachabilityAnnouncePayload,
    #[serde(rename = "c")]
    pub enrollment: EnrollmentCert,
    #[serde(
        rename = "s",
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub sig: [u8; 64],
    #[serde(rename = "b", default, skip_serializing_if = "Vec::is_empty")]
    pub signer_certs: Vec<EnrollmentCert>,
}

/// Bytes the requester's device-#2 key signs for an [`IntroduceRequest`].
/// `"hir1"` domain tag + requester + broker + target + the full reachability
/// (binding `to_addr` blocks re-aiming; binding `target` blocks swapping whom we
/// asked to meet; folding the reachability blocks substituting a dial target).
pub fn introduce_request_sig_preimage(
    from_addr: OwnerAddr,
    to_addr: OwnerAddr,
    target: OwnerAddr,
    reachability: &ReachabilityAnnouncePayload,
) -> Vec<u8> {
    #[derive(Serialize)]
    struct P<'a> {
        d: &'static str,
        a: OwnerAddr,
        t: OwnerAddr,
        x: OwnerAddr,
        r: &'a ReachabilityAnnouncePayload,
    }
    let mut out = Vec::new();
    ciborium::into_writer(
        &P {
            d: "hir1",
            a: from_addr,
            t: to_addr,
            x: target,
            r: reachability,
        },
        &mut out,
    )
    .expect("fixed-shape encode is infallible");
    out
}

pub fn sign_introduce_request(
    device2: &SigningKey,
    from_addr: OwnerAddr,
    to_addr: OwnerAddr,
    target: OwnerAddr,
    reachability: ReachabilityAnnouncePayload,
    enrollment: EnrollmentCert,
) -> IntroduceRequest {
    let preimage = introduce_request_sig_preimage(from_addr, to_addr, target, &reachability);
    let sig = device2.sign(&preimage).to_bytes();
    IntroduceRequest {
        from_addr,
        to_addr,
        target,
        reachability,
        enrollment,
        sig,
        signer_certs: Vec::new(),
    }
}

/// Authenticate an inbound [`IntroduceRequest`] against F's own owner. Order is
/// security-load-bearing: `to_addr` → cert → signature (mirrors
/// `authenticate_catalog_request`). Does NOT check authorization (that `target`
/// is an Active+referrable friend) — the caller does that against its graph.
pub fn authenticate_introduce_request(
    req: &IntroduceRequest,
    self_owner: OwnerAddr,
    now_secs: u64,
) -> Result<(), IntroAuthError> {
    if req.to_addr != self_owner {
        return Err(IntroAuthError::WrongTarget);
    }
    let verified =
        verify_enrolled_device(&req.enrollment, &req.signer_certs, req.from_addr, now_secs)
            .map_err(|_| IntroAuthError::Auth)?;
    let vk = VerifyingKey::from_bytes(&verified.device_ed25519)
        .map_err(|_| IntroAuthError::SignatureInvalid)?;
    let preimage =
        introduce_request_sig_preimage(req.from_addr, req.to_addr, req.target, &req.reachability);
    vk.verify_strict(&preimage, &Signature::from_bytes(&req.sig))
        .map_err(|_| IntroAuthError::SignatureInvalid)?;
    Ok(())
}

pub fn encode_introduce_request(req: &IntroduceRequest) -> Result<Vec<u8>, ReferralCodecError> {
    let mut out = Vec::new();
    ciborium::into_writer(req, &mut out).map_err(|e| ReferralCodecError::Encode(e.to_string()))?;
    Ok(out)
}

pub fn decode_introduce_request(bytes: &[u8]) -> Result<IntroduceRequest, ReferralCodecError> {
    decode_strict(bytes)
}

/// F → X: a signed vouch. F's `sig` covers the subject's cert + reachability, so
/// X can trust "F vouches this subject, reachable here, asked to meet me" — F
/// cannot forge the subject (their Master-issued cert rides inside; F only
/// relays it). `to_addr` binds X (re-aim guard).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Introduction {
    #[serde(rename = "v")]
    pub voucher: OwnerAddr,
    #[serde(rename = "d")]
    pub to_addr: OwnerAddr,
    #[serde(rename = "u")]
    pub subject: OwnerAddr,
    #[serde(rename = "c")]
    pub subject_cert: EnrollmentCert,
    #[serde(rename = "r")]
    pub reachability: ReachabilityAnnouncePayload,
    #[serde(rename = "t")]
    pub at: Hlc,
    #[serde(
        rename = "s",
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub sig: [u8; 64],
    #[serde(rename = "e")]
    pub voucher_enrollment: EnrollmentCert,
    #[serde(rename = "b", default, skip_serializing_if = "Vec::is_empty")]
    pub signer_certs: Vec<EnrollmentCert>,
}

/// Bytes F's device-#2 key signs for an [`Introduction`]. `"hin1"` domain tag +
/// voucher + target(X) + subject + subject's cert + reachability + clock.
pub fn introduction_sig_preimage(
    voucher: OwnerAddr,
    to_addr: OwnerAddr,
    subject: OwnerAddr,
    subject_cert: &EnrollmentCert,
    reachability: &ReachabilityAnnouncePayload,
    at: &Hlc,
) -> Vec<u8> {
    #[derive(Serialize)]
    struct P<'a> {
        d: &'static str,
        v: OwnerAddr,
        t: OwnerAddr,
        u: OwnerAddr,
        c: &'a EnrollmentCert,
        r: &'a ReachabilityAnnouncePayload,
        h: &'a Hlc,
    }
    let mut out = Vec::new();
    ciborium::into_writer(
        &P {
            d: "hin1",
            v: voucher,
            t: to_addr,
            u: subject,
            c: subject_cert,
            r: reachability,
            h: at,
        },
        &mut out,
    )
    .expect("fixed-shape encode is infallible");
    out
}

#[allow(clippy::too_many_arguments)]
pub fn sign_introduction(
    device2: &SigningKey,
    voucher: OwnerAddr,
    to_addr: OwnerAddr,
    subject: OwnerAddr,
    subject_cert: EnrollmentCert,
    reachability: ReachabilityAnnouncePayload,
    at: Hlc,
    voucher_enrollment: EnrollmentCert,
) -> Introduction {
    let preimage =
        introduction_sig_preimage(voucher, to_addr, subject, &subject_cert, &reachability, &at);
    let sig = device2.sign(&preimage).to_bytes();
    Introduction {
        voucher,
        to_addr,
        subject,
        subject_cert,
        reachability,
        at,
        sig,
        voucher_enrollment,
        signer_certs: Vec::new(),
    }
}

/// Verify an [`Introduction`] on X. Order: `to_addr`(us) → voucher-match →
/// voucher cert+sig → subject cert. Does NOT run the reachability inner check
/// (the caller runs `reachability_record::verify_inner_signature` +
/// freshness, mapping failure to `ReachabilityInvalid`, so it can pass X's own
/// clock/window) nor policy enforcement.
pub fn verify_introduction(
    intro: &Introduction,
    expected_voucher: OwnerAddr,
    expected_target: OwnerAddr,
    now_secs: u64,
) -> Result<(), IntroAuthError> {
    if intro.to_addr != expected_target {
        return Err(IntroAuthError::WrongTarget);
    }
    if intro.voucher != expected_voucher {
        return Err(IntroAuthError::VoucherMismatch);
    }
    let vverified = verify_enrolled_device(
        &intro.voucher_enrollment,
        &intro.signer_certs,
        intro.voucher,
        now_secs,
    )
    .map_err(|_| IntroAuthError::Auth)?;
    let vk = VerifyingKey::from_bytes(&vverified.device_ed25519)
        .map_err(|_| IntroAuthError::SignatureInvalid)?;
    let preimage = introduction_sig_preimage(
        intro.voucher,
        intro.to_addr,
        intro.subject,
        &intro.subject_cert,
        &intro.reachability,
        &intro.at,
    );
    vk.verify_strict(&preimage, &Signature::from_bytes(&intro.sig))
        .map_err(|_| IntroAuthError::SignatureInvalid)?;
    // Bind the subject's cert → subject owner (X pins this into the FriendEntry).
    verify_enrolled_device(&intro.subject_cert, &[], intro.subject, now_secs)
        .map_err(|_| IntroAuthError::Auth)?;
    Ok(())
}

pub fn encode_introduction(intro: &Introduction) -> Result<Vec<u8>, ReferralCodecError> {
    let mut out = Vec::new();
    ciborium::into_writer(intro, &mut out)
        .map_err(|e| ReferralCodecError::Encode(e.to_string()))?;
    Ok(out)
}

pub fn decode_introduction(bytes: &[u8]) -> Result<Introduction, ReferralCodecError> {
    decode_strict(bytes)
}

/// A tagged frame on the friend-PEX ALPN for the 2b introduction directions.
/// Browse (`CatalogRequest`, 2a) stays BARE on the wire — it is NOT a variant
/// here; the acceptor falls back to `decode_catalog_request` when a body does
/// not parse as a `PexFrame`. This keeps every 2a peer working with no flag-day
/// and leaves the `zeb375_pex_fixtures` bytes untouched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PexFrame {
    #[serde(rename = "ir")]
    IntroduceRequest(Box<IntroduceRequest>),
    #[serde(rename = "in")]
    Introduction(Box<Introduction>),
}

/// What `decode_pex_frame_or_catalog` resolved a friend-PEX body to.
///
/// `CatalogRequest` is left unboxed to match the documented `PexDecoded`
/// interface (and the direct, un-dereferenced `PexDecoded::Catalog(g)` match
/// callers use). One `decode_pex_frame_or_catalog` call happens per inbound
/// friend-PEX body — not a hot loop — so the ~400-byte size delta clippy flags
/// here isn't a real perf concern.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum PexDecoded {
    /// A bare 2a `CatalogRequest` (browse). Fallback path.
    Catalog(crate::referral_catalog::CatalogRequest),
    /// A tagged 2b frame.
    Frame(PexFrame),
}

pub fn encode_pex_frame(frame: &PexFrame) -> Result<Vec<u8>, ReferralCodecError> {
    let mut out = Vec::new();
    ciborium::into_writer(frame, &mut out)
        .map_err(|e| ReferralCodecError::Encode(e.to_string()))?;
    Ok(out)
}

/// Try `PexFrame` first (a single-key tagged map); on ANY decode failure, fall
/// back to a bare `CatalogRequest` (a multi-key map that cannot match the
/// single-key enum shape, so the disambiguation is unambiguous). Both attempts
/// use the strict, bounded, trailing-byte-rejecting decoder.
pub fn decode_pex_frame_or_catalog(bytes: &[u8]) -> Result<PexDecoded, ReferralCodecError> {
    match decode_strict::<PexFrame>(bytes) {
        Ok(frame) => Ok(PexDecoded::Frame(frame)),
        Err(_) => Ok(PexDecoded::Catalog(decode_catalog_request(bytes)?)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_membership::mint_test_owner;
    use crate::owner_state_types::OwnerAddr;

    fn reach() -> crate::reachability_record::ReachabilityAnnouncePayload {
        crate::reachability_record::ReachabilityAnnouncePayload {
            iroh_node_id: [0x11; 32],
            home_relay_url: "https://r".into(),
            direct_addresses: vec![],
            announced_at_ms: 1,
            identity_signature: [0x22; 64],
            butler_set: vec![],
            bs_at: 0,
        }
    }

    #[test]
    fn introduce_request_authenticates_and_rejects_reaim() {
        let from = mint_test_owner(0x11);
        let broker = mint_test_owner(0x22);
        let target = OwnerAddr([0x33; 16]);
        let req = sign_introduce_request(
            &from.device_key,
            from.owner,
            broker.owner,
            target,
            reach(),
            from.cert.clone(),
        );
        // Authentic request to the correct broker verifies.
        authenticate_introduce_request(&req, broker.owner, 0).expect("authentic to broker");
        // Re-aimed at a different broker → WrongTarget (before any sig spend).
        assert_eq!(
            authenticate_introduce_request(&req, OwnerAddr([0x99; 16]), 0),
            Err(IntroAuthError::WrongTarget),
        );
    }

    #[test]
    fn introduce_request_rejects_tampered_target() {
        let from = mint_test_owner(0x11);
        let broker = mint_test_owner(0x22);
        let mut req = sign_introduce_request(
            &from.device_key,
            from.owner,
            broker.owner,
            OwnerAddr([0x33; 16]),
            reach(),
            from.cert.clone(),
        );
        req.target = OwnerAddr([0x44; 16]); // swap whom we asked to meet
        assert_eq!(
            authenticate_introduce_request(&req, broker.owner, 0),
            Err(IntroAuthError::SignatureInvalid),
        );
    }

    #[test]
    fn introduction_verifies_and_binds_voucher_and_target() {
        let voucher = mint_test_owner(0x22);
        let subject = mint_test_owner(0x11);
        let target = mint_test_owner(0x33); // X (self)
        let intro = sign_introduction(
            &voucher.device_key,
            voucher.owner,
            target.owner,
            subject.owner,
            subject.cert.clone(),
            reach(),
            hlc(5),
            voucher.cert.clone(),
        );
        // X verifies: voucher == who we think F is, target == us.
        verify_introduction(&intro, voucher.owner, target.owner, 0).expect("authentic");
        // Wrong expected voucher → VoucherMismatch (before sig spend).
        assert_eq!(
            verify_introduction(&intro, OwnerAddr([0x77; 16]), target.owner, 0),
            Err(IntroAuthError::VoucherMismatch),
        );
        // Relayed to the wrong X → WrongTarget.
        assert_eq!(
            verify_introduction(&intro, voucher.owner, OwnerAddr([0x88; 16]), 0),
            Err(IntroAuthError::WrongTarget),
        );
    }

    fn hlc(w: u64) -> Hlc {
        Hlc {
            wall_ms: w,
            logical: 0,
            device_id: "d".into(),
        }
    }

    #[test]
    fn pex_frame_round_trips_and_bare_catalog_falls_back() {
        use crate::referral_catalog::{encode_catalog_request, sign_catalog_request};
        let from = mint_test_owner(0x11);
        let broker = mint_test_owner(0x22);
        // A tagged IntroduceRequest frame decodes as a Frame.
        let ir = sign_introduce_request(
            &from.device_key,
            from.owner,
            broker.owner,
            OwnerAddr([0x33; 16]),
            reach(),
            from.cert.clone(),
        );
        let frame = PexFrame::IntroduceRequest(Box::new(ir.clone()));
        let bytes = encode_pex_frame(&frame).unwrap();
        match decode_pex_frame_or_catalog(&bytes).unwrap() {
            PexDecoded::Frame(PexFrame::IntroduceRequest(g)) => assert_eq!(*g, ir),
            other => panic!("expected IntroduceRequest frame, got {other:?}"),
        }
        // A BARE (2a) CatalogRequest — a 4-key map — falls back to Catalog, never
        // mis-decoding as a single-key frame.
        let cr = sign_catalog_request(
            &from.device_key,
            from.owner,
            broker.owner,
            from.cert.clone(),
        );
        let bare = encode_catalog_request(&cr).unwrap();
        match decode_pex_frame_or_catalog(&bare).unwrap() {
            PexDecoded::Catalog(g) => assert_eq!(g, cr),
            other => panic!("expected Catalog fallback, got {other:?}"),
        }
    }
}
