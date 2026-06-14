# ZEB-425 periodic watermark re-sync — Implementation Plan

**Goal:** Add a low-frequency periodic re-sync floor to the satisfied
(`Idle`) state of both latch drivers, so a long-lived device eventually
re-queries history/state even with no transport-epoch bump.

**Architecture:** New `Option<u64>` interval param + `resync_tick` helper;
a third `tokio::select!` arm in each driver's `Idle` branch performing the
same `latch.reset(…)` re-arm the epoch path does. `None` = disabled
(legacy contract). Spec: `docs/specs/2026-06-14-zeb-425-periodic-resync-design.md`.

**Tech Stack:** Rust, tokio (paused-time tests), existing
`channel_backfill.rs` pure-logic + async-driver split.

All commands run from `src-tauri/`. Per-task gate scope `-p harmony-app
--lib` (channel_backfill is lib-internal; no integration relink). Final
sweep `--all-targets` before PR.

---

### Task 1: Constant + `resync_tick` helper

**Files:** Modify `src-tauri/src/channel_backfill.rs`

- [ ] **Step 1:** Add near `EPOCH_REARM_COOLDOWN_MS`:

```rust
/// Anti-entropy floor (ZEB-425): re-arm a satisfied backfill/root-fetch
/// latch at most this long after it last (re-)synced, regardless of
/// transport-epoch bumps. 1 h — well above EPOCH_REARM_COOLDOWN_MS, so it
/// only acts when the edge-triggered re-arm never fires. Governs BOTH
/// run_backfill_driver and run_root_fetch_driver.
pub const PERIODIC_RESYNC_FLOOR_MS: u64 = 3_600_000;
```

- [ ] **Step 2:** Add the helper next to `epoch_bump`:

```rust
/// Fire after `interval_ms` when set; pend forever when `None` (the
/// periodic re-sync is disabled) so its select! arm never wins. Mirrors
/// `epoch_bump`'s pend-when-absent shape.
async fn resync_tick(interval_ms: Option<u64>) {
    match interval_ms {
        Some(ms) => tokio::time::sleep(std::time::Duration::from_millis(ms)).await,
        None => std::future::pending().await,
    }
}
```

- [ ] **Step 3:** `cargo fmt --all` ; `cargo clippy -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings` (allow `dead_code` will resolve once used in Task 2).
- [ ] **Step 4:** Commit: `feat(zeb-425): periodic re-sync floor const + resync_tick helper`.

---

### Task 2: Thread interval into `run_backfill_driver` + Idle periodic arm

**Files:** Modify `src-tauri/src/channel_backfill.rs`,
`src-tauri/src/community_channel_log_engine.rs`

- [ ] **Step 1 (failing test):** Add to `mod tests` a paused-time test:
  satisfied-after-empty-page latch, `epoch_rx = None` but
  `resync_interval_ms = Some(small)`; a `request_page` that returns an
  empty `Completed(0, limit)` and increments an `AtomicUsize`; assert the
  counter advances a second time only after virtual time passes the
  interval. (Drives the new signature + behavior.)

```rust
#[tokio::test(start_paused = true)]
async fn backfill_periodic_resync_refetches_when_no_epoch_bump() { /* … */ }
```

- [ ] **Step 2:** Change signature — add `resync_interval_ms: Option<u64>`
  immediately before `now_ms`:

```rust
pub async fn run_backfill_driver<Rq, RqFut, Wm, WmFut>(
    mut latch: BackfillLatch,
    request_page: Rq,
    current_watermark: Wm,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
    mut epoch_rx: Option<tokio::sync::watch::Receiver<u64>>,
    resync_interval_ms: Option<u64>,
    now_ms: impl Fn() -> u64,
) where /* unchanged */
```

- [ ] **Step 3:** In the `BackfillAction::Idle` branch:
  - change the early return to
    `if epoch_rx.is_none() && resync_interval_ms.is_none() { return; }`
  - add a select! arm:

```rust
_ = resync_tick(resync_interval_ms) => {
    // ZEB-425 anti-entropy floor: re-arm regardless of epoch bumps.
    // No cooldown — the interval is the rate limit (a re-armed latch
    // with still-no-holders backs off via WaitUntil, never straight
    // back to Idle, so this arm cannot storm).
    latch.reset(current_watermark().await);
}
```

- [ ] **Step 4:** Fix compile: pass `None` at every existing
  `run_backfill_driver(` unit-test call site (lines ~954/1010/1053/1098/
  1144/1190/1238) and at the production spawn in
  `community_channel_log_engine.rs:1787` pass `Some(crate::channel_backfill::PERIODIC_RESYNC_FLOOR_MS)`.
- [ ] **Step 5:** Run the new test + the existing backfill driver tests:
  `cargo nextest run -p harmony-app --lib --features test-fixtures -E 'test(backfill)'` → PASS.
- [ ] **Step 6:** `cargo fmt --all` ; clippy `--lib` ; Commit:
  `feat(zeb-425): periodic re-sync floor in run_backfill_driver`.

---

### Task 3: Thread interval into `run_root_fetch_driver` + Idle periodic arm

**Files:** Modify `src-tauri/src/channel_backfill.rs`,
`src-tauri/src/community_state_sync.rs`, `src-tauri/src/event_loop.rs`

- [ ] **Step 1 (failing test):** paused-time test on
  `run_root_fetch_driver`: satisfied latch (`on_reply`), `epoch_rx = None`,
  `resync_interval_ms = Some(small)`, `request_root` returns `Answered`
  and counts; assert a second request fires after the interval elapses.
- [ ] **Step 2:** Add `resync_interval_ms: Option<u64>` before `now_ms`
  in `run_root_fetch_driver`'s signature.
- [ ] **Step 3:** Same `Idle`-branch changes (early-return guard + select!
  arm calling `latch.reset()` — no watermark arg for root-fetch).
- [ ] **Step 4:** Fix compile: `None` at every `run_root_fetch_driver(`
  unit-test call site (~1420/1466/1491/1525/1554/1597/1664);
  `Some(PERIODIC_RESYNC_FLOOR_MS)` at the two production spawns
  (`community_state_sync.rs:4677`, `event_loop.rs:2831`).
- [ ] **Step 5:** `cargo nextest run -p harmony-app --lib --features test-fixtures -E 'test(root_fetch)'` (+ the new test) → PASS.
- [ ] **Step 6:** `cargo fmt --all` ; clippy `--lib` ; Commit:
  `feat(zeb-425): periodic re-sync floor in run_root_fetch_driver (community + mail root)`.

---

### Task 4: Behavior-guard tests (disabled / no-storm / shutdown)

**Files:** Modify `src-tauri/src/channel_backfill.rs` (`mod tests`)

- [ ] **Step 1:** `None` disables — satisfied latch, `epoch_rx = None`,
  `resync_interval_ms = None` → driver returns on `Idle`; advancing time
  yields no extra request. (Guards against accidental always-on.)
- [ ] **Step 2:** No storm — periodic re-sync fires, holder still absent
  (`NoReply`/empty stays unsatisfied per driver) → next action is
  `WaitUntil` backoff, the periodic arm does not re-fire until satisfied.
- [ ] **Step 3:** Shutdown prompt — resync enabled, latch parked in
  `Idle`; flip `shutdown_rx` → driver returns promptly (no interval wait).
- [ ] **Step 4:** `cargo nextest run -p harmony-app --lib --features test-fixtures -E 'test(resync)'` → PASS.
- [ ] **Step 5:** Commit: `test(zeb-425): periodic re-sync disabled/no-storm/shutdown guards`.

---

### Final sweep (pre-PR)

- [ ] `cargo fmt --all -- --check` → clean
- [ ] `cargo clippy -p harmony-app --all-targets --features test-fixtures --no-deps -- -D warnings` → clean
- [ ] `cargo nextest run -p harmony-app --lib --features test-fixtures` → green (lib scope covers channel_backfill + driver consumers; integration binaries unaffected by an internal driver signature change, but run the targeted integration files that spawn engines if quick: `-E 'test(community_channel)'`)
- [ ] MSRV: `cargo check --locked --all-targets --features test-fixtures`
- [ ] Push branch; open PR (plain-text ZEB-425/ZEB-418 refs); enter bot loop.
