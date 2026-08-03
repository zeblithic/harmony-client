# ZEB-868 — Bound the cost of out-of-order projection rebuilds (Tier-3 voting)

**Ticket:** ZEB-868 (ZEB-860 follow-up). Medium.
**Branch:** `zeblith/zeb-868-rebuild-crypto-cost`
**Depends on / composes with:** ZEB-861 (PR #591, merged) — per-actor push caps; ZEB-860 (PR #590, merged) — canonical-order projection rebuild.

> **Design note (PR #592 review, 2026-08-03):** the original design had two
> components — a trigger-gate (A) and a verify-cache (B). Adversarial review
> (CodeAnt Critical) showed **Component A is divergence-unsafe** and it was
> **dropped**; see §3. The shipped change is **Component B only** — the
> load-bearing, provably-correct crypto-free-rebuild cache, which fully closes
> the ticket's cost-model gap on its own.

## 1. Problem & premise reconciliation

ZEB-860 re-materializes a Tier-3 poll's projection (`Tier3PollState::rebuild_from_events`)
whenever an **accepted, out-of-order** event of the order-dependent family
`{ss, md, ds, dv}` arrives (`community_voting_log.rs` `apply_with_snapshot`
Tier-3 branch). ZEB-860 spec §4 intended all four trigger kinds to be
pre-Ratification and crypto-free in apply, so rebuilds would be cheap and no
memoization was needed. But `ss`/`md` have **no apply-time stage gate**, so a
backdated `ss`/`md` arriving while the poll is already in Ratification is
`Applied` + out-of-order + trigger-kind ⇒ fires a full rebuild that re-folds the
poll's `rb`/`ts` events and re-runs their NIZK / DLEQ crypto.

**Premise reconciliation with ZEB-861 (verified against merged code):** the
ticket's original "unbounded backdated `md` → O(n²)" framing is **obsolete**.
ZEB-861's per-actor decline cap makes an over-cap `md` return
`ApplyOutcome::Dropped`, and the rebuild trigger requires `Applied`, so
`md`-driven rebuilds are bounded to ≤`MAX_DECLINES_PER_ACTOR` × mini-public
size. This ticket is therefore **cost-model completion** — delivering ZEB-860
§4's promise that rebuilds are crypto-free — not an active-DoS fix.

## 2. Invariants preserved (non-negotiable)

1. **ZEB-320:** silent-drop paths must not advance the projection watermark
   `last_hlc`. The change never touches `advance_last_hlc` or drop paths.
2. **ZEB-860 canonical projection:** the live projection must equal the
   canonical fold (and boot-restore) for every reachable state. Component B is a
   strict **behavior-preserving optimization** — it changes *when work is done*,
   never *what outcome is produced*.
3. **Divergence-safety (ZEB-847 family):** every accept/reject decision must be
   a pure function of canonically-ordered state, identical on all replicas.
   Component B changes no accept/reject decision.

## 3. Component A (trigger-gate) — considered and REJECTED

The original design gated the rebuild trigger on the triggering event's own-HLC
stage being pre-Ratification, to skip "provably useless" Ratification-stamped
`ss`/`md` rebuilds. **PR #592 review (CodeAnt, Critical) showed this is
divergence-unsafe**, and it was dropped. The soundness argument required each
skipped event's apply to be order-independent (live arrival-order == canonical
fold), which does **not** hold:

- **`ss` (SortitionSelection)** — `apply_event` overwrites `sortition_result`
  unconditionally (LWW by apply order). Two Ratification-era `ss` with different
  selections, delivered so the *lower*-HLC one is applied last (out-of-order) and
  its rebuild is skipped, leave live holding the lower-HLC selection while
  canonical replay retains the higher-HLC one → **divergent `sortition_result`**
  (and hence divergent mini-public membership).
- **`md` (MiniPublicDecline)** — ZEB-861's per-actor cap makes *which* declines
  are retained order-dependent (live keeps first-2-arrived; canonical keeps
  first-2-by-HLC). At a Ratification-era `now`, `current_mini_public` (consumed
  by `verify_sd` at an incoming event's HLC, and by the IPC read path) can then
  disagree on whether an actor is in the declined set → divergent admission /
  display.

Making the gate correct would require order-independent `ss`/`md` apply (out of
scope) or per-consumer commutativity guarantees the apply layer does not provide.
Since Component B already makes rebuilds crypto-free — so the gate's only
remaining benefit is skipping a *bounded* number of now-cheap folds — the gate is
not worth its divergence risk. **Removed.** A provably-safe rebuild-skip can be
revisited later as a separate change (e.g. after canonicalizing `ss`/`md` apply).

## 4. Component B — rb-NIZK verify-cache (the shipped change)

Memoize the se-mode ratification-ballot NIZK verdict so a rebuild's dominant
per-voter crypto becomes O(1) cache hits.

### 4.1 What is cached, and why it is sound

The `rb` se-mode arm computes
`verify_ballot_bundle(Y_epoch, ciphertexts_scores, ciphertexts_indicators, proof)`,
where `Y_epoch` = committee joint verifying key at `committee_oracle.latest_epoch()`
(a function of the **external** `committee_oracle`, preserved unchanged across
every rebuild reset) and ciphertexts+proof = the event payload, identified by
`event_hash = sha256_of_signing_bytes(ev)` (`[u8; 32]`). So the verdict is a
**pure function of `(event_hash, epoch)`**. A cache keyed on `(event_hash, epoch)`
returns exactly what a fresh `verify_ballot_bundle` would; if the committee
rotates, the key changes → cache miss → recompute, so the cache can never return
a verdict the live path would not.

### 4.2 Bounded admission (PR #592 review — Qodo Bug, CodeRabbit Major)

The cache inserts **only on the accept path** — a ballot that passes the NIZK
**and** is under the per-actor ballot cap (`MAX_RATIFICATION_BALLOTS_PER_ACTOR`).
Consequently:

- **Invalid-proof ballots** (verdict `false`) are **not** cached — unlimited
  distinct invalid proofs (attacker-controlled) cannot grow the map.
- **Over-cap valid ballots** are **not** cached — they are dropped, so they never
  enter `ratification_ballots` nor the cache.

The map is therefore bounded by the same quantity as `ratification_ballots`:
**ZEB-861's per-actor ballot cap × electorate × committee epochs**. The tradeoff:
a rebuild re-verifies invalid/over-cap events (not cached), but those are the
attacker-flood events that were going to be dropped anyway; the *retained*
ballots (the ones the tally re-folds) are crypto-free on rebuild — and the cache
adds **no** new unbounded-memory vector.

### 4.3 The field & preserve-across-reset

`Tier3PollState.rb_nizk_verdicts: BTreeMap<([u8; 32], u64), bool>`, initialized
empty in `new_from_create`, preserved across the `rebuild_from_events` reset
exactly like `committee_oracle` (capture before `*self = new_from_create(...)`,
restore after), and surfaced as a count in the hand-rolled `Debug`. Ephemeral:
never serialized (`Tier3PollState` has no serde); boot-restore starts empty and
repopulates during replay, paying the crypto once — same as today. `BTreeMap<…,
bool>` is `Clone`, so `#[derive(Clone)]` still holds.

### 4.4 Why `ts` DLEQ is deliberately **not** cached

The `ts` (TallyShare) DLEQ arm verifies each share against `c1_agg` — the
homomorphic aggregate of accepted ratification ballots — which is
fold-order-dependent. So the `ts` verdict is **not** a pure function of
`event_hash`; keying a cache on it would be unsound. `ts` is per-committee-member
(threshold-sized), far less frequent than per-voter `rb`, so this is a minor
residual.

## 5. Second-order correctness review

- **Reset vs. preserve discipline.** The per-actor caps (`declines_per_actor`,
  etc.) are correctly **reset** by `new_from_create` — the fold recomputes them.
  `rb_nizk_verdicts` is correctly **preserved** — it is a pure
  `(event_hash, epoch) → bool` memo, valid regardless of fold order.
- **Cache never changes an accept/reject.** A hit returns the identical bool a
  recompute would (§4.1), so the set of accepted ballots, the tally, and every
  downstream verdict are unchanged. No divergence, no live-vs-restore drift.
- **Bounded memory** (§4.2): admission is accept-only, so the map is bounded by
  the retained-ballot count; invalid/over-cap flood events never grow it.

## 6. What is explicitly NOT changed (residuals)

- `ts` DLEQ remains uncached (§4.4) — fold-order-dependent verdict.
- No ingest-time rejection is added (a naive "reject beyond N" would be
  receiver-dependent → divergence, the ZEB-847 trap). Volume is bounded by
  ZEB-861; this ticket bounds only *cost*.
- The out-of-order rebuild trigger is unchanged from ZEB-860 (Component A
  reverted): every accepted out-of-order `{ss,md,ds,dv}` still rebuilds — now
  crypto-free.

## 7. Tests

- `rb_nizk_verdict_preserved_across_rebuild` — the verdict cache survives the
  rebuild reset (so a rebuild re-folds crypto-free).
- `rb_nizk_cache_admits_only_accepted_ballots` — an accepted ballot memoizes
  `true`; an invalid-NIZK ballot is **not** cached; over-cap valid ballots are
  **not** cached (cache size == accepted count == per-actor cap); the key is
  `(event_hash, epoch)` (a different-epoch lookup misses).
- Existing se-mode rb tests (`kd_rb_se_mode_valid_ballot_accepted`,
  `…_invalid_nizk_silent_drops`, `…_n_derived_…`) and ZEB-860 rebuild tests
  (`live_out_of_order_vote_is_rebuilt`, `byzantine_backdated_vote_is_dropped_after_rebuild`,
  `in_order_delivery_does_not_rebuild`) still pass — accept/drop and the
  unconditional rebuild trigger are unchanged.

## 8. Global constraints (CI parity)

- Rust from `src-tauri/`. MSRV 1.91. Always `--locked`.
- `cargo fmt --all -- --check` clean; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` clean.
- `cargo nextest run --locked --workspace --all-targets --features test-fixtures` green.
- No new dependencies. No serialization-format change (the cache field is never
  persisted). No public-API change outside the two Tier-3 modules.
