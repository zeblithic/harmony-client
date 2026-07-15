//! ZEB-376 (Friends Phase 2b): active-introduction wire types + codecs for the
//! `harmony/friend-pex/v1` sub-protocol. Same strict-CBOR / single-char-key /
//! device-#2-signed / address-bound discipline as `referral_catalog.rs` (2a);
//! the two sub-protocols share one wire codec (`referral_catalog::decode_strict`,
//! `PEX_MAX_PACKET_LEN`) so their framing can never diverge.

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::iroh_friend_acceptor::verify_enrolled_device;
use crate::owner_state_types::{deserialize_bytes_from_bstr, serialize_bytes_as_bstr, OwnerAddr};
use crate::reachability_record::ReachabilityAnnouncePayload;
use crate::referral_catalog::{decode_strict, ReferralCodecError};
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
}
