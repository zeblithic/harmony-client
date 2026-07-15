//! ZEB-376 (Friends Phase 2b): active-introduction wire types + codecs for the
//! `harmony/friend-pex/v1` sub-protocol. Same strict-CBOR / single-char-key /
//! device-#2-signed / address-bound discipline as `referral_catalog.rs` (2a);
//! the two sub-protocols share one wire codec (`referral_catalog::decode_strict`,
//! `PEX_MAX_PACKET_LEN`) so their framing can never diverge.

use std::collections::{HashMap, VecDeque};
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

/// F's broker error: either the inbound [`IntroduceRequest`] failed
/// authentication, or the requested `target` is not an Active + referrable
/// friend of the broker — in which case F relays NOTHING (no envelope leaks a
/// non-opted-in friend). Mirrors `referral_catalog`'s split of auth vs
/// authorization failure.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum IntroBrokerError {
    #[error("introduce request authentication failed: {0}")]
    Auth(#[from] IntroAuthError),
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
    now_secs: u64,
) -> Result<Introduction, IntroBrokerError> {
    authenticate_introduce_request(req, self_owner, now_secs)?;
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

/// Process-local DoS hygiene layered BEFORE the (primary) policy/authentication
/// defenses on both friend-PEX introduction arms: a per-`key` sliding-window cap
/// plus a `(key, subject)` dedupe, so a compromised/spammy voucher (F) or
/// requester cannot flood a node with introductions. It is deliberately NOT an
/// authorization decision — it only sheds volume; every shed is LOGGED by the
/// caller ("no silent truncation") and answered with the same benign ack a
/// normal outcome writes, so a shed is network-indistinguishable (no oracle).
///
/// Guarded by a `std::sync::Mutex`; [`admit`](Self::admit) never `.await`s, so
/// the lock is never held across a suspension point.
pub struct IntroRateLimiter {
    inner: Mutex<IntroRateLimiterInner>,
}

struct IntroRateLimiterInner {
    /// Per-`key` admit timestamps (epoch-ms), pruned to
    /// [`INTRO_PER_VOUCHER_WINDOW_MS`] on each `admit`; `len()` is the in-window
    /// count checked against [`INTRO_PER_VOUCHER_MAX`].
    windows: HashMap<OwnerAddr, VecDeque<u64>>,
    /// Last time a `(key, subject)` pair was admitted, for TTL dedupe.
    last_seen: HashMap<(OwnerAddr, OwnerAddr), u64>,
}

impl IntroRateLimiter {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(IntroRateLimiterInner {
                windows: HashMap::new(),
                last_seen: HashMap::new(),
            }),
        }
    }

    /// Returns `Ok(())` to admit (and RECORDS the event: bumps the `key`'s
    /// window and the `(key, subject)` last-seen), or `Err(reason)` to shed
    /// WITHOUT recording. Sheds `"duplicate"` when `(key, subject)` was admitted
    /// within [`INTRO_DEDUPE_TTL_MS`], or `"per-voucher cap"` when `key` already
    /// has [`INTRO_PER_VOUCHER_MAX`] admits inside
    /// [`INTRO_PER_VOUCHER_WINDOW_MS`]. Pure over `now_ms`; the caller LOGS every
    /// shed.
    pub fn admit(
        &self,
        key: OwnerAddr,
        subject: OwnerAddr,
        now_ms: u64,
    ) -> Result<(), &'static str> {
        // Poison-tolerant: a panic elsewhere must not wedge the acceptor — the
        // guarded state is plain counters, safe to keep using.
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        // 1. Dedupe: a repeat of the SAME (key, subject) within the TTL is shed
        //    (a shed does NOT refresh the last-seen stamp — only an admit does).
        if let Some(&last) = inner.last_seen.get(&(key, subject)) {
            if now_ms.saturating_sub(last) < INTRO_DEDUPE_TTL_MS {
                return Err("duplicate");
            }
        }

        // 2. Per-`key` sliding window: drop entries older than the window, then
        //    enforce the cap on what remains.
        {
            let window = inner.windows.entry(key).or_default();
            let cutoff = now_ms.saturating_sub(INTRO_PER_VOUCHER_WINDOW_MS);
            while window.front().is_some_and(|&t| t < cutoff) {
                window.pop_front();
            }
            if window.len() >= INTRO_PER_VOUCHER_MAX {
                return Err("per-voucher cap");
            }
            window.push_back(now_ms);
        }

        // 3. Record the admit for future dedupe.
        inner.last_seen.insert((key, subject), now_ms);
        Ok(())
    }
}

impl Default for IntroRateLimiter {
    fn default() -> Self {
        Self::new()
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
            0,
        )
        .expect("active+referrable target → Introduction");
        // The Introduction relays the subject verbatim, vouched by the broker, aimed at X.
        assert_eq!(intro.voucher, broker.owner);
        assert_eq!(intro.to_addr, target.owner);
        assert_eq!(intro.subject, requester.owner);
        assert_eq!(intro.reachability, req.reachability);
        verify_introduction(&intro, broker.owner, target.owner, 0)
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
                0
            ),
            Err(IntroBrokerError::NotReferrable),
        ));
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
                rl.admit(key, subject, 1_000),
                Ok(()),
                "subject {i} is within the cap and must admit"
            );
        }
        // The (MAX + 1)-th distinct subject in the window sheds.
        assert_eq!(
            rl.admit(key, OwnerAddr([0xFF; 16]), 1_000),
            Err("per-voucher cap"),
        );
        // A DIFFERENT key has its own budget — unaffected by the first key's cap.
        assert_eq!(
            rl.admit(OwnerAddr([0xBB; 16]), OwnerAddr([0xFF; 16]), 1_000),
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
        assert_eq!(rl.admit(key, subject, 0), Ok(()), "first sighting admits");
        // Same pair strictly within the TTL → duplicate.
        assert_eq!(
            rl.admit(key, subject, INTRO_DEDUPE_TTL_MS - 1),
            Err("duplicate"),
        );
        // Same pair exactly at the TTL boundary (measured from the last ADMIT at
        // t=0, not the shed) → admits again.
        assert_eq!(rl.admit(key, subject, INTRO_DEDUPE_TTL_MS), Ok(()));
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
            assert_eq!(rl.admit(key, OwnerAddr([i as u8; 16]), 0), Ok(()));
        }
        // At the cap now → a fresh subject at t=0 sheds.
        assert_eq!(
            rl.admit(key, OwnerAddr([0xFE; 16]), 0),
            Err("per-voucher cap"),
        );
        // Advance past the window: all cap-counted entries are now stale and must
        // not count → a fresh subject admits again.
        assert_eq!(
            rl.admit(key, OwnerAddr([0xFD; 16]), INTRO_PER_VOUCHER_WINDOW_MS + 1),
            Ok(()),
        );
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
}
