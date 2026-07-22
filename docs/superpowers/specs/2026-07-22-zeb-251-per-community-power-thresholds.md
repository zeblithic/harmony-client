# ZEB-251 — per-community power-threshold customization

**Ticket:** [ZEB-251](https://linear.app/zeblith/issue/ZEB-251) — make the hardcoded per-action power thresholds (invite/kick/set_power) configurable per community, overriding the Sub-C v1 defaults.

**Goal (one sentence):** Let a community's admins raise or lower the power required to `invite`/`kick`/`set_power` via a signed, quorum-gated governance event that all members converge on — replacing the single global `POWER_THRESHOLDS` constant read inside `verify_event` with a per-community, materialized value that defaults to today's constants for every existing community.

## What exists today (grounded 2026-07-22)

- `PowerThresholds { invite, kick, set_power, max }` and `pub const POWER_THRESHOLDS = { invite: 0, kick: 50, set_power: 100, max: 100 }` — `src-tauri/src/community_membership.rs:5259`. The doc comment there already names ZEB-251 as the deferred customization.
- `verify_event(event, prior_state, ctx)` — `community_membership.rs:3894` — the single authorization entry point. It reads the **global const** directly at every gate: invite `:4492`, kick `:4506` (+ `KickTargetPowerNotLower` `:4519`), set_power `:4536`/`:4554`, range gate `:4539` (`level > POWER_THRESHOLDS.max`), invite-only join countersigner `:4010`, channel-config mod-tier = `.kick` (`:4584`/`:4608`/`:4643`), admin helpers `:3110`/`:3217`. Placeholder scaffolding at `:3888` (`#[allow(clippy::absurd_extreme_comparisons)]`) explicitly awaits this ticket.
- The **member-agreed governance-config precedent**: `admin_quorum` on `MaterializedMembership` (`community_membership.rs:1804`, `default_admin_quorum`), changed only via the signed `AdminProposal{ ProposalKind::ChangeQuorum{ new_quorum } }` event (`ProposalKind` `:50`, `ChangeQuorum` `:71`; verified in `verify_event` at `:4212`; applied to state at `:5471`). Two-phase where quorum > 1: `AdminProposal` + `AdminCountersign` (`:315`/`:326`).
- Frontend mirror const: `src/lib/types.ts:486` `POWER_THRESHOLDS = { invite:0, kick:50, setPower:100, max:100 }` (its "mirrors …:1108" comment is stale — backend const is at `:5259`).
- Governance IPC: getter `get_community_governance` → `CommunityGovernanceDto { adminQuorum }` (`src-tauri/src/lib.rs:44346`), setter `propose_change_quorum` (`lib.rs:45050`). Service wrapper `src/lib/community-service.ts:618`.
- Governance UI: `src/lib/components/CommunitySettingsPanel.svelte` "Admin governance" section `:571`, with `ChangeQuorumDialog.svelte`, `PendingAdminProposalsPanel.svelte`, and the `src/lib/components/governance/` primitives (ZEB-648).

## The load-bearing design decision — why *not* a `Space` field

The ticket proposes "a per-community `PowerThresholds` setting on `Space`." **That home is incorrect.** `Space` is *owner-state* CRDT — replicated only across a single owner's own devices (`owner_state_crdt.rs`, LWW-merged), never across community members. But thresholds gate `verify_event`, which **every member runs independently** to decide whether a `Kick`/`Invite`/`SetPower` event is valid. If members held divergent threshold values they would disagree on event validity → membership-CRDT split-brain. Thresholds must converge **identically and deterministically** across all members.

The codebase already solves exactly this for `admin_quorum`: it is a materialized field changed only through a signed, admin-gated event, so every member folds the same events to the same value. **This design mirrors that path.** A direct consequence we get for free: the threshold in effect when verifying any event is the value materialized **as-of that event's own HLC position** (`verify_event` receives `prior_state` = the fold up to but excluding the event). This is the correct at-event-HLC semantics ([[at-event-HLC membership verify]]); **no anti-backdating guard is added** (adding one would break backfill).

## Architecture — a mirror of `ChangeQuorum`

1. **State.** Add `thresholds: PowerThresholds` to `MaterializedMembership`, initialized to `POWER_THRESHOLDS` in `default_*`/materialize seed. Every community with no threshold-change event in its log therefore materializes today's exact values → transparent backward-compat, no migration, no wire-format change for existing communities.
2. **Event.** Add `ProposalKind::ChangeThresholds { new_thresholds: PowerThresholds }` (CBOR tag chosen to not collide with existing `ProposalKind` tags). It travels inside the existing `AdminProposal`/`AdminCountersign` envelope — **no new top-level `MembershipEventKind`** — so it inherits the entire quorum/countersign/pending-proposal machinery.
3. **Authorization (quorum-gated).** Verified by the same AP1–AP5 admin-proposal gates as `ChangeQuorum`, requiring the community's **own `admin_quorum`** countersign count. Consequence: a `quorum == 1` community gets effectively single-admin threshold changes automatically; a `quorum == K` community requires K admins — thresholds are exactly as protected as the community already chose for quorum changes, with zero new authorization concepts.
4. **Validity invariant (checked at verify time, before apply).** A proposed `new_thresholds` is rejected unless `0 ≤ invite ≤ kick ≤ set_power ≤ max` **and** `max == 100` (the ceiling is immovable). This makes lock-out configs unrepresentable (you can never raise the admin tier out of reach, never invert the gate ordering). A new `VerifyError` variant carries the rejection.
5. **Read path.** Every `POWER_THRESHOLDS.<field>` read inside `verify_event` becomes `prior_state.thresholds.<field>`. The `#[allow(clippy::absurd_extreme_comparisons)]` scaffold at `:3888` is removed once `invite` can be `> 0` (the comparison is no longer trivially-false).
6. **Apply.** Materialize `ChangeThresholds` into `m.thresholds` alongside the existing `ChangeQuorum → m.admin_quorum` arm (`:5471`).

### Customizable set (decided)

`invite`, `kick`, `set_power` are editable; `max` is fixed at 100. Covers the ticket's motivating examples ("invite requires 25", "kick requires 75") and any admin-tier retuning, while the fixed ceiling + ordering invariant remove every brick vector.

### Change-only (decided)

No create-time threshold parameter. Communities always start at the defaults; an admin sets custom thresholds by proposing a change (immediately after creation if desired). This keeps a **single** authorization path (the event) and leaves the community-create surface untouched. The ticket's "at creation OR later" acceptance is satisfied functionally.

## Surfaces (full stack)

1. **Rust CRDT** (`community_membership.rs`): the `ProposalKind` variant, `MaterializedMembership.thresholds` + seed, the verify-time invariant + `VerifyError` variant, the `verify_event` read-path swap (+ scaffold removal), the materialize apply arm.
2. **IPC** (`src-tauri/src/lib.rs`): extend `CommunityGovernanceDto` (`:44346`) with the current `thresholds` (camelCase `{ invite, kick, setPower }`, plus `max` for display); add `propose_change_thresholds(community_id, invite, kick, set_power)` mirroring `propose_change_quorum` (`:45050`) — signs an `AdminProposal{ChangeThresholds}` through the community engine/outbox (same path as quorum). Register in `generate_handler!`; if a curated-verb assertion gates registration (as in `api/rpc.rs` for headless verbs), add the name.
3. **Service layer** (`src/lib/community-service.ts`): the `getCommunityGovernance()` DTO type gains `thresholds`; add `proposeChangeThresholds(...)` wrapping `invoke('propose_change_thresholds', …)`.
4. **Frontend defaults** (`src/lib/types.ts`): keep `POWER_THRESHOLDS` as the **default/fallback**; components that currently gate on the const (`CommunitySettingsPanel.svelte:187/194/196/359/365`, etc.) consume the per-community `thresholds` from the governance DTO, falling back to the const when absent.
5. **UI** (`src/lib/components/`): a `ChangeThresholdsDialog.svelte` sibling to `ChangeQuorumDialog.svelte`, launched from the CommunitySettingsPanel "Admin governance" section (`:571`), gated by `canAdmin` (`myPower ≥ thresholds.set_power`). Three number inputs (invite/kick/set_power) with live client-side validation of the same ordering invariant (server re-checks authoritatively). Pending threshold-change proposals surface through the existing `PendingAdminProposalsPanel` if it renders `ProposalKind` generically; if it switches on kind, add a `ChangeThresholds` arm (to confirm in the plan).

## Test plan

- **Rust unit (`community_membership.rs` tests):** default thresholds materialize to `POWER_THRESHOLDS` for a community with no change event (backward-compat); a valid `ChangeThresholds` proposal at quorum materializes and subsequently governs verification (e.g. after raising `invite` to 25, a power-10 member's `Invite` now fails `InsufficientPower`; before the change it succeeded); an invalid `new_thresholds` (ordering violation, or `max != 100`) is rejected at verify with the new `VerifyError`; a `ChangeThresholds` below quorum stays pending and does **not** yet govern; at-event-HLC — an `Invite` event ordered *before* a threshold-raise still verifies against the old (lower) threshold, one ordered *after* against the new.
- **Rust IPC:** `get_community_governance` returns the live thresholds; `propose_change_thresholds` emits a well-formed proposal (and is rejected client-cheaply on an obviously-invalid set, though the CRDT is the authority).
- **Frontend (vitest):** the DTO type round-trips `thresholds`; `ChangeThresholdsDialog` disables submit on an ordering-invalid entry; gates in `CommunitySettingsPanel` consume per-community values (e.g. a community with `invite: 25` shows the invite affordance disabled for a power-10 viewer).

## Out of scope (explicitly deferred)

- Decoupling **channel-config** authority (Create/Modify/Delete) from the mod-tier `kick` threshold — those stay coupled to `kick` and therefore move *with* a `kick` change; a separate channel-config threshold is a later ticket.
- Customizing `max` / the 0–100 power scale itself.
- A create-time threshold parameter (change-only, see above).
- Per-**role** or per-**channel** thresholds (community-wide only).

## Global constraints

- Rust gates (from `src-tauri/`): `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures`.
- Frontend gates (repo root): `npx tsc --noEmit`; `npx vitest run`.
- CBOR wire compatibility: the new `ProposalKind` variant must use a fresh tag and must not perturb existing variants' encodings (existing communities carry no `ChangeThresholds` events, so their logs are byte-identical).
- `MaterializedMembership` is a derived fold of the event log; adding `thresholds` is safe **iff** it is never persisted/snapshotted. If a snapshot cache exists (as voting does via `poll_restore`), the new field needs `#[serde(default)]` seeding to `POWER_THRESHOLDS` — confirm the persistence posture in the plan before relying on pure recomputation.
- IPC naming: Rust `snake_case` params ↔ JS `camelCase` (`set_power` ↔ `setPower`).
- **Second-order correctness:** thresholds must be read from `prior_state` (at-event-HLC), never from a mutable "current" snapshot; the change event must be quorum-gated exactly like `ChangeQuorum` (no weaker path); the validity invariant must be enforced at **verify** (so every member rejects an invalid change identically), not only in the UI; and `max` must remain 100 so the admin tier can never be lifted out of reach.
