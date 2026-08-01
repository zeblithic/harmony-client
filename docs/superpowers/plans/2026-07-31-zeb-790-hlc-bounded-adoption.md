# ZEB-790 HLC Bounded Causal Adoption Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give `Hlc` bounded cross-device causal adoption — a mint that has verified-and-applied a remote stamp within 5 s of local now always exceeds it — plus deterministic full-tuple ordering on the two governance UI sorts and an honest `Hlc` doc.

**Architecture:** One session-only `HlcAdoptFloor` (atomic `max(verified remote wall)+1`) lives on `NodeState`, is fed **only after** the commit/record step at the three verified accept sites (community-state, channel-log, owner-fleet), and is read by every kernel mint seam as `effective_wall = max(now, min(floor, now + CAP))`. Empty floor = identity, so existing tests hold. No wire change; the upstream `HlcTick` kernel is untouched.

**Tech Stack:** Rust (Tauri backend, `src-tauri/`), Svelte 5 + TypeScript frontend, cargo-nextest, vitest.

**Spec:** `docs/superpowers/specs/2026-07-31-zeb-790-hlc-bounded-adoption-design.md` — read it first; §3 (floor algebra), §4 (feed invariant), §5 (mint seams) are normative.

## Global Constraints

- `HLC_ADOPT_FORWARD_CAP_MS = 5_000` — exact value; the budget-relation test (Task 8) pins it.
- **Never add a field to `Hlc`** (`owner_state_types.rs:324`) — it is a signature-preimage + locked-CBOR wire type.
- **A rejected or unverified frame must never move the floor** — every `observe()` call sits strictly after the accept path's `commit`/`record`/`check_and_advance` success.
- The floor is **not persisted** and is **rebuilt fresh in `start_node`**.
- All cargo commands run from `src-tauri/`; always `--locked`; tests always `--features test-fixtures`; clippy always `--all-targets`.
- Scoped test runs per task (`-E 'test(...)'`); one full sweep only in Task 11 (a `lib.rs` change relinks ~97 integration binaries — do not run the full suite per task).
- Commit after every task with the message given in the task (repo style: `ZEB-790: <what>`).

---

### Task 1: `HlcAdoptFloor` module

**Files:**
- Create: `src-tauri/src/hlc_adopt_floor.rs`
- Modify: `src-tauri/src/lib.rs` (one `mod hlc_adopt_floor;` line — place it alphabetically among the existing `mod` declarations near the top of the file)

**Interfaces:**
- Consumes: nothing.
- Produces: `pub struct HlcAdoptFloor` (Clone + Default), `pub fn new() -> Self`, `pub fn observe(&self, remote_wall_ms: u64)`, `pub fn merged_now(&self, wall_now_ms: u64) -> u64`, `pub const HLC_ADOPT_FORWARD_CAP_MS: u64 = 5_000`. Every later task uses exactly these names.

- [ ] **Step 1: Write the module with failing-first tests**

```rust
//! ZEB-790: bounded causal adoption floor for HLC minting.
//!
//! A session-only high-water of verified remote `wall_ms` values. Feeding
//! happens ONLY after an accept path's commit/record succeeded (the same
//! censorship-defence discipline as the replay trackers — a rejected frame
//! must never move this). Reading happens inside the mint seams:
//! `effective_wall = max(now, min(floor, now + HLC_ADOPT_FORWARD_CAP_MS))`.
//!
//! The stored value is `max observed remote wall + 1`: we adopt only the
//! wall (not `logical`), and a remote stamp `(W, l>0)` would out-sort a
//! naive adoption minted at `(W, 0)` — storing `W+1` makes the adopted
//! mint strictly exceed the observed stamp on the FIRST tuple component,
//! so `logical` and `device_id` never matter. Cost: ≤1ms inflation per
//! causal hop, all inside the cap.
//!
//! Not persisted: re-learned from live traffic within seconds, and the
//! clamp is applied against current `now` at every read anyway.
//! See docs/superpowers/specs/2026-07-31-zeb-790-hlc-bounded-adoption-design.md §3.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// How far ahead of this device's own wall clock the mint may be pulled by
/// adopting a verified remote stamp. 5s = 5x the observed ZEB-788 failure
/// class (~1s skew), 12x under the tightest wall-time-coupled consumer
/// budget (the 60s invite/open-join forward windows). Task 8's
/// budget-relation test pins those margins.
pub const HLC_ADOPT_FORWARD_CAP_MS: u64 = 5_000;

/// 0 = nothing observed yet (wall_ms 0 is the epoch; no real stamp is 0,
/// and `merged_now` degenerates to the identity on 0 regardless).
#[derive(Clone, Debug, Default)]
pub struct HlcAdoptFloor(Arc<AtomicU64>);

impl HlcAdoptFloor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed: record a VERIFIED remote stamp's wall. Callers must sit
    /// strictly after the accept path's commit/record success.
    pub fn observe(&self, remote_wall_ms: u64) {
        self.0
            .fetch_max(remote_wall_ms.saturating_add(1), Ordering::Relaxed);
    }

    /// Read: the wall the mint should use instead of `wall_now_ms`.
    /// max(now, min(floor, now + CAP)) — see the case table in the spec §3.
    pub fn merged_now(&self, wall_now_ms: u64) -> u64 {
        let floor = self.0.load(Ordering::Relaxed);
        wall_now_ms.max(floor.min(wall_now_ms.saturating_add(HLC_ADOPT_FORWARD_CAP_MS)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_floor_is_identity() {
        let f = HlcAdoptFloor::new();
        assert_eq!(f.merged_now(1_000_000), 1_000_000);
        assert_eq!(f.merged_now(0), 0);
    }

    #[test]
    fn remote_behind_is_identity() {
        let f = HlcAdoptFloor::new();
        f.observe(999);
        assert_eq!(f.merged_now(5_000), 5_000, "floor 1000 <= now: identity");
    }

    #[test]
    fn adopts_within_cap_strictly_past_observed_wall() {
        let f = HlcAdoptFloor::new();
        let now = 1_000_000u64;
        f.observe(now + 600); // the ZEB-788 class: remote 600ms ahead
        assert_eq!(f.merged_now(now), now + 601, "floor = W+1, adopted");
    }

    #[test]
    fn clamps_beyond_cap() {
        let f = HlcAdoptFloor::new();
        let now = 1_000_000u64;
        f.observe(now + HLC_ADOPT_FORWARD_CAP_MS + 60_000); // hostile far-future
        assert_eq!(
            f.merged_now(now),
            now + HLC_ADOPT_FORWARD_CAP_MS,
            "damage bounded at CAP"
        );
    }

    #[test]
    fn boundary_w_equals_now_plus_cap_clamps_to_w() {
        // The contract is strict (W < now+CAP): at exactly now+CAP the +1
        // floor clamps TO W, not past it. Spec §2.
        let f = HlcAdoptFloor::new();
        let now = 1_000_000u64;
        let w = now + HLC_ADOPT_FORWARD_CAP_MS;
        f.observe(w);
        assert_eq!(f.merged_now(now), w, "not w+1: clamped");
    }

    #[test]
    fn observe_is_monotone_max() {
        let f = HlcAdoptFloor::new();
        f.observe(500);
        f.observe(300); // lower: no regression
        assert_eq!(f.merged_now(0), 501);
    }

    #[test]
    fn observe_saturates_at_u64_max() {
        let f = HlcAdoptFloor::new();
        f.observe(u64::MAX); // +1 must not wrap to 0
        let now = 1_000u64;
        assert_eq!(f.merged_now(now), now + HLC_ADOPT_FORWARD_CAP_MS);
    }

    #[test]
    fn clones_share_state() {
        let f = HlcAdoptFloor::new();
        let g = f.clone();
        g.observe(9_999);
        // Read with now = the observed wall: merged_now clamps against
        // `now + CAP`, so a tiny `now` would cap the answer — this is also
        // why the feed-site tests (Tasks 5-7) read merged_now(at.wall_ms).
        assert_eq!(f.merged_now(9_999), 10_000, "Arc-shared: feed via clone visible");
    }
}
```

- [ ] **Step 2: Add `mod hlc_adopt_floor;` to `src-tauri/src/lib.rs`** (alphabetical among the existing mod declarations).

- [ ] **Step 3: Run the module tests**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(hlc_adopt)'`
Expected: 8 PASS.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/hlc_adopt_floor.rs src-tauri/src/lib.rs
git commit -m "ZEB-790: HlcAdoptFloor — session-only bounded adoption floor (+1 rule, 5s cap)"
```

---

### Task 2: `NodeState` field + fresh floor per `start_node`

**Files:**
- Modify: `src-tauri/src/lib.rs:941-947` region (the `NodeState` struct — add the field right after `hlc_tracker`), `src-tauri/src/lib.rs:5166-5171` region (`start_node`, where the shared tracker is constructed), and the `NodeState`→guard assignment region near `lib.rs:12345` (`guard.hlc_tracker = tracker_for_state.clone()`).

**Interfaces:**
- Consumes: `HlcAdoptFloor` from Task 1.
- Produces: `NodeState.hlc_adopt_floor: crate::hlc_adopt_floor::HlcAdoptFloor` (NOT `Option` — always present, cheap Arc clone). A local `let adopt_floor = crate::hlc_adopt_floor::HlcAdoptFloor::new();` in `start_node` next to the tracker construction, which Tasks 3–7 clone into engines/ctxs. Every later task obtains the floor either from a `NodeState` guard (`guard.hlc_adopt_floor.clone()`) or from this `start_node` local threaded at construction.

- [ ] **Step 1: Add the struct field**

```rust
    /// ZEB-790: bounded causal-adoption floor. Fed by the verified accept
    /// paths, read by every mint seam. Reset to a fresh floor per
    /// start_node (session-only — see hlc_adopt_floor.rs module docs).
    hlc_adopt_floor: crate::hlc_adopt_floor::HlcAdoptFloor,
```

Run `cd src-tauri && cargo check --locked --all-targets --features test-fixtures` — `--all-targets` is load-bearing here: without it Cargo skips the `#[cfg(test)]` code that holds most `NodeState { .. }` construction sites, so they wouldn't be flagged. The compiler then flags every construction site; initialize each with `hlc_adopt_floor: crate::hlc_adopt_floor::HlcAdoptFloor::new(),` (if `NodeState` has a `Default`/constructor fn, add it there once instead).

- [ ] **Step 2: Construct a fresh floor in `start_node`**

Immediately after the `let tracker = std::sync::Arc::new(...)` at `lib.rs:5166-5171`:

```rust
                    // ZEB-790: fresh adoption floor per node session.
                    let adopt_floor = crate::hlc_adopt_floor::HlcAdoptFloor::new();
```

And where the tracker is lifted onto the guard (near `lib.rs:12345`, `guard.hlc_tracker = tracker_for_state.clone();`), add:

```rust
                    guard.hlc_adopt_floor = adopt_floor.clone();
```

(Thread `adopt_floor` to that point the same way `tracker_for_state` travels — likely a sibling local at `lib.rs:11603`.)

- [ ] **Step 3: Verify compile**

Run: `cd src-tauri && cargo check --locked --all-targets --features test-fixtures`
Expected: clean (field exists, initialized everywhere, otherwise unused).

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "ZEB-790: NodeState.hlc_adopt_floor, fresh per start_node"
```

---

### Task 3: Mint seams adopt the floor (the mechanical sweep)

**Files:**
- Modify: `src-tauri/src/dm_outbox.rs:3295-3305` (`reserve_next_hlc_for_device`), `src-tauri/src/fleet_sync.rs:1113-1166` (`mint_next_hlc` / `peek_next_hlc` / `compute_next_hlc`), plus every call site the compiler flags (~90 in `lib.rs`, `community_channel_log_engine.rs`, `community_voting_log_engine.rs`, `community_fork.rs`, `community_membership.rs`, `community_device_retire_deposit.rs`, `profile_broadcast.rs`, `iroh_friend_acceptor.rs`, `iroh_pex_acceptor.rs`, `notes_commands.rs`, `community_relay_prod.rs`, `iroh_butler_acceptor.rs`).
- Test: `src-tauri/src/dm_outbox.rs` (new `#[cfg(test)]` tests beside the existing reserve tests near `dm_outbox.rs:8752`).

**Interfaces:**
- Consumes: `HlcAdoptFloor` (Task 1), `NodeState.hlc_adopt_floor` / `start_node`'s `adopt_floor` local (Task 2).
- Produces: the new signatures every later task compiles against:

```rust
pub async fn reserve_next_hlc_for_device<T: DeviceHlcStore>(
    tracker: &std::sync::Arc<tokio::sync::Mutex<T>>,
    floor: &crate::hlc_adopt_floor::HlcAdoptFloor,
    device_id: &str,
    wall_now_ms: u64,
) -> Hlc
// fleet_sync:
pub async fn mint_next_hlc(tracker: &Arc<Mutex<ReplayTracker<String, Hlc>>>, floor: &crate::hlc_adopt_floor::HlcAdoptFloor, device_id: &str) -> Hlc
pub fn peek_next_hlc(tracker_snapshot: &BTreeMap<String, Hlc>, floor: &crate::hlc_adopt_floor::HlcAdoptFloor, device_id: &str) -> Hlc
fn compute_next_hlc(tracker: &BTreeMap<String, Hlc>, device_id: &str, wall_ms: u64) -> Hlc  // UNCHANGED (pure; callers pass merged wall)
```

- [ ] **Step 1: Write the failing tests** (in `dm_outbox.rs`'s test module, beside `reserve_next_hlc_for_device_advances_tracker_atomically`):

```rust
    #[tokio::test]
    async fn reserve_adopts_verified_future_stamp_within_cap() {
        // ZEB-790: the ZEB-788 621ms inversion, made impossible. A mint
        // that follows a verified-and-applied remote stamp W (W < now+CAP)
        // must exceed W — even when the remote carried logical > 0.
        let tracker = std::sync::Arc::new(tokio::sync::Mutex::new(
            std::collections::BTreeMap::<String, Hlc>::new(),
        ));
        let floor = crate::hlc_adopt_floor::HlcAdoptFloor::new();
        let now = 1_785_021_611_000u64; // "Ildwyn's clock"
        let remote_wall = now + 600; // "AVALON's stamp", 600ms ahead
        floor.observe(remote_wall); // what the engines do post-verify
        let minted = reserve_next_hlc_for_device(&tracker, &floor, "ildwyn-dev", now).await;
        assert_eq!(minted.wall_ms, remote_wall + 1, "wall strictly exceeds W");
        assert_eq!(minted.logical, 0);
        // Strictly after the remote stamp for ANY remote logical (the +1 rule):
        let remote = Hlc { wall_ms: remote_wall, logical: u32::MAX, device_id: "avalon-dev".into() };
        assert!(minted.is_strictly_newer_than(&remote));
    }

    #[tokio::test]
    async fn reserve_clamps_beyond_cap_and_stays_device_monotone() {
        let tracker = std::sync::Arc::new(tokio::sync::Mutex::new(
            std::collections::BTreeMap::<String, Hlc>::new(),
        ));
        let floor = crate::hlc_adopt_floor::HlcAdoptFloor::new();
        let now = 1_000_000u64;
        floor.observe(now + 3_600_000); // hostile: one hour ahead
        let a = reserve_next_hlc_for_device(&tracker, &floor, "dev", now).await;
        assert_eq!(
            a.wall_ms,
            now + crate::hlc_adopt_floor::HLC_ADOPT_FORWARD_CAP_MS,
            "clamped to CAP"
        );
        // Per-device strict monotonicity survives adoption:
        let b = reserve_next_hlc_for_device(&tracker, &floor, "dev", now).await;
        assert!(b.is_strictly_newer_than(&a), "wall tied at clamp -> logical bumps");
    }

    #[tokio::test]
    async fn reserve_with_empty_floor_is_todays_behavior() {
        let tracker = std::sync::Arc::new(tokio::sync::Mutex::new(
            std::collections::BTreeMap::<String, Hlc>::new(),
        ));
        let floor = crate::hlc_adopt_floor::HlcAdoptFloor::new();
        let minted = reserve_next_hlc_for_device(&tracker, &floor, "dev", 42_000).await;
        assert_eq!(minted.wall_ms, 42_000, "identity: no observed remote");
        assert_eq!(minted.logical, 0);
    }
```

- [ ] **Step 2: Implement the seam change in `dm_outbox.rs`**

In `reserve_next_hlc_for_device` (keep the doc comment; append one paragraph noting the ZEB-790 merge):

```rust
pub async fn reserve_next_hlc_for_device<T: DeviceHlcStore>(
    tracker: &std::sync::Arc<tokio::sync::Mutex<T>>,
    floor: &crate::hlc_adopt_floor::HlcAdoptFloor,
    device_id: &str,
    wall_now_ms: u64,
) -> Hlc {
    // ZEB-790: bounded causal adoption — the floor read is a lock-free
    // atomic, so the ZEB-267 single-lock atomicity is unchanged.
    let wall_now_ms = floor.merged_now(wall_now_ms);
    let mut t = tracker.lock().await;
    let prev = t.last_for(device_id).cloned();
    let next = next_hlc(prev.as_ref(), wall_now_ms, device_id);
    t.record_local(device_id, next.clone());
    next
}
```

And in `fleet_sync.rs`, `mint_next_hlc` / `peek_next_hlc` gain the same `floor: &crate::hlc_adopt_floor::HlcAdoptFloor` parameter and apply `floor.merged_now(...)` to the wall before calling `compute_next_hlc` (which stays pure/unchanged):

```rust
pub async fn mint_next_hlc(
    tracker: &Arc<Mutex<ReplayTracker<String, Hlc>>>,
    floor: &crate::hlc_adopt_floor::HlcAdoptFloor,
    device_id: &str,
) -> Hlc {
    let wall_ms = floor.merged_now(now_wall_ms());
    let mut tracker = tracker.lock().await;
    let now = compute_next_hlc(tracker.accepted(), device_id, wall_ms);
    tracker.observe_local(now.clone());
    now
}

pub fn peek_next_hlc(
    tracker_snapshot: &BTreeMap<String, Hlc>,
    floor: &crate::hlc_adopt_floor::HlcAdoptFloor,
    device_id: &str,
) -> Hlc {
    compute_next_hlc(tracker_snapshot, device_id, floor.merged_now(now_wall_ms()))
}
```

- [ ] **Step 3: Sweep the call sites (compiler-driven).** Run `cargo check --locked --all-targets --features test-fixtures` and fix every flagged site by family:

1. **`lib.rs` IPC handlers (~60):** each already clones `hlc_tracker` out of the `NodeState` guard. In the same guard scope add `let adopt_floor = guard.hlc_adopt_floor.clone();` and pass `&adopt_floor`. Example shape (the `send_dm` handler near `lib.rs:14586`): where today it computes `wall_now_ms` and calls the mint, insert the clone next to the tracker clone and pass it through.
2. **Engines with a tracker field** (`ChannelLogEngine` at `community_channel_log_engine.rs:483`, `VotingLogEngine` via `community_voting_log_engine.rs:297/:361`): add an `adopt_floor: crate::hlc_adopt_floor::HlcAdoptFloor` field to the engine struct AND its params struct (`ChannelLogEngineParams` near `:448`, `VotingLogEngineParams`); pass `&self.adopt_floor` at the mint calls (`:1062`, `:1274`, `:600`); populate the params at every spawn/construction site the compiler flags (source: the `NodeState` guard or the `start_node` local from Task 2).
3. **Functions taking the tracker as an argument** (`community_fork.rs` mints at `:475/:516/:691/:818/:969`, `community_membership.rs:6151/:6259`, `ChannelLogRegistry::spawn`/`reconcile_from_state` at `community_channel_log_engine.rs:3032-3040`): widen the function signature with `floor: &crate::hlc_adopt_floor::HlcAdoptFloor` (or add the field to their ctx struct if they take one) and thread from the caller.
4. **Acceptor/broadcast ctx structs** (`profile_broadcast.rs:445` trait impl + its ctx, `iroh_friend_acceptor.rs:1991` + struct field near `:1598`, `iroh_pex_acceptor.rs:359` + struct field near `:82`, `community_device_retire_deposit.rs:248-255` ctx): add the floor field, populate at construction (`lib.rs:10104/:10258/:10352/:6967`), use in their `next_hlc` fns — these call the pure `dm_outbox::next_hlc` or `reserve_next_hlc_for_device`; for the pure-fn callers apply `floor.merged_now(...)` to the wall they pass.
5. **`send_dm`'s split mint** (`lib.rs:14596` reads `prev` then calls pure `dm_outbox::next_hlc` at `dm_outbox.rs:1008` via `DmOutbox::send_dm`): apply `merged_now` to the `wall_now_ms` the handler computes before passing it in. Do NOT change the pure `next_hlc(prev, wall, device)` itself.
6. **Separate per-engine trackers** (notes/dm_inbox/relay_hold/relay_optin/dm_outhold/fleet_net/owner_trust/owner_quorum/fleet_keys, constructed in `start_node` `lib.rs:5498-6552`; direct mints at `notes_commands.rs:78/:87/:135`, `community_relay_prod.rs:191`, `iroh_butler_acceptor.rs:364`): they call `fleet_sync::mint_next_hlc`/`peek_next_hlc` — thread the same node-wide `adopt_floor` (one floor per node, NOT per engine).

Repeat `cargo check` until clean. **Do not change any replay-lane logic, `record_local`'s debug_assert, or the pure `next_hlc`/`compute_next_hlc`/`community_hlc_tick` functions.**

- [ ] **Step 4: Run the scoped tests**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(reserve_) or test(hlc_adopt) or test(next_hlc) or test(hlc)'`
Expected: all PASS — the three new tests plus every existing hlc/mint test (empty floors = identity; `lib.rs:37370/:44655`'s `wall == wall_now` assertions hold).

- [ ] **Step 5: Commit**

```bash
git add -A src-tauri/src
git commit -m "ZEB-790: mint seams adopt the floor — reserve/fleet mints take &HlcAdoptFloor (empty floor = identity)"
```

---

### Task 4: Community-state mint seam

**Files:**
- Modify: `src-tauri/src/community_state_sync.rs:2006` (`struct InternalCtx` — add field), `:3356-3377` (`next_hlc(ctx)`), plus the `InternalCtx { .. }` construction site(s) (grep `InternalCtx {` — inside `spawn_engine`) and the public engine-spawn params that reach it (compiler-flagged up to the `lib.rs` caller, which supplies the Task-2 floor).

**Interfaces:**
- Consumes: `HlcAdoptFloor` (Task 1), node floor (Task 2).
- Produces: `InternalCtx.adopt_floor: crate::hlc_adopt_floor::HlcAdoptFloor`; community mints adopt. `community_hlc_tick` stays pure `(prev, wall_ms, device_id)` — the ZEB-750 non-vacuity tests (`community_state_sync.rs:6310/:6342`) must not change.

- [ ] **Step 1: Add the field** to `InternalCtx` (after `tracker`):

```rust
    /// ZEB-790: node-wide bounded causal-adoption floor (shared, not
    /// per-community). Fed at step 14 (Task 5); read in next_hlc.
    adopt_floor: crate::hlc_adopt_floor::HlcAdoptFloor,
```

- [ ] **Step 2: Merge in `next_hlc(ctx)`** — replace the `wall_ms` computation's last line:

```rust
    let wall_ms = ctx.adopt_floor.merged_now(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
    );
```

(Everything else in `next_hlc` — the lock, `accepted_from(&local)`, `community_hlc_tick`, `observe_local` — unchanged.)

- [ ] **Step 3: Thread construction.** `cargo check --locked --all-targets --features test-fixtures`; populate `adopt_floor` at every `InternalCtx { .. }` literal (and any spawn-params struct between it and `lib.rs`) from the node floor. Test-only harnesses construct `HlcAdoptFloor::new()`.

- [ ] **Step 4: Run scoped tests**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(community_hlc) or test(zeb750) or test(community_sync)'`
Expected: PASS (notably `hlc_tick_ties_instead_of_manufacturing_a_wall_advance_at_saturation_zeb750` and `community_hlc_tick_advances_on_wall_tie_and_backward_step_zeb750` — untouched pure fn).

- [ ] **Step 5: Commit**

```bash
git add -A src-tauri/src
git commit -m "ZEB-790: community-state mint adopts the node floor (community_hlc_tick stays pure)"
```

---

### Task 5: Feed site 1 — community-state commit

**Files:**
- Modify: `src-tauri/src/community_state_sync.rs:4457-4460` (step 14, the `tracker.commit(replay_ticket)` block).
- Test: `src-tauri/src/community_state_sync.rs` test module (donor harness: `a_self_echo_is_distinguished_from_a_replay_zeb750` at `:6369` — read it first and reuse its engine/publish setup).

**Interfaces:**
- Consumes: `InternalCtx.adopt_floor` (Task 4).
- Produces: verified community publishes feed the floor.

- [ ] **Step 1: Add the feed** — inside/immediately after the step-14 block:

```rust
    {
        let mut tracker = ctx.tracker.lock().await;
        tracker.commit(replay_ticket);
    }
    // ZEB-790: feed the adoption floor ONLY here — after commit, i.e.
    // after sig-verify + membership-at-HLC + merge all succeeded. A
    // rejection path returns before this line, so a rejected frame can
    // never move the floor (same invariant as the tracker itself).
    ctx.adopt_floor.observe(payload.at.wall_ms);
```

- [ ] **Step 2: Write the tests** (reuse the `:6369` donor's setup; the assertions are the deliverable):

```rust
    // In the accepted-publish path of the donor harness, after the engine
    // reports Applied for a publish stamped `at`. NOTE: read with
    // `merged_now(at.wall_ms)` — merged_now clamps against `now + CAP`,
    // so merged_now(0) would return CAP, not the observed wall.
    assert_eq!(
        ctx.adopt_floor.merged_now(at.wall_ms),
        at.wall_ms + 1,
        "accepted publish feeds the floor (+1 rule)"
    );

    // And in a rejection-path test (reuse any existing rejected-publish
    // harness — e.g. the ZEB-256 spoof test via tracker_arc at :1363, or a
    // PublisherNotJoined case): after the engine reports the rejection:
    assert_eq!(
        ctx.adopt_floor.merged_now(0),
        0,
        "rejected publish must NOT move the floor"
    );
```

Name them `accepted_publish_feeds_adopt_floor` and `rejected_publish_does_not_feed_adopt_floor`. (If the harness doesn't expose `ctx`, expose the floor the same way `tracker_arc` (`:1363`) exposes the tracker for tests — a `#[cfg(any(test, feature = "test-fixtures"))]` accessor.)

- [ ] **Step 3: Run**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(adopt_floor)'`
Expected: both PASS.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/community_state_sync.rs
git commit -m "ZEB-790: community-state commit feeds the adoption floor (rejections inert)"
```

---

### Task 6: Feed site 2 — channel-log engine

**Files:**
- Modify: `src-tauri/src/community_channel_log_engine.rs` — the 2c block at `:1708-1724` (`check_and_advance` success) in `process_inbound_packet`; the engine already holds the floor field from Task 3 family 2.
- Test: `src-tauri/src/community_channel_log_engine.rs` test module (donor: `receive_replay_drops_silently` at `:4842` — full engine harness with signed events).

**Interfaces:**
- Consumes: `ChannelLogEngine.adopt_floor` (Task 3).
- Produces: verified channel events feed the floor — the path where the ZEB-788 inversion was observed.

- [ ] **Step 1: Add the feed** — immediately after the 2c block succeeds (after the closing brace at `:1724`, before step 3 Append):

```rust
        // ZEB-790: feed the adoption floor ONLY after 2c — decrypt,
        // sig-verify (2b) and the authoritative replay advance (2c) all
        // succeeded. Every earlier `return` (garbage, replay, invalid)
        // leaves the floor untouched. Use the same event accessor
        // `would_accept` reads the stamp through.
        self.adopt_floor.observe(event.at.wall_ms);
```

(If the event type exposes the stamp via a method rather than a field, match whatever `ChannelLogReplayTracker::would_accept` (`community_channel_log.rs:1027`) uses.)

- [ ] **Step 2: Write the tests** (extend the `:4842` donor harness):

```rust
    // verified_inbound_feeds_adopt_floor: run the donor's accepted-event
    // flow with an event stamped `at`; then (read with the wall as `now`
    // — see the Task 5 note on merged_now's clamp):
    assert_eq!(engine.adopt_floor.merged_now(at.wall_ms), at.wall_ms + 1);

    // sig_failed_inbound_does_not_feed_adopt_floor: corrupt the event
    // signature (or author key) so 2b fails; then:
    assert_eq!(engine.adopt_floor.merged_now(0), 0, "floor still empty");

    // replayed_inbound_does_not_feed_floor_twice: deliver the SAME event
    // twice; merged_now(at.wall_ms) still equals at.wall_ms + 1
    // (idempotent max), and the replay drop counter advanced (reuse
    // replay_drop_count()).
```

- [ ] **Step 3: Run**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(adopt_floor) or test(receive_replay)'`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/community_channel_log_engine.rs
git commit -m "ZEB-790: channel-log ingest feeds the adoption floor post-2c (sig-fail/replay inert)"
```

---

### Task 7: Feed site 3 — owner-fleet commit

**Files:**
- Modify: `src-tauri/src/fleet_sync.rs:1421-1423` (step 9 `commit`); the ctx struct holding `replay_tracker` gains `adopt_floor` (compiler-flagged; populated where engines are built in `start_node`, Task 3 family 6 already threads the floor to those constructors).
- Test: `src-tauri/src/owner_state_sync.rs` test module (donor: `subscriber_accepts_strictly_newer_hlc_and_updates_tracker` at `:1102`).

**Interfaces:**
- Consumes: node floor (Task 2/3).
- Produces: accepted sibling state-root publishes feed the floor.

- [ ] **Step 1: Add the feed** after the step-9 commit:

```rust
    // 9. NOW advance the watermark — only after a successful apply. The
    //    ticket has ridden every fallible step above to get here.
    ctx.replay_tracker.lock().await.commit(ticket);
    // ZEB-790: verified sibling stamp — feed the adoption floor (post-
    // commit only; every earlier Dropped return leaves it untouched).
    ctx.adopt_floor.observe(payload.at.wall_ms);
```

- [ ] **Step 2: Tests** — extend the `:1102` donor: after an accepted publish stamped `at`, `assert_eq!(floor.merged_now(at.wall_ms), at.wall_ms + 1)` (read with the wall as `now` — see the Task 5 note on merged_now's clamp); after a stale/duplicate publish (the donor family has one), `assert_eq!(floor.merged_now(0), 0)` (floor still empty). Names: `accepted_sibling_publish_feeds_adopt_floor`, `stale_sibling_publish_does_not_feed_adopt_floor`.

- [ ] **Step 3: Run**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(adopt_floor)'`
Expected: all feed tests (Tasks 5-7) PASS.

- [ ] **Step 4: Commit**

```bash
git add -A src-tauri/src
git commit -m "ZEB-790: fleet commit feeds the adoption floor"
```

---

### Task 8: Consumer updates — the ZEB-792 comment + budget-relation pin

**Files:**
- Modify: `src-tauri/src/community_membership.rs:5490-5509` (the `ADMIN_PROPOSAL_MAX_FORWARD_SKEW_MS` doc).
- Test: `src-tauri/src/hlc_adopt_floor.rs` test module.

**Interfaces:**
- Consumes: `HLC_ADOPT_FORWARD_CAP_MS` (Task 1); `community_membership::ADMIN_PROPOSAL_MAX_FORWARD_SKEW_MS` (pub const, existing).
- Produces: honest doc + a pin nobody can silently widen CAP past.

- [ ] **Step 1: Rewrite the final doc paragraph** (keep everything above it):

```rust
/// ZEB-790 (bounded adoption): the `now_ms` side is peer-influenced by at
/// most `hlc_adopt_floor::HLC_ADOPT_FORWARD_CAP_MS + 1` ms — verified
/// peers can pull this device's minted wall forward up to the cap, never
/// further (see hlc_adopt_floor.rs). The effective forward bound is
/// therefore 30 min + CAP (~0.3% weakening), which the
/// `adopt_cap_stays_far_below_consumer_budgets` test pins.
pub const ADMIN_PROPOSAL_MAX_FORWARD_SKEW_MS: u64 = 30 * 60 * 1000;
```

- [ ] **Step 2: Add the budget-relation test** (in `hlc_adopt_floor.rs`):

```rust
    #[test]
    fn adopt_cap_stays_far_below_consumer_budgets() {
        // ZEB-790 spec §6.2. Widening CAP past these relations invalidates
        // the blast-radius analysis — re-run it before touching this test.
        // 60_000 = the invite/open-join forward windows
        // (open_join_admit.rs `now + 60_000`, community_invite.rs same).
        assert!(HLC_ADOPT_FORWARD_CAP_MS * 12 <= 60_000);
        assert!(
            HLC_ADOPT_FORWARD_CAP_MS * 360
                <= crate::community_membership::ADMIN_PROPOSAL_MAX_FORWARD_SKEW_MS
        );
    }
```

- [ ] **Step 3: Run**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(adopt_cap) or test(admin_proposal)'`
Expected: PASS (including the existing `:7165-7205` boundary tests, untouched).

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/community_membership.rs src-tauri/src/hlc_adopt_floor.rs
git commit -m "ZEB-790: rewrite ZEB-792 skew-bound premise; pin CAP against consumer budgets"
```

---

### Task 9: Backend DTO full tuple + full-tuple pre-sort

**Files:**
- Modify: `src-tauri/src/lib.rs` — struct defs for `DeliberationStatementExport`, `Tier3PollExport`, `Tier3PollSummary` (grep `struct DeliberationStatementExport` etc.); projections at `:55592-55600`, `:55641-55657`, `:55763-55773`; pre-sort at `:55776`.

**Interfaces:**
- Consumes: `s.created_at_hlc: Hlc` / `t3.meta.poll_create_hlc: Hlc` (existing).
- Produces: serde-camelCase fields the FE (Task 10) reads: `createdAtHlcLogical: u32` + `createdAtHlcDeviceId: String` on statements; `pollCreateHlcLogical` + `pollCreateHlcDeviceId` on both poll DTOs. Existing `*HlcMs` fields kept unchanged.

- [ ] **Step 1: Add fields to the three structs** (snake_case in Rust; the structs' existing serde rename gives camelCase):

```rust
    // ZEB-790: full HLC tuple so the UI can order deterministically
    // (wallMs alone ties on same-ms stamps — the ZEB-244 lesson).
    created_at_hlc_logical: u32,
    created_at_hlc_device_id: String,
```

(and `poll_create_hlc_logical` / `poll_create_hlc_device_id` on `Tier3PollExport` + `Tier3PollSummary`).

- [ ] **Step 2: Populate in the three projections**, e.g. at `:55596`:

```rust
                created_at_hlc_ms: s.created_at_hlc.wall_ms as i128,
                created_at_hlc_logical: s.created_at_hlc.logical,
                created_at_hlc_device_id: s.created_at_hlc.device_id.clone(),
```

(same pattern with `t3.meta.poll_create_hlc` at `:55647` and `:55769`). Fix any Rust test constructing these structs (compiler-flagged) with `0` / `String::new()` literals.

- [ ] **Step 3: Full-tuple pre-sort** at `:55776`:

```rust
    summaries.sort_by(|a, b| {
        (b.poll_create_hlc_ms, b.poll_create_hlc_logical, &b.poll_create_hlc_device_id)
            .cmp(&(a.poll_create_hlc_ms, a.poll_create_hlc_logical, &a.poll_create_hlc_device_id))
    });
```

- [ ] **Step 4: Run scoped tests**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(tier3)'`
Expected: PASS (fields are additive; camelCase keys are new, existing keys unchanged).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "ZEB-790: tier-3 DTOs carry the full HLC tuple; backend pre-sort uses it"
```

---

### Task 10: Frontend — full-tuple ordering + tests

**Files:**
- Modify: `src/lib/types/voting.ts:494-504` (`DeliberationStatementExport`), `:632-645` (`Tier3PollSummary`), and the `Tier3PollExport` interface (same file, above `:618`); `src/lib/components/CharterView.svelte:83-88`; `src/lib/components/StatementVoteList.svelte:39-41`.
- Test: `src/lib/components/__tests__/StatementVoteList.test.ts`, `src/lib/components/__tests__/CharterView.test.ts`.

**Interfaces:**
- Consumes: Task 9's camelCase fields; `compareHlc` from `src/lib/hlc.ts:14` (`HlcLike { wallMs, logical, deviceId }`).
- Produces: deterministic full-tuple ordering on both surfaces. **New TS fields are optional** (`?: number` / `?: string`) with `?? 0` / `?? ''` fallbacks — ~10 existing test files build these DTO literals and must not need editing.

- [ ] **Step 1: Extend the three interfaces** (optional fields, doc-commented):

```typescript
  createdAtHlcMs: number;
  /** ZEB-790: full-tuple tiebreak (optional — absent in older fixtures). */
  createdAtHlcLogical?: number;
  createdAtHlcDeviceId?: string;
```

(and `pollCreateHlcLogical?` / `pollCreateHlcDeviceId?` on `Tier3PollSummary` + `Tier3PollExport`.)

- [ ] **Step 2: Switch the two sorts to `compareHlc`.**

`StatementVoteList.svelte` (add `import { compareHlc } from '../hlc';` to the script block):

```typescript
  let sortedStatements = $derived(
    [...detail.deliberationStatements].sort((a, b) =>
      compareHlc(
        { wallMs: a.createdAtHlcMs, logical: a.createdAtHlcLogical ?? 0, deviceId: a.createdAtHlcDeviceId ?? '' },
        { wallMs: b.createdAtHlcMs, logical: b.createdAtHlcLogical ?? 0, deviceId: b.createdAtHlcDeviceId ?? '' },
      ),
    ),
  );
```

`CharterView.svelte` (same import):

```typescript
  let finalized = $derived(
    (polls ?? [])
      .filter((p) => p.stage === 'fi')
      .slice()
      .sort((a, b) =>
        compareHlc(
          { wallMs: a.pollCreateHlcMs, logical: a.pollCreateHlcLogical ?? 0, deviceId: a.pollCreateHlcDeviceId ?? '' },
          { wallMs: b.pollCreateHlcMs, logical: b.pollCreateHlcLogical ?? 0, deviceId: b.pollCreateHlcDeviceId ?? '' },
        ),
      ),
  );
```

- [ ] **Step 3: Add ordering tests** (mirror each file's existing `render(...)` call for props). `StatementVoteList.test.ts`:

```typescript
  it('orders same-ms statements by logical then deviceId (ZEB-790)', () => {
    const a: DeliberationStatementExport = {
      ...stmt, statementEventHash: 'cc'.repeat(32),
      text: 'second-by-logical', createdAtHlcMs: 1_700_000_020_000,
      createdAtHlcLogical: 2, createdAtHlcDeviceId: 'dev-a',
    };
    const b: DeliberationStatementExport = {
      ...stmt, statementEventHash: 'dd'.repeat(32),
      text: 'first-by-logical', createdAtHlcMs: 1_700_000_020_000,
      createdAtHlcLogical: 1, createdAtHlcDeviceId: 'dev-z',
    };
    const adapter = new VotingAdapter();
    const { container } = render(StatementVoteList, {
      props: {
        detail: { ...baseDetail, deliberationStatements: [a, b] },
        adapter, myAddr: otherAddr, onChange: () => {},
      },
    });
    const text = container.textContent ?? '';
    expect(text.indexOf('first-by-logical')).toBeGreaterThan(-1);
    expect(text.indexOf('first-by-logical')).toBeLessThan(text.indexOf('second-by-logical'));
  });
```

`CharterView.test.ts`: same shape — two `stage: 'fi'` summaries with equal `pollCreateHlcMs`, `pollCreateHlcLogical: 2`/`1`, distinct `proposalText`, assert `container.textContent` index order (mirror that file's existing props for the render call).

- [ ] **Step 4: Run frontend gates**

Run (repo root): `npx vitest run src/lib/components/__tests__/StatementVoteList.test.ts src/lib/components/__tests__/CharterView.test.ts && npx tsc --noEmit`
Expected: PASS / clean.

- [ ] **Step 5: Commit**

```bash
git add src/lib/types/voting.ts src/lib/components/CharterView.svelte src/lib/components/StatementVoteList.svelte src/lib/components/__tests__
git commit -m "ZEB-790: governance sorts use the full HLC tuple via compareHlc (+ ordering tests)"
```

---

### Task 11: `Hlc` doc rewrite + full gates

**Files:**
- Modify: `src-tauri/src/owner_state_types.rs:311-322` (the `Hlc` doc-comment — keep the wire-format and field-order paragraphs verbatim; add the guarantee paragraph).

**Interfaces:** none new — this is docs + verification.

- [ ] **Step 1: Add the guarantee paragraph** to the `Hlc` doc (after the existing wire-format paragraph, before the field-order paragraph):

```rust
/// ## What this clock guarantees (ZEB-790)
///
/// 1. **Per-device strict monotonicity** — every mint strictly exceeds
///    this device's previous stamp (structural; `HlcTick::next`).
/// 2. **Bounded causal adoption** — if this device verified-and-applied
///    a remote event with wall `W` before minting, and `W` is less than
///    `hlc_adopt_floor::HLC_ADOPT_FORWARD_CAP_MS` ahead of local now,
///    the next mint's wall exceeds `W` (see `hlc_adopt_floor.rs`).
///
/// Beyond the cap this is NOT a full Kulkarni HLC: cross-device
/// happens-before degrades to per-device order. Consumers that treat
/// `wall_ms` as ≈real time (skew bounds, expiry windows, invite
/// freshness) tolerate the cap by construction — the
/// `adopt_cap_stays_far_below_consumer_budgets` test pins the margins.
```

- [ ] **Step 2: Full local gates** (this is the CI-parity sweep — budget ~an hour on a warm cache):

```bash
cd src-tauri && cargo fmt --all -- --check \
  && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings \
  && cargo nextest run --locked --workspace --all-targets --features test-fixtures
cd .. && npx tsc --noEmit && npx vitest run
```

Expected: all green. If `cargo fmt --check` fails, run `cargo fmt --all` and re-check.

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/owner_state_types.rs
git commit -m "ZEB-790: Hlc doc states the real guarantee (per-device monotone + bounded adoption)"
```

---

## Self-Review Notes (written into the plan for the executor)

- **Spec coverage:** §3 floor → Task 1; §5 mint seams → Tasks 3-4; §4 feeds → Tasks 5-7; §6 consumers → Task 8; §7 UI → Tasks 9-10; §2/§7-docs → Task 11. Spec §8 tests are distributed into their owning tasks (the ZEB-788 repro is Task 3's first test).
- **Ordering:** mint plumbing (3-4) lands before feeds (5-7) — an empty floor is the identity, so every intermediate commit is green and behavior-neutral until the first feed lands.
- **Do-not-touch list:** upstream `harmony-crdt-sync` (lockstep rev), `Hlc` fields/order, replay-lane logic, `community_hlc_tick`/`compute_next_hlc`/pure `next_hlc` bodies, `record_local`'s debug_assert, wire fixtures.
