# ZEB-677 S2: Verifier Chokepoint + Seam Rollout — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Peers accept Quorum-issued enrollment/revocation certs everywhere Master-issued certs are accepted today, via depth-1 chain carriage (spec §2/§6), with all 8 verifier seams routed through ONE chokepoint module.

**Architecture:** Bump the `harmony-owner` git dep to the S1 merge (`1ecb416…`) which carries `verify_quorum_with_signers` on both cert types. Add `src-tauri/src/enrollment_verify.rs` — the single issuer-policy decision point — and reroute every seam through it. Add additive `#[serde(default)]` signer-bundle wire fields so a presenting peer can carry its signer certs. Flip the 3 pinned rejection tests into accept/reject matrices; close the retire-pair quorum test gap.

**Tech Stack:** Rust (src-tauri workspace), harmony-owner git dep, ciborium CBOR wire, cargo-nextest.

**Spec:** `docs/specs/2026-07-12-zeb-677-quorum-wiring-design.md` §2, §6, §9 (S2 row), §10.

## Global Constraints

- harmony-owner (and its 7 rev-siblings) bump to rev `1ecb4160ee62f19da23158e246e856d449159f93` — do NOT touch `harmony-pkarr` (separate rev `80f6d808…`).
- `OwnerState::add_revocation` is now `(cert, now: u64, active_window_secs: u64)` — Unix SECONDS, window = `harmony_owner::trust::DEFAULT_ACTIVE_WINDOW_SECS`.
- All new wire fields: additive, `#[serde(default, skip_serializing_if = …)]`, declared LAST in the struct, field-code length matching the struct's existing codes (1-char in friend/referral structs, 2-char elsewhere) — the canonical-CBOR same-length-key invariant (`owner_state_crypto.rs:650-669`).
- Empty bundle ⇒ Master-issued as today; old decoders ignore the new key; old encoders omit it (`default` fills empty).
- Depth-1: quorum signer certs must be Master-issued; enforced by the crate (`verify_quorum_with_signers`), never re-implemented client-side.
- Retire-pair keeps issued-at-time expiry semantics (`enrollment.verify(enrollment.issued_at)` idiom → chokepoint `now_secs = cert.issued_at`).
- Presenting-side bundle THREADING (DmOutbox etc.) is explicitly deferred to S4 — today no quorum cert can exist locally, every bundle would serialize empty. S2 ships the `own_cert_bundle` helper + wire capability only.
- Gates per repo CLAUDE.md: `cargo fmt --all -- --check`, `clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`, nextest `--locked --workspace --all-targets --features test-fixtures`. Iterative gates via `scripts/test-select --context task`; FINAL gate is the full sweep.
- Commit trailers: `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>` + `Claude-Session: https://claude.ai/code/session_01MsT6ZD7kqbpbKoeenyQPtc`.

---

### Task 1: Rev bump + `add_revocation` 3-arg migration

**Files:**
- Modify: `src-tauri/Cargo.toml:103-120` (8 rev strings)
- Modify: `src-tauri/src/owner_trust_sync.rs:91` (+ test sites 465, 657, 772)
- Modify: `src-tauri/src/owner_commands.rs:710` (+ test sites 1868, 1971, 1996, 2311, 2378, 2421, 2456)
- Modify: `src-tauri/src/community_device_intro_ingest.rs:755`

**Interfaces:**
- Produces: workspace compiles against harmony-owner @ `1ecb416` — `EnrollmentCert::{quorum_signing_payload_bytes, sign_quorum_part, assemble_quorum, verify_quorum_with_signers}` and the `RevocationCert` quartet become available to later tasks.

- [ ] **Step 1: Bump the 8 shared-rev git deps**

In `src-tauri/Cargo.toml`, replace all 8 occurrences of `8b870ae05449e710a54fd03421dadfc582d26c6a` with `1ecb4160ee62f19da23158e246e856d449159f93` (lines 103-109: harmony-runtime, harmony-identity, harmony-content, harmony-compute, harmony-telemetry, harmony-mailbox, harmony-owner; line 120: harmony-tunnel). Leave harmony-pkarr (lines 115, 223) untouched.

```bash
cd src-tauri && perl -pi -e 's/8b870ae05449e710a54fd03421dadfc582d26c6a/1ecb4160ee62f19da23158e246e856d449159f93/g' Cargo.toml
grep -c '1ecb4160' Cargo.toml   # expect 8
grep -n '80f6d808' Cargo.toml   # expect lines 115 + 223 intact
```

- [ ] **Step 2: Refresh the lockfile for the git group**

```bash
cd src-tauri && cargo update -p harmony-owner -p harmony-runtime -p harmony-identity -p harmony-content -p harmony-compute -p harmony-telemetry -p harmony-mailbox -p harmony-tunnel
cargo check --locked -p harmony-app 2>&1 | tail -20
```

Expected: compile errors ONLY at the 13 `add_revocation` call sites (E0061 wrong number of arguments).

- [ ] **Step 3: Migrate the 2 production call sites**

`owner_trust_sync.rs:91` (fn `merge_trust_remote_into_local`; `now` already computed at :66, `DEFAULT_ACTIVE_WINDOW_SECS` imported at :22):

```rust
if let Err(e) = local.add_revocation(cert.clone(), now, DEFAULT_ACTIVE_WINDOW_SECS) {
```

`owner_commands.rs:710` — the closure is `move`; bind time/window BEFORE it. Above the `mutate_trust_state(access, move |s| …)` call, add:

```rust
let now = now_unix();
```

then inside the closure:

```rust
s.add_revocation(cert, now, trust::DEFAULT_ACTIVE_WINDOW_SECS)
```

- [ ] **Step 4: Migrate the 11 test call sites**

Each already has a `now` local and the window constant in scope (survey verified). Pattern: `add_revocation(X)` → `add_revocation(X, now, DEFAULT_ACTIVE_WINDOW_SECS)` (or `trust::DEFAULT_ACTIVE_WINDOW_SECS` in owner_commands.rs). Sites: owner_trust_sync.rs 465/657/772; owner_commands.rs 1868/1971/1996/2311/2378/2421/2456; community_device_intro_ingest.rs 755. Where no `now` local exists in the immediate test, use the same timestamp the sibling `add_enrollment` call uses.

- [ ] **Step 5: Gate + commit**

```bash
cd src-tauri && cargo check --locked --all-targets --features test-fixtures
scripts/test-select --context task   # from repo root
git add -A && git commit -m "ZEB-677 S2: bump harmony crates to 1ecb416 (S1 quorum primitives) + add_revocation 3-arg migration"
```

---

### Task 2: `enrollment_verify.rs` chokepoint module (TDD)

**Files:**
- Create: `src-tauri/src/enrollment_verify.rs`
- Modify: `src-tauri/src/lib.rs` (add `pub mod enrollment_verify;` in the module list)

**Interfaces:**
- Produces (consumed by Tasks 3-5):

```rust
pub enum EnrollmentVerifyError { Expired, OwnerMismatch, Invalid(harmony_owner::OwnerError) }

pub struct VerifiedEnrollment {
    pub device_ed25519: [u8; 32],   // the enrolled device verify key
    pub master_ed25519: [u8; 32],   // the owner's master anchor (cert issuer for Master; signer certs' common master for Quorum)
}

pub fn verify_enrollment_any_issuer(
    cert: &EnrollmentCert,
    signer_certs: &[EnrollmentCert],
    expected_owner: Option<&[u8; 16]>,
    now_secs: u64,
) -> Result<VerifiedEnrollment, EnrollmentVerifyError>;

pub fn verify_revocation_any_issuer(
    cert: &RevocationCert,
    target_enrollment: &EnrollmentCert,
    signer_certs: &[EnrollmentCert],
    now_secs: u64,
) -> Result<(), EnrollmentVerifyError>;

pub fn own_cert_bundle(state: &harmony_owner::state::OwnerState, cert: &EnrollmentCert) -> Vec<EnrollmentCert>;

#[cfg(any(test, feature = "test-fixtures"))]
pub mod quorum_fixtures {
    pub struct QuorumWorld { /* master sk+bundle, owner_id, signer sks+certs (A,B), quorum cert for device C + its sk, bundle: Vec<EnrollmentCert> */ }
    pub fn mint_quorum_world(seed: u8) -> QuorumWorld;
    pub fn mint_quorum_revocation(world: &QuorumWorld, target: [u8; 16], issued_at: u64) -> harmony_owner::certs::RevocationCert;
}
```

**Semantics (spec §2 + crate API from survey):**

`verify_enrollment_any_issuer`:
1. `expected_owner` mismatch → `OwnerMismatch` (checked FIRST so callers get the same error class as today).
2. `Master` issuer: `cert.verify(now_secs)` (self-contained), mapping `OwnerError::EnrollmentCertExpired{..}` → `Expired`, others → `Invalid(e)`; master anchor = `master_pubkey.classical.ed25519_verify`.
3. `Quorum` issuer: `cert.verify_quorum_with_signers(signer_certs, now_secs)` (same error mapping — the crate checks structure, per-signer presence/owner/Master-issued/validity/backdating/signature). Master anchor = the signer certs' common `master_pubkey.classical.ed25519_verify`: read it from each cert named in `issuer.signers`, require all equal (defense-in-depth; `Invalid(InvalidSignature{cert_type:"Enrollment-Quorum-Master-Anchor"})`-style error via the closest OwnerError, see Step 3). Empty bundle → the crate returns `NotEnrolled` → `Invalid`.
4. Return both keys.

`verify_revocation_any_issuer` (three issuer arms, mirroring `verify_device_retire_certs`):
- `SelfDevice`: vk = `target_enrollment.device_pubkeys.classical.ed25519_verify` → `cert.verify(Some(&vk))`.
- `Master`: `cert.verify(None)`.
- `Quorum`: `cert.verify_quorum_with_signers(signer_certs, now_secs)`.
All errors → `Invalid(e)` (revocation has no expiry concept; no Expired mapping).

`own_cert_bundle`: `Master` → `vec![]`; `Quorum { signers, .. }` → `signers.iter().filter_map(|id| state.enrollments.get(id).cloned()).collect()`.

- [ ] **Step 1: Write failing unit tests**

In the new module's `#[cfg(test)] mod tests`, using `quorum_fixtures` (write fixtures first — they are compile prerequisites). Fixture recipe (mirrors crate S1 tests + client `mint_test_owner` seed idiom): master sk from `[seed; 32]`; devices A (`seed ^ 0x01`), B (`seed ^ 0x02`), C (`seed ^ 0x03`); A+B get `EnrollmentCert::sign_master(&master_sk, master_bundle.clone(), …, issued_at 1_700_000_000, None)`; C's quorum cert: `quorum_signing_payload_bytes(owner_id, c_id, &c_bundle, 1_700_000_100, None, &[a_id, b_id])` → `sign_quorum_part(&a_sk, …)` + `(&b_sk, …)` → `assemble_quorum(…, vec![(a_id, sig_a), (b_id, sig_b)])`. Bundle = `vec![a_cert, b_cert]`.

Test matrix (names as listed; all `now = 1_700_000_200`):
- `master_cert_verifies_without_bundle` — Master cert, empty bundle → Ok; `master_ed25519` == master bundle key.
- `quorum_cert_verifies_with_bundle` — C's cert + bundle → Ok; `device_ed25519` == C's key; `master_ed25519` == master key (recovered from signer certs).
- `quorum_cert_missing_bundle_rejected` — empty bundle → `Invalid(_)`.
- `quorum_cert_partial_bundle_rejected` — bundle = `[a_cert]` only → `Invalid(_)` (crate `NotEnrolled` for B).
- `quorum_cert_wrong_owner_signer_rejected` — bundle where `b_cert` is replaced by another owner's cert with B's device_id (mint second world, transplant) → `Invalid(_)`.
- `quorum_cert_bad_part_signature_rejected` — reassemble C's cert with `(b_id, sig_a)` (swapped sig) → `Invalid(_)`.
- `quorum_signer_cert_not_master_rejected` — bundle where a signer cert is itself Quorum-issued (hand-build: clone `a_cert`, set `issuer = Quorum{signers: vec![[9;16],[8;16]], signatures: vec![vec![0;64];2]}`, `signature = vec![]`) → `Invalid(_)` (depth-1).
- `expected_owner_mismatch_rejected` — valid quorum cert + bundle, `expected_owner = Some(&[0xEE; 16])` → `OwnerMismatch`.
- `expired_master_cert_maps_to_expired` — Master cert with `expires_at: Some(1_700_000_050)`, now later → `Expired`.
- `revocation_selfdevice_and_master_verify` — self-revoke by C (sign_self) and master revoke of C → Ok via `verify_revocation_any_issuer`.
- `revocation_quorum_verifies_with_bundle` — `mint_quorum_revocation` (payload/sign/assemble quartet, signers A+B) + bundle → Ok; empty bundle → `Invalid(_)`.
- `own_cert_bundle_collects_signers` — build an `OwnerState` via `mint_owner` + `add_enrollment` A and B (3-arg), then `own_cert_bundle(&state, &c_quorum_cert)` returns exactly A+B's certs; Master cert → empty vec.

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(enrollment_verify)'` — expect FAIL (module absent).

- [ ] **Step 2: Implement the module**

Full skeleton (error Display via `thiserror` is NOT the file idiom — hand-impl Display like `FriendHandshakeError`):

```rust
//! ZEB-677 §2: the ONE issuer-policy decision point for peer-presented
//! enrollment/revocation certs. Master → self-contained verify; Quorum →
//! depth-1 chain carriage against the presented signer-cert bundle.
//! Seams map `EnrollmentVerifyError` into their local error types.

use harmony_owner::certs::{EnrollmentCert, EnrollmentIssuer, RevocationCert, RevocationIssuer};
use harmony_owner::OwnerError;

#[derive(Debug)]
pub enum EnrollmentVerifyError {
    /// The cert (or a signer cert) is expired at `now_secs`.
    Expired,
    /// `cert.owner_id` does not match the caller's expected owner.
    OwnerMismatch,
    /// Any other verification failure (carries the crate error for tracing).
    Invalid(OwnerError),
}
```

`verify_enrollment_any_issuer` per the semantics block above. For the Quorum master-anchor recovery:

```rust
EnrollmentIssuer::Quorum { signers, .. } => {
    cert.verify_quorum_with_signers(signer_certs, now_secs).map_err(map_owner_err)?;
    let mut anchor: Option<[u8; 32]> = None;
    for id in signers {
        let sc = signer_certs.iter().find(|c| c.device_id == *id).ok_or(
            EnrollmentVerifyError::Invalid(OwnerError::NotEnrolled { owner: cert.owner_id, device: *id }),
        )?;
        let EnrollmentIssuer::Master { master_pubkey } = &sc.issuer else {
            return Err(EnrollmentVerifyError::Invalid(OwnerError::InvalidSignature {
                cert_type: "Enrollment-Quorum-Signer-Not-Master",
            }));
        };
        let m = master_pubkey.classical.ed25519_verify;
        if *anchor.get_or_insert(m) != m {
            return Err(EnrollmentVerifyError::Invalid(OwnerError::InvalidSignature {
                cert_type: "Enrollment-Quorum-Master-Anchor-Mismatch",
            }));
        }
    }
    anchor.ok_or(EnrollmentVerifyError::Invalid(OwnerError::InsufficientQuorum { min: 2, got: 0 }))?
}
```

(`map_owner_err`: `EnrollmentCertExpired{..}` → `Expired`, else `Invalid(e)`.)

- [ ] **Step 3: Run the tests to green**

```bash
cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(enrollment_verify)'
```

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "ZEB-677 S2: enrollment_verify chokepoint — any-issuer enrollment/revocation verification + quorum fixtures"
```

---

### Task 3: Friend / PEX / referral seams + wire fields

**Files:**
- Modify: `src-tauri/src/iroh_friend_acceptor.rs` (`verify_enrolled_device` :770, `master_ed25519_from_cert` :799, structs :268/:373, module docs :11-20, pinned test :2200)
- Modify: `src-tauri/src/referral_catalog.rs` (:257, :310, structs :52/:74)
- Modify: `src-tauri/src/friend_token.rs` (payload struct :53)
- Modify: `src-tauri/src/lib.rs` (call sites :50514, :50672, :50791, :53787-53792, :53910; acceptor reply is in iroh_friend_acceptor.rs:1186)

**Interfaces:**
- Consumes: Task 2's `verify_enrollment_any_issuer`, `VerifiedEnrollment`, `quorum_fixtures`.
- Produces: `verify_enrolled_device(cert, signer_certs: &[EnrollmentCert], claimed_owner, now_secs) -> Result<VerifiedEnrollment, FriendHandshakeError>` — NOTE return type upgrade; callers take `.device_ed25519` where they used the bare key.

- [ ] **Step 1: Wire fields (1-char codes, declared last)**

`FriendLinkRequest` and `FriendLinkAccepted` (iroh_friend_acceptor.rs) — append:

```rust
/// ZEB-677: Master-issued signer certs backing a Quorum-issued `enrollment`.
/// Empty for Master-issued certs (the wire omits the key entirely).
#[serde(rename = "b", default, skip_serializing_if = "Vec::is_empty")]
pub signer_certs: Vec<EnrollmentCert>,
```

`CatalogRequest` + `ReferralCatalog` (referral_catalog.rs) — same field, rename `"b"`.
`FriendTokenPayload` (friend_token.rs, 2-char codes) — same field, rename `"ib"`, name `inviter_signer_certs`.

Every construction site gains `signer_certs: Vec::new(),` (or `inviter_signer_certs`): lib.rs :50667-50680, :53787, iroh_friend_acceptor.rs :1182-1194, friend_token.rs :203, referral sign fns :223-237/:270-287, plus test constructors (compiler-guided). Presenting-side threading deferred to S4 (Global Constraints).

- [ ] **Step 2: Reroute `verify_enrolled_device` + retire `master_ed25519_from_cert`**

```rust
pub fn verify_enrolled_device(
    cert: &EnrollmentCert,
    signer_certs: &[EnrollmentCert],
    claimed_owner: OwnerAddr,
    now_secs: u64,
) -> Result<crate::enrollment_verify::VerifiedEnrollment, FriendHandshakeError> {
    crate::enrollment_verify::verify_enrollment_any_issuer(
        cert, signer_certs, Some(&claimed_owner.0), now_secs,
    )
    .map_err(|e| match e {
        crate::enrollment_verify::EnrollmentVerifyError::Expired => FriendHandshakeError::EnrollmentCertExpired,
        crate::enrollment_verify::EnrollmentVerifyError::OwnerMismatch => FriendHandshakeError::EnrollmentOwnerMismatch,
        crate::enrollment_verify::EnrollmentVerifyError::Invalid(_) => FriendHandshakeError::EnrollmentCertInvalid,
    })
}
```

Delete `master_ed25519_from_cert`; its consumer in `process_friend_request` (FriendEntry anchor) now uses the returned `VerifiedEnrollment.master_ed25519`. Update module-header docs (:11-20).

- [ ] **Step 3: Update the 7 call sites**

Pass the bundle from the struct in hand; take `.device_ed25519` from the result:
- iroh_friend_acceptor.rs:963 + :1021 → `&req.signer_certs`
- lib.rs:50514 → `&payload.inviter_signer_certs`
- lib.rs:50791 + :53910 → `&accepted.signer_certs`
- referral_catalog.rs:257 → `&req.signer_certs`; :310 → `&cat.signer_certs`

- [ ] **Step 4: Flip the pinned test + add matrix**

`verify_enrolled_device_rejects_non_master_issuer` (:2200) splits into:
- `verify_enrolled_device_rejects_quorum_without_bundle` — same construction, empty bundle, same assert (`EnrollmentCertInvalid`).
- `verify_enrolled_device_accepts_quorum_with_bundle` — `quorum_fixtures::mint_quorum_world`, verify C's cert + bundle against `OwnerAddr(world.owner_id)` → Ok; assert `master_ed25519` equals the world's master key.
- serde round-trip: `FriendLinkRequest` WITHOUT the field decodes (encode with empty → key omitted → decode → empty vec); WITH a 2-cert bundle round-trips equal.

- [ ] **Step 5: Gate + commit**

```bash
cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(iroh_friend_acceptor) or test(referral_catalog) or test(friend_token)'
scripts/test-select --context task
git add -A && git commit -m "ZEB-677 S2: friend/PEX/referral seams — quorum cert acceptance via chokepoint + signer-bundle wire fields"
```

---

### Task 4: Membership family — event bundle field, membership/invite/open-join/retire seams

**Files:**
- Modify: `src-tauri/src/community_membership.rs` (struct :469-514, `enrolled_key_from_cert` :1412, `verify_device_retire_certs` :1460-1499, caller :3407, pinned test :12282, retire tests ~:13036-13150)
- Modify: `src-tauri/src/community_invite.rs` (payload struct :107-186, `verify_inviter_enrollment` :1981-2011)
- (open_join_admit.rs + community_invite.rs:1530/:1845 ride the event field — signature untouched)

**Interfaces:**
- Consumes: Task 2's chokepoint + fixtures.
- Produces: `SignedMembershipEvent.signer_certs: Vec<EnrollmentCert>` (serde `"eb"`); `verify_device_retire_certs(actor, revocation, enrollment, signer_certs: &[EnrollmentCert])`.

- [ ] **Step 1: Event + payload wire fields (2-char codes, declared last)**

`SignedMembershipEvent` — append after `enrollment` (:512):

```rust
/// ZEB-677: Master-issued signer certs backing a Quorum-issued cert in
/// `enrollment` OR in a `DeviceRetire` payload (either position). Outside
/// the signed payload, like `enrollment`. Empty for Master-issued certs.
#[serde(rename = "eb", default, skip_serializing_if = "Vec::is_empty")]
pub signer_certs: Vec<EnrollmentCert>,
```

`CommunityInvitePayload` — append `inviter_signer_certs: Vec<EnrollmentCert>`, rename `"eb"`, same attrs. Construction sites gain empty vecs (compiler-guided; incl. `sign_event`-family helpers and lib.rs invite mint :27512-ish).

- [ ] **Step 2: Reroute `enrolled_key_from_cert`**

Replace the verify+Master-gate block (:1421-1430) with:

```rust
let verified = crate::enrollment_verify::verify_enrollment_any_issuer(
    cert, &event.signer_certs, Some(&event.actor.0), event.at.wall_ms / 1000,
)
.map_err(|e| match e {
    crate::enrollment_verify::EnrollmentVerifyError::OwnerMismatch => VerifyError::EnrollmentOwnerMismatch,
    _ => VerifyError::EnrollmentCertInvalid,
})?;
Ok(EnrolledDeviceKey { owner: event.actor, device_ed25519: verified.device_ed25519 })
```

(Keeps the ms→s conversion and the owner-mismatch error split exactly as today.)

- [ ] **Step 3: Reroute `verify_device_retire_certs` (both positions) + caller**

New param `signer_certs: &[EnrollmentCert]`; caller at :3407 passes `&event.signer_certs`. Enrollment position (replace :1472-1477):

```rust
if crate::enrollment_verify::verify_enrollment_any_issuer(
    enrollment, signer_certs, Some(&actor.0), enrollment.issued_at,
).is_err() {
    return Err(VerifyError::DeviceRetireCertInvalid);
}
```

(NOTE: `Some(&actor.0)` subsumes the existing `enrollment.owner_id != actor.0` check; `now = enrollment.issued_at` preserves issued-at expiry semantics.)

Revocation position (replace the `match &revocation.issuer` block :1483-1493):

```rust
let ok = crate::enrollment_verify::verify_revocation_any_issuer(
    revocation, enrollment, signer_certs, revocation.issued_at,
).is_ok();
```

Keep the owner/target bindings and reason-length cap untouched. Update `VerifyError` doc comments (:913-914, :955-962).

- [ ] **Step 4: Reroute `verify_inviter_enrollment`**

Replace :1999-2008 verify+gate with chokepoint call (`&payload.inviter_signer_certs`, `Some(&token.inviter.0)`, `now_secs`), mapping OwnerMismatch → `InviterEnrollmentOwnerMismatch`, else → `InviterEnrollmentCertInvalid`; keep the device-key extraction + token-sig verification.

- [ ] **Step 5: Flip pinned test + retire quorum coverage + round-trips**

- `enrollment_cert_quorum_issuer_rejected` (:12282) → split: `..._rejected_without_bundle` (unchanged assert, empty bundle) + `enrollment_cert_quorum_issuer_accepted_with_bundle` (fixture world; Join event with C's quorum cert, `signer_certs: world.bundle`, actor = owner → `enrolled_key_from_cert` Ok with C's key).
- NEW `verify_event_accepts_quorum_enrollment_device_retire` — retire pair where the RETIRED device's enrollment cert is Quorum-issued (world's C), revocation master-signed; event carries the bundle → verify_event Ok.
- NEW `verify_event_accepts_quorum_revocation_device_retire` — revocation minted via `mint_quorum_revocation` (signers A+B), enrollment Master-issued; bundle attached → Ok.
- NEW rejects: both retire positions with the bundle STRIPPED → `DeviceRetireCertInvalid`.
- serde round-trip: `SignedMembershipEvent` without `"eb"` key decodes to empty; with bundle round-trips.

- [ ] **Step 6: Gate + commit**

```bash
cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(community_membership) or test(community_invite) or test(open_join)'
scripts/test-select --context task
git add -A && git commit -m "ZEB-677 S2: membership/invite/open-join/retire seams — event signer-bundle field + chokepoint reroute + retire quorum coverage"
```

---

### Task 5: Butler / relay / profile-card seams

**Files:**
- Modify: `src-tauri/src/butler_deposit.rs` (`DepositFrame` :148, `build_deposit_frame` :433)
- Modify: `src-tauri/src/iroh_butler_acceptor.rs` (verifier block :537-581)
- Modify: `src-tauri/src/community_relay.rs` (structs :149/:192/:254, builders :447)
- Modify: `src-tauri/src/iroh_community_relay_acceptor.rs` (blocks :243-262, :430-452)
- Modify: `src-tauri/src/community_relay_pull_driver.rs` (builders :283/:301)
- Modify: `src-tauri/src/profile_card_broadcast.rs` (struct :30, `verify_card` :166-177, `sign_card` :91, pinned test :1137)

**Interfaces:**
- Consumes: Task 2's chokepoint + fixtures.

- [ ] **Step 1: Frame fields — cert-bytes structs get a bytes bundle (2-char codes, declared last)**

`DepositFrame`, `RelayDepositFrame`, `RelayPullQuery`, `RelayPullAckFrame` — append:

```rust
/// ZEB-677: canonical CBOR of `Vec<EnrollmentCert>` — Master-issued signer
/// certs backing a Quorum-issued sender/requester cert. Empty when the
/// cert is Master-issued.
#[serde(rename = "sc", default, skip_serializing_if = "Vec::is_empty", with = "serde_bytes")]
pub signer_certs_cbor: Vec<u8>,
```

Builders (`build_deposit_frame`, `build_relay_deposit_frame`, pull-driver `build_query`/`make_ack_builder`) gain a `signer_certs_cbor: Vec<u8>` parameter set verbatim; ALL current callers pass `Vec::new()` (threading deferred to S4).

Decoder helper in EACH acceptor next to its `decode_enrollment_cert_strict` (same strict trailing-byte idiom, same reject type):

```rust
fn decode_signer_certs_strict(bytes: &[u8]) -> Result<Vec<EnrollmentCert>, /* local reject */> {
    if bytes.is_empty() { return Ok(Vec::new()); }
    // ciborium::from_reader + cursor-position trailing check, mirroring decode_enrollment_cert_strict
}
```

- [ ] **Step 2: Reroute the 3 inline verifier blocks**

Each block (butler :537-581, relay deposit :243-262, relay pull :430-452) replaces `cert.verify` + `match issuer` + owner-binding with:

```rust
let signer_certs = decode_signer_certs_strict(&frame.signer_certs_cbor)?;
let verified = crate::enrollment_verify::verify_enrollment_any_issuer(
    &cert, &signer_certs, Some(&frame.sender_owner), ctx.now_secs(),
)
.map_err(|_| /* local BadCert */)?;
let cert_master = verified.master_ed25519;
let device_vk_bytes = verified.device_ed25519;
```

The D29.1 admission checks (`Admission::Friend` master equality / `CoMember` owner-id-derivation) stay verbatim — they now work for quorum certs too because `master_ed25519` is recovered from the verified bundle. Pull path: `expected_owner = Some(recipient_owner)`.

- [ ] **Step 3: Profile card**

`ProfileCardBroadcast` — append (NOTE: inside the whole-struct signature — sender must populate before signing; old cards with empty default still verify):

```rust
/// ZEB-677: Master-issued signer certs backing a Quorum-issued `enrollment`.
#[serde(rename = "eb", default, skip_serializing_if = "Vec::is_empty")]
pub signer_certs: Vec<EnrollmentCert>,
```

`sign_card` gains `signer_certs: Vec<EnrollmentCert>` param (callers pass `Vec::new()`); `verify_card` reroutes :166-177 through the chokepoint (`&card.signer_certs`, `Some(&card.owner_id)`, `now_secs`; OwnerMismatch → `EnrollmentOwnerMismatch`, else → `EnrollmentCertInvalid`).

- [ ] **Step 4: Flip pinned test + matrices + round-trips**

- `verify_card_rejects_non_master_issuer` (:1137) → `..._rejects_quorum_without_bundle` (unchanged assert) + `verify_card_accepts_quorum_with_bundle` (fixture world; card enrollment = C's quorum cert signed by C's device key, `signer_certs = world.bundle` populated pre-sign → Ok).
- Butler + relay: accept tests with a quorum sender cert + populated `signer_certs_cbor` (canonical CBOR of the bundle); reject tests with bundle stripped (existing BadCert asserts stay for that case). Use each file's existing test harness/ctx mocks.
- serde round-trips for all four frames (absent key → empty; populated round-trips).

- [ ] **Step 5: Gate + commit**

```bash
cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(butler) or test(relay) or test(profile_card)'
scripts/test-select --context task
git add -A && git commit -m "ZEB-677 S2: butler/relay/profile-card seams — signer-bundle frames + chokepoint reroute"
```

---

### Task 6: Full gates + PR

- [ ] **Step 1: Full sweep (final gate — no test-select)**

```bash
cd src-tauri && cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
npx tsc --noEmit && npx vitest run   # from repo root — frontend untouched but cheap
```

- [ ] **Step 2: Self-review the branch diff** (second-order check: does any seam now SKIP a check it used to make? owner-binding, expiry semantics, D29.1 anchors, reason-length cap)

- [ ] **Step 3: Open PR** to zeblithic/harmony-client, title `ZEB-677 S2: quorum cert acceptance — enrollment_verify chokepoint, 8 seams, signer-bundle wire fields`; fire `@coderabbitai review` ONCE; converge per standing loop rules.
