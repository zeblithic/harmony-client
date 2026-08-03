//! ZEB-376 (Friends Phase 2b): active-introduction wire types + codecs for the
//! `harmony/friend-pex/v1` sub-protocol. Same strict-CBOR / single-char-key /
//! device-#2-signed / address-bound discipline as `referral_catalog.rs` (2a);
//! the two sub-protocols share one wire codec (`referral_catalog::decode_strict`,
//! `PEX_MAX_PACKET_LEN`) so their framing can never diverge.

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::sync::Mutex;

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
    /// ZEB-376 (#4, defense-in-depth): the signed `at` envelope timestamp is
    /// implausibly old (older than [`INTRODUCTION_MAX_ENVELOPE_AGE_MS`]) or set
    /// too far in the future (beyond [`INTRODUCTION_MAX_FORWARD_SKEW_MS`]).
    #[error("intro envelope timestamp outside the freshness window")]
    Stale,
}

/// ZEB-376 (#4): max age of an [`Introduction`]'s signed `at` timestamp before X
/// rejects the envelope as stale. Reuses the reachability record TTL (7 days) —
/// an introduction older than the reachability it carries could never dial
/// anyway, so the same generous ceiling is the natural bound.
pub const INTRODUCTION_MAX_ENVELOPE_AGE_MS: u64 =
    crate::reachability_record::REACHABILITY_RECORD_TTL_MS;
/// ZEB-376 (#4): max forward clock skew tolerated on an [`Introduction`]'s `at`.
/// A voucher whose clock runs slightly ahead of X's is fine; one set far in the
/// future (a fabricated-freshness attempt) is rejected.
pub const INTRODUCTION_MAX_FORWARD_SKEW_MS: u64 = 30 * 60 * 1000; // 30 min

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
    // ZEB-680 §1: consulted (via the inner `verify_enrolled_device`) against the
    // requester's owner + verified device key.
    revoked: &crate::revoked_device_projection::RevokedDeviceProjection,
    now_secs: u64,
) -> Result<(), IntroAuthError> {
    if req.to_addr != self_owner {
        return Err(IntroAuthError::WrongTarget);
    }
    let verified = verify_enrolled_device(
        &req.enrollment,
        &req.signer_certs,
        req.from_addr,
        revoked,
        now_secs,
    )
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
    /// ZEB-376 (Q2): the SUBJECT's signer-cert bundle, forwarded from the
    /// requester's `IntroduceRequest.signer_certs` so a quorum-issued
    /// `subject_cert` can be verified on X (an empty bundle fails a quorum cert).
    /// This is VERIFICATION CONTEXT, exactly like `signer_certs` for the voucher
    /// cert — it is deliberately NOT part of `introduction_sig_preimage` (F does
    /// not sign it; it is F relaying the subject's own Master-issued chain). Key
    /// "g"; empty (the common master-issued-subject case) omits it from the wire.
    #[serde(rename = "g", default, skip_serializing_if = "Vec::is_empty")]
    pub subject_signer_certs: Vec<EnrollmentCert>,
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
    subject_signer_certs: Vec<EnrollmentCert>,
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
        subject_signer_certs,
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
    // ZEB-680 §1: consulted for BOTH carried certs — the voucher (a friend whose
    // revocations we likely know; the security-relevant check) and the subject
    // (opportunistic — the subject's owner may be a stranger). Each consult binds
    // the projection against that cert's own owner.
    revoked: &crate::revoked_device_projection::RevokedDeviceProjection,
    now_secs: u64,
) -> Result<(), IntroAuthError> {
    if intro.to_addr != expected_target {
        return Err(IntroAuthError::WrongTarget);
    }
    if intro.voucher != expected_voucher {
        return Err(IntroAuthError::VoucherMismatch);
    }
    // ZEB-376 (#4, defense-in-depth): bound the signature-covered `at` envelope
    // timestamp against X's own wall clock. Replay is already bounded (the pre-auth
    // is one-shot + TTL, and the relayed reachability is freshness-checked by the
    // caller), so this is a cheap extra guard, not the primary defense: reject an
    // envelope stamped implausibly far in the past or future. `now_secs` is in
    // seconds; `at.wall_ms` in epoch-ms — compare in ms. Checked before the sig
    // spend (an out-of-window `at` is rejected regardless of its signature).
    let now_ms = now_secs.saturating_mul(1000);
    if now_ms.saturating_sub(intro.at.wall_ms) > INTRODUCTION_MAX_ENVELOPE_AGE_MS
        || intro.at.wall_ms.saturating_sub(now_ms) > INTRODUCTION_MAX_FORWARD_SKEW_MS
    {
        return Err(IntroAuthError::Stale);
    }
    let vverified = verify_enrolled_device(
        &intro.voucher_enrollment,
        &intro.signer_certs,
        intro.voucher,
        revoked,
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
    // ZEB-376 (Q2): pass the FORWARDED subject signer bundle (not `&[]`) so a
    // quorum-issued subject cert can recover its master anchor + verify its part
    // signatures; a master-issued subject cert verifies with an empty bundle.
    verify_enrolled_device(
        &intro.subject_cert,
        &intro.subject_signer_certs,
        intro.subject,
        revoked,
        now_secs,
    )
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

/// F's broker error: either the inbound [`IntroduceRequest`] failed
/// authentication, or the requested `target` is not an Active + referrable
/// friend of the broker — in which case F relays NOTHING (no envelope leaks a
/// non-opted-in friend). Mirrors `referral_catalog`'s split of auth vs
/// authorization failure.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum IntroBrokerError {
    #[error("introduce request authentication failed: {0}")]
    Auth(#[from] IntroAuthError),
    /// ZEB-376 (Q1): the REQUESTER is not an Active friend of the broker. An
    /// authenticated non-friend must not be able to make F sign/relay an
    /// `Introduction` vouching for them — F relays nothing (mirrors the catalog
    /// path's requester friend-gate in `serve_catalog_for_request`). Like
    /// `NotReferrable`, this maps to the acceptor's benign-ack arm, so it is
    /// network-indistinguishable from any other broker decline (no oracle).
    #[error("requester is not an active friend")]
    RequesterNotFriend,
    /// The requested target is not an Active + referrable friend of the broker —
    /// F relays nothing (no leak of a non-opted-in friend).
    #[error("target is not an active referrable friend")]
    NotReferrable,
}

/// F's PURE broker decision: authenticate the request, require `target` is an
/// Active + `referrable` friend, then build+sign an [`Introduction`] that relays
/// the subject (requester) + their cert + their reachability, vouched by F, aimed
/// at the target. Read-only over `fg`. Mirrors `serve_catalog_for_request`.
#[allow(clippy::too_many_arguments)]
pub fn build_introduction_for_request(
    req: &IntroduceRequest,
    fg: &crate::friend_graph::FriendGraph,
    self_owner: OwnerAddr,
    self_enrollment: EnrollmentCert,
    device2: &SigningKey,
    at: Hlc,
    // ZEB-680 §1: threaded to the inner `authenticate_introduce_request` so F
    // refuses to vouch for a revoked requester.
    revoked: &crate::revoked_device_projection::RevokedDeviceProjection,
    now_secs: u64,
) -> Result<Introduction, IntroBrokerError> {
    authenticate_introduce_request(req, self_owner, revoked, now_secs)?;
    // ZEB-376 (Q1): friend-gate the REQUESTER. Authentication only proves the
    // requester controls `from_addr`; it does NOT prove they are F's friend.
    // Without this an authenticated non-friend could make F vouch for them.
    // Mirror `serve_catalog_for_request`'s requester gate: require an Active
    // friend entry keyed on `req.from_addr`.
    let requester_is_friend = fg
        .friends
        .get(&req.from_addr)
        .is_some_and(|e| e.status == crate::friend_graph::FriendStatus::Active);
    if !requester_is_friend {
        return Err(IntroBrokerError::RequesterNotFriend);
    }
    let referrable = fg
        .friends
        .get(&req.target)
        .is_some_and(|e| e.status == crate::friend_graph::FriendStatus::Active && e.referrable);
    if !referrable {
        return Err(IntroBrokerError::NotReferrable);
    }
    Ok(sign_introduction(
        device2,
        self_owner,
        req.target,
        req.from_addr,
        req.enrollment.clone(),
        req.reachability.clone(),
        at,
        self_enrollment,
        // ZEB-376 (Q2): forward the subject's (requester's) signer bundle so a
        // quorum-issued subject cert verifies on X. F relays it as-is; F does not
        // sign it (not in the preimage).
        req.signer_certs.clone(),
    ))
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

/// Outcome of X's `PeerIntroPolicy` decision for an inbound `Introduction`.
/// Pure; no I/O — mirrors `iroh_friend_acceptor::ConsentDecision`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntroDecision {
    /// Form the link now (X dials the introducee).
    Proceed,
    /// Stage an introduction-offer in the pending inbox; proceed only on the
    /// user's explicit accept (`AskMe`).
    Stage,
    /// Reject (relay a benign "declined" to the requester).
    Reject,
}

/// Enforce `PeerIntroPolicy` on X for an inbound `Introduction`.
/// `voucher_is_active_friend` = the voucher (F) is currently an `Active` friend
/// of X (already established by `verify_introduction` + a graph check at the
/// call site). Authentication ALWAYS runs before this — policy only gates
/// whether to proceed/prompt/reject, never whether to authenticate.
pub fn decide_introduction(
    policy: crate::friend_graph::PeerIntroPolicy,
    voucher_is_active_friend: bool,
) -> IntroDecision {
    use crate::friend_graph::PeerIntroPolicy::*;
    match policy {
        Open => IntroDecision::Proceed,
        FriendsOfFriends if voucher_is_active_friend => IntroDecision::Proceed,
        FriendsOfFriends => IntroDecision::Reject,
        AskMe => IntroDecision::Stage,
        Closed => IntroDecision::Reject,
    }
}

/// The canonical HLC both the introducee's reachability signer and X's
/// verifier use for a relayed introduction reachability's inner signature.
/// The reachability's real HLC never travels on the wire (the payload doesn't
/// carry it), and CRDT ordering is irrelevant to a one-shot dial hint — so
/// both sides pin a constant. Freshness comes from `announced_at_ms`; F cannot
/// fabricate the payload without the subject's device-#2 key. NOTE: `Hlc` does
/// NOT derive Default, so this must be spelled explicitly.
pub(crate) fn introduction_reachability_hlc() -> crate::owner_state_types::Hlc {
    crate::owner_state_types::Hlc {
        wall_ms: 0,
        logical: 0,
        device_id: String::new(),
    }
}

/// ZEB-376 Task 12: build the SUBJECT's ("your") own current reachability
/// announce for an outbound [`IntroduceRequest`]. A thin wrapper over
/// [`crate::reachability_record::build_signed_payload_with_key`] pinned to the
/// CANONICAL [`introduction_reachability_hlc`] — the exact HLC X's Task-10
/// verifier re-derives — with an EMPTY butler set (`bs_at = 0`); an empty set is
/// acceptable for a first-contact dial target. The HLC is deliberately NOT a
/// parameter: signing with any other clock would make X reject every
/// introduction, so it must not be caller-overridable.
pub(crate) fn build_self_reachability_announce(
    iroh_node_id: [u8; 32],
    home_relay_url: String,
    direct_addresses: Vec<std::net::SocketAddr>,
    announced_at_ms: u64,
    actor: &OwnerAddr,
    device2_signing_key: &SigningKey,
) -> Result<ReachabilityAnnouncePayload, crate::owner_state_crypto::CryptoError> {
    crate::reachability_record::build_signed_payload_with_key(
        iroh_node_id,
        home_relay_url,
        direct_addresses,
        announced_at_ms,
        actor,
        &introduction_reachability_hlc(),
        Vec::new(),
        0,
        device2_signing_key,
    )
}

/// ZEB-376 Task 13 (abuse hygiene): the sliding window over which a single
/// `key` (a voucher on X's arm, or a requester on F's arm) may drive
/// [`INTRO_PER_VOUCHER_MAX`] introductions before being shed.
pub const INTRO_PER_VOUCHER_WINDOW_MS: u64 = 60 * 60 * 1000; // 1h
/// Max introductions a single `key` may drive within
/// [`INTRO_PER_VOUCHER_WINDOW_MS`]; the (`INTRO_PER_VOUCHER_MAX` + 1)-th within
/// the window is shed.
pub const INTRO_PER_VOUCHER_MAX: usize = 20; // per key per window
/// A repeat `(key, subject)` seen within this TTL is shed as a duplicate.
pub const INTRO_DEDUPE_TTL_MS: u64 = 5 * 60 * 1000; // 5 min

/// ZEB-694: pre-auth per-connection-endpoint cap over the same 1h window.
/// Generous vs the per-owner 20 because one iroh endpoint may legitimately
/// host/relay for several owners; a genuine single-endpoint flood is still shed.
pub const INTRO_PER_CONNECTION_MAX: usize = 40;

/// Hard ceiling on tracked dedupe pairs — bounds each per-role dedupe map's OWN
/// memory. The Tier-2 quota methods admit (and record) as soon as a frame's
/// owner is AUTHENTICATED but before it is a known friend, so a flood of frames
/// bearing rotating `(owner, owner)` pairs could otherwise grow these maps
/// unbounded. When a map exceeds this cap, [`KeyedDedupe`] first drops entries
/// already past [`INTRO_DEDUPE_TTL_MS`] (they can no longer shed a duplicate, so
/// removing them changes no decision) and then, if a genuine fresh flood keeps
/// it over, evicts the oldest-timestamped entries down to a low-watermark below
/// the cap. 8192 pairs is well under a MB and orders of magnitude above the
/// volume of legitimate, rare, human-initiated introductions.
const MAX_DEDUPE_ENTRIES: usize = 8192;
/// Hard ceiling on tracked window keys — the counterpart to
/// [`MAX_DEDUPE_ENTRIES`] for every [`KeyedSlidingWindow`] (the pre-auth
/// connection shield keyed on un-spoofable endpoint ids, plus the two per-role
/// owner quotas), with the same stale-then-oldest eviction discipline.
const MAX_WINDOW_KEYS: usize = 8192;

/// A per-key sliding-window counter, bounded to `MAX_WINDOW_KEYS` distinct keys.
/// Extracted from the ZEB-376 `IntroRateLimiter` so both the pre-auth connection
/// shield and the post-auth owner quotas share one audited implementation.
///
/// ZEB-853: `pub(crate)` (with `new`/`admit`) so the open-join limiters in
/// `open_join_admit` reuse this same bounded-eviction primitive rather than
/// re-implementing a per-source window — a keyed limiter MUST be memory-bounded
/// against rotating-key floods, and that discipline lives here.
pub(crate) struct KeyedSlidingWindow<K> {
    max: usize,
    window_ms: u64,
    windows: HashMap<K, VecDeque<u64>>,
}

impl<K: Copy + Eq + Hash> KeyedSlidingWindow<K> {
    pub(crate) fn new(max: usize, window_ms: u64) -> Self {
        Self {
            max,
            window_ms,
            windows: HashMap::new(),
        }
    }

    /// `true` if admitted (recorded), `false` if the key is at its in-window cap.
    pub(crate) fn admit(&mut self, key: K, now_ms: u64) -> bool {
        if self.max == 0 {
            return false; // a zero cap admits nothing; avoid inserting an unbounded empty entry
        }
        {
            let window = self.windows.entry(key).or_default();
            let cutoff = now_ms.saturating_sub(self.window_ms);
            while window.front().is_some_and(|&t| t < cutoff) {
                window.pop_front();
            }
            if window.len() >= self.max {
                return false;
            }
            window.push_back(now_ms);
        }
        self.evict(now_ms);
        true
    }

    fn evict(&mut self, now_ms: u64) {
        if self.windows.len() <= MAX_WINDOW_KEYS {
            return;
        }
        let cutoff = now_ms.saturating_sub(self.window_ms);
        self.windows.retain(|_, dq| {
            while dq.front().is_some_and(|&t| t < cutoff) {
                dq.pop_front();
            }
            !dq.is_empty()
        });
        if self.windows.len() <= MAX_WINDOW_KEYS {
            return;
        }
        let target = MAX_WINDOW_KEYS / 4 * 3;
        let mut recents: Vec<(u64, K)> = self
            .windows
            .iter()
            .map(|(&k, dq)| {
                (
                    *dq.back().expect("deque is non-empty after the stale prune"),
                    k,
                )
            })
            .collect();
        let excess = recents.len() - target;
        recents.select_nth_unstable_by_key(excess, |&(ts, _)| ts);
        for &(_, k) in &recents[..excess] {
            self.windows.remove(&k);
        }
    }
}

/// A per-key "last admitted at" map with a TTL duplicate check, bounded to
/// `MAX_DEDUPE_ENTRIES` distinct keys. Extracted from the ZEB-376 limiter.
struct KeyedDedupe<K> {
    ttl_ms: u64,
    last_seen: HashMap<K, u64>,
}

impl<K: Copy + Eq + Hash> KeyedDedupe<K> {
    fn new(ttl_ms: u64) -> Self {
        Self {
            ttl_ms,
            last_seen: HashMap::new(),
        }
    }

    fn is_duplicate(&self, key: K, now_ms: u64) -> bool {
        self.last_seen
            .get(&key)
            .is_some_and(|&last| now_ms.saturating_sub(last) < self.ttl_ms)
    }

    fn record(&mut self, key: K, now_ms: u64) {
        self.last_seen.insert(key, now_ms);
        self.evict(now_ms);
    }

    fn evict(&mut self, now_ms: u64) {
        if self.last_seen.len() <= MAX_DEDUPE_ENTRIES {
            return;
        }
        self.last_seen
            .retain(|_, &mut ts| now_ms.saturating_sub(ts) < self.ttl_ms);
        if self.last_seen.len() <= MAX_DEDUPE_ENTRIES {
            return;
        }
        let target = MAX_DEDUPE_ENTRIES / 4 * 3;
        let mut stamps: Vec<(u64, K)> = self.last_seen.iter().map(|(&k, &ts)| (ts, k)).collect();
        let excess = stamps.len() - target;
        stamps.select_nth_unstable_by_key(excess, |&(ts, _)| ts);
        for &(_, k) in &stamps[..excess] {
            self.last_seen.remove(&k);
        }
    }
}

/// ZEB-694: two-tier introduction rate limiter.
/// - Tier 1 (`admit_connection`): pre-auth flood shield keyed on the connecting
///   iroh endpoint's authenticated `remote_id()` — un-spoofable, runs before any
///   signature verification.
/// - Tier 2 (`admit_requester` / `admit_voucher`): post-auth per-owner quotas +
///   dedupe, keyed on the AUTHENTICATED owner, in DISJOINT per-role namespaces so
///   requester traffic and voucher traffic never share a budget.
///
/// It is deliberately NOT an authorization decision — it only sheds volume;
/// every shed is LOGGED by the caller ("no silent truncation") and answered with
/// the same benign ack a normal outcome writes, so a shed is
/// network-indistinguishable (no oracle). Guarded by a `std::sync::Mutex`; the
/// admit methods never `.await`, so the lock is never held across a suspension
/// point. Every tracked map is bounded by [`MAX_DEDUPE_ENTRIES`] /
/// [`MAX_WINDOW_KEYS`] with amortized-O(1) eviction (see [`KeyedSlidingWindow`] /
/// [`KeyedDedupe`]), so a flood of rotating keys cannot turn this DoS-hygiene
/// layer into a memory-DoS of its own.
pub struct IntroRateLimiter {
    inner: Mutex<Inner>,
    /// ZEB-711: monotonic epoch for the production `now_ms` feed. The admit
    /// methods keep taking an explicit `now_ms` (the unit-test seam), but
    /// acceptors derive it from [`Self::monotonic_now_ms`] instead of wall
    /// clock — a wall step would otherwise distort enforcement (forward
    /// jump: a flood gets a fresh budget; backward jump: an honest shed
    /// peer stays shed longer). `tokio::time::Instant` also honors the
    /// paused test clock, keeping window mechanics deterministic in tests.
    epoch: tokio::time::Instant,
}

struct Inner {
    /// Tier 1: pre-auth per-connection-endpoint window, keyed on the connecting
    /// iroh endpoint's authenticated `remote_id` (un-spoofable).
    conn: KeyedSlidingWindow<[u8; 32]>,
    /// Tier 2 (requester role): per-authenticated-requester window + dedupe of a
    /// repeated `(requester, target)`.
    req_window: KeyedSlidingWindow<OwnerAddr>,
    req_dedupe: KeyedDedupe<(OwnerAddr, OwnerAddr)>,
    /// Tier 2 (voucher role): per-verified-voucher window + dedupe of a repeated
    /// `(voucher, subject)`. Disjoint from the requester role's maps.
    vouch_window: KeyedSlidingWindow<OwnerAddr>,
    vouch_dedupe: KeyedDedupe<(OwnerAddr, OwnerAddr)>,
}

impl IntroRateLimiter {
    pub fn new() -> Self {
        Self::with_caps(
            INTRO_PER_CONNECTION_MAX,
            INTRO_PER_VOUCHER_MAX,
            INTRO_PER_VOUCHER_WINDOW_MS,
            INTRO_DEDUPE_TTL_MS,
        )
    }

    /// Test/tuning constructor — deterministic tiny caps in unit tests.
    pub fn with_caps(
        conn_max: usize,
        per_owner_max: usize,
        window_ms: u64,
        dedupe_ttl_ms: u64,
    ) -> Self {
        Self {
            inner: Mutex::new(Inner {
                conn: KeyedSlidingWindow::new(conn_max, window_ms),
                req_window: KeyedSlidingWindow::new(per_owner_max, window_ms),
                req_dedupe: KeyedDedupe::new(dedupe_ttl_ms),
                vouch_window: KeyedSlidingWindow::new(per_owner_max, window_ms),
                vouch_dedupe: KeyedDedupe::new(dedupe_ttl_ms),
            }),
            epoch: tokio::time::Instant::now(),
        }
    }

    /// ZEB-711: the production timeline for every admit call on this
    /// limiter — milliseconds since this limiter was constructed, from the
    /// monotonic (and test-pausable) tokio clock. Window state and epoch
    /// live and die together with the limiter instance, so the timeline is
    /// internally consistent by construction. Wall time stays for wall
    /// domains only (HLC stamping, token/cert expiry, recorded-at).
    pub fn monotonic_now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    /// Poison-tolerant lock: a panic elsewhere must not wedge the acceptor — the
    /// guarded state is plain counters, safe to keep using.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Tier 1 — pre-auth. Key = the connecting endpoint's authenticated
    /// `remote_id`. `Ok(())` admits (and records); `Err("per-connection cap")`
    /// sheds without recording. Runs BEFORE any signature verification.
    pub fn admit_connection(&self, remote_id: [u8; 32], now_ms: u64) -> Result<(), &'static str> {
        if self.lock().conn.admit(remote_id, now_ms) {
            Ok(())
        } else {
            Err("per-connection cap")
        }
    }

    /// Tier 2 — post-auth requester quota. Key = the AUTHENTICATED requester.
    /// Sheds `"duplicate"` for a repeat `(requester, target)` within the TTL, or
    /// `"per-requester cap"` at the per-owner window cap; a windowed shed does
    /// NOT record the dedupe stamp.
    pub fn admit_requester(
        &self,
        requester: OwnerAddr,
        target: OwnerAddr,
        now_ms: u64,
    ) -> Result<(), &'static str> {
        let mut inner = self.lock();
        if inner.req_dedupe.is_duplicate((requester, target), now_ms) {
            return Err("duplicate");
        }
        if !inner.req_window.admit(requester, now_ms) {
            return Err("per-requester cap");
        }
        inner.req_dedupe.record((requester, target), now_ms);
        Ok(())
    }

    /// Tier 2 — post-auth voucher quota. Key = the VERIFIED voucher. Sheds
    /// `"duplicate"` for a repeat `(voucher, subject)` within the TTL, or
    /// `"per-voucher cap"` at the per-owner window cap; a windowed shed does NOT
    /// record the dedupe stamp.
    pub fn admit_voucher(
        &self,
        voucher: OwnerAddr,
        subject: OwnerAddr,
        now_ms: u64,
    ) -> Result<(), &'static str> {
        let mut inner = self.lock();
        if inner.vouch_dedupe.is_duplicate((voucher, subject), now_ms) {
            return Err("duplicate");
        }
        if !inner.vouch_window.admit(voucher, now_ms) {
            return Err("per-voucher cap");
        }
        inner.vouch_dedupe.record((voucher, subject), now_ms);
        Ok(())
    }

    /// Test helper: (voucher-role dedupe entries, voucher-role window keys). The
    /// migrated Task-13 flood tests exercise `admit_voucher`, so they assert on
    /// the voucher role's maps. Same-module access to the primitives' private
    /// fields.
    #[cfg(test)]
    pub(crate) fn tracked_len(&self) -> (usize, usize) {
        let inner = self.lock();
        (
            inner.vouch_dedupe.last_seen.len(),
            inner.vouch_window.windows.len(),
        )
    }
}

impl Default for IntroRateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

/// ZEB-700: friend/v1 handshake shield — per-connection-endpoint cap over
/// [`FRIEND_HANDSHAKE_WINDOW_MS`]. Generous vs the per-owner cap for the same
/// reason as [`INTRO_PER_CONNECTION_MAX`] (an endpoint legitimately retries;
/// a genuine single-endpoint flood is still shed).
pub const FRIEND_HANDSHAKE_PER_CONNECTION_MAX: usize = 40;
/// ZEB-700: post-auth per-owner friend-handshake cap. Far above the legit
/// re-dial flows (request → `Pending` → approve → re-dial; `Pending` → token →
/// re-dial ≈ 2-3 dials/h) yet bounds an authenticated flood.
pub const FRIEND_HANDSHAKE_PER_OWNER_MAX: usize = 20;
/// ZEB-700: sliding window shared by both friend-handshake tiers.
pub const FRIEND_HANDSHAKE_WINDOW_MS: u64 = 60 * 60 * 1000; // 1h

/// ZEB-700: two-tier rate limiter for the `harmony/friend/v1` handshake ALPN —
/// the [`IntroRateLimiter`] pattern extended to the friend-link acceptor, with
/// DISJOINT budgets (friend handshakes and introductions never share a window).
///
/// - Tier 1 ([`admit_connection`](Self::admit_connection)): pre-auth flood
///   shield keyed on the connecting endpoint's authenticated `remote_id()` —
///   un-spoofable, runs before decode and all signature/cert verification
///   (the unbounded pre-consent crypto ZEB-700 bounds).
/// - Tier 2 ([`admit_owner`](Self::admit_owner)): post-auth per-owner window
///   keyed on the AUTHENTICATED requester.
///
/// Deliberately NO dedupe tier (divergence from [`IntroRateLimiter`]): the
/// legit friend flows re-dial the same `(requester, acceptor)` pair within
/// minutes — request → `Pending` → user approves → re-dial inline-accepts, and
/// `Pending` → token obtained → re-dial redeems it. A `(owner, owner)` dedupe
/// TTL would shed exactly those; the per-owner window never does.
///
/// Same posture as the intro limiter: NOT an authorization decision — sheds
/// volume only; the caller LOGS every shed ("no silent truncation") and writes
/// the SAME benign `Pending` reply a normal Path-A outcome writes (no oracle).
/// The admit methods never `.await`, so the mutex is never held across a
/// suspension point; both maps are bounded by [`MAX_WINDOW_KEYS`] with the
/// audited [`KeyedSlidingWindow`] eviction.
pub struct FriendRateLimiter {
    inner: Mutex<FriendLimiterInner>,
    /// ZEB-711: monotonic epoch — see [`IntroRateLimiter::epoch`]. Both
    /// ALPN limiters migrate in one move so their audited posture stays
    /// uniform.
    epoch: tokio::time::Instant,
}

struct FriendLimiterInner {
    /// Tier 1: pre-auth per-connection-endpoint window (un-spoofable iroh id).
    conn: KeyedSlidingWindow<[u8; 32]>,
    /// Tier 2: post-auth per-authenticated-owner window.
    owner: KeyedSlidingWindow<OwnerAddr>,
}

impl FriendRateLimiter {
    pub fn new() -> Self {
        Self::with_caps(
            FRIEND_HANDSHAKE_PER_CONNECTION_MAX,
            FRIEND_HANDSHAKE_PER_OWNER_MAX,
            FRIEND_HANDSHAKE_WINDOW_MS,
        )
    }

    /// Test/tuning constructor — deterministic tiny caps in unit tests.
    pub fn with_caps(conn_max: usize, owner_max: usize, window_ms: u64) -> Self {
        Self {
            inner: Mutex::new(FriendLimiterInner {
                conn: KeyedSlidingWindow::new(conn_max, window_ms),
                owner: KeyedSlidingWindow::new(owner_max, window_ms),
            }),
            epoch: tokio::time::Instant::now(),
        }
    }

    /// ZEB-711: production timeline for this limiter's admit calls — see
    /// [`IntroRateLimiter::monotonic_now_ms`].
    pub fn monotonic_now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    /// Poison-tolerant lock, as [`IntroRateLimiter::lock`]: plain counters,
    /// safe to keep using after a panic elsewhere.
    fn lock(&self) -> std::sync::MutexGuard<'_, FriendLimiterInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Tier 1 — pre-auth. `Ok(())` admits (and records); `Err` sheds.
    pub fn admit_connection(&self, remote_id: [u8; 32], now_ms: u64) -> Result<(), &'static str> {
        if self.lock().conn.admit(remote_id, now_ms) {
            Ok(())
        } else {
            Err("per-connection cap")
        }
    }

    /// Tier 2 — post-auth per-owner quota. `Ok(())` admits; `Err` sheds.
    pub fn admit_owner(&self, owner: OwnerAddr, now_ms: u64) -> Result<(), &'static str> {
        if self.lock().owner.admit(owner, now_ms) {
            Ok(())
        } else {
            Err("per-owner cap")
        }
    }
}

impl Default for FriendRateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_membership::mint_test_owner;
    use crate::owner_state_types::OwnerAddr;

    /// ZEB-680: an empty revoked-device projection for verifier call sites that
    /// don't exercise revocation (it revokes nothing).
    fn no_revocations() -> crate::revoked_device_projection::RevokedDeviceProjection {
        crate::revoked_device_projection::RevokedDeviceProjection::new()
    }

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

    /// A distinct 16-byte owner address from a counter — the low 8 bytes carry
    /// `n`, so every `addr16(n)` is unique and every value with a non-zero byte
    /// in positions 8..16 (e.g. `[0xC7; 16]`) is disjoint from all of them.
    /// Used by the flood/bound tests to spray rotating spoofed addresses.
    fn addr16(n: u64) -> OwnerAddr {
        let mut b = [0u8; 16];
        b[..8].copy_from_slice(&n.to_le_bytes());
        OwnerAddr(b)
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
        authenticate_introduce_request(&req, broker.owner, &no_revocations(), 0)
            .expect("authentic to broker");
        // Re-aimed at a different broker → WrongTarget (before any sig spend).
        assert_eq!(
            authenticate_introduce_request(&req, OwnerAddr([0x99; 16]), &no_revocations(), 0),
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
            authenticate_introduce_request(&req, broker.owner, &no_revocations(), 0),
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
            Vec::new(),
        );
        // X verifies: voucher == who we think F is, target == us.
        verify_introduction(&intro, voucher.owner, target.owner, &no_revocations(), 0)
            .expect("authentic");
        // Wrong expected voucher → VoucherMismatch (before sig spend).
        assert_eq!(
            verify_introduction(
                &intro,
                OwnerAddr([0x77; 16]),
                target.owner,
                &no_revocations(),
                0
            ),
            Err(IntroAuthError::VoucherMismatch),
        );
        // Relayed to the wrong X → WrongTarget.
        assert_eq!(
            verify_introduction(
                &intro,
                voucher.owner,
                OwnerAddr([0x88; 16]),
                &no_revocations(),
                0
            ),
            Err(IntroAuthError::WrongTarget),
        );
    }

    /// ZEB-680 §1 (T3 regression pin): `authenticate_introduce_request` consults
    /// the revoked-device projection through the inner `verify_enrolled_device`.
    /// A request from a revoked requester fails auth (`verify_enrolled_device`'s
    /// `DeviceRevoked` maps to `IntroAuthError::Auth`); the SAME request with an
    /// empty projection authenticates. The only difference between the two calls
    /// is the projection, so the rejection can only come from the revocation
    /// consult — pinning the per-site enforcement.
    #[test]
    fn authenticate_introduce_request_rejects_revoked_requester() {
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
        // Empty projection revokes nothing: authenticates.
        authenticate_introduce_request(&req, broker.owner, &no_revocations(), 0)
            .expect("empty projection revokes nothing");
        // Seed the requester's enrolled device key against its own owner.
        let revoked = crate::revoked_device_projection::RevokedDeviceProjection::new();
        let keys: std::collections::BTreeSet<[u8; 32]> =
            std::iter::once(from.cert.device_pubkeys.classical.ed25519_verify).collect();
        revoked.union_from_members(std::iter::once((from.owner, &keys)));
        let err = authenticate_introduce_request(&req, broker.owner, &revoked, 0).unwrap_err();
        assert!(
            matches!(err, IntroAuthError::Auth),
            "expected Auth (from DeviceRevoked), got {err:?}"
        );
    }

    /// ZEB-680 §1 (T3 regression pin): `verify_introduction` consults the
    /// projection for the VOUCHER cert (the security-relevant check — a friend
    /// whose revocations we likely know). Seeding the voucher's device key rejects
    /// the envelope; the same envelope with an empty projection verifies.
    #[test]
    fn verify_introduction_rejects_revoked_voucher() {
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
            Vec::new(),
        );
        // Empty projection revokes nothing: verifies.
        verify_introduction(&intro, voucher.owner, target.owner, &no_revocations(), 0)
            .expect("empty projection revokes nothing");
        // Seed the VOUCHER's enrolled device key against the voucher owner.
        let revoked = crate::revoked_device_projection::RevokedDeviceProjection::new();
        let keys: std::collections::BTreeSet<[u8; 32]> =
            std::iter::once(voucher.cert.device_pubkeys.classical.ed25519_verify).collect();
        revoked.union_from_members(std::iter::once((voucher.owner, &keys)));
        let err =
            verify_introduction(&intro, voucher.owner, target.owner, &revoked, 0).unwrap_err();
        assert!(
            matches!(err, IntroAuthError::Auth),
            "expected Auth (from DeviceRevoked), got {err:?}"
        );
    }

    /// ZEB-680 §1 (T3 regression pin): `verify_introduction` ALSO consults the
    /// projection for the SUBJECT cert (opportunistic — the subject may be a
    /// stranger). This proves that consult independently: the voucher stays clean
    /// (so the voucher consult and the signature verify both pass), and ONLY the
    /// subject's device key is revoked — the rejection therefore originates at the
    /// subject-side consult, not the voucher one.
    #[test]
    fn verify_introduction_rejects_revoked_subject() {
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
            Vec::new(),
        );
        // Empty projection revokes nothing: verifies.
        verify_introduction(&intro, voucher.owner, target.owner, &no_revocations(), 0)
            .expect("empty projection revokes nothing");
        // Seed ONLY the SUBJECT's enrolled device key against the subject owner;
        // the voucher is left clean so the voucher-side consult passes.
        let revoked = crate::revoked_device_projection::RevokedDeviceProjection::new();
        let keys: std::collections::BTreeSet<[u8; 32]> =
            std::iter::once(subject.cert.device_pubkeys.classical.ed25519_verify).collect();
        revoked.union_from_members(std::iter::once((subject.owner, &keys)));
        let err =
            verify_introduction(&intro, voucher.owner, target.owner, &revoked, 0).unwrap_err();
        assert!(
            matches!(err, IntroAuthError::Auth),
            "expected Auth (from DeviceRevoked), got {err:?}"
        );
    }

    fn hlc(w: u64) -> Hlc {
        Hlc {
            wall_ms: w,
            logical: 0,
            device_id: "d".into(),
        }
    }

    /// A full valid Active `FriendEntry`, varying only the `referrable` opt-in the
    /// broker keys on. Mirrors the integration test's `friend_entry`:
    /// `master_ed25519` is derived from `seed` so the friend-graph key invariant
    /// (map key == owner derived from this master key) holds by construction.
    fn active_referrable_entry(seed: u8, referrable: bool) -> crate::friend_graph::FriendEntry {
        crate::friend_graph::FriendEntry {
            master_ed25519: SigningKey::from_bytes(&[seed; 32])
                .verifying_key()
                .to_bytes(),
            display: Some("x".to_string()),
            status: crate::friend_graph::FriendStatus::Active,
            established_via: crate::friend_graph::FriendOrigin::Token,
            referrable,
            learned_at: hlc(1),
            sealed_secret: None,
        }
    }

    #[test]
    fn broker_builds_introduction_only_for_active_referrable_target() {
        let requester = mint_test_owner(0x11);
        let broker = mint_test_owner(0x22);
        let target = mint_test_owner(0x33);
        let mut fg = crate::friend_graph::FriendGraph::default();
        fg.friends
            .insert(target.owner, active_referrable_entry(0x33, true));
        // ZEB-376 (Q1): the broker now friend-gates the REQUESTER too — seed the
        // requester as an Active friend so the happy path still builds.
        fg.friends
            .insert(requester.owner, active_referrable_entry(0x11, true));
        let req = sign_introduce_request(
            &requester.device_key,
            requester.owner,
            broker.owner,
            target.owner,
            reach(),
            requester.cert.clone(),
        );
        let intro = build_introduction_for_request(
            &req,
            &fg,
            broker.owner,
            broker.cert.clone(),
            &broker.device_key,
            hlc(1),
            &no_revocations(),
            0,
        )
        .expect("active+referrable target → Introduction");
        // The Introduction relays the subject verbatim, vouched by the broker, aimed at X.
        assert_eq!(intro.voucher, broker.owner);
        assert_eq!(intro.to_addr, target.owner);
        assert_eq!(intro.subject, requester.owner);
        assert_eq!(intro.reachability, req.reachability);
        verify_introduction(&intro, broker.owner, target.owner, &no_revocations(), 0)
            .expect("F's signature verifies on X");
        // Non-referrable target → NotReferrable (no envelope leaks a non-opted-in friend).
        fg.friends
            .insert(target.owner, active_referrable_entry(0x33, false));
        assert!(matches!(
            build_introduction_for_request(
                &req,
                &fg,
                broker.owner,
                broker.cert.clone(),
                &broker.device_key,
                hlc(1),
                &no_revocations(),
                0
            ),
            Err(IntroBrokerError::NotReferrable),
        ));
    }

    /// ZEB-376 (Q1, HIGH auth bypass): an AUTHENTICATED requester who is NOT an
    /// Active friend of F must not be able to make F vouch for them — even when
    /// the target IS an active+referrable friend. The broker returns
    /// `RequesterNotFriend` and builds no Introduction (mirrors the catalog path's
    /// requester friend-gate). The request itself authenticates fine; only the
    /// authorization (requester ∈ F's friends) fails.
    #[test]
    fn broker_rejects_non_friend_requester() {
        let requester = mint_test_owner(0x11);
        let broker = mint_test_owner(0x22);
        let target = mint_test_owner(0x33);
        let mut fg = crate::friend_graph::FriendGraph::default();
        // Target IS an active+referrable friend; requester is NOT in the graph.
        fg.friends
            .insert(target.owner, active_referrable_entry(0x33, true));
        let req = sign_introduce_request(
            &requester.device_key,
            requester.owner,
            broker.owner,
            target.owner,
            reach(),
            requester.cert.clone(),
        );
        // The request authenticates (correct broker, valid cert + sig)…
        authenticate_introduce_request(&req, broker.owner, &no_revocations(), 0)
            .expect("request authenticates");
        // …but the broker refuses because the requester is not F's friend.
        assert!(matches!(
            build_introduction_for_request(
                &req,
                &fg,
                broker.owner,
                broker.cert.clone(),
                &broker.device_key,
                hlc(1),
                &no_revocations(),
                0,
            ),
            Err(IntroBrokerError::RequesterNotFriend),
        ));
        // A merely-Pending requester is also rejected (only Active passes).
        fg.friends.insert(requester.owner, {
            let mut e = active_referrable_entry(0x11, true);
            e.status = crate::friend_graph::FriendStatus::Pending;
            e
        });
        assert!(matches!(
            build_introduction_for_request(
                &req,
                &fg,
                broker.owner,
                broker.cert.clone(),
                &broker.device_key,
                hlc(1),
                &no_revocations(),
                0,
            ),
            Err(IntroBrokerError::RequesterNotFriend),
        ));
    }

    /// ZEB-376 (Q2, correctness): F forwards the subject's signer-cert bundle in
    /// the `Introduction` so a QUORUM-issued subject cert verifies on X. With the
    /// bundle present `verify_introduction` accepts; stripped, the subject-cert
    /// verify fails closed (`Auth`). Proves the field round-trips through F's
    /// broker into X's verifier.
    #[test]
    fn introduction_forwards_subject_signer_bundle_for_quorum_cert() {
        use crate::enrollment_verify::quorum_fixtures::{mint_quorum_world, WORLD_NOW};
        let voucher = mint_test_owner(0x22); // F
        let target = mint_test_owner(0x33); // X (self)
        let world = mint_quorum_world(0x80); // subject with a quorum-issued cert
        let subject_owner = OwnerAddr(world.owner_id);
        // Stamp `at` fresh relative to WORLD_NOW so the #4 envelope bound passes.
        let at = hlc(WORLD_NOW.saturating_mul(1000));

        let intro = sign_introduction(
            &voucher.device_key,
            voucher.owner,
            target.owner,
            subject_owner,
            world.c_quorum_cert.clone(),
            reach(),
            at,
            voucher.cert.clone(),
            world.bundle.clone(),
        );
        assert_eq!(
            intro.subject_signer_certs, world.bundle,
            "F forwards the subject's signer bundle verbatim"
        );

        // WITH the forwarded bundle the quorum subject cert verifies through X.
        verify_introduction(
            &intro,
            voucher.owner,
            target.owner,
            &no_revocations(),
            WORLD_NOW,
        )
        .expect("quorum subject cert verifies via the forwarded signer bundle");

        // WITHOUT it, the subject-cert verify fails closed (the exact bug Q2 fixes:
        // an empty bundle cannot verify a quorum cert).
        let mut stripped = intro.clone();
        stripped.subject_signer_certs.clear();
        assert_eq!(
            verify_introduction(
                &stripped,
                voucher.owner,
                target.owner,
                &no_revocations(),
                WORLD_NOW
            ),
            Err(IntroAuthError::Auth),
            "a quorum subject cert with no signer bundle must fail closed",
        );
    }

    /// ZEB-376 (#4, defense-in-depth): `verify_introduction` bounds the signed
    /// `at` envelope timestamp against X's wall clock — an envelope stamped far in
    /// the past or future is rejected `Stale` (the sig is valid over the stale
    /// stamp; freshness is the axis under test). A fresh stamp still verifies.
    #[test]
    fn introduction_rejects_stale_envelope_timestamp() {
        let voucher = mint_test_owner(0x22);
        let subject = mint_test_owner(0x11);
        let target = mint_test_owner(0x33);
        let now_secs = 1_700_000_000u64;
        let now_ms = now_secs * 1000;

        let sign_at = |wall_ms: u64| {
            sign_introduction(
                &voucher.device_key,
                voucher.owner,
                target.owner,
                subject.owner,
                subject.cert.clone(),
                reach(),
                hlc(wall_ms),
                voucher.cert.clone(),
                Vec::new(),
            )
        };

        // Fresh `at` (== now) verifies.
        verify_introduction(
            &sign_at(now_ms),
            voucher.owner,
            target.owner,
            &no_revocations(),
            now_secs,
        )
        .expect("a fresh envelope verifies");

        // Far in the past (> max age) → Stale.
        let stale = sign_at(now_ms - INTRODUCTION_MAX_ENVELOPE_AGE_MS - 1);
        assert_eq!(
            verify_introduction(
                &stale,
                voucher.owner,
                target.owner,
                &no_revocations(),
                now_secs
            ),
            Err(IntroAuthError::Stale),
            "an envelope older than the max age must be rejected",
        );

        // Far in the future (> forward skew) → Stale.
        let future = sign_at(now_ms + INTRODUCTION_MAX_FORWARD_SKEW_MS + 1);
        assert_eq!(
            verify_introduction(
                &future,
                voucher.owner,
                target.owner,
                &no_revocations(),
                now_secs
            ),
            Err(IntroAuthError::Stale),
            "an envelope beyond the forward-skew must be rejected",
        );
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

    /// ZEB-376 Task 10: the X-arm reachability checks. Build an `Introduction`
    /// whose relayed reachability is signed by the SUBJECT's device-#2 key over
    /// the CANONICAL `introduction_reachability_hlc()` — the exact convention X
    /// re-derives — while the broker stamps a DIFFERENT `intro.at`. This proves X
    /// verifies against the canonical HLC and IGNORES `intro.at` (verifying with
    /// `intro.at` must fail), then confirms X's inner-sig + freshness gates accept
    /// the honest record and reject a tampered sig or a stale stamp, all BEFORE
    /// any dial.
    #[test]
    fn x_arm_verifies_and_rejects_bad_relayed_reachability() {
        use crate::reachability_record::{
            build_signed_payload_with_key, reachability_freshness_check, verify_inner_signature,
            REACHABILITY_RECORD_TTL_MS,
        };

        let voucher = mint_test_owner(0x22); // F
        let subject = mint_test_owner(0x11); // the introducee (reachability owner)
        let target = mint_test_owner(0x33); // X (self)
        let announced_at_ms = 1_700_000_000_000u64;
        // The broker's clock (`intro.at`) is DELIBERATELY different from the
        // canonical reachability-signing HLC — the reachability's real signing
        // clock never rides the wire, so X must ignore `intro.at` entirely.
        let broker_at = hlc(announced_at_ms);

        // The subject signs their reachability over (actor=subject,
        // hlc=introduction_reachability_hlc()) with their enrolled device-#2 key —
        // exactly the canonical HLC X re-derives + verifies against.
        let reachability = build_signed_payload_with_key(
            [0x44; 32],
            "https://relay.example/".into(),
            vec![],
            announced_at_ms, // announced_at_ms carries freshness
            &subject.owner,
            &introduction_reachability_hlc(),
            Vec::new(),
            0,
            &subject.device_key,
        )
        .expect("sign reachability");

        let intro = sign_introduction(
            &voucher.device_key,
            voucher.owner,
            target.owner,
            subject.owner,
            subject.cert.clone(),
            reachability,
            broker_at.clone(),
            voucher.cert.clone(),
            Vec::new(),
        );

        // X derives the subject's device-#2 verifying key from the relayed cert.
        let subj_vk = crate::dm_signing::device2_verifying_key(&intro.subject_cert)
            .expect("subject cert has a device-#2 key");

        // Honest record verified against the CANONICAL HLC (what X's arm uses):
        // inner sig + freshness both pass.
        verify_inner_signature(
            &intro.reachability,
            &intro.subject,
            &introduction_reachability_hlc(),
            &subj_vk,
        )
        .expect("honest relayed reachability verifies against the canonical HLC");
        reachability_freshness_check(&intro.reachability, announced_at_ms)
            .expect("honest record is fresh at its stamp");

        // Verifying with `intro.at` (the broker clock) MUST fail — the reachability
        // was NOT signed over it. This is the axis the arm's fix is about: X pins
        // the canonical HLC, never `intro.at`.
        assert_ne!(
            intro.at,
            introduction_reachability_hlc(),
            "fixture must use a broker clock distinct from the canonical HLC"
        );
        assert!(
            verify_inner_signature(&intro.reachability, &intro.subject, &intro.at, &subj_vk)
                .is_err(),
            "verifying the relayed reachability against intro.at (not the canonical \
             HLC) must fail — X ignores intro.at"
        );

        // Tampered inner sig → rejected (X never dials).
        let mut bad_sig = intro.clone();
        bad_sig.reachability.identity_signature[0] ^= 0xFF;
        assert!(
            verify_inner_signature(
                &bad_sig.reachability,
                &bad_sig.subject,
                &introduction_reachability_hlc(),
                &subj_vk
            )
            .is_err(),
            "a tampered relayed reachability inner-sig must be rejected"
        );

        // Stale announced_at_ms (older than the TTL) → freshness gate rejects.
        assert!(
            reachability_freshness_check(
                &intro.reachability,
                announced_at_ms + REACHABILITY_RECORD_TTL_MS + 1
            )
            .is_err(),
            "a relayed reachability past its TTL must be rejected"
        );
    }

    /// ZEB-376 Task 12: the SELF side's reachability assembly. Build our OWN
    /// reachability announce via `build_self_reachability_announce` (signed by our
    /// device-#2 key over the CANONICAL `introduction_reachability_hlc()`) and
    /// prove it verifies against that same canonical HLC — the exact convention
    /// X's Task-10 verifier re-derives. A non-canonical HLC MUST fail, pinning the
    /// sign/verify HLC convention as coherent end-to-end (and guarding against a
    /// verifier that trivially accepts).
    #[test]
    fn build_self_reachability_announce_verifies_against_canonical_hlc() {
        use crate::reachability_record::verify_inner_signature;
        let me = mint_test_owner(0x55);
        let announced_at_ms = 1_700_000_000_000u64;
        let payload = build_self_reachability_announce(
            [0x44; 32],
            "https://relay.example/".into(),
            vec![],
            announced_at_ms,
            &me.owner,
            &me.device_key,
        )
        .expect("sign self reachability");
        // The wrapper stamps our inputs verbatim + an EMPTY butler set.
        assert_eq!(payload.iroh_node_id, [0x44; 32]);
        assert_eq!(payload.home_relay_url, "https://relay.example/");
        assert_eq!(payload.announced_at_ms, announced_at_ms);
        assert!(payload.butler_set.is_empty());
        assert_eq!(payload.bs_at, 0);
        // The inner sig verifies against the CANONICAL HLC (what X re-derives).
        let vk = me.device_key.verifying_key();
        verify_inner_signature(&payload, &me.owner, &introduction_reachability_hlc(), &vk)
            .expect("self reachability verifies against the canonical HLC");
        // A non-canonical HLC MUST fail — proving the canonical HLC is load-bearing.
        let wrong_hlc = hlc(announced_at_ms);
        assert_ne!(wrong_hlc, introduction_reachability_hlc());
        assert!(
            verify_inner_signature(&payload, &me.owner, &wrong_hlc, &vk).is_err(),
            "verifying self reachability against a non-canonical HLC must fail"
        );
    }

    // ── ZEB-376 Task 13: IntroRateLimiter (abuse hygiene). ──────────────────

    /// Admitting up to `INTRO_PER_VOUCHER_MAX` DISTINCT subjects from one key is
    /// fine; the next distinct subject in the same window sheds the per-voucher
    /// cap. A different key is accounted independently.
    #[test]
    fn intro_rate_limiter_per_voucher_cap_sheds_over_max() {
        let rl = IntroRateLimiter::new();
        let key = OwnerAddr([0xAA; 16]);
        // INTRO_PER_VOUCHER_MAX distinct subjects, all at the same instant, admit.
        for i in 0..INTRO_PER_VOUCHER_MAX {
            let subject = OwnerAddr([i as u8; 16]);
            assert_eq!(
                rl.admit_voucher(key, subject, 1_000),
                Ok(()),
                "subject {i} is within the cap and must admit"
            );
        }
        // The (MAX + 1)-th distinct subject in the window sheds.
        assert_eq!(
            rl.admit_voucher(key, OwnerAddr([0xFF; 16]), 1_000),
            Err("per-voucher cap"),
        );
        // A DIFFERENT key has its own budget — unaffected by the first key's cap.
        assert_eq!(
            rl.admit_voucher(OwnerAddr([0xBB; 16]), OwnerAddr([0xFF; 16]), 1_000),
            Ok(())
        );
    }

    /// A repeat of the SAME (key, subject) within the dedupe TTL sheds
    /// `"duplicate"`; the same pair at/after the TTL boundary admits again (the
    /// earlier shed did NOT refresh the last-seen stamp).
    #[test]
    fn intro_rate_limiter_dedupes_repeat_within_ttl_then_admits_after() {
        let rl = IntroRateLimiter::new();
        let key = OwnerAddr([0x01; 16]);
        let subject = OwnerAddr([0x02; 16]);
        assert_eq!(
            rl.admit_voucher(key, subject, 0),
            Ok(()),
            "first sighting admits"
        );
        // Same pair strictly within the TTL → duplicate.
        assert_eq!(
            rl.admit_voucher(key, subject, INTRO_DEDUPE_TTL_MS - 1),
            Err("duplicate"),
        );
        // Same pair exactly at the TTL boundary (measured from the last ADMIT at
        // t=0, not the shed) → admits again.
        assert_eq!(rl.admit_voucher(key, subject, INTRO_DEDUPE_TTL_MS), Ok(()));
    }

    /// Entries older than `INTRO_PER_VOUCHER_WINDOW_MS` are pruned and do NOT
    /// count toward the cap: a key at the cap admits again once its entries age
    /// out of the window.
    #[test]
    fn intro_rate_limiter_prunes_entries_older_than_window() {
        let rl = IntroRateLimiter::new();
        let key = OwnerAddr([0x03; 16]);
        // Fill the window to the cap at t=0 with distinct subjects.
        for i in 0..INTRO_PER_VOUCHER_MAX {
            assert_eq!(rl.admit_voucher(key, OwnerAddr([i as u8; 16]), 0), Ok(()));
        }
        // At the cap now → a fresh subject at t=0 sheds.
        assert_eq!(
            rl.admit_voucher(key, OwnerAddr([0xFE; 16]), 0),
            Err("per-voucher cap"),
        );
        // Advance past the window: all cap-counted entries are now stale and must
        // not count → a fresh subject admits again.
        assert_eq!(
            rl.admit_voucher(key, OwnerAddr([0xFD; 16]), INTRO_PER_VOUCHER_WINDOW_MS + 1),
            Ok(()),
        );
    }

    /// ZEB-376 Task 13 (memory bound): `admit` runs BEFORE frame authentication,
    /// so a peer spraying friend-PEX frames with rotating spoofed
    /// `from_addr`/`voucher`/`subject` values would grow BOTH limiter maps
    /// without bound — a remote pre-auth memory-DoS inside the very DoS-hygiene
    /// layer. Fire well over both hard caps with distinct keys AND subjects at
    /// monotonically increasing `now_ms` (so every admit is a fresh pair on a
    /// fresh key: no dedupe / cap shed, unbounded growth without eviction) and
    /// assert both tracked-entry counts stay bounded by the caps.
    #[test]
    fn intro_rate_limiter_bounds_memory_under_rotating_flood() {
        let rl = IntroRateLimiter::new();
        // now_ms stays far inside the window/TTL, so the stale passes free
        // nothing → the oldest-eviction fallback is what actually holds the bound.
        let inserted = MAX_DEDUPE_ENTRIES.max(MAX_WINDOW_KEYS) * 2;
        for i in 0..inserted as u64 {
            assert_eq!(
                rl.admit_voucher(addr16(2 * i), addr16(2 * i + 1), i + 1),
                Ok(()),
                "fresh key+subject at t={i} must admit",
            );
        }
        let (dedupe_len, window_len) = rl.tracked_len();
        assert!(
            dedupe_len <= MAX_DEDUPE_ENTRIES,
            "last_seen must stay bounded under a rotating flood: {dedupe_len} > {MAX_DEDUPE_ENTRIES}",
        );
        assert!(
            window_len <= MAX_WINDOW_KEYS,
            "windows must stay bounded under a rotating flood: {window_len} > {MAX_WINDOW_KEYS}",
        );
        // Sanity: we really pushed far past both caps, so eviction was exercised.
        assert!(inserted > MAX_DEDUPE_ENTRIES.max(MAX_WINDOW_KEYS));
    }

    /// Eviction must never disturb legitimate, within-cap traffic: after a flood
    /// forces eviction on both maps, a fresh legit key (disjoint from the flood
    /// namespace — a non-zero high byte `addr16` never sets) still admits exactly
    /// up to the per-voucher cap and still dedupes a repeat pair. The live
    /// cap/dedupe logic is unchanged by the memory bound; eviction only ever
    /// removes stale-or-oldest flood entries, never the newest legit ones.
    #[test]
    fn intro_rate_limiter_eviction_preserves_legit_sequence() {
        let rl = IntroRateLimiter::new();
        let flood = MAX_DEDUPE_ENTRIES.max(MAX_WINDOW_KEYS) * 2;
        for i in 0..flood as u64 {
            let _ = rl.admit_voucher(addr16(2 * i), addr16(2 * i + 1), i + 1);
        }
        // A fresh legit key, newest in the maps → survives any later eviction.
        let now = flood as u64 + 10;
        let key = OwnerAddr([0xC7; 16]);
        for j in 0..INTRO_PER_VOUCHER_MAX {
            assert_eq!(
                rl.admit_voucher(key, OwnerAddr([0x80 | j as u8; 16]), now),
                Ok(()),
                "legit subject {j} within the cap must still admit after a flood",
            );
        }
        // Cap still enforced for the legit key.
        assert_eq!(
            rl.admit_voucher(key, OwnerAddr([0xFF; 16]), now),
            Err("per-voucher cap"),
        );
        // Dedupe still enforced for a repeated legit pair within the TTL.
        let key2 = OwnerAddr([0xC8; 16]);
        let subj = OwnerAddr([0xC9; 16]);
        assert_eq!(rl.admit_voucher(key2, subj, now), Ok(()));
        assert_eq!(rl.admit_voucher(key2, subj, now + 1), Err("duplicate"));
    }

    #[test]
    fn decide_introduction_truth_table() {
        use crate::friend_graph::PeerIntroPolicy::*;
        // Open: always proceed, regardless of voucher.
        assert_eq!(decide_introduction(Open, true), IntroDecision::Proceed);
        assert_eq!(decide_introduction(Open, false), IntroDecision::Proceed);
        // FriendsOfFriends: proceed iff the voucher is an Active friend.
        assert_eq!(
            decide_introduction(FriendsOfFriends, true),
            IntroDecision::Proceed
        );
        assert_eq!(
            decide_introduction(FriendsOfFriends, false),
            IntroDecision::Reject
        );
        // AskMe: always stage a prompt (voucher-active is irrelevant to staging).
        assert_eq!(decide_introduction(AskMe, true), IntroDecision::Stage);
        assert_eq!(decide_introduction(AskMe, false), IntroDecision::Stage);
        // Closed: always reject.
        assert_eq!(decide_introduction(Closed, true), IntroDecision::Reject);
        assert_eq!(decide_introduction(Closed, false), IntroDecision::Reject);
    }

    // ── ZEB-694 Task A1: KeyedSlidingWindow<K> primitive. ───────────────────

    #[test]
    fn keyed_window_enforces_cap_within_window() {
        let mut w = KeyedSlidingWindow::new(2, 1000);
        assert!(w.admit(7u64, 0));
        assert!(w.admit(7u64, 10));
        assert!(
            !w.admit(7u64, 20),
            "third within the window is over the cap"
        );
        // a different key has its own budget
        assert!(w.admit(9u64, 20));
    }

    #[test]
    fn keyed_window_prunes_stale_timestamps() {
        let mut w = KeyedSlidingWindow::new(1, 1000);
        assert!(w.admit(7u64, 0));
        assert!(!w.admit(7u64, 500), "still within the 1000ms window");
        assert!(
            w.admit(7u64, 1001),
            "t=0 pruned (cutoff=1), window empty again"
        );
    }

    #[test]
    fn keyed_window_evicts_over_cap_keys() {
        let mut w = KeyedSlidingWindow::new(1, 1_000_000);
        for k in 0u64..(MAX_WINDOW_KEYS as u64 + 100) {
            w.admit(k, k); // distinct keys, distinct timestamps
        }
        w.evict(MAX_WINDOW_KEYS as u64 + 100);
        assert!(
            w.windows.len() <= MAX_WINDOW_KEYS,
            "map bounded after eviction"
        );
    }

    #[test]
    fn keyed_window_zero_cap_admits_nothing_and_stays_empty() {
        let mut w = KeyedSlidingWindow::new(0, 1000);
        assert!(!w.admit(1u64, 0));
        assert!(!w.admit(2u64, 0));
        assert!(
            w.windows.is_empty(),
            "a zero cap must not insert map entries"
        );
    }

    // ── ZEB-694 Task A2: KeyedDedupe<K> primitive. ───────────────────────────

    #[test]
    fn keyed_dedupe_flags_repeat_within_ttl() {
        let mut d = KeyedDedupe::new(1000);
        assert!(
            !d.is_duplicate(5u64, 0),
            "never-seen key is not a duplicate"
        );
        d.record(5u64, 0);
        assert!(
            d.is_duplicate(5u64, 500),
            "repeat within ttl is a duplicate"
        );
        assert!(!d.is_duplicate(5u64, 1000), "past ttl is not a duplicate");
        assert!(
            !d.is_duplicate(6u64, 500),
            "different key is not a duplicate"
        );
    }

    #[test]
    fn keyed_dedupe_evicts_over_cap() {
        let mut d = KeyedDedupe::new(1_000_000);
        for k in 0u64..(MAX_DEDUPE_ENTRIES as u64 + 100) {
            d.record(k, k);
        }
        assert!(
            d.last_seen.len() <= MAX_DEDUPE_ENTRIES,
            "map bounded after record-time eviction"
        );
    }

    // ── ZEB-694 Task A3: two-tier role-separated IntroRateLimiter. ───────────

    #[test]
    fn limiter_roles_have_independent_budgets() {
        // Greptile regression: requester traffic must not starve an unrelated vouch.
        let rl = IntroRateLimiter::with_caps(100, 1, 3_600_000, 300_000);
        let o = OwnerAddr([1; 16]);
        let t = OwnerAddr([2; 16]);
        let t2 = OwnerAddr([3; 16]);
        assert!(rl.admit_requester(o, t, 0).is_ok());
        assert_eq!(
            rl.admit_requester(o, t2, 1),
            Err("per-requester cap"),
            "requester at cap"
        );
        // Disjoint dedupe AND window: the SAME (o, t) pair recorded in the requester
        // dedupe must still admit as a voucher (separate dedupe map) and exercises the
        // voucher window independently of the exhausted requester window.
        assert!(
            rl.admit_voucher(o, t, 2).is_ok(),
            "voucher role has disjoint window AND dedupe from the requester role"
        );
    }

    #[test]
    fn limiter_requester_dedupes_repeat_within_ttl() {
        let rl = IntroRateLimiter::with_caps(100, 100, 3_600_000, 300_000);
        let r = OwnerAddr([1; 16]);
        let t = OwnerAddr([2; 16]);
        assert!(rl.admit_requester(r, t, 0).is_ok());
        assert_eq!(
            rl.admit_requester(r, t, 1),
            Err("duplicate"),
            "a repeat (requester, target) within the ttl is deduped"
        );
        assert!(
            rl.admit_requester(r, OwnerAddr([9; 16]), 2).is_ok(),
            "a different target is not a duplicate"
        );
    }

    #[test]
    fn limiter_connection_shield_sheds_one_endpoint_only() {
        let rl = IntroRateLimiter::with_caps(1, 100, 3_600_000, 300_000);
        assert!(rl.admit_connection([1; 32], 0).is_ok());
        assert_eq!(
            rl.admit_connection([1; 32], 1),
            Err("per-connection cap"),
            "same endpoint shed at cap"
        );
        assert!(
            rl.admit_connection([2; 32], 2).is_ok(),
            "a different endpoint still admits"
        );
    }

    // ---- ZEB-700: FriendRateLimiter (friend/v1 handshake tiers) ----------

    #[test]
    fn friend_limiter_connection_cap_sheds_over_max_zeb700() {
        let rl = FriendRateLimiter::with_caps(2, 100, 3_600_000);
        assert!(rl.admit_connection([1; 32], 0).is_ok());
        assert!(rl.admit_connection([1; 32], 1).is_ok());
        assert_eq!(
            rl.admit_connection([1; 32], 2),
            Err("per-connection cap"),
            "third handshake from the same endpoint within the window is shed"
        );
        assert!(
            rl.admit_connection([2; 32], 3).is_ok(),
            "a different endpoint still admits"
        );
    }

    #[test]
    fn friend_limiter_owner_cap_sheds_over_max_zeb700() {
        let rl = FriendRateLimiter::with_caps(100, 1, 3_600_000);
        let owner = OwnerAddr([7; 16]);
        assert!(rl.admit_owner(owner, 0).is_ok());
        assert_eq!(
            rl.admit_owner(owner, 1),
            Err("per-owner cap"),
            "second handshake from the same owner within the window is shed"
        );
        assert!(
            rl.admit_owner(OwnerAddr([8; 16]), 2).is_ok(),
            "a different owner still admits"
        );
    }

    /// The window SLIDES: entries older than the window fall out, so a shed
    /// endpoint/owner self-heals by waiting it out (the "honest peer that
    /// somehow trips the cap re-dials later" story).
    #[test]
    fn friend_limiter_window_slides_and_readmits_zeb700() {
        let window = 1_000;
        let rl = FriendRateLimiter::with_caps(1, 1, window);
        let owner = OwnerAddr([7; 16]);
        assert!(rl.admit_connection([1; 32], 0).is_ok());
        assert!(rl.admit_owner(owner, 0).is_ok());
        assert_eq!(rl.admit_connection([1; 32], 500), Err("per-connection cap"));
        assert_eq!(rl.admit_owner(owner, 500), Err("per-owner cap"));
        assert!(
            rl.admit_connection([1; 32], window + 1).is_ok(),
            "past the window the endpoint re-admits"
        );
        assert!(
            rl.admit_owner(owner, window + 1).is_ok(),
            "past the window the owner re-admits"
        );
    }

    /// Tier budgets are DISJOINT: exhausting the connection tier does not
    /// consume the owner tier and vice versa (mirrors the intro limiter's
    /// per-role separation).
    #[test]
    fn friend_limiter_tiers_have_independent_budgets_zeb700() {
        let rl = FriendRateLimiter::with_caps(1, 1, 3_600_000);
        assert!(rl.admit_connection([1; 32], 0).is_ok());
        assert_eq!(rl.admit_connection([1; 32], 1), Err("per-connection cap"));
        assert!(
            rl.admit_owner(OwnerAddr([7; 16]), 2).is_ok(),
            "owner tier unaffected by the exhausted connection tier"
        );
    }

    /// Zero caps admit nothing (the acceptor-level shed tests rely on this to
    /// force a shed on the first handshake) and, per the audited primitive,
    /// record no unbounded empty entries.
    #[test]
    fn friend_limiter_zero_caps_admit_nothing_zeb700() {
        let rl = FriendRateLimiter::with_caps(0, 0, 3_600_000);
        assert_eq!(rl.admit_connection([1; 32], 0), Err("per-connection cap"));
        assert_eq!(rl.admit_owner(OwnerAddr([7; 16]), 0), Err("per-owner cap"));
    }

    // ---- ZEB-711: monotonic limiter timeline --------------------------------
    //
    // The production `now_ms` feed comes from the limiter's own
    // `monotonic_now_ms()` (tokio monotonic clock), not `wall_now_ms()`
    // (`SystemTime`). These tests pin the property that makes that
    // distinguishable: under tokio's paused test clock the timeline
    // advances ONLY via `tokio::time::advance` — impossible for a
    // SystemTime-backed source, which ignores the paused clock (and can
    // step arbitrarily in production, distorting the windows: a forward
    // jump grants a flood a fresh budget, a backward jump keeps an honest
    // shed peer shed).

    #[tokio::test(start_paused = true)]
    async fn friend_limiter_timeline_is_the_paused_tokio_clock_zeb711() {
        let rl = FriendRateLimiter::with_caps(1, 1, 1_000);
        let t0 = rl.monotonic_now_ms();
        tokio::time::advance(std::time::Duration::from_millis(5_000)).await;
        assert_eq!(
            rl.monotonic_now_ms(),
            t0 + 5_000,
            "the limiter timeline must advance exactly with the tokio clock"
        );

        // Window mechanics driven entirely through the limiter's own clock:
        // cap 1 → second admit in-window sheds; advancing the tokio clock
        // past the window re-admits. No wall clock involved anywhere.
        let id = [9u8; 32];
        assert_eq!(rl.admit_connection(id, rl.monotonic_now_ms()), Ok(()));
        assert_eq!(
            rl.admit_connection(id, rl.monotonic_now_ms()),
            Err("per-connection cap")
        );
        tokio::time::advance(std::time::Duration::from_millis(1_001)).await;
        assert_eq!(
            rl.admit_connection(id, rl.monotonic_now_ms()),
            Ok(()),
            "advancing the paused clock past the window must re-admit"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn intro_limiter_timeline_is_the_paused_tokio_clock_zeb711() {
        let rl = IntroRateLimiter::with_caps(1, 1, 1_000, 500);
        let t0 = rl.monotonic_now_ms();
        tokio::time::advance(std::time::Duration::from_millis(2_500)).await;
        assert_eq!(
            rl.monotonic_now_ms(),
            t0 + 2_500,
            "the limiter timeline must advance exactly with the tokio clock"
        );

        let id = [3u8; 32];
        assert_eq!(rl.admit_connection(id, rl.monotonic_now_ms()), Ok(()));
        assert_eq!(
            rl.admit_connection(id, rl.monotonic_now_ms()),
            Err("per-connection cap")
        );
        tokio::time::advance(std::time::Duration::from_millis(1_001)).await;
        assert_eq!(
            rl.admit_connection(id, rl.monotonic_now_ms()),
            Ok(()),
            "advancing the paused clock past the window must re-admit"
        );
    }
}
