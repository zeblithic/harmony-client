# ZEB-868 — Bound the cost of out-of-order projection rebuilds (Tier-3 voting)

**Ticket:** ZEB-868 (ZEB-860 follow-up). Medium.
**Branch:** `zeblith/zeb-868-rebuild-crypto-cost`
**Depends on / composes with:** ZEB-861 (PR #591, merged) — per-actor push caps; ZEB-860 (PR #590, merged) — canonical-order projection rebuild.

## 1. Problem & premise reconciliation

ZEB-860 re-materializes a Tier-3 poll's projection (`Tier3PollState::rebuild_from_events`)
whenever an **accepted, out-of-order** event of the order-dependent family
`{ss, md, ds, dv}` arrives (`community_voting_log.rs` `apply_with_snapshot`
Tier-3 branch, the `out_of_order && outcome == Applied && trigger_kind` block
at ~L547). ZEB-860 spec §4 intended all four trigger kinds to be
pre-Ratification and crypto-free in apply, so rebuilds would be cheap and no
memoization was needed. `ds`/`dv` self-gate to Deliberation (dropped elsewhere
⇒ not `Applied` ⇒ cannot trigger), but `ss`/`md` have **no apply-time stage
gate**, so a backdated `ss`/`md` arriving while the poll is already in
Ratification is `Applied` + out-of-order + trigger-kind ⇒ fires a full rebuild
that re-folds the poll's `rb`/`ts` events and re-runs their NIZK / DLEQ crypto.

**Premise reconciliation with ZEB-861 (verified against merged code):**

- The ticket's original "unbounded backdated `md` → O(n²)" framing is **obsolete**.
  ZEB-861 added a per-actor decline cap: an over-cap `md` sets
  `advance_last_hlc = false` (`community_voting_tier3.rs` L586–598), and
  `apply_event` returns `ApplyOutcome::Dropped` when `advance_last_hlc == false`
  (L1221–1224). The rebuild trigger requires `outcome == Applied`
  (`community_voting_log.rs` L549). Therefore a member can force **at most
  `MAX_DECLINES_PER_ACTOR` (2) md-driven rebuilds**, × mini-public size — bounded,
  not unbounded. `ss` has no per-actor cap and no stage gate, so it is the
  remaining (low-frequency) trigger vector.
- This ticket is therefore a **cost-model completion** ("make ZEB-860 §4's
  crypto-free-rebuild promise literally true, at real scale"), not an
  active-DoS fix. Scope confirmed with Jake: ship **both** components below.

## 2. Invariants preserved (non-negotiable)

1. **ZEB-320:** silent-drop paths must not advance the projection watermark
   `last_hlc`. Neither component touches `advance_last_hlc` or drop paths.
2. **ZEB-860 canonical projection:** the live projection must equal the
   canonical fold (and boot-restore) for every reachable state. Both components
   are strict **behavior-preserving optimizations** — they change *when work is
   done*, never *what outcome is produced*.
3. **Divergence-safety (ZEB-847 family):** every accept/reject decision must be
   a pure function of canonically-ordered state, identical on all replicas.
   Neither component changes any accept/reject decision.
4. **at-event-HLC (hard rule):** membership/stage judgments are evaluated at the
   event's **own** HLC, never at arrival stage. Component A evaluates
   `current_stage_at(&event.hlc)`.

## 3. Component A — trigger-gate: skip provably-useless rebuilds

Only rebuild when the triggering event's **own HLC** is pre-Ratification.

### 3.1 The predicate

`Stage` derives only `PartialEq, Eq` (no `Ord`, L20), so express the gate as an
explicit predicate rather than a `<` comparison. Add a method on `Stage`:

```rust
impl Stage {
    /// ZEB-868: a trigger-kind event whose own HLC lands in Ratification or a
    /// terminal stage cannot retroactively change any canonically-earlier
    /// Deliberation event, so it never needs a projection rebuild. Only the
    /// three pre-Ratification stages can.
    pub(crate) fn is_pre_ratification(self) -> bool {
        matches!(self, Stage::Sortition | Stage::Deliberation | Stage::Drafting)
    }
}
```

### 3.2 The call site (`community_voting_log.rs`, the trigger block ~L547–569)

The event's HLC is captured before the event is moved into `self.events`
(alongside `ev_key3`/`trigger_kind` at ~L485–497); add
`let ev_hlc = event.hlc.clone();` there. Restructure the trigger block so the
gate is evaluated first and the `mem::take` + rebuild + lifecycle re-sync run
only when a rebuild will actually happen:

```rust
if out_of_order
    && outcome == crate::community_voting_tier3::ApplyOutcome::Applied
    && trigger_kind
{
    let state = self.polls.get_mut(&poll_id).expect("poll present (just appended)");
    // ZEB-868: gate on the triggering event's OWN canonical HLC. A ss/md whose
    // HLC lands in Ratification+ sorts canonically-last and cannot change any
    // earlier Deliberation event's outcome (see §3.3), so its rebuild is pure
    // waste — and would re-run rb/ts crypto. Evaluated post-apply so the stage
    // reflects the event just applied.
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

(`as_tier3_mut` is used for the read because `current_stage_at` is `&self`;
if a `&self` accessor `as_tier3()` exists, use it for the read — cosmetic.)

### 3.3 Why skipping is sound (the second-order proof)

Skipping a rebuild asserts the **live arrival-order projection already equals
the canonical fold** for this event. A trigger-kind event with a Ratification+
HLC sorts canonically *after* every Deliberation event. The only events whose
outcomes depend on `ss`/`md` are:

- **`ds`/`dv` validity** — depends on `current_mini_public(&ds.hlc)`, which
  filters declines by `decline.hlc <= ds.hlc` (`community_voting_tier3.rs`
  L1355–1359). A decline with a Ratification-era HLC is `> ds.hlc` for every
  Deliberation-stage `ds`, so it never enters any deliberation mini-public.
- **`sortition_result`** — the `ss` apply arm overwrites it unconditionally
  (L572–579); the canonically-last `ss` wins in *both* arrival order
  (applied last) and canonical fold (sorted last), so the final value agrees.
  Deliberation events were already computed against the earlier `ss` in both
  orders (they precede the late `ss` by HLC in the fold too).

Hence a Ratification+-HLC trigger event cannot alter any earlier-applied
event's outcome: live == canonical already, and the rebuild is a no-op we can
skip. (A Deliberation- or Drafting-stamped event still rebuilds — it *can*
retroactively change the mini-public — so it stays in the `is_pre_ratification`
set.)

## 4. Component B — rb-NIZK verify-cache: crypto-free rebuilds at scale

Memoize the se-mode ratification-ballot NIZK verdict so a rebuild's dominant
per-voter crypto becomes O(1) cache hits.

### 4.1 What is cached, and why it is sound

The `rb` se-mode arm (`community_voting_tier3.rs` L916–943) computes
`verify_ballot_bundle(Y_epoch, ciphertexts_scores, ciphertexts_indicators, proof)`,
where:

- `Y_epoch` = committee joint verifying key at
  `committee_oracle.latest_epoch()` — a function of the **external**
  `committee_oracle` (an `Arc<dyn CommitteeOracle>` **preserved unchanged**
  across every rebuild reset, L511–514) and the epoch number.
- ciphertexts + proof = the event payload, uniquely identified by
  `event_hash = sha256_of_signing_bytes(ev)` (`[u8; 32]`, as used at L606).

So the verdict is a **pure function of `(event_hash, epoch)`**. A cache keyed on
`(event_hash, epoch)` returns exactly what a fresh `verify_ballot_bundle` would
compute. If the committee ever rotates (`latest_epoch()` advances), the key
changes → cache miss → recompute, so the cache can never return a verdict the
live path would not — it inherits, never introduces, the "verify against latest
epoch" behavior of the current code.

### 4.2 The field

Add to `Tier3PollState` (near the other ephemeral replay-derived fields
`max_applied`/`rebuild_count`, ~L235–240):

```rust
    /// ZEB-868: memoized se-mode `verify_ballot_bundle` (rb NIZK) verdicts,
    /// keyed on `(event_hash, committee_epoch)`. The verdict is a pure function
    /// of those two inputs (the committee oracle is external and preserved
    /// across rebuilds), so a cache hit is provably identical to a fresh verify
    /// — the cache changes *when* the NIZK runs, never *whether* a ballot is
    /// accepted. Ephemeral: never serialized; preserved across each rebuild's
    /// reset (like `committee_oracle`) so a rebuild re-folds crypto-free.
    /// Bounded by ZEB-861's per-actor ballot cap × electorate × committee epochs.
    pub(crate) rb_nizk_verdicts: std::collections::BTreeMap<([u8; 32], u64), bool>,
```

- **`new_from_create`** (L464–484): initialize
  `rb_nizk_verdicts: std::collections::BTreeMap::new(),`.
- **`rebuild_from_events`** (L508–515): preserve across the reset, mirroring the
  `committee_oracle` clone-and-restore:

  ```rust
  let oracle = self.committee_oracle.clone();
  let verdicts = std::mem::take(&mut self.rb_nizk_verdicts); // ZEB-868 preserve
  let rebuilds = self.rebuild_count;
  *self = Tier3PollState::new_from_create(meta, electorate);
  self.committee_oracle = oracle;
  self.rb_nizk_verdicts = verdicts; // ZEB-868 restore
  self.rebuild_count = rebuilds + 1;
  ```

- **hand-rolled `Debug`** (L256+): add
  `.field("rb_nizk_verdicts", &self.rb_nizk_verdicts.len())` to keep the dump
  complete (count only — verdicts are uninteresting and potentially large).

`BTreeMap<([u8;32], u64), bool>` is `Clone`, so `#[derive(Clone)]` on
`Tier3PollState` still holds. No serde (the struct has none; the field is never
persisted).

### 4.3 The rb-arm integration (L916–943)

Compute the epoch once, then look up / populate the cache around the crypto:

```rust
} else if mode == "se" {
    // NIZK verify against committee Y at latest known epoch, memoized by
    // (event_hash, epoch) — the verdict's only inputs (ZEB-868).
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
                        let proof_struct = crate::community_voting_tier3_nizk::BallotBundleProof {
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
    // ... unchanged: if !nizk_ok { drop } else { per-actor ballot cap + push }
}
```

Only the `Some(cs)` path (committee known → the expensive
decompress+`verify_ballot_bundle`) is memoized. `epoch == None` (no committee)
yields `false` without caching — it is transient (a later epoch may appear) and
cheap. `advance_last_hlc`, the per-actor ballot cap, and the push are unchanged.

### 4.4 Why `ts` DLEQ is deliberately **not** cached

The `ts` (TallyShare) DLEQ arm (L1044–1067) verifies each share against
`c1_agg` — the **homomorphic aggregate of accepted ratification ballots** —
whose value depends on which ballots have been folded when the `ts` applies
(fold-order-dependent). So the `ts` verdict is **not** a pure function of
`event_hash`; keying a cache on `event_hash` (or `(event_hash, epoch)`) would be
unsound (a hit could return a verdict a fresh recompute would not). `ts` is left
uncached. This is an acceptable residual: `ts` is per-committee-member
(threshold-sized), far less frequent than per-voter `rb`, so `rb` NIZK is the
dominant rebuild cost. Documented in §6.

## 5. Second-order correctness review

- **Reset vs. preserve discipline.** The per-actor caps (`declines_per_actor`,
  `candidates_per_actor`, `ballots_per_actor`) are correctly **reset** by
  `new_from_create` — the fold recomputes them. `rb_nizk_verdicts` is correctly
  **preserved** — it is a pure `(event_hash, epoch) → bool` memo, valid
  regardless of fold order. Mixing these up (resetting the cache would only cost
  a re-verify; preserving a cap would double-count) — neither happens.
- **Cache never changes an accept/reject.** A hit returns the identical bool a
  recompute would (§4.1), so the set of accepted ballots, the tally, and every
  downstream verdict are unchanged. No divergence, no live-vs-restore drift.
  Boot-restore starts with an empty cache and recomputes once — identical
  outcomes, same cost as today.
- **Trigger-gate never skips a needed rebuild** (§3.3 proof): the only skipped
  rebuilds are provably no-ops (live already equals canonical).
- **No ZEB-860 trigger-kind change.** The `{ss, md, ds, dv}` trigger set is
  untouched; `dc`/`da`/`rb`/`ts` remain non-triggers (ZEB-861 deliberately kept
  `dc`/`rb` out of the trigger set). Component A only *narrows* when the existing
  trigger fires.
- **Bounded memory.** `rb_nizk_verdicts` holds ≤ one entry per
  `(distinct accepted-or-seen rb event, epoch)`; rb events are bounded by
  ZEB-861's `MAX_RATIFICATION_BALLOTS_PER_ACTOR` × electorate, epochs by
  committee rotations. Ephemeral (dropped on process exit, rebuilt on boot).

## 6. What is explicitly NOT changed (residuals)

- `ts` DLEQ remains uncached (§4.4) — fold-order-dependent verdict; per-committee
  frequency makes it a minor residual.
- No ingest-time rejection is added (a naive "reject beyond N" would be
  receiver-dependent → divergence, the ZEB-847 trap). Volume is already bounded
  by ZEB-861; this ticket bounds only *cost*.
- No rebuild coalescing/debounce (a valid future optimization; not needed once
  rebuilds are crypto-free and count-bounded).

## 7. Tests

Component A (in `community_voting_log.rs` tests, at `apply_with_snapshot` level):
1. `ratification_stamped_md_out_of_order_does_not_rebuild` — a poll in
   Ratification; a backdated-but-Ratification-HLC `md` arrives out-of-order; assert
   `rebuild_count` does **not** increment (and projection unchanged).
2. `deliberation_stamped_md_delivered_in_ratification_does_rebuild` — a `md`
   whose HLC is in Deliberation, delivered while the poll is in Ratification,
   out-of-order; assert `rebuild_count` **increments** (retroactive mini-public
   change is honored).
3. (defense-in-depth, from the ZEB-860 review) `dc`/`da` out-of-order at
   `apply_with_snapshot` level assert **no** rebuild (they are non-triggers) —
   currently only pinned at `apply_event` level.

Component B (in `community_voting_tier3.rs` tests):
4. `rb_nizk_verdict_memoized_across_rebuild` — build an se-mode poll in
   Ratification with ≥1 valid `rb`; apply it (populates cache); force a
   `rebuild_from_events`; assert the cache survived the reset (non-empty) and the
   ballot is still accepted. Use a fixture `CommitteeOracle` whose
   `verify_ballot_bundle` inputs are known-good.
5. `rb_nizk_cache_hit_equals_fresh_verify` — assert a cached verdict equals a
   fresh `verify_ballot_bundle` for the same `(event_hash, epoch)` (accept AND a
   deliberately-invalid-proof reject both memoize correctly).
6. `rb_nizk_cache_key_includes_epoch` — two committee epochs with different
   `Y`; the same `event_hash` under a rotated epoch is a cache **miss** and
   recomputes (guards the epoch-keying).
7. `stage_is_pre_ratification_predicate` — unit-test the `Stage::is_pre_ratification`
   truth table (Sortition/Deliberation/Drafting = true; Ratification/Finalized/
   Failed = false).

All se-mode crypto tests reuse the existing NIZK/committee fixtures
(`community_voting_tier3_nizk`, the fixture oracle at ~L5611).

## 8. Global constraints (CI parity)

- Rust from `src-tauri/`. MSRV 1.91.
- `cargo fmt --all -- --check` = clean.
- `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
  = clean (touches inline `#[cfg(test)]` + integration targets).
- `cargo nextest run --locked --workspace --all-targets --features test-fixtures`
  = green (full CI-parity sweep before PR; `scripts/test-select` for iterative
  gates, pasting its `round=… bucket=…` summary line into task reports).
- No new dependencies. No serialization-format change (the cache field is never
  persisted). No public-API change outside the two Tier-3 modules.
