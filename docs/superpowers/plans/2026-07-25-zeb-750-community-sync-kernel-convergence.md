# ZEB-750 community_state_sync Kernel Convergence Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `community_state_sync.rs` a `harmony-crdt-sync` kernel adopter — replay admission, HLC tick, and debounce — reaching parity with `fleet_sync.rs`.

**Architecture:** Three swaps in dependency order. The runtime replay type becomes core `ReplayTracker<(OwnerAddr, String), Hlc>` while `CommunityRootHlcTracker` is demoted to a pure serde DTO at the persistence boundary, so `replay.cbor` and its byte-pin fixtures are unchanged by construction. Removing `record` forces `next_hlc`'s two tracker touches to migrate in Task 1; Task 2 then swaps only the arithmetic between them. Task 3 is independent.

**Tech Stack:** Rust, tokio, `harmony-crdt-sync` (core `main` `4eb42086`), `cargo nextest`, serde/ciborium canonical CBOR.

**Spec:** `docs/superpowers/specs/2026-07-25-zeb-750-community-sync-kernel-convergence-design.md`

## Global Constraints

- Base: client `main` `48dffce7`, branch `zeb-750-community-sync-convergence`. Core `main` `4eb42086`.
- **Zero core changes.** Every kernel already exists in `harmony-crdt-sync`. Do not edit the `harmony` repo.
- **Zero fixture regeneration.** `src-tauri/tests/wire_format/community_sync_fixtures.rs` and `community_fixtures.rs` must pass untouched. A regenerated fixture is a failure, not a fix.
- `replay.cbor`'s on-disk CBOR shape must not change: `CommunityRootHlcTracker { per_device: BTreeMap<(OwnerAddr, String), Hlc> }`, `CanonicalPayload`, `BTreeMap` key order.
- Do not touch: `CommunitySyncRegistry`'s multi-instance design, the membership-gated verify pipeline and its TOCTOU re-check, `CommunityMembershipDelta` notifications, ZEB-761's retry policy.
- Every cargo command runs from `src-tauri/`.
- Full gate before PR: `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`, `cargo nextest run --locked --workspace --all-targets --features test-fixtures`.
- Iterative gates use `scripts/test-select --context task` (`--context round` for PR converge rounds); the full sweep is only for the final pre-PR run. **Paste the printed `round=… bucket=…` summary line into the task report** so the selection is auditable — a selective run whose bucket is not recorded cannot be distinguished after the fact from a full one. `scripts/test-select` exits and demands `--full` if the branch touched `Cargo.toml` / `Cargo.lock` / `.cargo/` / `vendor/`; this branch touches none of those, so selection stays valid throughout.
- Each new test needs a **negative control** — revert the fix, confirm the test fails, restore. Record the observed failure message in the PR body.

## File Structure

| File | Responsibility | Change |
|---|---|---|
| `src-tauri/src/community_state_sync.rs` | the engine; all three kernels land here | Modify |
| `src-tauri/src/community_state_persist.rs` | `save_replay`/`load_replay` over the DTO | Unchanged (signatures already take `&CommunityRootHlcTracker`) |
| `src-tauri/tests/community_sync/community_sync_engine_unit.rs` | engine-level behaviour tests | Modify |

All production changes are in one file. That file is 8270 lines, so **anchor every edit at a quoted use-site and build scoped** (`cargo check -p harmony-app --features test-fixtures`) rather than eye-counting braces.

---

### Task 1: Adopt core `ReplayTracker` as the runtime replay type

**Files:**
- Modify: `src-tauri/src/community_state_sync.rs` — DTO demotion (`:823`-`:871`), field types (`:1017`, `:1136`, `:1977`), `tracker_arc` (`:1340`), `tracker_snapshot` (`:5486`), load (`:5153`), save (`:4188`, `:4213`), `next_hlc` (`:3216`, `:3255`), `handle_incoming_publish` (`:3699`-`:3704`, `:4053`-`:4056`), inline test constructions (`:7156`, `:7191`, `:7344`, `:7460`, `:7485`, `:7788`)
- Test: `src-tauri/src/community_state_sync.rs` (inline `#[cfg(test)]`)

**Interfaces:**
- Consumes: core `harmony_crdt_sync::{Admission, CommitTicket, ReplayTracker}`.
- Produces: `type CommunityReplayTracker = ReplayTracker<(OwnerAddr, String), Hlc>;` — Tasks 2 uses `accepted_from(&local)` and `observe_local(hlc)` on it. `CommunityRootHlcTracker` survives as the DTO with its `per_device` field public.

- [ ] **Step 1: Write the failing round-trip test**

Add to `community_state_sync.rs`'s inline test module:

```rust
/// ZEB-750: the runtime tracker converts to and from the persistence DTO
/// without disturbing the CBOR shape. `local` is NOT persisted — it is
/// supplied from ctx at load — so a round trip through the DTO must
/// preserve exactly the accepted watermarks and nothing else.
#[test]
fn replay_tracker_round_trips_through_the_persistence_dto_zeb750() {
    let local = (OwnerAddr([1u8; 16]), "local-dev".to_string());
    let peer = (OwnerAddr([2u8; 16]), "peer-dev".to_string());
    let clock = Hlc {
        wall_ms: 1_000,
        logical: 3,
        device_id: "peer-dev".to_string(),
    };

    let mut tracker = CommunityReplayTracker::new(local.clone());
    match tracker.admit(&peer, &clock) {
        Admission::Accept(ticket) => assert!(tracker.commit(ticket)),
        other => panic!("expected Accept, got {other:?}"),
    }

    let dto = CommunityRootHlcTracker {
        per_device: tracker.accepted().clone(),
    };
    let restored = CommunityReplayTracker::from_accepted(local.clone(), dto.per_device.clone());

    assert_eq!(restored.accepted(), tracker.accepted());
    assert_eq!(restored.local(), &local);
    assert_eq!(dto.per_device.len(), 1, "only the peer watermark is stored");
}
```

- [ ] **Step 2: Run it and confirm it fails**

Run: `cd src-tauri && cargo nextest run --features test-fixtures -E 'test(replay_tracker_round_trips_through_the_persistence_dto_zeb750)'`
Expected: FAIL to compile — `cannot find type CommunityReplayTracker in this scope`.

- [ ] **Step 3: Import the kernel and declare the runtime alias**

At `community_state_sync.rs:38`, extend the existing import:

```rust
use harmony_crdt_sync::{Admission, CommitTicket, ReplayTracker, RetryBackoff};
```

Immediately above `pub struct CommunityRootHlcTracker` (`:823`), add:

```rust
/// The runtime replay type: core's `ReplayTracker` keyed by
/// `(publisher_addr, device_id)` and clocked by the domain `Hlc`.
///
/// ZEB-750: this replaces `CommunityRootHlcTracker`'s own
/// `would_accept`/`record` pair. That pair enforced apply-before-advance
/// with a `debug_assert!` — compiled out of release builds — across the
/// 354-line window between the admission check and the advance in
/// `handle_incoming_publish`. Core enforces the same discipline with a
/// `CommitTicket` that only `admit` can mint and `commit` consumes, so
/// the ordering is unforgeable rather than merely documented.
///
/// `CommunityRootHlcTracker` remains as the persistence DTO: `local` is
/// not part of the on-disk shape, so `replay.cbor` is byte-unchanged.
pub type CommunityReplayTracker = ReplayTracker<(OwnerAddr, String), Hlc>;
```

- [ ] **Step 4: Demote the DTO — delete `would_accept`, `record`, and the `debug_assert`**

Replace the whole `impl CommunityRootHlcTracker { … }` block (`:833`-`:871`) with nothing, and update the struct's doc comment (`:810`-`:821`) so its first line reads:

```rust
/// Persistence DTO for the per-publisher-device latest-accepted HLC map,
/// namespaced by publisher `OwnerAddr`. ZEB-256: re-keyed from
/// `BTreeMap<String, Hlc>` so a member cannot squat another member's HLC
/// slot via shared `EpochKey`.
///
/// ZEB-750: this type is now **only** the on-disk shape. The admission
/// logic that used to live here moved to `CommunityReplayTracker` (core
/// `ReplayTracker`); convert at the persistence boundary with
/// `ReplayTracker::from_accepted` on load and `accepted().clone()` on save.
```

Keep the `Serialize`/`Deserialize` derives, the `CanonicalPayload` impls, and the paragraph explaining the CBOR 2-array key encoding — those are the byte-pin contract.

- [ ] **Step 5: Run the round-trip test and confirm it passes**

Run: `cd src-tauri && cargo nextest run --features test-fixtures -E 'test(replay_tracker_round_trips_through_the_persistence_dto_zeb750)'`
Expected: PASS. Other call sites will still be broken — that is Step 6.

- [ ] **Step 6: Migrate the field types and the six inline test constructions**

Change the three field declarations from `Arc<Mutex<CommunityRootHlcTracker>>` to `Arc<Mutex<CommunityReplayTracker>>` at `:1017`, `:1136`, `:1977`, and `tracker_arc`'s return type at `:1340`.

The six inline test constructions (`:7156`, `:7191`, `:7344`, `:7460`, `:7485`, `:7788`) currently read `Arc::new(Mutex::new(CommunityRootHlcTracker::default()))`. `ReplayTracker` has no `Default` (it needs `local`), so each becomes:

```rust
Arc::new(Mutex::new(CommunityReplayTracker::new((
    cfg.self_owner,
    cfg.device_id.clone(),
))))
```

At `:7485` the binding is `let b_tracker = …`; use whatever `self_owner`/`device_id` that test already has in scope rather than inventing new values.

- [ ] **Step 7: Convert at the persistence boundary**

At `:4188` (`persist_both`) and `:4213` (`persist_replay_only`), the snapshot line currently reads `let tracker_snap = ctx.tracker.lock().await.clone();`. Replace both with:

```rust
    // ZEB-750: snapshot into the persistence DTO under the lock. `local`
    // is ctx-derived and deliberately absent from the on-disk shape.
    let tracker_snap = CommunityRootHlcTracker {
        per_device: ctx.tracker.lock().await.accepted().clone(),
    };
```

`save_replay(&replay_path, &tracker_snap)` is unchanged — it already takes `&CommunityRootHlcTracker`.

At the load site (`:5153`), the `spawn_blocking` closure returns `(CommunityState, CommunityRootHlcTracker)`. Leave the closure alone — it loads the DTO — and wrap after the `??`:

```rust
    // ZEB-750: attach `local` from cfg. Both fields live on
    // `CommunityRegistryConfig`, so no two-step build is needed.
    let initial_tracker = CommunityReplayTracker::from_accepted(
        (self.cfg.self_owner, self.cfg.device_id.clone()),
        initial_tracker.per_device,
    );
```

At `tracker_snapshot` (`:5486`), keep the `Option<CommunityRootHlcTracker>` return type so test callers are unaffected, and convert:

```rust
        let snap = CommunityRootHlcTracker {
            per_device: tracker.lock().await.accepted().clone(),
        };
        Some(snap)
```

- [ ] **Step 8: Migrate `next_hlc`'s two tracker touches**

At `:3216`, the read currently reads `let prev = tracker.per_device.get(&key).cloned();`. `per_device` no longer exists on the runtime type:

```rust
    let local = (ctx.self_owner, ctx.device_id.clone());
    let prev = tracker.accepted_from(&local).cloned();
```

Delete the now-unused `let key = (ctx.self_owner, ctx.device_id.clone());` line above it.

At `:3255`, replace `tracker.record(ctx.self_owner, now.clone());` with:

```rust
    // ZEB-750: the local mint's write. `observe_local` is monotone and
    // returns false rather than asserting, so a tick that fails to
    // advance is a no-op instead of a debug-build panic.
    tracker.observe_local(now.clone());
```

Leave the three-branch arithmetic between them alone — that is Task 2.

- [ ] **Step 9: Thread the `CommitTicket` through `handle_incoming_publish`**

Both ends are inside one function (`:3464`-`:4112`), so the ticket is a local held across the body and every early return drops it — which is the correct, retry-safe outcome.

At `:3699`-`:3704`, replace the admission block with:

```rust
    let replay_ticket = {
        let tracker = ctx.tracker.lock().await;
        let source = (payload.publisher_addr, payload.at.device_id.clone());
        match tracker.admit(&source, &payload.at) {
            Admission::Accept(ticket) => ticket,
            // Our own publish, reflected back by the transport. Not a
            // replay and not an error — nothing to do.
            Admission::Echo => return IncomingOutcome::Duplicate,
            Admission::Duplicate => return IncomingOutcome::Duplicate,
        }
    };
```

At `:4053`-`:4056`, replace the advance block with:

```rust
    {
        let mut tracker = ctx.tracker.lock().await;
        tracker.commit(replay_ticket);
    }
```

Keep the existing step-14 comment above it verbatim — it already states the invariant (*"the SINGLE state-mutation point for tracker progress… preserving the 'tracker NOT advanced on any rejection' invariant"*) that the ticket now enforces mechanically. Append one line to it:

```rust
    //     ZEB-750: that invariant is now enforced by the type system —
    //     `commit` consumes the `CommitTicket` minted at step 5, and
    //     every early return between here and there drops it.
```

Between the two sites, add a `let _ = &replay_ticket;` nowhere — the value must simply live; if the compiler reports it as unused, that means a path returns without reaching `commit`, which is correct.

- [ ] **Step 10: Build scoped and fix fallout**

Run: `cd src-tauri && cargo check -p harmony-app --features test-fixtures 2>&1 | tail -40`
Expected: clean. Any `CommitTicket` move error identifies a path that reached `commit` twice — read it before "fixing" it; the compiler is auditing the pipeline.

- [ ] **Step 11: Run the scoped suite**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(community)'`
Expected: all pass, including both wire-format fixture suites with no regeneration.

- [ ] **Step 12: Negative control**

Temporarily change Step 9's `Admission::Accept(ticket) => ticket` arm to commit immediately (`{ let mut t = ...; t.commit(ticket); }` before the pipeline runs) and confirm a rejection-path test now shows an advanced watermark. Restore, re-run, confirm green. Record the observed failure in the PR body.

- [ ] **Step 13: Commit**

```bash
git add src-tauri/src/community_state_sync.rs
git commit -m "community_state_sync adopts core ReplayTracker (ZEB-750 1/3)"
```

---

### Task 2: Adopt core `HlcTick` for the local mint arithmetic

**Files:**
- Modify: `src-tauri/src/community_state_sync.rs` — `next_hlc` (`:3210`-`:3257`)
- Test: `src-tauri/src/community_state_sync.rs` (inline `#[cfg(test)]`)

**Interfaces:**
- Consumes: Task 1's `CommunityReplayTracker`, its `accepted_from(&local)` read and `observe_local(hlc)` write — both already in place.
- Produces: no new public surface. `next_hlc`'s signature is unchanged: `async fn next_hlc(ctx: &InternalCtx) -> Hlc`.

- [ ] **Step 1: Write the failing saturation test**

```rust
/// ZEB-750: at `logical == u32::MAX` the tick TIES its predecessor rather
/// than manufacturing a wall-clock advance. Core's rule, shared with
/// fleet_sync: "a stall, which is strictly preferable to admitting a
/// replay." The old branch existed only to dodge the `debug_assert!` that
/// Task 1 deleted.
#[test]
fn hlc_tick_ties_instead_of_manufacturing_a_wall_advance_at_saturation_zeb750() {
    let prev = HlcTick {
        wall_ms: 5_000,
        logical: u32::MAX,
    };
    // Same wall millisecond, counter already saturated.
    let next = HlcTick::next(Some(prev), 5_000);

    assert_eq!(next, prev, "saturated tick must tie, not advance");
    assert_eq!(
        next.wall_ms, 5_000,
        "the wall reading must NOT be manufactured forward"
    );

    // And `observe_local` reports the non-advance instead of asserting.
    let local = (OwnerAddr([9u8; 16]), "sat-dev".to_string());
    let mut tracker = CommunityReplayTracker::new(local.clone());
    let hlc = |t: HlcTick| Hlc {
        wall_ms: t.wall_ms,
        logical: t.logical,
        device_id: "sat-dev".to_string(),
    };
    assert!(tracker.observe_local(hlc(prev)));
    assert!(
        !tracker.observe_local(hlc(next)),
        "a tied tick must not advance the local watermark"
    );
}
```

- [ ] **Step 2: Run it and confirm it fails**

Run: `cd src-tauri && cargo nextest run --features test-fixtures -E 'test(hlc_tick_ties_instead_of_manufacturing_a_wall_advance_at_saturation_zeb750)'`
Expected: FAIL to compile — `cannot find type HlcTick in this scope`.

- [ ] **Step 3: Import `HlcTick`**

At `community_state_sync.rs:38`, extend the import:

```rust
use harmony_crdt_sync::{Admission, CommitTicket, HlcTick, ReplayTracker, RetryBackoff};
```

- [ ] **Step 4: Replace the three-branch match with the kernel**

In `next_hlc`, replace the entire `let now = match prev.as_ref() { … };` block (`:3231`-`:3254`, all four arms including the `u32::MAX` escape) with:

```rust
    // ZEB-750: the tick rule is core's, shared with fleet_sync — one
    // audited implementation of a subtle monotonicity contract. Under
    // logical saturation it ties `prev` (a stall) rather than
    // manufacturing a wall advance; receivers then reject the tied stamp
    // as a duplicate until the wall clock catches up.
    let prev_tick = prev.as_ref().map(|p| HlcTick {
        wall_ms: p.wall_ms,
        logical: p.logical,
    });
    let tick = HlcTick::next(prev_tick, wall_ms);
    let now = Hlc {
        wall_ms: tick.wall_ms,
        logical: tick.logical,
        device_id: ctx.device_id.clone(),
    };
```

The `wall_ms` binding read from `SystemTime::now()` at the top of `next_hlc` is unchanged, as are the `prev` read and the `observe_local` write Task 1 put in place.

- [ ] **Step 5: Run the test and confirm it passes**

Run: `cd src-tauri && cargo nextest run --features test-fixtures -E 'test(hlc_tick_ties_instead_of_manufacturing_a_wall_advance_at_saturation_zeb750)'`
Expected: PASS.

- [ ] **Step 6: Run the scoped suite**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(community)'`
Expected: all pass. Any test asserting the old manufactured-advance behaviour is a **deliberate** break — update it and note it in the PR body rather than restoring the branch.

- [ ] **Step 7: Negative control**

Restore the `Some(p) if p.logical == u32::MAX` arm temporarily; confirm the saturation test fails on `assert_eq!(next, prev)`. Remove it again, re-run, confirm green.

- [ ] **Step 8: Commit**

```bash
git add src-tauri/src/community_state_sync.rs
git commit -m "community next_hlc adopts core HlcTick (ZEB-750 2/3)"
```

---

### Task 3: Adopt core `DebounceLatch` for the publish window

**Files:**
- Modify: `src-tauri/src/community_state_sync.rs` — `internal_task` debounce/flush/shutdown arms and `settle_publish`
- Test: `src-tauri/tests/community_sync/community_sync_engine_unit.rs`

**Interfaces:**
- Consumes: `harmony_crdt_sync::{DebounceLatch, DirtySignal, PublishClaim, PublishOutcome}`; ZEB-761's `RetryBackoff` and its `wake_at` min-selection, unchanged.
- Produces: `settle_publish` changes signature from `(ctx, was_dirty: bool, pub_result, retry, now_ms)` to `(ctx, claim: PublishClaim, pub_result, retry, now_ms)`, matching `fleet_sync::settle_publish`.

- [ ] **Step 1: Extend the import**

At `community_state_sync.rs:38`:

```rust
use harmony_crdt_sync::{
    Admission, CommitTicket, DebounceLatch, DirtySignal, HlcTick, PublishClaim, PublishOutcome,
    ReplayTracker, RetryBackoff,
};
```

- [ ] **Step 2: Rewrite `settle_publish` to take a claim**

Replace the ZEB-761 helper body with the claim-based form, mirroring `fleet_sync.rs`:

```rust
/// ZEB-750: settle one publish attempt against both schedules.
///
/// The claim carries whether this path actually took the caller's dirty
/// signal; `settle` decides whether that signal must be restored. This is
/// the same helper `fleet_sync` runs — ZEB-761 introduced it in both
/// engines but had to give community a raw `was_dirty: bool` because it
/// was still pre-kernel. Both now speak one vocabulary.
fn settle_publish(
    ctx: &InternalCtx,
    claim: PublishClaim,
    pub_result: &Result<(), CommunitySyncError>,
    retry: &mut RetryBackoff,
    now_ms: u64,
) {
    let outcome = if pub_result.is_ok() {
        PublishOutcome::Published
    } else {
        PublishOutcome::Failed
    };
    match claim.settle(outcome) {
        DirtySignal::Restore => {
            ctx.has_pending_dirty.store(true, Ordering::Release);
            retry.on_failure(now_ms);
        }
        DirtySignal::Spent => {}
    }
    if pub_result.is_ok() {
        retry.clear(now_ms);
    }
}
```

Confirm the exact `PublishOutcome` variant names against `harmony-crdt-sync/src/debounce_latch.rs:95` before writing this — use whatever that enum declares.

- [ ] **Step 3: Replace the hand-rolled window with the latch**

In `internal_task`, alongside `let mut retry = RetryBackoff::default();` add:

```rust
    let mut latch = DebounceLatch::new(cfg.debounce_ms);
```

Delete the `next_wakeup` local and its manual recomputation. The ZEB-761 wake selection becomes (mirroring `fleet_sync.rs:779`):

```rust
    let wake_at = match (latch.deadline(), retry.pending_at()) {
        (Some(debounce_at), Some(retry_at)) => Some(debounce_at.min(retry_at)),
        (debounce_at, retry_at) => debounce_at.or(retry_at),
    };
```

The arm guard stays `if wake_at.is_some()`.

- [ ] **Step 4: Route all three arms through the latch**

| Arm | Replace with |
|---|---|
| notify wake | `latch.mark_dirty(now_ms());` |
| debounce fire | `let claim = latch.on_deadline(ctx.has_pending_dirty.swap(false, Ordering::AcqRel));` |
| `flush_now` | `let claim = latch.on_flush(ctx.has_pending_dirty.swap(false, Ordering::AcqRel));` |
| shutdown | `let claim = latch.on_shutdown(ctx.has_pending_dirty.load(Ordering::Relaxed));` |

Each arm then calls `settle_publish(&ctx, claim, &pub_result, &mut retry, retry_now_ms());`. The debounce arm keeps its existing `tracing::warn!(community_id = ?ctx.community_id, error = %e, "community publish_root_now failed");` at the call site — it has community-scoped context worth reporting.

Gate the shutdown publish on `claim.should_publish()`, which is false when there was no unpublished work.

- [ ] **Step 5: Build scoped**

Run: `cd src-tauri && cargo check -p harmony-app --features test-fixtures 2>&1 | tail -30`
Expected: clean.

- [ ] **Step 6: Write the debounce-collapse test**

Add to `src-tauri/tests/community_sync/community_sync_engine_unit.rs`:

```rust
/// ZEB-750: a burst of mutations inside one debounce window collapses to a
/// single publish. Pins the sliding-window semantics after the latch swap —
/// the hand-rolled version recomputed `next_wakeup` by hand, and a
/// regression there would silently publish once per mutation.
#[tokio::test(start_paused = true)]
async fn a_burst_of_mutations_collapses_into_one_publish_zeb750() {
    // Build the engine with the same harness the ZEB-761 tests use, with a
    // CAS stub that counts PutLocal ops.
    // Then: notify_dirty() x5 spaced 10ms apart inside a 500ms window,
    // advance past the window, and assert exactly one put was recorded.
}
```

Fill the body using the harness already present in that file (the `FlakyPutCas`/`CasOp` stub the ZEB-761 tests use) — reuse it rather than writing a second stub.

- [ ] **Step 7: Run the new test and the ZEB-761 pair**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(zeb750) or test(zeb761)'`
Expected: all pass. The two ZEB-761 tests must be green **untouched** — they pin the retry behaviour this task must not disturb.

- [ ] **Step 8: Negative control**

Change `on_deadline` to `on_flush` in the debounce arm (both return a claim, so it compiles) and confirm the collapse test still passes but a retry test breaks — proving the arms are distinguishable. Restore.

- [ ] **Step 9: Commit**

```bash
git add src-tauri/src/community_state_sync.rs src-tauri/tests/community_sync/community_sync_engine_unit.rs
git commit -m "community_state_sync adopts core DebounceLatch (ZEB-750 3/3)"
```

---

### Task 4: Full gate and PR

- [ ] **Step 1: fmt**

Run: `cd src-tauri && cargo fmt --all -- --check`
Expected: no output.

- [ ] **Step 2: clippy**

Run: `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
Expected: exit 0.

- [ ] **Step 3: full sweep**

Run: `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures`
Expected: `0 failed`. Confirm the two wire-format fixture suites ran and passed with **no regenerated fixture files** — check `git status` is clean of `tests/wire_format/` changes.

- [ ] **Step 4: Push and open the PR**

```bash
git push -u origin zeb-750-community-sync-convergence
gh pr create --repo zeblithic/harmony-client --title "both community engines adopt the harmony-crdt-sync decision kernels (ZEB-750)" --body-file <path>
```

The PR body must call out, prominently: (a) the ticket's stale premise and what ZEB-748 already settled; (b) the **deliberate saturation behaviour change**; (c) the negative-control results from each task. Post exactly **one** `@coderabbitai review` comment at open, and **zero** `@` characters in every subsequent comment on this PR.

## Self-Review

**Spec coverage:** Component 1 → Task 1; Component 2 → Task 2; Component 3 → Task 3; Error handling (`Admission::Echo`) → Task 1 Step 9; Testing → the per-task tests plus Task 4; Risks 1 and 3 → Task 1 Step 10 and Task 2 Step 4; Risk 2 → Task 4 Step 4. The spec's load-time constraint is resolved in Task 1 Step 7 (`CommunityRegistryConfig` carries both fields). No gaps.

**Placeholder scan:** Task 3 Step 6's test body is described rather than written, because it must reuse a harness whose exact constructor lives in the test file and would be guesswork here. That is the one deliberate exception; every other step carries literal code. Task 3 Step 2 instructs verifying the `PublishOutcome` variant names at the source rather than trusting this document.

**Type consistency:** `CommunityReplayTracker` is defined in Task 1 Step 3 and used in Tasks 1 and 2. `CommunityRootHlcTracker` keeps its name and `per_device` field throughout. `settle_publish`'s new signature appears once, in Task 3 Step 2, and its call sites in Step 4 match it.
