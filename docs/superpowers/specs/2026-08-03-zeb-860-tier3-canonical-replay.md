# ZEB-860 — Tier-3 canonical-order projection materialization

**Goal:** Make a Tier-3 poll's materialized `Tier3PollState` a deterministic function of the *set* of applied events — folded in canonical HLC order — so every replica converges on identical poll state, live and after restore, regardless of event delivery order.

**Architecture:** Fold the poll projection in a canonical total order `(wall_ms, logical, device_id, event_hash)` on the restore path (sort before re-fold) and, on the live path, rebuild an affected poll's projection from its canonically-ordered events whenever an out-of-order event arrives (in-order arrivals keep today's incremental fast path). A per-event apply-verdict cache keeps rebuilds free of repeated NIZK/DLEQ work.

**Tech stack:** Rust (`src-tauri/`), the existing community-voting subsystem (`community_voting_tier3.rs`, `community_voting_log.rs`, `community_voting_log_engine.rs`, `community_voting_persist.rs`), replay entry `reconcile_voting_from_state` (`lib.rs`).

---

## Background — the verified bug (on `main` @ 9076f03d)

Tier-3 poll projections (`Tier3PollState`) are **not persisted**. `PersistedVotingLog` stores `events: Vec<SignedVotingEvent>` + `policy` + a small `poll_restore` overlay; the materialized projection is re-folded on boot by replaying `events` through `VotingLog::apply_with_snapshot` (`reconcile_voting_from_state`), **in append (delivery) order, with no HLC sort**.

Several apply rules **silently drop** an event — return `Ok(())`, leave `advance_last_hlc = false` — when a prerequisite is missing, yet the event is **still appended** to the log (the append happens after `apply_event` returns `Ok`). Most cross-lane prerequisites are gated a second time at ingest (`inbound_eligibility_check`), which returns `Err` *before* the append, so those events are never logged out of order and their replay is order-safe: `da→dc` (`verify_da_candidate_exists`), `rs→close` (`verify_sr`), `rb→candidates` (`verify_ratification_ballot`).

The gap is the three **ingest-ungated** kinds — `ds` / `dv` / `ts`. The sharp instance is **`kd=dv`** (DeliberationVote): it references a specific statement (`kd=ds`) by `statement_event_hash` and is dropped when that statement is not yet present. Because you cannot vote on your own statement (`self_vote` is dropped), `dv.actor ≠ ds.author` always — the dependency is inherently **cross-lane**, so the per-`(actor,device)` receive-watermark cannot order the two. A `dv` delivered before its `ds` is dropped, still appended, and **never re-materialized** when the `ds` later arrives. Result: two honest replicas that saw the events in different orders hold permanently divergent tallies, persistent across restart. (`kd=ds` shares the class more softly via its dependence on `ss`/`md` for stage and mini-public membership.)

### Why canonical order is both safe and *more* correct

Every apply rule other than `dv`'s accept/drop decision is already order-independent: idempotent set-inserts keyed by `event_hash` (statements, candidates, approvals, decline lists) or last-writer-wins resolved **by HLC**, not by arrival (`dv` value, `ts` upsert, `ss` overwrite). So sorting the fold by HLC changes nothing for the convergent rules and *fixes* `dv`.

It is also causally sound: `dv` references `ds` by hash, so an honest `ds` was authored before its `dv` ⇒ `ds.hlc < dv.hlc` ⇒ in canonical order the statement always precedes the vote and the vote applies. A Byzantine `dv` stamped *earlier* than its statement stays dropped under canonical order — correct, because voting on a statement that did not yet exist is invalid. Sorting also makes the `ds` per-author 5-statement cap deterministic (keeps the 5 HLC-earliest) and makes #589's `close_event_hash` first-close-wins converge (every replica freezes the HLC-earliest close). Nothing regresses.

## The invariant

> For a given set `E` of events applied to a poll, `Tier3PollState = fold(sort_canonical(E))`, where `sort_canonical` orders by `(wall_ms, logical, device_id, event_hash)` — identical on every replica, evaluated the same live and after restore.

Corollary the implementation must preserve: **rebuild is monotone-additive.** Re-folding in canonical order never *removes* an event that applied in arrival order (all non-`dv` rules are order-independent); it only *adds* events that arrival order unfairly dropped.

## Design

### 1. Canonical order key

A helper that yields the total-order key `(hlc.wall_ms, hlc.logical, hlc.device_id, event_hash)` for a `SignedVotingEvent`. `event_hash` is the same signing-bytes hash used elsewhere as the LWW final tiebreaker, guaranteeing a strict total order with no ties even under a device that reuses an HLC.

### 2. Restore path (subsumes the contained "sort-on-restore" option)

In `reconcile_voting_from_state`, sort the loaded `events` by the canonical key **before** the re-fold loop. A global canonical sort induces per-poll canonical order, so every poll's projection is rebuilt deterministically. The per-event membership snapshot resolution (`snapshot_at(community_id, &event.hlc)`) is unchanged — each event is still applied against membership at its own HLC.

### 3. Live path — out-of-order rebuild

Track, per poll, the maximum canonical key that has been folded into the projection (`max_applied`). When an event is dispatched to a poll:

- **In-order** (its canonical key `> max_applied`): apply incrementally exactly as today, then advance `max_applied`. This is the common case and the hot path — unchanged behavior.
- **Out-of-order** (its canonical key `≤ max_applied`, i.e. an already-folded event outranks it): **rebuild the poll's projection** from its canonically-ordered event set.

The triggering event is **appended to the community log first** (it becomes part of the set), *then* the rebuild folds the full set — so the rebuild always includes the event that triggered it. This means the out-of-order detection and rebuild are orchestrated at the layer that owns both the `events` Vec and the membership resolver (the live ingest / `apply_with_snapshot` caller — `process_inbound` / `publish_event` / `apply_backfilled_event`), not inside the per-event `apply_event`. `apply_event` stays a pure per-event fold step.

Out-of-order detection is O(1) from `max_applied`. `max_applied` advances on **every** applied (appended) event — accept or silent-drop — because a dropped event still participates in canonical order (a later `ds` must out-rank an earlier-arriving-but-later-stamped event to be seen as in-order).

### 4. Rebuild = a mini-restore scoped to one poll

`rebuild_poll_projection(poll_id)`:

1. Collect the poll's events (filter the community `events` for this `poll_id`) and sort by the canonical key.
2. **Reset only the replay-derived projection fields** to their initial state — statements, deliberation votes, candidates, approvals, decline lists, sortition/tally result, `last_hlc`, `last_received_hlc`, `max_applied` — while **preserving the `poll_restore` overlay** (`meta` lifecycle, `tier2_timing`, `tier3_community_epoch`), which is not replay-derived.
3. Re-fold the sorted events through `apply_event`, resolving a fresh membership snapshot at each event's own HLC (identical discipline to the restore loop; no anti-backdating guard).

Because the fold resolves a snapshot per event, `rebuild_poll_projection` needs the **membership resolver** and is therefore `async` — reinforcing that it lives at the ingest/engine layer (which already holds the resolver for the live single-event snapshot), not inside `apply_event`. Folding in canonical order also means each lane's events are re-applied in ascending HLC, so the existing per-lane monotonic guard never spuriously rejects during a rebuild.

Restore (§2) and live rebuild (§4) share one primitive: *fold a canonically-ordered event list against per-event membership snapshots*. Restore folds all polls once; live rebuild folds one poll.

### 5. Apply-verdict cache (rebuild cost bound)

`apply_event` runs crypto inside its drop checks for two kinds — `rb` se-mode NIZK verification and `ts` T2 DLEQ verification. A naive rebuild re-runs those for every event, and an adversary can send cheap out-of-order events to force repeated rebuilds (a ZEB-858-flavored amplification). Bound it with a **per-event apply-verdict cache**: memoize the expensive verify result keyed by `event_hash`, so a rebuild reuses the verdict rather than re-running the proof.

- Ephemeral: lives on the in-memory replay tracker; never persisted, never `notify_dirty`, never replicated (same discipline as the ZEB-858 `verify_sr_memo`).
- Bounded: a fixed `MAX` entry cap with clear-on-overflow (mirrors `verify_sr_memo`).
- Soundness: only a *successful* verification is cached; a failed proof is never cached (short-circuits before insert). The verdict is a pure function of the event bytes, so it is rebuild-stable.

The residual O(n) fold cost per rebuild is bounded by the poll's event count — an unbounded log is **ZEB-861's** concern, out of scope here.

## Order-invariance obligations (must hold, verified by tests)

Canonical replay is only correct if every non-`dv` rule is order-invariant under it. The audit below is the acceptance contract:

- **Set-insert, idempotent** (`ds` statements, `dc` candidates, `da` approvals, `md` declines): keyed by `event_hash`; canonical order does not change the resulting set.
- **LWW by HLC** (`dv` value once accepted, `ts` upsert, `ss` overwrite): resolution already uses the HLC key; canonical order is consistent with it.
- **Monotone/terminal** (`rs` result + `Finalized`, `sf` Failed, `cl` first-close-wins): deterministic under canonical order (first-close-wins picks the HLC-earliest close).
- **Same-actor accumulation** (`ds` 5-statement cap): becomes deterministic (HLC-earliest 5 survive) — an intended improvement.
- **The fix** (`dv` accept/drop, `ds` via `ss`/`md`): the *only* rules whose accepted set changes under canonical order — the divergence closes.

## Testing strategy

1. **Divergence repro → fix (the headline test).** Build a poll to Deliberation with a statement `S` (author A) and a vote `V` on `S` (voter B, `V.hlc > S.hlc`). Fold two `VotingLog`s from the **same event set in opposite delivery orders** (`[V, S]` vs `[S, V]`). Pre-fix: the `[V, S]` log drops `V` permanently. Post-fix: both logs yield byte-identical projections with `V` applied. Assert both live folds *and* a restore (persist→load→re-fold) converge.
2. **Live out-of-order rebuild.** Apply `V` before `S` on a live log; assert `V` is absent immediately after `V` (dropped), then present immediately after `S` arrives (rebuild fired) — no restart.
3. **In-order fast path untouched.** Apply `S` then `V` in order; assert no rebuild occurs (e.g. an instrumentation counter or by asserting `max_applied` monotonic with no reset) and the result matches.
4. **Rebuild preserves the overlay.** After a rebuild, assert `meta` lifecycle / `tier2_timing` / `tier3_community_epoch` are unchanged (not reset).
5. **Verdict cache.** An se-mode `rb` (or `ts`) event applied, then a forced rebuild: assert the projection is unchanged and the expensive verify is not re-run (cache hit); assert a *forged* proof is still rejected (only successful verdicts cached).
6. **5-cap determinism.** Six statements from one author delivered in two different orders converge to the same 5 (HLC-earliest).
7. **Order-invariance regression sweep.** For each non-`dv` kind, fold a representative event set in ≥2 delivery orders and assert identical projections (guards the audit contract).
8. **Byzantine-backdated `dv`.** A `dv` stamped *earlier* than its `ds` stays dropped under canonical order (correct rejection), and does not spuriously trigger perpetual rebuilds.

## Out of scope / follow-ups (already filed)

- **ZEB-861** — unbounded, member-controlled `device_id` lane map / no per-member event-volume cap. Bounds the rebuild's O(n); tracked separately.
- **ZEB-867** — `kd=rs` verify/apply TOCTOU (pu-mode stale result).
- **Backfill re-request wiring** — the existing ZEB-718 gap-fill is sufficient; late arrivals are handled by the live rebuild. No new backfill work here.

## Global constraints

- Canonical order key is exactly `(wall_ms, logical, device_id, event_hash)` everywhere it is used (restore sort, live rebuild sort, out-of-order detection) — one shared helper, no divergent keys.
- Membership is resolved at **each event's own HLC** on every fold path (restore and rebuild). Never add an anti-backdating guard; containment is epoch-encryption's job (per ZEB-717).
- The apply-verdict cache is ephemeral, bounded, and never persisted / `notify_dirty` / replicated. Only successful verifications are cached.
- Rebuild resets only replay-derived fields; the `poll_restore` overlay is preserved.
- Rebuild is monotone-additive: it must never drop an event that applied under arrival order.
- CI parity before every push: `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures --no-fail-fast`.
