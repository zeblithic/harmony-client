# ZEB-854 — Reject future-stamped sibling liveness certs at ingest — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop a sibling device's future-stamped `LivenessCert` from reading as permanently "active"/"fresh" by rejecting a beyond-tolerance future timestamp at the single client ingest funnel (`merge_trust_remote_into_local`).

**Architecture:** Two client-side changes, primitive → consumer. (1) Add a seconds-domain control-tier forward-skew helper to `clock_trust.rs`. (2) Apply it in the liveness fold of `owner_trust_sync.rs::merge_trust_remote_into_local`, before the LWW check, refusing a future cert; extract a `_now`-injecting inner so the branch (including the fail-open path) is deterministically testable. No `harmony-owner` change.

**Tech Stack:** Rust, `cargo nextest`, the `clock_trust` bounded-time policy module, `harmony-owner` CRDT trust state (frozen, rev `b904b0b`).

**Spec:** `docs/superpowers/specs/2026-08-04-zeb-854-reject-future-sibling-liveness-certs-design.md`

## Global Constraints

- **Control tier, 5 min** — liveness gates trust state; use `MAX_FORWARD_SKEW_MS` (5 min), never the 30-min display tier (`clock_trust` doc forbids pointing a control consumer at the display tier).
- **Fail-open on an unreadable local clock** — `now_secs == 0` (the `SystemTime…unwrap_or_default().as_secs()` sentinel) ⇒ accept (apply-all): a bad *local* clock must never drop honest sibling state.
- **Reject, never clamp** — a clamped future cert still reads fresh and diverges per-receiver; decline to `add_liveness`.
- **Reject before the LWW `known_newer` check** — a future cert that is strictly newer than a stored one must still be refused.
- **`LivenessCert::timestamp` is epoch seconds** — bound in the seconds domain (no `*1000`).
- **No `harmony-owner` change**; no change to windows/thresholds, `add_liveness`, or the CRDT/LWW merge semantics; no new persisted state; no frontend change.
- **Rust gates from `src-tauri/`:** `cargo nextest run --locked --workspace --all-targets --features test-fixtures`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo fmt --all -- --check`.

## File Structure

- `src-tauri/src/clock_trust.rs` — owns the bounded-time policy primitives. Add the seconds-domain control-tier const + helper + their unit tests here (one responsibility: the auditable bound).
- `src-tauri/src/owner_trust_sync.rs` — owns the sibling-trust CRDT merge. Add the reject branch + the `_at` test seam + merge tests here.

---

### Task 1: `clock_trust` — seconds-domain control-tier forward-skew bound

**Files:**
- Modify: `src-tauri/src/clock_trust.rs` (const near `MAX_FORWARD_SKEW_MS`:38 / `DISPLAY_SKEW_TOLERANCE_SECS`:47; helper near `wall_exceeds_forward_skew`:137; tests in the existing `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: existing `MAX_FORWARD_SKEW_MS` const and `reject_future(stamp, now, tolerance) -> bool` (both in `clock_trust.rs`).
- Produces:
  - `pub const MAX_FORWARD_SKEW_SECS: u64` ( = 300)
  - `pub fn secs_exceeds_forward_skew(stamp_secs: u64, now_secs: u64) -> bool` — `true` iff `stamp_secs` is > `MAX_FORWARD_SKEW_SECS` ahead of `now_secs`; `now_secs == 0` ⇒ `false` (apply-all). Task 2 consumes this.

- [ ] **Step 1: Write the failing tests**

Add to `clock_trust.rs`'s `#[cfg(test)] mod tests` (alongside `wall_exceeds_forward_skew_honors_the_inclusive_ceiling`):

```rust
#[test]
fn max_forward_skew_secs_is_five_minutes() {
    assert_eq!(MAX_FORWARD_SKEW_SECS, 300);
    assert_eq!(MAX_FORWARD_SKEW_SECS * 1000, MAX_FORWARD_SKEW_MS);
}

#[test]
fn secs_exceeds_forward_skew_zero_now_is_apply_all() {
    // Unreadable/pre-epoch local clock ⇒ never reject (fail-open).
    assert!(!secs_exceeds_forward_skew(u64::MAX, 0));
    assert!(!secs_exceeds_forward_skew(0, 0));
}

#[test]
fn secs_exceeds_forward_skew_honors_the_inclusive_ceiling() {
    let now = 1_700_000_000u64;
    assert!(!secs_exceeds_forward_skew(now, now)); // present: accept
    assert!(!secs_exceeds_forward_skew(now - 10_000, now)); // past: accept
    assert!(!secs_exceeds_forward_skew(now + MAX_FORWARD_SKEW_SECS, now)); // boundary: accept
    assert!(secs_exceeds_forward_skew(now + MAX_FORWARD_SKEW_SECS + 1, now)); // just over: reject
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(secs_exceeds_forward_skew) + test(max_forward_skew_secs_is_five_minutes)'`
Expected: FAIL to compile — `cannot find value MAX_FORWARD_SKEW_SECS` / `cannot find function secs_exceeds_forward_skew`.

- [ ] **Step 3: Add the const and helper**

After `DISPLAY_SKEW_TOLERANCE_SECS` (line 47) add the const:

```rust
/// [`MAX_FORWARD_SKEW_MS`] in whole seconds, for control-tier stamps whose
/// native unit is epoch-seconds (e.g. a `LivenessCert.timestamp`). Mirrors
/// [`DISPLAY_SKEW_TOLERANCE_SECS`] for the control tier.
pub const MAX_FORWARD_SKEW_SECS: u64 = MAX_FORWARD_SKEW_MS / 1000;
```

After `wall_exceeds_forward_skew` (line 139) add the helper:

```rust
/// `true` iff a control-tier epoch-**seconds** `stamp_secs` is implausibly far in
/// the receiver's future (> [`MAX_FORWARD_SKEW_SECS`] ahead of `now_secs`).
/// `now_secs == 0` (unreadable / pre-epoch local clock) ⇒ `false` (apply-all): a
/// bad LOCAL clock must never drop honest state. Seconds-native sibling of
/// [`wall_exceeds_forward_skew`]; boundary inclusive.
#[inline]
pub fn secs_exceeds_forward_skew(stamp_secs: u64, now_secs: u64) -> bool {
    now_secs != 0 && reject_future(stamp_secs, now_secs, MAX_FORWARD_SKEW_SECS)
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(secs_exceeds_forward_skew) + test(max_forward_skew_secs_is_five_minutes)'`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/clock_trust.rs
git commit -m "feat(zeb-854): clock_trust seconds-domain control-tier forward-skew bound"
```

---

### Task 2: `owner_trust_sync` — reject future sibling liveness certs at the merge

**Files:**
- Modify: `src-tauri/src/owner_trust_sync.rs` — `merge_trust_remote_into_local` (:64–124): split out `merge_trust_remote_into_local_at(now)`; add the reject in the liveness fold (:108–119). Tests in the existing `#[cfg(test)] mod tests` (:376).

**Interfaces:**
- Consumes: `clock_trust::secs_exceeds_forward_skew` (Task 1); existing test fixtures `test_mint(now) -> (OwnerState, RecoveryArtifact, SigningKey)`, `test_enroll_second_device(&artifact, &state, now) -> (SigningKey, EnrollmentCert)`, `test_liveness(&signer_sk, owner_id, now) -> LivenessCert`.
- Produces: `fn merge_trust_remote_into_local_at(local: &mut OwnerState, remote: OwnerState, now: u64) -> MergeOutcome` (module-private; the public `merge_trust_remote_into_local` delegates to it with the real clock). No public-signature change.

- [ ] **Step 1: Write the failing tests**

Add to `owner_trust_sync.rs`'s `#[cfg(test)] mod tests` (after `merge_revocation_wins_over_concurrent_liveness`):

```rust
#[test]
fn merge_rejects_future_stamped_sibling_liveness() {
    // A sibling cert stamped far in our future must NOT enter local.liveness —
    // else it reads "active"/"fresh" forever in harmony-owner's one-sided
    // freshness checks (ZEB-854).
    let now = 1_700_000_000u64;
    let (mut local, artifact, _sk1) = test_mint(now);
    let (sk2, cert2) = test_enroll_second_device(&artifact, &local, now + 10);
    let d2 = cert2.device_id;
    local
        .add_enrollment(cert2, now + 10, DEFAULT_ACTIVE_WINDOW_SECS)
        .unwrap();
    let owner_id = local.owner_id;

    let mut remote = local.clone();
    remote
        .add_liveness(test_liveness(&sk2, owner_id, now + 3600)) // +1h ≫ 5-min tol
        .unwrap();

    merge_trust_remote_into_local_at(&mut local, remote, now);
    assert!(!local.liveness.contains_key(&d2));
}

#[test]
fn merge_accepts_in_window_sibling_liveness() {
    let now = 1_700_000_000u64;
    let (mut local, artifact, _sk1) = test_mint(now);
    let (sk2, cert2) = test_enroll_second_device(&artifact, &local, now + 10);
    let d2 = cert2.device_id;
    local
        .add_enrollment(cert2, now + 10, DEFAULT_ACTIVE_WINDOW_SECS)
        .unwrap();
    let owner_id = local.owner_id;

    let mut remote = local.clone();
    remote
        .add_liveness(test_liveness(&sk2, owner_id, now + 60)) // within 5-min tol
        .unwrap();

    merge_trust_remote_into_local_at(&mut local, remote, now);
    assert!(local.liveness.contains_key(&d2));
}

#[test]
fn merge_fails_open_on_unreadable_local_clock() {
    // now == 0 (pre-epoch/unreadable) ⇒ apply-all: even a future cert is
    // accepted, so a bad LOCAL clock never drops honest sibling state.
    let now = 1_700_000_000u64;
    let (mut local, artifact, _sk1) = test_mint(now);
    let (sk2, cert2) = test_enroll_second_device(&artifact, &local, now + 10);
    let d2 = cert2.device_id;
    local
        .add_enrollment(cert2, now + 10, DEFAULT_ACTIVE_WINDOW_SECS)
        .unwrap();
    let owner_id = local.owner_id;

    let mut remote = local.clone();
    remote
        .add_liveness(test_liveness(&sk2, owner_id, now + 3600))
        .unwrap();

    merge_trust_remote_into_local_at(&mut local, remote, 0);
    assert!(local.liveness.contains_key(&d2));
}

#[test]
fn merge_reject_does_not_clobber_stored_honest_liveness() {
    // A stored honest cert must survive an incoming future cert for the same
    // signer (reject fires before the LWW branch).
    let now = 1_700_000_000u64;
    let (mut local, artifact, _sk1) = test_mint(now);
    let (sk2, cert2) = test_enroll_second_device(&artifact, &local, now + 10);
    let d2 = cert2.device_id;
    local
        .add_enrollment(cert2, now + 10, DEFAULT_ACTIVE_WINDOW_SECS)
        .unwrap();
    let owner_id = local.owner_id;
    local
        .add_liveness(test_liveness(&sk2, owner_id, now + 30)) // stored honest cert
        .unwrap();

    let mut remote = local.clone();
    remote
        .add_liveness(test_liveness(&sk2, owner_id, now + 3600)) // future overwrite attempt
        .unwrap();

    merge_trust_remote_into_local_at(&mut local, remote, now);
    assert_eq!(local.liveness.get(&d2).unwrap().timestamp, now + 30);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(merge_rejects_future_stamped_sibling_liveness) + test(merge_accepts_in_window_sibling_liveness) + test(merge_fails_open_on_unreadable_local_clock) + test(merge_reject_does_not_clobber_stored_honest_liveness)'`
Expected: FAIL to compile — `cannot find function merge_trust_remote_into_local_at`.

- [ ] **Step 3: Extract the `_at` seam and add the reject**

Replace the head of `merge_trust_remote_into_local` (:64–69) so the public fn computes `now` and delegates, and the body moves into a new private `_at` that takes `now`:

```rust
pub fn merge_trust_remote_into_local(local: &mut OwnerState, remote: OwnerState) -> MergeOutcome {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    merge_trust_remote_into_local_at(local, remote, now)
}

fn merge_trust_remote_into_local_at(
    local: &mut OwnerState,
    remote: OwnerState,
    now: u64,
) -> MergeOutcome {
    let before = harmony_owner::cbor::to_canonical(&*local).ok();
    let OwnerState {
        owner_id: _,
        enrollments,
        vouching,
        revocations,
        liveness,
    } = remote;
    // …enrollments / revocations / vouching loops move here VERBATIM
    //   (the pre-change lines 77–107, unchanged)…
```

Then the liveness loop (pre-change :108–119) becomes, with the reject added first:

```rust
    for (id, cert) in liveness {
        // ZEB-854: harmony-owner's freshness reads (trust.rs / state.rs) are
        // one-sided lower bounds, so a sibling cert stamped in our future reads
        // as "active"/"fresh" forever. Reject a beyond-tolerance future stamp at
        // this ingest funnel — the sibling-side mirror of the ZEB-721 self-cert
        // ClockRegressed guard, extending the ZEB-847 reject-at-ingest pattern to
        // this (trust) merge. Fail-open when our own clock is unreadable (now == 0).
        if crate::clock_trust::secs_exceeds_forward_skew(cert.timestamp, now) {
            tracing::warn!(
                skew_secs = cert.timestamp.saturating_sub(now),
                "trust merge: sibling liveness cert rejected (future-stamped beyond skew tolerance)"
            );
            continue;
        }
        let known_newer = local
            .liveness
            .get(&id)
            .is_some_and(|l| l.timestamp >= cert.timestamp);
        if known_newer {
            continue;
        }
        if let Err(e) = local.add_liveness(cert) {
            tracing::warn!(error = %e, "trust merge: liveness dropped");
        }
    }
    let after = harmony_owner::cbor::to_canonical(&*local).ok();
    MergeOutcome {
        changed: before != after,
    }
}
```

(The `now` computation is now ONLY in the public wrapper; the `_at` body uses the `now` parameter. The enrollment/revocation active-window checks that already used `now` — pre-change :83 and :91 — keep working unchanged.)

- [ ] **Step 4: Run the new tests to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(merge_rejects_future_stamped_sibling_liveness) + test(merge_accepts_in_window_sibling_liveness) + test(merge_fails_open_on_unreadable_local_clock) + test(merge_reject_does_not_clobber_stored_honest_liveness)'`
Expected: PASS (4 tests).

- [ ] **Step 5: Run the whole `owner_trust_sync` + `clock_trust` module tests (no regressions)**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(merge) + test(trust_persist) + test(forward_skew) + test(reject_future)'`
Expected: PASS — the pre-existing merge tests (`merge_folds_new_enrollment_from_remote`, `merge_is_idempotent_and_reports_unchanged`, `merge_revocation_wins_over_concurrent_liveness`, `merge_drops_record_for_foreign_owner_without_degrading`) stay green (they call the public fn, whose behavior is unchanged), and Task 1's tests stay green.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/owner_trust_sync.rs
git commit -m "fix(zeb-854): reject future-stamped sibling liveness certs at trust merge"
```

---

## Final gate (before PR)

Run the full CI-parity sweep from `src-tauri/` (Rust-only change; frontend gates are parity only):

```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

Expected: fmt clean; clippy clean; all tests pass. Then open the PR (`Closes ZEB-854`) and fire `@coderabbitai review` exactly once.

## Self-Review

- **Spec coverage:**
  - `clock_trust` const + helper (spec §The change/1) → Task 1. ✓
  - Reject in the liveness fold before LWW (spec §The change/2) → Task 2, Step 3. ✓
  - `_at` test seam (spec §The change/3) → Task 2, Step 3 + tests. ✓
  - Fail-open on `now==0` (spec §Error handling) → Task 1 test `..._zero_now_is_apply_all` + Task 2 test `merge_fails_open_...`. ✓
  - Reject-doesn't-clobber-stored (spec §Behavior matrix row 4) → Task 2 test `..._does_not_clobber_...`. ✓
  - Testing plan (spec §Testing) → Task 1 (3 tests) + Task 2 (4 tests) + final sweep. ✓
  - Out-of-scope items (no harmony-owner change, no view-gate, no non-liveness bounds) → honored; nothing in the tasks touches them.
- **Placeholder scan:** none. The one "move verbatim" is a precise relocation of named pre-change lines (77–107), not a deferral.
- **Type consistency:** `secs_exceeds_forward_skew(stamp_secs: u64, now_secs: u64) -> bool` and `merge_trust_remote_into_local_at(&mut OwnerState, OwnerState, u64) -> MergeOutcome` are used identically in both tasks' code and tests; `MAX_FORWARD_SKEW_SECS` is `u64`. ✓
