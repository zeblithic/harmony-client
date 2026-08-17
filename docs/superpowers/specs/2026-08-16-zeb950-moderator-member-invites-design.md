# ZEB-950 — moderator/member invites (invites from any sufficiently-powered member, not just the admin)

**Ticket:** [ZEB-950](https://linear.app/zeblith/issue/ZEB-950) — allow invites to come from any community moderator, not just admins.

**Goal (one sentence):** Let any community member with power ≥ the community's `invite` threshold generate a working community invite — replacing the hardcoded "inviter must be the single `admin_addr`" mint guard with a power-threshold check, so growth can be socially delegated to trusted members without a direct admin↔invitee association.

## Approved decisions (2026-08-16, with Jake)

1. **Default = any member (threshold 0).** Honor the existing per-community `power_thresholds.invite`, which defaults to `0`. Every Joined member can invite out of the box; a community can raise the bar via the already-shipped `ChangeThresholds` governance event (ZEB-251).
2. **Scope = full (mint + field reframe + UI).** One coherent change, not a narrow hardening.
3. **Enforcement rides the existing witness model, not a new inviter-power rule** (see §"The load-bearing realization").

## What exists today (grounded 2026-08-16)

### Power / governance model

- Governance is a flat numeric `u8` power level per owner, **no named roles**. `MaterializedMembership.power_levels: BTreeMap<OwnerAddr, u8>` (`community_membership.rs:1831`). "Admin" = power 100; "moderator" = the `kick`-tier band (power ≥ 50); "member" below. "Moderator" is a UI label from `powerToRole(myPower)`, not a type.
- `PowerThresholds { invite: 0, kick: 50, set_power: 100, max: 100 }` (`community_membership.rs:5592`), per-community-customizable via `ProposalKind::ChangeThresholds` (ZEB-251, shipped) and materialized onto `MaterializedMembership.power_thresholds`. `verify_event` reads the **per-community** value, at-event-HLC.
- Power is granted only by a signed `SetPower { target, level }` event (`community_membership.rs:152`), quorum-gated when admin-affecting (level 100). The community creator is seeded to power 100 at bootstrap (`materialize`, `:2688`).

### The admin model — single trust anchor, plural authority

- `Space.admin_addr: Option<OwnerAddr>` (`owner_state_types.rs:1967`) is a **single, creation-pinned, immutable** field — the owner-state CRDT rejects any change to it (`owner_state_crdt.rs:361`). It is the community's **cryptographic trust anchor** (what membership verification roots on via `verify_admin_bootstrap`), not the authority set.
- *Governance authority* is separate and power-based: multiple members can be raised to power 100 (effective admins), and `admin_quorum` (default 1) supports M-of-N admin-affecting actions. So "admins, plural" and "moderators" already exist as power tiers; the singular `admin_addr` does not block either.

### Mint side — where the admin lock actually lives

- Invite-only generation is gated by `invite_only_generation_guard(self_owner, admin)` (`lib.rs:36596`), a hardcoded **identity equality** to `admin_addr`. Its own doc comment (`lib.rs:36583`, call site `:36235`) says to replace it with a `power ≥ invite_threshold` check "when non-admin invite ships."
- **Open** invites (`generate_invite_impl` else-branch, `lib.rs:36380`) have **no backend guard at all** — any member holding the epoch key can already mint one; only the UI hides the button. Open invites carry no inviter identity (the link *is* the gate), so there is nothing to authority-check on the receive side for them.
- `generate_invite` → `generate_invite_impl` (`lib.rs:36101`). For invite-only it mints an `InviteToken` with `inviter = self_owner` (`:36358`), extracts the admin's self-Join `admin_bootstrap` from the synced log, and sets `admin_identity_pub = self_private_identity.identity.to_public_bytes()` (`:36370`) — i.e. **the generator's own identity**, which for v1 (self == admin) happens to be the admin's.

### Receive side — the witness model (ZEB-911)

- Redemption finalizes when **any Joined member witnesses/countersigns** it (ZEB-911, #654). In `handle_unicast` step 5 (`community_invite.rs:2721`), the accepting witness checks: its **own** `power ≥ invite_threshold` (`SelfPowerInsufficient`), that `invite_token.inviter` is a resolvable member with non-empty `enrolled_device_keys`, and that the token signature verifies against those inviter keys (`verify_invite_token_sig_device_key`). It does **not** check the inviter's power — the inviter's role is "a member who vouched."
- Consensus enforcement lives on the countersign: `verify_event` requires the **countersigner (witness)** to have `power ≥ prior_state.power_thresholds.invite` at the event's HLC (`community_membership.rs:4316`). Every member folds and re-verifies this identically.
- The joiner's `verify_admin_bootstrap` (`community_invite.rs:2019`) roots trust in the community's single `admin_addr` via the admin's self-Join — independent of who the inviter is.
- `admin_identity_pub` is consumed at redeem to verify the token signature (`verify_invite_token_signature`, `community_invite.rs:2507`, keys off `admin_identity_pub[32..]`) and is required present for case-A redemption (`lib.rs:64691`). It is really "the inviter's identity," mislabeled because v1 inviter == admin.

## The load-bearing realization

Because ZEB-911 already put admission authority at the **finalization/witness** step (consensus checks the *witness's* power ≥ `invite_threshold`), the receive side needs **no new inviter-power rule**. The only thing preventing a non-admin from inviting is the **mint-side identity guard**. At the chosen default (`invite_threshold = 0`):

- Any member can generate an invite (once the guard is relaxed).
- Any member can witness/finalize it (power 0 ≥ 0) — the moderator can even witness their own invite, so the admin is never in the loop.

For a community that *raises* `invite_threshold` (e.g. to 50), the witness model already yields a coherent policy: a lower-power member may still *create* an invite, but it only **finalizes when a member with power ≥ threshold witnesses it** — i.e. moderators retain admission control by being the required finalizers. This is the existing, tested semantics; we reuse it rather than bolt a parallel inviter-power gate beside it.

## Architecture — three focused changes

### 1. Mint: honor the threshold instead of hardcoding admin

- Replace `invite_only_generation_guard(self_owner, admin)` with a power check: materialize the caller's power from the community log (`materialize_with_now(&events, admin, Some(wall_now_ms)).power_levels.get(self_owner)`, default 0) and require it `≥ power_thresholds.invite`. At threshold 0, every Joined member passes. Return a clear error (`InsufficientPowerToInvite { power, threshold }`) otherwise.
- Also require the caller be a **Joined** member (not Left/Banned) — a materialized-status check, so a departed member cannot mint.
- The token's `inviter` stays `self_owner`; `admin_bootstrap` stays the admin's self-Join from the log (unchanged trust anchor).
- Rename the guard to `invite_generation_power_guard(self_power, threshold)` (pure, unit-testable) and keep the call site's `materialize` read local.

### 2. Field reframe: `admin_identity_pub` → `inviter_identity_pub`

- The field is semantically the **inviter's** identity, used to verify the invite-token signature. Rename the Rust field `admin_identity_pub` → `inviter_identity_pub` and its accessors; **keep the `ap` serde wire key** and the bstr (de)serializers so existing invites and fixtures stay byte-identical. Update `MissingAdminIdentityPub` → `MissingInviterIdentityPub` (error text only; keep any structured code stable if one exists).
- `generate_invite_impl` already sets it from the generator's own identity — correct for a moderator inviter with no change. Verify the receive-side token-verify paths resolve the **inviter's** keys (they already do in `handle_unicast` via membership; confirm the `verify_invite_token_signature`/`PendingJoin` path does not assume admin).
- This is a **local rename**, not a wire-format change: no new tag, no migration.

### 3. Frontend: verify the (already-present) threshold gate + reachability

- The invite section **already** gates on `{#if myPower >= thresholds.invite}` (`CommunitySettingsPanel.svelte:614`), not on the admin tier — so at threshold 0 every member already sees the invite control. The production change here is small-to-none: verify a non-admin Joined member can actually *reach* the panel/section (no higher-level admin-only gate hides it), and add regression coverage. If a higher-level gate does hide it, relax that entry to `myPower >= thresholds.invite`.
- `powerToRole` and role labels are unaffected.

## The one implementation detail pinned by TDD

The two token-verification code paths must both accept a **non-admin** inviter:

1. `handle_unicast` (witness) — resolves the inviter's enrolled device keys from materialized membership and verifies via `verify_invite_token_sig_device_key`. Believed already inviter-agnostic.
2. `verify_invite_token_signature(token, inviter_identity_pub)` (`community_invite.rs:2507`) — verifies against the identity ed25519 in `inviter_identity_pub[32..]`. Must be consistent with how `generate_invite_impl` signs the token (device key vs identity key). The plan's first implementation task writes a **failing test** — moderator generates an invite-only invite, a witness accepts, a fresh joiner redeems — to reveal exactly which path (if any) rejects a non-admin inviter today, before any production change.

## Test plan

- **Rust — mint guard:** a power-0 member passes the invite-generation guard at threshold 0; a Left/Banned member is rejected (`NotJoined`); with `invite_threshold` raised to 60, a power-50 member is rejected (`InsufficientPowerToInvite`), a power-60 member passes. The pure `invite_generation_power_guard` is unit-tested in isolation.
- **Rust — end-to-end (the TDD driver):** a moderator (power 50, not admin) mints an invite-only invite whose `admin_bootstrap` is the admin's log self-Join; a witness (`handle_unicast`) accepts it; a fresh empty-log joiner verifies the admin bootstrap + inviter token and redeems — asserting the join lands and materializes, with the admin never signing anything in the redemption.
- **Rust — witness-model authority at a raised threshold:** with `invite_threshold = 60`, a countersign from a power-50 witness is rejected by `verify_event` (existing `CounterSigPowerInsufficient`), a power-60 witness's countersign is accepted — pinning that admission control tracks the threshold with no new rule.
- **Rust — backward-compat:** an existing threshold-0 community's invite encode/decode is byte-identical after the `admin_identity_pub` → `inviter_identity_pub` rename (wire key `ap` unchanged); a v1 admin-generated invite still redeems.
- **Frontend (vitest):** `canInvite` derives from `myPower >= thresholds.invite`; a power-0 member sees the invite affordance at threshold 0; a power-10 member does not when a community sets `invite: 50`; an admin always does.

## Out of scope (explicitly deferred)

- Adding a *separate* consensus rule that the **inviter** (distinct from the witness) held power ≥ threshold. The witness model already governs admission; a dual inviter+witness gate is a policy change, not required for this feature, and would be its own ticket if ever wanted.
- Admin transfer / multiple `Space.admin_addr` (the trust anchor stays singular and pinned).
- Tightening inviter *status* checks in `handle_unicast` beyond what this change adds at the mint (any pre-existing "Left member's keys still resolvable" behavior is unchanged and tracked separately if it matters).
- Changing the `invite` default away from 0, or per-role/per-channel invite thresholds (community-wide only, already covered by ZEB-251).

## Global constraints

- Rust gates (from `src-tauri/`): `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures`. Iterative dev may scope with `-p harmony-app --lib` / `scripts/test-select`; the final pre-PR sweep is the full command.
- Frontend gates (repo root): `npx tsc --noEmit`; `npx vitest run`.
- **Wire compatibility:** no new CBOR tags, no field additions/removals — the `admin_identity_pub` → `inviter_identity_pub` change is a Rust-symbol rename only, with the `ap` serde key preserved. Existing invites and wire-format fixtures must stay byte-identical.
- IPC naming: Rust `snake_case` params ↔ JS `camelCase`.
- **Second-order correctness:** the threshold must be read at-event-HLC from `prior_state` (never a mutable "current" snapshot); the mint guard is UX/advisory only (client-controlled) — the *witness countersign power check* is the real, consensus-enforced boundary and must remain intact; the rename must not perturb any wire byte; and the end-to-end test must prove the **admin is not required** to sign anything during a moderator-originated redemption.
