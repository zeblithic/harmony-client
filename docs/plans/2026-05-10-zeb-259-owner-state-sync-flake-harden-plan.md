# ZEB-259: owner_state_sync flake-harden Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Eliminate CI timing flake in `owner_state_sync.rs` integration tests by converting 14 convergence-wait `tokio::time::sleep` callsites to bounded `wait_until` polling, leaving 8 time-bound-by-nature callsites untouched.

**Architecture:** Local `wait_until` async helper + per-callsite predicate extraction from existing assertions. Mirrors the codebase convention (every integration test file has its own helper).

**Tech Stack:** Rust 2021, `tokio::test`, `cargo nextest`.

**Spec:** `docs/specs/2026-05-10-zeb-259-owner-state-sync-flake-harden-design.md` (commit `21b7784`).

**Branch:** `zeb-259-owner-state-sync-flake-harden` (cut from `origin/main` `afea8ca`).

---

## Task 0: Pre-flight green-baseline confirm

**Files:** none (verification only)

- [ ] **Step 1: Confirm branch state**

```bash
git status
git log --oneline -3
```

Expected: clean tree, on `zeb-259-owner-state-sync-flake-harden`, HEAD is `21b7784` (spec commit) with `afea8ca` (origin/main) below it.

- [ ] **Step 2: Run all 5 CI gates**

From `src-tauri/`:
```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
cargo check --locked --all-targets --features test-fixtures
```

From repo root:
```bash
npx tsc --noEmit
npx vitest run
```

Expected: all 5 gates green. ANY red gate here means the baseline is dirty and our changes can't be cleanly attributed — STOP and report.

- [ ] **Step 3: NO COMMIT** — verification only.

---

## Task 1: Add `wait_until` helper + convert the two known-flaky callsites

**Files:**
- Modify: `src-tauri/src/owner_state_sync.rs` (add helper + edit lines 1612, 1645)

This task seeds the pattern + immediately fixes the two callsites called out in the original ZEB-259 report. Smallest meaningful unit.

- [ ] **Step 1: Locate the `mod integration_tests` opening**

```bash
grep -n "mod integration_tests" src-tauri/src/owner_state_sync.rs
```

Expected: a `#[cfg(test)] mod integration_tests {` block (the `#[tokio::test]` listing at lines 692-2040 lives inside it).

- [ ] **Step 2: Add `wait_until` helper at the top of `mod integration_tests`**

Insert after the existing `use` statements at the top of the test module:

```rust
async fn wait_until<F, Fut>(mut cond: F, timeout: std::time::Duration) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if cond().await {
            return true;
        }
        if tokio::time::Instant::now() > deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}
```

If `Duration` is already imported into the module's scope, the inline `std::time::Duration` references can be elided to bare `Duration`. Check existing imports first.

- [ ] **Step 3: Convert callsite at line 1612 (the flaky one)**

In `lagging_peer_ack_after_dedupe_still_merges` test, replace:

```rust
        dev.a_engine.notify_dirty();
        dev.b_engine.notify_dirty();
        tokio::time::sleep(Duration::from_millis(500)).await;

        // After sync: A's outbox should have been canonicalized to id=1.
        {
            let a = dev.a_state.lock().await;
            let entry = a.outbox.get(&OutboxEntryId([42; 16])).unwrap();
            assert_eq!(
                entry.space_id,
                SpaceId([1; 16]),
                "A's outbox must have canonicalized space_id"
            );
        }
```

with:

```rust
        dev.a_engine.notify_dirty();
        dev.b_engine.notify_dirty();
        let converged = wait_until(|| async {
            let a = dev.a_state.lock().await;
            a.outbox
                .get(&OutboxEntryId([42; 16]))
                .map_or(false, |e| e.space_id == SpaceId([1; 16]))
        }, Duration::from_secs(3)).await;
        assert!(converged, "A's outbox did not canonicalize space_id within 3s");

        // After sync: A's outbox should have been canonicalized to id=1.
        {
            let a = dev.a_state.lock().await;
            let entry = a.outbox.get(&OutboxEntryId([42; 16])).unwrap();
            assert_eq!(
                entry.space_id,
                SpaceId([1; 16]),
                "A's outbox must have canonicalized space_id"
            );
        }
```

- [ ] **Step 4: Convert callsite at line 1645 (the second flaky sleep, same test)**

Replace:

```rust
        dev.a_engine.notify_dirty();
        tokio::time::sleep(Duration::from_millis(300)).await;

        // After sync: A's entry still on canonicalized space_id=1,
        // and BOTH acks ({1, 2}) are present → Complete.
        let a = dev.a_state.lock().await;
        let entry = a.outbox.get(&OutboxEntryId([42; 16])).unwrap();
        assert_eq!(entry.space_id, SpaceId([1; 16]));
        assert_eq!(entry.delivered_to.len(), 2);
        assert_eq!(entry.delivery_status, DeliveryStatus::Complete);
        drop(a);
```

with:

```rust
        dev.a_engine.notify_dirty();
        let converged = wait_until(|| async {
            let a = dev.a_state.lock().await;
            a.outbox.get(&OutboxEntryId([42; 16])).map_or(false, |e| {
                e.space_id == SpaceId([1; 16])
                    && e.delivered_to.len() == 2
                    && e.delivery_status == DeliveryStatus::Complete
            })
        }, Duration::from_secs(3)).await;
        assert!(converged, "A's outbox did not reach Complete with 2 acks within 3s");

        // After sync: A's entry still on canonicalized space_id=1,
        // and BOTH acks ({1, 2}) are present → Complete.
        let a = dev.a_state.lock().await;
        let entry = a.outbox.get(&OutboxEntryId([42; 16])).unwrap();
        assert_eq!(entry.space_id, SpaceId([1; 16]));
        assert_eq!(entry.delivered_to.len(), 2);
        assert_eq!(entry.delivery_status, DeliveryStatus::Complete);
        drop(a);
```

- [ ] **Step 5: Run formatter + clippy + this specific test**

```bash
cd src-tauri
cargo fmt --all
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --features test-fixtures \
    -E 'test(lagging_peer_ack_after_dedupe_still_merges)'
```

Expected: 0 fmt diff, 0 clippy warnings, the test passes.

- [ ] **Step 6: 10× rerun loop on the formerly-flaky test**

```bash
cd src-tauri
for i in {1..10}; do
    cargo nextest run --locked --features test-fixtures \
        -E 'test(lagging_peer_ack_after_dedupe_still_merges)' \
        || { echo "FAIL on run $i"; exit 1; }
done
echo "10/10 passing"
```

Expected: 10/10 passing. ANY failure here means the polling predicate is wrong or the timeout is too tight — diagnose before proceeding.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/owner_state_sync.rs
git commit -m "$(cat <<'EOF'
fix(zeb-259): wait_until helper + convert known-flaky lagging_peer_ack sleeps

Replaces bare tokio::time::sleep at lines 1612 + 1645 with bounded
wait_until polling on the assertion target. Healthy runs exit at the
first ~20ms poll-tick where the condition holds; CI runners under
load get a 3s headroom budget before timeout.

10/10 local rerun loop on lagging_peer_ack_after_dedupe_still_merges
passing.

Spec: docs/specs/2026-05-10-zeb-259-owner-state-sync-flake-harden-design.md (21b7784)

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Convert remaining 12 Tier A callsites

**Files:**
- Modify: `src-tauri/src/owner_state_sync.rs` (lines 974, 1011, 1018, 1089, 1153, 1196, 1248, 1278, 1497, 1523, 1556, 1708)

Pattern is identical to Task 1: `notify_dirty()` (or `sub_tx.send()`) → `wait_until` polling on the immediately-following assertion's truth-condition → original assertion left in place as affirmative final check.

- [ ] **Step 1: For each Tier A callsite, read the surrounding context**

Use `Read` tool with offsets covering ~15 lines around each line number. The triage table in spec §4 names the convergence target for the well-understood ones; for lines 1089/1153/1196/1248/1278 the implementer must read the surrounding test to identify the assertion target.

- [ ] **Step 2: Apply the §6 conversion pattern to each callsite**

Per spec §7 timeout policy:
- Most callsites: `Duration::from_secs(2)`
- Heavier convergence (line 1556 `cross_device_dedupe_through_sync`): `Duration::from_secs(3)`

The polling predicate is the boolean form of the existing assertion. Examples:

| Line | Existing assertion | wait_until predicate |
|---|---|---|
| 974 | `assert_eq!(stored.wall_ms, 1000); assert_eq!(stored.logical, 0);` | `tracker.lock().await.get("peer-bob").map_or(false, \|s\| s.wall_ms == 1000 && s.logical == 0)` |
| 1497 | `assert!(b.spaces.contains_key(&SpaceId([1; 16])))` | `dev.b_state.lock().await.spaces.contains_key(&SpaceId([1; 16]))` |
| 1556 | `a.spaces.contains_key(&SpaceId([1; 16])) && !a.spaces.contains_key(&SpaceId([5; 16])) && b.spaces.contains_key(&SpaceId([1; 16])) && !b.spaces.contains_key(&SpaceId([5; 16]))` | same boolean expression in the closure |
| 1708 | `b.owner_device_cache.devices.get(&owner).expect(...)` | `dev.b_state.lock().await.owner_device_cache.devices.contains_key(&owner)` |

For lines 1089/1153/1196/1248/1278: locate via `Read`, derive the predicate from the immediately-following assertion(s).

For lines 1011 + 1018: the test `subscriber_rejects_strictly_older_hlc` has TWO sequential publishes with TWO sleeps; convert each independently — the first waits for the first publish to be recorded, the second waits for the (non-)effect of the second publish. Predicate for the second is `tracker.get("peer-bob").map_or(false, |s| s.wall_ms == 2000)` (still 2000, not regressed to 1000).

- [ ] **Step 3: Run formatter + clippy + the entire owner_state_sync test module**

```bash
cd src-tauri
cargo fmt --all
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --features test-fixtures \
    -E 'test(owner_state_sync::integration_tests)'
```

Expected: 0 fmt diff, 0 clippy warnings, all owner_state_sync integration tests pass.

- [ ] **Step 4: 10× rerun loop on owner_device_cache_converges_through_sync (the second-most-likely-flaky test)**

```bash
cd src-tauri
for i in {1..10}; do
    cargo nextest run --locked --features test-fixtures \
        -E 'test(owner_device_cache_converges_through_sync) | test(lagging_peer_ack_after_dedupe_still_merges)' \
        || { echo "FAIL on run $i"; exit 1; }
done
echo "10/10 passing"
```

Expected: 10/10 passing on both formerly-vulnerable tests.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/owner_state_sync.rs
git commit -m "$(cat <<'EOF'
fix(zeb-259): convert remaining 12 Tier A convergence-wait sleeps

Sweeps the rest of mod integration_tests in owner_state_sync.rs.
Tier B callsites (lines 740, 743, 802, 1816, 1820, 1822, 1824, 2027)
intentionally untouched — they're debounce-window / negative-assertion /
pacing waits where polling would mask real bugs (per spec §3 triage).

20× rerun loop on lagging_peer_ack + owner_device_cache_converges
green locally.

Spec: docs/specs/2026-05-10-zeb-259-owner-state-sync-flake-harden-design.md (21b7784)

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Final verification + push + PR creation

**Files:** none (verification + remote actions only)

- [ ] **Step 1: Run all 5 CI gates locally**

From `src-tauri/`:
```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
cargo check --locked --all-targets --features test-fixtures
```

From repo root:
```bash
npx tsc --noEmit
npx vitest run
```

Expected: all 5 green.

- [ ] **Step 2: Push branch**

```bash
git push -u origin zeb-259-owner-state-sync-flake-harden
```

- [ ] **Step 3: Create PR**

Use `gh pr create` with this body:

```markdown
## Summary
- Fixes ZEB-259: owner_state_sync.rs CI timing flake on `lagging_peer_ack_after_dedupe_still_merges`
- Adds local `wait_until` helper at top of `mod integration_tests` (mirrors codebase convention — 5 other test files have their own copy)
- Converts 14 Tier A (convergence-wait + assert) sleeps to bounded polling; leaves 8 Tier B (debounce-window / negative-assertion / pacing) sleeps untouched per the §3 triage rule
- Healthy runs exit early on first poll-tick (~20ms); CI runners under load get 2-3s headroom budget

## Triage rule (per spec §3)
- **Tier A (CONVERT):** `notify_dirty()` → `sleep(N)` → `assert!`/`assert_eq!` checking propagated state
- **Tier B (LEAVE):** debounce-window count tests, negative assertions ("state stays empty"), pacing loops — polling would mask real bugs

## Spec
[docs/specs/2026-05-10-zeb-259-owner-state-sync-flake-harden-design.md](https://github.com/zeblithic/harmony-client/blob/zeb-259-owner-state-sync-flake-harden/docs/specs/2026-05-10-zeb-259-owner-state-sync-flake-harden-design.md) (commit 21b7784)

## Plan
[docs/plans/2026-05-10-zeb-259-owner-state-sync-flake-harden-plan.md](https://github.com/zeblithic/harmony-client/blob/zeb-259-owner-state-sync-flake-harden/docs/plans/2026-05-10-zeb-259-owner-state-sync-flake-harden-plan.md)

## Test plan
- [x] `cargo fmt --check` clean
- [x] `cargo clippy --features test-fixtures -D warnings` clean
- [x] `cargo nextest run --features test-fixtures` (full workspace) green
- [x] `cargo check --features test-fixtures` (MSRV) clean
- [x] `npx tsc --noEmit` clean
- [x] `npx vitest run` green
- [x] 10× rerun loop on `lagging_peer_ack_after_dedupe_still_merges` + `owner_device_cache_converges_through_sync`: 10/10 passing each

Resolves ZEB-259
```

The PR body uses BARE `Resolves ZEB-259` (correct — auto-closes ZEB-259 on merge per Linear's GH integration). No parent epic refs needed (ZEB-215 is DONE; ZEB-259 has no parent).

- [ ] **Step 4: NO additional commit** — push + PR is the terminal action.

---

## Self-review checklist

- [x] Spec coverage: every Tier A callsite has a task that converts it (Tasks 1+2 split). Tier B callsites are noted in spec §4 and explicitly out of scope per §9.
- [x] No placeholders — every step has actual commands or actual code.
- [x] Type consistency — `wait_until` signature matches the canonical helper at `community_open_flow_integration.rs:53`. The polling predicates are derived from existing assertions in the same file.
- [x] Each task except Task 0 ends in a commit.
- [x] Final verification (Task 3) covers all 5 CI gates per the user-memory rule.

## Out of scope (per spec §9)

1. Tier B callsites — leave as-is per triage rule.
2. Sweep of other integration test files (those have their own helpers, no flake reports).
3. Extracting a shared `wait_until` to a feature-gated module.
4. Refactoring the `SyncEngine` debounce-and-publish loop.
