# Friends Phase 2b — Introduction Broker + PeerIntroPolicy Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship ZEB-376 Path C — a friend F actively brokers a signed introduction from you to F's referrable friend X, X enforces its `PeerIntroPolicy`, and on accept X forms a direct mutual friend-link with you (F drops out) — plus `PeerIntroPolicy` persistence/IPC/enforcement/UI.

**Architecture:** Two new device-#2-signed CBOR wire types (`IntroduceRequest` you→F, `Introduction` F→X) ride a tagged `PexFrame` enum on the existing `harmony/friend-pex/v1` ALPN. F validates + relays; X verifies + enforces policy via a new pure `decide_introduction`; the introducee's own self-authenticating `ReachabilityAnnouncePayload` rides inside the introduction so X dials you at first contact with no global-discoverability requirement. The final link reuses the Path-A handshake through an extracted `dial_and_link_friend` helper parameterized by `FriendOrigin`, with your side auto-accepting via a new `PendingOutboundIntroductions` pre-auth and `ConsentDecision::AcceptInlineIntroduced`.

**Tech Stack:** Rust (Tauri v2 backend, ciborium CBOR, ed25519-dalek, iroh transport), Svelte 5 + TypeScript frontend, cargo-nextest, vitest.

**Design spec:** `docs/specs/2026-07-15-zeb-376-friends-phase-2b-introduction-broker-design.md`. Arc spec: `docs/specs/2026-06-03-friends-peer-introductions-design.md` (§5.4/§6.2/§7).

## Global Constraints

- **Wire discipline (mirror 2a `referral_catalog.rs` exactly):** strict CBOR; single-/two-char serde map keys; bounded decode at `PEX_MAX_PACKET_LEN` (256 KiB) with trailing-byte rejection; device-#2 Ed25519 signature over a domain-separated CBOR preimage; `verify_enrolled_device` is the auth chokepoint (binds cert→owner_id); anti-replay via address-binding (`to_addr`/`target`/`subject`), **no nonce**.
- **Domain tags:** `"hir1"` = `IntroduceRequest` preimage; `"hin1"` = `Introduction` preimage. Distinct from 2a `"hcr1"`/`"hrc1"`.
- **Verification order is security-load-bearing** (mirror 2a `authenticate_catalog_request`/`verify_referral_catalog`): target/identity checks (`WrongTarget`, mismatch) BEFORE cert/signature checks, so a mis-addressed/mis-attributed message is rejected without spending a signature verify.
- **Reachability-in-envelope:** the `Introduction` carries the introducee's own current `ReachabilityAnnouncePayload` (`reachability_record.rs:81`). X MUST run `verify_inner_sig()` + `verify_identity_match()` + `verify_freshness()` on it (the same checks `connectivity_add_friend_by_key_inner` runs on a Case-B-resolved record, lib.rs:54683-54690) BEFORE dialing. No Case-B, no global discoverability.
- **`PeerIntroPolicy` home = `ConnectivitySettings`** (single-user `connectivity-settings.json`), NOT the owner-state CRDT (that is per-friend `referrable`). Fresh-install default = `FriendsOfFriends`; `fail_closed_defaults()` (corrupt-file path) = `Closed`.
- **`FriendOrigin::Introduction` already exists** (`friend_graph.rs:107`) — no CRDT migration. Both sides of an introduced link write `established_via: Introduction`.
- **Transport:** `PexFrame` enum on the existing friend-PEX ALPN — NOT a new ALPN. Decoder tries `PexFrame` first, falls back to bare `CatalogRequest`; 2a `zeb375_pex_fixtures` bytes stay byte-pinned (unchanged).
- **No silent truncation:** any cap that sheds (per-voucher intro cap, dedupe eviction) MUST log what it dropped.
- **Tauri IPC:** Rust params `snake_case`, JS callers `camelCase`; error extraction `e instanceof Error ? e.message : String(e)`.
- **CI gates (run from `src-tauri/` unless noted):** `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures`; MSRV `cargo check --locked --all-targets --features test-fixtures` (toolchain 1.91); frontend (repo root) `npx tsc --noEmit` + `npx vitest run`. Iterative per-task gate: `scripts/test-select --context task` (paste its `round=… bucket=…` line into task reports); the FINAL pre-PR sweep is the full `--workspace --all-targets` commands.

## File Structure

**New files:**
- `src-tauri/src/friend_intro.rs` — 2b wire types (`IntroduceRequest`, `Introduction`), the `PexFrame` enum, their sign/verify/codec (mirrors `referral_catalog.rs`'s discipline), and the pure `decide_introduction` policy fn. One cohesive "friend introduction sub-protocol" module.
- `src-tauri/tests/wire_format/zeb376_intro_fixtures.rs` — byte-pinned CBOR fixtures (mirrors `zeb375_pex_fixtures.rs`).
- `src-tauri/tests/identity/introduction_broker_roundtrip_integration.rs` — the 3-node (You–F–X) e2e (mirrors `referral_catalog_roundtrip_integration.rs`).

**Modified files:**
- `src-tauri/src/referral_catalog.rs` — promote `decode_strict` to `pub(crate)` (DRY reuse by `friend_intro`).
- `src-tauri/src/connectivity_settings.rs` — add `peer_intro_policy` field + `default_peer_intro_policy` + fail-closed value.
- `src-tauri/src/friend_requests.rs` — add `PendingOutboundIntroductions` store; add a `kind` discriminant to `PendingInbound` (`LinkRequest`/`IntroductionOffer`).
- `src-tauri/src/iroh_friend_acceptor.rs` — `ConsentDecision::AcceptInlineIntroduced`; extend `resolve_consent_consuming_approval` + the acceptor struct with the pending-outbound handle; `process_friend_request` `origin_override` param; the new consent arm.
- `src-tauri/src/iroh_pex_acceptor.rs` — `serve()` decodes `PexFrame` (fallback to bare `CatalogRequest`); F's `IntroduceRequest` broker arm; X's `Introduction` arm (verify + policy + link/stage); new acceptor deps + builders.
- `src-tauri/src/lib.rs` — extract `link_over_connection`; `request_introduction`/`get_peer_intro_policy`/`set_peer_intro_policy` IPCs + registration; thread new PEX-acceptor deps at its construction site (~9555-9574); `PendingFriendRequestDto.introducedBy`; production `FriendEventEmit`.
- `src/lib/friend-service.ts` + `src/lib/components/FriendsPanel.svelte` — policy dropdown, request-intro button, offer badge.

## Cross-task interface summary (names every task must use verbatim)

- Wire: `IntroduceRequest`, `Introduction`, `PexFrame::{IntroduceRequest(Box<IntroduceRequest>), Introduction(Box<Introduction>)}`, `encode_pex_frame`, `decode_pex_frame_or_catalog(&[u8]) -> PexDecoded`, `PexDecoded::{Catalog(CatalogRequest), Frame(PexFrame)}`.
- Sign/verify: `introduce_request_sig_preimage`/`introduction_sig_preimage` (tags `"hir1"`/`"hin1"`); `sign_introduce_request`, `authenticate_introduce_request(req, self_owner, now_secs) -> Result<(), IntroAuthError>`, `sign_introduction`, `verify_introduction(intro, expected_voucher, expected_target, now_secs) -> Result<(), IntroAuthError>`.
- Policy: `PeerIntroPolicy` (exists, `friend_graph.rs:118`), `decide_introduction(policy, voucher_is_active_friend) -> IntroDecision`, `IntroDecision::{Proceed, Stage, Reject}`.
- Link: `link_over_connection(conn, dial_config, origin: FriendOrigin, …self…) -> Result<AddFriendOutcome, String>`; `endpoint_addr_from_routing` (exists, lib.rs:50402); `build_signed_payload_with_key` (exists, `reachability_record.rs:313`).
- Consent: `ConsentDecision::AcceptInlineIntroduced`; `PendingOutboundIntroductions::{new, record(target, now_ms), take(target, now_ms) -> bool}`; `process_friend_request(…, origin_override: Option<FriendOrigin>)`.
- Pending inbox: `PendingKind::{LinkRequest, IntroductionOffer(Box<StoredIntroductionOffer>)}`; `PendingFriendRequests::record_introduction_offer(subject, display, now_ms, offer)`.

---

### Task 1: `friend_intro.rs` scaffold + `IntroduceRequest` wire type (you→F)

**Files:**
- Create: `src-tauri/src/friend_intro.rs`
- Modify: `src-tauri/src/referral_catalog.rs` (promote `decode_strict` to `pub(crate)`); `src-tauri/src/lib.rs` (add `pub mod friend_intro;` beside `pub mod referral_catalog;`)
- Test: inline `#[cfg(test)] mod tests` in `friend_intro.rs`

**Interfaces:**
- Consumes: `referral_catalog::{PEX_MAX_PACKET_LEN, decode_strict}` (the latter newly `pub(crate)`); `owner_state_types::{OwnerAddr, serialize_bytes_as_bstr, deserialize_bytes_from_bstr}`; `reachability_record::ReachabilityAnnouncePayload`; `harmony_owner::certs::EnrollmentCert`; `iroh_friend_acceptor::verify_enrolled_device`.
- Produces: `IntroduceRequest`, `IntroAuthError`, `introduce_request_sig_preimage(from, to, target, &reachability) -> Vec<u8>` (tag `"hir1"`), `sign_introduce_request(device2, from, to, target, reachability, enrollment) -> IntroduceRequest`, `authenticate_introduce_request(&IntroduceRequest, self_owner, now_secs) -> Result<(), IntroAuthError>`, `encode_introduce_request`/`decode_introduce_request`.

- [ ] **Step 1: Promote `decode_strict`.** In `referral_catalog.rs:104`, change `fn decode_strict<T: ...>` to `pub(crate) fn decode_strict<T: ...>` (no other change). Add `pub mod friend_intro;` to `lib.rs` next to the existing `pub mod referral_catalog;` declaration.

- [ ] **Step 2: Write the failing tests** (in `friend_intro.rs`'s test module):

```rust
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
            &from.device_key, from.owner, broker.owner, target, reach(), from.cert.clone(),
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
            &from.device_key, from.owner, broker.owner, OwnerAddr([0x33; 16]), reach(), from.cert.clone(),
        );
        req.target = OwnerAddr([0x44; 16]); // swap whom we asked to meet
        assert_eq!(
            authenticate_introduce_request(&req, broker.owner, 0),
            Err(IntroAuthError::SignatureInvalid),
        );
    }
}
```

- [ ] **Step 3: Run to verify failure.** `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(friend_intro)'` → FAIL (module/types undefined).

- [ ] **Step 4: Write the implementation** (`friend_intro.rs` head):

```rust
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
use crate::referral_catalog::{decode_strict, ReferralCodecError, PEX_MAX_PACKET_LEN};
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
        &P { d: "hir1", a: from_addr, t: to_addr, x: target, r: reachability },
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
    IntroduceRequest { from_addr, to_addr, target, reachability, enrollment, sig, signer_certs: Vec::new() }
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
    let verified = verify_enrolled_device(&req.enrollment, &req.signer_certs, req.from_addr, now_secs)
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
```

- [ ] **Step 5: Run to verify pass.** `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(friend_intro)'` → PASS.

- [ ] **Step 6: Gate + commit.** `cargo fmt --all`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`. Then:
```bash
git add src-tauri/src/friend_intro.rs src-tauri/src/referral_catalog.rs src-tauri/src/lib.rs
git commit -m "feat(zeb-376): IntroduceRequest wire type + sign/authenticate (friend-PEX 2b)"
```

---

### Task 2: `Introduction` wire type (F→X)

**Files:**
- Modify: `src-tauri/src/friend_intro.rs`
- Test: inline test module

**Interfaces:**
- Consumes: same imports as Task 1.
- Produces: `Introduction`, `introduction_sig_preimage(voucher, to, subject, &subject_cert, &reachability, &at) -> Vec<u8>` (tag `"hin1"`), `sign_introduction(device2, voucher, to, subject, subject_cert, reachability, at, voucher_enrollment) -> Introduction`, `verify_introduction(&Introduction, expected_voucher, expected_target, now_secs) -> Result<(), IntroAuthError>`, `encode_introduction`/`decode_introduction`.

- [ ] **Step 1: Write the failing tests:**

```rust
#[test]
fn introduction_verifies_and_binds_voucher_and_target() {
    let voucher = mint_test_owner(0x22);
    let subject = mint_test_owner(0x11);
    let target = mint_test_owner(0x33); // X (self)
    let intro = sign_introduction(
        &voucher.device_key, voucher.owner, target.owner, subject.owner,
        subject.cert.clone(), reach(), hlc(5), voucher.cert.clone(),
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

fn hlc(w: u64) -> Hlc { Hlc { wall_ms: w, logical: 0, device_id: "d".into() } }
```

- [ ] **Step 2: Run to verify failure.** `cargo nextest run --locked --features test-fixtures -E 'test(introduction_verifies)'` → FAIL.

- [ ] **Step 3: Implement:**

```rust
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
        &P { d: "hin1", v: voucher, t: to_addr, u: subject, c: subject_cert, r: reachability, h: at },
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
        voucher, to_addr, subject, subject_cert, reachability, at, sig, voucher_enrollment,
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
    let vverified =
        verify_enrolled_device(&intro.voucher_enrollment, &intro.signer_certs, intro.voucher, now_secs)
            .map_err(|_| IntroAuthError::Auth)?;
    let vk = VerifyingKey::from_bytes(&vverified.device_ed25519)
        .map_err(|_| IntroAuthError::SignatureInvalid)?;
    let preimage = introduction_sig_preimage(
        intro.voucher, intro.to_addr, intro.subject, &intro.subject_cert, &intro.reachability, &intro.at,
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
    ciborium::into_writer(intro, &mut out).map_err(|e| ReferralCodecError::Encode(e.to_string()))?;
    Ok(out)
}

pub fn decode_introduction(bytes: &[u8]) -> Result<Introduction, ReferralCodecError> {
    decode_strict(bytes)
}
```

Note: the subject may carry quorum `signer_certs` in a later slice; for 2b the subject cert is Master-issued (`&[]`), matching every other 2b path.

- [ ] **Step 4: Run to verify pass.** → PASS.
- [ ] **Step 5: Gate + commit.** fmt + clippy, then `git commit -m "feat(zeb-376): Introduction wire type + sign/verify (friend-PEX 2b)"`.

---

### Task 3: `PexFrame` enum + fallback decode + byte-pinned wire fixtures

**Files:**
- Modify: `src-tauri/src/friend_intro.rs`
- Create: `src-tauri/tests/wire_format/zeb376_intro_fixtures.rs`
- Modify: `src-tauri/tests/wire_format_tests.rs` (register the fixture module: `mod zeb376_intro_fixtures;`)
- Test: inline test module + the fixtures file

**Interfaces:**
- Consumes: `referral_catalog::{CatalogRequest, decode_catalog_request}`; Task 1/2 types.
- Produces: `PexFrame::{IntroduceRequest(Box<IntroduceRequest>), Introduction(Box<Introduction>)}`, `encode_pex_frame(&PexFrame) -> Result<Vec<u8>, ReferralCodecError>`, `PexDecoded::{Catalog(CatalogRequest), Frame(PexFrame)}`, `decode_pex_frame_or_catalog(&[u8]) -> Result<PexDecoded, ReferralCodecError>`.

- [ ] **Step 1: Write the failing tests (frame round-trip + fallback):**

```rust
#[test]
fn pex_frame_round_trips_and_bare_catalog_falls_back() {
    use crate::referral_catalog::{encode_catalog_request, sign_catalog_request};
    let from = mint_test_owner(0x11);
    let broker = mint_test_owner(0x22);
    // A tagged IntroduceRequest frame decodes as a Frame.
    let ir = sign_introduce_request(
        &from.device_key, from.owner, broker.owner, OwnerAddr([0x33; 16]), reach(), from.cert.clone(),
    );
    let frame = PexFrame::IntroduceRequest(Box::new(ir.clone()));
    let bytes = encode_pex_frame(&frame).unwrap();
    match decode_pex_frame_or_catalog(&bytes).unwrap() {
        PexDecoded::Frame(PexFrame::IntroduceRequest(g)) => assert_eq!(*g, ir),
        other => panic!("expected IntroduceRequest frame, got {other:?}"),
    }
    // A BARE (2a) CatalogRequest — a 4-key map — falls back to Catalog, never
    // mis-decoding as a single-key frame.
    let cr = sign_catalog_request(&from.device_key, from.owner, broker.owner, from.cert.clone());
    let bare = encode_catalog_request(&cr).unwrap();
    match decode_pex_frame_or_catalog(&bare).unwrap() {
        PexDecoded::Catalog(g) => assert_eq!(g, cr),
        other => panic!("expected Catalog fallback, got {other:?}"),
    }
}
```

- [ ] **Step 2: Run to verify failure.** → FAIL.

- [ ] **Step 3: Implement the enum + fallback decoder:**

```rust
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
#[derive(Debug)]
pub enum PexDecoded {
    /// A bare 2a `CatalogRequest` (browse). Fallback path.
    Catalog(crate::referral_catalog::CatalogRequest),
    /// A tagged 2b frame.
    Frame(PexFrame),
}

pub fn encode_pex_frame(frame: &PexFrame) -> Result<Vec<u8>, ReferralCodecError> {
    let mut out = Vec::new();
    ciborium::into_writer(frame, &mut out).map_err(|e| ReferralCodecError::Encode(e.to_string()))?;
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
```
Add `use crate::referral_catalog::{decode_catalog_request};` to the imports.

- [ ] **Step 4: Run to verify pass.** → PASS.

- [ ] **Step 5: Write `zeb376_intro_fixtures.rs`** mirroring `zeb375_pex_fixtures.rs` exactly (fixed-input builders → `encode_*` → `pin_hex` byte-compare against `EXPECTED_*_HEX` with the `FILL_AFTER` regen panic, plus a structural `ciborium::Value` key-order assertion). Pin: `IntroduceRequest` (keys `["a","d","x","r","c","s"]`), `Introduction` (keys `["v","d","u","c","r","t","s","e"]`), and `PexFrame::IntroduceRequest` (single-key `["ir"]`). Use a fixed `ReachabilityAnnouncePayload` fixture (`iroh_node_id:[0x11;32]`, `home_relay_url:"https://r"`, `direct_addresses:vec![]`, `announced_at_ms:1`, `identity_signature:[0x22;64]`, `butler_set:vec![]`, `bs_at:0`), fixed owners via `mint_test_owner`, fixed `sig:[0x09;64]`. Register `mod zeb376_intro_fixtures;` in `wire_format_tests.rs`.

- [ ] **Step 6: Regenerate + pin the hex.** Run the fixture tests once with `cargo nextest run --locked --features test-fixtures -E 'test(zeb376_intro_fixtures)'` (they panic `REGENERATE …`), paste the printed hex into the `EXPECTED_*_HEX` consts, re-run → PASS. Also run `cargo nextest run --locked --features test-fixtures -E 'test(zeb375_pex_fixtures)'` → still PASS (byte-pinned 2a bytes unchanged — the wire-compat proof). `--features test-fixtures` is REQUIRED (the wire_format fixtures compile against the deterministic-crypto helpers behind that feature); `--locked` matches CI's dependency graph.

- [ ] **Step 7: Gate + commit.** fmt + clippy, then `git commit -m "feat(zeb-376): PexFrame enum + fallback decode + byte-pinned wire fixtures"`.

---

### Task 4: `PeerIntroPolicy` persistence in `ConnectivitySettings`

**Files:**
- Modify: `src-tauri/src/connectivity_settings.rs` (struct at `9-39`, `Default` at `64-76`, `fail_closed_defaults` at `301-317`, helper region `41-62`)
- Test: inline `#[cfg(test)]` round-trip test (mirror the existing settings tests)

**Interfaces:**
- Consumes: `crate::friend_graph::PeerIntroPolicy` (exists, `friend_graph.rs:118`, variants Open/FriendsOfFriends(default)/AskMe/Closed, serde tags `"open"/"fof"/"ask"/"closed"`).
- Produces: `ConnectivitySettings.peer_intro_policy: PeerIntroPolicy`; `default_peer_intro_policy() -> PeerIntroPolicy`.

- [ ] **Step 1: Write the failing tests:**

```rust
#[test]
fn peer_intro_policy_defaults_to_fof_and_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("connectivity-settings.json");
    // Fresh install default = FriendsOfFriends.
    assert_eq!(
        ConnectivitySettings::load_or_default(&path).peer_intro_policy,
        crate::friend_graph::PeerIntroPolicy::FriendsOfFriends,
    );
    let mut s = ConnectivitySettings::default();
    s.peer_intro_policy = crate::friend_graph::PeerIntroPolicy::AskMe;
    s.save(&path).unwrap();
    assert_eq!(
        ConnectivitySettings::load_or_default(&path).peer_intro_policy,
        crate::friend_graph::PeerIntroPolicy::AskMe,
    );
}

#[test]
fn corrupt_settings_fails_closed_to_closed_policy() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("connectivity-settings.json");
    std::fs::write(&path, b"{ this is not json").unwrap();
    // A corrupt file must NOT silently widen the policy: fail closed to Closed.
    assert_eq!(
        ConnectivitySettings::load_or_default(&path).peer_intro_policy,
        crate::friend_graph::PeerIntroPolicy::Closed,
    );
}
```

- [ ] **Step 2: Run to verify failure.** `cargo nextest run --locked --features test-fixtures -E 'test(peer_intro_policy)'` → FAIL.

- [ ] **Step 3: Implement.** Add the field to the struct (after `friend_auto_accept_known`, `9-39`):
```rust
    /// ZEB-376 (Friends Phase 2b): per-user policy for inbound friend-vouched
    /// introductions. Enforced on X's node when it receives an `Introduction`
    /// (never the voucher's). Fresh-install default `FriendsOfFriends`; a
    /// corrupt file fails closed to `Closed` (see `fail_closed_defaults`).
    #[serde(default = "default_peer_intro_policy")]
    pub peer_intro_policy: crate::friend_graph::PeerIntroPolicy,
```
Add the helper next to `default_friend_auto_accept_known` (`41-62`):
```rust
/// Default for [`ConnectivitySettings::peer_intro_policy`]: `FriendsOfFriends`
/// (arc §4.2 default — accept an introduction only when the voucher is an Active
/// friend).
fn default_peer_intro_policy() -> crate::friend_graph::PeerIntroPolicy {
    crate::friend_graph::PeerIntroPolicy::FriendsOfFriends
}
```
Add to `Default` (`64-76`): `peer_intro_policy: default_peer_intro_policy(),`.
Add to `fail_closed_defaults` (`301-317`), with a comment matching the sibling fields:
```rust
            // ZEB-376: fail closed = Closed. A corrupt/unreadable file must never
            // silently accept an introduction from a stranger; Closed rejects all
            // inbound introductions until the file is fixed. (Distinct from the
            // fresh-install default `FriendsOfFriends`, which trusts active-friend
            // vouchers — the closed value is strictly the restrictive floor.)
            peer_intro_policy: crate::friend_graph::PeerIntroPolicy::Closed,
```

- [ ] **Step 4: Run to verify pass.** → PASS.
- [ ] **Step 5: Gate + commit.** fmt + clippy, then `git commit -m "feat(zeb-376): persist PeerIntroPolicy in ConnectivitySettings (fail-closed Closed)"`.

---

### Task 5: `get_peer_intro_policy` / `set_peer_intro_policy` IPC

**Files:**
- Modify: `src-tauri/src/lib.rs` (clone `get_/set_friend_auto_accept` at `54364-54440`; register in the friend cluster `58398-58419`)
- Test: inline `#[cfg(test)]` round-trip (mirror `set_friend_auto_accept_persists_round_trips`)

**Interfaces:**
- Consumes: `connectivity_settings_write_lock()` (`lib.rs:52369`), `NodeState.connectivity_settings_path`, `ConnectivitySettings.peer_intro_policy` (Task 4).
- Produces: IPC commands `get_peer_intro_policy() -> Result<PeerIntroPolicy, String>`, `set_peer_intro_policy(policy: PeerIntroPolicy) -> Result<(), String>`; event `connectivity-peer-intro-policy-changed`.

- [ ] **Step 1: Write the failing test** (mirror `set_friend_auto_accept_persists_round_trips`, `lib.rs:56204`): construct a `NodeState` with a tempdir `connectivity_settings_path`, `set_peer_intro_policy(AskMe)`, assert the file's `peer_intro_policy == AskMe`, then `get_peer_intro_policy() == AskMe`. (Follow the exact harness the auto-accept test uses.)

- [ ] **Step 2: Run to verify failure.** → FAIL (commands undefined).

- [ ] **Step 3: Implement** — clone `set_friend_auto_accept` (`54364-54418`) and `get_friend_auto_accept` (`54420-54440`) verbatim, swapping the field, the arg, the emit name, and the `None`-path default:
```rust
/// Set the per-user introduction policy (ZEB-376) and persist it to
/// `connectivity-settings.json`. Unlike the friend-acceptor's auto-accept toggle
/// (which is captured by value at start_node), this applies LIVE: X's
/// `Introduction` handler reads the policy fresh from the settings file per
/// inbound introduction, so a change takes effect on the next introduction with
/// no restart.
#[tauri::command]
async fn set_peer_intro_policy(
    app: tauri::AppHandle,
    state: tauri::State<'_, Mutex<NodeState>>,
    policy: crate::friend_graph::PeerIntroPolicy,
) -> Result<(), String> {
    let path = {
        state.lock().map_err(|e| format!("NodeState poisoned: {e}"))?
            .connectivity_settings_path.clone()
    };
    let Some(path) = path else { return Err("connectivity_settings_path missing".into()); };
    {
        let _settings_guard = connectivity_settings_write_lock().lock().await;
        let rmw_path = path.clone();
        tokio::task::spawn_blocking(move || {
            let mut settings =
                connectivity_settings::ConnectivitySettings::load_or_default(&rmw_path);
            settings.peer_intro_policy = policy;
            settings.save(&rmw_path).map_err(|e| format!("save connectivity-settings: {e}"))
        })
        .await
        .map_err(|e| format!("save connectivity-settings task: {e}"))??;
    }
    if let Err(e) = app.emit(
        "connectivity-peer-intro-policy-changed",
        serde_json::json!({ "policy": policy }),
    ) {
        tracing::warn!(error = %e, "set_peer_intro_policy: emit failed");
    }
    Ok(())
}

/// Read the current introduction policy from the persisted settings. Returns the
/// spec default (`FriendsOfFriends`) when the settings path is not initialized.
#[tauri::command]
async fn get_peer_intro_policy(
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<crate::friend_graph::PeerIntroPolicy, String> {
    let path = {
        state.lock().map_err(|e| format!("NodeState poisoned: {e}"))?
            .connectivity_settings_path.clone()
    };
    let Some(path) = path else {
        return Ok(crate::friend_graph::PeerIntroPolicy::FriendsOfFriends);
    };
    Ok(connectivity_settings::ConnectivitySettings::load_or_default(&path).peer_intro_policy)
}
```
Register both in the `generate_handler!` friend cluster (`58398-58419`), next to `get_friend_auto_accept`:
```rust
            set_peer_intro_policy,
            get_peer_intro_policy,
```

- [ ] **Step 4: Run to verify pass.** → PASS.
- [ ] **Step 5: Gate + commit.** fmt + clippy, then `git commit -m "feat(zeb-376): get/set_peer_intro_policy IPC + live-apply doc"`.

---

### Task 6: `decide_introduction` pure policy function

**Files:**
- Modify: `src-tauri/src/friend_intro.rs`
- Test: inline test module

**Interfaces:**
- Consumes: `crate::friend_graph::PeerIntroPolicy`.
- Produces: `IntroDecision::{Proceed, Stage, Reject}`, `decide_introduction(policy: PeerIntroPolicy, voucher_is_active_friend: bool) -> IntroDecision`.

- [ ] **Step 1: Write the failing test (full truth table):**

```rust
#[test]
fn decide_introduction_truth_table() {
    use crate::friend_graph::PeerIntroPolicy::*;
    // Open: always proceed, regardless of voucher.
    assert_eq!(decide_introduction(Open, true), IntroDecision::Proceed);
    assert_eq!(decide_introduction(Open, false), IntroDecision::Proceed);
    // FriendsOfFriends: proceed iff the voucher is an Active friend.
    assert_eq!(decide_introduction(FriendsOfFriends, true), IntroDecision::Proceed);
    assert_eq!(decide_introduction(FriendsOfFriends, false), IntroDecision::Reject);
    // AskMe: always stage a prompt (voucher-active is irrelevant to staging).
    assert_eq!(decide_introduction(AskMe, true), IntroDecision::Stage);
    assert_eq!(decide_introduction(AskMe, false), IntroDecision::Stage);
    // Closed: always reject.
    assert_eq!(decide_introduction(Closed, true), IntroDecision::Reject);
    assert_eq!(decide_introduction(Closed, false), IntroDecision::Reject);
}
```

- [ ] **Step 2: Run to verify failure.** → FAIL.

- [ ] **Step 3: Implement:**

```rust
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
```

- [ ] **Step 4: Run to verify pass.** → PASS.
- [ ] **Step 5: Gate + commit.** fmt + clippy, then `git commit -m "feat(zeb-376): decide_introduction policy fn (Open/FoF/AskMe/Closed)"`.

---

### Task 7: Extract `link_over_connection` (Path-A tail, `FriendOrigin`-parameterized)

**Files:**
- Modify: `src-tauri/src/lib.rs` — factor the tail of `connectivity_add_friend_by_key_inner` (from `let (mut send, mut recv) = … conn.open_bi()` onward, currently ~`54810`-`55059`) into a new `pub(crate) async fn link_over_connection`; the Case-B caller calls it with `FriendOrigin::MutualKey` after its connect+retry.
- Test: rely on the existing Case-B integration tests as the regression gate (behavior-preserving refactor).

**Interfaces:**
- Consumes: `AddFriendOutcome` (`lib.rs:54540`), `apply_handshaked_friend` (`lib.rs:51430`), `friend_rendezvous::{generate_ephemeral, derive_friendship_secret}`, the `FriendLinkRequest`/`FriendLinkResponse` codec, `SelfHandshakeReachability`.
- Produces:
```rust
pub(crate) async fn link_over_connection(
    conn: iroh::endpoint::Connection,
    dial_config: HandshakeDialConfig,
    origin: crate::friend_graph::FriendOrigin,       // MutualKey (Case-B) | Introduction (Path C)
    expected_peer: Option<crate::owner_state_types::OwnerAddr>, // None (Case-B) | Some(subject) (Path C)
    self_owner: crate::owner_state_types::OwnerAddr,
    self_display: Option<String>,
    self_enrollment: harmony_owner::certs::EnrollmentCert,
    self_device2_signing_key: std::sync::Arc<ed25519_dalek::SigningKey>,
    self_reachability: Option<crate::iroh_friend_acceptor::SelfHandshakeReachability>,
    keytree: std::sync::Arc<crate::owner_state_crypto::KeyTree>,
    crdt_state: std::sync::Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
    hlc_tracker: std::sync::Arc<tokio::sync::Mutex<std::collections::BTreeMap<String, crate::owner_state_types::Hlc>>>,
    device_id: String,
    peer_label: String,                              // for logs only (identity/owner hex)
) -> Result<AddFriendOutcome, String>
```

- [ ] **Step 1: Move the tail verbatim** into `link_over_connection`, taking `conn` (already connected) and doing `conn.open_bi()` → build+sign token-less `FriendLinkRequest` → write → read `FriendLinkResponse` → `Pending` short-circuit → verify accept → **`expected_peer` check** → derive secret → build `FriendEntry` → `apply_handshaked_friend` → `Ok(AddFriendOutcome::Linked{..})`. Two changes to the moved code: (a) `FriendEntry { … established_via: crate::friend_graph::FriendOrigin::MutualKey, … }` (currently `lib.rs:55020`) becomes `established_via: origin,`; (b) right after the accept-cert verification (step 8, `~lib.rs:55010`), insert `if let Some(exp) = expected_peer { if accepted.from_addr != exp { return Err("link: accept came from an unexpected owner".into()); } }`. Replace `identity_pub_hex` log references with `peer_label`.

- [ ] **Step 2: Rewire the Case-B caller.** In `connectivity_add_friend_by_key_inner`, after the connect+retry loop yields `conn` (the B4 diverse-relay re-resolve retry STAYS here — it is Case-B-specific), replace the moved tail with a single call (`expected_peer` is `None` — Case-B learns the owner from the accept):
```rust
    return link_over_connection(
        conn, dial_config, crate::friend_graph::FriendOrigin::MutualKey, None,
        self_owner, self_display, self_enrollment, self_device2_signing_key, self_reachability,
        keytree, crdt_state, hlc_tracker, device_id, identity_pub_hex,
    )
    .await;
```

- [ ] **Step 3: Run the Case-B regression gate.** The extraction must be behavior-preserving:
`cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(add_friend_by_key) + test(friend_token_roundtrip) + test(path_a)'` → all PASS (same outcomes as before the move). If any Case-B test changes outcome, the extraction diverged — diff the moved block against the pre-move tail.

- [ ] **Step 4: Gate + commit.** fmt + clippy `--all-targets`, then `git commit -m "refactor(zeb-376): extract link_over_connection (FriendOrigin-parameterized Path-A tail)"`.

---

### Task 8: `PendingOutboundIntroductions` + `AcceptInlineIntroduced` (your-side pre-auth)

**Files:**
- Modify: `src-tauri/src/friend_requests.rs` (new store), `src-tauri/src/iroh_friend_acceptor.rs` (`ConsentDecision` variant + `resolve_consent_consuming_approval` + `process_friend_request` origin override + acceptor field/builder + the new consent arm), `src-tauri/src/lib.rs` (`NodeState` field + thread to acceptor at `start_node` ~`9513-9520`).
- Test: inline `friend_requests.rs` tests for the store; the acceptor arm is exercised by the Task 15 e2e.

**Interfaces:**
- Consumes: `OwnerAddr`; the existing `ConsentDecision`, `decide_consent`, `resolve_consent_consuming_approval`.
- Produces:
```rust
pub struct PendingOutboundIntroductions { /* Mutex<HashMap<OwnerAddr, u64 /*recorded_ms*/>> */ }
impl PendingOutboundIntroductions {
    pub fn new() -> Self;
    pub fn record(&self, target: OwnerAddr, now_ms: u64);      // idempotent refresh of the deadline
    pub fn take(&self, target: &OwnerAddr, now_ms: u64) -> bool; // true+remove iff present AND fresh (< TTL)
}
pub const OUTBOUND_INTRO_TTL_MS: u64 = 10 * 60 * 1000; // 10 min
// ConsentDecision gains: AcceptInlineIntroduced  (→ established_via: Introduction)
// process_friend_request gains: origin_override: Option<FriendOrigin>
```

- [ ] **Step 1: Write the failing store tests** (in `friend_requests.rs`):
```rust
#[test]
fn outbound_intro_take_is_one_shot_and_ttl_bounded() {
    let s = PendingOutboundIntroductions::new();
    s.record(addr(1), 1_000);
    // Fresh + present → true, and consumed (one-shot).
    assert!(s.take(&addr(1), 1_500));
    assert!(!s.take(&addr(1), 1_600));
    // Expired records never authorize.
    s.record(addr(2), 1_000);
    assert!(!s.take(&addr(2), 1_000 + OUTBOUND_INTRO_TTL_MS + 1));
    // Unknown target → false.
    assert!(!s.take(&addr(3), 2_000));
}
```

- [ ] **Step 2: Run to verify failure.** → FAIL.

- [ ] **Step 3: Implement the store** (append to `friend_requests.rs`):
```rust
use std::collections::HashMap;

/// ZEB-376: process-local pre-authorization for introductions the user
/// initiated. When you send an `IntroduceRequest` for target X you `record(X)`;
/// X's inbound introduction-driven `FriendLinkRequest` then auto-accepts because
/// its authenticated sender is `take`-able here. One-shot + TTL-bounded so a
/// stale pre-auth can't silently accept an unrelated later dial. Not persisted
/// (ephemeral, like `PendingFriendRequests`).
#[derive(Default)]
pub struct PendingOutboundIntroductions {
    inner: std::sync::Mutex<HashMap<OwnerAddr, u64>>,
}
pub const OUTBOUND_INTRO_TTL_MS: u64 = 10 * 60 * 1000;
impl PendingOutboundIntroductions {
    pub fn new() -> Self { Self::default() }
    pub fn record(&self, target: OwnerAddr, now_ms: u64) {
        self.inner.lock().expect("outbound-intro mutex poisoned").insert(target, now_ms);
    }
    /// Remove + return true iff `target` was recorded AND still within the TTL.
    /// A present-but-expired entry is removed and returns false.
    pub fn take(&self, target: &OwnerAddr, now_ms: u64) -> bool {
        let mut m = self.inner.lock().expect("outbound-intro mutex poisoned");
        match m.remove(target) {
            Some(rec) => now_ms.saturating_sub(rec) < OUTBOUND_INTRO_TTL_MS,
            None => false,
        }
    }
}
```

- [ ] **Step 4: Run store tests.** → PASS.

- [ ] **Step 5: Extend the consent tree** (`iroh_friend_acceptor.rs`). Add the variant to `ConsentDecision` (`812-821`):
```rust
    /// ZEB-376: an introduction the user initiated — accept inline AND stamp
    /// `established_via: Introduction` (distinct from `AcceptInline`'s MutualKey).
    AcceptInlineIntroduced,
```
Extend `resolve_consent_consuming_approval` (`863-877`) with the pending-outbound handle + clock, checked BEFORE the regular approval:
```rust
fn resolve_consent_consuming_approval(
    pending: Option<&crate::friend_requests::PendingFriendRequests>,
    pending_outbound: Option<&crate::friend_requests::PendingOutboundIntroductions>,
    token_sig: Option<&[u8; 64]>,
    known: bool,
    auto_accept_known: bool,
    from: &OwnerAddr,
    now_ms: u64,
) -> ConsentDecision {
    let decision = decide_consent(token_sig, known, auto_accept_known, false);
    if matches!(decision, ConsentDecision::Pending) {
        if pending_outbound.map(|p| p.take(from, now_ms)).unwrap_or(false) {
            return ConsentDecision::AcceptInlineIntroduced;
        }
        if pending.map(|p| p.take_approved(from)).unwrap_or(false) {
            return ConsentDecision::AcceptInline;
        }
    }
    decision
}
```

- [ ] **Step 6: Add the origin override to `process_friend_request`.** Add a parameter `origin_override: Option<crate::friend_graph::FriendOrigin>` (last param). Change the `established_via` computation (`1057-1061`) to:
```rust
        established_via: origin_override.unwrap_or_else(|| {
            if req.token_sig.is_some() { FriendOrigin::Token } else { FriendOrigin::MutualKey }
        }),
```
Update the two existing call sites (the `TokenPath` and `AcceptInline` arms) to pass `None`.

- [ ] **Step 7: Add the acceptor field + builder + consent arm.** New field on the acceptor struct (beside `pending_requests`, `1253-1263`): `pending_outbound: Option<Arc<crate::friend_requests::PendingOutboundIntroductions>>` (default `None` in `with_config`); builder `with_pending_outbound(mut self, p: Option<Arc<…>>) -> Self`. Thread `self.pending_outbound.as_deref()` + `wall_now_ms()` into the `resolve_consent_consuming_approval` call (`1631-1637`). Add the new match arm (a copy of `AcceptInline` `1690-1717`, but calling `process_friend_request(…, Some(FriendOrigin::Introduction))`):
```rust
            ConsentDecision::AcceptInlineIntroduced => {
                let learned_at = self.next_hlc().await;
                let fresh_home_relay = self.current_fresh_home_relay();
                let accepted = {
                    let mut state = self.crdt_state.lock().await;
                    process_friend_request(
                        &mut state, learned_at, &req, self.self_owner, self.self_display.clone(),
                        &self.self_enrollment, &self.device2_signing_key, &self.keytree, now_secs,
                        self.self_statics.as_ref(), fresh_home_relay,
                        Some(crate::friend_graph::FriendOrigin::Introduction),
                    ).map_err(FriendAcceptError::Handshake)?
                };
                if let Some(pending) = self.pending_requests.as_ref() {
                    pending.clear_completed(&req.from_addr);
                }
                self.emit_friend_added(&req);
                accepted
            }
```

- [ ] **Step 8: Park the store on `NodeState` + thread to the acceptor.** Add `pending_outbound_introductions: Arc<PendingOutboundIntroductions>` to `NodeState` (constructed once, like `pending_friend_requests_for_state`). At the acceptor constructor (`lib.rs:9513-9520`), add `.with_pending_outbound(Some(Arc::clone(&pending_outbound_for_state)))`.

- [ ] **Step 9: Gate + commit.** fmt + clippy `--all-targets`, then:
```bash
git commit -m "feat(zeb-376): PendingOutboundIntroductions + AcceptInlineIntroduced consent (your-side pre-auth)"
```

---

### Task 9: PEX `serve()` `PexFrame` dispatch + F's broker arm

**Files:**
- Modify: `src-tauri/src/iroh_pex_acceptor.rs` (`serve()` decode + dispatch; new acceptor deps for F→X delivery), `src-tauri/src/friend_intro.rs` (pure `build_introduction_for_request`).
- Test: inline unit tests for `build_introduction_for_request`; the existing `referral_catalog_roundtrip_integration` regression gate for the Catalog path.

**Interfaces:**
- Consumes: `decode_pex_frame_or_catalog`, `PexDecoded`, `PexFrame`, `authenticate_introduce_request`, `sign_introduction`, `FriendGraph`, `browse_friend_referrals`'s resolve+dial pattern (`lib.rs:53622-53900`).
- Produces:
```rust
// friend_intro.rs — PURE broker decision (mirrors serve_catalog_for_request):
pub fn build_introduction_for_request(
    req: &IntroduceRequest,
    fg: &crate::friend_graph::FriendGraph,
    self_owner: OwnerAddr,
    self_enrollment: EnrollmentCert,
    device2: &SigningKey,
    at: Hlc,
    now_secs: u64,
) -> Result<Introduction, IntroBrokerError>;
pub enum IntroBrokerError { Auth(IntroAuthError), NotReferrable }
```

- [ ] **Step 1: Write the failing tests** for the pure broker step (in `friend_intro.rs`):
```rust
#[test]
fn broker_builds_introduction_only_for_active_referrable_target() {
    let requester = mint_test_owner(0x11);
    let broker = mint_test_owner(0x22);
    let target = mint_test_owner(0x33);
    let mut fg = crate::friend_graph::FriendGraph::default();
    fg.friends.insert(target.owner, active_referrable_entry(0x33, true));
    let req = sign_introduce_request(
        &requester.device_key, requester.owner, broker.owner, target.owner, reach(), requester.cert.clone(),
    );
    let intro = build_introduction_for_request(
        &req, &fg, broker.owner, broker.cert.clone(), &broker.device_key, hlc(1), 0,
    ).expect("active+referrable target → Introduction");
    // The Introduction relays the subject verbatim, vouched by the broker, aimed at X.
    assert_eq!(intro.voucher, broker.owner);
    assert_eq!(intro.to_addr, target.owner);
    assert_eq!(intro.subject, requester.owner);
    assert_eq!(intro.reachability, req.reachability);
    verify_introduction(&intro, broker.owner, target.owner, 0).expect("F's signature verifies on X");
    // Non-referrable target → NotReferrable (no envelope leaks a non-opted-in friend).
    fg.friends.insert(target.owner, active_referrable_entry(0x33, false));
    assert!(matches!(
        build_introduction_for_request(&req, &fg, broker.owner, broker.cert.clone(), &broker.device_key, hlc(1), 0),
        Err(IntroBrokerError::NotReferrable),
    ));
}
```
(Add an `active_referrable_entry(seed, referrable)` test helper mirroring the integration test's `friend_entry`.)

- [ ] **Step 2: Run to verify failure.** → FAIL.

- [ ] **Step 3: Implement `build_introduction_for_request`:**
```rust
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
/// Active + `referrable` friend, then build+sign an `Introduction` that relays
/// the subject (requester) + their cert + their reachability, vouched by F, aimed
/// at the target. Read-only over `fg`. Mirrors `serve_catalog_for_request`.
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
    let referrable = fg.friends.get(&req.target).is_some_and(|e| {
        e.status == crate::friend_graph::FriendStatus::Active && e.referrable
    });
    if !referrable {
        return Err(IntroBrokerError::NotReferrable);
    }
    Ok(sign_introduction(
        device2, self_owner, req.target, req.from_addr, req.enrollment.clone(),
        req.reachability.clone(), at, self_enrollment,
    ))
}
```

- [ ] **Step 4: Run to verify pass.** → PASS.

- [ ] **Step 5: Rewire `serve()` to dispatch (`iroh_pex_acceptor.rs`).** Replace the `let req = decode_catalog_request(&body)…` + auth + serve section with a `PexDecoded` match. `Catalog(req)` → the EXISTING pre-HLC auth + `serve_catalog_for_request` + write-back (unchanged). `Frame(PexFrame::IntroduceRequest(ir))` → F's broker arm: snapshot `friend_graph` under the crdt lock, `build_introduction_for_request(...)` (drop the lock), then SPAWN the F→X delivery (see Step 6) and write a small ack frame back. `Frame(PexFrame::Introduction(intro))` → **defer to Task 10** (for this task, decode + `tracing::debug!("introduction arm: Task 10")` + ack; Task 10 replaces this arm). Both intro arms require the acceptor's new deps (Step 7).

- [ ] **Step 6: Add the F→X delivery helper.** In `lib.rs` (near `browse_friend_referrals`), add `pub(crate) async fn deliver_introduction_to_target(...)` that mirrors `browse_friend_referrals`' resolve+dial verbatim but: resolves the TARGET (X) via Case-D from F's friend entry, dials `HARMONY_FRIEND_PEX_V1`, writes `encode_pex_frame(&PexFrame::Introduction(Box::new(intro)))`, and reads the ack (no catalog). The PEX acceptor's IntroduceRequest arm calls this in a `tokio::spawn` so `serve()` stays single-shot and non-blocking.

- [ ] **Step 7: Thread F's new deps onto `IrohFriendPexAcceptor`.** Add fields (with `with_*` builders, default `None`, mirroring the friend acceptor's optional-dep pattern): `pkarr_resolver`, `iroh_endpoint`, `owner_keytree` (for Case-D decrypt), `connectivity_settings_path` (X arm, Task 10), `pending_requests` + `pending_outbound` (X arm, Task 11), `event_emit` (X arm, Task 11). At the PEX-acceptor construction site (`lib.rs:9555-9574`), thread the same handles `browse_friend_referrals`/the friend acceptor already hold.

- [ ] **Step 8: Run the Catalog regression gate.** `cargo nextest run --locked --features test-fixtures -E 'test(referral_catalog_roundtrip)'` → PASS (browse still works through the new dispatch; the fallback path is exercised).

- [ ] **Step 9: Gate + commit.** fmt + clippy `--all-targets`, then:
```bash
git commit -m "feat(zeb-376): PEX serve() PexFrame dispatch + F broker arm (deliver Introduction to X)"
```

---

### Task 10: X's `Introduction` arm — verify + policy + Proceed(link)/Reject

**Files:**
- Modify: `src-tauri/src/iroh_pex_acceptor.rs` (replace the deferred Introduction arm from Task 9), `src-tauri/src/lib.rs` (add `complete_introduction` + thread X's self-dial handles onto the PEX acceptor; expose `build_self_handshake_reachability` as `pub(crate)` at `51387`).
- Test: the Task 15 e2e is the integration gate (Open policy); an inline routing test for the verify/policy decision.

**Interfaces:**
- Consumes: `verify_introduction`, `reachability_record::verify_inner_signature`, `decide_introduction`/`IntroDecision`, `ConnectivitySettings::load_or_default(&path).peer_intro_policy`, `link_over_connection` (Task 7), `endpoint_addr_from_routing` (lib.rs:50402), `dm_signing::device2_combined_pub`/the subject cert's device-#2 verifying key.
- Produces:
```rust
// lib.rs — the shared "X dials the introducee and links" action (also used by
// the AskMe accept path in Task 11):
#[allow(clippy::too_many_arguments)]
pub(crate) async fn complete_introduction(
    subject: crate::owner_state_types::OwnerAddr,           // the introducee (expected peer)
    reachability: crate::reachability_record::ReachabilityAnnouncePayload,
    iroh_endpoint: std::sync::Arc<crate::iroh_endpoint::IrohEndpoint>,
    dial_config: HandshakeDialConfig,
    self_owner: crate::owner_state_types::OwnerAddr,
    self_display: Option<String>,
    self_enrollment: harmony_owner::certs::EnrollmentCert,
    self_device2_signing_key: std::sync::Arc<ed25519_dalek::SigningKey>,
    self_reachability: Option<crate::iroh_friend_acceptor::SelfHandshakeReachability>,
    keytree: std::sync::Arc<crate::owner_state_crypto::KeyTree>,
    crdt_state: std::sync::Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
    hlc_tracker: std::sync::Arc<tokio::sync::Mutex<std::collections::BTreeMap<String, crate::owner_state_types::Hlc>>>,
    device_id: String,
) -> Result<AddFriendOutcome, String>;
```

- [ ] **Step 1: Implement `complete_introduction` (lib.rs).** Synthesize the dial target from the relayed reachability, connect on the friend ALPN, and link — passing `Some(subject)` as `expected_peer` so the handshake accept MUST come from the introducee (not an impostor at that address):
```rust
    let target_addr = endpoint_addr_from_routing(&reachability)
        .map_err(|e| format!("synthesize introducee addr: {e}"))?;
    let conn = tokio::time::timeout(
        dial_config.connect_timeout,
        iroh_endpoint.inner().connect(target_addr, crate::iroh_endpoint::alpn::HARMONY_FRIEND_V1),
    )
    .await
    .map_err(|_| "introducee unreachable: connect timeout".to_string())?
    .map_err(|e| format!("introducee unreachable: connect failed: {e}"))?;
    link_over_connection(
        conn, dial_config, crate::friend_graph::FriendOrigin::Introduction, Some(subject),
        self_owner, self_display, self_enrollment, self_device2_signing_key, self_reachability,
        keytree, crdt_state, hlc_tracker, device_id, hex::encode(subject.0),
    ).await
```
(`link_over_connection` gains `expected_peer: Option<OwnerAddr>` — see the amendment to Task 7 below; Case-B passes `None`.)

- [ ] **Step 2: Implement X's Introduction arm** (`iroh_pex_acceptor.rs`, replacing the Task 9 deferred arm). Under the crdt lock, verify + read `known`/`voucher_is_active_friend`; DROP the lock; read the policy fresh; decide; act:
```rust
PexDecoded::Frame(PexFrame::Introduction(intro)) => {
    let now_secs = crate::iroh_friend_acceptor::wall_now_secs();
    // 1. Verify F's vouch (voucher == a friend we know?, sig, subject cert).
    //    voucher_is_active_friend is read under the crdt lock.
    let voucher_active = {
        let s = self.crdt_state.lock().await;
        s.friend_graph.friends.get(&intro.voucher)
            .map(|e| e.status == crate::friend_graph::FriendStatus::Active).unwrap_or(false)
    };
    crate::friend_intro::verify_introduction(&intro, intro.voucher, self.self_owner, now_secs)
        .map_err(|e| format!("introduction verify: {e:?}"))?;
    // 2. Verify the RELAYED reachability is self-authenticated + fresh (the same
    //    inner check the Case-B initiator runs on a resolved record).
    let subj_vk = crate::dm_signing::device2_verifying_key(&intro.subject_cert)
        .ok_or_else(|| "introduction subject cert has no device-#2 key".to_string())?;
    crate::reachability_record::verify_inner_signature(
        &intro.reachability, &intro.subject, &intro.at, &subj_vk,
    ).map_err(|e| format!("relayed reachability inner-sig: {e:?}"))?;
    reachability_freshness_check(&intro.reachability, wall_now_ms())?; // window per Case-D
    // 3. Enforce policy (read fresh from settings — live-apply, no restart).
    let policy = self.connectivity_settings_path.as_ref()
        .map(|p| crate::connectivity_settings::ConnectivitySettings::load_or_default(p).peer_intro_policy)
        .unwrap_or(crate::friend_graph::PeerIntroPolicy::FriendsOfFriends);
    match crate::friend_intro::decide_introduction(policy, voucher_active) {
        crate::friend_intro::IntroDecision::Proceed => {
            // X dials the introducee (spawned; serve() stays single-shot).
            self.spawn_complete_introduction(&intro);
        }
        crate::friend_intro::IntroDecision::Stage => {
            // Task 11 fills this (record an offer + emit).
            self.stage_introduction_offer(&intro);
        }
        crate::friend_intro::IntroDecision::Reject => {
            tracing::debug!(voucher = %hex::encode(intro.voucher.0),
                "introduction rejected by PeerIntroPolicy");
        }
    }
    // ack (write a small PexFrame ack the same way the catalog path writes back)
}
```
`device2_verifying_key` is a thin `dm_signing` helper returning the cert's device-#2 `VerifyingKey` (add if absent — it parallels the existing `device2_combined_pub`). `reachability_freshness_check` bounds `announced_at_ms` against a window (reuse the Case-D freshness constant). `spawn_complete_introduction` gathers the acceptor's threaded self-dial handles + builds `SelfHandshakeReachability` via `build_self_handshake_reachability(self_statics, current_fresh_home_relay)` and `tokio::spawn`s `complete_introduction(...)`.

- [ ] **Step 3: Amend Task 7 — `link_over_connection` gains `expected_peer: Option<OwnerAddr>`.** After the accept-cert verification (step 8, `lib.rs:55010`), add: `if let Some(exp) = expected_peer { if accepted.from_addr != exp { return Err("introduction: accept came from an unexpected owner".into()); } }`. The Case-B caller (Task 7 step 2) passes `None`.

- [ ] **Step 4: Thread X's self-dial handles onto the PEX acceptor** (extends Task 9 step 7): `self_statics: Option<SelfHandshakeStatics>`, a `current_fresh_home_relay()` reader (mirror the friend acceptor's), `dial_config` (or `HandshakeDialConfig::from_env()` at dial time). Wire at `lib.rs:9555-9574`.

- [ ] **Step 5: Test.** Run the Task 15 e2e's Open-policy case (written later) — or, in isolation, an inline unit test that drives `decide_introduction` routing with a stub. Then the full backend gate: `scripts/test-select --context task`.

- [ ] **Step 6: Gate + commit.** fmt + clippy `--all-targets`, then:
```bash
git commit -m "feat(zeb-376): X Introduction arm — verify + reachability check + policy + Proceed link"
```

---

### Task 11: AskMe staging via the pending-request inbox

**Files:**
- Modify: `src-tauri/src/friend_requests.rs` (`PendingKind` discriminant + `record_introduction_offer` + list projection), `src-tauri/src/iroh_pex_acceptor.rs` (`stage_introduction_offer`), `src-tauri/src/lib.rs` (`accept_friend_request` branches on kind; `PendingFriendRequestDto.introduced_by`).
- Test: inline `friend_requests.rs` tests; the Task 15 e2e's AskMe case.

**Interfaces:**
- Consumes: `PendingFriendRequests`, `Introduction`, `emit_friend_request_received`, `complete_introduction` (Task 10).
- Produces:
```rust
pub enum PendingKind { LinkRequest, IntroductionOffer(Box<StoredIntroductionOffer>) }
pub struct StoredIntroductionOffer {
    pub voucher: OwnerAddr,
    pub subject: OwnerAddr,
    pub reachability: crate::reachability_record::ReachabilityAnnouncePayload,
}
// PendingInbound gains: pub kind: PendingKind
impl PendingFriendRequests {
    pub fn record_introduction_offer(&self, subject: OwnerAddr, display: Option<String>, now_ms: u64, offer: StoredIntroductionOffer);
    pub fn take_offer(&self, subject: &OwnerAddr) -> Option<StoredIntroductionOffer>; // remove + return for the accept path
}
// PendingFriendRequestDto gains: pub introduced_by: Option<String> (voucher owner hex; None for a plain LinkRequest)
```

- [ ] **Step 1: Write the failing tests:**
```rust
#[test]
fn introduction_offer_stages_and_take_consumes() {
    let store = PendingFriendRequests::new();
    let offer = StoredIntroductionOffer { voucher: addr(2), subject: addr(1), reachability: fixture_reach() };
    store.record_introduction_offer(addr(1), Some("alice".into()), 1_000, offer);
    // Surfaces in the inbox with its introduced_by voucher.
    let list = store.list();
    assert_eq!(list.len(), 1);
    assert!(matches!(&list[0].1.kind, PendingKind::IntroductionOffer(o) if o.voucher == addr(2)));
    // take_offer consumes it once.
    assert!(store.take_offer(&addr(1)).is_some());
    assert!(store.take_offer(&addr(1)).is_none());
    assert!(store.list().is_empty());
}
```
(Add `record_inbound` sets `kind: PendingKind::LinkRequest` — keep the existing Path-A tests green.)

- [ ] **Step 2: Run to verify failure.** → FAIL.

- [ ] **Step 3: Implement** the `PendingKind`/`StoredIntroductionOffer` additions, `PendingInbound.kind` (default `LinkRequest` in `record_inbound`), `record_introduction_offer`, `take_offer`. Keep `list` returning `(OwnerAddr, PendingInbound)` (now carrying `kind`).

- [ ] **Step 4: Run to verify pass.** → PASS.

- [ ] **Step 5: Implement `stage_introduction_offer`** (`iroh_pex_acceptor.rs`): record the offer via `record_introduction_offer(intro.subject, /*display*/None, wall_now_ms(), StoredIntroductionOffer{ voucher: intro.voucher, subject: intro.subject, reachability: intro.reachability.clone() })`, then `self.emit_introduction_offer()` (the acceptor's `FriendEventEmit` handle → `emit_friend_request_received`, reusing the existing event).

- [ ] **Step 6: Branch `accept_friend_request` on kind** (`lib.rs:53988`). If the pending entry for `owner_id_hex` is an `IntroductionOffer`, `take_offer(subject)` and run `complete_introduction(subject, offer.reachability, …NodeState self-dial handles…)` (X dials the introducee — the SAME action as Task 10's Proceed), then emit `friend-list-changed`. Otherwise the existing `store.approve(owner)` path (Path-A), unchanged. `decline_friend_request` drops either kind (existing `decline`).

- [ ] **Step 7: Add `introduced_by` to the DTO.** `PendingFriendRequestDto` (`lib.rs:53921`) gains `#[serde(rename_all = camelCase)] introduced_by: Option<String>` (voucher owner hex when the entry is an `IntroductionOffer`, else `None`); populate it in `list_pending_friend_requests`. Mirror the TS type in Task 14.

- [ ] **Step 8: Gate + commit.** fmt + clippy `--all-targets`, then:
```bash
git commit -m "feat(zeb-376): AskMe staging — IntroductionOffer inbox kind + accept-runs-link + introducedBy DTO"
```

---

### Task 12: `request_introduction` IPC (you→F)

**Files:**
- Modify: `src-tauri/src/lib.rs` (new IPC + registration; reuse `browse_friend_referrals`' resolve+dial; build self reachability via `reachability_record::build_signed_payload_with_key`).
- Test: inline test for the self-reachability assembly; the Task 15 e2e for the full path.

**Interfaces:**
- Consumes: `PendingOutboundIntroductions` (Task 8, on `NodeState`), `sign_introduce_request`/`encode_pex_frame`/`PexFrame::IntroduceRequest`, `resolve_friend_case_d`, `build_signed_payload_with_key` (`reachability_record.rs:313`), the live iroh endpoint (node_id / home relay / direct addrs).
- Produces: IPC `request_introduction(via_owner_id_hex: String, target_owner_id_hex: String) -> Result<(), String>`; event reuse `friend-list-changed` when the eventual link lands.

- [ ] **Step 1: Write the failing test** for the self-reachability assembly: a `pub(crate) fn build_self_reachability_announce(iroh_node_id, home_relay, direct_addrs, actor, hlc, device2_key) -> ReachabilityAnnouncePayload` that calls `build_signed_payload_with_key(...)` with an empty butler set; assert the result `verify_inner_signature`s against the device-#2 key. (Reuse whatever the reachability publisher assembles if it is already exposed as a callable; otherwise this thin wrapper is the seam — empty butler set is acceptable for a first-contact dial target.)

- [ ] **Step 2: Run to verify failure.** → FAIL.

- [ ] **Step 3: Implement `build_self_reachability_announce`** (the thin wrapper over `build_signed_payload_with_key`, `announced_at_ms = wall_now_ms()`, `butler_set = vec![]`, `bs_at = 0`).

- [ ] **Step 4: Implement `request_introduction`** modeled on `browse_friend_referrals` (`lib.rs:53622`): snapshot resolver/crdt/keytree/iroh_endpoint/self_owner/dm_outbox + `pending_outbound_introductions` from `NodeState`; parse both hex owner ids; require the VIA friend (F) is Active with a sealed rendezvous secret; **`pending_outbound_introductions.record(target, wall_now_ms())`** (pre-authorize the target X BEFORE dialing F, so X's return link auto-accepts even under a fast round-trip); build the self reachability announce; `sign_introduce_request(self_device2, self_owner, F, target, reachability, self_enrollment)`; Case-D resolve F + dial `HARMONY_FRIEND_PEX_V1`; write `encode_pex_frame(&PexFrame::IntroduceRequest(Box::new(req)))` with the `[u32 LE len][body]` framing; read F's ack. Return `Ok(())` on ack (the actual X→you link arrives asynchronously and surfaces via `friend-list-changed`). Register `request_introduction` in the friend cluster (`58398-58419`).

- [ ] **Step 5: Run tests + backend gate.** `scripts/test-select --context task`.

- [ ] **Step 6: Gate + commit.** fmt + clippy `--all-targets`, then:
```bash
git commit -m "feat(zeb-376): request_introduction IPC (self-reachability in envelope + pending-outbound pre-auth)"
```

---

### Task 13: Abuse hygiene — per-voucher cap + `(voucher, subject)` dedupe

**Files:**
- Modify: `src-tauri/src/friend_intro.rs` (`IntroRateLimiter`), `src-tauri/src/iroh_pex_acceptor.rs` (apply on both intro arms).
- Test: inline `friend_intro.rs` tests.

**Interfaces:**
- Produces:
```rust
pub struct IntroRateLimiter { /* Mutex<HashMap<OwnerAddr, sliding window>> + Mutex<HashSet<(OwnerAddr,OwnerAddr)>> with ts */ }
pub const INTRO_PER_VOUCHER_WINDOW_MS: u64 = 60 * 60 * 1000; // 1h
pub const INTRO_PER_VOUCHER_MAX: usize = 20;                  // per voucher per window
pub const INTRO_DEDUPE_TTL_MS: u64 = 5 * 60 * 1000;          // 5 min
impl IntroRateLimiter {
    pub fn new() -> Self;
    /// Returns Ok(()) to admit; Err(reason) to shed. Admitting records the event.
    /// Sheds if the per-`key` count in the window is at the cap, or the
    /// `(key, subject)` pair was seen within the dedupe TTL. Callers LOG the shed.
    pub fn admit(&self, key: OwnerAddr, subject: OwnerAddr, now_ms: u64) -> Result<(), &'static str>;
}
```

- [ ] **Step 1: Write the failing tests** — admit up to `INTRO_PER_VOUCHER_MAX` distinct subjects from one voucher, then the next distinct subject sheds `"per-voucher cap"`; a repeat `(voucher, subject)` within `INTRO_DEDUPE_TTL_MS` sheds `"duplicate"`; the same pair after the TTL admits again; entries older than the window don't count toward the cap.

- [ ] **Step 2: Run to verify failure.** → FAIL.

- [ ] **Step 3: Implement `IntroRateLimiter`** (sliding-window `VecDeque<u64>` timestamps per key, pruned to the window on each `admit`; a `HashMap<(OwnerAddr,OwnerAddr), u64>` last-seen for dedupe). Pure over `now_ms`; `Mutex`-guarded; no `.await` under the lock.

- [ ] **Step 4: Run to verify pass.** → PASS.

- [ ] **Step 5: Apply in the acceptor.** Park one `Arc<IntroRateLimiter>` on the PEX acceptor. On F's `IntroduceRequest` arm: `admit(req.from_addr, req.target, now)` (per-requester cap + dedupe of repeated (requester,target)); on X's `Introduction` arm: `admit(intro.voucher, intro.subject, now)` (per-voucher cap + dedupe). On `Err(reason)`: `tracing::warn!(reason, key=…, "introduction shed by rate limiter")` and drop (no leak to the peer beyond a generic ack). Policy enforcement is the primary defense; this is DoS hygiene layered before it.

- [ ] **Step 6: Gate + commit.** fmt + clippy `--all-targets`, then:
```bash
git commit -m "feat(zeb-376): IntroRateLimiter — per-voucher cap + (voucher,subject) dedupe with logged shedding"
```

---

### Task 14: Frontend — policy dropdown, request-intro button, offer badge

**Files:**
- Modify: `src/lib/friend-service.ts`, `src/lib/components/FriendsPanel.svelte`
- Test: `src/lib/friend-service.test.ts` (or the panel's existing vitest) — mirror the auto-accept/browse tests.

**Interfaces:**
- Consumes: IPC `get_peer_intro_policy`/`set_peer_intro_policy`/`request_introduction`; the `introducedBy` DTO field (Task 11).
- Produces: `FriendService.{getPeerIntroPolicy, setPeerIntroPolicy, requestIntroduction}`; a `PeerIntroPolicy` TS union; UI: policy `<select>`, request-intro button, offer badge.

- [ ] **Step 1: Write the failing vitest** — assert `getPeerIntroPolicy()` invokes `get_peer_intro_policy`; `setPeerIntroPolicy('ask')` invokes `set_peer_intro_policy` with `{ policy: 'ask' }`; `requestIntroduction('ffee…','aabb…')` invokes `request_introduction` with `{ viaOwnerIdHex: 'ffee…', targetOwnerIdHex: 'aabb…' }` (camelCase per CLAUDE.md).

- [ ] **Step 2: Run to verify failure.** `npx vitest run friend-service` → FAIL.

- [ ] **Step 3: Implement the service methods** (mirror `getAutoAccept`/`setAutoAccept`/`browseReferrals`, `friend-service.ts:297-339`):
```ts
export type PeerIntroPolicy = 'open' | 'fof' | 'ask' | 'closed';

/** Read the current inbound-introduction policy. */
async getPeerIntroPolicy(): Promise<PeerIntroPolicy> {
  return this.invoke<PeerIntroPolicy>('get_peer_intro_policy', {});
}
/** Set the inbound-introduction policy (applies live on the next introduction). */
async setPeerIntroPolicy(policy: PeerIntroPolicy): Promise<void> {
  await this.invoke<void>('set_peer_intro_policy', { policy });
}
/** Ask a friend (`viaOwnerIdHex`) to introduce us to one of their referrable
 *  friends (`targetOwnerIdHex`). The eventual link surfaces via friend-list-changed. */
async requestIntroduction(viaOwnerIdHex: string, targetOwnerIdHex: string): Promise<void> {
  await this.invoke<void>('request_introduction', { viaOwnerIdHex, targetOwnerIdHex });
}
```
Add `introducedBy: string | null` to `PendingFriendRequestDto` (Task 11's DTO field).

- [ ] **Step 4: Run to verify pass.** → PASS.

- [ ] **Step 5: Wire the UI** (`FriendsPanel.svelte`):
  - **Policy dropdown:** a new `action-block` beside the auto-accept section (`1138-1154`); a `peerIntroPolicy` state quartet mirroring `autoAccept*` (`156-160`, `loadPeerIntroPolicy` like `loadAutoAccept` `223-232` called in the mount effect `290-325`, `handlePeerIntroPolicyChange` like `handleAutoAcceptToggle` `684-697`). A `<select data-testid="peer-intro-policy-select">` with Open/Friends-of-friends/Ask me/Closed options bound to `peerIntroPolicy`.
  - **Request-intro button:** in the referral-item `<li>` (`868-902`), replace the `{#if r.alreadyFriend}` badge-only block with `{#if r.alreadyFriend}<badge>{:else}<button data-testid="request-intro-btn" onclick={() => handleRequestIntro(f.ownerIdHex, r.ownerIdHex)}>Request introduction</button>{/if}`; `handleRequestIntro(viaHex, targetHex)` calls `service.requestIntroduction`, with an in-flight guard + transient status (mirror `handleBrowseReferrals`).
  - **Offer badge:** in the `friend-requests-section` (`962-1018`), when `req.introducedBy` is set, render an `introduced by {shortId(req.introducedBy)}` badge beside the name. Accept/Decline reuse `handleAccept`/`handleDecline` unchanged.

- [ ] **Step 6: Frontend gate + commit.** `npx tsc --noEmit` + `npx vitest run`, then:
```bash
git commit -m "feat(zeb-376): FriendsPanel — policy dropdown, request-introduction button, offer badge"
```

---

### Task 15: Three-node (You–F–X) end-to-end integration test

**Files:**
- Create: `src-tauri/tests/identity/introduction_broker_roundtrip_integration.rs` (register in `tests/identity_tests.rs` or the identity module aggregator, mirroring `referral_catalog_roundtrip_integration.rs`).
- Test: this file.

**Interfaces:** Consumes the full stack (no new production code).

- [ ] **Step 1: Build the harness** mirroring `referral_catalog_roundtrip_integration.rs` (`tests/identity/…:1-357`): three hermetic nodes You/F/X, each with a real `IrohEndpoint` + `IrohZenohLinkManager` + accept loop + a `MultiplexHandshakeDispatcher`. F and X run REAL `IrohFriendPexAcceptor`s (threaded with the Task 9/10 deps) AND real friend acceptors (You runs a real friend acceptor to accept X's inbound link). Seed friend graphs directly: You↔F Active (both directions), F↔X Active with X `referrable` in F's graph. Use `#[serial_test::serial]`, 30s per-IO + 90s outer timeouts, `RelayMode::Disabled` + `hermetic_dns_resolver()`, loopback `EndpointAddr` synthesis (`server_addr` pattern).

- [ ] **Step 2: Write the Open-policy assertion.** Set X's `peer_intro_policy = Open` (write X's `connectivity-settings.json`). You calls the `request_introduction` path to F for target X. Poll until You's friend graph holds X and X's holds You, both `established_via == FriendOrigin::Introduction`; assert **F's friend graph holds NO You↔X edge** (F dropped out); assert both sides carry a sealed rendezvous secret (Case-D armed).

- [ ] **Step 3: Write the AskMe-policy assertion.** Fresh nodes, X `peer_intro_policy = AskMe`. After the request, poll X's `list_pending_friend_requests` until an `IntroductionOffer` with `introducedBy == F` appears; assert NO link yet on either side. Call X's `accept_friend_request` for You's owner; poll until the mutual `established_via: Introduction` link forms.

- [ ] **Step 4: Write the Closed-policy assertion.** Fresh nodes, X `peer_intro_policy = Closed`. After the request, wait a bounded settle window and assert NO link forms on either side and NO offer is staged on X.

- [ ] **Step 5: Run the e2e.** `cd src-tauri && cargo build --bin harmony-app` first if a spawned binary is involved (this test is in-process, so not needed), then `cargo nextest run --locked --features test-fixtures -E 'test(introduction_broker_roundtrip)' --test-threads 1` → all three cases PASS. Never weaken an assertion to mask a timing flake — bump the timeout.

- [ ] **Step 6: Gate + commit.** fmt + clippy `--all-targets`, then:
```bash
git commit -m "test(zeb-376): 3-node You-F-X introduction e2e (Open/AskMe/Closed policies)"
```

---

## Final pre-PR sweep (after all tasks)

- [ ] Full CI-parity gate (NOT `test-select`): from `src-tauri/`, `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures`; MSRV `cargo check --locked --all-targets --features test-fixtures`. From repo root: `npx tsc --noEmit`; `npx vitest run`.
- [ ] Confirm `zeb375_pex_fixtures` bytes are UNCHANGED in the diff (wire-compat proof).
- [ ] Whole-branch review, then bot converge, then PR.
