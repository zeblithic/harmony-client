# ZEB-300 — Route admin-affecting Tier 2 auto-exec SetPower through AdminProposal

**Ticket:** ZEB-300 (parent ZEB-291 Tier 2 Conviction voting; related ZEB-250 admin quorum, ZEB-297 the narrow bug-fix that surfaced this).

**Goal:** When a multi-admin-quorum community's Tier 2 Conviction vote finalizes an *admin-affecting* SetPower outcome, the auto-exec must mint an `AdminProposal::SetPower` that admins countersign to quorum — instead of the current NoOp (`AutoExecOutcome::SkippedRequiresQuorum`). Close the gap where the vote is decided at the voting layer but never reaches the membership layer.

---

## 1. The gap

`verify_event` enforces **three** preconditions for a direct `SetPower` (`community_membership.rs:4535`):

1. `actor_power >= POWER_THRESHOLDS.set_power`
2. actor `status == Joined` (shared joined-membership block)
3. **`admin_quorum > 1 && admin_affecting → VerifyError::SetPowerRequiresQuorum`** (ZEB-250 §4.5) — a direct SetPower of an admin-affecting target is rejected; it must arrive as an `AdminProposal`.

`apply_auto_exec_set_power` (`community_membership.rs:5641`) guards (1) via `local_actor_can_mint_set_power` and (3) via `setpower_mint_admin_blocked_by_quorum`. When (3) fires it returns `AutoExecOutcome::SkippedRequiresQuorum` (`:5739-5747`) — correct as ZEB-297's narrow bug-fix (don't burn an HLC on a doomed event), but it leaves a **structural NoOp**: in a multi-admin-quorum community, a Tier 2 vote to promote-to-admin or demote-an-admin finalizes but the membership never changes on any replica.

**"Admin-affecting"** (ZEB-250 §4.3): for SetPower, `level == 100 || current_power[target] == 100`. (`POWER_THRESHOLDS.max == 100`.)

Neither the ZEB-250 nor ZEB-289 spec documents the Tier2 × admin-quorum interaction; this doc is the first prose spec of it.

---

## 2. Why the naïve fixes fail (the mechanism is forced)

The direct-SetPower auto-exec is a **safe race**: every admin replica's tick independently mints its own `SetPower` event, and HLC-last-write-wins collapses them because they all encode the identical `(target, level)` effect. Materialization is idempotent for the same effect, so N redundant events converge.

**`AdminProposal` is not safe the same way.** Each `mint_admin_proposal_*` stamps a fresh random `EventId` (`lib.rs:41035`). N admins minting independently produce N *distinct* proposals, each needing its own quorum; admins can split countersigns across instances and **quorum stalls forever**.

- **Rejected — deterministic shared `EventId`** (derive the id from `(community_id, target, level, poll_id)` so all admins "mint the same proposal"). The CRDT event store is a `BTreeMap<EventId, SignedMembershipEvent>` **keyed by `EventId`**. Two events with the same id but different `actor`/`signature` *collide* to a single map entry — only one survives, so `count_signers` sees one signer and quorum is never reached. The keyspace itself rules this out.

- **Rejected — elect a single proposer** (lowest admin `OwnerAddr` mints; others only countersign). Eliminates dangling proposals but reintroduces a **single-admin liveness dependency**: if that admin is offline, no proposal is ever minted and the decision stalls — defeating the purpose of M-of-N quorum (tolerating absent admins).

- **Chosen — any-admin-proposes + everyone countersigns the canonical proposal.** See §3.

---

## 3. Mechanism: any-admin-proposes + canonical (min-`EventId`) countersign

On each admin replica's tick, for a finalized admin-affecting SetPower under `admin_quorum > 1`, the auto-exec computes a **plan** from the current materialized state + event log and mints at most one event:

- **No live proposal exists for `(target, level)`** → mint `AdminProposal::SetPower { target, level }` (this admin becomes *a* proposer).
- **One or more live proposals exist** → pick the **canonical** one (smallest `EventId`, a total order every replica computes identically) and mint `AdminCountersign { target_event_id: canonical.id }` — unless this admin already signed the canonical (proposer or prior countersign), in which case **no-op**.
- **Target already at `level`** (the effect already applied — quorum was reached on an earlier tick) → **no-op**.

`count_signers` (`lib.rs:43020`) already counts distinct actors across `AdminProposal{id==pid}` ∪ `AdminCountersign{target==pid}`, and the materialize main-pass (`community_membership.rs:3373`) applies the effect at the event that tips the running signer count to `admin_quorum`. `apply_admin_proposal_effect`'s SetPower arm (`:5441`) sets `power_levels[target] = level` — identical to the direct arm. So reaching quorum on the canonical proposal produces exactly the intended membership change on every replica.

### Convergence properties

- **Staggered ticks (common case)** — replica A ticks → mints P_A → syncs → replica B ticks → sees P_A (sole ⇒ canonical) → countersigns → P_A has `{A, B}` = quorum 2 → **both replicas materialize target at `level` after one tick each** (satisfies AC #3).
- **Simultaneous ticks (edge)** — A and B both mint before syncing (P_A, P_B). On the next tick each deterministically countersigns `min(P_A.id, P_B.id)` = say P_A → P_A reaches quorum; **self-heals in one extra tick**. The non-canonical P_B is left **inert** — it never reaches quorum, has no effect, and expires after `ADMIN_PROPOSAL_EXPIRY_MS` (30 days). Inert dangling proposals are the accepted cost of coordinator-free availability.
- **Absent admin** — a proposal already synced to the log is enough; any other admin countersigns the canonical one. No dependence on the proposer staying online.
- **Single admin with `admin_quorum > 1`** (out of scope, §7) — mints a proposal that can never reach quorum; degrades to an inert proposal, no panic.

---

## 4. The pure planner

All decision logic lives in a **pure, `NodeState`-free** function (cheap to unit-test, no engine, no relink cost):

```rust
/// Decide what (if anything) this admin replica should mint to advance a
/// finalized admin-affecting Tier 2 SetPower toward AdminProposal quorum.
pub(crate) enum AdminProposalPlan {
    MintProposal,
    Countersign(EventId),   // EventId = [u8; 16]
    Noop,
}

pub(crate) fn plan_admin_proposal_auto_exec(
    mat: &MaterializedMembership,
    events: &BTreeMap<EventId, SignedMembershipEvent>,
    target: OwnerAddr,
    level: u8,
    self_owner: OwnerAddr,
    now_ms: u64,
) -> AdminProposalPlan;
```

Logic:
1. If `mat.power_levels.get(&target) == Some(&level)` → `Noop` (effect already applied).
2. Collect **live** candidates: `AdminProposal { proposal_kind: SetPower { target: t, level: l } }` with `t == target && l == level` and `now_ms.saturating_sub(e.at.wall_ms) <= ADMIN_PROPOSAL_EXPIRY_MS`.
3. If none → `MintProposal`.
4. `canonical = candidates.min_by_key(|e| e.id)`.
5. If this admin already signed `canonical.id` (an `AdminProposal` with that id and `actor == self_owner`, or an `AdminCountersign` targeting it) → `Noop`.
6. Else → `Countersign(canonical.id)`.

The already-signed scan reuses the exact predicate from `countersign_admin_proposal_impl` (`lib.rs:43172-43182`).

---

## 5. Code changes

**`src-tauri/src/community_membership.rs`**
- Add `AdminProposalPlan` enum + `plan_admin_proposal_auto_exec` (pure, above).
- Add `apply_auto_exec_admin_proposal_set_power(node_state, community_id, target_pubkey, level) -> Result<AutoExecOutcome, String>`: read `(mat, events)` under the engine state lock (the state guard already exposes `.events`, per `count_signers`' usage), derive `now_ms` from the reserved HLC, compute the plan, and:
  - `MintProposal` → `mint_admin_proposal_set_power_event(...)` → `insert_local_event` → `RoutedProposalMinted`.
  - `Countersign(pid)` → `mint_admin_countersign_event(...)` → `insert_local_event` → `RoutedProposalCountersigned`.
  - `Noop` → `RoutedProposalPending`.
- In `apply_auto_exec_set_power`, replace the `blocked_by_quorum` NoOp branch (`:5739-5747`) with a call to the new helper. The direct-SetPower path (below) is **untouched**.
- **Signing key (load-bearing):** the AdminProposal/AdminCountersign events MUST be signed with `outbox.community_signing_key` — matching the manual `set_power_level` proposal path (`lib.rs:41863`) — **not** the `outbox.signing_key` used by the direct-SetPower arm. Signing with the wrong key makes the routed events fail `verify_event` silently.
- **`AutoExecOutcome`**: retire `SkippedRequiresQuorum`; add `RoutedProposalMinted`, `RoutedProposalCountersigned`, `RoutedProposalPending`.
- **DRY**: extract the triplicated admin-affecting test into `fn is_admin_affecting_set_power(mat, target, level) -> bool` and call it from `setpower_mint_admin_blocked_by_quorum`, the `set_power_level` IPC, and (optionally) the `verify_event` SetPower arm. Behavior-preserving refactor.

**`src-tauri/src/community_voting_tick.rs`**
- `TickStats`: retire `tier2_auto_execs_skipped_requires_quorum`; add `tier2_auto_execs_routed_proposal_minted`, `_routed_proposal_countersigned`, `_routed_proposal_pending`.
- Tick dispatch (`:307-347`): update the `match` on `AutoExecOutcome` to bump the new stats. The `AutoExecSetPowerFn` closure signature `(SpaceId, OwnerAddr, u32)` is **unchanged** — only the returned enum grows, so no tick-signature churn.

**`src-tauri/src/lib.rs`**
- No signature changes to the `apply_auto_exec_set_power` wiring in `start_node`. If the `set_power_level` IPC adopts the shared `is_admin_affecting_set_power` helper, update that call site.

---

## 6. Testing (planner + materialize; per approved depth)

**Pure-planner unit tests** (in `community_membership.rs`, `mod auto_exec_tests` or a new sibling) — exhaustive:
- already-at-power → `Noop`
- no candidate → `MintProposal`
- one live candidate, not yet signed by me → `Countersign(that.id)`
- one live candidate I proposed → `Noop`
- one live candidate I already countersigned → `Noop`
- two candidates → `Countersign(min EventId)` (canonical selection)
- only-expired candidate(s) → `MintProposal` (fresh window)

**Materialize convergence test** (reuse the hand-built-events pattern in `tests/community_misc/community_admin_quorum_integration.rs`, incl. its `bootstrap_two_admins_raise_quorum` helper): two admins, `admin_quorum = 2`, promote a non-admin to 100 → construct `AdminProposal` (admin A) + `AdminCountersign` (admin B) → `materialize` → assert `power_levels[target] == 100` on the merged log (both replicas see the same log ⇒ same materialization). Covers AC #3 without a heavyweight dual-live-engine + tick fixture (which the recon confirmed does not yet exist).

**Update the ZEB-297 pin**: `community_voting_tick_tier2_auto_exec_set_power_skipped_when_quorum_blocks` (`community_voting_tick.rs:935`) — assert the new routed outcome / stat instead of the retired `SkippedRequiresQuorum`.

**Signing-path test**: extend the existing `auto_exec_set_power_signing_path_produces_verifiable_signature` pattern to assert a routed `AdminProposal` verifies under `verify_event` (guards the community_signing_key requirement from §5).

---

## 7. Out of scope (per ticket)

- Communities with `admin_quorum > 1` but only **one** admin (cannot satisfy quorum) — a community-creation-time guard, separate concern. This design degrades gracefully (inert proposal).
- AdminProposal expiry / GC interaction with Tier 2's archive sweep.
- Target kicked mid-flight between finalize and quorum — already covered by ZEB-289 §10 (`PollExecutionFailed`, manual enact, no retry/panic); unchanged here.
- Tier 2 auto-exec of non-SetPower actions (ZEB-289 §5: v1 auto-exec scope is SetPower only).

---

## 8. Spec amendments

- New: this doc.
- `docs/specs/2026-05-16-zeb-289-voting-polling-design.md` — add a short note under §5 (auto-exec actions) / §10 (auto-exec invalidation): in `admin_quorum > 1` communities, an admin-affecting SetPower auto-exec routes through AdminProposal (any-admin-proposes + canonical countersign), reaching quorum across ticks.

---

## 9. Acceptance-criteria mapping

| AC | Satisfied by |
|----|-------------|
| 1. Mint `AdminProposal::SetPower` when the quorum-blocked branch fires | §5 `apply_auto_exec_admin_proposal_set_power` `MintProposal` arm |
| 2. Tick handles an already-pending proposal it can countersign, idempotent (each admin signs ≤ once) | §4 planner `Countersign`/`Noop` (already-signed scan); runs every tick |
| 3. Two admins, `admin_quorum = 2`, promote non-admin to 100 → both materialize target at 100 | §3 convergence + §6 materialize test |
| 4. `SkippedRequiresQuorum` retires | §5 outcome enum change |
| 5. `tier2_auto_execs_skipped_requires_quorum` → 0 (retired); observability updated | §5 stats change + §8 docs |
