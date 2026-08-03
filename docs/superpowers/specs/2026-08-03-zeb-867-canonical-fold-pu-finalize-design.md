# ZEB-867 — Canonical-fold pu finalize (Tier-3 kd=rs verify/apply TOCTOU)

**Ticket:** ZEB-867 (Tier-3 kd=rs verify/apply TOCTOU — pu-mode PollResult can be applied stale). Medium.
**Branch:** `zeblith/zeb-867-tier-3-kdrs-verifyapply-toctou-pu-mode-result-can-be-applied`
**Composes with:** ZEB-850 (verify-at-ingest), ZEB-858 (kd=rs se-mode memo + lock discipline), ZEB-859 (first-close-wins `close_event_hash`), ZEB-860 (canonical-order projection rebuild), ZEB-861 (per-actor materialization caps), ZEB-868 (crypto-free rebuilds — rb-NIZK verify-cache).

> **Design refinement (during spec authoring):** the brainstorm approved a third
> component — adding `kd=rs` to the rebuild-trigger family for "lowest-HLC-`kd=rs`
> wins." Deeper analysis (§3.3) shows this is **unnecessary and less safe**: every
> valid ballot is canonically *before every honest finalize* (stage-gating), so the
> finalize's HLC cut-point is irrelevant — the result is the tally over all received
> ballots regardless of which `kd=rs` finalizes. Dropping it keeps the `kd=rs` ingest
> path (and its ZEB-858 DoS early-out) **untouched** and removes an adversarial
> backdated-finalize-cut vector. The shipped design is **two components**.

## 1. Problem

`process_inbound` (`community_voting_log_engine.rs`) verifies a `kd=rs` (PollResult)
and applies it as **two separate `voting_log` lock acquisitions**: the verify
(`inbound_eligibility_check`, `:2977`) clones the poll under the lock, **releases
it**, then recomputes the expected tally and compares; the apply (`:2988`)
re-acquires the lock and `apply_event` stores `payload.result` **verbatim**
(`community_voting_tier3.rs:1230`). The clone-and-release is load-bearing — ZEB-858
must never hold `voting_log` across the se-mode threshold-decrypt.

Between verify and apply the lock is open, so a concurrent `kd=rb` ballot apply can
shift the pu tally. In pu-mode `tally_star` depends on `ratification_ballots`, so
the applied (verbatim) result can be stale relative to the state it finalizes into.

### 1.1 Why the verbatim store makes the *stored value* TOCTOU-safe — but not enough

Because `apply_event` stores the signed constant `payload.result`, the verify/apply
gap cannot corrupt the stored value: it is `payload.result` whether or not a ballot
slips in. This is also what keeps ZEB-860's live == boot-restore invariant true today
(both paths store the same constant). **se-mode is fully safe** — the recovered tally
is Lagrange-invariant, so committee shares beyond `threshold` and late ballots do not
change the result.

The residual is **cross-replica divergence in pu-mode**: each node engine-auto-mints
its own `kd=rs` from its *own* ballot set, and there is no canonical "close the ballot
set" boundary — so two replicas with different received ballots finalize to different
results, and a late (backdated) ballot arriving after finalize is never folded in. This
is the "which ballots are authoritative at close" protocol question the ticket flags as
separate from the TOCTOU mechanics. This spec **resolves it**: the authoritative ballot
set is *every received ballot* (all are canonically pre-finalize), folded in canonical
order, converging as ballots propagate.

### 1.2 Why the naive fix (recompute-at-apply alone) would REGRESS ZEB-860

Recomputing the pu tally at apply, on its own, breaks live == restore: live recomputes
over arrival-order ballots while boot-restore's `rebuild_from_events` recomputes over
canonical-order ballots — and a backdated ballot sits *before* the `kd=rs` in HLC order.
So recompute-at-apply is only sound as a **package** with a rebuild that re-folds
post-finalize backdated ballots (§3.2).

## 2. Invariants preserved (non-negotiable)

1. **ZEB-860 canonical projection:** live projection == canonical fold == boot-restore
   for every reachable state. The finalized pu result becomes a pure function of the
   poll's canonical event fold.
2. **ZEB-320:** silent-drop paths must not advance `last_hlc`. Unchanged — the rebuild
   path re-derives `last_hlc` from the replayed events; no drop advances it.
3. **Divergence-safety (ZEB-847 family):** every accept/reject and every stored result
   is a pure function of canonically-ordered state, identical on all replicas that have
   received the same events. No receiver-dependent stored value.
4. **ZEB-858 lock discipline:** `voting_log` is never held across the se-mode
   threshold-decrypt. se-mode apply stays verbatim; the recompute-at-apply is pu-only
   (cheap `tally_star`, no decrypt). The `kd=rs` ingest path is untouched.

## 3. Design — two components

### 3.1 Component 1: pu recompute-at-apply

`apply_event` PollResult arm (`community_voting_tier3.rs:1227`). Branch on
`self.meta.config.privacy_mode`:

- **pu:** recompute the result from current state and store that, instead of
  `payload.result`. Reuse the existing `expected_result_from_state(self)` (which for pu
  calls `tally_star(ordered_candidates, &self.ratification_ballots)` — cheap, no
  decrypt, `Some` once drafting has produced candidates). On the (unexpected) recompute
  `Err` — e.g. `StatusQuoNotSynthesized` in a malformed state — **fall back to the
  verbatim `payload.result`** so the arm is never worse than today.
- **se:** unchanged — store `payload.result` verbatim (Lagrange-invariant; the ingest
  memo already validated it; must not decrypt under the apply lock).

Because all valid `kd=rb` are stage-gated into the Ratification window
(`verify_ratification_ballot` requires `current_stage_at(hlc) == Ratification`) and the
finalize sits after close, **every valid ballot is canonically pre-finalize** — so
`tally_star` over the full ballot *set* equals the canonical-prefix tally (an
order-independent sum). Recompute-over-set is therefore correct.

### 3.2 Component 2: record-and-rebuild for post-finalize backdated ballots (pu-gated)

`community_voting_log.rs`, the ZEB-860 apply block (`:499`). Today
`apply_event(&event)?` propagates `PollInFinalizedState` and the event is dropped
(never recorded). For Option B, a backdated ballot arriving after finalize must be
foldable. So:

> When `apply_event` returns `Err(PollInFinalizedState)` for an **out-of-order**
> (`ev_key3 <= prev_max`) `kd=rb` on a **pu** poll, do **not** propagate: append the
> event to `state.events` + `self.events` and call `rebuild_from_events`, then
> `sync_lifecycle_from_stage`.

The canonical replay folds the backdated ballot in *before* the `kd=rs`, re-derives the
tally via Component 1, and re-finalizes. Genuinely post-close events (HLC after the
finalize) are **not** out-of-order (`ev_key3 > prev_max`, since the finalize advanced
`max_applied`) → not recorded → dropped, correctly excluded.

- **pu-gated:** se polls keep today's exact behavior (the post-finalize event is dropped
  — se finalize is Lagrange-invariant, so nothing would change anyway; avoids se rebuild
  churn and any `ts`/`c1_agg` interaction). The mode is read from the poll's tier3 state
  already in hand at the log-apply site (`tier3_state.meta.config.privacy_mode`).
- **`kd=rb` only:** a post-finalize `kd=rs` is already rejected at ingest by the ZEB-858
  `PollAlreadyFinalized` early-out and never reaches apply — and (§3.3) it is not needed.
- **`Finalized` only:** `Failed` is not loosened. A late ballot must never un-fail a poll.
- **No partial mutation:** the terminal guard is `apply_event` step 1 (before any field
  write), so an `Err(PollInFinalizedState)` leaves the projection untouched; the rebuild
  is the sole mutation.

### 3.3 Why lowest-HLC-`kd=rs`-wins is unnecessary (and dropped)

Convergence does **not** require a canonical finalize point. A valid `kd=rb` has
`current_stage_at(hlc) == Ratification`, i.e. `hlc ∈ [rat_open, rat_close)`; an honest
`kd=rs` is minted after close, so `rs.hlc ≥ rat_close > every ballot's hlc`. Thus **all
ballots precede every honest finalize** — `tally_star` over "ballots before `rs_A`" ==
"ballots before `rs_B`" == "all received ballots," independent of which `kd=rs`
finalizes. Two replicas that have received the same ballots converge to the same result
regardless of finalize HLC. Making `kd=rs` a trigger would only matter if a ballot could
straddle two finalize HLCs — which the stage-gating forbids — and would *hand an
adversary a backdated-finalize-cut* to exclude ballots. So the `kd=rs` ingest/apply path
is left exactly as ZEB-858 shipped it.

## 4. Data flow — the load-bearing scenario

Poll in Ratification, ballots `b1,b2` applied (`ratification_ballots = [b1,b2]`).

1. `kd=rs` (HLC_rs) arrives → Component 1 recomputes `tally_star` over `[b1,b2]` → `R12`;
   `stage = Finalized`.
2. Backdated `b0` (HLC_b0 < HLC_rs, in the Ratification window) arrives late.
   - Ingest verify passes (`current_stage_at(HLC_b0) == Ratification`, electorate ok).
   - `apply_event(b0)` → `Err(PollInFinalizedState)` (terminal guard).
   - Component 2: out-of-order + `kd=rb` + pu → append `b0` + `rebuild_from_events`.
   - Rebuild replays canonically: `b0,b1,b2` fold into `ratification_ballots`, then `kd=rs`
     recomputes `tally_star` over `[b0,b1,b2]` → `R012`; `stage = Finalized`.
3. Boot-restore replays the same `self.events` → `R012`. **live == restore.** A second
   replica that received `{b0,b1,b2,rs}` in any order converges to `R012`.

## 5. Second-order correctness review

- **live == restore.** All valid ballots are canonically pre-finalize (§3.1), so the only
  live-vs-restore gap under recompute-at-apply is a post-finalize backdated ballot — closed
  by Component 2. No valid ballot has HLC > finalize (stage-gating), so recompute-over-set ==
  canonical-prefix.
- **Divergence-safety.** The stored result is a pure function of the canonical fold over
  received events; no receiver-dependent stored value. Convergence is eventual (as ballots
  propagate), the CRDT property.
- **DoS bound.** A post-finalize backdated ballot forces one crypto-free (ZEB-868) rebuild,
  bounded by ZEB-861's per-actor ballot cap (≤ `MAX_RATIFICATION_BALLOTS_PER_ACTOR`) ×
  electorate. No new unbounded work.
- **se untouched.** Components 1 and 2 are pu-gated; se apply, memo, `kd=rs` ingest, and lock
  discipline are byte-for-byte unchanged.
- **Terminal guard intact for the live projection** — Component 2 re-derives only via the
  controlled `rebuild_from_events` path; direct post-finalize mutation is still rejected, and
  `Failed` is never loosened.

## 6. What is explicitly NOT changed

- se-mode finalize (verbatim + ZEB-858 memo/lock discipline) and the `kd=rs` ingest path.
- The ingest verify (`inbound_eligibility_check`) stays as a gossip-hygiene + admission gate.
- The ZEB-860 pre-finalize trigger family `{ss,md,ds,dv}` (`:491`) — unchanged; the new
  behavior lives only in the `Err(PollInFinalizedState)` branch (§3.2).
- ZEB-846 forward-skew reject still bounds how far a backdated HLC can reach.
- No ingest-time "reject beyond N ballots" (receiver-dependent → divergence; ZEB-861 already
  bounds volume). This ticket bounds *correctness of the fold*, not volume.
- No rebuild coalescing/debounce (bounded by ZEB-861 caps, crypto-free via ZEB-868; a future
  optimization if post-finalize rebuild frequency ever matters at scale).

## 7. Tests

- `pu_backdated_ballot_after_finalize_refolds` — the §4 scenario: post-finalize backdated pu
  ballot re-finalizes to the augmented tally; live projection == a forced `rebuild_from_events`.
- `pu_finalize_converges_under_reordered_delivery` — two `Tier3PollState`s fed `{b0,b1,b2,rs}`
  in different orders finalize to identical `result`.
- `se_late_ballot_after_finalize_is_unaffected` — se poll: a late `kd=rb` after finalize is
  dropped (today's behavior), result stable, no rebuild.
- `pu_post_close_higher_hlc_ballot_excluded` — a `kd=rb` with HLC after the finalize is not
  out-of-order → dropped → stays excluded across a rebuild.
- `pu_recompute_at_apply_matches_verbatim_when_no_late_ballot` — with no interleaving, the
  recomputed result equals the minted `payload.result` (no behavior change on the happy path).
- Existing se-mode rb/rs tests, ZEB-860 rebuild tests, and ZEB-868 cache tests still pass.

## 8. Global constraints (CI parity)

- Rust from `src-tauri/`. MSRV 1.91. Always `--locked`.
- `cargo fmt --all -- --check` clean; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` clean.
- `cargo nextest run --locked --workspace --all-targets --features test-fixtures` green.
- No new dependencies. No serialization-format change. No public-API change outside
  `community_voting_tier3.rs` and `community_voting_log.rs`.
