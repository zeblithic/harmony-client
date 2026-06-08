# ZEB-378 — Enforce `EnrollmentCert` expiry at the `verify()` core

**Status:** Approved 2026-06-08
**Issue:** [ZEB-378](https://linear.app/zeblith/issue/ZEB-378) (Medium, `Bug`, harmony-client; parent ZEB-321)
**Found by:** ZEB-375 Phase 2a adversarial review
**Cross-repo:** harmony (`harmony-owner` crate) + harmony-client

## Problem

`EnrollmentCert::verify()` (harmony-owner `certs/enrollment.rs:91`) cryptographically
binds an `expires_at: Option<u64>` into the signed payload but **never compares it
against a clock** — in *neither* the `Master` nor the `Quorum` branch. Every
point-to-point auth path inherits this gap:

- **Friend handshake** — `verify_enrolled_device` (`iroh_friend_acceptor.rs:398`)
  wraps `cert.verify()` and adds a Master-issuer + owner-binding check, but no
  expiry. Used by `authenticate_friend_request` + `process_friend_request`.
- **Referral catalog (ZEB-375 Phase 2a)** — `authenticate_catalog_request` /
  `verify_referral_catalog` (`referral_catalog.rs`) also flow through
  `verify_enrolled_device`. This path is **fully offline** (no live token/connection
  gate as a backstop), so a stale cert has no secondary mitigation.
- **Community membership** — `enrolled_key_from_cert` (`community_membership.rs:1221`)
  calls `cert.verify()` directly for identity-introducing CRDT events.
- **Owner state** — `OwnerState::add_enrollment` (`state.rs:208`) and the
  **`DmOutbox::new`** construction assert (`dm_outbox.rs:461`) also call `verify()`.

**Consequence:** a cert with a past `expires_at` still passes, so its bound
device-#2 key still authenticates on every path above.

## Threat model & severity

The cert rides an **owner-device-signed** payload, so this is **griefing within a
trusted community / a compromised-or-revoked member**, not open-internet DoS. The
existing verify-on-fetch `hash == CID` and signature checks close
byte-*substitution*; this closes the orthogonal **time** dimension that signatures
do not bound.

Pre-existing, codebase-wide gap — **not** introduced by ZEB-375. **Latent today**
because production certs are minted with `expires_at: None` (non-expiring), which
makes the check a no-op. Hence Medium, not Urgent — but it must be closed *before*
certs carry real expiries.

## Goal

Enforce expiry at the **source** — inside `EnrollmentCert::verify()` itself — so the
gap cannot be reintroduced by any current or future caller, rather than relying on
each call site to remember a separate check.

## Architecture — two coordinated PRs

harmony-client pins `harmony-owner` to an immutable git rev
(`04449d60…`, `src-tauri/Cargo.toml`). Making `verify()` clock-aware is a breaking
signature change in harmony-owner, so the work splits:

- **PR A (harmony repo, `harmony-owner`):** `verify(&self) → verify(&self, now_ms: u64)`,
  add the expiry check + `OwnerError` variant, update the one production caller and
  the in-crate tests.
- **PR B (harmony-client):** bump the `harmony-owner` rev to PR A's merged commit;
  thread a clock through every `verify()` caller; add the friend-path expiry error;
  add regression tests.

**Merge order is A → rev-bump → B** (PR B cannot compile against the new signature
until PR A is merged). During PR-A review, PR B can be developed/tested locally by
temporarily pointing `Cargo.toml` at the PR-A branch rev; the *mergeable* PR B
references the merged commit. Both merges are the maintainer's gate.

## Component 1 — harmony-owner: clock-aware `verify()`

In `EnrollmentCert::verify`, after the `version` check and **before** the issuer
`match` (so it covers both `Master` and `Quorum`):

```rust
pub fn verify(&self, now_ms: u64) -> Result<(), OwnerError> {
    if self.version != ENROLLMENT_VERSION {
        return Err(OwnerError::UnknownVersion(self.version));
    }
    if let Some(exp) = self.expires_at {
        if now_ms > exp {
            return Err(OwnerError::EnrollmentCertExpired { expires_at: exp, now_ms });
        }
    }
    match &self.issuer { /* unchanged */ }
}
```

- New `error.rs` variant:
  `EnrollmentCertExpired { expires_at: u64, now_ms: u64 }` (sibling of `Revoked`),
  `#[error("enrollment cert expired at {expires_at} (now {now_ms})")]`.
- **Production caller** `OwnerState::add_enrollment(cert, now, active_window_secs)`
  (`state.rs:208`) already has `now` in scope → `cert.verify(now)?`. No new plumbing.
- **In-crate tests** (`enrollment.rs` ×3, `reclamation.rs` ×3) pass a fixed clock
  constant well past each fixture's `issued_at`.

`now_ms == 0` semantics: `expires_at.map(|e| 0 > e)` is always `false`, so a `0`
clock degrades to a structural+signature-only verify. We do **not** rely on this as
an API contract; every caller passes a real clock (see the table). It is noted only
so reviewers understand `0` is safe, not a bypass to depend on.

## Component 2 — harmony-client: thread the clock per caller

The clock *source* is **security-load-bearing** and differs by path:

| Caller | Clock passed | Rationale |
|---|---|---|
| `verify_enrolled_device(cert, owner, now_ms)` — the chokepoint | live `wall_now_ms()` (from transport) | friend/referral auth is **live**: reject a cert expired *now* |
| `authenticate_friend_request` / `process_friend_request` / `authenticate_catalog_request` / `verify_referral_catalog` | gain a `now_ms` param; their transport/IPC callers pass `wall_now_ms()` | keeps the pure auth fns deterministically testable, mirroring the existing `is_friend_token_active(&sig, now_ms)` idiom |
| `enrolled_key_from_cert(event)` (`community_membership.rs:1228`) | **`event.at.wall_ms`** (event-time, *not* now) | **CRDT determinism** — see Component 3 |
| `DmOutbox::new` assert (`dm_outbox.rs:461`) | `wall_now_ms()` | fail-fast: don't construct around an expired cert (inert today) |

`verify_enrolled_device` returns the new
`FriendHandshakeError::EnrollmentCertExpired` when `cert.verify(now_ms)` yields the
owner-side expiry error (mapped from `OwnerError::EnrollmentCertExpired`). The other
two paths keep their existing coarse buckets — referral → `ReferralAuthError::Auth`,
membership → `VerifyError::EnrollmentCertInvalid` — exactly as they already collapse
`EnrollmentCertInvalid`/`OwnerMismatch` (rejection is the security property; no new
variants are warranted there, YAGNI).

The 4 production callers of `enrolled_key_from_cert`
(`community_invite.rs:1328,1574`; `lib.rs:17656,20211`; `community_membership.rs:2650`)
**do not change** — the clock is sourced from the `SignedMembershipEvent` the
function already receives, so there is **no cascade** into `verify_event`.

## Component 3 — why community membership uses event-time, not `now`

Enforcing expiry inside `verify()` means it now also fires on the
**community-membership CRDT path**. Membership events are materialized and replayed
across machines at *different* wall-clock times. If expiry there compared
`expires_at` against the **current** wall clock, two peers could disagree on a single
event's validity (one materializes before expiry, the other after) →
**state divergence**. Passing the **event's own** `event.at.wall_ms` makes the
decision a pure function of the event, identical on every machine forever, and asks
the semantically correct question: *was the cert valid at the moment the event was
signed?* This is deterministic and is the intended hardening.

`add_enrollment` and `DmOutbox::new` are **local, live** operations (not replayed
CRDT state), so they correctly use the live wall clock.

## Data flow & error handling

- Expired cert on the **friend handshake** → `verify_enrolled_device` returns
  `FriendHandshakeError::EnrollmentCertExpired` → handshake rejected.
- Expired cert on the **referral catalog** → `ReferralAuthError::Auth` → rejected.
- Expired cert on a **membership event** (as-of `event.at.wall_ms`) →
  `VerifyError::EnrollmentCertInvalid` → event rejected by `verify_event`.
- Expired cert at **`add_enrollment` / `DmOutbox::new`** → `OwnerError::…Expired` /
  construction assert fails.

All are no-ops for `expires_at: None`, which is every production cert today.

## Testing

- **harmony-owner unit (`enrollment.rs`):** a cert with `expires_at: Some(past)` →
  `verify(now)` is `Err(EnrollmentCertExpired)`; `None` and `Some(future)` → `Ok`;
  the existing structural/sig tests pass a fixed clock.
- **harmony-client friend path:** `verify_enrolled_device` rejects a past-expiry cert
  (`EnrollmentCertExpired`); accepts `None`/future. `authenticate_friend_request`
  end-to-end: expired → reject.
- **harmony-client referral path:** `authenticate_catalog_request` /
  `verify_referral_catalog` reject a past-expiry cert (`Auth`); accept `None`/future.
- **harmony-client membership path:** `enrolled_key_from_cert` rejects an event whose
  cert was expired as-of `event.at.wall_ms`; accepts when the cert was still valid at
  that timestamp — *and* a determinism assertion: the same event verifies identically
  regardless of "current" wall clock.
- All tests inject a fixed `now_ms`; existing 2-arg `verify_enrolled_device` /
  `cert.verify()` call sites are updated to pass a fixed clock constant.
- **Gates:** PR A — harmony-owner `cargo fmt --all -- --check`, `clippy -D warnings`,
  `nextest`. PR B — the full harmony-client suite (`fmt`, `clippy --all-targets
  --features test-fixtures -D warnings`, `nextest --all-targets --features
  test-fixtures`, MSRV, frontend unaffected).

## Sequencing / gating

- **Safe to land now.** Every behavior change is a no-op while certs carry
  `expires_at: None`, so the change **cannot perturb** the in-flight Ildwyn+Koya
  friend-handshake smoke test. No need to wait for that test to finish.
- **The local harmony checkout is on a stale branch** (`zeb-380-relay-pool-hotswap`,
  already-merged work, dirty `Cargo.lock`). PR A **must** branch off fresh
  `origin/main`, not this branch.

## Acceptance criteria (from ticket)

- [ ] `verify_enrolled_device` rejects a past-`expires_at` cert; accepts `None` and
  future.
- [ ] All `verify_enrolled_device` (and `verify()`) call sites pass a real clock.
- [ ] Regression tests on friend-handshake + referral-catalog paths (plus membership,
  per the approved full-reach scope).
- [ ] Gates green in both repos.

## Out of scope

- **Per-path distinct error variants** on the referral/membership buckets — their
  coarse rejection is correct; add only if telemetry later needs the granularity.
- **Cert-rotation / re-issuance UX** when a cert nears expiry — separate concern; this
  ticket only *enforces* expiry.
- **Choosing production expiry durations / actually minting non-`None` `expires_at`**
  — a policy decision tracked separately; this ticket makes enforcement correct so
  that policy can be turned on safely.
