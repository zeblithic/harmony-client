# ZEB-259: Harden owner_state_sync.rs convergence-wait tests against CI timing flake

**Status:** approved 2026-05-10
**Parent:** ZEB-215 (Phase 1 of owner-state-sync) — original test author
**Linear:** [ZEB-259](https://linear.app/zeblith/issue/ZEB-259)

---

## 1. Problem

`owner_state_sync::integration_tests::lagging_peer_ack_after_dedupe_still_merges` (`src-tauri/src/owner_state_sync.rs:1580`) flaked once on PR #87 CI run [25471462308](https://github.com/zeblithic/harmony-client/actions/runs/25471462308) (HEAD `7a8a5ee`).

Failure: `assertion left == right failed: A's outbox must have canonicalized space_id` — observed non-canonical `SpaceId([5, 5, ...])` where the test expected `SpaceId([1, 1, ...])` (line 1618).

Investigation per ZEB-259:
- 10/10 single-test runs pass locally on PR #87 branch
- 3/3 single-test runs pass on bare `origin/main` locally
- Full local `cargo test --locked --workspace --all-targets` passes cleanly in 147s on PR #87 HEAD
- CI rounds 0-3 succeeded; round 4 first reproduced; round 5 succeeded
- Phase 3 work (PR #87) does NOT touch `owner_state_sync.rs`

The flake is **timing-sensitive**: tests use bare `tokio::time::sleep(Duration::from_millis(N))` to wait for cross-engine sync convergence, then assert on observable state. CI runners under load can miss the window.

## 2. Architecture

Replace bare `tokio::time::sleep` followed by an assertion with a **bounded polling loop** that exits as soon as the assertion target is observable. The test still has a deadline (so a real regression still fails fast), but healthy runs exit early and CI runners under load get headroom.

```rust
async fn wait_until<F, Fut>(mut cond: F, timeout: Duration) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if cond().await { return true; }
        if tokio::time::Instant::now() > deadline { return false; }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
```

Mirrors the helper at `src-tauri/tests/community_open_flow_integration.rs:53` (and 4 other test files in this codebase).

## 3. Triage rule (Tier A vs Tier B)

Not every `tokio::time::sleep` in tests is a convergence wait. Triage each callsite:

**Tier A — convergence-wait + assert** (CONVERT to `wait_until`):
- Pattern: `notify_dirty()` (or `sub_tx.send(...)`) followed by `sleep(N)` followed by `lock().await + assert!`/`assert_eq!` checking that some state mutation has propagated.
- The assertion's truth-condition becomes the polling predicate.

**Tier B — time-bound by nature** (LEAVE bare-sleep, optionally bump value, add comment):
- "Wait for debounce window to fire, then drain + count publishes" — cannot poll on "did N+1 events NOT fire".
- "Wait for absence of state change" (negative assertion: assert state stays empty after window).
- "Pacing between rapid-fire calls" (e.g., `1ms` between 50 `notify_dirty()` calls — semantic, not convergence).

Converting Tier B to polling would HIDE real bugs (a debounce regression that double-publishes; an unwanted-state-change regression).

## 4. Callsite inventory

22 bare-sleep callsites in tests (line 261 is production engine `select!` await — out of scope).

### Tier A — 14 callsites, CONVERT

| Line | Sleep (ms) | Test | Convergence target |
|---|---|---|---|
| 974 | 50 | `subscriber_accepts_fresh_hlc` (or similar) | `tracker.get("peer-bob").wall_ms == 1000` |
| 1011 | 50 | `subscriber_rejects_strictly_older_hlc` | first publish recorded |
| 1018 | 50 | (same test) | tracker not regressed after replay |
| 1089 | 100 | (next test) | (locate by reading) |
| 1153 | 100 | (next test) | (locate by reading) |
| 1196 | 100 | (next test) | (locate by reading) |
| 1248 | 100 | (next test) | (locate by reading) |
| 1278 | 100 | (next test) | (locate by reading) |
| 1497 | 300 | `one_way_convergence` | `b.spaces.contains_key(SpaceId([1; 16]))` |
| 1523 | 500 | `bidirectional_convergence` | both `a.spaces` and `b.spaces` have both DMs |
| 1556 | 500 | `cross_device_dedupe_through_sync` | both converged on winner SpaceId(1), lost SpaceId(5) |
| **1612** | **500** | **`lagging_peer_ack_after_dedupe_still_merges`** (FLAKY) | `a.outbox[42].space_id == SpaceId([1; 16])` |
| **1645** | **300** | (same test) | `a.outbox[42].delivered_to.len() == 2` AND `delivery_status == Complete` |
| 1708 | 400 | `owner_device_cache_converges_through_sync` | `b.owner_device_cache.devices.contains_key(&owner)` |

### Tier B — 8 callsites, LEAVE (with comment if not already obvious)

| Line | Sleep (ms) | Test | Why bare-sleep is correct |
|---|---|---|---|
| 740 | 1 | `rapid_notify_dirty_collapses_to_one_publish` | semantic pacing between rapid `notify_dirty()` calls — not convergence |
| 743 | 200 | (same test) | wait for debounce window to fire, then count publishes — can't poll |
| 802 | 400 | `flush_now_cancels_pending_wakeup` | wait for debounce window then drain + count — can't poll |
| 1816 | 800 | `convergence_under_random_concurrent_writes` | "let convergence settle" before forced flushes — sequenced flush dance, not single-shot |
| 1820 | 200 | (same test) | inter-flush propagation delay |
| 1822 | 200 | (same test) | inter-flush propagation delay |
| 1824 | 200 | (same test) | inter-flush propagation delay |
| 2027 | 150 | `subscriber_drops_unknown_root_cid` | wait then assert state stays empty (negative assertion — can't poll for absence) |

## 5. Helper placement

Add `wait_until` as a private helper at the **top of `mod integration_tests`** in `owner_state_sync.rs`. Mirrors the codebase convention — every test file with timing-sensitive integration tests has its own copy:
- `src-tauri/tests/community_open_flow_integration.rs:53`
- `src-tauri/tests/community_channel_messages_integration.rs:137`
- `src-tauri/tests/community_sync_integration.rs:83`, `:2067`

DO NOT extract a shared helper. Rust integration tests can't share helper code without a `test-fixtures` feature gate; introducing one purely for `wait_until` is scope creep that fights the established pattern.

## 6. Conversion pattern

**Before:**
```rust
dev.a_engine.notify_dirty();
tokio::time::sleep(Duration::from_millis(500)).await;
let a = dev.a_state.lock().await;
let entry = a.outbox.get(&OutboxEntryId([42; 16])).unwrap();
assert_eq!(
    entry.space_id, SpaceId([1; 16]),
    "A's outbox must have canonicalized space_id"
);
```

**After:**
```rust
dev.a_engine.notify_dirty();
let converged = wait_until(|| async {
    let a = dev.a_state.lock().await;
    a.outbox
        .get(&OutboxEntryId([42; 16]))
        .map_or(false, |e| e.space_id == SpaceId([1; 16]))
}, Duration::from_secs(3)).await;
assert!(converged, "A's outbox did not canonicalize space_id within 3s");

// Affirmative final check (cheap; preserves the original assertion's specificity)
let a = dev.a_state.lock().await;
let entry = a.outbox.get(&OutboxEntryId([42; 16])).unwrap();
assert_eq!(entry.space_id, SpaceId([1; 16]));
```

## 7. Timeout policy

Standardize on `Duration::from_secs(2)` for most Tier A callsites (4-40× bare-sleep value, plenty of CI headroom). Use `Duration::from_secs(3)` for the heavier-convergence cases:
- 1556 (cross_device_dedupe_through_sync — both engines must converge on winner)
- 1612 (lagging_peer_ack — known-flaky, bigger budget warranted)
- 1645 (lagging_peer_ack second sleep — same)

These bounds matter only for failure cases; healthy runs exit at the first poll-tick where the condition holds (~20ms cadence).

## 8. Acceptance criteria

1. `wait_until` helper added at top of `mod integration_tests` in `owner_state_sync.rs`.
2. All 14 Tier A callsites converted per the §6 pattern.
3. All 8 Tier B callsites untouched (or only gain a brief `// Tier B: ...` comment per ZEB-259 §3 if their reason isn't already obvious in surrounding context).
4. All 5 CI gates green: `cargo fmt --check`, `cargo clippy --features test-fixtures -D warnings`, `cargo nextest run --features test-fixtures`, `cargo check (msrv)`, `npx tsc --noEmit`, `npx vitest run`.
5. The two flaky tests (`lagging_peer_ack_after_dedupe_still_merges`, plus its sibling `owner_device_cache_converges_through_sync` if also exhibiting the same pattern) run 10× green locally on the branch via `cargo nextest run --features test-fixtures -E 'test(lagging_peer_ack_after_dedupe_still_merges) | test(owner_device_cache_converges_through_sync)'` (rerun loop).
6. Test runtime should improve marginally on healthy runs (polling exits early vs. waiting full sleep duration).

## 9. Out of scope

1. Tier B callsites — leave as-is per §3 triage rule.
2. Sweep of other integration test files (`tests/community_*_integration.rs`, `tests/owner_*_integration.rs`) — those already have their own `wait_until` helpers and aren't reported as flaky. ZEB-259 is scoped to `owner_state_sync.rs` per the original report.
3. Extracting a shared `wait_until` to `harmony-app::test_helpers` or a feature-gated module — fights established codebase pattern (every test file has its own); scope creep.
4. Refactoring the `SyncEngine` debounce-and-publish loop — orthogonal to test hardening.

## 10. Known limitations

1. **`wait_until` does not detect SHRINKING state windows.** If a state mutation arrives, the test passes the `wait_until` boundary, then a subsequent state mutation undoes the property before the affirmative final check at §6, the test fails on the second check (which is correct: the property didn't hold). This is a feature, not a bug — preserves the affirmative final assertion's role as the "did the test actually observe the right state?" gate.
2. **Polling cadence is fixed at 20ms.** Could be parameterized but isn't worth the complexity — 20ms matches every other helper in the codebase and gives ~100 polls per 2-second timeout (ample resolution).

## 11. Verification

Local pre-push:
- `cd src-tauri && cargo fmt --all -- --check` — 0
- `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` — 0
- `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures` — all pass
- `cd src-tauri && cargo check --locked --all-targets --features test-fixtures` (MSRV) — 0
- `npx tsc --noEmit` (from repo root) — 0
- `npx vitest run` (from repo root) — all pass
- 10× rerun loop on the two flaky tests:
  ```bash
  for i in {1..10}; do
    cd src-tauri && cargo nextest run --locked --features test-fixtures \
      -E 'test(lagging_peer_ack_after_dedupe_still_merges) | test(owner_device_cache_converges_through_sync)' \
      || { echo "FAIL on run $i"; exit 1; }
  done
  ```

CI: 5 gates green on PR.
