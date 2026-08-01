# ZEB-846 Bounded Forward-Skew on Governance `event.at.wall_ms` — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A single participant's skewed or malicious wall clock must not bypass a governance control or poison event ordering for other participants — bound every peer-supplied `event.at.wall_ms` that gates a membership / voting / channel control to a plausible forward window of the *receiver's own* clock, at both admission (reject) and materialize (defer / clamp).

**Architecture:** Two layers sharing one constant (`clock_trust::MAX_FORWARD_SKEW_MS`, 5 min). **Layer 1 — admission reject** at each live-ingest verify boundary keeps new poison out of the persisted CBOR log and stops onward gossip (defense-in-depth). **Layer 2 — materialize defer/clamp** re-evaluated against the receiver's real clock on every read, including after reload; it is the load-bearing, reload-safe, slow-clock-safe bound. The receiver-`now` is always the node's own `SystemTime::now()` — never the peer-influenced `HlcAdoptFloor`, whose forward value the attacker we are bounding can move.

**Tech Stack:** Rust (workspace under `src-tauri/`); `clock_trust` module (`reject_future` / `clamp_future` / `MAX_FORWARD_SKEW_MS`); `harmony-crdt-sync` `VerifiedLog<MembershipPolicy>` engine; tokio async; `cargo nextest`.

## Global Constraints

- Cargo commands run **from `src-tauri/`**. CI gates, all three must pass:
  - `cargo fmt --all -- --check`
  - `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
  - `cargo nextest run --locked --workspace --all-targets --features test-fixtures`
- **One skew constant only:** `clock_trust::MAX_FORWARD_SKEW_MS` (`= 5 * 60 * 1000`, ms). No new skew constant. The 30-min `community_membership::ADMIN_PROPOSAL_MAX_FORWARD_SKEW_MS` is unified onto it (Task 4).
- **Units are milliseconds** throughout (membership/voting/channel walls are all `u64` ms).
- **`receiver_now = None` ⇒ bound disabled** at every layer (apply/accept everything). A bad *local* clock must never drop or defer honest governance. This is the P1 lesson (`reference_forward_skew_gate_view_not_store`).
- **Receiver-`now` is `std::time::SystemTime::now()` only** — never `HlcAdoptFloor`/`merged_now`/any peer-supplied wall.
- **No backward / anti-backdating bound anywhere.** Membership is verified at the event's own HLC; backdating containment is epoch-encryption (ZEB-717), out of scope. This work adds a forward (too-far-*future*) bound only.
- **`reject_future(stamp, now, tol)` is inclusive at the boundary** (`stamp == now + tol` is accepted; `> tol` rejected). `clamp_future(stamp, now, tol) = stamp.min(now.saturating_add(tol))`.
- Own focused PR; **not bundled** with other ZEB-831 tickets (ZEB-847…854).
- Frequent commits: each task ends with a commit. Use `scripts/test-select --context task` for iterative per-task gates; run the **full** `--workspace --all-targets` sweep before the PR.

---

## Design rationale locked during grounding (read before Task 1)

Three structural facts (verified against the pinned `harmony-crdt-sync` rev and the current tree) that shape the tasks:

1. **The CRDT engine caches no materialized state, and `VerifiedLog::from_verified_events` (the deserialize/reload path) never materializes and takes no clock.** On reload, materialization is deferred to whichever `CommunityState` read accessor is first called. So "the bound applies on reload" means **the bound lives in the read accessors**, not in a load hook.
2. **The security-critical reads** (admin / power / kick / ban / relay / presence gates — `community_membership::apply_auto_exec_*`, `voice_moderation`, `voice_presence`, `community_relay_prod`, `community_state_sync` local-insert authorization, several IPC reads) go through **`CommunityState::materialized(admin_addr)`**, which threads floor `None` via the 2-arg `community_membership::materialize`. Only the recovery reads use the receiver-clock-aware `materialized_with_now`. A bound placed only inside `materialize_with_now` would therefore miss exactly the reads that enforce authorization — the bound must reach the `materialized()` cached path.
3. **The forward ceiling only ever *excludes* events**, so it is exactly *"pre-filter the event slice, then materialize normally."* Excluding an event from the input slice is provably equivalent to skipping it in the sort/apply loop: it drops out of `event_sort_key` ordering (no POISON-SQUAT), out of `events_max_wall_ms` (aging floor stays honest), and out of every recovery/expiry control. This lets us add a thin `materialize_with_bounds` wrapper that pre-filters and delegates to the untouched `materialize_with_now` — **zero ripple to its 11 prod + ~44 test call sites**.

Because receiver-`now` is the node's own `SystemTime::now()` (not a value any caller must supply for correctness), the accessors compute it **internally** — collapsing what would be a ~15-call-site clock-threading ripple into a 3-accessor change.

---

## File Structure

| File | Responsibility in this change |
|---|---|
| `src-tauri/src/community_membership.rs` | Task 1: add `materialize_with_bounds` (forward-ceiling pre-filter) + unit tests. Task 3: `VerifyContext.now_ms` field + Layer-1 reject in `verify_event`. Task 4: unify `ADMIN_PROPOSAL_MAX_FORWARD_SKEW_MS` onto `clock_trust::MAX_FORWARD_SKEW_MS`. |
| `src-tauri/src/community_state_crdt.rs` | Task 2: route the three `CommunityState` read accessors through `materialize_with_bounds` with an internally-computed receiver-`now`. Task 3: pass `now_ms` when building `VerifyContext` in `insert_event`'s caller chain (field plumbing). |
| `src-tauri/src/community_state_sync.rs` | Task 3: set `VerifyContext.now_ms = Some(SystemTime::now())` at the live-merge site (`:4363`); `None` at the other constructors. |
| `src-tauri/src/community_state_persist.rs` | Task 2: (test only) reload-safety test persists a poison frame and calls `load_crdt`. No production change. |
| `src-tauri/src/community_voting_log_engine.rs` | Task 5: Layer-1 reject in `process_inbound`; Layer-2 clamp of stage-`now` in `maybe_trigger_engine_auto_orchestration`. |
| `src-tauri/src/community_channel_log.rs` | Task 6: add `verify_channel_event_at(.., now_ms)` (Layer-1 reject + Layer-2 `deleted_at` clamp); keep `verify_channel_event` as a real-clock delegate. |

No new files. No `clock_trust.rs` change (its constants/helpers already exist and are pinned).

---

## Task 1: `materialize_with_bounds` — the forward-ceiling primitive (membership Layer 2 core)

**Files:**
- Modify: `src-tauri/src/community_membership.rs` — add `materialize_with_bounds` immediately after the 2-arg `materialize` wrapper (after line 2293, before `materialize_with_now` at 2573).
- Test: `src-tauri/src/community_membership.rs` — new `#[cfg(test)] mod zeb846_forward_ceiling` at end of the test region.

**Interfaces:**
- Consumes: existing `materialize_with_now(events: &[SignedMembershipEvent], admin_addr: OwnerAddr, now_ms: Option<u64>) -> MaterializedMembership` (unchanged); `clock_trust::reject_future`, `clock_trust::MAX_FORWARD_SKEW_MS`.
- Produces: `pub fn materialize_with_bounds(events: &[SignedMembershipEvent], admin_addr: OwnerAddr, now_ms: Option<u64>, receiver_now_ms: Option<u64>) -> MaterializedMembership` — used by Task 2's accessors.

- [ ] **Step 1: Write the failing tests**

Add at the end of the test region of `community_membership.rs`. Reuse the existing module helpers `make_identity(seed_byte) -> (TestOwner, [u8;64], OwnerAddr)`, `make_join_event(id_byte: u8, actor: OwnerAddr, at_wall_ms: u64) -> SignedMembershipEvent`, and `make_kick_event(..)` (patterns at `:7268`/`:7356`/`:7819`; copy the minimal set your module needs, mirroring the nearest existing `mod tests`). The test's own logic:

```rust
#[cfg(test)]
mod zeb846_forward_ceiling {
    use super::*;

    // Reuse the module's existing event/identity helpers (mirror the block
    // near community_membership.rs:7268-7380: make_identity, make_join_event,
    // make_kick_event). Admin mints; a member joins honestly at t=1_000_000ms.

    const T_NOW: u64 = 1_000_000; // receiver "now", ms
    const SKEW: u64 = crate::clock_trust::MAX_FORWARD_SKEW_MS; // 300_000

    #[test]
    fn ceiling_excludes_future_kick_so_victim_retains_membership() {
        // admin, victim identities; victim joins at an honest past wall.
        let (admin_owner, _admin_pk, admin_addr) = make_identity(1);
        let (_v_owner, _v_pk, victim) = make_identity(2);
        let join = make_join_event(10, victim, T_NOW - 10_000);
        // Malicious Kick stamped far in the future (now + 1 year). It sorts
        // LAST (wall is primary sort key) and would win LWW without the bound.
        let kick = make_kick_event_at(20, admin_addr, victim, T_NOW + 365 * 86_400_000, &admin_owner);
        let events = vec![join, kick];

        // With the receiver-now ceiling: the future Kick is excluded, victim stays.
        let bounded = materialize_with_bounds(&events, admin_addr, None, Some(T_NOW));
        assert!(
            member_is_present(&bounded, &victim),
            "future-dated Kick must be excluded by the forward ceiling"
        );

        // Sanity: without the ceiling (receiver_now = None) the poison applies.
        let unbounded = materialize_with_bounds(&events, admin_addr, None, None);
        assert!(
            !member_is_present(&unbounded, &victim),
            "control: with no ceiling the future Kick dominates LWW and removes the victim"
        );
    }

    #[test]
    fn ceiling_disabled_when_receiver_now_is_none_applies_all() {
        // A benign event stamped slightly ahead (within honest jitter) must NOT
        // be dropped when there is no trusted receiver clock.
        let (admin_owner, _pk, admin_addr) = make_identity(1);
        let (_o, _p, m) = make_identity(2);
        let join = make_join_event(10, m, T_NOW + 60_000); // 1 min ahead
        let events = vec![join];
        let mat = materialize_with_bounds(&events, admin_addr, None, None);
        assert!(member_is_present(&mat, &m), "None ceiling ⇒ apply-all, honest event kept");
        let _ = admin_owner;
    }

    #[test]
    fn slow_local_clock_defers_not_drops_and_recovers_when_now_advances() {
        // Receiver clock lags real time: an honest event looks future-dated.
        // It must be EXCLUDED (deferred), never deleted — and reappear once the
        // receiver clock catches up. materialize_with_bounds is a pure view over
        // the same input slice, so "reappear" = re-materialize with a larger now.
        let (_ao, _apk, admin_addr) = make_identity(1);
        let (_o, _p, m) = make_identity(2);
        let honest = make_join_event(10, m, T_NOW); // stamped at real now
        let events = vec![honest];

        // Behind clock: now = T_NOW - (SKEW + 1) ⇒ honest event is > now+SKEW ⇒ excluded.
        let behind = T_NOW - (SKEW + 1);
        let deferred = materialize_with_bounds(&events, admin_addr, None, Some(behind));
        assert!(!member_is_present(&deferred, &m), "behind clock defers the honest join");

        // Clock corrects: same input slice, larger now ⇒ event applies. Nothing was lost.
        let applied = materialize_with_bounds(&events, admin_addr, None, Some(T_NOW));
        assert!(member_is_present(&applied, &m), "advancing now re-admits the deferred event");
    }
}
```

Notes for the implementer:
- `member_is_present(&MaterializedMembership, &OwnerAddr) -> bool`: use the module's existing membership accessor to assert the victim is/ isn't a current member (mirror how the nearest `mod tests` inspects `MaterializedMembership` — e.g. the `members`/`member_state`/joined-set accessor already used by `kick_of_joined_member_does_trigger_epoch_rotation` at `:11190`). Do not invent a new accessor; use whatever that test uses.
- `make_kick_event_at(id_byte, admin_addr, target, at_wall_ms, admin_owner)`: if the existing `make_kick_event` helper does not expose a controllable `at_wall_ms`, add a tiny local variant in this test module that sets the Kick's `at.wall_ms` explicitly (copy `make_kick_event` and parameterize the wall). The whole point of these tests is controlling `at.wall_ms`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(zeb846_forward_ceiling)'`
Expected: FAIL — `materialize_with_bounds` is not defined (compile error).

- [ ] **Step 3: Implement `materialize_with_bounds`**

Insert after the `materialize` wrapper (after line 2293):

```rust
/// Materialize with a forward-skew *ceiling* (ZEB-846) on top of the optional
/// aging *floor* (`now_ms`).
///
/// `receiver_now_ms` is the *receiver's own* trusted wall clock
/// (`std::time::SystemTime::now()`), never a peer-supplied or `HlcAdoptFloor`
/// value — a forward bound is only sound when measured against a clock the
/// attacker cannot move. When `Some(rn)`, any event whose `at.wall_ms` is more
/// than [`crate::clock_trust::MAX_FORWARD_SKEW_MS`] beyond `rn` is *excluded
/// from the input entirely* before materializing. Excluding an event from the
/// slice is exactly equivalent to skipping it in the sort/apply loop of
/// [`materialize_with_now`]: it drops out of `event_sort_key` ordering (no
/// POISON-SQUAT), out of `events_max_wall_ms` (the aging floor stays honest),
/// and out of every recovery/expiry control — so a future-dated event cannot
/// gain power.
///
/// `None` disables the ceiling (apply-all). This is the load-bearing
/// non-destructive property: a *bad local* clock must never drop or defer
/// honest governance, so callers without a trusted receiver clock pass `None`.
/// The exclusion is a live view, re-evaluated on every materialize; nothing is
/// ever deleted from the persisted log (`community_state_persist`).
pub fn materialize_with_bounds(
    events: &[SignedMembershipEvent],
    admin_addr: OwnerAddr,
    now_ms: Option<u64>,
    receiver_now_ms: Option<u64>,
) -> MaterializedMembership {
    match receiver_now_ms {
        Some(rn) => {
            let effective: Vec<SignedMembershipEvent> = events
                .iter()
                .filter(|e| {
                    !crate::clock_trust::reject_future(
                        e.at.wall_ms,
                        rn,
                        crate::clock_trust::MAX_FORWARD_SKEW_MS,
                    )
                })
                .cloned()
                .collect();
            materialize_with_now(&effective, admin_addr, now_ms)
        }
        None => materialize_with_now(events, admin_addr, now_ms),
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(zeb846_forward_ceiling)'`
Expected: PASS (all three).

- [ ] **Step 5: Lint + format the touched file**

Run: `cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/community_membership.rs
git commit -m "ZEB-846: add materialize_with_bounds forward-skew ceiling primitive

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01DUvg7gyEHqXrVU85Eg2D8D"
```

---

## Task 2: Route the three `CommunityState` read accessors through the ceiling (membership Layer 2 wiring + reload-safety)

**Files:**
- Modify: `src-tauri/src/community_state_crdt.rs` — `materialized` (`:469-506`, cache-miss recompute at `:496`), `materialize_now` (`:568-571`, call at `:570`), `materialized_with_now` (`:586-593`, call at `:592`).
- Test: `src-tauri/src/community_state_crdt.rs` (accessor bound + reload-safety) — new `#[cfg(test)] mod zeb846_accessor_ceiling`.

**Interfaces:**
- Consumes: `community_membership::materialize_with_bounds` (Task 1); `community_state_persist::{save_crdt, load_crdt}` (unchanged, for the reload test).
- Produces: no new public signatures — the three accessors keep their signatures; only their bodies change to apply the receiver-`now` ceiling.

**Behavioral contract for each accessor:**
- `materialized(admin_addr)` (cached, floor `None`): recompute path calls `materialize_with_bounds(&events, admin_addr, None, Some(receiver_now))` where `receiver_now = SystemTime::now()`. Cache remains version+admin keyed; the ceiling is monotonic-safe (advancing real time only ever *un*-excludes events, and un-exclusion requires a log mutation, which already busts the cache), so caching a `now`-relative result is fail-safe (only ever over-excludes future-dated events, never under-excludes).
- `materialize_now(admin_addr)` (uncached, floor `None`): `materialize_with_bounds(&events, admin_addr, None, Some(SystemTime::now()))`.
- `materialized_with_now(admin_addr, now_ms)` (uncached, floor `Some(now_ms)`): `now_ms` here **is** the receiver clock (recovery passes `wall_now_ms`). Pass it as both floor and ceiling: `materialize_with_bounds(&events, admin_addr, Some(now_ms), Some(now_ms))`.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod zeb846_accessor_ceiling {
    use super::*;
    // Reuse this module's existing CommunityState test helpers (an admin mint +
    // a signed Kick builder). Mirror the nearest `mod tests` in this file.

    #[test]
    fn cached_materialized_excludes_future_dated_kick() {
        // Build a CommunityState whose log holds an honest join + a far-future Kick.
        let (state, admin_addr, victim) = state_with_join_and_future_kick();
        let m = state.materialized(admin_addr);
        assert!(
            membership_contains(&m, &victim),
            "cached materialized() read must exclude the future-dated Kick (receiver-now ceiling)"
        );
    }

    #[test]
    fn poison_survives_reload_but_is_still_deferred_on_materialize() {
        // The load-bearing, reload-safe proof: persist a poison frame directly to
        // crdt.cbor (bypassing verify_event / insert_event), load_crdt it back,
        // and confirm the bound STILL holds — proving the fix does not depend on
        // admission having run.
        let dir = tempdir().unwrap();
        let path = dir.path().join("crdt.cbor");
        let (state, admin_addr, victim, community_id) = state_with_join_and_future_kick_full();
        community_state_persist::save_crdt(&path, &state).unwrap();

        let reloaded = community_state_persist::load_crdt(&path, community_id).unwrap();
        // The poison event is STILL on disk / in the log (retain, don't delete):
        assert!(
            reloaded.log_len() >= 2,
            "reload must RETAIN the poison event (non-destructive); we only exclude it from the view"
        );
        // ...but the materialized view still defers it:
        let m = reloaded.materialized(admin_addr);
        assert!(
            membership_contains(&m, &victim),
            "after reload, materialized() still defers the future-dated Kick"
        );
    }
}
```

Notes:
- `state_with_join_and_future_kick[_full]()`: construct a `CommunityState` by `insert_event`-ing an honest join, then inserting (or, for the future Kick, **force-inserting past admission** since Layer 3 will reject it live — use the same test seam the file already uses to build a log with a chosen event; if none exists, build the `VerifiedLog` via the crate-visible path used by other tests in this file). The far-future Kick's `at.wall_ms = <real now> + 365 days`.
- `membership_contains`, `log_len`: reuse existing accessors on `CommunityState` / `MaterializedMembership` used elsewhere in this file's tests. `tempdir` from the `tempfile` dev-dependency (already used across the suite).

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(zeb846_accessor_ceiling)'`
Expected: FAIL — `materialized()` currently applies the future Kick (no ceiling).

- [ ] **Step 3: Route `materialized` (cached, `:496`)**

At the cache-miss recompute (currently `let fresh = community_membership::materialize(&events, admin_addr);` at `:496`), replace with:

```rust
let receiver_now = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .map(|d| d.as_millis() as u64)
    .ok();
let fresh = community_membership::materialize_with_bounds(
    &events,
    admin_addr,
    None,            // floor unchanged (event-driven)
    receiver_now,    // ZEB-846 forward ceiling; None if the clock is pre-epoch
);
```

(Adjust the local variable name `fresh`/`events` to match the existing code at `:496`; do not change the caching/version logic around it.)

- [ ] **Step 4: Route `materialize_now` (`:570`) and `materialized_with_now` (`:592`)**

`materialize_now` (`:570`): replace `community_membership::materialize(&self.log_events(), admin_addr)` with:

```rust
let receiver_now = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .map(|d| d.as_millis() as u64)
    .ok();
community_membership::materialize_with_bounds(&self.log_events(), admin_addr, None, receiver_now)
```

`materialized_with_now` (`:592`): replace `community_membership::materialize_with_now(&self.log_events(), admin_addr, Some(now_ms))` with:

```rust
community_membership::materialize_with_bounds(&self.log_events(), admin_addr, Some(now_ms), Some(now_ms))
```

(Use the exact event-slice accessor the current code uses at each site — shown here as `self.log_events()`; keep it verbatim.)

- [ ] **Step 5: Run the new tests to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(zeb846_accessor_ceiling)'`
Expected: PASS (both).

- [ ] **Step 6: Full-suite sweep + test realignment (REQUIRED — the ceiling retroactively trips real-clock tests)**

Introducing a receiver-`now` ceiling into `materialized()` makes any existing test that stamps events **more than 5 min ahead of real `SystemTime::now()`** (as a convenience, not as poison) see those events excluded — the failure mode documented in `feedback_wall_clock_gate_retroactively_breaks_realclock_tests`. Sweep wide and realign:

Run: `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures`

For each newly-failing test: if it read state via `materialized()`/`materialize_now()`/`materialized_with_now()` and used an **artificially future** `at.wall_ms` (e.g. a large constant like `9_999_999_999_999` or `now + hours` used only to force ordering), **backdate** the stamp to a realistic value at or before real now (the fix is the stamp, not the gate). If a test *legitimately* needs to observe an un-bounded materialize (pure ordering unit tests), have it call the free `community_membership::materialize`/`materialize_with_now` directly (floor/ceiling `None`) rather than the `CommunityState` accessors. Do **not** weaken the ceiling to make a test pass.

Expected after realignment: full suite green.

- [ ] **Step 7: Lint + format**

Run: `cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
Expected: clean.

- [ ] **Step 8: Commit**

```bash
git add -A
git commit -m "ZEB-846: apply forward-skew ceiling in CommunityState read accessors (reload-safe)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01DUvg7gyEHqXrVU85Eg2D8D"
```

---

## Task 3: Membership Layer 1 — admission reject in `verify_event`

**Files:**
- Modify: `src-tauri/src/community_membership.rs` — `VerifyContext` struct (`:3938-3942`) add `now_ms: Option<u64>`; `verify_event` (`:3979`) add the forward reject immediately after the community-binding guard (`:3993-3995`), before crypto (`:4016`). Add a `VerifyError` variant for the reject.
- Modify: `src-tauri/src/community_state_sync.rs` — set `now_ms` in every `VerifyContext { .. }` constructor (`:4363` = `Some(SystemTime::now())`; `:1634`, `:1817`, `:1822`, `:2193` = `None`).
- Modify: `src-tauri/src/community_state_crdt.rs` — any `VerifyContext { .. }` literal in this file gains `now_ms: None` (the insert-time policy path does not carry a receiver clock; Layer 2 governs reads). Verify `#[derive(Clone, Copy)]` on `VerifyContext` still holds (`Option<u64>` is `Copy`).

**Interfaces:**
- Consumes: `clock_trust::reject_future`, `clock_trust::MAX_FORWARD_SKEW_MS`.
- Produces: `VerifyContext { expected_community_id, admin_addr, is_invite_only, now_ms: Option<u64> }`; new `VerifyError::FutureSkew` (or similarly named) variant.

**Scoping note (record in the commit body):** Layer 1 is defense-in-depth. The insert-time prior-state materialize (`MembershipPolicy::materialize`, floor = candidate's own wall) is **not** given the receiver ceiling here — it reconstructs state to verify a *new* event, Layer 1 rejects the new event if it is future-dated, and the authorization *reads* (Task 2) carry the ceiling. Legacy poison already in prior-state during the transition is a known, bounded residual the read path does not suffer.

- [ ] **Step 1: Write the failing test**

Add to the `verify_event` test module (near `:8464`, reuse its `make_identity`/signing helpers):

```rust
#[test]
fn verify_event_rejects_far_future_wall_when_receiver_now_present() {
    let (admin_owner, _pk, admin_addr) = make_identity(1);
    let (_o, _p, joiner) = make_identity(2);
    let now = 1_000_000u64;
    let prior = /* MaterializedMembership after admin mint — reuse existing setup */;
    let future_join = make_join_event(10, joiner, now + crate::clock_trust::MAX_FORWARD_SKEW_MS + 1);

    let ctx_bounded = VerifyContext {
        expected_community_id: /* the community id */,
        admin_addr,
        is_invite_only: false,
        now_ms: Some(now),
    };
    assert!(
        matches!(verify_event(&future_join, &prior, &ctx_bounded), Err(VerifyError::FutureSkew)),
        "a wall beyond now+5min must be rejected at admission when a receiver clock is present"
    );

    // Boundary: exactly now+5min is accepted (reject_future is inclusive).
    let edge_join = make_join_event(11, joiner, now + crate::clock_trust::MAX_FORWARD_SKEW_MS);
    assert!(
        !matches!(verify_event(&edge_join, &prior, &ctx_bounded), Err(VerifyError::FutureSkew)),
        "exactly now+5min is within the inclusive bound"
    );

    // None ⇒ no forward reject (bad local clock must not drop honest events).
    let ctx_unbounded = VerifyContext { now_ms: None, ..ctx_bounded };
    assert!(
        !matches!(verify_event(&future_join, &prior, &ctx_unbounded), Err(VerifyError::FutureSkew)),
        "None receiver clock disables the forward reject"
    );
    let _ = admin_owner;
}
```

(Fill `prior` and `expected_community_id` from the module's existing admin-mint setup — mirror an adjacent `verify_event` test.)

- [ ] **Step 2: Run to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(verify_event_rejects_far_future_wall)'`
Expected: FAIL — `VerifyContext` has no `now_ms`, no `VerifyError::FutureSkew` (compile error).

- [ ] **Step 3: Add the field, the error variant, and the reject**

`VerifyContext` (`:3938`):

```rust
pub struct VerifyContext {
    pub expected_community_id: SpaceId,
    pub admin_addr: OwnerAddr,
    pub is_invite_only: bool,
    /// ZEB-846: the receiver's own trusted wall clock (ms). `Some` ⇒ reject an
    /// event whose `at.wall_ms` is beyond `now + MAX_FORWARD_SKEW_MS`. `None` ⇒
    /// no forward reject (a bad local clock must never drop honest governance).
    pub now_ms: Option<u64>,
}
```

Add the `VerifyError` variant (place with the other variants in the `VerifyError` enum):

```rust
/// ZEB-846: event `at.wall_ms` is implausibly far ahead of the receiver's clock.
FutureSkew,
```

In `verify_event`, immediately after the community-binding guard (after `:3995`):

```rust
// ZEB-846 (Layer 1): reject an implausibly-future wall before any further
// work, so poison never enters the persisted log or gets re-gossiped. Only
// when a trusted receiver clock is supplied — otherwise apply-all.
if let Some(now) = ctx.now_ms {
    if crate::clock_trust::reject_future(
        event.at.wall_ms,
        now,
        crate::clock_trust::MAX_FORWARD_SKEW_MS,
    ) {
        return Err(VerifyError::FutureSkew);
    }
}
```

- [ ] **Step 4: Update every `VerifyContext` constructor**

- `community_state_sync.rs:4363` (live merge): `now_ms: Some(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0))`. (A pre-epoch `unwrap_or(0)` yields `now=0`; `reject_future(wall, 0, tol)` rejects only walls `> tol` — acceptably degenerate and never drops honest present-day events, which are all `> tol`. If you prefer strict `None`-on-error semantics, use `.ok()` and pass the `Option` — either is acceptable; be consistent.)
- `community_state_sync.rs:1634`, `:1817`, `:1822`, `:2193`: `now_ms: None` (fork-veto pre-validate and other clock-less callers keep working).
- Any `VerifyContext { .. }` in `community_state_crdt.rs` and in test modules: add `now_ms: None` unless the test specifically exercises the bound.

Grep to be exhaustive: `cd src-tauri && grep -rn "VerifyContext {" src/` — every literal must gain the field.

- [ ] **Step 5: Run the new test + compile the workspace**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(verify_event_rejects_far_future_wall)'`
Then: `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures`
Expected: new test PASS; whole suite green (realign any `VerifyContext` literal that fails to compile).

- [ ] **Step 6: Lint + format**

Run: `cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
Expected: clean.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "ZEB-846: reject far-future membership walls at admission (verify_event Layer 1)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01DUvg7gyEHqXrVU85Eg2D8D"
```

---

## Task 4: Unify `ADMIN_PROPOSAL_MAX_FORWARD_SKEW_MS` onto `clock_trust::MAX_FORWARD_SKEW_MS`

**Files:**
- Modify: `src-tauri/src/community_membership.rs` — constant (`:5513`), planner filter (`:5932`). Planner tests at `:7185`/`:7207` reference the constant symbolically and need no value edits.

**Interfaces:**
- Consumes: `clock_trust::MAX_FORWARD_SKEW_MS`.
- Produces: `ADMIN_PROPOSAL_MAX_FORWARD_SKEW_MS` redefined as an alias (value now 5 min).

**Why an alias, not a delete:** the constant is `pub` and referenced by the `clock_trust::skew_tiers_stay_within_consumer_budgets` pin (`clock_trust.rs:141-143`, asserts `MAX_FORWARD_SKEW_MS <= ADMIN_PROPOSAL_MAX_FORWARD_SKEW_MS`) and by two planner tests. Aliasing keeps every symbolic reference valid across the change; the pin becomes `MAX <= MAX` (trivially true), and the planner tests' `now + ADMIN_PROPOSAL_MAX_FORWARD_SKEW_MS` boundaries stay self-consistent at the new 5-min value.

- [ ] **Step 1: Redefine the constant (`:5513`)**

```rust
/// ZEB-846: unified onto the house control-tier ceiling. Retained as a named
/// alias for the planner filter and the two planner boundary tests; the value
/// is now 5 min, not 30. Delete once no external reference remains.
pub const ADMIN_PROPOSAL_MAX_FORWARD_SKEW_MS: u64 = crate::clock_trust::MAX_FORWARD_SKEW_MS;
```

- [ ] **Step 2: Point the planner filter at the house constant (`:5932`)**

Change the forward-skew term to reference `crate::clock_trust::MAX_FORWARD_SKEW_MS` directly:

```rust
.filter(|e| {
    now_ms.saturating_sub(e.at.wall_ms) <= ADMIN_PROPOSAL_EXPIRY_MS
        && e.at.wall_ms.saturating_sub(now_ms) <= crate::clock_trust::MAX_FORWARD_SKEW_MS
})
```

(Leave the backward-expiry term `ADMIN_PROPOSAL_EXPIRY_MS` untouched — that is not a skew bound.)

- [ ] **Step 3: Run the planner + clock_trust tests**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(plan_countersign_tolerates_benign_forward_skew) + test(plan_mint_when_candidate_exceeds_forward_skew_bound) + test(skew_tiers_stay_within_consumer_budgets)'`
Expected: PASS. The boundary tests (`just_inside = now + ADMIN_PROPOSAL_MAX_FORWARD_SKEW_MS`, `just_outside = ... + 1`) still pass because the filter uses the same (now 5-min) value.

- [ ] **Step 4: Lint + format**

Run: `cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
Expected: clean (watch for a now-unused-import or dead-constant warning; there should be none since the alias is still referenced).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/community_membership.rs
git commit -m "ZEB-846: unify admin-proposal forward-skew constant onto clock_trust::MAX_FORWARD_SKEW_MS

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01DUvg7gyEHqXrVU85Eg2D8D"
```

---

## Task 5: Voting — Layer 1 reject in `process_inbound` + Layer 2 stage-`now` clamp (E1)

**Files:**
- Modify: `src-tauri/src/community_voting_log_engine.rs` — `process_inbound` (`:2746`): reject a future `event.hlc.wall_ms` before `apply_with_snapshot` (`:2805`). `maybe_trigger_engine_auto_orchestration` (`:945`, clamp at the stage-`now` derivation `:1108-1117`).

**Interfaces:**
- Consumes: `clock_trust::reject_future`, `clock_trust::clamp_future`, `clock_trust::MAX_FORWARD_SKEW_MS`.
- Produces: no signature changes (both edits are internal to the two async methods; `process_inbound` already returns `Result<Option<(SignedVotingEvent, PollId)>, String>`).

**Scoping note (record in commit body):** the sibling `current_stage_at` consumers do not need clamps. `community_voting_tier3.rs:507`/`:585` require `Stage::Deliberation` and are **fail-closed** for a future stamp (a future event computes a *later* stage → its deliberation-only action is rejected). `:1425` (`verify_ratification_ballot` B2) is fail-open but runs at **admission**, so Task 5's `process_inbound` reject covers it (verify never re-runs on replay). `lib.rs:55853/56224/56324` and `community_voting_log.rs:258` are display-tier (UI stage label; mis-sort only). Only `:1108` (auto-orchestration finalize) is both fail-open and reload-reachable via legacy `last_hlc`, so it is the one Layer-2 clamp.

- [ ] **Step 1: Write the failing tests**

Add to `community_voting_log_engine.rs`'s test module (reuse its packet/engine builders):

```rust
#[tokio::test]
async fn process_inbound_rejects_far_future_voting_event() {
    // Build an engine + a signed voting event whose hlc.wall_ms is real-now + 1 year.
    // ... reuse the module's existing process_inbound test harness ...
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
    let poison = signed_voting_event_at(/* .. */, now_ms + 365 * 86_400_000);
    let res = CommunityVotingLogEngine::process_inbound(
        community_id, &voting_log, &tracker, id_res, mem_res, &floor, &encode(&poison),
    ).await;
    assert!(res.is_err(), "a voting event beyond now+5min must be rejected at admission");
    // And it must NOT have been applied / observed into the floor.
}

#[tokio::test]
async fn future_event_does_not_advance_poll_stage_to_ratification() {
    // Force a Tier3 poll whose last_hlc is far-future (model legacy poison that
    // predates Layer 1), then invoke the auto-orchestration trigger and assert the
    // clamped stage stays pre-Ratification, so the poll does not instant-finalize.
    // ... reuse the module's Tier3 poll fixture ...
}
```

(Use the module's existing helpers for building a signed voting event with a chosen HLC and for constructing a Tier3 poll fixture — mirror the nearest `#[tokio::test]` in the file.)

- [ ] **Step 2: Run to verify they fail**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(process_inbound_rejects_far_future_voting_event) + test(future_event_does_not_advance_poll_stage)'`
Expected: FAIL.

- [ ] **Step 3: Add the Layer-1 reject in `process_inbound`**

Just before `verify_voting_event` / `apply_with_snapshot` (around `:2791`/`:2805`), after the event is decoded:

```rust
// ZEB-846 (Layer 1): reject an implausibly-future voting event before it can
// be applied, observed into the adoption floor, or re-gossiped.
let receiver_now_ms = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .map(|d| d.as_millis() as u64)
    .unwrap_or(0);
if receiver_now_ms != 0
    && crate::clock_trust::reject_future(
        event.hlc.wall_ms,
        receiver_now_ms,
        crate::clock_trust::MAX_FORWARD_SKEW_MS,
    )
{
    return Err(format!(
        "voting event wall {} is beyond receiver now {} + {}ms forward-skew bound",
        event.hlc.wall_ms, receiver_now_ms, crate::clock_trust::MAX_FORWARD_SKEW_MS
    ));
}
```

(The `receiver_now_ms != 0` guard is the `None`-equivalent apply-all fallback for a pre-epoch clock — consistent with the rest of the work. Place the block after `event` is bound and before `floor.observe(event.hlc.wall_ms)` at `:2823` so a rejected event never touches the floor.)

- [ ] **Step 4: Add the Layer-2 stage-`now` clamp in `maybe_trigger_engine_auto_orchestration`**

At the `last_wall` derivation (`:1108-1117`), clamp before building `now_hlc_cl`:

```rust
let last_wall = match t3.last_hlc.as_ref() {
    Some(h) => h.wall_ms,
    None => return,
};
// ZEB-846 (Layer 2): a future accepted event (legacy poison predating Layer 1,
// or replay) must not let last_hlc jump the poll straight to Ratification.
// Clamp the effective "now" to the receiver's own clock + 5min.
let last_wall = match std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .map(|d| d.as_millis() as u64)
{
    Ok(rn) => crate::clock_trust::clamp_future(last_wall, rn, crate::clock_trust::MAX_FORWARD_SKEW_MS),
    Err(_) => last_wall, // pre-epoch clock ⇒ apply-all fallback
};
let now_hlc_cl = Hlc { wall_ms: last_wall, logical: 0, device_id: String::new() };
let stage_now = t3.current_stage_at(&now_hlc_cl);
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(process_inbound_rejects_far_future_voting_event) + test(future_event_does_not_advance_poll_stage)'`
Expected: PASS.

- [ ] **Step 6: Scoped voting-suite sweep, then lint + format**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(voting)'`
Then: `cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
Expected: voting suite green (realign any test that fed the engine a far-future event as a convenience — backdate it); lint clean.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/community_voting_log_engine.rs
git commit -m "ZEB-846: bound voting event walls (process_inbound reject + stage-now clamp)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01DUvg7gyEHqXrVU85Eg2D8D"
```

---

## Task 6: Channel — `verify_channel_event_at` (Layer 1 reject + Layer 2 `deleted_at` clamp)

**Files:**
- Modify: `src-tauri/src/community_channel_log.rs` — add `verify_channel_event_at(.., now_ms: Option<u64>)` holding the logic (the current `verify_channel_event` body + the two new guards); make `verify_channel_event` a real-clock delegate. Tombstone gate at `:1523-1532`.

**Interfaces:**
- Consumes: `clock_trust::reject_future`, `clock_trust::clamp_future`, `clock_trust::MAX_FORWARD_SKEW_MS`; `Hlc { wall_ms: u64, logical: u32, device_id: String }` with `is_strictly_newer_than(&self, other) = self > other` (derived `Ord` on the tuple).
- Produces:
  - `pub async fn verify_channel_event<S>(event, expected_community_id, expected_channel_id, state, replay_tracker) -> Result<(), ChannelEventError> where S: CommunityStateAtHlc + Sync + ?Sized` — **unchanged signature**; now delegates to `_at` with `Some(SystemTime::now())`.
  - `pub async fn verify_channel_event_at<S>(event, expected_community_id, expected_channel_id, state, replay_tracker, now_ms: Option<u64>) -> Result<(), ChannelEventError> where S: ...` — the logic, clock-injectable for tests.
- Add a `ChannelEventError` variant for the future-skew reject (e.g. `FutureSkew`), or reuse the existing `NotAuthorized(String)` with a clear message if adding a variant is disproportionate — pick one and be consistent.

**Why a wrapper, not a param on the existing fn:** `verify_channel_event` has exactly one production caller (`community_channel_log_engine.rs:1681`) and ~30 in-file test callers. The `_at` wrapper gives full clock injectability for the CD test while leaving all existing callers (prod + tests) untouched — the production caller automatically gets the real clock via the delegate.

- [ ] **Step 1: Write the failing tests**

Add to `community_channel_log.rs`'s test module (reuse its channel-state fixture + signed-event builders used by the tests at `:3008`+, and the `deleted_at: Some(Hlc { .. })` pattern at `:4304`):

```rust
#[tokio::test]
async fn far_future_deleted_at_still_gates_posts_after_clamp_window() {
    // Channel deleted with a far-future deleted_at (real-now + 1 year) — models a
    // poison ChannelDelete persisted before Layer 1. A post stamped past the
    // clamp window (now + 5min) must be REJECTED (the deletion still gates writes).
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
    let deleted_at = Hlc { wall_ms: now_ms + 365 * 86_400_000, logical: 0, device_id: "x".into() };
    let state = channel_state_with_deletion(deleted_at.clone());
    let post = signed_post_at(now_ms + crate::clock_trust::MAX_FORWARD_SKEW_MS + 60_000); // 6 min ahead
    let mut tracker = ChannelLogReplayTracker::default();
    let res = verify_channel_event_at(
        &post, &community_id, &channel_id, &state, &mut tracker, Some(now_ms),
    ).await;
    assert!(res.is_err(), "post past the clamped deletion window must be gated");
}

#[tokio::test]
async fn none_receiver_now_uses_unclamped_deleted_at() {
    // None ⇒ apply-all: with a far-future deleted_at and None now, a present-day
    // post is NOT gated (deleted_at unclamped) — the non-destructive fallback.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
    let deleted_at = Hlc { wall_ms: now_ms + 365 * 86_400_000, logical: 0, device_id: "x".into() };
    let state = channel_state_with_deletion(deleted_at);
    let post = signed_post_at(now_ms);
    let mut tracker = ChannelLogReplayTracker::default();
    let res = verify_channel_event_at(
        &post, &community_id, &channel_id, &state, &mut tracker, None,
    ).await;
    assert!(res.is_ok(), "None receiver clock ⇒ unclamped deleted_at, post allowed");
}

#[tokio::test]
async fn future_post_wall_rejected_at_admission() {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
    let state = channel_state_open(); // no deletion
    let post = signed_post_at(now_ms + crate::clock_trust::MAX_FORWARD_SKEW_MS + 1);
    let mut tracker = ChannelLogReplayTracker::default();
    let res = verify_channel_event_at(
        &post, &community_id, &channel_id, &state, &mut tracker, Some(now_ms),
    ).await;
    assert!(res.is_err(), "a post wall beyond now+5min is rejected at admission");
}
```

(`channel_state_with_deletion`, `channel_state_open`, `signed_post_at` — reuse/mirror the fixtures the existing tests in this file already build; `signed_post_at(wall)` sets the event's `at.wall_ms`.)

- [ ] **Step 2: Run to verify they fail**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(far_future_deleted_at_still_gates_posts) + test(none_receiver_now_uses_unclamped) + test(future_post_wall_rejected_at_admission)'`
Expected: FAIL — `verify_channel_event_at` undefined.

- [ ] **Step 3: Split into `verify_channel_event_at` + delegate**

Rename the current `verify_channel_event` body into `verify_channel_event_at` with the extra `now_ms: Option<u64>` param, and add the delegate:

```rust
pub async fn verify_channel_event<S>(
    event: &SignedChannelEvent,
    expected_community_id: &SpaceId,
    expected_channel_id: &ChannelId,
    state: &S,
    replay_tracker: &mut ChannelLogReplayTracker,
) -> Result<(), ChannelEventError>
where
    S: CommunityStateAtHlc + Sync + ?Sized,
{
    // ZEB-846: production path uses the node's own trusted clock.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .ok();
    verify_channel_event_at(
        event, expected_community_id, expected_channel_id, state, replay_tracker, now_ms,
    )
    .await
}

pub async fn verify_channel_event_at<S>(
    event: &SignedChannelEvent,
    expected_community_id: &SpaceId,
    expected_channel_id: &ChannelId,
    state: &S,
    replay_tracker: &mut ChannelLogReplayTracker,
    now_ms: Option<u64>,
) -> Result<(), ChannelEventError>
where
    S: CommunityStateAtHlc + Sync + ?Sized,
{
    // ... existing verify_channel_event body ...
}
```

- [ ] **Step 4: Add the Layer-1 post/config reject (in `_at`)**

Near where `at` is bound (`:1356`), before the snapshot resolve:

```rust
// ZEB-846 (Layer 1): reject an implausibly-future event wall at admission —
// covers both a post's at.wall_ms and a ChannelDelete/config event's wall.
if let Some(now) = now_ms {
    if crate::clock_trust::reject_future(at.wall_ms, now, crate::clock_trust::MAX_FORWARD_SKEW_MS) {
        return Err(ChannelEventError::FutureSkew); // or NotAuthorized("future-skew ...".into())
    }
}
```

- [ ] **Step 5: Add the Layer-2 `deleted_at` clamp (in `_at`, at the tombstone gate `:1523-1532`)**

```rust
if let Some(deleted_at) = &channel_info.deleted_at {
    // ZEB-846 (Layer 2): clamp a far-future deleted_at down to now+5min so a
    // poison ChannelDelete persisted before Layer 1 still gates posts. Skipping
    // the deletion would un-delete the channel — the exact bug — so we clamp,
    // never defer. None ⇒ unclamped (apply-all fallback).
    let effective_deleted = match now_ms {
        Some(rn) => Hlc {
            wall_ms: crate::clock_trust::clamp_future(
                deleted_at.wall_ms, rn, crate::clock_trust::MAX_FORWARD_SKEW_MS,
            ),
            logical: deleted_at.logical,
            device_id: deleted_at.device_id.clone(),
        },
        None => deleted_at.clone(),
    };
    // Strictly-newer-than: a post AFTER the (clamped) deletion is rejected.
    if at.is_strictly_newer_than(&effective_deleted) {
        return Err(ChannelEventError::NotAuthorized(format!(
            "channel deleted at {:?} (clamped {:?}), post at {:?}",
            deleted_at, effective_deleted, at
        )));
    }
}
```

- [ ] **Step 6: Run the new tests to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(far_future_deleted_at_still_gates_posts) + test(none_receiver_now_uses_unclamped) + test(future_post_wall_rejected_at_admission)'`
Expected: PASS.

- [ ] **Step 7: Scoped channel-suite sweep, then lint + format**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(channel)'`
Then: `cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
Expected: channel suite green (realign any existing test that used a far-future post/`deleted_at` as a convenience — the ~30 existing `verify_channel_event` callers now get the real clock via the delegate, so a test stamping a post >5min ahead of real now will newly reject; backdate such stamps); lint clean.

- [ ] **Step 8: Commit**

```bash
git add src-tauri/src/community_channel_log.rs
git commit -m "ZEB-846: bound channel event walls (verify_channel_event_at reject + deleted_at clamp)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01DUvg7gyEHqXrVU85Eg2D8D"
```

---

## Final gate (before opening the PR)

- [ ] **Full CI-parity sweep:** `cd src-tauri && cargo fmt --all -- --check && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && cargo nextest run --locked --workspace --all-targets --features test-fixtures` — all green.
- [ ] **Whole-branch review** (subagent-driven-development final review) on the most capable model, focused on: the cache-staleness reasoning in `materialized()` (Task 2), the `None`-fallback correctness at every site, and that no backward/anti-backdating bound crept in.
- [ ] **Cross-node convergence note** (spec §5.5): confirm in the PR description that honest nodes with correct clocks all defer the same far-future poison and converge once real time passes the walls; a node whose *own* clock is far-future is itself degraded (documented residual, not a regression).

---

## Self-Review

**1. Spec coverage** (spec §1 findings → task):
- A1 recovery-veto, `event_sort_key` poison, A2/A3 expiry, A4/A6 pending-join/community-drop, RR rotation-finality → **Task 1 + Task 2** (forward-ceiling pre-filter excludes the future event from `sorted`, `events_max_wall_ms`, and the recovery post-pass; wired into the read accessors incl. reload).
- CD future `deleted_at` → **Task 6** (clamp).
- E1 instant finalize → **Task 5** (stage-`now` clamp).
- Layer 1 admission (all three subsystems) → **Task 3** (membership), **Task 5** (voting), **Task 6** (channel).
- 30-min unification (spec §2/§4.1/§6) → **Task 4**.
- Testing matrix (spec §5): discrimination per finding (Tasks 1/2/5/6), restart/replay (Task 2 Step 1), `now=None` fallback (Tasks 1/3/6), slow-clock defer-not-drop (Task 1), cross-node convergence note (Final gate).

**2. Placeholder scan:** All code steps carry real code. Test-helper names that are module-local (`make_join_event`, `make_kick_event`, `make_identity`, `channel_state_with_deletion`, `signed_post_at`, `membership_contains`) are named against **verified-existing** helpers in each target file; where a helper needs a controllable `at.wall_ms` and the existing one hides it, the step instructs a one-line local variant — not a placeholder.

**3. Type consistency:** `materialize_with_bounds(events, admin_addr, now_ms: Option<u64>, receiver_now_ms: Option<u64>)` is the single new membership signature, used identically in Tasks 1 and 2. `VerifyContext.now_ms: Option<u64>` (Task 3) matches its constructor updates. `verify_channel_event_at(.., now_ms: Option<u64>)` (Task 6) matches its delegate. `clock_trust::reject_future(stamp, now, tol) -> bool` and `clamp_future(stamp, now, tol) -> u64` are used with `MAX_FORWARD_SKEW_MS` at every site. `Hlc { wall_ms: u64, logical: u32, device_id: String }` fields match the clamp construction in Tasks 5 and 6.

**4. Ordering / dependencies:** Task 1 (primitive) → Task 2 (wiring, depends on 1). Tasks 3, 4, 5, 6 are independent of each other and of 1/2. Execute 1 → 2 → 3 → 4 → 5 → 6.
