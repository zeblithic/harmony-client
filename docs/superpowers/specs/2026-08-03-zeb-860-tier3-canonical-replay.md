# ZEB-860 — Tier-3 canonical-order projection materialization

**Goal:** Make a Tier-3 poll's materialized `Tier3PollState` a deterministic function of the *set* of applied events — folded in canonical HLC order — so every replica converges on identical poll state, live and after restore, regardless of event delivery order.

**Architecture:** Detect out-of-order arrival of the order-dependent Deliberation event family and rebuild that one poll's projection by re-folding its events in canonical order `(wall_ms, logical, device_id, event_hash)`. In-order arrivals keep today's incremental fast path. The rebuild is **synchronous** and lives entirely in the apply layer (`VotingLog::apply_with_snapshot`'s Tier-3 branch), so every caller — live inbound, local publish, backfill, and boot restore — inherits convergence through the same seam.

**Tech stack:** Rust (`src-tauri/`), the community-voting subsystem (`community_voting_tier3.rs`, `community_voting_log.rs`).

---

## Background — the verified bug (on `main` @ 9076f03d)

Tier-3 poll projections (`Tier3PollState`) are **not persisted**. `PersistedVotingLog` stores `events: Vec<SignedVotingEvent>` + `policy` + a `poll_restore` overlay; the materialized projection is re-folded on boot by replaying `events` through `VotingLog::apply_with_snapshot` (`reconcile_voting_from_state`), **in append (delivery) order, with no HLC sort**.

Several apply rules **silently drop** an event — return `Ok`, leave `advance_last_hlc = false` — when a prerequisite is missing, yet the event is **still appended** to the log (the append happens after `apply_event` returns `Ok`). Most cross-lane prerequisites are gated a second time at ingest (`inbound_eligibility_check`), which returns `Err` *before* the append, so those events are never logged out of order and their replay is order-safe: `da→dc` (`verify_da_candidate_exists`), `rs→close` (`verify_sr`), `rb→candidates` (`verify_ratification_ballot`).

The gap is the three **ingest-ungated** kinds — `ds` / `dv` / `ts`. The sharp instance is **`kd=dv`** (DeliberationVote): it references a specific statement (`kd=ds`) by `statement_event_hash` and is dropped when that statement is not yet present. Because you cannot vote on your own statement (`self_vote` is dropped), `dv.actor ≠ ds.author` always — the dependency is inherently **cross-lane**, so the per-`(actor,device)` receive-watermark cannot order the two. A `dv` delivered before its `ds` is dropped, still appended, and **never re-materialized** when the `ds` later arrives. Result: two honest replicas that saw the events in different orders hold permanently divergent tallies, persistent across restart. (`kd=ds` shares the class via its dependence on `ss`/`md` for stage and mini-public membership.)

### Why canonical order is both safe and *more* correct

Every apply rule other than the Deliberation family's accept/drop decision is already order-independent: idempotent set-inserts keyed by `event_hash` (statements, candidates, approvals, decline lists) or last-writer-wins resolved **by HLC**, not by arrival (`dv` value, `ts` upsert, `ss` overwrite). So folding in HLC order changes nothing for the convergent rules and *fixes* the divergent ones.

It is also causally sound: `dv` references `ds` by hash, so an honest `ds` was authored before its `dv` ⇒ `ds.hlc < dv.hlc` ⇒ in canonical order the statement precedes the vote and the vote applies. A Byzantine `dv` stamped *earlier* than its statement stays dropped under canonical order — correct, because voting on a statement that did not yet exist is invalid. Sorting also makes the `ds` per-author 5-statement cap deterministic (keeps the HLC-earliest 5). Nothing regresses.

## The invariant

> For the set `E` of events applied to a **non-terminal** poll, `Tier3PollState = fold(sort_canonical(E))`, where `sort_canonical` orders by `(wall_ms, logical, device_id, event_hash)` — identical on every replica, evaluated the same live and after restore.

Corollary the implementation must preserve: **rebuild is monotone-additive** — re-folding in canonical order never *removes* an event that legitimately applied; it only re-materializes events that arrival order unfairly dropped (and correctly drops the Byzantine-backdated ones).

## Design

### 1. Canonical order key

A helper `canonical_key(ev) -> (u64, u32, String, [u8;32])` = `(hlc.wall_ms, hlc.logical, hlc.device_id, sha256_of_signing_bytes(ev))`. `event_hash` is the final tiebreaker (already the `dv`/`ts` LWW tiebreak), giving a strict total order with no ties even if a device reuses an HLC.

### 2. `apply_event` reports its outcome

`Tier3PollState::apply_event` returns `Result<ApplyOutcome, ApplyError>` where `ApplyOutcome ∈ { Applied, Dropped }` (`Applied` = the event changed the projection, i.e. today's `advance_last_hlc == true`; `Dropped` = a silent-drop branch). Existing `?`/`.unwrap()` callers that ignore the `Ok` value are unaffected. This lets the orchestrator distinguish an accepted event (which can retroactively change outcomes) from a silently-dropped one (which cannot), without inferring it from `last_hlc` movement.

### 3. Out-of-order detection

Add `max_applied: Option<(u64, u32, String)>` to `Tier3PollState` (the `(wall_ms, logical, device_id)` prefix of the canonical key). `apply_event` advances it at the tail on **every** `Ok` (accept OR silent-drop), beside the existing `last_received_hlc` advance — a dropped event still occupies a canonical position. Reset to `None` in `new_from_create`; never serialized (nothing on `Tier3PollState` is).

An arriving event is **out-of-order** iff its `(wall_ms, logical, device_id)` `≤ max_applied` — O(1).

### 4. Rebuild trigger (scoped, in `apply_with_snapshot`'s Tier-3 branch)

After the existing `apply_event` call and lifecycle sync and append, fire a rebuild iff **all** hold:

1. the event was **out-of-order** (its key `≤` the poll's `max_applied` *captured before* this apply), and
2. `apply_event` returned **`Applied`** (a silently-dropped event introduces no prerequisite and cannot change another event's outcome — this also denies an outsider spamming dropped `dv`s any rebuild leverage), and
3. the event kind is in the **order-dependent Deliberation family `{ss, md, ds, dv}`** — the only kinds whose acceptance can retroactively change another event's accept/drop outcome (`ss`/`md` gate stage & mini-public; `ds` is the vote prerequisite; `dv` covers the Byzantine-backdated case where an incremental accept must be re-dropped).

The trigger kinds' accepted volume is bounded by sortition parameters (mini-public ≤ 300, `ds` ≤ 5/author), so rebuilds are bounded — no memoization needed. `ds`/`dv` **self-gate** to Deliberation (dropped elsewhere ⇒ not `Applied` ⇒ never trigger), so their rebuilds are crypto-free. `ss`/`md` have **no** apply-time stage gate, so a backdated `ss`/`md` arriving during Ratification *can* fire a rebuild that re-runs `rb`/`ts` crypto — bounded, insider-only, ZEB-846-capped, and **non-divergent** (the rebuild reproduces what restore computes); tightening the trigger to `current_stage_at(event.hlc) < Ratification` for `ss`/`md` is tracked as **ZEB-868**. Order-independent kinds (`rb`, `ts`, `da`, `dc`, `rs`, `cl`) arriving out of order need no rebuild; their incremental apply is already canonical.

### 5. Rebuild = a synchronous mini-restore scoped to one poll

`Tier3PollState::rebuild_from_events(&mut self, events: &[SignedVotingEvent])`:

1. **Preserve** the non-replay-derived state: `meta` (holds `community_epoch`, patched post-create by `set_tier3_poll_epoch`; also poll config / `poll_create_hlc`), `eligible_electorate_snapshot`, and `committee_oracle` (installed by the engine; `new_from_create` defaults it to `NullCommitteeOracle`).
2. `*self = Tier3PollState::new_from_create(meta, electorate)` then re-install `committee_oracle`. Using the canonical constructor as the reset means a future field added to `new_from_create` is reset correctly by construction; only externally-installed `committee_oracle` needs the explicit re-install.
3. Re-fold `events`, canonically sorted, through `apply_event` (each `Err` is a terminal/monotonic rejection, ignored exactly as replay does). The poll's events come straight from `PollState.events` (each `PollState` already owns its per-poll event Vec), so no filtering of the global log is needed. Folding in canonical order means each lane's events re-apply in ascending HLC, so the per-lane monotonic guard never spuriously rejects.

The orchestrator (Tier-3 branch) re-runs the existing `stage → PollState.meta.lifecycle` sync after the rebuild. Rebuilds only fire on non-terminal polls (a terminal poll rejects the trigger event at `apply_event`, so it is never appended and never triggers), so the outer lifecycle stays `Open` across a rebuild.

Restore (`reconcile_voting_from_state`) needs **no change**: it replays through `apply_with_snapshot`, so out-of-order Deliberation events trigger the same rebuild during boot, converging each poll to the canonical projection. (Bounded by the same sortition caps; the `poll_restore` overlay is applied *after* replay, so it still lands on top.)

## Order-invariance obligations (the acceptance contract, verified by tests)

- **Set-insert, idempotent** (`ds` statements, `dc` candidates, `da` approvals, `md` declines): keyed by `event_hash`; canonical order does not change the set. (The `candidates` Vec is stored in arrival order and is *not* re-canonicalized, but its order is **non-observable**: the tally indexes ballots against `ratification_candidates_ordering` — a re-sort by `(approval_count DESC, event_hash ASC)` — and the projection is never serialized, so only the set/approvals matter.)
- **LWW by HLC** (`dv` value once accepted, `ts` upsert, `ss` overwrite): resolution already uses the HLC key; canonical order is consistent with it.
- **Same-actor accumulation** (`ds` 5-statement cap): becomes deterministic (HLC-earliest 5) — an intended improvement.
- **The fix** (`ss`/`md`/`ds`/`dv` accept/drop): the family whose accepted set changes under canonical order — the divergence closes.
- **Order-independent, no rebuild** (`rb`, `ts`, `da`, `dc`): incremental apply is already canonical; excluded from the trigger.

## Watermark-poison amplification (bounded by ZEB-846 — accepted)

`max_applied` advances on **every** dispatch including silent drops (load-bearing: a dropped `dv(4000)` must raise the watermark so a later `ds(3000)` is seen as out-of-order and re-materializes it). A consequence: a *dropped* trigger-kind event with a high `wall_ms` lifts `max_applied` above the applied frontier, so subsequent honest trigger-kind events look out-of-order and rebuild — an insider rebuild-amplification (bounded by the poll's event count, crypto-free). This is **accepted, not fixed**, because it is already bounded: both peer-facing ingest paths (`process_inbound`, `apply_backfilled_event`) reject `wall_ms > receiver_now + MAX_FORWARD_SKEW_MS` (ZEB-846, 5 min), so the poison cannot exceed `now + 5min` and self-heals as real time advances past it. Distinguishing a legitimate high-key dropped event from a malicious one is only possible on wall-clock plausibility — which is the ingest gate's job (ZEB-831 family), not the rebuild mechanism's. A precise fix (a dropped-`dv`-by-statement-hash index that rebuilds only on a genuine unblock) is possible but unwarranted for a bounded, self-healing, insider-only transient.

## Known residual (documented, out of scope)

Terminal events **`sf`/`rs`** (and `cl`'s first-close-wins hash) are order-dependent in the narrow sense that a Byzantine-backdated terminal event replayed in canonical order would drop post-finalize junk that arrival order kept. This is **benign and excluded**: the finalized `result` is a deterministic `verify_sr` recompute (identical on every replica regardless of order), so only non-result projection fields on an already-dead poll could differ — never the tally. `cl`'s `close_event_hash` divergence is already documented-tolerated (not in any cross-peer state root). Making terminal ordering canonical would force rebuilds during Ratification (re-running `rb`/`ts` crypto) and is not needed to fix ZEB-860.

## Testing strategy

1. **Divergence repro → fix (headline).** A poll in Deliberation with statement `S` (author A) and vote `V` on `S` (voter B, `V.hlc > S.hlc`). Fold two `VotingLog`s from the **same event set in opposite delivery orders** (`[V,S]` vs `[S,V]`); assert byte-identical projections with `V` applied in both. Pre-fix, `[V,S]` drops `V`.
2. **Live out-of-order rebuild, no restart.** Apply `V` before `S` on a live log; assert `V` absent right after `V` (dropped), present right after `S` (rebuild fired).
3. **In-order fast path untouched.** Apply `S` then `V`; assert no rebuild (a test-visible rebuild counter stays 0) and the result matches.
4. **Byzantine-backdated `dv`.** With `dv.hlc < ds.hlc`: order `[ds, dv]` accepts `dv` incrementally then the rebuild **drops** it; order `[dv, ds]` also drops it; both converge to dropped (canonical).
5. **Outsider dropped-`dv` triggers no rebuild.** A non-mini-public `dv` (silently dropped) delivered out of order does not fire a rebuild (counter stays 0) — the DoS guard.
6. **Rebuild preserves non-replay state.** After a rebuild assert `meta.community_epoch`, the installed `committee_oracle` behaviour, and `eligible_electorate_snapshot` are unchanged; outer `meta.lifecycle` correctly re-synced.
7. **5-cap determinism.** Six statements from one author in two delivery orders converge to the same HLC-earliest 5.
8. **Order-invariance regression sweep.** For each of `rb`/`ts`/`da`/`dc`, fold a representative set in ≥2 delivery orders and assert identical projections (and no rebuild fires for them).
9. **Restore convergence.** Persist a log whose append order drops a `dv`; reload via the reconcile path; assert the restored projection has `dv` applied (restore inherits the rebuild).

## Out of scope / follow-ups (already filed)

- **ZEB-861** — unbounded, member-controlled `device_id` lane map / no per-member event-volume cap. Bounds the log the rebuild folds; tracked separately.
- **ZEB-867** — `kd=rs` verify/apply TOCTOU (pu-mode stale result).
- Terminal-event (`sf`/`rs`) canonical ordering — benign residual above.

## Global constraints

- Canonical order key is exactly `(wall_ms, logical, device_id, event_hash)` — one shared `canonical_key` helper used by the rebuild sort; out-of-order detection uses its `(wall_ms, logical, device_id)` prefix (`max_applied`).
- The rebuild trigger is exactly: out-of-order **AND** `ApplyOutcome::Applied` **AND** kind `∈ {ss, md, ds, dv}`. No broader (DoS/perf) and no narrower (soundness).
- Rebuild resets only replay-derived fields via `new_from_create`; `meta`, `eligible_electorate_snapshot`, and `committee_oracle` are preserved. Rebuild is monotone-additive.
- Membership stays resolved from stored poll state at each event's own HLC (no external snapshot for Tier-3 apply); never add an anti-backdating guard (ZEB-717).
- Nothing on `Tier3PollState` (incl. `max_applied`) is serialized / `notify_dirty` / replicated.
- CI parity before every push: `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures --no-fail-fast`.
