# ZEB-497: inviter_enrollment verification on the community redeem path — design

**Status:** approved 2026-06-18 (Jake)
**Ticket:** ZEB-497 — ZEB-340 §4 follow-up (the non-mechanical half; the mechanical removals were ZEB-496).
**Branch:** `zeb-497-invite-principal-auth` off `origin/main` @ `9a7f8221`. One PR, entirely in `harmony-client`.

## Problem

Invite-only `CommunityInvitePayload` carries an `inviter_enrollment: Option<EnrollmentCert>`
(`community_invite.rs:190`) — the inviter's owner→device binding. On the community redeem
path it is **only presence-validated** and never cryptographically consumed:

- `encode_invite_url` (`community_invite.rs:988`) rejects an invite-only payload that is *missing* the field.
- `decode_invite_url` (`community_invite.rs:1061`) repeats that presence check.
- No code on the community redeem path verifies the cert's signature, expiry, issuer, or binds it to
  anything. (Repo-wide, the only cryptographic consumer of an `inviter_enrollment` field is on the
  **friend** path — `verify_enrolled_device` at `iroh_friend_acceptor.rs:738`, wired at `lib.rs:40521` —
  against the *different* `FriendTokenPayload` struct.)

### Why this is currently safe but still wrong

In v1 the field is **redundant, not exploitable**. The real authenticator of a community invite is the
`InviteToken` signature, and v1 pins it to the **admin**:

- Invite generation is admin-only (`lib.rs:20611`: "restricts invite-only generation to the admin… Non-admin
  invite is a follow-up").
- On receipt, the counter-signer requires `invite_token.inviter == self_owner` and verifies the token
  signature against its *own* enrolled device key (`community_invite.rs:1635`, `:1661`).
- In the community log, `verify_event` PendingJoin gate **P2** forces `invite_token.inviter == admin_addr`
  (`community_membership.rs:2937`) and **P5** verifies the token signature against the inviter's enrolled key
  resolved from **already-materialized membership** (`community_membership.rs:2960`) — i.e. the admin's key
  from the synced log, *not* the URL's `inviter_enrollment`.

So substituting a forged `inviter_enrollment` into a URL changes nothing today: no redeem-path code reads it,
and the token signature (admin-bound, forge-resistant without the admin's device key) is the gate.

### The latent gap this closes

`inviter_enrollment` exists precisely for the cases v1 hasn't shipped (`community_invite.rs:186-188`): a
**non-admin inviter**, and/or a **cold-cache joiner** who has not synced the community log and therefore
*cannot* resolve the inviter's key from materialized membership (the input P5 depends on). The moment
`invite_token.inviter` is allowed to be a non-admin member, P5's materialized-membership lookup is unavailable
to a cold-cache joiner, and the only carrier of the inviter's owner→device binding is `inviter_enrollment` —
which nothing verifies. The friend path already closes this exact gap; the community path does not.

This ticket makes the existing-but-inert field a real cryptographic control now — before the non-admin-inviter
feature lands and turns the no-op into an exploitable hole — and gives the joiner local fail-fast rejection of
tampered/forged invite URLs in the meantime.

## Non-goals

- **`admin_bootstrap` is out of scope.** It is healthy and load-bearing: cert-verified via
  `verify_admin_bootstrap` → `enrolled_key_from_cert` (`community_invite.rs:1361`, `community_membership.rs:1314`),
  then inserted into the joiner's empty CRDT so the admin's later events verify. ZEB-339 already modernized it to
  the cert mechanism and ZEB-496 removed the dead OnceLock machinery. The ticket's "fold admin_bootstrap into the
  modern mechanism" framing is already satisfied; nothing to do.
- **`admin_identity_pub` is out of scope.** It is now inert on the verify path (`community_invite.rs:150-159`)
  but still required at encode time. Removing the encode requirement is a separate mechanical cleanup, not this
  security change.
- **No friend-path refactor.** We do not extract a shared verifier across the friend and community paths (that
  would touch the working friend path). If the ~6-site duplication is worth de-duplicating, it gets its own
  ticket. (This is the "minimal focused wiring" approach chosen over "wire + unify".)

## Design

### The verification function

A new pure function in `community_invite.rs`:

```rust
fn verify_inviter_enrollment(
    payload: &CommunityInvitePayload,
    now_secs: u64,
) -> Result<(), CommunityInviteVerifyError>
```

Logic:

1. **Short-circuit** for open communities: `if !payload.is_invite_only { return Ok(()) }` — no token/cert to verify.
2. **Require presence** of `inviter_enrollment` and `invite_token` (defensive — encode already guarantees both
   on invite-only payloads).
3. **Recover the inviter's device key from the bare cert.** A small community-side verifier mirroring the friend
   path's 4-step `verify_enrolled_device`:
   - `cert.verify(now_secs)` — checks the master signature and expiry against the cert's embedded `Master` key.
   - Reject non-`Master` issuers.
   - **Bind `cert.owner_id == invite_token.inviter`.**
   - Return the device Ed25519 verify key.
   Per the minimal-wiring approach we duplicate these ~4 lines with the community error type rather than reach
   into the friend module; `enrolled_key_from_cert` is not reusable here because it requires a
   `SignedMembershipEvent` wrapper, and `inviter_enrollment` is a bare cert.
4. **Verify the token signature** against the recovered key via the existing
   `verify_invite_token_sig_device_key` (`community_invite.rs:1767`).
5. **Hard-fail** on any mismatch — the redeem is rejected.

### Two settled decisions

- **Binding principal: `invite_token.inviter`** (not `admin_addr`). In v1 they are equal, so no behavior change;
  binding to `token.inviter` is already correct when non-admin inviters ship, so the control needs no rework then.
- **Expiry clock: wall-now** (mirrors the friend path's `verify_enrolled_device(..., wall_now_secs())`).
  Security-positive edge: if the inviter's enrollment has *expired* since the invite was minted, redeem fails —
  an expired or revoked enrollment should not keep authenticating tokens it signed.

### Call sites (both invite-only redeem paths)

- **Main redeem** — `redeem_invite_inner_with_overrides` (`lib.rs:22554`). Call
  `verify_inviter_enrollment(&payload, wall_now_secs())` **immediately after `decode_invite_url` (`~22599`)**,
  before the joiner reserves an HLC and mints its own bootstrap Join (`~22615`). Earliest fail-fast point: a
  forged invite is rejected before any local state work or network round-trip. (The existing
  `verify_admin_bootstrap` runs later at `~22864` because it also *inserts*; our check is pure, so it goes first.)
- **Orphan-dir pre-check** — `orphan_dir_adoption_eligible` (`lib.rs:22444`), alongside its existing
  `verify_admin_bootstrap` (`~22463`), for parity.

### Presence checks — kept

The two presence gates (`encode_invite_url:988`, `decode_invite_url:1061`) stay as cheap structural early-outs.
`decode_invite_url` remains pure and clock-free; the cryptographic verification is layered above it at the redeem
sites where `wall_now_secs()` is available. This is an addition, not a removal.

### Error taxonomy

`verify_inviter_enrollment` returns `CommunityInviteVerifyError` with distinct variants for attribution (the error
surfaces to the UI via the Tauri IPC boundary, and precise variants make the tests assert the right failure):

- `InviterEnrollmentCertInvalid` — `cert.verify` failed (bad master signature / expired) or non-`Master` issuer.
- `InviterEnrollmentOwnerMismatch` — `cert.owner_id != invite_token.inviter`.
- `InviterTokenSignatureInvalid` — token signature does not verify against the recovered device key.

Variant names will be aligned with the existing `CommunityInviteVerifyError` conventions during implementation.

## Edge cases

- **Open (non-invite-only) communities:** short-circuit at step 1 → `Ok`. No token/cert present.
- **Cold-cache joiner:** this is the target case — verification uses only the URL's cert + token, no synced log.
- **Expired inviter enrollment:** redeem fails (deliberate; see clock decision).
- **v1 admin-minted invite:** `inviter == admin`, cert is the admin's own enrollment; binding to `token.inviter`
  (== `admin_addr` in v1) passes. No regression to the live ZEB-367/369 flow.

## Testing

TDD. Unit tests are pure and fast, reusing the enrollment-cert / token-signing fixtures the existing
`verify_admin_bootstrap` and friend-path `verify_enrolled_device` tests already use.

1. **Happy path** — valid cert + token signed by the inviter's enrolled device key → `Ok`.
2. **Forged token sig** — token signed by a different key → `InviterTokenSignatureInvalid`.
3. **Owner mismatch** — `cert.owner_id != token.inviter` → `InviterEnrollmentOwnerMismatch`.
4. **Tampered cert** — bad master signature → `InviterEnrollmentCertInvalid`.
5. **Expired cert** — `expires_at < now_secs` → `InviterEnrollmentCertInvalid`.
6. **Non-Master issuer** (Quorum) → rejected (`InviterEnrollmentCertInvalid`).
7. **Non-invite-only payload** → `Ok` (short-circuit).
8. **v1 regression (integration)** — extend the existing two-engine community integration test to confirm a real
   admin-minted invite (inviter == admin) still redeems end-to-end with the new gate active.

## Files touched

- `src-tauri/src/community_invite.rs` — new `verify_inviter_enrollment` + the small bare-cert verifier helper +
  error variants + unit tests 1–7.
- `src-tauri/src/lib.rs` — two call sites (`~22463` orphan pre-check, `~22599` main redeem).
- the existing two-engine community integration test — case 8.

## Risks & mitigations

- **Regressing the live invite flow.** Mitigated by the v1 regression test (case 8) and by binding to
  `token.inviter` (== `admin_addr` in v1, so legitimate admin invites pass unchanged).
- **Over-strict expiry breaking valid redeems.** Accepted and intended: an expired enrollment should not
  authenticate. Invite tokens default to a 7-day expiry, so the realistic redeem window is short; an enrollment
  that expires inside it is an edge worth failing on.
- **Line-number drift.** All `lib.rs`/`community_invite.rs` anchors are current as of `9a7f8221`; the implementer
  re-locates by symbol (`decode_invite_url`, `verify_admin_bootstrap`, `verify_invite_token_sig_device_key`) if
  they have moved.

## Validation / success criteria

- All eight tests pass; the full gate (`cargo fmt`, `cargo clippy --all-targets --features test-fixtures`,
  `cargo nextest run --all-targets --features test-fixtures`) is green.
- A forged/tampered invite URL is rejected locally at the joiner (cases 2–6), and legitimate admin invites still
  redeem (case 8).
- No change to the friend path or to `admin_bootstrap`.
