# ZEB-850 T-VOTE-LANE — per-(actor,device) tier-3 watermark + peer-ingest authz + engine clamp (design)

**Ticket:** ZEB-850 (ZEB-831 wall-clock threat model §4 HIGH, §6.3).
**Branch:** `zeblith/zeb-850-t-vote-lane` off `main@cff98226`.
**Status:** design approved 2026-08-02 (bundle all three parts: E2 watermark re-key + peer-ingest authz enforcement + E1 clamp test).

## Scope decision (recorded)

The ticket framed the "apply-path authz gap" as a rider on the E2 clock fix.
Recon showed it is larger and a **different threat class** (an authorization
bypass, not wall-clock): `verify_ss/verify_sd/verify_sf/verify_sr/verify_ratification_ballot`
have **zero production callers**, so several peer-admissible tier-3 kinds are
forgeable by any community member. Jake chose to **bundle all three** parts into
ZEB-850 rather than split. This doc covers all three.

---

## Part A — E2: per-(actor,device) watermark re-key

### Problem (confirmed against source)

`Tier3PollState.last_received_hlc: Option<Hlc>` (`community_voting_tier3.rs:222`)
is a **single global per-poll** watermark. It advances on **every** `apply_event`
`Ok` return — accepted *or* silently dropped (`:1052`, ZEB-320 Option B) — and is
read by the monotonic-HLC guard at the top of `apply_event` (`:457-467`), which
rejects any incoming event whose `(wall_ms, logical, device_id)` tuple sorts below
it as `HlcNotMonotonic`.

Because the watermark is global, **one** event stamped `now + 1h` (from a skewed
or malicious sibling — it need not even be *accepted*, a silent drop still advances
the watermark) pushes the global floor an hour ahead. Every subsequent honest event
from **any** member (stamped `now`) then sorts below the floor and is rejected for
the skew duration, on **every** replica, **durable across restart** (the persisted
`events` vec replays the poisoned watermark). GRIEF-LOCKOUT: one peer freezes the
whole poll's mini-public.

The forward-skew ingest bound (ZEB-846 T-GOV) shrinks the adoption floor but does
not close this — the attacker sets the wall directly, and the griefing works even
with an *accepted* far-future event.

### Approach (chosen): mirror the proven ZEB-585 per-peer lane

Re-key the watermark from a single scalar to a per-`(actor, device_id)` map, exactly
mirroring the channel-log catch-up lane (`community_channel_log.rs:799`, ZEB-585 —
the proven model):

```rust
// was: pub last_received_hlc: Option<Hlc>,
pub last_received_hlc: BTreeMap<(OwnerAddr, String), (u64, u32)>,
```

Key = `(actor = OwnerAddr, device_id = String)`; value = `(wall_ms, logical)`.
`device_id` is constant within a lane, so dropping it from the *compared* tuple is
equivalent to the current 3-tuple compare.

- **Monotonic guard** (`:457`): look up the lane for `(ev.actor, ev.hlc.device_id)`;
  if present and `(ev.hlc.wall_ms, ev.hlc.logical) < lane_val`, return
  `HlcNotMonotonic`. Per-lane — a future event on device A's lane no longer blocks
  device B's honest events.
- **Write** (`:1052`): raise the `(actor, device_id)` lane to
  `max(existing, (wall_ms, logical))` — mirror `raise_watermark`
  (`community_channel_log.rs:1008`). Still advances on every `Ok` (accept or drop),
  preserving the ZEB-320 property *within the lane*.

### Consumers to fix (would break under the type change)

Recon identified the non-test readers of `last_received_hlc`:

1. `community_voting_log_engine.rs:1246` — kd=rs **pu-mode** mint floor
   (`t3.last_received_hlc.clone()`), expects a single global max HLC.
2. `community_voting_log_engine.rs:1725` — kd=rs **se-mode** mint floor, same.

Add a helper `Tier3PollState::max_received_hlc() -> Option<Hlc>` returning the
`Hlc` at the max `(wall_ms, logical)` over all lanes (its `device_id` is empty —
the lane's device is irrelevant to the mint floor); the two mint-floor sites rebuild their
floor from it (the engine mints with its **own** `device_id`, so the lane's
`device_id` is irrelevant to the floor). `last_hlc` (the accepted-only projection
watermark) is genuinely a single global value and is **not** changed.

### Persistence: no migration

`Tier3PollState` is **never serialized** (`community_voting_persist.rs:8-16, 52-54`
— only `events` is persisted; materialized poll state is rebuilt by replaying events
through `apply_with_snapshot`). The field-type change is a pure in-memory change with
no on-disk migration and no `poll_restore` overlay entry.

### Second-order correctness: ZEB-320 / Qodo #154 preserved

The global guard was added (ZEB-320 / Qodo PR #154, documented at `:214-221`) so that
after a silent drop, an out-of-order **earlier** event that would change a drop
decision is surfaced as `HlcNotMonotonic` rather than silently applied inconsistently.
Per-lane keying **preserves** this within a lane: a device's own out-of-order delivery
is still caught. Across devices, events are **concurrent** by HLC partial order (no
causal ordering requirement between distinct devices), and replicas converge because
replay is deterministic over the fixed persisted `events` order — the global guard was
simply *over-strict*, which is the root of the griefing. A dedicated regression test
pins that the #154 same-lane scenario still trips the guard while a cross-lane future
event does not stall another lane.

---

## Part B — peer-ingest authorization enforcement

### Problem (confirmed against source)

The five authz verifiers have **zero production callers** — every invocation is under
`#[cfg(test)]`. The two peer-admission routes — `process_inbound`
(`community_voting_log_engine.rs:2770`, live gossip) and `apply_backfilled_event`
(`:2899`, backfill pull) — gate only on Ed25519 signature + community membership
(`verify_voting_event`) and then `inbound_eligibility_check` (`:3193`), whose
`Tier::Sortition` arm **deliberately no-ops** for `ss/sf/rs/md/dc/da/rb`
(`:3341-3353`) on a comment that is only true for `ds/dv`. `apply_event` is a pure
materialize layer and enforces no authz for these kinds. Result — **any community
member can forge**:

- `kd=sf` → sets `Stage::Failed`, **killing any poll** (`:714-718`).
- `kd=rs` → sets an **arbitrary result** + `Stage::Finalized` (`:1030-1035`).
- `kd=ss` → installs a **chosen mini-public** (`:482-489`); forged members then pass
  the `ds/dv` inline membership checks (amplifier).
- `kd=md/dc/da` → forge declines / candidates / approvals.
- `kd=rb` → ratification ballot skips the **B3 electorate-membership** check (crypto
  is checked inline, voter authz is not).

`ds/dv` already inline-check stage + `current_mini_public().contains(actor)`
(`:507-521`, `:585-611`) and need no additional gate.

### Approach: enforce the verifiers at `inbound_eligibility_check`

The correct location is the **admission seam**, not `apply_event` — `apply_event` must
stay a deterministic, I/O-free materialize layer (adding an async, oracle-dependent
check there would break replica convergence). `inbound_eligibility_check` already runs
before the apply lock on both peer routes and already has `voting_log` (so it can look
up the poll's `Tier3PollState` via `log.polls.get(&decode_poll_id_ref(payload)).and_then(as_tier3)`).
The local-origination and engine-auto mint paths stay trusted (self-signed; invariants
inlined at mint) — only the two peer routes get the gate.

**Sync verifiers (no oracle, no await) — run under the log guard:**

| kd | verifier | closes |
|----|----------|--------|
| `sf` | `verify_sf` | kill-any-poll |
| `md`, `dc` | `verify_sd` | forge decline / candidate |
| `da` | `verify_sd` + `verify_da_candidate_exists` | forge approval / dangling ref |
| `rb` | `verify_ratification_ballot` | ballot B3 electorate authz |
| `rs` | `verify_sr` | forge outcome (requires `close_event_hash` present) |

**Async verifier `kd=ss` → `verify_ss`:** needs a `&dyn BeaconOracle`. Thread a
beacon oracle into the seam from `self.dfrost_registry` (constructing a
`DfrostBeaconOracle`); `process_inbound` is a static free-fn, so it and
`inbound_eligibility_check` gain a `beacon_oracle` parameter passed from the
`&self` dispatch site (`process_inbound_dispatch`) and from `apply_backfilled_event`.

- **Lock discipline (ZEB-803 class):** `verify_ss` awaits `vrf_output_for`, which locks
  the *dfrost* log internally. Do **not** hold the `voting_log` guard across that await.
  Clone the needed `Tier3PollState` (or its verify-relevant fields) under the guard,
  **drop the guard**, then `await verify_ss` on the owned copy.
- **Fail-closed on `BeaconNotYetAvailable`:** `DfrostBeaconOracle` returns `None` until
  the VRF beacon is locally indexed, so a peer `kd=ss` racing ahead of the beacon can't
  be verified. Drop it (fail-closed). This is **liveness-safe**: `kd=ss` is engine-auto-
  derived by each node from the beacon (`publish_sortition_selection` from
  `on_dfrost_beacon`, `:753/:692`; idempotent per the `:4556` test), so a node that
  lacks the beacon mints the correct sortition itself once it indexes it — it never
  depends on a peer's gossiped `ss`. Fail-closed prevents a forged `ss` from setting
  `sortition_result` during the pre-beacon window while costing no liveness. If no
  beacon oracle is available at all (node without a dfrost registry), `kd=ss` is
  likewise dropped (that node is a sortition non-participant anyway).

### Rejection handling

Inbound rejections are logged-and-dropped (`:2760-2763`); a forged event is simply not
admitted. Honest events that are *temporarily* unverifiable (`kd=ss` pre-beacon, `kd=rs`
pre-close) are dropped too, but are re-derived locally (engine-auto) or re-pulled via
backfill once their precondition is met — no permanent loss.

---

## Part C — E1: engine-trigger clamp (verify + pin)

The ticket's E1 (the kd=cl auto-trigger reading `t3.last_hlc.wall_ms` as "now") is
**already mitigated by ZEB-846** (`community_voting_log_engine.rs:1123-1135`): the
trigger clamps `last_hlc.wall_ms` to `receiver_now + MAX_FORWARD_SKEW_MS` via
`clock_trust::clamp_future` before `current_stage_at`, and the kd=rs trigger does not
feed `current_stage_at` at all. Scope here is a **discrimination test** pinning that
clamp (a future-poisoned `last_hlc` must not advance the auto-computed stage past the
control tier), so a regression that removes the clamp fails. No behavior change unless
review surfaces a residual (e.g. a stricter `min(peer, local)` for the timer "now" —
noted as possible, not planned).

---

## Tests (discrimination, revert-sensitive)

Each must fail with the corresponding gate neutralized:

- **A / lane isolation:** device A posts `now+1h`; device B posts `now`. B's event
  applies (its lane is clean); a second B event still applies. Under the old global
  watermark B would be `HlcNotMonotonic`.
- **A / within-lane monotonic preserved (#154):** same `(actor, device)` posts `t=10`
  then `t=5` → the `t=5` event trips `HlcNotMonotonic` (the ZEB-320/#154 property still
  holds per lane).
- **A / mint-floor over lanes:** kd=rs mint floor equals the max `(wall_ms, logical)`
  across lanes (not a single lane), so the minted result HLC clears every lane.
- **B / verify_sf:** a non-proposer peer `kd=sf` is rejected at ingest (poll not
  Failed); the legitimate proposer `kd=sf` with an exhausted pool is admitted.
- **B / verify_sd:** a non-mini-public peer `kd=md`/`kd=dc` is rejected; a member's is
  admitted.
- **B / verify_sr:** a peer `kd=rs` with a mismatched tally is rejected
  (`TallyMismatch`); one matching the recompute is admitted; a `kd=rs` before `kd=cl`
  is rejected (`NotInClosedStage`).
- **B / verify_ratification_ballot B3:** a non-electorate peer `kd=rb` is rejected.
- **B / verify_ss fail-closed:** a forged `kd=ss` (mismatched sortition) is rejected
  when the beacon is available (`SortitionMismatch`); a `kd=ss` arriving before the
  beacon is indexed is dropped (fail-closed) and does **not** set `sortition_result`.
- **B / no-lock-across-await:** `verify_ss` is awaited without the `voting_log` guard
  held (structural — asserted by the code path; a smoke test that ingest + a concurrent
  dfrost read do not deadlock).
- **C / E1 clamp:** a future-poisoned `last_hlc` does not push the kd=cl auto-trigger
  stage past `receiver_now + MAX_FORWARD_SKEW_MS`.

## Non-goals

- **Not** changing `last_hlc` (correctly a global accepted-projection watermark).
- **No** wire/schema change (all changes are in-memory state + ingest gating).
- **No** retry/requeue machinery for temporarily-unverifiable events beyond the
  existing backfill re-pull + engine-auto re-derivation (sufficient for `ss`/`rs`).
- **Already-persisted** poison watermark from before this fix is healed on the next
  restart (replay reconstructs the per-lane map from `events`).

## Files

- `src-tauri/src/community_voting_tier3.rs` — `last_received_hlc` type; per-lane guard
  + write; `max_received_hlc()` helper.
- `src-tauri/src/community_voting_log_engine.rs` — `inbound_eligibility_check` per-kind
  verify gate; thread `beacon_oracle` into `process_inbound` / the seam;
  `apply_backfilled_event` gate; fix the two kd=rs mint-floor consumers; E1 test.
- Tests in the respective files' `#[cfg(test)] mod tests`.
