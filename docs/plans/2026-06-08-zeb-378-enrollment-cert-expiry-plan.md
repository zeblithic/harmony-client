# ZEB-378 Enrollment-Cert Expiry Enforcement — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `EnrollmentCert::verify()` clock-aware so a past `expires_at` is rejected at the source, closing the expiry-bypass on the friend-handshake, referral-catalog, and community-membership auth paths.

**Architecture:** Two coordinated PRs. **PR A** (harmony repo, `harmony-owner` crate) changes `verify(&self) → verify(&self, now_ms: u64)`, adds an expiry check + `OwnerError::EnrollmentCertExpired`, and updates its one production caller + in-crate tests. **PR B** (harmony-client) bumps the `harmony-owner` git rev to PR A's merged commit and threads a clock through every `verify()`/`verify_enrolled_device` caller. Merge order is **A → rev-bump → B** (PR B can't compile against the new signature until A merges).

**Tech Stack:** Rust, `cargo` (fmt/clippy/nextest), `thiserror`, `ed25519-dalek`, Tauri (harmony-client). Spec: `docs/specs/2026-06-08-zeb-378-enrollment-cert-expiry-design.md`.

**The `now_ms = 0` rule (used throughout):** `now_ms > expires_at` with `now_ms = 0` is `false` for every non-negative `expires_at`, so passing `0` is a **provable no-op for the expiry gate** — `verify(0)` behaves exactly like the pre-change `verify()`. Therefore: existing call sites/tests that must preserve behavior pass `0`; only *new expiry tests* and the *live production paths* pass a real clock. Production live paths: `add_enrollment → now`, `verify_enrolled_device` transport callers `→ wall_now_ms()`, `enrolled_key_from_cert → event.at.wall_ms`. The `DmOutbox::new` wiring assert passes `0` by design (spec Component 3).

**Repo paths:**
- harmony (PR A): `/Users/zeblith/work/zeblithic/harmony`, crate `crates/harmony-owner/`
- harmony-client (PR B): `/Users/zeblith/work/zeblithic/harmony-client`, cargo root `src-tauri/`

---

## GROUP A — harmony repo PR (`harmony-owner`)

### Task A0: Branch setup (controller-run, not a subagent task)

**Files:** none (git only). The local harmony checkout is on stale branch `zeb-380-relay-pool-hotswap` with a dirty `Cargo.lock` and an untracked research doc.

- [ ] **Step 1: Stash the dirty lockfile (non-destructive) and branch off fresh main**

```bash
cd /Users/zeblith/work/zeblithic/harmony
git stash push -- Cargo.lock          # preserves the zeb-380 lockfile drift on the stash stack
git fetch origin --prune
git checkout -b zeb-378-enrollment-cert-expiry origin/main
git log --oneline -1                   # confirm we're on latest origin/main
```

Expected: clean checkout on a new branch based at `origin/main`. The untracked `docs/research/*.md` is unaffected (untracked files survive checkout).

### Task A1: Clock-aware `verify()` + `OwnerError::EnrollmentCertExpired`

**Files:**
- Modify: `crates/harmony-owner/src/error.rs` (add variant)
- Modify: `crates/harmony-owner/src/certs/enrollment.rs:91` (`verify` signature + expiry check)
- Test: `crates/harmony-owner/src/certs/enrollment.rs` (test mod at bottom)

- [ ] **Step 1: Write the failing tests** (append to the `#[cfg(test)] mod tests` in `enrollment.rs`)

```rust
#[test]
fn verify_rejects_past_expiry() {
    let (master_sk, master_bundle) = fresh_pubkey_bundle(1, 2);
    let (_d_sk, device_bundle) = fresh_pubkey_bundle(3, 4);
    let device_id = device_bundle.identity_hash();
    // issued at 1_000, expires at 2_000.
    let cert = EnrollmentCert::sign_master(
        &master_sk, master_bundle, device_id, device_bundle, 1_000, Some(2_000),
    )
    .unwrap();
    // now = 2_001 > expires_at 2_000 → expired.
    assert!(matches!(
        cert.verify(2_001),
        Err(OwnerError::EnrollmentCertExpired { expires_at: 2_000, now_ms: 2_001 })
    ));
    // exactly at expiry is still valid (not strictly greater).
    assert!(cert.verify(2_000).is_ok());
    // before expiry valid.
    assert!(cert.verify(1_500).is_ok());
}

#[test]
fn verify_accepts_none_expiry_at_any_clock() {
    let (master_sk, master_bundle) = fresh_pubkey_bundle(5, 6);
    let (_d_sk, device_bundle) = fresh_pubkey_bundle(7, 8);
    let device_id = device_bundle.identity_hash();
    let cert = EnrollmentCert::sign_master(
        &master_sk, master_bundle, device_id, device_bundle, 1_000, None,
    )
    .unwrap();
    assert!(cert.verify(0).is_ok());
    assert!(cert.verify(u64::MAX).is_ok());
}
```

- [ ] **Step 2: Run tests to verify they fail to compile** (signature still 0-arg)

Run: `cd /Users/zeblith/work/zeblithic/harmony && cargo test -p harmony-owner verify_rejects_past_expiry 2>&1 | tail -20`
Expected: compile error — `verify` takes 0 args / `EnrollmentCertExpired` not found.

- [ ] **Step 3: Add the `OwnerError` variant** (`crates/harmony-owner/src/error.rs`, after the `Revoked` variant)

```rust
    #[error("enrollment cert expired at {expires_at} (now {now_ms})")]
    EnrollmentCertExpired { expires_at: u64, now_ms: u64 },
```

- [ ] **Step 4: Make `verify` clock-aware** (`crates/harmony-owner/src/certs/enrollment.rs`)

Change the signature and insert the expiry check after the version check, before the `match &self.issuer`:

```rust
    pub fn verify(&self, now_ms: u64) -> Result<(), OwnerError> {
        if self.version != ENROLLMENT_VERSION {
            return Err(OwnerError::UnknownVersion(self.version));
        }
        if let Some(exp) = self.expires_at {
            if now_ms > exp {
                return Err(OwnerError::EnrollmentCertExpired {
                    expires_at: exp,
                    now_ms,
                });
            }
        }
        match &self.issuer {
            // ... existing Master / Quorum arms unchanged ...
        }
    }
```

- [ ] **Step 5: Update the existing in-crate `verify()` callers to `verify(0)`** (behavior-preserving)

These are existing tests asserting structural/sig behavior; pass `0` (provable no-op for expiry):
- `crates/harmony-owner/src/certs/enrollment.rs:226` `cert.verify().unwrap();` → `cert.verify(0).unwrap();`
- `crates/harmony-owner/src/certs/enrollment.rs:247` `let result = cert.verify();` → `let result = cert.verify(0);`
- `crates/harmony-owner/src/certs/enrollment.rs:270` `decoded.verify().unwrap();` → `decoded.verify(0).unwrap();`
- `crates/harmony-owner/src/certs/reclamation.rs:148` `cert.verify().unwrap();` → `cert.verify(0).unwrap();`
- `crates/harmony-owner/src/certs/reclamation.rs:191` `let result = cert.verify();` → `let result = cert.verify(0);`
- `crates/harmony-owner/src/certs/reclamation.rs:230` `let result = cert.verify();` → `let result = cert.verify(0);`

- [ ] **Step 6: Run the new + existing tests**

Run: `cd /Users/zeblith/work/zeblithic/harmony && cargo test -p harmony-owner 2>&1 | tail -25`
Expected: all pass, including `verify_rejects_past_expiry` + `verify_accepts_none_expiry_at_any_clock`.

- [ ] **Step 7: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony
git add crates/harmony-owner/src/error.rs crates/harmony-owner/src/certs/enrollment.rs crates/harmony-owner/src/certs/reclamation.rs
git commit -m "feat(harmony-owner): enforce EnrollmentCert expires_at in verify(now_ms) (ZEB-378)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

### Task A2: Thread the clock through the one production caller (`add_enrollment`)

**Files:**
- Modify: `crates/harmony-owner/src/state.rs:208`

- [ ] **Step 1: Pass the existing `now` into `verify`**

`OwnerState::add_enrollment(&mut self, cert, now, active_window_secs)` already has `now: u64` in scope. Change `state.rs:208`:

```rust
        cert.verify(now)?;
```

- [ ] **Step 2: Run the harmony-owner state tests**

Run: `cd /Users/zeblith/work/zeblithic/harmony && cargo test -p harmony-owner 2>&1 | tail -25`
Expected: all pass. If any `add_enrollment` test mints a cert with `expires_at: Some(past)` relative to its `now`, that test now correctly rejects — read it; if it was asserting acceptance of an expired cert it encoded the bug and its expectation must flip (note in commit). If it used `None`, no change.

- [ ] **Step 3: Full harmony-owner gate**

```bash
cd /Users/zeblith/work/zeblithic/harmony
cargo fmt --all -- --check
cargo clippy -p harmony-owner --all-targets -- -D warnings
cargo test -p harmony-owner
```
Expected: fmt clean, clippy 0 warnings, all tests pass.

- [ ] **Step 4: Commit + push + open PR A**

```bash
cd /Users/zeblith/work/zeblithic/harmony
git add crates/harmony-owner/src/state.rs
git commit -m "refactor(harmony-owner): pass apply-time clock into add_enrollment verify (ZEB-378)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
git push -u origin zeb-378-enrollment-cert-expiry
git rev-parse HEAD   # record this SHA — it is the rev PR B develops against
gh pr create --repo zeblithic/harmony --title "ZEB-378: enforce EnrollmentCert expires_at in verify()" --body "<see PR-A body template in plan>"
```

---

## INTER-PR BOOTSTRAP — point harmony-client at PR A's branch rev (controller-run)

PR B needs the new `verify(now_ms)` signature to compile. Until PR A merges, point the client at the **pushed branch SHA** from Task A2 Step 4.

- [ ] **Step 1: Bump the dev rev** in `harmony-client/src-tauri/Cargo.toml:87`

```toml
harmony-owner = { git = "https://github.com/zeblithic/harmony.git", rev = "<PR-A branch HEAD SHA>", features = ["recovery"] }
```

- [ ] **Step 2: Refresh the lockfile**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo update -p harmony-owner --precise <PR-A branch HEAD SHA> 2>&1 | tail -5 || cargo fetch
```
Expected: `Cargo.lock` updated to the new harmony-owner rev. (This Cargo.toml/lock change is a **temporary dev commit**; the final mergeable PR B re-points to PR A's *merged* commit — see Group B Task B7.)

---

## GROUP B — harmony-client PR

> Branch already created by controller: `zeb-378-enrollment-cert-expiry` (off `af2dae7c`, latest main), with the spec + this plan committed. All cargo commands run from `src-tauri/`.

### Task B1: `verify_enrolled_device(now_ms)` + `FriendHandshakeError::EnrollmentCertExpired`

**Files:**
- Modify: `src-tauri/src/iroh_friend_acceptor.rs` (enum ~281, `verify_enrolled_device` ~398)
- Test: `src-tauri/src/iroh_friend_acceptor.rs` (test mod)

- [ ] **Step 1: Write the failing test** (in the `iroh_friend_acceptor` test mod, near the other `verify_enrolled_device_*` tests ~1494)

```rust
#[test]
fn verify_enrolled_device_rejects_expired_cert() {
    // mint a TestOwner-style cert with a past expiry. Reuse the existing test
    // helper that builds an owner, but mint the cert with Some(expiry).
    let (master_sk, master_bundle) = enrollment_test_keys(0x51); // existing helper pattern
    let (device_sk, device_bundle) = enrollment_test_keys(0x52);
    let device_id = device_bundle.identity_hash();
    let owner = OwnerAddr(master_bundle.identity_hash());
    let cert = EnrollmentCert::sign_master(
        &master_sk, master_bundle, device_id, device_bundle, 1_000, Some(2_000),
    )
    .expect("sign");
    let _ = device_sk;
    // now = 2_001 > 2_000 → expired.
    assert!(matches!(
        verify_enrolled_device(&cert, owner, 2_001),
        Err(FriendHandshakeError::EnrollmentCertExpired)
    ));
    // valid before expiry.
    assert!(verify_enrolled_device(&cert, owner, 1_500).is_ok());
}
```

(If no `enrollment_test_keys` helper exists, build the bundles inline as `community_membership.rs:1452-1459` does — `ed25519_dalek::SigningKey::from_bytes` + `PubKeyBundle { classical: ClassicalKeys { ed25519_verify, x25519_pub: [0;32] }, post_quantum: None }`. The implementer reads the existing `verify_enrolled_device_accepts_valid_cert` test for the exact in-scope helper.)

- [ ] **Step 2: Run to verify it fails** (signature is 2-arg)

Run: `cd src-tauri && cargo test -p harmony-app --lib verify_enrolled_device_rejects_expired 2>&1 | tail -15`
Expected: compile error — `verify_enrolled_device` takes 2 args / `EnrollmentCertExpired` not found.

- [ ] **Step 3: Add the error variant** (`iroh_friend_acceptor.rs`, in `FriendHandshakeError` ~308, after `SignatureInvalid`)

```rust
    /// `cert.verify()` failed specifically because the cert's `expires_at` is in
    /// the past (ZEB-378). Distinct from `EnrollmentCertInvalid` for telemetry.
    #[error("enrollment cert expired")]
    EnrollmentCertExpired,
```

- [ ] **Step 4: Make `verify_enrolled_device` clock-aware** (`iroh_friend_acceptor.rs:398`)

```rust
pub fn verify_enrolled_device(
    cert: &EnrollmentCert,
    claimed_owner: OwnerAddr,
    now_ms: u64,
) -> Result<[u8; 32], FriendHandshakeError> {
    cert.verify(now_ms).map_err(|e| match e {
        harmony_owner::OwnerError::EnrollmentCertExpired { .. } => {
            FriendHandshakeError::EnrollmentCertExpired
        }
        _ => FriendHandshakeError::EnrollmentCertInvalid,
    })?;
    if !matches!(cert.issuer, EnrollmentIssuer::Master { .. }) {
        return Err(FriendHandshakeError::EnrollmentCertInvalid);
    }
    if cert.owner_id != claimed_owner.0 {
        return Err(FriendHandshakeError::EnrollmentOwnerMismatch);
    }
    Ok(cert.device_pubkeys.classical.ed25519_verify)
}
```

(Confirm the `OwnerError` path: it is re-exported as `harmony_owner::OwnerError`. If the file already imports it under another path, match that.)

- [ ] **Step 5: Update the in-file `verify_enrolled_device` test call sites to pass a clock**

The existing tests at `iroh_friend_acceptor.rs:1496, 1508, 1521, 1538, 1552, 1645, 1742-area` call it with 2 args. Add `, 0` (their certs are `None`-expiry, so `0` preserves behavior). The implementer updates each to `verify_enrolled_device(&cert, owner, 0)`.

- [ ] **Step 6: Run the verify_enrolled_device tests**

Run: `cd src-tauri && cargo test -p harmony-app --lib verify_enrolled_device 2>&1 | tail -20`
Expected: all `verify_enrolled_device_*` tests pass incl. the new expired-cert one.

- [ ] **Step 7: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/src/iroh_friend_acceptor.rs
git commit -m "feat(zeb-378): clock-aware verify_enrolled_device + EnrollmentCertExpired

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

### Task B2: Thread `now_ms` through the friend-handshake auth wrappers

**Files:**
- Modify: `src-tauri/src/iroh_friend_acceptor.rs` (`authenticate_friend_request` ~575, `process_friend_request` ~601, their callers ~1028/1068/1096, tests)

- [ ] **Step 1: Add `now_ms` params to the pure auth fns**

```rust
pub fn authenticate_friend_request(
    req: &FriendLinkRequest,
    now_ms: u64,
) -> Result<(), FriendHandshakeError> {
    let device_key = verify_enrolled_device(&req.enrollment, req.from_addr, now_ms)?;
    // ... rest unchanged ...
}
```

```rust
#[allow(clippy::too_many_arguments)]
pub fn process_friend_request(
    state: &mut OwnerState,
    learned_at: Hlc,
    req: &FriendLinkRequest,
    self_owner: OwnerAddr,
    self_display: Option<String>,
    self_enrollment: &EnrollmentCert,
    self_device2: &ed25519_dalek::SigningKey,
    keytree: &crate::owner_state_crypto::KeyTree,
    now_ms: u64,
) -> Result<FriendLinkAccepted, FriendHandshakeError> {
    let device_key = verify_enrolled_device(&req.enrollment, req.from_addr, now_ms)?;
    // ... rest unchanged ...
}
```

- [ ] **Step 2: Update production callers (transport handlers) to pass `wall_now_ms()`**

- `iroh_friend_acceptor.rs:1028` `authenticate_friend_request(&req)` → `authenticate_friend_request(&req, wall_now_ms())`
- `iroh_friend_acceptor.rs:1068` and `:1096` `process_friend_request(...)` → append `, wall_now_ms()` as the final arg.

(`wall_now_ms()` is the private helper already in this file at `:540` — in-scope.)

- [ ] **Step 3: Update test callers to pass `0`**

`authenticate_friend_request` tests `:2116, :2122` and `process_friend_request` tests `:1613, :1681, :1720, :1762, :2138` — append `, 0` (None-expiry certs, behavior-preserving).

- [ ] **Step 4: Add an end-to-end expired-handshake test** (test mod)

```rust
#[test]
fn authenticate_friend_request_rejects_expired_cert() {
    // Build a valid FriendLinkRequest whose enrollment cert has expires_at in the
    // past, reusing the existing request-builder test helper but minting the cert
    // with Some(expiry). now > expiry → EnrollmentCertExpired.
    let req = friend_request_with_cert_expiry(/*issued*/1_000, /*expires*/Some(2_000));
    assert!(matches!(
        authenticate_friend_request(&req, 2_001),
        Err(FriendHandshakeError::EnrollmentCertExpired)
    ));
    assert!(authenticate_friend_request(&req, 1_500).is_ok());
}
```

(The implementer adapts the existing `authenticate_friend_request_accepts_valid_and_rejects_tampered` test's request builder to accept a cert expiry; if that builder is inline, factor a tiny local helper `friend_request_with_cert_expiry`.)

- [ ] **Step 5: Run + commit**

```bash
cd src-tauri && cargo test -p harmony-app --lib 'authenticate_friend_request|process_friend_request' 2>&1 | tail -20
```
Expected: all pass.
```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/iroh_friend_acceptor.rs
git commit -m "feat(zeb-378): thread now_ms through friend-handshake auth wrappers

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

### Task B3: Thread `now_ms` through the referral-catalog auth

**Files:**
- Modify: `src-tauri/src/referral_catalog.rs` (`authenticate_catalog_request` ~249, `verify_referral_catalog` ~300, tests)
- Modify: `src-tauri/src/iroh_pex_acceptor.rs` (callers ~40, ~186; tests)

- [ ] **Step 1: Add `now_ms` params**

```rust
pub fn authenticate_catalog_request(
    req: &CatalogRequest,
    self_owner: OwnerAddr,
    now_ms: u64,
) -> Result<(), ReferralAuthError> {
    if req.to_addr != self_owner {
        return Err(ReferralAuthError::WrongTarget);
    }
    let device_key = verify_enrolled_device(&req.enrollment, req.from_addr, now_ms)
        .map_err(|_| ReferralAuthError::Auth)?;
    // ... rest unchanged ...
}
```

```rust
pub fn verify_referral_catalog(
    cat: &ReferralCatalog,
    expected_author: OwnerAddr,
    expected_subject: OwnerAddr,
    now_ms: u64,
) -> Result<(), ReferralAuthError> {
    if cat.author != expected_author {
        return Err(ReferralAuthError::AuthorMismatch);
    }
    let device_key = verify_enrolled_device(&cat.enrollment, cat.author, now_ms)
        .map_err(|_| ReferralAuthError::Auth)?;
    // ... rest unchanged ...
}
```

(Expired → coarse `ReferralAuthError::Auth`, per spec.)

- [ ] **Step 2: Update production callers** (`iroh_pex_acceptor.rs` has its own `wall_now_ms()` at `:251`)

- `iroh_pex_acceptor.rs:40` `authenticate_catalog_request(req, self_owner)` → `authenticate_catalog_request(req, self_owner, wall_now_ms())`
- `iroh_pex_acceptor.rs:186` `authenticate_catalog_request(&req, self.self_owner)` → `..., wall_now_ms())`
- `lib.rs:35553` `verify_referral_catalog(&cat, friend_owner, self_owner)` → `..., crate::iroh_friend_acceptor::wall_now_ms())` (requires Task B4's `pub(crate)` promotion; sequence B4 before this push or do both in this task — see B4).

- [ ] **Step 3: Update test callers to pass `0`**

`referral_catalog.rs` tests `:553,556,559,564,582,594,604,607,612,682` and `iroh_pex_acceptor.rs` tests `:316,348,406,442,481` — add the trailing `, 0`.

- [ ] **Step 4: Add an expired-catalog test** (`referral_catalog.rs` test mod)

```rust
#[test]
fn verify_referral_catalog_rejects_expired_cert() {
    // adapt the existing catalog-signing test helper to mint enrollment with a
    // past expiry; assert Auth rejection at now > expiry, Ok before.
    let (cat, author, subject) = referral_catalog_with_cert_expiry(1_000, Some(2_000));
    assert!(matches!(
        verify_referral_catalog(&cat, author, subject, 2_001),
        Err(ReferralAuthError::Auth)
    ));
    assert!(verify_referral_catalog(&cat, author, subject, 1_500).is_ok());
}
```

- [ ] **Step 5: Run + commit** (combine with B4 if the `lib.rs:35553` caller needs the promoted helper)

```bash
cd src-tauri && cargo test -p harmony-app --lib 'referral_catalog|catalog_request' 2>&1 | tail -20
```

### Task B4: `lib.rs` `verify_enrolled_device` direct callers + promote `wall_now_ms`

**Files:**
- Modify: `src-tauri/src/iroh_friend_acceptor.rs:540` (visibility)
- Modify: `src-tauri/src/lib.rs:33819, 34056, 36052` (verify_enrolled_device) and `:35553` (verify_referral_catalog, from B3)

- [ ] **Step 1: Promote the clock helper**

`iroh_friend_acceptor.rs:540` `fn wall_now_ms() -> u64` → `pub(crate) fn wall_now_ms() -> u64`. (Keep the existing doc comment.)

- [ ] **Step 2: Update the three `lib.rs` verify_enrolled_device call sites**

Each currently calls `verify_enrolled_device(&<cert>, <addr>)`. Append `, crate::iroh_friend_acceptor::wall_now_ms()`:
- `lib.rs:33819` `verify_enrolled_device(&payload.inviter_enrollment, payload.inviter_addr)` → `..., crate::iroh_friend_acceptor::wall_now_ms())`
- `lib.rs:34056` `verify_enrolled_device(&accepted.enrollment, payload.inviter_addr)` → `..., crate::iroh_friend_acceptor::wall_now_ms())`
- `lib.rs:36052` `verify_enrolled_device(&accepted.enrollment, target_addr_master)` → `..., crate::iroh_friend_acceptor::wall_now_ms())`

(These are live invite-accept friend flows → live clock is correct. If a `now_ms`/wall clock is already in scope at any of these sites, prefer reusing it; otherwise the helper is correct.)

- [ ] **Step 3: Run + commit**

```bash
cd src-tauri && cargo build -p harmony-app 2>&1 | tail -15   # confirm lib.rs compiles
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/iroh_friend_acceptor.rs src-tauri/src/lib.rs src-tauri/src/referral_catalog.rs src-tauri/src/iroh_pex_acceptor.rs
git commit -m "feat(zeb-378): thread live clock through referral + lib.rs auth call sites

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

### Task B5: Community-membership event-time expiry (`enrolled_key_from_cert`)

**Files:**
- Modify: `src-tauri/src/community_membership.rs:1228`
- Test: `src-tauri/src/community_membership.rs` (test mod, near `enrolled_key_from_cert_accepts_valid_cert` ~10497)

- [ ] **Step 1: Write the failing determinism test**

```rust
#[test]
fn enrolled_key_from_cert_rejects_cert_expired_at_event_time() {
    // Event stamped at wall_ms = T; cert expires_at = T-1 (expired as-of the event).
    let event = membership_event_with_cert_expiry(/*event_wall_ms*/3_000, /*expires*/Some(2_999));
    assert!(matches!(
        enrolled_key_from_cert(&event),
        Err(VerifyError::EnrollmentCertInvalid)
    ));
    // Cert valid as-of the event (expires_at = T) → accepted.
    let ok_event = membership_event_with_cert_expiry(3_000, Some(3_000));
    assert!(enrolled_key_from_cert(&ok_event).is_ok());
    // Determinism: the decision depends only on event.at.wall_ms, never on the
    // current wall clock — same event, same result, regardless of when run.
    assert!(enrolled_key_from_cert(&ok_event).is_ok());
}
```

(`membership_event_with_cert_expiry` adapts the existing `enrolled_key_from_cert_accepts_valid_cert` event builder to set the cert's `expires_at` and the event's `at.wall_ms`. The implementer reads ~10497 for the in-scope helper and the `SignedMembershipEvent` `.at` field.)

- [ ] **Step 2: Run to verify it fails**

Run: `cd src-tauri && cargo test -p harmony-app --lib enrolled_key_from_cert_rejects_cert_expired 2>&1 | tail -15`
Expected: FAIL — currently `enrolled_key_from_cert` ignores expiry, so the expired event is wrongly `Ok`.

- [ ] **Step 3: Pass the event clock into verify** (`community_membership.rs:1228`)

```rust
    cert.verify(event.at.wall_ms)
        .map_err(|_| VerifyError::EnrollmentCertInvalid)?;
```

- [ ] **Step 4: Run the test + the membership suite**

Run: `cd src-tauri && cargo test -p harmony-app --lib 'enrolled_key_from_cert|verify_event' 2>&1 | tail -25`
Expected: new test passes; existing membership tests pass (their certs are `None`-expiry, so event-time check is a no-op). Any existing test that *does* set a non-`None` cert expiry below its event's `wall_ms` will now reject — read it; if it asserted acceptance it encoded the bug (flip the expectation, note in commit).

- [ ] **Step 5: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/community_membership.rs
git commit -m "feat(zeb-378): enforce cert expiry at event-time on membership path (CRDT-deterministic)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

### Task B6: `DmOutbox::new` wiring assert + remaining `cert.verify()` sweep + full gate

**Files:**
- Modify: `src-tauri/src/dm_outbox.rs:461`
- Modify: `src-tauri/src/community_membership.rs:1470` (TestOwner helper) + any other remaining 0-arg `cert.verify()` callers surfaced by the compiler.

- [ ] **Step 1: `DmOutbox::new` assert → structural-only (`verify(0)`)**

`dm_outbox.rs:461` `assert!(enrollment_cert.verify().is_ok(), ...)` →

```rust
        assert!(
            enrollment_cert.verify(0).is_ok(),
            "DmOutbox: enrollment_cert must verify (structural; expiry-agnostic by design — ZEB-378)"
        );
```

- [ ] **Step 2: Update the `TestOwner` mint self-check + sweep all remaining 0-arg callers**

`community_membership.rs:1470` `cert.verify().expect("self-minted cert verifies");` → `cert.verify(0).expect(...)`. Then let the compiler find any other stragglers:

```bash
cd src-tauri && cargo build -p harmony-app --all-targets --features test-fixtures 2>&1 | grep -E "this method takes|expected .* arguments" | head
```
Update each remaining `cert.verify()` / `.verify()` on an `EnrollmentCert` to `verify(0)` (behavior-preserving for None-expiry test fixtures).

- [ ] **Step 3: Full harmony-client gate**

```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked -p harmony-app --lib --features test-fixtures
```
Expected: fmt clean, clippy 0 warnings, lib tests pass. (Reserve the full `--workspace --all-targets` nextest for the final sweep before pushing — see B7.)

- [ ] **Step 4: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/dm_outbox.rs src-tauri/src/community_membership.rs
git commit -m "feat(zeb-378): DmOutbox wiring assert stays structural (verify(0)) + verify() caller sweep

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

### Task B7: Final integration gate + push + PR B (controller-run, after PR A merges)

- [ ] **Step 1: Full-suite gate** (integration tests included)

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tail -30
```
Expected: green (known iroh/zenoh first-bind orphan-flakes are non-blocking; re-run `--failed` once if they appear).

- [ ] **Step 2: Re-point the rev to PR A's *merged* commit** (only after the maintainer merges PR A)

Update `src-tauri/Cargo.toml:87` `rev = "<PR-A MERGED commit SHA on harmony main>"`, then `cargo update -p harmony-owner --precise <merged SHA>`, rebuild + re-gate. Commit:
```bash
git commit -am "chore(zeb-378): pin harmony-owner to merged PR-A rev

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

- [ ] **Step 3: Push + open PR B**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git push -u origin zeb-378-enrollment-cert-expiry
gh pr create --repo zeblithic/harmony-client --title "ZEB-378: enforce EnrollmentCert expiry (client clock threading)" --body "<PR-B body: links spec + plan + PR A; clock-source table; defense-in-depth summary; inert-today note>"
```

---

## Self-review notes

- **Spec coverage:** A1/A2 cover Component 1 (harmony-owner). B1 covers the friend-path distinct error. B2/B4 cover friend handshake. B3/B4 cover referral catalog. B5 covers membership event-time + CRDT determinism. B6 covers DmOutbox structural-only + the verify() sweep. Testing section: A1/B1/B2/B3/B5 add the required regression tests. Sequencing/gating: A0 (stale-branch handling) + bootstrap + B7 (rev-bump dance).
- **`now_ms = 0` rule** is stated once up top and referenced; every behavior-preserving call site uses it; live paths use real clocks; membership uses event-time.
- **Type/name consistency:** `verify(now_ms)`, `OwnerError::EnrollmentCertExpired { expires_at, now_ms }`, `FriendHandshakeError::EnrollmentCertExpired`, `verify_enrolled_device(cert, owner, now_ms)` are used identically across tasks.
- **Open implementer latitude (acceptable):** the exact test-helper names (`enrollment_test_keys`, `friend_request_with_cert_expiry`, `referral_catalog_with_cert_expiry`, `membership_event_with_cert_expiry`) are *new local helpers* the implementer factors from the existing adjacent test builders; each task names the existing test to read for the in-scope pattern.
