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
- **Simultaneous ticks (edge)** — A and B both mint before syncing (P_A, P_B). On a **later** tick each deterministically countersigns `min(P_A.id, P_B.id)` = say P_A → P_A reaches quorum; **self-heals within the auto-exec retry window** (see below). The non-canonical P_B is left **inert** — it never reaches quorum, has no effect, and expires after `ADMIN_PROPOSAL_EXPIRY_MS` (30 days). Inert dangling proposals are the accepted cost of coordinator-free availability.
- **Absent admin** — a proposal already synced to the log is enough; any other admin countersigns the canonical one. No dependence on the proposer staying online.
- **Single admin with `admin_quorum > 1`** (out of scope, §7) — mints a proposal that can never reach quorum; degrades to an inert proposal, no panic.

### Re-dispatch in the tick (ZEB-300 converge R1, refined R2)

The "later tick" above only works if `run_voting_tick` **revisits Finalized polls** — the original tick dispatched auto-exec ONLY on the single tick a poll transitioned `ThresholdReached → Finalized`, so in the simultaneous-finalize case A and B would each mint P_A / P_B on their own finalize tick and **neither would ever countersign the other's**: quorum stalls forever (Qodo). The fix splits Pass 3 into:

- **Pass 3a** — the finalize transition (unchanged): stamp `meta.finalized_at_ms`, emit `voting-proposal-finalized`.
- **Pass 3b** — re-dispatch auto-exec for **every** poll that is still `Lifecycle::Finalized` and carries a SetPower auto-exec. The just-finalized poll is included (same tick), so single-tick finalize-then-dispatch still holds.

**R2 (Greptile):** the R1 version gated Pass 3b on an arbitrary `AUTO_EXEC_RETRY_WINDOW_MS` (1h) since `finalized_at_ms`. That cutoff was shorter than the poll's actionable lifetime — a poll stays `Finalized` until the 24h archive sweep (`ARCHIVE_SWEEP_INTERVAL_MS`) flips it to `Archived` — so a canonical proposal that synced to a needed signer after 1h would never be auto-countersigned. R2 **removes the fixed window**: re-dispatch continues for as long as the poll is `Finalized`, bounded naturally by the archive sweep (~24h). Each replica's re-dispatch is still anchored to its own local finalize (a later-arriving admin's replica finalizes later and re-dispatches from then), and because the AdminProposal persists `ADMIN_PROPOSAL_EXPIRY_MS` (30 days), quorum accumulates across admins. For admin absences longer than the ~24h Finalized lifetime, recovery is via the manual `countersign_admin_proposal` IPC (the proposal is still live).

**Idempotency stops re-mint** once the effect lands. Both the direct-SetPower guard (`apply_auto_exec_set_power`) and the AdminProposal-routed planner (step 1) check `power_levels[target] == level` and return the terminal `AutoExecOutcome::AlreadyApplied` — so re-dispatch is a cheap no-op on every replica the instant the SetPower materializes.

### Production wiring (ZEB-300 converge R2, Task 20.1)

The voting tick's `auto_exec_set_power` callback was a **stub** in production (`lib.rs`, returned `SkippedNotAdmin` unconditionally — the deliberate ZEB-291 Phase-2 "Task 20.1" deferral). R2 **wires it**: the `'static` tick closure captures the typed Tauri `AppHandle` (`wry_handle`) and, at call time, fetches the managed `Mutex<NodeState>` via `app.state().inner()`, dispatching through `apply_auto_exec_set_power`. To make this typecheck the helper's parameter was relaxed `&Arc<Mutex<NodeState>>` → `&Mutex<NodeState>` (it only locks — never Arc-clones; existing `&Arc<…>` call sites still compile by Deref coercion). Cross-peer voting sync ("Task 19.1") was already live — the voting-log Zenoh adapter is spawned via `ensure_voting_engine_for` and IPCs publish through `engine.publish_event`; the resulting SetPower/AdminProposal membership events ride the already-wired community state-root log. The headless serve/test path (`wry_handle == None`) keeps the `SkippedNotAdmin` stub (a follow-up: expose an owned `NodeState` handle to the serve boot so agent-testing can exercise auto-exec).

---

## 4. The pure planner

All decision logic lives in a **pure, `NodeState`-free** function (cheap to unit-test, no engine, no relink cost):

```rust
/// Decide what (if anything) this admin replica should mint to advance a
/// finalized admin-affecting Tier 2 SetPower toward AdminProposal quorum.
pub(crate) enum AdminProposalPlan {
    MintProposal,
    Countersign(EventId),   // EventId = [u8; 16]
    AlreadyApplied,         // effect already in materialized state → terminal
    Pending,                // I already signed the canonical; awaiting quorum
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
1. If `mat.power_levels.get(&target) == Some(&level)` → `AlreadyApplied` (effect already applied; **terminal** — the tick's re-dispatch stops here, so nothing re-mints once the SetPower lands on this replica).
2. Collect **live** candidates: `AdminProposal { proposal_kind: SetPower { target: t, level: l } }` with `t == target && l == level` and `now_ms.saturating_sub(e.at.wall_ms) <= ADMIN_PROPOSAL_EXPIRY_MS`.
3. If none → `MintProposal`.
4. `canonical = candidates.min_by_key(|e| e.id)`.
5. If this admin already signed `canonical.id` (an `AdminProposal` with that id and `actor == self_owner`, or an `AdminCountersign` targeting it) → `Pending` (nothing to mint this tick, but **not** terminal — the effect has not landed yet, so re-dispatch keeps polling until a peer supplies the final quorum signature).
6. Else → `Countersign(canonical.id)`.

The already-signed scan reuses the exact predicate from `countersign_admin_proposal_impl` (`lib.rs:43172-43182`).

The wrapper maps the plan to `AutoExecOutcome`: `MintProposal → RoutedProposalMinted`, `Countersign → RoutedProposalCountersigned`, `AlreadyApplied → AlreadyApplied`, `Pending → RoutedProposalPending`. Splitting the old `Noop` into `AlreadyApplied` (terminal) and `Pending` (in-flight) is what lets Pass 3b's bounded re-dispatch distinguish "stop, the effect landed" from "keep polling, quorum still accruing".

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
- **`AutoExecOutcome`**: retire `SkippedRequiresQuorum`; add `RoutedProposalMinted`, `RoutedProposalCountersigned`, `RoutedProposalPending`, and (ZEB-300 converge R1) the terminal `AlreadyApplied` (effect already in materialized state — nothing minted; the re-dispatch stop condition, returned by both the direct-SetPower and AdminProposal-routed paths).
- **DRY**: extract the triplicated admin-affecting test into `fn is_admin_affecting_set_power(mat, target, level) -> bool` and call it from `setpower_mint_admin_blocked_by_quorum`, the `set_power_level` IPC, and (optionally) the `verify_event` SetPower arm. Behavior-preserving refactor.

**`src-tauri/src/community_voting_tick.rs`**
- `TickStats`: retire `tier2_auto_execs_skipped_requires_quorum`; add `tier2_auto_execs_routed_proposal_minted`, `_routed_proposal_countersigned`, `_routed_proposal_pending`, and (ZEB-300 converge R1) `tier2_auto_execs_already_applied`.
- Tick dispatch: update the `match` on `AutoExecOutcome` to bump the new stats. The `AutoExecSetPowerFn` closure signature `(SpaceId, OwnerAddr, u32)` is **unchanged** — only the returned enum grows, so no tick-signature churn.
- **Pass 3 split (ZEB-300 converge R1; window removed R2):** Pass 3a keeps only the finalize transition, Pass 3b re-dispatches auto-exec for every poll still `Lifecycle::Finalized` carrying a SetPower auto-exec (bounded by the 24h archive sweep; see §3). `AlreadyApplied` is counted separately from `tier2_auto_execs_attempted` (idempotent no-op, not a mint attempt).

**`src-tauri/src/lib.rs`**
- **Task 20.1 (R2):** wire the `start_node_inner` voting-tick `auto_exec_set_power` closure to `apply_auto_exec_set_power` via `wry_handle`'s managed-state seam (see §3 "Production wiring"). `apply_auto_exec_set_power` / `apply_auto_exec_admin_proposal_set_power` take `&Mutex<NodeState>`.

---

## 6. Testing (planner + materialize; per approved depth)

**Pure-planner unit tests** (in `community_membership.rs`, `mod plan_admin_proposal_tests`) — exhaustive:
- already-at-power → `AlreadyApplied`
- no candidate → `MintProposal`
- one live candidate, not yet signed by me → `Countersign(that.id)`
- one live candidate I proposed → `Pending`
- one live candidate I already countersigned → `Pending`
- two candidates → `Countersign(min EventId)` (canonical selection)
- only-expired candidate(s) → `MintProposal` (fresh proposal)

**Tick re-dispatch tests (ZEB-300 converge R1; renamed R2)** (in `community_voting_tick.rs`):
- `tier2_auto_exec_redispatches_finalized_poll` — a poll finalizes on tick 1 (dispatched once), then a later tick (well past the old 1h cutoff) still re-dispatches auto-exec while the poll is `Finalized`.
- `tier2_auto_exec_skips_redispatch_when_not_finalized` — a poll whose lifecycle is not `Finalized` (e.g. `Archived`) is NOT re-dispatched, proving the `lifecycle == Finalized` filter is the bound.

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
