# ZEB-868 — Bound out-of-order rebuild cost Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Tier-3 poll projection rebuilds cheap — skip provably-useless rebuilds and memoize the dominant per-ballot crypto — without changing any accept/reject outcome.

**Architecture:** Two behavior-preserving optimizations in the Tier-3 voting engine. (A) Gate the `{ss,md,ds,dv}` out-of-order rebuild trigger on the triggering event's own-HLC stage being pre-Ratification. (B) Memoize the se-mode `rb` NIZK verdict in `Tier3PollState`, keyed on `(event_hash, epoch)`, preserved across the rebuild reset. Neither changes what a poll finalizes to; both change only *when work runs*.

**Tech Stack:** Rust (`src-tauri/`), `cargo nextest`, existing Tier-3 NIZK/committee-oracle fixtures.

**Spec:** `docs/superpowers/specs/2026-08-03-zeb-868-rebuild-trigger-crypto-cost-design.md` (commit 9ccbe9cd).

## Global Constraints

- Build/test from `src-tauri/`. MSRV **1.91**. Always `--locked`.
- `cargo fmt --all -- --check` clean; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` clean.
- Full CI-parity sweep before PR: `cargo nextest run --locked --workspace --all-targets --features test-fixtures`. Iterative gates may use `scripts/test-select --context task|round` — paste its `round=… bucket=…` summary line into the task report.
- **No accept/reject decision may change** — both components are strict optimizations; every existing test must still pass unchanged.
- No new dependencies. No serialization-format change (the new cache field is never persisted; `Tier3PollState` has no serde). No public-API change outside the two Tier-3 modules.
- Invariants preserved: ZEB-320 (drops don't advance `last_hlc`), ZEB-860 (live projection == canonical fold == boot-restore), divergence-safety (ZEB-847 family), at-event-HLC judgments.

---

### Task 1: Component A — trigger-gate on own-HLC stage

**Files:**
- Modify: `src-tauri/src/community_voting_tier3.rs` (add `Stage::is_pre_ratification`, ~after the `Stage` enum L21 or in an existing `impl Stage`)
- Modify: `src-tauri/src/community_voting_log.rs` (capture `ev_hlc`; gate the trigger block ~L485–569)
- Test: inline `#[cfg(test)]` in both files

**Interfaces:**
- Produces: `Stage::is_pre_ratification(self) -> bool` (`pub(crate)`), true for `Sortition | Deliberation | Drafting`.
- Consumes: existing `Tier3PollState::current_stage_at(&Hlc) -> Stage`, `PollState.tier_state.as_tier3_mut()`, `ApplyOutcome::Applied`, `sync_lifecycle_from_stage`.

- [ ] **Step 1: Write the failing predicate test** (`community_voting_tier3.rs` tests)

```rust
#[test]
fn stage_is_pre_ratification_truth_table() {
    assert!(Stage::Sortition.is_pre_ratification());
    assert!(Stage::Deliberation.is_pre_ratification());
    assert!(Stage::Drafting.is_pre_ratification());
    assert!(!Stage::Ratification.is_pre_ratification());
    assert!(!Stage::Finalized.is_pre_ratification());
    assert!(!Stage::Failed.is_pre_ratification());
}
```

- [ ] **Step 2: Run it — verify it fails to compile** (`is_pre_ratification` undefined)

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(stage_is_pre_ratification_truth_table)'`
Expected: compile error (no method `is_pre_ratification`).

- [ ] **Step 3: Add the predicate**

In `community_voting_tier3.rs`, near the `Stage` enum:

```rust
impl Stage {
    /// ZEB-868: a trigger-kind event whose own HLC lands in Ratification or a
    /// terminal stage cannot retroactively change any canonically-earlier
    /// Deliberation event, so it never needs a projection rebuild.
    pub(crate) fn is_pre_ratification(self) -> bool {
        matches!(self, Stage::Sortition | Stage::Deliberation | Stage::Drafting)
    }
}
```

(If an `impl Stage` block already exists, add the method there rather than opening a new block.)

- [ ] **Step 4: Run the predicate test — verify it passes**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(stage_is_pre_ratification_truth_table)'`
Expected: PASS.

- [ ] **Step 5: Write the failing trigger-gate tests** (`community_voting_log.rs` tests, `apply_with_snapshot` level)

Follow the existing ZEB-860 rebuild tests in this file for setup helpers (a poll driven to Ratification, then an out-of-order event fed via the dispatch path). Read `rebuild_count` through the tier3 accessor. Two tests:

```rust
#[test]
fn ratification_stamped_md_out_of_order_does_not_rebuild() {
    // Build a poll in Ratification with ≥1 applied deliberation event so a
    // rebuild would be observable. Feed an out-of-order md whose OWN hlc is in
    // the Ratification window. Assert rebuild_count is unchanged.
    // (See the sibling ZEB-860 rebuild test for the poll-construction helper.)
}

#[test]
fn deliberation_stamped_md_delivered_in_ratification_does_rebuild() {
    // Same poll in Ratification; feed an out-of-order md whose OWN hlc is in the
    // Deliberation window (arriving late). Assert rebuild_count increments by 1.
}
```

Plus the defense-in-depth non-trigger test (currently only pinned at `apply_event` level):

```rust
#[test]
fn dc_out_of_order_does_not_rebuild_at_apply_with_snapshot() {
    // A kd=dc arriving out-of-order is NOT a trigger kind → no rebuild.
    // Assert rebuild_count unchanged.
}
```

- [ ] **Step 6: Run the trigger-gate tests — verify they fail**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(ratification_stamped_md_out_of_order_does_not_rebuild) | test(deliberation_stamped_md_delivered_in_ratification_does_rebuild) | test(dc_out_of_order_does_not_rebuild_at_apply_with_snapshot)'`
Expected: `ratification_stamped_md…` and `dc_out_of_order…` FAIL (current code rebuilds unconditionally for md; dc test should already pass if dc is a non-trigger — if so, note it as a regression guard). `deliberation_stamped_md…` should already PASS (current behavior rebuilds).

- [ ] **Step 7: Implement the gate** (`community_voting_log.rs`)

Capture the event HLC alongside `ev_key3`/`trigger_kind` (~L485–497):

```rust
let ev_hlc = event.hlc.clone();
```

Replace the trigger block (~L547–569) so the stage gate decides whether to rebuild:

```rust
if out_of_order
    && outcome == crate::community_voting_tier3::ApplyOutcome::Applied
    && trigger_kind
{
    let state = self
        .polls
        .get_mut(&poll_id)
        .expect("poll present (just appended)");
    // ZEB-868: gate on the triggering event's OWN canonical HLC. A ss/md whose
    // HLC lands in Ratification+ sorts canonically-last and cannot change any
    // earlier Deliberation event's outcome, so its rebuild is pure waste (and
    // would re-run rb/ts crypto). Evaluated post-apply so the stage reflects the
    // event just applied. See spec §3.3 for the soundness proof.
    let should_rebuild = state
        .tier_state
        .as_tier3_mut()
        .map(|t3| t3.current_stage_at(&ev_hlc).is_pre_ratification())
        .unwrap_or(false);
    if should_rebuild {
        let events = std::mem::take(&mut state.events);
        if let Some(t3) = state.tier_state.as_tier3_mut() {
            t3.rebuild_from_events(&events);
        }
        state.events = events;
        sync_lifecycle_from_stage(state);
    }
}
```

- [ ] **Step 8: Run the trigger-gate tests — verify all pass**

Run: same `-E` filter as Step 6.
Expected: all three PASS.

- [ ] **Step 9: Task gate — fmt + clippy + scoped tests**

Run:
```
cd src-tauri && cargo fmt --all -- --check \
 && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings \
 && ../scripts/test-select --context task
```
Expected: fmt clean, clippy clean, selected suite green (record the `round=… bucket=…` line). If `test-select` bails on dep-graph change, use `cargo nextest run --locked --features test-fixtures -E 'test(voting)'` as the scoped fallback.

- [ ] **Step 10: Commit**

```bash
git add src-tauri/src/community_voting_tier3.rs src-tauri/src/community_voting_log.rs
git commit -m "ZEB-868 A: gate out-of-order rebuild on own-HLC pre-Ratification stage"
```

---

### Task 2: Component B — rb-NIZK verify-cache

**Files:**
- Modify: `src-tauri/src/community_voting_tier3.rs` — add `rb_nizk_verdicts` field (~L235), init in `new_from_create` (~L472), preserve in `rebuild_from_events` (~L509–515), extend hand-rolled `Debug` (~L256), wire cache into the `rb` se-mode arm (~L916–943)
- Test: inline `#[cfg(test)]` in `community_voting_tier3.rs`

**Interfaces:**
- Produces: `Tier3PollState.rb_nizk_verdicts: BTreeMap<([u8;32], u64), bool>` (`pub(crate)`).
- Consumes: `sha256_of_signing_bytes(ev) -> [u8;32]`, `committee_oracle.latest_epoch() -> Option<u64>`, `committee_oracle.committee_at_epoch(u64) -> Option<CommitteePublicState>` (owned), `verify_ballot_bundle`, the fixture `CommitteeOracle` (~L5611).

- [ ] **Step 1: Add the field, init, preserve, Debug** (compilable scaffold, no behavior change yet)

- Field on `Tier3PollState` (~after `rebuild_count`, L240):
```rust
    /// ZEB-868: memoized se-mode `verify_ballot_bundle` (rb NIZK) verdicts,
    /// keyed on `(event_hash, committee_epoch)` — the verdict's only inputs
    /// (the committee oracle is external and preserved across rebuilds), so a
    /// hit is provably identical to a fresh verify. Ephemeral: never serialized;
    /// preserved across each rebuild's reset so a rebuild re-folds crypto-free.
    /// Bounded by ZEB-861's per-actor ballot cap × electorate × committee epochs.
    pub(crate) rb_nizk_verdicts: std::collections::BTreeMap<([u8; 32], u64), bool>,
```
- `new_from_create` (~L472, in the struct literal):
```rust
            rb_nizk_verdicts: std::collections::BTreeMap::new(),
```
- `rebuild_from_events` (~L509–515): preserve like `committee_oracle`:
```rust
        let oracle = self.committee_oracle.clone();
        let verdicts = std::mem::take(&mut self.rb_nizk_verdicts); // ZEB-868 preserve
        let rebuilds = self.rebuild_count;
        *self = Tier3PollState::new_from_create(meta, electorate);
        self.committee_oracle = oracle;
        self.rb_nizk_verdicts = verdicts; // ZEB-868 restore
        self.rebuild_count = rebuilds + 1;
```
- hand-rolled `Debug` (~L256+): add
```rust
            .field("rb_nizk_verdicts", &self.rb_nizk_verdicts.len())
```

- [ ] **Step 2: Verify it compiles (no test yet)**

Run: `cd src-tauri && cargo check --locked --features test-fixtures`
Expected: compiles (field added everywhere the struct is constructed — `new_from_create` is the only literal).

- [ ] **Step 3: Write the failing cache tests**

```rust
#[test]
fn rb_nizk_verdict_memoized_across_rebuild() {
    // se-mode poll in Ratification + a valid rb (fixture oracle w/ known-good
    // verify inputs). Apply the rb → cache populated (len == 1). Force
    // rebuild_from_events → assert cache survived the reset (non-empty) AND the
    // ballot is still accepted (ratification_ballots non-empty).
}

#[test]
fn rb_nizk_cache_hit_equals_fresh_verify() {
    // Apply a valid rb (memoizes true) and an invalid-proof rb (memoizes false).
    // Assert rb_nizk_verdicts holds both verdicts and they match a direct
    // verify_ballot_bundle call on the same (event_hash, epoch).
}

#[test]
fn rb_nizk_cache_key_includes_epoch() {
    // Fixture oracle exposing two epochs with different joint keys. Apply an rb
    // at epoch e0 (cache miss → insert (hash,e0)). Rotate latest_epoch to e1.
    // Assert a lookup for (hash,e1) is a MISS (recompute), i.e. the same
    // event_hash under a different epoch is not served the e0 verdict.
}
```

- [ ] **Step 4: Run the cache tests — verify they fail**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(rb_nizk_verdict_memoized_across_rebuild) | test(rb_nizk_cache_hit_equals_fresh_verify) | test(rb_nizk_cache_key_includes_epoch)'`
Expected: FAIL (cache never populated — the rb arm doesn't touch `rb_nizk_verdicts` yet).

- [ ] **Step 5: Wire the cache into the rb se-mode arm** (~L916–943)

```rust
} else if mode == "se" {
    // NIZK verify against committee Y at latest known epoch, memoized by
    // (event_hash, epoch) — the verdict's only inputs (ZEB-868, spec §4).
    let epoch = self.committee_oracle.latest_epoch();
    let nizk_ok = match epoch.and_then(|e| self.committee_oracle.committee_at_epoch(e)) {
        Some(cs) => {
            let key = (sha256_of_signing_bytes(ev), epoch.expect("Some by and_then"));
            if let Some(&cached) = self.rb_nizk_verdicts.get(&key) {
                cached
            } else {
                let verdict = match crate::community_voting_tier3_crypto::decompress_point(
                    &cs.joint_verifying_key,
                ) {
                    Some(y_point) => {
                        let proof_ref = payload.proof.as_ref().unwrap();
                        let proof_struct =
                            crate::community_voting_tier3_nizk::BallotBundleProof {
                                range_proofs: proof_ref.range_proofs.clone(),
                                consistency_proofs: proof_ref.consistency_proofs.clone(),
                            };
                        crate::community_voting_tier3_nizk::verify_ballot_bundle(
                            &y_point,
                            payload.ciphertexts_scores.as_ref().unwrap(),
                            payload.ciphertexts_indicators.as_ref().unwrap(),
                            &proof_struct,
                        )
                    }
                    None => false,
                };
                self.rb_nizk_verdicts.insert(key, verdict);
                verdict
            }
        }
        None => false, // no committee yet → transient false, not cached
    };
    if !nizk_ok {
        // ... unchanged drop branch ...
    } else {
        // ... unchanged per-actor ballot cap + push ...
    }
}
```

Preserve the existing `if !nizk_ok { … } else { … }` body verbatim (per-actor ballot cap + push). Only the computation of `nizk_ok` is wrapped in the cache.

- [ ] **Step 6: Run the cache tests — verify they pass**

Run: same `-E` filter as Step 4.
Expected: all three PASS.

- [ ] **Step 7: Task gate — fmt + clippy + scoped tests**

Run:
```
cd src-tauri && cargo fmt --all -- --check \
 && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings \
 && ../scripts/test-select --context task
```
Expected: fmt clean, clippy clean (watch for `.expect()` lint / needless clone), selected suite green (record the `round=… bucket=…` line). Fallback if test-select bails: `cargo nextest run --locked --features test-fixtures -E 'test(voting)'`.

- [ ] **Step 8: Commit**

```bash
git add src-tauri/src/community_voting_tier3.rs
git commit -m "ZEB-868 B: memoize se-mode rb NIZK verdict across projection rebuilds"
```

---

### Final: whole-branch verification + PR

- [ ] **Full CI-parity sweep** (relink is expected — the lib change rebuilds integration binaries):
```
cd src-tauri && cargo fmt --all -- --check \
 && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings \
 && cargo nextest run --locked --workspace --all-targets --features test-fixtures
```
Expected: fmt/clippy clean, full suite green (no accept/reject regressions).

- [ ] **Whole-branch self-review** against the spec: both components behavior-preserving; reset-vs-preserve discipline correct (caps reset, cache preserved); no ZEB-860 trigger-kind change; ts correctly left uncached.

- [ ] **Open PR** (base `main`, `Closes ZEB-861`? → **`Closes ZEB-868`**). Fire CodeRabbit once at open; exclude Greptile; converge Qodo/CodeAnt findings in one push/round. Never auto-merge.

## Self-Review (plan vs spec)

- **Spec coverage:** §3 Component A → Task 1; §4 Component B → Task 2; §7 tests → the 7 spec tests are distributed across Tasks 1–2 (predicate truth-table, 2 gate tests + 1 non-trigger guard; 3 cache tests — the spec's #4/#5/#6, with #7 predicate covered in Task 1); §8 constraints → Global Constraints + per-task gates. ✓
- **Placeholder scan:** test bodies for the `apply_with_snapshot`-level gate tests describe setup by reference to the existing ZEB-860 rebuild test helper rather than reproducing it verbatim (the helper is long and file-local); the implementer reads the sibling test for the exact construction. All code to be *written* (predicate, gate block, field/init/preserve/Debug, rb-arm cache) is given in full. ✓
- **Type consistency:** `event_hash: [u8;32]` (from `sha256_of_signing_bytes`), `epoch: u64` (from `latest_epoch`), key `([u8;32], u64)`, map `BTreeMap<([u8;32],u64), bool>` — consistent across field decl, init, and rb-arm usage. `is_pre_ratification(self) -> bool` consistent between Task 1 produce and its Task 1 consume. ✓
