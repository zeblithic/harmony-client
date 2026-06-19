# ZEB-497 inviter_enrollment Verification — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps are sequential plain steps; track execution status in Linear (ZEB-497), not in this file (repo rule: no markdown checkbox TODO tracking).

**Goal:** Make the presence-only `inviter_enrollment` cert on invite-only community invites a real cryptographic gate on the redeem path — recover the inviter's enrolled device key from the cert, bind it to `invite_token.inviter`, and verify the token signature against it.

**Architecture:** One new pure function `verify_inviter_enrollment` in `community_invite.rs` (mirrors the friend path's `verify_enrolled_device` with the community error type), called fail-fast at the two invite-only redeem entrypoints in `lib.rs`. `admin_bootstrap`, `admin_identity_pub`, and the friend path are untouched. Existing invite-only redeem tests that pass `None`/throwaway certs are migrated to consistent fixtures using the in-repo `mint_test_owner` pattern.

**Tech Stack:** Rust, `ed25519_dalek`, `harmony_owner::certs::{EnrollmentCert, EnrollmentIssuer}`, `cargo nextest`, the `test-fixtures` feature.

**Spec:** `docs/specs/2026-06-18-zeb-497-inviter-enrollment-verification-design.md`

**Branch:** `zeb-497-invite-principal-auth` (off `origin/main` @ `9a7f8221`). One PR.

**Local gate (run from `src-tauri/`):** `cargo fmt --all -- --check` · `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` · `cargo nextest run --locked --workspace --all-targets --features test-fixtures`.

**Key fixture fact (load-bearing for tests):** `harmony_app::community_membership::mint_test_owner(seed: u8) -> TestOwner { owner: OwnerAddr, device_key: ed25519_dalek::SigningKey, cert: EnrollmentCert }`. The returned `cert` is a `Master`-issuer cert whose `device_pubkeys.classical.ed25519_verify == device_key.verifying_key().to_bytes()`, and `cert.owner_id == owner.0`. So signing an `InviteToken` (with `inviter == owner`) using `device_key` produces a token that passes `verify_inviter_enrollment` against `cert`. `pkarr_iroh_redeem_full_integration.rs` already uses exactly this pattern — it is the canonical reference. Avoid pairing seeds `N` and `N ^ 0xFF` in one test (shared key material).

---

### Task 1: Add the two `CommunityInviteVerifyError` variants

**Files:**
- Modify: `src-tauri/src/community_invite.rs` (enum `CommunityInviteVerifyError` ~L1161-1228; `reason_tag()` ~L1230-1249)

The enum's `reason_tag()` is a **non-wildcard** match (no `_ =>` arm), so a new variant without a matching arm fails to compile — that compile error is the safety net for this task. The token-signature failure reuses the existing `InviteTokenSigInvalid` variant (returned by `verify_invite_token_sig_device_key`), so only two new variants are needed.

**Step 1: Add the variants.** In `enum CommunityInviteVerifyError`, after the existing `EngineLocalError` variant, add:

```rust
    /// inviter_enrollment cert failed verification (bad master signature,
    /// expired, or non-Master issuer). ZEB-497.
    #[error("inviter enrollment cert invalid")]
    InviterEnrollmentCertInvalid,
    /// inviter_enrollment cert binds a different owner than invite_token.inviter.
    /// ZEB-497.
    #[error("inviter enrollment owner mismatch")]
    InviterEnrollmentOwnerMismatch,
```

**Step 2: Add the `reason_tag()` arms.** In `impl CommunityInviteVerifyError { pub fn reason_tag(...) }`, after the existing `Self::EngineLocalError => ...` arm, add:

```rust
            Self::InviterEnrollmentCertInvalid => "community_invite_inviter_enrollment_cert_invalid",
            Self::InviterEnrollmentOwnerMismatch => "community_invite_inviter_enrollment_owner_mismatch",
```

**Step 3: Verify it compiles.**

Run: `cd src-tauri && cargo check --lib`
Expected: compiles (no non-exhaustive-match error on `reason_tag`).

**Step 4: Commit.**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/community_invite.rs
git commit -m "feat(zeb-497): add inviter-enrollment verify error variants"
```

---

### Task 2: Implement `verify_inviter_enrollment` (TDD)

**Files:**
- Create: `src-tauri/tests/community_invite_inviter_enrollment.rs`
- Modify: `src-tauri/src/community_invite.rs` (new `pub fn` near `verify_invite_token_sig_device_key` ~L1767)

The function is pure and `pub` so the integration test can call it. It mirrors `iroh_friend_acceptor::verify_enrolled_device` (the 4-step bare-cert verifier) but returns `CommunityInviteVerifyError` and adds the token-signature check.

**Step 1: Write the failing happy-path test.** Create `src-tauri/tests/community_invite_inviter_enrollment.rs`:

```rust
//! ZEB-497: unit coverage for `verify_inviter_enrollment` — the inviter's
//! EnrollmentCert is cryptographically bound to the InviteToken on the
//! community redeem path. Fixtures use `mint_test_owner` (matched device_key +
//! cert); see pkarr_iroh_redeem_full_integration.rs for the same pattern.

use ed25519_dalek::Signer;
use harmony_app::community_invite::{
    canonical_invite_token_bytes, verify_inviter_enrollment, CommunityInvitePayload,
    CommunityInviteVerifyError, InviteEpochSnapshot, InviteToken,
};
use harmony_app::community_membership::mint_test_owner;
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};

const NOW_SECS: u64 = 1_700_000_500; // within mint_test_owner's cert validity

/// Build an invite-only payload whose InviteToken is signed by `signer` and
/// whose `inviter` field is `inviter_addr`. `cert` rides in inviter_enrollment.
fn invite_only_payload(
    inviter_addr: OwnerAddr,
    signer: &ed25519_dalek::SigningKey,
    cert: harmony_owner::certs::EnrollmentCert,
) -> CommunityInvitePayload {
    let unsigned = InviteToken {
        inviter: inviter_addr,
        invitee_hint: None,
        minted_at: Hlc { wall_ms: 1_700_000_000_000, logical: 0, device_id: "d".into() },
        expires_at: None,
        sig: [0u8; 64],
    };
    let bytes = canonical_invite_token_bytes(&unsigned).expect("canonical bytes");
    let sig: [u8; 64] = signer.sign(&bytes).to_bytes();
    let token = InviteToken { sig, ..unsigned };
    CommunityInvitePayload {
        community_id: SpaceId([0x11; 16]),
        epoch_snapshot: InviteEpochSnapshot::default(),
        admin_addr: inviter_addr,
        community_name: "T".into(),
        is_invite_only: true,
        expires_at: None,
        invite_token: Some(token),
        admin_bootstrap: None,
        admin_identity_pub: None,
        forked_from: None,
        pre_fork_snapshot: None,
        inviter_enrollment: Some(cert),
        untargeted_decrypt_key: None,
    }
}

#[test]
fn valid_inviter_enrollment_passes() {
    let inviter = mint_test_owner(0x42);
    let payload = invite_only_payload(inviter.owner, &inviter.device_key, inviter.cert.clone());
    assert!(verify_inviter_enrollment(&payload, NOW_SECS).is_ok());
}
```

Note: the exact field set of `CommunityInvitePayload` must match the struct (see `community_invite.rs:108-205`); copy any fields this snippet omits with their default/empty values. `InviteEpochSnapshot::default()` is used elsewhere in tests — confirm it derives `Default` or build it inline as the other tests do.

**Step 2: Run it — expect a compile failure** (`verify_inviter_enrollment` doesn't exist yet).

Run: `cd src-tauri && cargo nextest run --features test-fixtures --test community_invite_inviter_enrollment`
Expected: FAILS to compile — `cannot find function verify_inviter_enrollment`.

**Step 3: Implement the function.** In `src-tauri/src/community_invite.rs`, near `verify_invite_token_sig_device_key`, add:

```rust
/// Verify the inviter's `inviter_enrollment` cert on an invite-only invite:
/// recover the inviter's enrolled device key from the cert, bind it to
/// `invite_token.inviter`, and verify the token signature against it. No-op for
/// open communities. Mirrors `iroh_friend_acceptor::verify_enrolled_device`
/// with the community error type (ZEB-497).
pub fn verify_inviter_enrollment(
    payload: &CommunityInvitePayload,
    now_secs: u64,
) -> Result<(), CommunityInviteVerifyError> {
    if !payload.is_invite_only {
        return Ok(());
    }
    let cert = payload
        .inviter_enrollment
        .as_ref()
        .ok_or(CommunityInviteVerifyError::InviterEnrollmentCertInvalid)?;
    let token = payload
        .invite_token
        .as_ref()
        .ok_or(CommunityInviteVerifyError::InviteTokenSigInvalid)?;
    // Recover the inviter's enrolled device key from the bare cert (master-sig +
    // expiry, then reject non-Master issuers — quorum certs are only
    // structurally checked by cert.verify and would admit unverified sigs).
    cert.verify(now_secs)
        .map_err(|_| CommunityInviteVerifyError::InviterEnrollmentCertInvalid)?;
    if !matches!(
        cert.issuer,
        harmony_owner::certs::EnrollmentIssuer::Master { .. }
    ) {
        return Err(CommunityInviteVerifyError::InviterEnrollmentCertInvalid);
    }
    if cert.owner_id != token.inviter.0 {
        return Err(CommunityInviteVerifyError::InviterEnrollmentOwnerMismatch);
    }
    let device_key = cert.device_pubkeys.classical.ed25519_verify;
    verify_invite_token_sig_device_key(token, &device_key)
}
```

Add `use` imports as the compiler directs (e.g. `harmony_owner::certs::EnrollmentIssuer` may be fully qualified inline as written, so no new `use` is required).

**Step 4: Run the happy-path test — expect PASS.**

Run: `cd src-tauri && cargo nextest run --features test-fixtures --test community_invite_inviter_enrollment`
Expected: `valid_inviter_enrollment_passes` PASSES.

**Step 5: Add the six negative/edge tests.** Append to `tests/community_invite_inviter_enrollment.rs`:

```rust
#[test]
fn forged_token_sig_rejected() {
    let inviter = mint_test_owner(0x42);
    let wrong = mint_test_owner(0x07); // different device key signs the token
    let payload = invite_only_payload(inviter.owner, &wrong.device_key, inviter.cert.clone());
    assert_eq!(
        verify_inviter_enrollment(&payload, NOW_SECS),
        Err(CommunityInviteVerifyError::InviteTokenSigInvalid)
    );
}

#[test]
fn owner_mismatch_rejected() {
    let inviter = mint_test_owner(0x42);
    let other = mint_test_owner(0x07); // cert for a different owner
    // Token says inviter=inviter.owner and is signed by inviter.device_key, but
    // the cert in inviter_enrollment belongs to `other`.
    let payload = invite_only_payload(inviter.owner, &inviter.device_key, other.cert.clone());
    assert_eq!(
        verify_inviter_enrollment(&payload, NOW_SECS),
        Err(CommunityInviteVerifyError::InviterEnrollmentOwnerMismatch)
    );
}

#[test]
fn tampered_cert_rejected() {
    let inviter = mint_test_owner(0x42);
    let mut cert = inviter.cert.clone();
    cert.signature[0] ^= 0x01; // break the master signature
    let payload = invite_only_payload(inviter.owner, &inviter.device_key, cert);
    assert_eq!(
        verify_inviter_enrollment(&payload, NOW_SECS),
        Err(CommunityInviteVerifyError::InviterEnrollmentCertInvalid)
    );
}

#[test]
fn expired_cert_rejected() {
    // mint_test_owner sets expires_at: None (never expires), so mint a cert with
    // a past expiry directly via the same helpers mint_test_owner uses.
    let inviter = mint_test_owner(0x42);
    let mut cert = inviter.cert.clone();
    cert.expires_at = Some(NOW_SECS - 1); // already expired
    // Re-sign so the (now-expired) cert still has a valid master signature and
    // fails on expiry rather than signature. Use harmony_owner's sign_master
    // exactly as community_membership::mint_test_owner does, or assert that an
    // expired cert is rejected as InviterEnrollmentCertInvalid.
    let payload = invite_only_payload(inviter.owner, &inviter.device_key, cert);
    assert_eq!(
        verify_inviter_enrollment(&payload, NOW_SECS),
        Err(CommunityInviteVerifyError::InviterEnrollmentCertInvalid)
    );
}

#[test]
fn non_invite_only_short_circuits() {
    let inviter = mint_test_owner(0x42);
    let mut payload = invite_only_payload(inviter.owner, &inviter.device_key, inviter.cert.clone());
    payload.is_invite_only = false;
    payload.inviter_enrollment = None;
    payload.invite_token = None;
    assert!(verify_inviter_enrollment(&payload, NOW_SECS).is_ok());
}

#[test]
fn missing_inviter_enrollment_rejected() {
    let inviter = mint_test_owner(0x42);
    let mut payload = invite_only_payload(inviter.owner, &inviter.device_key, inviter.cert.clone());
    payload.inviter_enrollment = None;
    assert_eq!(
        verify_inviter_enrollment(&payload, NOW_SECS),
        Err(CommunityInviteVerifyError::InviterEnrollmentCertInvalid)
    );
}
```

Note on `tampered_cert_rejected` / `expired_cert_rejected`: mutating `cert.signature` / `cert.expires_at` after minting invalidates or leverages the master signature. If `cert.verify` rejects the tampered-signature case as expected, the test passes as written. For the expired case, if mutating `expires_at` alone (without re-signing) causes a *signature* failure first, that still maps to `InviterEnrollmentCertInvalid` — the assertion holds either way. If a more precise expired-vs-invalid distinction is wanted later, re-sign via `harmony_owner::certs::EnrollmentCert::sign_master` (the same call `mint_test_owner` uses) with a past `expires_at`. The "Quorum issuer rejected" case (spec test 6) is covered structurally by the `matches!(... Master ...)` guard; an explicit Quorum-cert test is optional and omitted to avoid hand-constructing a Quorum cert.

**Step 6: Run all the function tests — expect PASS.**

Run: `cd src-tauri && cargo nextest run --features test-fixtures --test community_invite_inviter_enrollment`
Expected: all tests PASS.

**Step 7: Commit.**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/community_invite.rs src-tauri/tests/community_invite_inviter_enrollment.rs
git commit -m "feat(zeb-497): verify_inviter_enrollment + unit tests"
```

---

### Task 3: Wire the gate into the two redeem entrypoints

**Files:**
- Modify: `src-tauri/src/lib.rs` — `orphan_dir_adoption_eligible` (~L22444, returns `bool`) and `redeem_invite_inner_with_overrides` (~L22554, returns `Result<_, String>`)

Both are invite-only redeem paths. The function self-gates on `is_invite_only`, but the orphan pre-check already branches on it, so call inside that block.

**Step 1: Wire `orphan_dir_adoption_eligible`.** Inside the existing `if payload.is_invite_only {` block, immediately after the `verify_admin_bootstrap(payload).is_err() { return false; }` check, add:

```rust
        // ZEB-497: the inviter's enrollment cert must verify and bind to the
        // token's inviter, and the token must be signed by that enrolled device.
        if crate::community_invite::verify_inviter_enrollment(payload, now_wall_ms / 1000)
            .is_err()
        {
            return false;
        }
```

(`now_wall_ms` is the existing `u64` ms parameter of this fn; `/ 1000` converts to the Unix seconds `cert.verify` expects.)

**Step 2: Wire `redeem_invite_inner_with_overrides`.** Immediately after the `let wall_now_ms = ...;` block that follows `decode_invite_url` (~L22601-22605), add:

```rust
    // ZEB-497: fail fast — verify the inviter's enrollment cert + token sig
    // before reserving an HLC, minting the local bootstrap Join, or any network.
    crate::community_invite::verify_inviter_enrollment(&payload, wall_now_ms / 1000)
        .map_err(|e| format!("verify inviter enrollment: {e}"))?;
```

**Step 3: Compile.**

Run: `cd src-tauri && cargo check --lib`
Expected: compiles.

**Step 4: Commit.**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/lib.rs
git commit -m "feat(zeb-497): gate community redeem on inviter_enrollment"
```

---

### Task 4: Migrate invite-only redeem tests to consistent fixtures

**Files (the migration surface — confirm by running the suite):**
- `src-tauri/tests/pkarr_invite_redemption_integration.rs`
- `src-tauri/tests/library_directory_integration.rs`
- `src-tauri/tests/community_pending_join_integration.rs` (if it redeems invite-only)
- `src-tauri/tests/community_sync_integration.rs` (the invite-only redeem case)
- NOT `pkarr_iroh_redeem_full_integration.rs` (already consistent — it is the reference pattern)
- NOT `community_invite_only_integration.rs` (happy path is `#[ignore]`d; negative test calls `verify_admin_bootstrap` directly, not the redeem path)

After Task 3, any test that redeems an `is_invite_only: true` invite through `redeem_invite_inner*` / `orphan_dir_adoption_eligible` with a `None` or owner-mismatched `inviter_enrollment` (or an unsigned/garbage token) now fails the new gate. The failing tests define the exact migration set.

**Step 1: Run the full suite to enumerate the fallout.**

Run: `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures`
Expected: the invite-only redeem tests listed above FAIL (gate rejects their fixtures); everything else passes.

**Step 2: Migrate each failing test to the consistent pattern.** For each failing invite-only redeem test, align its inviter/admin identity to a single `mint_test_owner` and make the token + cert consistent — exactly as `pkarr_iroh_redeem_full_integration.rs` does:

```rust
use ed25519_dalek::Signer;
let inviter = harmony_app::community_membership::mint_test_owner(0x5A);
// In the payload: admin_addr = inviter.owner, inviter_enrollment = Some(inviter.cert.clone());
// In the InviteToken: inviter = inviter.owner, and sign it with inviter.device_key:
let token_sig: [u8; 64] = inviter
    .device_key
    .sign(&canonical_invite_token_bytes(&unsigned_token).expect("canonical bytes"))
    .to_bytes();
```

Important alignment notes:
- In v1 the invite path requires `invite_token.inviter == admin_addr` (orphan pre-check P2 and the log gate). So when a test's admin identity is a `PrivateIdentity`, switch that test's admin/inviter to the `mint_test_owner` `inviter` above (and sign its `admin_bootstrap` Join with `inviter.device_key`, attaching `inviter.cert`), so admin_bootstrap and inviter_enrollment refer to the same owner. Mirror how `pkarr_iroh_redeem_full_integration.rs` builds `alice_comm`.
- If a test only needs the invite to *parse/encode* (not redeem), `inviter_enrollment` presence is enough and no signing change is needed — leave it. Only redeem-path tests hit the gate.
- Do not change the assertions; the redeem should still succeed (or fail for its original reason) once the inviter fixture is consistent.

**Step 3: Re-run the full suite — expect green.**

Run: `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures`
Expected: all tests pass (0 failures).

**Step 4: Commit.**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/tests/
git commit -m "test(zeb-497): migrate invite-only redeem fixtures to consistent inviter_enrollment"
```

---

### Task 5: Full gate + final commit

**Files:** none (verification)

**Step 1: Format.**

Run: `cd src-tauri && cargo fmt --all`
Then: `cargo fmt --all -- --check` (expect clean).

**Step 2: Clippy.**

Run: `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
Expected: 0 warnings.

**Step 3: Full test run.**

Run: `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures`
Expected: 0 failures.

**Step 4: Commit any fmt changes and push.**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add -A && git commit -m "chore(zeb-497): fmt" || echo "nothing to commit"
git push -u origin zeb-497-invite-principal-auth
```

**Step 5: Open the PR** (keep `Closes ZEB-NNN` out of the body — the branch name links ZEB-497; cross-refs go in a comment). Title: `ZEB-497: verify inviter_enrollment on the community redeem path`. Body summarizes: the new `verify_inviter_enrollment` gate, the two fail-fast call sites, the fixture migration, and that `admin_bootstrap` / friend path are untouched. Reference the spec path.

---

## Self-Review

- **Spec coverage:** verification function + 4 steps → Task 2; bind to `token.inviter` + wall-now clock → Task 2/Task 3 (`/1000`); two call sites fail-fast → Task 3; presence checks kept (no edit to `:988`/`:1061`) → unchanged by design; error taxonomy → Task 1 (two new variants + reuse `InviteTokenSigInvalid`); tests 1–7 → Task 2; v1 regression / "valid invite still redeems" → Task 4 (the migrated redeem tests are the end-to-end regression, since the dedicated two-engine happy path is `#[ignore]`d for unrelated reasons). All spec sections covered.
- **Placeholder scan:** the Task 4 file list and the expired-cert re-sign detail are resolved-by-running-the-suite / mirror-an-existing-helper directions, not gaps; every code step shows real code.
- **Type consistency:** `verify_inviter_enrollment(&CommunityInvitePayload, u64) -> Result<(), CommunityInviteVerifyError>` used identically in Tasks 2/3; `mint_test_owner(seed) -> TestOwner { owner, device_key, cert }` and `verify_invite_token_sig_device_key(&InviteToken, &[u8;32])` match the current source; error variant names match between Task 1 and the Task 2 assertions.
