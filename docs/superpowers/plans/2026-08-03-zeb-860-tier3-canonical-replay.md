# ZEB-860 Tier-3 Canonical-Order Projection Materialization — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make a Tier-3 poll's `Tier3PollState` a deterministic function of the *set* of applied events (folded in canonical HLC order) so replicas converge regardless of delivery order — fixing the `kd=dv → kd=ds` cross-lane silent-drop-and-persist divergence.

**Architecture:** Detect out-of-order arrival of the order-dependent Deliberation family `{ss, md, ds, dv}` and synchronously rebuild that one poll's projection by re-folding its (per-poll) events in canonical order. In-order arrivals keep the incremental fast path. The trigger lives in `VotingLog::apply_with_snapshot`'s Tier-3 branch, so live inbound, publish, backfill, and boot restore all inherit it. No async, no membership resolver, no memoization.

**Tech Stack:** Rust; `src-tauri/src/community_voting_tier3.rs` and `community_voting_log.rs`.

## Global Constraints

- Canonical key is exactly `(wall_ms, logical, device_id, event_hash)` via one shared `canonical_key` helper; out-of-order detection uses its `(wall_ms, logical, device_id)` prefix (`max_applied`).
- Rebuild trigger is exactly: **out-of-order AND `ApplyOutcome::Applied` AND kind ∈ {SortitionSelection, MiniPublicDecline, DeliberationStatement, DeliberationVote}**. No broader, no narrower.
- Rebuild resets only replay-derived fields via `new_from_create`; `meta` (holds `community_epoch`), `eligible_electorate_snapshot`, and `committee_oracle` are preserved. Rebuild is monotone-additive.
- Membership stays resolved from stored poll state at each event's own HLC; never add an anti-backdating guard.
- Nothing on `Tier3PollState` (incl. `max_applied`, `rebuild_count`) is serialized / `notify_dirty` / replicated.
- Full CI-parity sweep before the PR: `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures --no-fail-fast`. Run from `src-tauri/`. Iterative gates may use `--lib`.

---

## File Structure

- `src-tauri/src/community_voting_tier3.rs` — add `ApplyOutcome`; change `apply_event` return type; add `max_applied` + `rebuild_count` fields; add `canonical_key` helper + `rebuild_from_events` method. (Tasks 1, 2)
- `src-tauri/src/community_voting_log.rs` — wire the out-of-order rebuild trigger into the Tier-3 branch of `apply_with_snapshot`. (Task 3)
- `src-tauri/src/lib.rs` — one restore-convergence integration test in the existing voting-reconcile test module. (Task 4)

Interfaces produced (consumed by later tasks):
- `pub(crate) enum ApplyOutcome { Applied, Dropped }` and `apply_event(&mut self, ev) -> Result<ApplyOutcome, ApplyError>` (Task 1).
- `pub(crate) fn canonical_key(ev: &SignedVotingEvent) -> (u64, u32, String, [u8; 32])` (Task 1).
- `Tier3PollState.max_applied: Option<(u64, u32, String)>` (Task 1); `Tier3PollState.rebuild_count: u64` (Task 2).
- `pub(crate) fn rebuild_from_events(&mut self, events: &[SignedVotingEvent])` (Task 2).

---

### Task 1: `ApplyOutcome` return, `max_applied` field, `canonical_key` helper

**Files:**
- Modify: `src-tauri/src/community_voting_tier3.rs` (`Tier3PollState`, `apply_event`, `new_from_create`; add `ApplyOutcome` + `canonical_key`)
- Test: same file's `#[cfg(test)]` module

**Interfaces:**
- Produces: `ApplyOutcome`, the new `apply_event` signature, `canonical_key`, and `max_applied`.
- Consumes: existing `sha256_of_signing_bytes(ev)` helper (used by the `cl` arm today) for the hash component.

**Context:** `apply_event` (tier3.rs:443) currently returns `Result<(), ApplyError>`. All 155 test call-sites use `.expect(...)`/`.unwrap()` as statements and discard the result, so a richer `Ok` value is non-breaking. The sole prod caller (`community_voting_log.rs:442`) uses `.map_err(...)?` in statement position and also discards it — leave it discarding for now; Task 3 captures it. The accept/drop distinction already exists internally as `advance_last_hlc` (true at line 474, set false in every silent-drop branch).

- [ ] **Step 1: Write failing tests.**

```rust
#[test]
fn apply_event_reports_applied_vs_dropped() {
    let mut poll = build_poll_in_deliberation_stage(); // sets mini-public
    // An accepted statement from a mini-public author:
    let s = ds_event_with_text(10_000, addr(1), "hello world statement");
    assert_eq!(poll.apply_event(&s), Ok(ApplyOutcome::Applied));
    // A dv referencing a NON-existent statement is silently dropped:
    let missing = [0u8; 32];
    let v = dv_event(11_000, addr(2), missing, 3);
    assert_eq!(poll.apply_event(&v), Ok(ApplyOutcome::Dropped));
}

#[test]
fn max_applied_advances_on_accept_and_drop() {
    let mut poll = build_poll_in_deliberation_stage();
    let s = ds_event_with_text(10_000, addr(1), "a statement here");
    let _ = poll.apply_event(&s).unwrap();
    let after_accept = poll.max_applied.clone();
    assert!(after_accept.is_some());
    // A dropped event with a HIGHER key still advances max_applied:
    let dropped = dv_event(20_000, addr(2), [0u8; 32], 3); // dropped (no statement)
    assert_eq!(poll.apply_event(&dropped), Ok(ApplyOutcome::Dropped));
    assert!(poll.max_applied > after_accept, "drop path must still advance max_applied");
}

#[test]
fn canonical_key_orders_by_hlc_then_hash() {
    let a = ds_event_with_text(10_000, addr(1), "first statement text");
    let b = ds_event_with_text(20_000, addr(1), "second statement text");
    assert!(canonical_key(&a) < canonical_key(&b), "earlier wall_ms sorts first");
    // Same (wall,logical,device), different payload → event_hash breaks the tie deterministically.
    let c = ds_event_with_text(10_000, addr(1), "zzz different text same time");
    let (ka, kc) = (canonical_key(&a), canonical_key(&c));
    assert_eq!((ka.0, ka.1, ka.2.clone()), (kc.0, kc.1, kc.2.clone()));
    assert_ne!(ka.3, kc.3, "distinct events get distinct hash tiebreakers");
}
```

- [ ] **Step 2: Run tests → fail to compile** (`ApplyOutcome`, `canonical_key`, `max_applied` don't exist).

```
cd src-tauri && cargo nextest run --locked --lib --features test-fixtures -E 'test(apply_event_reports_applied) or test(max_applied_advances) or test(canonical_key_orders)'
```

- [ ] **Step 3: Add `ApplyOutcome` + `canonical_key`** near the top of the module (after imports / near `ApplyError`):

```rust
/// Whether an `apply_event` call changed the projection (`Applied`) or hit a
/// silent-drop branch (`Dropped`). Used by the apply layer's out-of-order
/// rebuild trigger to tell a state-changing accept from a no-op drop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApplyOutcome {
    Applied,
    Dropped,
}

/// Canonical total order for replay/rebuild: `(wall_ms, logical, device_id,
/// event_hash)`. `event_hash` (SHA-256 of signing bytes) is the final
/// tiebreaker — a strict total order with no ties even if a device reuses an
/// HLC. Matches the `dv`/`ts` LWW tiebreak.
pub(crate) fn canonical_key(ev: &SignedVotingEvent) -> (u64, u32, String, [u8; 32]) {
    (
        ev.hlc.wall_ms,
        ev.hlc.logical,
        ev.hlc.device_id.clone(),
        sha256_of_signing_bytes(ev),
    )
}
```

- [ ] **Step 4: Add `max_applied` field.** In the struct (after `last_received_hlc`):

```rust
    /// ZEB-860: highest `(wall_ms, logical, device_id)` key dispatched to this
    /// poll (accepted OR silently dropped). Read by the apply layer to detect
    /// out-of-order arrival and trigger a canonical-order rebuild. Never
    /// serialized (like `last_received_hlc`).
    pub(crate) max_applied: Option<(u64, u32, String)>,
```

Set it in `new_from_create` (add `max_applied: None,`) and in the hand-rolled `Debug` impl (`.field("max_applied", &self.max_applied)`).

- [ ] **Step 5: Change `apply_event` signature + tail.** Signature → `pub fn apply_event(&mut self, ev: &SignedVotingEvent) -> Result<ApplyOutcome, ApplyError>`. At the unified tail (where `last_received_hlc` advances, tier3.rs ~1056-1069), advance `max_applied` and return the outcome:

```rust
    // ZEB-860: advance the canonical out-of-order watermark on every dispatch
    // (accept or silent drop), beside the per-lane receive-watermark.
    let key3 = (ev.hlc.wall_ms, ev.hlc.logical, ev.hlc.device_id.clone());
    if self.max_applied.as_ref().is_none_or(|m| key3 > *m) {
        self.max_applied = Some(key3);
    }
    if advance_last_hlc {
        self.last_hlc = Some(ev.hlc.clone());
    }
    Ok(if advance_last_hlc {
        ApplyOutcome::Applied
    } else {
        ApplyOutcome::Dropped
    })
```

(Every early `return Ok(())` inside a match arm — if any — must become `return Ok(ApplyOutcome::Applied)`. Verify none of the silent-drop branches early-return: they set `advance_last_hlc = false` and fall through to this tail. If clippy flags `is_none_or` MSRV, use `self.max_applied.as_ref().map_or(true, |m| key3 > *m)`.)

- [ ] **Step 6: Run the three tests → pass.** Then the lib compiles; run the deliberation test group to confirm no regression:

```
cd src-tauri && cargo nextest run --locked --lib --features test-fixtures -E 'test(deliberation) or test(apply_event) or test(max_applied) or test(canonical_key)'
```

- [ ] **Step 7: Commit.**

```
git add -A && git commit -m "feat(voting): ZEB-860 apply_event returns ApplyOutcome + max_applied watermark + canonical_key"
```

---

### Task 2: `rebuild_from_events` (synchronous mini-restore) + `rebuild_count`

**Files:**
- Modify: `src-tauri/src/community_voting_tier3.rs` (`Tier3PollState`: add `rebuild_count`; add `rebuild_from_events`)
- Test: same file's `#[cfg(test)]` module

**Interfaces:**
- Consumes: `new_from_create`, `apply_event`, `canonical_key` (Task 1), `install_committee_oracle`.
- Produces: `rebuild_from_events`, `rebuild_count`.

**Context:** Each `PollState` owns its per-poll `events` Vec (`community_voting_log.rs:474`), so the rebuild input is `state.events` — no filtering of the global log. `new_from_create(meta, electorate)` is the canonical initial state; it defaults `committee_oracle` to `NullCommitteeOracle`, so the installed oracle must be re-installed after reset. `meta` carries `community_epoch` (patched post-create), so preserving `meta` preserves the epoch.

- [ ] **Step 1: Write failing tests.**

```rust
#[test]
fn rebuild_rematerializes_dropped_vote() {
    let mut poll = build_poll_in_deliberation_stage(); // addr(1), addr(2) in mini-public
    let s = ds_event_with_text(10_000, addr(1), "the statement being voted on");
    let s_hash = sha256_of_signing_bytes(&s);
    let v = dv_event(20_000, addr(2), s_hash, 4); // vote by addr(2) on s
    // Deliver the VOTE first (out of order): it drops because the statement is absent.
    assert_eq!(poll.apply_event(&v), Ok(ApplyOutcome::Dropped));
    assert_eq!(poll.apply_event(&s), Ok(ApplyOutcome::Applied));
    assert!(!poll.deliberation.votes.contains_key(&(addr(2), s_hash)),
        "arrival-order fold drops the early vote");
    // Rebuild from the same set → the vote is re-materialized.
    poll.rebuild_from_events(&[v.clone(), s.clone()]);
    assert!(poll.deliberation.votes.contains_key(&(addr(2), s_hash)),
        "canonical rebuild applies the vote (ds precedes dv by HLC)");
    assert_eq!(poll.rebuild_count, 1);
}

#[test]
fn rebuild_preserves_non_replay_state() {
    let mut poll = build_poll_in_deliberation_stage();
    let epoch_before = poll.meta.community_epoch;
    let electorate_before = poll.eligible_electorate_snapshot.clone();
    poll.install_committee_oracle(std::sync::Arc::new(MockCommitteeOracle { /* per existing fixture */ }));
    let s = ds_event_with_text(10_000, addr(1), "some statement text here");
    poll.rebuild_from_events(&[s]);
    assert_eq!(poll.meta.community_epoch, epoch_before, "community_epoch preserved");
    assert_eq!(poll.eligible_electorate_snapshot, electorate_before, "electorate preserved");
    // Oracle preserved: a MockCommitteeOracle answers where NullCommitteeOracle returns None.
    // (Assert via whatever query the fixture supports; the point is it is NOT NullCommitteeOracle.)
}

#[test]
fn rebuild_is_monotone_additive_for_in_order_set() {
    let mut poll = build_poll_in_deliberation_stage();
    let s = ds_event_with_text(10_000, addr(1), "statement one text");
    let s_hash = sha256_of_signing_bytes(&s);
    let v = dv_event(20_000, addr(2), s_hash, 5);
    poll.apply_event(&s).unwrap();
    poll.apply_event(&v).unwrap();
    let votes_before = poll.deliberation.votes.clone();
    let statements_before = poll.deliberation.statements.clone();
    poll.rebuild_from_events(&[s, v]);
    assert_eq!(poll.deliberation.votes, votes_before, "in-order set unchanged by rebuild");
    assert_eq!(poll.deliberation.statements, statements_before);
}
```

- [ ] **Step 2: Run → fail** (`rebuild_from_events`, `rebuild_count` missing).

```
cd src-tauri && cargo nextest run --locked --lib --features test-fixtures -E 'test(rebuild_rematerializes) or test(rebuild_preserves) or test(rebuild_is_monotone)'
```

- [ ] **Step 3: Add `rebuild_count` field** (after `max_applied`): `pub(crate) rebuild_count: u64,`; set `rebuild_count: 0` in `new_from_create`; add to `Debug`.

- [ ] **Step 4: Implement `rebuild_from_events`:**

```rust
    /// ZEB-860: re-materialize this poll's projection as a deterministic fold of
    /// `events` in canonical order. Resets only replay-derived fields (via the
    /// canonical `new_from_create`), preserving `meta` (incl. `community_epoch`),
    /// `eligible_electorate_snapshot`, and the installed `committee_oracle`.
    /// Synchronous: `apply_event` reads only stored poll state, so no membership
    /// resolver is needed. Each `Err` on re-fold is a terminal/monotonic
    /// rejection, ignored exactly as boot replay ignores un-replayable events.
    pub(crate) fn rebuild_from_events(&mut self, events: &[SignedVotingEvent]) {
        let meta = self.meta.clone();
        let electorate = std::mem::take(&mut self.eligible_electorate_snapshot);
        let oracle = self.committee_oracle.clone();
        let rebuilds = self.rebuild_count;
        *self = Tier3PollState::new_from_create(meta, electorate);
        self.committee_oracle = oracle;
        self.rebuild_count = rebuilds + 1;

        let mut sorted: Vec<&SignedVotingEvent> = events.iter().collect();
        sorted.sort_by(|a, b| canonical_key(a).cmp(&canonical_key(b)));
        for ev in sorted {
            let _ = self.apply_event(ev);
        }
    }
```

- [ ] **Step 5: Run the three tests → pass**, then the deliberation regression group:

```
cd src-tauri && cargo nextest run --locked --lib --features test-fixtures -E 'test(rebuild) or test(deliberation)'
```

- [ ] **Step 6: Commit.**

```
git add -A && git commit -m "feat(voting): ZEB-860 rebuild_from_events — synchronous canonical mini-restore"
```

---

### Task 3: Wire the out-of-order rebuild trigger into `apply_with_snapshot`

**Files:**
- Modify: `src-tauri/src/community_voting_log.rs` (Tier-3 branch of `apply_with_snapshot`, ~lines 433-477)
- Test: same file's `#[cfg(test)]` module

**Interfaces:**
- Consumes: `ApplyOutcome`, `canonical_key`, `max_applied`, `rebuild_from_events`, `rebuild_count` (Tasks 1-2); `as_tier3`/`as_tier3_mut` (log.rs:139/145).

**Context:** The Tier-3 branch calls `tier3_state.apply_event(&event)`, syncs `state.meta.lifecycle` from `tier3_state.stage`, then appends to `state.events` + `self.events`. Capture `max_applied` *before* the apply (the apply advances it), and after the append, rebuild when the trigger holds. `state.events` and `state.tier_state` are disjoint fields, so `t3.rebuild_from_events(&state.events)` split-borrows cleanly.

- [ ] **Step 1: Write failing tests** (in `community_voting_log.rs` tests; use the crate's existing Tier-3 poll+event test helpers — build a poll into Deliberation, then drive events through `apply_with_snapshot`). Assert via `log.polls[&pid].tier_state.as_tier3().unwrap()`:

```rust
#[test]
fn live_out_of_order_vote_is_rebuilt() {
    // ... construct log with a Tier-3 poll in Deliberation (pid), members A=addr(1), B=addr(2) ...
    // Deliver dv (vote by B on A's statement) BEFORE ds (A's statement):
    // apply_with_snapshot(dv) → dropped; apply_with_snapshot(ds) → out-of-order Applied ds → rebuild.
    let t3 = log.polls.get(&pid).unwrap().tier_state.as_tier3().unwrap();
    assert!(t3.deliberation.votes.contains_key(&(addr(2), s_hash)), "vote rebuilt live");
    assert_eq!(t3.rebuild_count, 1);
}

#[test]
fn in_order_delivery_does_not_rebuild() {
    // Deliver ds then dv (in HLC order). Vote applies incrementally; no rebuild.
    let t3 = log.polls.get(&pid).unwrap().tier_state.as_tier3().unwrap();
    assert!(t3.deliberation.votes.contains_key(&(addr(2), s_hash)));
    assert_eq!(t3.rebuild_count, 0, "in-order fast path must not rebuild");
}

#[test]
fn outsider_dropped_vote_does_not_rebuild() {
    // A dv from a NON-mini-public actor, delivered out of order, is silently
    // dropped (Dropped) → no rebuild (DoS guard).
    let t3 = log.polls.get(&pid).unwrap().tier_state.as_tier3().unwrap();
    assert_eq!(t3.rebuild_count, 0);
}

#[test]
fn byzantine_backdated_vote_is_dropped_after_rebuild() {
    // dv.hlc < ds.hlc but delivered [ds, dv]. dv accepts incrementally, then the
    // rebuild (canonical [dv, ds]) drops it — vote must be ABSENT, converging to canonical.
    let t3 = log.polls.get(&pid).unwrap().tier_state.as_tier3().unwrap();
    assert!(!t3.deliberation.votes.contains_key(&(addr(2), s_hash)));
    assert_eq!(t3.rebuild_count, 1);
}
```

- [ ] **Step 2: Run → fail.**

```
cd src-tauri && cargo nextest run --locked --lib --features test-fixtures -E 'test(live_out_of_order) or test(in_order_delivery) or test(outsider_dropped) or test(byzantine_backdated)'
```

- [ ] **Step 3: Wire the trigger.** Replace the Tier-3 branch body (log.rs:433-477) so it captures the prior watermark and outcome and rebuilds on the trigger:

```rust
        if event.tier == Tier::Sortition && event.kind != PollEventKindCode::PollCreate {
            let state = self.polls.get_mut(&poll_id).ok_or(ApplyError::EventBeforePollCreate)?;
            let tier3_state = state.tier_state.as_tier3_mut().ok_or(ApplyError::WrongTierStateForTier3Event)?;

            // ZEB-860: snapshot the out-of-order watermark BEFORE apply (apply advances it).
            let prev_max = tier3_state.max_applied.clone();
            let ev_key3 = (event.hlc.wall_ms, event.hlc.logical, event.hlc.device_id.clone());

            let outcome = tier3_state.apply_event(&event).map_err(|e| match e {
                crate::community_voting_tier3::ApplyError::InvalidKindForTier3(_) => ApplyError::InvalidKindForTier3,
                crate::community_voting_tier3::ApplyError::PollInFailedState
                | crate::community_voting_tier3::ApplyError::PollInFinalizedState
                | crate::community_voting_tier3::ApplyError::HlcNotMonotonic => ApplyError::IllegalTransition,
                crate::community_voting_tier3::ApplyError::PayloadDecode(_) => ApplyError::PayloadDecode,
            })?;

            // Cluster K: sync PollMeta.lifecycle from tier3 stage (BEFORE append).
            sync_lifecycle_from_stage(state); // extract the existing match into a helper, OR inline as today

            state.events.push(event.clone());
            self.events.push(event.clone());

            // ZEB-860: out-of-order arrival of an order-dependent Deliberation-family
            // event that WAS applied can retroactively change other events' outcomes
            // (a late ds unblocks a dropped dv; a backdated dv must be re-dropped).
            // Rebuild that poll's projection from its canonical-ordered events.
            let out_of_order = prev_max.as_ref().is_some_and(|m| ev_key3 <= *m);
            let trigger_kind = matches!(
                event.kind,
                PollEventKindCode::SortitionSelection
                    | PollEventKindCode::MiniPublicDecline
                    | PollEventKindCode::DeliberationStatement
                    | PollEventKindCode::DeliberationVote
            );
            if out_of_order && outcome == crate::community_voting_tier3::ApplyOutcome::Applied && trigger_kind {
                let state = self.polls.get_mut(&poll_id).expect("poll present (just appended)");
                // split-borrow: events (immut) + tier_state (mut) are disjoint fields.
                let events = std::mem::take(&mut state.events);
                if let Some(t3) = state.tier_state.as_tier3_mut() {
                    t3.rebuild_from_events(&events);
                }
                state.events = events;
                sync_lifecycle_from_stage(state); // re-sync after rebuild
            }
            return Ok(poll_id);
        }
```

Notes for the implementer:
- Confirm the exact `MiniPublicDecline` variant name against the enum (the decline kind — kd=md). Use the real variant.
- The `std::mem::take(&mut state.events)` + restore avoids a borrow conflict between `&state.events` and the `&mut` rebuild; alternatively use a direct disjoint field borrow if the borrow checker accepts `let events = &state.events; let t3 = state.tier_state.as_tier3_mut()...` — either is fine; pick the one that compiles cleanly without cloning the whole Vec twice.
- `sync_lifecycle_from_stage` is the existing `match tier3_state.stage { Finalized => …, Failed => …, _ => {} }` block (log.rs:460-472). Extract it to a small free fn `fn sync_lifecycle_from_stage(state: &mut PollState)` so it can run both before append and after rebuild without duplication; if extraction is awkward, inline the same match a second time after the rebuild.

- [ ] **Step 4: Run the four tests → pass.** Then the Tier-3 dispatch regression group:

```
cd src-tauri && cargo nextest run --locked --lib --features test-fixtures -E 'test(tier3) or test(dispatch_tier3) or test(out_of_order) or test(rebuild)'
```

- [ ] **Step 5: Commit.**

```
git add -A && git commit -m "feat(voting): ZEB-860 wire out-of-order canonical rebuild trigger in apply_with_snapshot"
```

---

### Task 4: Restore convergence + order-invariance regression sweep

**Files:**
- Test: `src-tauri/src/lib.rs` (existing voting-reconcile test module, near `reconcile_restores_tier3_community_epoch`, ~line 54105)
- Test: `src-tauri/src/community_voting_tier3.rs` (order-invariance sweep)

**Interfaces:** Consumes everything from Tasks 1-3. No production changes expected (tests only). If a genuine gap surfaces, fix at its source and note it.

**Context:** Restore replays through `apply_with_snapshot`, so it inherits the Task-3 trigger — this task proves it end-to-end and guards the order-invariance contract.

- [ ] **Step 1: Restore-convergence test** (mirror `reconcile_restores_tier3_community_epoch`). Build a `VotingLog` with a Tier-3 poll in Deliberation where the append order drops a `dv` (vote appended before its statement); persist via the snapshot/write path; reconcile from disk; assert the restored projection HAS the vote applied.

```rust
#[tokio::test]
async fn reconcile_converges_out_of_order_deliberation_vote() {
    // ... build log, drive [dv, ds] via apply_with_snapshot so state.events = [dv, ds]
    //     and the live projection already rebuilt (Task 3) — persist it ...
    // ... snapshot_for_persist → write → load → reconcile_voting_from_state ...
    let restored = /* fetch reconciled Tier3PollState */;
    assert!(restored.deliberation.votes.contains_key(&(voter, s_hash)),
        "restored projection is canonical: the out-of-order vote is applied");
}
```

- [ ] **Step 2: Order-invariance sweep** (tier3 test module): for each of `rb`, `ts`, `da`, `dc`, fold a representative event set in two delivery orders through a poll and assert the two projections are equal AND `rebuild_count == 0` (these kinds are order-independent and must NOT trigger a rebuild). Use existing per-kind event helpers.

```rust
#[test]
fn order_independent_kinds_converge_without_rebuild() {
    // e.g. two dc + one da, delivered [dc1, dc2, da] vs [da, dc2, dc1] (adjust so da's
    // candidate exists per ingest rules in the chosen fold), assert equal candidates/approvals
    // and rebuild_count == 0 for both.
}
```

- [ ] **Step 3: Run both new tests + the full voting groups → pass.**

```
cd src-tauri && cargo nextest run --locked --lib --features test-fixtures -E 'test(reconcile) or test(order_independent) or test(voting) or test(tier3)'
```

- [ ] **Step 4: Commit.**

```
git add -A && git commit -m "test(voting): ZEB-860 restore convergence + order-invariance regression sweep"
```

---

## Post-Task: Full CI-parity sweep (controller runs before PR)

From `src-tauri/`:

```
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures --no-fail-fast
```

All green → open PR (base `main`), body closes ZEB-860, fire CodeRabbit once.

## Self-Review Notes

- **Spec coverage:** invariant (Task 1-3), sync rebuild (Task 2), scoped trigger (Task 3), restore convergence (Task 4 §1), order-invariance contract (Task 4 §2), preserve-non-replay-state (Task 2), DoS guard = accepted-only trigger (Task 3 test). Byzantine-backdated dv (Task 3 test). Benign terminal residual: intentionally untested (out of scope).
- **Type consistency:** `ApplyOutcome`/`canonical_key`/`max_applied`/`rebuild_from_events`/`rebuild_count` names are identical across tasks. `apply_event` returns `Result<ApplyOutcome, ApplyError>` everywhere after Task 1.
- **Placeholder scan:** test bodies that depend on crate-private construction helpers (poll+event setup in `community_voting_log.rs`, per-kind sweeps) are described by intent + exact assertions; the implementer wires construction from the existing helpers (`build_poll_in_deliberation_stage`, `ds_event_with_text`, `dv_event`, `MockCommitteeOracle`, and the log-module Tier-3 poll builders). This is deliberate — those helpers live in the target modules and must be read in place, not guessed here.
