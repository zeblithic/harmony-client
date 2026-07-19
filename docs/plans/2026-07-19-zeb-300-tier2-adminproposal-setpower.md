# ZEB-300 — Route admin-affecting Tier 2 auto-exec SetPower through AdminProposal — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When a multi-admin-quorum community's Tier 2 Conviction vote finalizes an admin-affecting SetPower, the auto-exec mints an `AdminProposal::SetPower` that admins countersign to quorum (any-admin-proposes + canonical countersign), instead of the current `SkippedRequiresQuorum` NoOp.

**Architecture:** A pure planner `plan_admin_proposal_auto_exec` decides mint-vs-countersign-vs-noop from `(materialized state, event log)`; a thin `apply_auto_exec_admin_proposal_set_power` wrapper reads state under the engine lock, runs the plan, and mints the corresponding `AdminProposal`/`AdminCountersign` event. The `blocked_by_quorum` branch of `apply_auto_exec_set_power` now calls this wrapper. `AutoExecOutcome::SkippedRequiresQuorum` is retired for three routed outcomes.

**Tech Stack:** Rust (Tauri backend under `src-tauri/`), existing community-membership CRDT + AdminProposal machinery, `cargo nextest`.

**Design doc:** `docs/specs/2026-07-19-zeb-300-tier2-adminproposal-setpower-design.md`

## Global Constraints

- **Run cargo from `src-tauri/`.** Iterative gates use scoped selectors, NOT a full build: `cargo nextest run --lib <filter>` for unit tests; the full `cargo nextest run --workspace --all-targets` sweep + `cargo clippy --workspace --all-targets -- -D warnings` + `cargo fmt --all -- --check` only as the FINAL gate (a lib change relinks ~97 integ binaries ≈ 50 min, so avoid `--all-targets` mid-task).
- **`--locked`** on the final workspace gate.
- **Signing key (load-bearing):** routed `AdminProposal` / `AdminCountersign` events MUST be signed with `outbox.community_signing_key` (matching the manual `set_power_level` proposal path, `lib.rs:~41863`), NOT the direct-SetPower `outbox.signing_key`. Wrong key → silent `verify_event` rejection.
- **Canonical selection:** the proposal with the numerically smallest `EventId` (`[u8; 16]`) — a total order every replica computes identically.
- **"Admin-affecting" test:** `level == POWER_THRESHOLDS.max || current_power[target] == POWER_THRESHOLDS.max` (== 100).
- **Idempotency:** each admin signs a given proposal at most once (proposer OR one countersign) — enforced by the planner's already-signed scan.
- Commit after each task (frequent commits). Do NOT run the full `--all-targets` sweep until Task 6.

---

### Task 1: Extract shared `is_admin_affecting_set_power` helper (DRY, behavior-preserving)

**Files:**
- Modify: `src-tauri/src/community_membership.rs` (add helper; call it from `setpower_mint_admin_blocked_by_quorum` at ~`:5587`)
- Test: `src-tauri/src/community_membership.rs` (`mod auto_exec_tests` at ~`:5784`)

**Interfaces:**
- Produces: `pub(crate) fn is_admin_affecting_set_power(mat: &MaterializedMembership, target: OwnerAddr, level: u8) -> bool`

- [ ] **Step 1: Write the failing test**

Add to `mod auto_exec_tests`:

```rust
#[test]
fn is_admin_affecting_set_power_true_for_promote_to_100() {
    let mut mat = MaterializedMembership::default();
    let target = OwnerAddr([7u8; 32]);
    // target currently non-admin (power 0), level 100 => admin-affecting
    assert!(is_admin_affecting_set_power(&mat, target, 100));
    // target currently admin (power 100), level 50 (demote) => admin-affecting
    mat.power_levels.insert(target, 100);
    assert!(is_admin_affecting_set_power(&mat, target, 50));
    // non-admin-affecting: target power 10, level 20
    let other = OwnerAddr([8u8; 32]);
    mat.power_levels.insert(other, 10);
    assert!(!is_admin_affecting_set_power(&mat, other, 20));
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd src-tauri && cargo nextest run --lib is_admin_affecting_set_power_true_for_promote_to_100`
Expected: FAIL — `cannot find function is_admin_affecting_set_power`.

- [ ] **Step 3: Implement the helper**

Add near `setpower_mint_admin_blocked_by_quorum` (~`:5587`):

```rust
/// ZEB-250 §4.3: a SetPower is "admin-affecting" when it grants top power
/// (`level == max`) or touches a member who currently holds top power.
/// Extracted from the three prior copies (verify_event, set_power_level IPC,
/// setpower_mint_admin_blocked_by_quorum).
pub(crate) fn is_admin_affecting_set_power(
    mat: &MaterializedMembership,
    target: OwnerAddr,
    level: u8,
) -> bool {
    let target_power = mat.power_levels.get(&target).copied().unwrap_or(0);
    level == POWER_THRESHOLDS.max || target_power == POWER_THRESHOLDS.max
}
```

Then rewrite `setpower_mint_admin_blocked_by_quorum`'s body to delegate:

```rust
pub fn setpower_mint_admin_blocked_by_quorum(
    mat: &MaterializedMembership,
    target: OwnerAddr,
    level: u8,
) -> bool {
    if mat.admin_quorum <= 1 {
        return false;
    }
    is_admin_affecting_set_power(mat, target, level)
}
```

- [ ] **Step 4: Run to verify pass (new + existing quorum-blocked unit tests)**

Run: `cd src-tauri && cargo nextest run --lib is_admin_affecting_set_power setpower_mint_admin_blocked_by_quorum`
Expected: PASS — new test + the four existing `setpower_mint_admin_blocked_by_quorum_*` tests still green.

- [ ] **Step 5: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/community_membership.rs
git commit -m "ZEB-300 T1: extract is_admin_affecting_set_power helper (DRY)"
```

---

### Task 2: The pure planner `plan_admin_proposal_auto_exec`

**Files:**
- Modify: `src-tauri/src/community_membership.rs` (add `AdminProposalPlan` enum + planner near the auto-exec helpers ~`:5600`)
- Test: `src-tauri/src/community_membership.rs` (`mod auto_exec_tests`)

**Interfaces:**
- Consumes: `MaterializedMembership`, `BTreeMap<EventId, SignedMembershipEvent>`, `OwnerAddr`, `EventId = [u8; 16]`, `ADMIN_PROPOSAL_EXPIRY_MS` (`:~5372`), `ProposalKind::SetPower { target, level }`, `MembershipEventKind::{AdminProposal, AdminCountersign}`.
- Produces:
  - `pub(crate) enum AdminProposalPlan { MintProposal, Countersign(EventId), Noop }`
  - `pub(crate) fn plan_admin_proposal_auto_exec(mat: &MaterializedMembership, events: &BTreeMap<EventId, SignedMembershipEvent>, target: OwnerAddr, level: u8, self_owner: OwnerAddr, now_ms: u64) -> AdminProposalPlan`

- [ ] **Step 1: Write the failing tests**

Add a new `mod plan_admin_proposal_tests` in `community_membership.rs` (or extend `auto_exec_tests`). Use a small builder for an `AdminProposal::SetPower` / `AdminCountersign` `SignedMembershipEvent` (mirror the construction in `zeb_250_admin_proposal_materialize_tests` ~`:11581`; a local `fn mk_proposal(id, actor, target, level, wall_ms)` and `fn mk_countersign(id, actor, target_id, wall_ms)` helper). Cover all branches:

```rust
// (a) already at power => Noop
#[test]
fn plan_noop_when_target_already_at_level() {
    let target = OwnerAddr([1;32]); let me = OwnerAddr([2;32]);
    let mut mat = MaterializedMembership::default();
    mat.power_levels.insert(target, 100);
    let events = BTreeMap::new();
    assert!(matches!(
        plan_admin_proposal_auto_exec(&mat, &events, target, 100, me, 1_000),
        AdminProposalPlan::Noop));
}

// (b) no candidate => MintProposal
#[test]
fn plan_mint_when_no_existing_proposal() {
    let target = OwnerAddr([1;32]); let me = OwnerAddr([2;32]);
    let mat = MaterializedMembership::default();
    let events = BTreeMap::new();
    assert!(matches!(
        plan_admin_proposal_auto_exec(&mat, &events, target, 100, me, 1_000),
        AdminProposalPlan::MintProposal));
}

// (c) one live candidate not signed by me => Countersign(that id)
#[test]
fn plan_countersign_existing_unsigned_proposal() {
    let target = OwnerAddr([1;32]); let proposer = OwnerAddr([3;32]); let me = OwnerAddr([2;32]);
    let pid: EventId = [9u8;16];
    let mat = MaterializedMembership::default();
    let mut events = BTreeMap::new();
    events.insert(pid, mk_proposal(pid, proposer, target, 100, 1_000));
    match plan_admin_proposal_auto_exec(&mat, &events, target, 100, me, 1_500) {
        AdminProposalPlan::Countersign(got) => assert_eq!(got, pid),
        other => panic!("expected Countersign, got {other:?}"),
    }
}

// (d) I already proposed it => Noop
#[test]
fn plan_noop_when_i_am_proposer() {
    let target = OwnerAddr([1;32]); let me = OwnerAddr([2;32]);
    let pid: EventId = [9u8;16];
    let mat = MaterializedMembership::default();
    let mut events = BTreeMap::new();
    events.insert(pid, mk_proposal(pid, me, target, 100, 1_000));
    assert!(matches!(
        plan_admin_proposal_auto_exec(&mat, &events, target, 100, me, 1_500),
        AdminProposalPlan::Noop));
}

// (e) I already countersigned it => Noop
#[test]
fn plan_noop_when_i_already_countersigned() {
    let target = OwnerAddr([1;32]); let proposer = OwnerAddr([3;32]); let me = OwnerAddr([2;32]);
    let pid: EventId = [9u8;16]; let cid: EventId = [10u8;16];
    let mat = MaterializedMembership::default();
    let mut events = BTreeMap::new();
    events.insert(pid, mk_proposal(pid, proposer, target, 100, 1_000));
    events.insert(cid, mk_countersign(cid, me, pid, 1_100));
    assert!(matches!(
        plan_admin_proposal_auto_exec(&mat, &events, target, 100, me, 1_500),
        AdminProposalPlan::Noop));
}

// (f) two candidates => Countersign(min EventId)
#[test]
fn plan_countersign_canonical_min_event_id() {
    let target = OwnerAddr([1;32]); let a = OwnerAddr([3;32]); let b = OwnerAddr([4;32]); let me = OwnerAddr([2;32]);
    let low: EventId = [1u8;16]; let high: EventId = [2u8;16];
    let mat = MaterializedMembership::default();
    let mut events = BTreeMap::new();
    events.insert(high, mk_proposal(high, a, target, 100, 1_000));
    events.insert(low,  mk_proposal(low,  b, target, 100, 1_000));
    match plan_admin_proposal_auto_exec(&mat, &events, target, 100, me, 1_500) {
        AdminProposalPlan::Countersign(got) => assert_eq!(got, low),
        other => panic!("expected canonical Countersign(low), got {other:?}"),
    }
}

// (g) only an expired candidate => MintProposal (fresh window)
#[test]
fn plan_mint_when_only_candidate_expired() {
    let target = OwnerAddr([1;32]); let proposer = OwnerAddr([3;32]); let me = OwnerAddr([2;32]);
    let pid: EventId = [9u8;16];
    let mat = MaterializedMembership::default();
    let mut events = BTreeMap::new();
    events.insert(pid, mk_proposal(pid, proposer, target, 100, 1_000));
    let now = 1_000 + ADMIN_PROPOSAL_EXPIRY_MS + 1;
    assert!(matches!(
        plan_admin_proposal_auto_exec(&mat, &events, target, 100, me, now),
        AdminProposalPlan::MintProposal));
}

// (h) candidate for a DIFFERENT (target,level) is ignored => MintProposal
#[test]
fn plan_mint_ignores_proposal_for_other_target_or_level() {
    let target = OwnerAddr([1;32]); let other = OwnerAddr([5;32]); let proposer = OwnerAddr([3;32]); let me = OwnerAddr([2;32]);
    let pid: EventId = [9u8;16];
    let mat = MaterializedMembership::default();
    let mut events = BTreeMap::new();
    events.insert(pid, mk_proposal(pid, proposer, other, 100, 1_000)); // different target
    assert!(matches!(
        plan_admin_proposal_auto_exec(&mat, &events, target, 100, me, 1_500),
        AdminProposalPlan::MintProposal));
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cd src-tauri && cargo nextest run --lib plan_admin_proposal`
Expected: FAIL — `plan_admin_proposal_auto_exec` / `AdminProposalPlan` not found.

- [ ] **Step 3: Implement the enum + planner**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdminProposalPlan {
    MintProposal,
    Countersign(EventId),
    Noop,
}

/// Decide what (if anything) this admin replica should mint to advance a
/// finalized admin-affecting Tier 2 SetPower toward AdminProposal quorum.
/// Pure: no NodeState / engine. See design §4.
pub(crate) fn plan_admin_proposal_auto_exec(
    mat: &MaterializedMembership,
    events: &std::collections::BTreeMap<EventId, SignedMembershipEvent>,
    target: OwnerAddr,
    level: u8,
    self_owner: OwnerAddr,
    now_ms: u64,
) -> AdminProposalPlan {
    // 1. Effect already applied (quorum reached on an earlier tick).
    if mat.power_levels.get(&target).copied() == Some(level) {
        return AdminProposalPlan::Noop;
    }
    // 2. Live proposals for this exact (target, level).
    let canonical = events
        .values()
        .filter(|e| match &e.kind {
            MembershipEventKind::AdminProposal { proposal_kind } => matches!(
                proposal_kind,
                ProposalKind::SetPower { target: t, level: l } if *t == target && *l == level
            ),
            _ => false,
        })
        .filter(|e| now_ms.saturating_sub(e.at.wall_ms) <= ADMIN_PROPOSAL_EXPIRY_MS)
        .min_by_key(|e| e.id);

    let Some(canonical) = canonical else {
        // 3. No live candidate → propose.
        return AdminProposalPlan::MintProposal;
    };

    // 4/5. Already signed the canonical (proposer or countersign) → nothing to do.
    let already_signed = events.values().any(|e| match &e.kind {
        MembershipEventKind::AdminProposal { .. } => e.id == canonical.id && e.actor == self_owner,
        MembershipEventKind::AdminCountersign { target_event_id } => {
            *target_event_id == canonical.id && e.actor == self_owner
        }
        _ => false,
    });
    if already_signed {
        AdminProposalPlan::Noop
    } else {
        AdminProposalPlan::Countersign(canonical.id)
    }
}
```

Note: confirm the `ProposalKind::SetPower` field names (`target`, `level`) and `MembershipEventKind::AdminProposal { proposal_kind }` / `AdminCountersign { target_event_id }` shapes against the definitions at `community_membership.rs:48-333`; adjust the match bindings if the recon's names drifted. `e.at.wall_ms` is the event HLC wall clock; `e.id: EventId`; `e.actor: OwnerAddr`.

- [ ] **Step 4: Run to verify pass**

Run: `cd src-tauri && cargo nextest run --lib plan_admin_proposal`
Expected: PASS — all 8 planner tests green.

- [ ] **Step 5: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/community_membership.rs
git commit -m "ZEB-300 T2: pure plan_admin_proposal_auto_exec planner + unit tests"
```

---

### Task 3: New `AutoExecOutcome` variants (retire `SkippedRequiresQuorum`)

**Files:**
- Modify: `src-tauri/src/community_membership.rs` (`AutoExecOutcome` at ~`:5502`)
- Modify: `src-tauri/src/community_voting_tick.rs` (`TickStats` ~`:32-58`; dispatch `match` ~`:307-347`)

**Interfaces:**
- Produces: `AutoExecOutcome::{Applied, SkippedNotAdmin, RoutedProposalMinted, RoutedProposalCountersigned, RoutedProposalPending}` (removes `SkippedRequiresQuorum`).
- Produces: `TickStats` fields `tier2_auto_execs_routed_proposal_minted`, `_routed_proposal_countersigned`, `_routed_proposal_pending` (removes `tier2_auto_execs_skipped_requires_quorum`).

This task is a mechanical type change; it compiles-red until Task 4 fills the new arm. Deliverable = the type + dispatch compile against a temporary `todo!()`-free stub so tests build. To keep it independently testable, wire the dispatch and update the existing tick test in the SAME task.

- [ ] **Step 1: Update `AutoExecOutcome`**

In `community_membership.rs:~5502`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoExecOutcome {
    Applied,
    SkippedNotAdmin,
    /// admin-affecting SetPower under admin_quorum>1: this replica minted the AdminProposal.
    RoutedProposalMinted,
    /// this replica countersigned the canonical pending AdminProposal.
    RoutedProposalCountersigned,
    /// nothing to mint this tick (already signed the canonical, or effect already applied).
    RoutedProposalPending,
}
```

- [ ] **Step 2: Update `TickStats` + dispatch + the ZEB-297 pin test**

In `community_voting_tick.rs`: replace the `tier2_auto_execs_skipped_requires_quorum` field with the three routed counters (keep doc comments accurate), and update the dispatch `match` (~`:307-347`):

```rust
Ok(crate::community_membership::AutoExecOutcome::RoutedProposalMinted) => {
    stats.tier2_auto_execs_routed_proposal_minted += 1;
}
Ok(crate::community_membership::AutoExecOutcome::RoutedProposalCountersigned) => {
    stats.tier2_auto_execs_routed_proposal_countersigned += 1;
}
Ok(crate::community_membership::AutoExecOutcome::RoutedProposalPending) => {
    stats.tier2_auto_execs_routed_proposal_pending += 1;
}
```

Update `community_voting_tick_tier2_auto_exec_set_power_skipped_when_quorum_blocks` (~`:935`): rename to `..._routes_to_proposal_when_quorum_blocks`; make its injected `auto_exec_set_power` closure return `RoutedProposalMinted`; assert `stats.tier2_auto_execs_routed_proposal_minted == 1` (and the old skip counter is gone). Grep the whole workspace for `SkippedRequiresQuorum` and `skipped_requires_quorum` and fix every reference (there may be other test asserts or observability code).

- [ ] **Step 3: Run to verify build + tick tests**

Run: `cd src-tauri && cargo nextest run --lib community_voting_tick`
Expected: PASS — all tick tests green including the renamed routing test. (`apply_auto_exec_set_power`'s `blocked_by_quorum` branch still returns a value — temporarily map it to `RoutedProposalPending` until Task 4; grep-confirm no remaining `SkippedRequiresQuorum`.)

- [ ] **Step 4: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/community_membership.rs src-tauri/src/community_voting_tick.rs
git commit -m "ZEB-300 T3: retire SkippedRequiresQuorum for three routed outcomes + stats"
```

---

### Task 4: Wire `apply_auto_exec_admin_proposal_set_power` (mint/countersign under the lock)

**Files:**
- Modify: `src-tauri/src/community_membership.rs` (`apply_auto_exec_set_power` `blocked_by_quorum` branch ~`:5739`; add the new async helper)
- Test: `src-tauri/src/community_membership.rs` (extend the signing-path test at ~`:6068`)

**Interfaces:**
- Consumes: `mint_admin_proposal_set_power_event` (`lib.rs:41022`), `mint_admin_countersign_event` (`lib.rs:41242`) — reference as `crate::mint_admin_proposal_set_power_event` / `crate::mint_admin_countersign_event`. `outbox.community_signing_key`. `engine_arc.insert_local_event`. State guard exposes `.events` (per `count_signers` usage in `lib.rs:43020`).
- Produces: `async fn apply_auto_exec_admin_proposal_set_power(node_state, community_id, target_pubkey, level) -> Result<AutoExecOutcome, String>`.

- [ ] **Step 1: Write the failing signing-path test**

Model it on `auto_exec_set_power_signing_path_produces_verifiable_signature` (~`:6068`). New test `auto_exec_admin_proposal_routes_and_verifies`: build a community whose materialized state has `admin_quorum = 2`, local actor an admin (power 100, Joined), and no existing proposal; call `apply_auto_exec_admin_proposal_set_power(...)` to promote a non-admin to 100; assert it returns `RoutedProposalMinted` AND the inserted event is `MembershipEventKind::AdminProposal { ProposalKind::SetPower { target, level: 100 } }` that passes `verify_event` under the community's prior state (i.e., signed with `community_signing_key`). If the existing signing-path harness cannot reach the two-admin engine state cheaply, assert instead at the planner+mint seam: that the minted event verifies. Keep the test at the smallest scope that proves the community_signing_key path.

- [ ] **Step 2: Run to verify it fails**

Run: `cd src-tauri && cargo nextest run --lib auto_exec_admin_proposal_routes_and_verifies`
Expected: FAIL — helper not defined.

- [ ] **Step 3: Implement the helper + rewire the branch**

Replace the `blocked_by_quorum` NoOp (`:5739-5747`) with:

```rust
if blocked_by_quorum {
    return apply_auto_exec_admin_proposal_set_power(
        node_state, community_id, target_pubkey, level,
    ).await;
}
```

Add the helper (mirror `apply_auto_exec_set_power`'s handle-snapshot + lock structure; snapshot `community_signing_key` from `dm_outbox`, `self_owner`, reserve an HLC and derive `now_ms = event_hlc.wall_ms`):

```rust
pub async fn apply_auto_exec_admin_proposal_set_power(
    node_state: &std::sync::Arc<std::sync::Mutex<crate::NodeState>>,
    community_id: crate::owner_state_types::SpaceId,
    target_pubkey: crate::owner_state_types::OwnerAddr,
    level: u8,
) -> Result<AutoExecOutcome, String> {
    // ... snapshot handles + self_owner + community_registry + dm_outbox (as apply_auto_exec_set_power does),
    //     resolve engine_arc, reserve HLC (event_hlc), now_ms = event_hlc.wall_ms ...

    let plan = {
        let state_arc = engine_arc.state();
        let state_g = state_arc.lock().await;
        let mat = state_g.materialized(engine_arc.admin_addr());
        plan_admin_proposal_auto_exec(&mat, &state_g.events, target_pubkey, level, self_owner, now_ms)
    };

    let event = match plan {
        AdminProposalPlan::Noop => return Ok(AutoExecOutcome::RoutedProposalPending),
        AdminProposalPlan::MintProposal => {
            let outbox_g = dm_outbox.lock().await;
            let signing_key = outbox_g.community_signing_key.as_ref();
            crate::mint_admin_proposal_set_power_event(
                community_id, self_owner, target_pubkey, level, signing_key, event_hlc,
            )?
        }
        AdminProposalPlan::Countersign(pid) => {
            let outbox_g = dm_outbox.lock().await;
            let signing_key = outbox_g.community_signing_key.as_ref();
            crate::mint_admin_countersign_event(
                community_id, self_owner, pid, signing_key, event_hlc,
            )?
        }
    };

    let outcome_kind = match plan {
        AdminProposalPlan::MintProposal => AutoExecOutcome::RoutedProposalMinted,
        AdminProposalPlan::Countersign(_) => AutoExecOutcome::RoutedProposalCountersigned,
        AdminProposalPlan::Noop => unreachable!(),
    };

    let insert = engine_arc.insert_local_event(event).await
        .map_err(|e| format!("insert_local_event (AdminProposal auto-exec): {e}"))?;
    if matches!(insert, crate::community_state_crdt::InsertOutcome::Rejected(_)) {
        return Err(format!("AdminProposal auto-exec rejected: {insert:?}"));
    }
    Ok(outcome_kind)
}
```

Verify against source: exact field name of the community signing key on the outbox (`community_signing_key`), the `materialized` vs `materialize_now` method name, the state guard's `.events` accessor, and `mint_admin_countersign_event`'s `target_event_id` param type (`[u8; 16]` == `EventId`). Adjust to match.

- [ ] **Step 4: Run to verify pass**

Run: `cd src-tauri && cargo nextest run --lib auto_exec_admin_proposal_routes_and_verifies plan_admin_proposal community_voting_tick`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/community_membership.rs
git commit -m "ZEB-300 T4: apply_auto_exec_admin_proposal_set_power mint/countersign wrapper"
```

---

### Task 5: Materialize-level two-admin convergence integration test (AC #3)

**Files:**
- Modify: `src-tauri/tests/community_misc/community_admin_quorum_integration.rs`

**Interfaces:**
- Consumes: the existing hand-built-events + `materialize` pattern in that file (and its `bootstrap_two_admins_raise_quorum` helper ~`:58-83`); `count_signers` is internal, so assert on the materialized `power_levels`, not signer counts.

- [ ] **Step 1: Write the convergence test**

`tier2_admin_proposal_two_admin_quorum_converges`: bootstrap two admins A, B at power 100 with `admin_quorum = 2` (reuse `bootstrap_two_admins_raise_quorum`); a non-admin target T at power 0. Construct the exact events the two replicas' ticks would mint under the planner:
1. `AdminProposal::SetPower { target: T, level: 100 }` by A (id `P`).
2. `AdminCountersign { target_event_id: P }` by B.
Append to the bootstrap log, `materialize`, and assert `power_levels[T] == 100` (quorum reached → effect applied). Add a negative control: with ONLY the proposal by A (no countersign), assert `power_levels[T] != 100` (single signer < quorum 2 — proves the countersign is load-bearing).

- [ ] **Step 2: Run to verify it fails, then passes**

Run: `cd src-tauri && cargo nextest run --test community_misc tier2_admin_proposal_two_admin_quorum_converges` (adjust the test-binary filter to however `community_admin_quorum_integration.rs` is aggregated — it may be `--test community_admin_quorum_integration` or a `#[path]`-included module in a parent binary; confirm with `cargo nextest list | grep tier2_admin_proposal`).
Expected: PASS after the events are constructed correctly (this test is data-only, no new production code — it should pass immediately once written against the Task 2–4 code).

- [ ] **Step 3: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/tests/community_misc/community_admin_quorum_integration.rs
git commit -m "ZEB-300 T5: two-admin quorum convergence materialize test (AC3)"
```

---

### Task 6: Spec cross-note + final full gate

**Files:**
- Modify: `docs/specs/2026-05-16-zeb-289-voting-polling-design.md` (short note under §5 auto-exec actions and/or §10 auto-exec invalidation)

- [ ] **Step 1: Add the spec cross-note**

Under ZEB-289 §5 (auto-exec actions) or §10, add: in `admin_quorum > 1` communities an admin-affecting SetPower auto-exec routes through `AdminProposal` (any-admin-proposes + canonical min-`EventId` countersign; converges across ticks, tolerates absent admins; inert dangling proposals under simultaneous ticks expire per `ADMIN_PROPOSAL_EXPIRY_MS`). Reference ZEB-300 + the design doc.

- [ ] **Step 2: Full workspace gate**

Run (from `src-tauri/`, allow ~50 min for the relink):
```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo nextest run --workspace --all-targets --locked
```
Expected: fmt clean; clippy clean; all tests pass (0 failed). Grep-confirm zero remaining `SkippedRequiresQuorum` / `skipped_requires_quorum` anywhere.

- [ ] **Step 3: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add docs/specs/2026-05-16-zeb-289-voting-polling-design.md
git commit -m "ZEB-300 T6: spec cross-note (Tier2 auto-exec x admin-quorum routing)"
```

---

## Self-Review

**1. Spec coverage:** AC1 → Task 4 (`MintProposal` arm). AC2 → Task 2 planner (`Countersign`/already-signed) + runs every tick via Task 4. AC3 → Task 5 materialize convergence test. AC4 → Task 3 (variant retired). AC5 → Task 3 (stat retired + new routed stats) + Task 6 (spec/observability note). DRY cleanup (design §5) → Task 1. All covered.

**2. Placeholder scan:** No TBDs; each code step shows the actual code. The two "verify names against source" notes (Task 2 Step 3, Task 4 Step 3) are deliberate guardrails against recon name-drift, not placeholders — the code is fully specified and adjusted-if-needed.

**3. Type consistency:** `AdminProposalPlan` (Task 2) is consumed in Task 4 with matching variant names (`MintProposal`/`Countersign(EventId)`/`Noop`). `AutoExecOutcome` routed variants (Task 3) match those returned in Task 4 and bumped in Task 3's dispatch. `is_admin_affecting_set_power` (Task 1) signature matches its `setpower_mint_admin_blocked_by_quorum` caller. `EventId = [u8; 16]` used consistently. `community_signing_key` used for both mint calls.

## Execution Handoff

Per the autonomous post-spec drive, execute inline via subagent-driven-development (fresh subagent per task, gate + review between tasks), then open the PR and run the bot-convergence loop — no human checkpoint unless a genuine blocker/conflict arises.
