# ZEB-846 T-GOV: bounded forward-skew on governance `event.at.wall_ms`

**Status:** design approved (2026-08-01). Successor to ZEB-831 (wall-clock threat
model, `docs/superpowers/specs/2026-08-01-zeb-831-wall-clock-threat-model.md` §6.1)
— the single highest-leverage fix it enumerated.

**Goal:** A single participant's skewed or malicious wall clock must not be able to
bypass a governance control or poison event ordering for other participants. Bound
every peer-supplied `event.at.wall_ms` that gates a membership / voting / channel
control to a plausible forward window of the *receiver's own* clock, at both the
admission boundary (reject) and the materialize boundary (defer / clamp).

**Non-goals (explicit):**

- **No backward / anti-backdating bound.** Membership is verified at the event's
  *own* HLC; backdating containment is epoch-encryption (ZEB-717), not an ingest
  guard. This spec adds a forward (too-far-*future*) bound only — the opposite
  direction. Do not add a "too old / backdated" reject anywhere in this work.
- **HLC remote-merge** (ZEB-790) is separate and already shipped for the adoption
  floor; this spec does not touch HLC reconciliation.
- The other ZEB-831 defense tickets (ZEB-847…854) are out of scope; this is its
  own focused PR (per the ticket), not bundled.
- **Signed / attested time** is out of scope. The 5-min tier assumes a roughly
  NTP-synced receiver clock; a node whose own clock is off by more than the
  tolerance is degraded, and the `now=None` fallback (below) keeps that degradation
  non-destructive rather than making it fatal.

---

## 1. Background: one root, eight symptoms

Every finding below has the same root cause: **no membership, voting, or channel
verify arm bounds `event.at.wall_ms`, and `materialize`/replay trusts admission to
have bounded it — which it never did.** `wall_ms` is the *primary* key of
`event_sort_key` (`community_membership.rs:2250`), so a future-dated event sorts
*last* and dominates every ordering-based control. Governance events are persisted
as the raw signed log inside `CommunityState` CBOR (`communities/{id}/crdt.cbor`);
`load_crdt` (`community_state_persist.rs:89`) decodes them verbatim and
`materialize` is pure-and-trusting by contract (`community_membership.rs:2279`), so
**a future-dated event that survives one admission is replayed, unbounded, on every
subsequent boot.** Verification runs only on live ingest, never on reload.

| # | Finding | Site | Mechanism |
|---|---|---|---|
| A1 | recovery-veto silently discarded | `community_membership.rs:2482` | future `RecoveryProposal` `t0` makes honest veto fail `*wall >= t0`. Sole non-quorum control on a recovery takeover — FAIL-OPEN. |
| — | `event_sort_key` poison | `:2250` | future `Kick`/`SetPower` sorts last, wins LWW/"latest admin"; `materialize` never re-verifies. POISON-SQUAT. |
| A2/A3 | admin-proposal / recovery-init expiry never fires | `:3538`, `:2464`/`:2475` | `saturating_sub` of a future wall = age 0 ⇒ never expires. Completes the ZEB-792 regression (planner bounded at `:5932`, apply path unbounded). |
| A4/A6 | pending-join expiry / community-wide drop | `:3326`, `:2593` | one future event raises `events_max_wall_ms` (the `max` aging floor) for the whole community. |
| CD | future `deleted_at` stops gating writes | `community_channel_log.rs:1526` | honest posts are not `strictly_newer_than` a far-future `deleted_at`, so the deletion never actually stops writes. |
| E1 | instant poll finalize | `community_voting_log_engine.rs:1108` → `community_voting_tier3.rs:1068` | `current_stage_at` uses `last_hlc.wall_ms` (peer-supplied) as "now"; a future event jumps straight to Ratification / auto-close. |
| RR | recovery-rotation finality | `:3113` | a rotation stamped 48h ahead jumps past `deadline + RECOVERY_ROTATION_FINALITY_MS` and kicks the admin early. |

---

## 2. Clock-trust model

- **One constant, control tier:** `clock_trust::MAX_FORWARD_SKEW_MS` (5 min,
  milliseconds). Every bound in this work uses it and only it, so the admission and
  materialize layers can never disagree. Governance's existing ad-hoc 30-min
  `ADMIN_PROPOSAL_MAX_FORWARD_SKEW_MS` (`community_membership.rs:5513`, used only at
  the planner filter `:5932`) is **unified onto** `MAX_FORWARD_SKEW_MS`; the
  `clock_trust` pin test already asserts `MAX_FORWARD_SKEW_MS ≤
  ADMIN_PROPOSAL_MAX_FORWARD_SKEW_MS` in anticipation. See §6.
- **Helper:** `clock_trust::reject_future(stamp, now, MAX_FORWARD_SKEW_MS) -> bool`
  (inclusive boundary: `stamp == now + tol` is accepted); `clamp_future(stamp, now,
  tol) -> u64 = stamp.min(now + tol)`. All milliseconds.
- **Receiver-`now` source:** the real local clock at each boundary —
  `std::time::SystemTime::now()` or the sync context's `HlcAdoptFloor` merged-now,
  both already present at or one frame above each ingest site (map §6).
- **`now = None` fallback (load-bearing safety property):** when no trustworthy
  receiver clock is available, the bound is **disabled** (apply / accept
  everything). This is the P1 lesson (`reference_forward_skew_gate_view_not_store`):
  a forward bound assumes the receiver's clock is good, so a *bad local* clock must
  never be allowed to drop or defer honest governance. Deferral and rejection are
  the response to an implausibly-future *peer* stamp measured against a *trusted*
  local now — never a blanket action taken under an untrusted local now.

---

## 3. Architecture: two layers, one constant

### Layer 1 — Admission reject (defense-in-depth)

At each live-ingest verify boundary, reject an event whose wall is beyond the
forward window:

```
if let Some(now) = receiver_now_ms {
    if clock_trust::reject_future(event_wall_ms, now, clock_trust::MAX_FORWARD_SKEW_MS) {
        return Err(/* future-skew reject */);
    }
}
```

- Membership: `verify_event` (`community_membership.rs:3979`).
- Voting: `CommunityVotingLogEngine::process_inbound` (`community_voting_log_engine.rs:2757`),
  before `apply_with_snapshot`.
- Channel: `verify_channel_event` (`community_channel_log.rs:1343`) — bound the
  `ChannelDelete` config event's wall (and the post's `at.wall_ms`).

Purpose: keep *new* poison out of the persisted CBOR log and stop this node from
re-gossiping it onward. It is **not** sufficient alone — reload never re-runs these
(map §2), and an event admitted before this ships is already persisted.

### Layer 2 — Materialize defer / clamp (load-bearing)

Re-evaluated against the live receiver-`now` on every materialize, including after
reload. This is the durable, reload-safe, slow-clock-safe bound. Two shapes,
chosen per finding by the rule **"a future event must not gain power"**:

- **Defer (skip)** where *absence is safe*: the future event is simply not applied
  while its wall is beyond `now + 5min`, and applies once `now` catches up.
  Membership ordering/expiry findings (A1, sort-key, A2/A3, A4/A6, RR). Skipping a
  future `Kick` / `SetPower` / `RecoveryProposal` / pending-join / rotation
  neutralizes it and keeps `events_max_wall_ms` honest.
- **Clamp** where *absence is the vulnerability*: skipping would remove a control
  that should still fire. Bound the effective quantity down to `now + 5min`:
  - **E1** (voting): clamp the effective "now" used by `current_stage_at` — i.e.
    `min(last_hlc.wall_ms, receiver_now + 5min)` — so a future event cannot jump the
    poll's stage. Skipping the event instead would just drop a vote.
  - **CD** (channel): clamp the effective `deleted_at` used by the
    `strictly_newer_than` gate (`community_channel_log.rs:1523`) down to
    `now + 5min`, so a far-future deletion still takes effect at a plausible time
    and gates subsequent posts. **Skipping the `ChannelDelete` would un-delete the
    channel — the exact bug — so defer is wrong here.**

When `receiver_now_ms` is `None`, Layer 2 applies no skip/clamp (apply-all), per §2.

---

## 4. Per-subsystem design

### 4.1 Membership (`community_membership.rs`)

**Admission.** Add `now_ms: Option<u64>` to `VerifyContext` (`:3938`). Thread it
from the live merge caller in `community_state_sync.rs` (which already holds
`SystemTime::now()` / `ctx.adopt_floor`, e.g. `:3365`, `:4474`) through
`CommunityState::insert_event` → `MembershipPolicy::verify` → `verify_event`. In
`verify_event`, add the forward reject immediately after the community-binding guard
(`:3995`), before crypto. `None` ⇒ no reject (fork-veto pre-validate paths and any
caller without a clock keep working).

**Materialize.** `materialize_with_now` (`:2573`) already takes
`now_ms: Option<u64>`, but production passes the candidate's *own* wall as an aging
*floor* (`community_state_crdt.rs:534`). Add a **distinct** receiver-`now` for the
forward *ceiling* — do not overload the aging floor, whose semantics must not shift.
Concretely: extend `materialize_with_now` to also carry a `receiver_now_ms:
Option<u64>` (name TBD in plan), and in the event-walking loop (`:2757`) `continue`
past any event with `reject_future(event.at.wall_ms, receiver_now, MAX_FORWARD_SKEW_MS)`
when `receiver_now` is `Some`. Skipped events are excluded from `sorted`, from
`events_max_wall_ms`, and from every downstream control (A1 veto, expiry, RR).
Thread a real receiver-`now` from the security-critical callers (state read for
control decisions, sync merge); the `materialize()` wrapper (`:2284`) stays
`None` → apply-all for callers that legitimately have no clock.

**30-min unification.** Replace `ADMIN_PROPOSAL_MAX_FORWARD_SKEW_MS` at the planner
filter (`:5932`) with `clock_trust::MAX_FORWARD_SKEW_MS`. Retire the local constant
(`:5513`) or redefine it as `pub const … = clock_trust::MAX_FORWARD_SKEW_MS` for one
release if external references exist; update the planner tests (`:7181`, `:7193`,
`:7215`) to the 5-min boundary.

### 4.2 Voting (`community_voting_log_engine.rs`, `community_voting_tier3.rs`)

**Admission.** In `process_inbound` (`:2757`), obtain receiver-`now`
(`SystemTime::now()`, already used at `:597`/`:635`/`:812`) and reject any event
with `reject_future(event.hlc.wall_ms, now, MAX_FORWARD_SKEW_MS)` before
`apply_with_snapshot`. `verify_voting_event` stays timestamp-free (V6 + Ed25519); the
bound is an engine-level admission arm, consistent with the adoption-floor observe
at `:2823`.

**Materialize / consumption (E1).** In `maybe_trigger_engine_auto_orchestration`
(`:1108`), the "now" fed to `current_stage_at` is `t3.last_hlc.wall_ms`. Clamp it:
`let stage_now_wall = clamp_future(last_wall, receiver_now, MAX_FORWARD_SKEW_MS)`
when receiver-`now` is available, so a future accepted event cannot advance the poll
stage. `current_stage_at` (`community_voting_tier3.rs:1068`) itself stays pure — the
clamp is applied to the `now` argument at the trigger site (and any other consumer
that derives a stage from `last_hlc`).

### 4.3 Channel log (`community_channel_log.rs`) (CD)

**Admission.** Add `now_ms: Option<u64>` to `verify_channel_event` (`:1343`); reject
a `ChannelDelete` (and any config event) whose `wall_ms` is beyond the window, and
reject a post whose `at.wall_ms` is beyond the window. Thread the engine's
`SystemTime`-derived now from the caller.

**Materialize / consumption.** At the tombstone gate (`:1523`), clamp the effective
`deleted_at` down to `receiver_now + 5min` before the `strictly_newer_than`
comparison, so a pre-gate persisted far-future deletion still gates honest posts.
`None` ⇒ use `deleted_at` unclamped (apply-all fallback).

---

## 5. Testing

Governance-critical; every closed finding gets a discrimination test, plus the
cross-cutting reload and clock-safety tests. All in the seconds/ms domain matching
each site.

1. **Discrimination test per finding** (A1, sort-key, A2/A3, A4/A6, RR, E1, CD): a
   poisoned event whose wall is beyond `now + 5min` **and** higher than a legit
   event's wall. Assert (a) admission **rejects** it with a visible error, and (b)
   when force-persisted (bypassing admission, to model a pre-gate frame), the
   materialize layer **defers/clamps** so the control still resolves correctly
   (honest veto counts; Kick doesn't dominate; expiry fires; poll stays in its real
   stage; deleted channel gates posts).
2. **Restart/replay test:** persist a poison frame directly into `crdt.cbor`
   (bypassing `verify_event`), `load_crdt`, then materialize with a real receiver-
   `now` → the bound still holds (poison not applied / clamped). This is the test
   the ticket calls out — proof the fix does not depend on admission having run.
3. **`now = None` fallback test:** materialize / verify with no receiver clock →
   apply-all; honest events are neither dropped nor deferred.
4. **Slow-local-clock test:** receiver-`now` set behind real time so honest events
   look future-dated → they are **deferred, not dropped**; advancing `now` past
   their walls makes them apply. Proves the non-destructive property.
5. **Cross-node convergence note (test or reasoning):** honest nodes with correct
   clocks all defer the same far-future poison, so they agree; a node whose *own*
   clock is far-future is itself degraded. Document that materialized governance
   state is (already, via expiry) receiver-clock-relative and converges once `now`
   passes the walls.

---

## 6. Global constraints

- Cargo commands run from `src-tauri/`. CI gates: `cargo fmt --all -- --check`;
  `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D
  warnings`; `cargo nextest run --locked --workspace --all-targets --features
  test-fixtures`.
- One constant only: `clock_trust::MAX_FORWARD_SKEW_MS` (5 min, ms). No new skew
  constant; the 30-min governance constant is unified onto it.
- Units are milliseconds throughout (membership/voting/channel walls are all ms).
- `now = None` ⇒ bound disabled at every layer (non-destructive local-clock floor).
- No backward/anti-backdating bound anywhere (Non-goals).
- Own focused PR; not bundled with other ZEB-831 tickets.
