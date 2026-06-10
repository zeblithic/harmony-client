# ZEB-434 Community-State Reconnect Catch-up Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A member who missed community-state publishes (channel set, membership, power) heals from their own side via a retrying state-root pull, the publisher re-seeds peers at boot, and every catch-up latch in the app re-arms when a new peer link appears.

**Architecture:** Four additions mirroring P3a's proven shapes: (1) a per-community zenoh queryable that serves a fresh state-root packet produced by the engine's single-writer task; (2) a `RootFetchLatch` + driver (sibling of `BackfillLatch`) that pulls at engine spawn with 30s→600s backoff; (3) a mint-style boot flush per community engine; (4) a transport-epoch `watch<u64>` bumped on new-zid detection in the event loop's existing 5s peer refresh, re-arming community/channel/mail latches with a 60s cooldown. No new wire formats — query replies are byte-identical to live root publishes and ingest through the existing verified inbound path.

**Tech Stack:** Rust (tokio, zenoh 1.9), existing `community_state_sync` / `channel_backfill` / `event_loop` modules. Spec: `docs/specs/2026-06-10-zeb-434-community-state-reconnect-catchup-design.md` (commit `e953f64c`).

**House rules for every task:** work directly on branch `zeb-434-community-state-catchup` (NO worktrees). Commit BEFORE running gates. Per-task gates (lib-scoped):

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all
cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked -p harmony-app --lib --features test-fixtures
```

10-minute wall-clock kill switch per gate command (Bash timeout param; macOS has no `timeout` cmd). `set -o pipefail` when piping. Statuses: DONE / DONE_WITH_CONCERNS / NEEDS_CONTEXT / BLOCKED. Implementers do NOT push.

---

## File map

| File | Role in this plan |
|---|---|
| `src-tauri/src/channel_backfill.rs` | Task 1: add `RootFetchLatch`, `RootFetch`, `RootFetchAction`, `run_root_fetch_driver`, `ROOT_REARM_COOLDOWN_MS`. Task 7: extend `run_backfill_driver` with optional epoch re-arm. |
| `src-tauri/src/community_state_sync.rs` | Task 2: extract `encode_root_packet` from `publish_root_now`. Task 3: `RootServeRequest` + query-serve `select!` arm in `internal_task`. Task 5: registry plumbing + fetch-driver spawn + `COMMUNITY_BOOT_FLUSH_DELAY_MS`. |
| `src-tauri/src/event_loop.rs` | Task 4: extend `CommunityAdapterRequest` (:127) + `spawn_community_state_zenoh_adapter` (:6567) with queryable + root-fetch tasks. Task 6: new-zid diff in the 5s peer refresh (:2940). Task 9: mail-root retry driver (:2599). |
| `src-tauri/src/lib.rs` | Task 5/6/8: create channel pairs + watch in `start_node`, extend boot spawn loop (:4678) with boot flush, thread epoch rx into registries. |
| `src-tauri/src/community_channel_log_engine.rs` | Task 7: pass epoch rx to the backfill driver spawn (:1742). |

Type names used throughout (defined in Task 1 unless noted): `RootFetch` (`Answered`/`NoReply`/`EngineGone`), `RootFetchAction` (`Request`/`WaitUntil(u64)`/`Idle`), `RootFetchLatch`, `run_root_fetch_driver`, `ROOT_REARM_COOLDOWN_MS`; Task 3: `RootServeRequest = tokio::sync::oneshot::Sender<Result<Vec<u8>, String>>`; Task 4: `CommunityRootFetchRequest { report: tokio::sync::oneshot::Sender<usize> }` (report payload = reply count).

---

### Task 1: `RootFetchLatch` + `run_root_fetch_driver` (pure decision core)

**Files:**
- Modify: `src-tauri/src/channel_backfill.rs` (append a new section after `run_backfill_driver`, before `#[cfg(test)]`)
- Test: same file, tests mod

The root fetch is page-less: a responder always has a root, so **≥1 reply = satisfied; 0 replies = no responder = backoff**. The driver outlives satisfaction when given a transport-epoch watch: it parks on `Idle` and re-arms on epoch bumps (spec D7), with re-arm queries deferred (not dropped) to a 60s cooldown since the last request.

- [ ] **Step 1: Write failing latch tests** (append inside the existing `mod tests`):

```rust
    // ── ZEB-434: RootFetchLatch ──────────────────────────────────────

    #[test]
    fn root_latch_satisfies_on_reply() {
        let mut latch = RootFetchLatch::new();
        assert!(!latch.is_satisfied());
        assert_eq!(latch.next_action(0), RootFetchAction::Request);
        latch.on_reply(0);
        assert!(latch.is_satisfied());
        assert_eq!(latch.next_action(0), RootFetchAction::Idle);
    }

    #[test]
    fn root_latch_backs_off_exponentially_with_cap() {
        let mut latch = RootFetchLatch::new();
        assert_eq!(latch.next_action(0), RootFetchAction::Request);
        latch.on_no_reply(0);
        assert_eq!(latch.next_action(0), RootFetchAction::WaitUntil(30_000));
        assert_eq!(latch.next_action(30_000), RootFetchAction::Request);
        latch.on_no_reply(30_000);
        assert_eq!(
            latch.next_action(30_000),
            RootFetchAction::WaitUntil(90_000)
        );
        // Cap check: drive delay past 600s — mirrors the BackfillLatch
        // schedule (30→60→120→240→480→600 capped).
        let mut t = 90_000u64;
        for _ in 0..4 {
            assert_eq!(latch.next_action(t), RootFetchAction::Request);
            latch.on_no_reply(t);
            let RootFetchAction::WaitUntil(next) = latch.next_action(t) else {
                panic!("expected WaitUntil");
            };
            t = next;
        }
        // After 480s the next doubling clamps to the 600s cap.
        assert_eq!(latch.next_action(t), RootFetchAction::Request);
        latch.on_no_reply(t);
        assert_eq!(
            latch.next_action(t),
            RootFetchAction::WaitUntil(t + ROOT_FETCH_RETRY_CAP_MS_TEST_ALIAS)
        );
    }

    #[test]
    fn root_latch_in_flight_guard() {
        let mut latch = RootFetchLatch::new();
        assert_eq!(latch.next_action(0), RootFetchAction::Request);
        assert!(matches!(latch.next_action(0), RootFetchAction::WaitUntil(_)));
    }

    #[test]
    fn root_latch_reset_unsatisfies() {
        let mut latch = RootFetchLatch::new();
        assert_eq!(latch.next_action(0), RootFetchAction::Request);
        latch.on_reply(0);
        assert!(latch.is_satisfied());
        latch.reset();
        assert!(!latch.is_satisfied());
        assert_eq!(latch.next_action(0), RootFetchAction::Request);
    }
```

Note: `ROOT_FETCH_RETRY_CAP_MS_TEST_ALIAS` is just `BACKFILL_RETRY_CAP_MS` — use the constant directly in the real test body (the latch shares P3a's `BACKFILL_RETRY_BASE_MS`/`BACKFILL_RETRY_CAP_MS`).

- [ ] **Step 2: Run to verify failure**

Run: `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(root_latch)'`
Expected: FAIL — `RootFetchLatch` not found.

- [ ] **Step 3: Implement the latch** (new section in `channel_backfill.rs`):

```rust
// ── ZEB-434: community state-root fetch latch ────────────────────────────────

/// Cooldown between transport-epoch-triggered re-arm queries (60 s).
/// Spec D7: a flapping link must not storm; deferred, not dropped.
pub const ROOT_REARM_COOLDOWN_MS: u64 = 60_000;

/// What the root-fetch driver should do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootFetchAction {
    /// Send one state-root query (full-state exchange — no `since`).
    Request,
    /// Re-poll `next_action` at or after this wall-clock ms.
    WaitUntil(u64),
    /// Satisfied — park until `reset()` (transport-recovery re-arm).
    Idle,
}

/// Outcome of one root-fetch attempt, as seen by the driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootFetch {
    /// ≥1 responder replied (replies ingest via the engine's normal
    /// inbound path — receipt is transport-level, same bar as P3a).
    Answered,
    /// Zero responders / query aborted — back off and retry.
    NoReply,
    /// Engine/adapter permanently gone — stop the driver.
    EngineGone,
}

/// Retry latch for one community's state-root pull (ZEB-434 D3).
///
/// Page-less sibling of [`BackfillLatch`]: a responder always has a
/// root, so ≥1 reply satisfies and zero replies means no responder.
/// Shares the spec D24/D3 backoff schedule (30 s base doubling to a
/// 600 s cap, retrying forever — the driver enforces shutdown).
#[derive(Debug, Clone)]
pub struct RootFetchLatch {
    satisfied: bool,
    in_flight: bool,
    next_retry_at: u64,
    retry_delay_ms: u64,
    retry_base_ms: u64,
    retry_cap_ms: u64,
}

impl RootFetchLatch {
    /// New, unsatisfied latch with the production backoff schedule.
    pub fn new() -> Self {
        Self::new_with_backoff(BACKFILL_RETRY_BASE_MS, BACKFILL_RETRY_CAP_MS)
    }

    /// Test-injectable backoff (mirrors `BackfillLatch::new_with_backoff`).
    pub fn new_with_backoff(base_ms: u64, cap_ms: u64) -> Self {
        Self {
            satisfied: false,
            in_flight: false,
            next_retry_at: 0,
            retry_delay_ms: 0,
            retry_base_ms: base_ms,
            retry_cap_ms: cap_ms,
        }
    }

    pub fn is_satisfied(&self) -> bool {
        self.satisfied
    }

    /// Decide the next driver action at wall-clock `now_ms`.
    pub fn next_action(&mut self, now_ms: u64) -> RootFetchAction {
        if self.satisfied {
            return RootFetchAction::Idle;
        }
        if self.in_flight {
            return RootFetchAction::WaitUntil(self.next_retry_at.max(now_ms));
        }
        if now_ms < self.next_retry_at {
            return RootFetchAction::WaitUntil(self.next_retry_at);
        }
        self.in_flight = true;
        RootFetchAction::Request
    }

    /// ≥1 responder replied: satisfied, backoff cleared.
    pub fn on_reply(&mut self, now_ms: u64) {
        self.in_flight = false;
        self.satisfied = true;
        self.retry_delay_ms = 0;
        self.next_retry_at = now_ms;
    }

    /// Zero responders: arm the escalating backoff (same schedule and
    /// clamp semantics as `BackfillLatch::arm_backoff`).
    pub fn on_no_reply(&mut self, now_ms: u64) {
        self.in_flight = false;
        self.retry_delay_ms = if self.retry_delay_ms == 0 {
            self.retry_base_ms.min(self.retry_cap_ms)
        } else {
            (self.retry_delay_ms * 2).min(self.retry_cap_ms)
        };
        self.next_retry_at = now_ms + self.retry_delay_ms;
    }

    /// Transport-recovery re-arm: unsatisfy, clear in-flight + backoff.
    pub fn reset(&mut self) {
        *self = Self::new_with_backoff(self.retry_base_ms, self.retry_cap_ms);
    }
}

impl Default for RootFetchLatch {
    fn default() -> Self {
        Self::new()
    }
}
```

- [ ] **Step 4: Run latch tests to verify pass**

Run: `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(root_latch)'`
Expected: 4 PASS.

- [ ] **Step 5: Write failing driver tests** (paused-time, mirroring the `run_backfill_driver` driver tests in the same file):

```rust
    #[tokio::test(start_paused = true)]
    async fn root_driver_retries_until_answered_then_parks_when_epoch_some() {
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        let request_root = move || {
            let counter = Arc::clone(&counter);
            async move {
                let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                if n < 2 { RootFetch::NoReply } else { RootFetch::Answered }
            }
        };
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let (epoch_tx, epoch_rx) = tokio::sync::watch::channel(0u64);
        let start = tokio::time::Instant::now();
        let driver = tokio::spawn(run_root_fetch_driver(
            RootFetchLatch::new(),
            request_root,
            shutdown_rx,
            Some(epoch_rx),
            move || start.elapsed().as_millis() as u64,
        ));
        // Request #1 fires at spawn (no sleep involved) — yield until
        // it lands. NOTE: a bare `while < 2 { yield_now }` would
        // deadlock under start_paused: the busy test task keeps the
        // runtime non-idle so paused time never auto-advances across
        // the driver's 30s backoff sleep. Advance the clock EXPLICITLY.
        while requests.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
        for _ in 0..16 { tokio::task::yield_now().await; }
        tokio::time::advance(Duration::from_millis(30_001)).await;
        while requests.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
        for _ in 0..16 { tokio::task::yield_now().await; }
        // Satisfied + epoch_rx Some → the driver must PARK, not return.
        assert!(!driver.is_finished(), "driver must park on Idle when epoch watch present");
        // Epoch bump → re-arm. Cooldown (60s since last request) defers
        // the re-query; advancing past it lets request #3 fire.
        epoch_tx.send(1).expect("epoch bump");
        tokio::time::advance(Duration::from_millis(ROOT_REARM_COOLDOWN_MS + 1)).await;
        while requests.load(Ordering::SeqCst) < 3 {
            tokio::task::yield_now().await;
        }
        assert_eq!(requests.load(Ordering::SeqCst), 3);
        driver.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn root_driver_returns_on_idle_when_epoch_none() {
        let request_root = || async { RootFetch::Answered };
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let start = tokio::time::Instant::now();
        run_root_fetch_driver(
            RootFetchLatch::new(),
            request_root,
            shutdown_rx,
            None,
            move || start.elapsed().as_millis() as u64,
        )
        .await; // must complete (legacy return-on-Idle behavior)
    }

    #[tokio::test(start_paused = true)]
    async fn root_driver_stops_on_shutdown_while_parked() {
        let request_root = || async { RootFetch::Answered };
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let (_epoch_tx, epoch_rx) = tokio::sync::watch::channel(0u64);
        let start = tokio::time::Instant::now();
        let driver = tokio::spawn(run_root_fetch_driver(
            RootFetchLatch::new(),
            request_root,
            shutdown_rx,
            Some(epoch_rx),
            move || start.elapsed().as_millis() as u64,
        ));
        for _ in 0..16 { tokio::task::yield_now().await; }
        shutdown_tx.send(true).expect("shutdown");
        driver.await.expect("driver task ends");
    }

    #[tokio::test(start_paused = true)]
    async fn root_driver_exits_on_engine_gone() {
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        let request_root = move || {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                RootFetch::EngineGone
            }
        };
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let (_epoch_tx, epoch_rx) = tokio::sync::watch::channel(0u64);
        let start = tokio::time::Instant::now();
        run_root_fetch_driver(
            RootFetchLatch::new(),
            request_root,
            shutdown_rx,
            Some(epoch_rx),
            move || start.elapsed().as_millis() as u64,
        )
        .await;
        assert_eq!(requests.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn root_driver_epoch_bump_mid_backoff_requeries_after_cooldown() {
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        // Always NoReply: the latch sits in escalating backoff.
        let request_root = move || {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                RootFetch::NoReply
            }
        };
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let (epoch_tx, epoch_rx) = tokio::sync::watch::channel(0u64);
        let start = tokio::time::Instant::now();
        let driver = tokio::spawn(run_root_fetch_driver(
            RootFetchLatch::new(),
            request_root,
            shutdown_rx,
            Some(epoch_rx),
            move || start.elapsed().as_millis() as u64,
        ));
        while requests.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
        for _ in 0..16 { tokio::task::yield_now().await; }
        // t≈0: first no-reply armed a 30s backoff. Bump the epoch:
        // D7 says reset backoff and re-query once the 60s cooldown
        // (since request #1) elapses — sooner than waiting out a
        // longer escalated backoff would have been in general, and
        // crucially with the backoff RESET to base afterwards.
        epoch_tx.send(1).expect("epoch bump");
        tokio::time::advance(Duration::from_millis(ROOT_REARM_COOLDOWN_MS + 1)).await;
        while requests.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
        assert_eq!(requests.load(Ordering::SeqCst), 2);
        driver.abort();
    }
```

- [ ] **Step 6: Run to verify failure** — `run_root_fetch_driver` not found.

- [ ] **Step 7: Implement the driver** (below the latch):

```rust
/// Drive one community's [`RootFetchLatch`]: query at spawn, back off
/// while unanswered, and — when `epoch_rx` is `Some` — park on
/// satisfaction and re-arm on transport-epoch bumps (ZEB-434 D6/D7).
///
/// - `request_root()` issues one state-root query and resolves to a
///   [`RootFetch`]. Reply payloads travel through the engine's normal
///   inbound path; the driver only sees counts.
/// - `epoch_rx = None` preserves return-on-Idle (used by tests and any
///   caller without a transport watch).
/// - Re-arm queries are deferred to [`ROOT_REARM_COOLDOWN_MS`] since
///   the last request — deferred, not dropped (spec D7).
/// - `now_ms` injects the wall clock (paused-time testability — same
///   precedent as `run_backfill_driver`).
pub async fn run_root_fetch_driver<Rq, RqFut>(
    mut latch: RootFetchLatch,
    request_root: Rq,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
    mut epoch_rx: Option<tokio::sync::watch::Receiver<u64>>,
    now_ms: impl Fn() -> u64,
) where
    Rq: Fn() -> RqFut,
    RqFut: std::future::Future<Output = RootFetch>,
{
    let mut last_request_at: Option<u64> = None;
    // Wait for an epoch bump (or pend forever when no watch is wired).
    // Returns false when the epoch sender dropped or shutdown fired.
    async fn epoch_bump(
        epoch_rx: &mut Option<tokio::sync::watch::Receiver<u64>>,
    ) -> bool {
        match epoch_rx.as_mut() {
            Some(rx) => rx.changed().await.is_ok(),
            None => std::future::pending().await,
        }
    }
    // Defer a re-arm to the cooldown boundary (deferred, not dropped).
    // Returns false if shutdown fires during the wait.
    async fn cooldown_wait(
        last_request_at: Option<u64>,
        now: u64,
        shutdown_rx: &mut tokio::sync::watch::Receiver<bool>,
    ) -> bool {
        let Some(last) = last_request_at else { return true };
        let target = last.saturating_add(ROOT_REARM_COOLDOWN_MS);
        if now >= target {
            return true;
        }
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(target - now)) => true,
            changed = shutdown_rx.changed() => {
                !(changed.is_err() || *shutdown_rx.borrow())
            }
        }
    }

    loop {
        if *shutdown_rx.borrow() {
            return;
        }
        match latch.next_action(now_ms()) {
            RootFetchAction::Idle => {
                if epoch_rx.is_none() {
                    return;
                }
                tokio::select! {
                    bumped = epoch_bump(&mut epoch_rx) => {
                        if !bumped { return; }
                        if !cooldown_wait(last_request_at, now_ms(), &mut shutdown_rx).await {
                            return;
                        }
                        latch.reset();
                    }
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() { return; }
                    }
                }
            }
            RootFetchAction::WaitUntil(target) => {
                let wait_ms = target
                    .saturating_sub(now_ms())
                    .max(BACKFILL_DRIVER_MIN_WAIT_MS);
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_millis(wait_ms)) => {}
                    bumped = epoch_bump(&mut epoch_rx) => {
                        // Mid-backoff bump: a new peer is exactly the
                        // signal that retrying now is worthwhile (D7).
                        if !bumped { return; }
                        if !cooldown_wait(last_request_at, now_ms(), &mut shutdown_rx).await {
                            return;
                        }
                        latch.reset();
                    }
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() { return; }
                    }
                }
            }
            RootFetchAction::Request => {
                last_request_at = Some(now_ms());
                match request_root().await {
                    RootFetch::Answered => latch.on_reply(now_ms()),
                    RootFetch::NoReply => latch.on_no_reply(now_ms()),
                    RootFetch::EngineGone => return,
                }
            }
        }
    }
}
```

Note the `WaitUntil` epoch arm: a bump while a request is **in-flight** would `reset()` and drop the in-flight guard — that is acceptable (worst case one duplicate query, idempotent merges absorb it) but implementers should keep the cooldown check, which bounds it.

- [ ] **Step 8: Run all Task 1 tests** — `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(root_)'` — expect all PASS, plus full lib gates.

- [ ] **Step 9: Commit** — `git add -A && git commit -m "feat(zeb-434): RootFetchLatch + root-fetch driver with epoch re-arm"`

---

### Task 2: Extract `encode_root_packet` from `publish_root_now`

**Files:**
- Modify: `src-tauri/src/community_state_sync.rs` (`publish_root_now` spans ~:2480-2631)

Pure refactor, zero behavior change. `publish_root_now(ctx)` currently does: epoch-stable snapshot loop (:2514-2560) → canonical-CBOR encode (:2563) → deterministic-nonce blob encrypt (:2570) → CID derive (:2575) → `put_serveable` (:2593) → signed payload with `next_hlc` (:2597-2610) → wire envelope (:2616) → `encrypt_root_publish` (:2622) → `publisher_tx.send` (:2625).

- [ ] **Step 1: Refactor.** Split everything **except the final send** into:

```rust
/// Build one complete state-root wire packet: epoch-stable snapshot,
/// blob encrypt + CAS pin (put_serveable), signed payload with a
/// strictly-newer HLC, wire-envelope encrypt. Shared by the debounced
/// publish path and the ZEB-434 query-serve arm — both produce
/// byte-class-identical packets, which is what keeps "no new wire
/// format" true.
async fn encode_root_packet(ctx: &InternalCtx) -> Result<Vec<u8>, CommunitySyncError> {
    // ... moved body of publish_root_now steps (snapshot loop .. step 8) ...
    Ok(wire)
}

async fn publish_root_now(ctx: &InternalCtx) -> Result<(), CommunitySyncError> {
    let wire = encode_root_packet(ctx).await?;
    ctx.publisher_tx
        .send(wire)
        .await
        .map_err(|_| CommunitySyncError::TransportClosed)
}
```

Keep every comment attached to its moved step. Do not change `next_hlc`, persist call sites, or the epoch-retry loop.

- [ ] **Step 2: Run lib gates** — all existing `community_state_sync` tests must stay green (refactor proof).

- [ ] **Step 3: Commit** — `git commit -m "refactor(zeb-434): extract encode_root_packet from publish_root_now"`

---

### Task 3: Engine query-serve arm (`internal_task`)

**Files:**
- Modify: `src-tauri/src/community_state_sync.rs` — `CommunitySyncEngineConfig`, `CommunitySyncEngine::new` (:986-1099), `InternalCtx` (~:1710-1790, mirror the field list at :1044-1070), `internal_task` `select!` (:2243+)
- Test: same file, tests mod

- [ ] **Step 1: Write the failing test.** Follow the existing two-engine test pattern in this file (tests around :4854 connect a CAS event-loop and drive `flush_now`; copy the setup of the nearest two-engine root-publish test):

```rust
    #[tokio::test]
    async fn query_serve_arm_replies_packet_that_peer_engine_ingests() {
        // Engine A: admin engine with one ChannelCreate inserted, root
        // serve channel wired. Engine B: same community, empty state.
        // Drive: send a RootServeRequest oneshot into A, take the
        // packet bytes, feed them into B's subscriber_tx. Assert B's
        // materialized channel set contains the channel — proving the
        // reply is byte-identical in format to a live root publish
        // (decrypt + replay-guard + verify + materialize all pass).
        //
        // Setup mirrors <nearest existing two-engine test in this
        // file>: same content_store stub, same membership_key, same
        // delta_tx wiring; add `root_serve_tx` kept by the test and
        // `root_serve_rx: Some(rx)` in A's config; B keeps None.
        // (Implementer: copy the existing helper setup verbatim, then:)
        let (serve_tx, serve_rx) = tokio::sync::mpsc::channel::<RootServeRequest>(4);
        // ... A's cfg gets root_serve_rx: Some(serve_rx) ...
        // insert ChannelCreate into A via insert_local_event (existing
        // test helpers build signed events); drain A's publisher_rx.
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        serve_tx.send(reply_tx).await.expect("serve channel");
        let packet = reply_rx
            .await
            .expect("engine replied")
            .expect("encode ok");
        // Feed to B's inbound and assert the channel materializes.
        b_subscriber_tx.send(packet).await.expect("b inbound");
        // Poll B's state until the channel appears (bounded loop,
        // tokio::time::sleep(50ms) x 40 max — inbound is async).
        // assert!(b_state.lock().await.materialize(...).channels contains the id)
    }
```

The implementer fleshes this out against the real helpers — the **assertions and channel shapes above are normative**; the setup boilerplate comes from the neighboring test.

- [ ] **Step 2: Run to verify failure** — `RootServeRequest` / cfg field missing.

- [ ] **Step 3: Implement.**

(a) Type + config:

```rust
/// ZEB-434 D1/D2: a state-root query-serve request. The queryable task
/// in event_loop sends one per inbound zenoh query; the engine's
/// single-writer task replies with a fresh wire packet (or an error
/// string for logging).
pub type RootServeRequest = tokio::sync::oneshot::Sender<Result<Vec<u8>, String>>;
```

`CommunitySyncEngineConfig` gains `pub root_serve_rx: Option<mpsc::Receiver<RootServeRequest>>`. Grep `CommunitySyncEngineConfig {` (2 construction sites in this file) and add `root_serve_rx: None` where not otherwise wired; the registry site (Task 5) threads the real receiver.

(b) `InternalCtx` gains `root_serve_rx: Option<mpsc::Receiver<RootServeRequest>>`; `CommunitySyncEngine::new` moves `cfg.root_serve_rx` into it.

(c) New `select!` arm in `internal_task` (alongside the `flush_now_rx` arm at :2295). `Option<Receiver>` can't be polled directly in `select!` — hold it as a local: at task start, `let mut root_serve_rx = ctx.root_serve_rx.take();` then:

```rust
            Some(reply_tx) = async {
                match root_serve_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                // ZEB-434 D2: serve a FRESH packet through this single-
                // writer task so publish, flush, and query-serve can
                // never disagree about HLC state. encode advances
                // next_hlc via the tracker — persist the replay tracker
                // on success, mirroring the publish arms' "never
                // advance the tracker unpersisted" rule. The CRDT
                // itself did not change → persist_replay_only.
                let result = encode_root_packet(&ctx).await;
                match &result {
                    Ok(_) => {
                        if let Err(e) = persist_replay_only(&ctx).await {
                            tracing::warn!(
                                community_id = ?ctx.community_id,
                                error = %e,
                                "community persist after query-serve encode failed"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            community_id = ?ctx.community_id,
                            error = %e,
                            "community query-serve encode failed"
                        );
                    }
                }
                // Receiver dropped (querier gone) is fine — fire and forget.
                let _ = reply_tx.send(result.map_err(|e| e.to_string()));
            }
```

(Adjust to the file's actual persist helper names — `persist_replay_only` exists per :2365.)

- [ ] **Step 4: Run the new test + lib gates** — PASS.

- [ ] **Step 5: Commit** — `git commit -m "feat(zeb-434): community engine query-serve arm replies fresh root packets"`

---

### Task 4: Zenoh adapter — queryable + root-fetch query driver

**Files:**
- Modify: `src-tauri/src/event_loop.rs` — `CommunityAdapterRequest` (:127-139), `spawn_community_state_zenoh_adapter` (:6567-6700), the adapter spawn site (:4938 region) and the on-demand drain (`community_adapter_request_rx`, :698)

No unit test in this task (zenoh-session glue; the format identity is already pinned by Task 3's test and P3a's adapter precedents) — gate is compile + clippy + existing tests. Integration behavior is exercised live in the final sweep + cross-WAN testing.

- [ ] **Step 1: Extend the request struct:**

```rust
/// ZEB-434: one root-fetch query request from the per-community fetch
/// driver. The adapter executes the GET and reports the reply count.
pub struct CommunityRootFetchRequest {
    /// Reply-count report (fire-and-forget; drop = query aborted).
    pub report: tokio::sync::oneshot::Sender<usize>,
}

pub struct CommunityAdapterRequest {
    pub id_hex: String,
    pub publisher_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    pub subscriber_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    /// ZEB-434 D1: queryable → engine serve requests (engine holds rx).
    pub root_serve_tx: tokio::sync::mpsc::Sender<crate::community_state_sync::RootServeRequest>,
    /// ZEB-434 D3/D4: fetch driver → adapter query requests.
    pub fetch_request_rx: tokio::sync::mpsc::Receiver<CommunityRootFetchRequest>,
}
```

- [ ] **Step 2: Extend `spawn_community_state_zenoh_adapter`** with the two new params (`root_serve_tx`, `fetch_request_rx`) and two new inner tasks, joined alongside `pub_handle`/`sub_handle`:

(a) **Queryable task** (template: channel-log queryable at :7075-7143, minus selector parsing — the key has no parameters):

```rust
        let session_qbl = Arc::clone(&session);
        let key_qbl = key_expr.clone();
        let topic_qbl = topic.clone();
        let closing_qbl = Arc::clone(&closing);
        let root_serve_tx_qbl = root_serve_tx.clone();
        let qbl_handle = tokio::spawn(async move {
            let qbl = match session_qbl.declare_queryable(&key_qbl).await {
                Ok(q) => q,
                Err(e) => {
                    if !closing_qbl.load(Ordering::SeqCst) {
                        tracing::error!(topic = %topic_qbl, error = %e,
                            "failed to declare community state-root queryable");
                    }
                    return;
                }
            };
            loop {
                tokio::select! {
                    biased;
                    res = qbl.recv_async() => {
                        let Ok(query) = res else { break; };
                        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                        if root_serve_tx_qbl.send(reply_tx).await.is_err() {
                            // Engine gone — stop serving.
                            break;
                        }
                        match reply_rx.await {
                            Ok(Ok(packet)) => {
                                if let Err(e) = query.reply(query.key_expr(), packet).await {
                                    tracing::warn!(topic = %topic_qbl, error = %e,
                                        "community state-root queryable reply failed");
                                }
                            }
                            // Encode error already logged engine-side;
                            // dropped oneshot = engine shutdown race.
                            Ok(Err(_)) | Err(_) => {}
                        }
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                        if closing_qbl.load(Ordering::SeqCst) { break; }
                    }
                }
            }
        });
```

(b) **Root-fetch query driver task** (template: channel-log query-request driver at :7153-7290, simplified — no paging, no progress events; 10 s GET timeout matching the mail-root query):

```rust
        let session_rf = Arc::clone(&session);
        let key_rf = topic.clone();
        let subscriber_tx_rf = subscriber_tx.clone();
        let closing_rf = Arc::clone(&closing);
        let rf_handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    maybe = fetch_request_rx.recv() => {
                        let Some(req) = maybe else { break; };
                        let receiver = match session_rf
                            .get(&key_rf)
                            .consolidation(zenoh::query::ConsolidationMode::None)
                            .timeout(std::time::Duration::from_secs(10))
                            .await
                        {
                            Ok(r) => r,
                            Err(e) => {
                                if !closing_rf.load(Ordering::SeqCst) {
                                    tracing::warn!(key = %key_rf, error = %e,
                                        "community state-root fetch query failed");
                                }
                                // req.report drops → driver maps to NoReply.
                                continue;
                            }
                        };
                        let mut replies: usize = 0;
                        let drained_clean: bool = loop {
                            tokio::select! {
                                biased;
                                res = receiver.recv_async() => {
                                    let Ok(reply) = res else { break true; };
                                    if let Ok(sample) = reply.into_result() {
                                        let bytes: Vec<u8> =
                                            sample.payload().to_bytes().to_vec();
                                        if subscriber_tx_rf.send(bytes).await.is_err() {
                                            return; // engine teardown
                                        }
                                        replies = replies.saturating_add(1);
                                    }
                                }
                                _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {
                                    if closing_rf.load(Ordering::SeqCst) { break false; }
                                }
                            }
                        };
                        if drained_clean {
                            let _ = req.report.send(replies);
                        }
                        // !drained_clean: req.report drops without a
                        // value → fetch driver sees NoReply (shutdown
                        // watch ends it promptly anyway).
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                        if closing_rf.load(Ordering::SeqCst) { break; }
                    }
                }
            }
        });
```

(c) Join the new handles wherever `pub_handle`/`sub_handle` are awaited at the end of the adapter task.

- [ ] **Step 3: Fix the call sites** that construct `CommunityAdapterRequest` / call `spawn_community_state_zenoh_adapter` (boot Vec at lib.rs:4704 + the :4938 spawn region + the on-demand path that feeds `community_adapter_request_rx` — grep `CommunityAdapterRequest {` across `src/`). **Tasks 4 and 5 are one compile/gate unit:** the channel pairs are created at the lib.rs call sites and their engine/driver halves land in Task 5's plumbing. Implement both tasks in sequence and run the gates once at the end of Task 5; commit Task 4's adapter-side diff separately first only if it compiles standalone (it may not — a single combined commit for 4+5 is acceptable).

- [ ] **Step 4: Run lib gates + `cargo check --locked --all-targets --features test-fixtures`** (adapter code paths compile under integration targets too).

- [ ] **Step 5: Commit** — `git commit -m "feat(zeb-434): state-root queryable + fetch query driver in community adapter"`

---

### Task 5: Registry plumbing — serve channel, fetch driver spawn, shutdown

**Files:**
- Modify: `src-tauri/src/community_state_sync.rs` — `spawn_engine_inner_now` (:4169), `spawn_engine_with_guard` (same param ripple), registry struct + `shutdown_all`, `CommunitySyncRegistryConfig`
- Modify: `src-tauri/src/lib.rs` — boot spawn loop (:4678-4710), join/create path (`spawn_engine_with_guard` caller at :18142)

- [ ] **Step 1: Extend `spawn_engine_inner_now`** (and `spawn_engine_with_guard`) signature:

```rust
    pub async fn spawn_engine_inner_now(
        &self,
        community_id: SpaceId,
        membership_key: EpochKey,
        admin_addr: OwnerAddr,
        is_invite_only: bool,
        publisher_tx: mpsc::Sender<Vec<u8>>,
        subscriber_rx: mpsc::Receiver<Vec<u8>>,
        // ZEB-434: query-serve receiver (engine side of the queryable
        // bridge) + fetch-request sender (driver → adapter) + transport
        // epoch watch. All None/absent in legacy tests.
        root_serve_rx: Option<mpsc::Receiver<RootServeRequest>>,
        fetch_request_tx: Option<mpsc::Sender<crate::event_loop::CommunityRootFetchRequest>>,
        transport_epoch_rx: Option<tokio::sync::watch::Receiver<u64>>,
    ) -> Result<bool, CommunitySyncError> {
```

Pass `root_serve_rx` into `CommunitySyncEngineConfig`. Update every existing caller (grep `spawn_engine_inner_now(` and `spawn_engine_with_guard(`) — tests pass `None, None, None`.

- [ ] **Step 2: Spawn the fetch driver** after the engine is inserted (only when `fetch_request_tx` is `Some`):

```rust
        if let Some(fetch_tx) = fetch_request_tx {
            let (driver_shutdown_tx, driver_shutdown_rx) =
                tokio::sync::watch::channel(false);
            self.root_fetch_shutdowns
                .lock()
                .await
                .insert(community_id, driver_shutdown_tx);
            let request_root = move || {
                let fetch_tx = fetch_tx.clone();
                async move {
                    let (report_tx, report_rx) = tokio::sync::oneshot::channel();
                    if fetch_tx
                        .send(crate::event_loop::CommunityRootFetchRequest { report: report_tx })
                        .await
                        .is_err()
                    {
                        // Adapter bridge closed for good.
                        return crate::channel_backfill::RootFetch::EngineGone;
                    }
                    match report_rx.await {
                        Ok(n) if n > 0 => crate::channel_backfill::RootFetch::Answered,
                        // Clean drain with zero replies = no responder
                        // (a community-root responder always has a root
                        // to serve), and an aborted query (sender
                        // dropped) is transient — both back off.
                        Ok(_) | Err(_) => crate::channel_backfill::RootFetch::NoReply,
                    }
                }
            };
            tokio::spawn(crate::channel_backfill::run_root_fetch_driver(
                crate::channel_backfill::RootFetchLatch::new(),
                request_root,
                driver_shutdown_rx,
                transport_epoch_rx,
                || {
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0)
                },
            ));
        }
```

Registry struct gains `root_fetch_shutdowns: tokio::sync::Mutex<std::collections::HashMap<SpaceId, tokio::sync::watch::Sender<bool>>>` (init empty in `new`). In `shutdown_all` AND any per-engine removal path (grep `engines.remove` / the shutdown path `shutdown_rx` feeds), flip + drop the matching sender: `if let Some(tx) = self.root_fetch_shutdowns.lock().await.remove(&community_id) { let _ = tx.send(true); }`.

- [ ] **Step 3: Wire lib.rs call sites.** Boot loop (:4678): for each community create `let (root_serve_tx, root_serve_rx) = mpsc::channel::<RootServeRequest>(8);` and `let (fetch_req_tx, fetch_req_rx) = mpsc::channel::<CommunityRootFetchRequest>(4);`, pass `Some(root_serve_rx), Some(fetch_req_tx), transport_epoch_rx.clone()` (watch created in Task 6 — for this commit create the watch channel in `start_node` near the registry construction and hand the sender to event_loop in Task 6) into `spawn_engine_inner_now`, and the `root_serve_tx`/`fetch_request_rx` halves into the extended `CommunityAdapterRequest`. Same pattern at the join/create path (:18142 region) — its adapter request goes through the on-demand `community_adapter_request_tx`.

- [ ] **Step 4: Run lib gates + `cargo check --locked --all-targets --features test-fixtures`.**

- [ ] **Step 5: Commit** — `git commit -m "feat(zeb-434): registry spawns root-fetch driver, threads serve/fetch channels"`

---

### Task 6: Transport-epoch watch — new-zid detection

**Files:**
- Modify: `src-tauri/src/event_loop.rs` — peer refresh (:2940-2951), `run` params/struct (where `community_adapters` lands, :688 region)
- Modify: `src-tauri/src/lib.rs` — create the watch in `start_node`, pass sender into event_loop and receivers into registries
- Test: `event_loop.rs` tests mod (pure helper)

- [ ] **Step 1: Failing test for the pure diff helper:**

```rust
    #[test]
    fn transport_epoch_bumps_only_on_new_zids() {
        let mut seen: std::collections::HashSet<String> =
            ["a".into(), "b".into()].into();
        // Same set → no bump.
        assert!(!merge_peers_detect_new(&mut seen, vec!["a".into(), "b".into()]));
        // Peer disappears → no bump (loss is not recovery).
        assert!(!merge_peers_detect_new(&mut seen, vec!["a".into()]));
        // New zid (rebooted peer = fresh session zid) → bump.
        assert!(merge_peers_detect_new(&mut seen, vec!["a".into(), "c".into()]));
        // Note: `seen` accumulates (c stays known even if it flaps out
        // and back) — a flapping link does not re-bump; genuine new
        // sessions do.
        assert!(!merge_peers_detect_new(&mut seen, vec!["c".into()]));
        assert!(!merge_peers_detect_new(&mut seen, vec!["a".into(), "c".into()]));
    }
```

**Semantics decision (normative):** the helper keeps an accumulating `seen` set rather than overwriting — a link that flaps down/up within one session does NOT re-bump (the same zid was already seen; nothing was published in the gap that we could have missed without also seeing a new publish-side session). A genuinely new session (reboot, new peer) always bumps. This makes the 60s driver cooldown a second-layer defense rather than the only one. Keep the existing overwrite variable `direct_peer_zids` for its current consumers (hop-distance comparisons) — the accumulating set is separate state for epoch detection only.

- [ ] **Step 2: Run to verify failure.**

- [ ] **Step 3: Implement helper + wire-in:**

```rust
/// ZEB-434 D6: fold a fresh peers_zid() snapshot into the accumulating
/// seen-set; true iff at least one never-before-seen zid appeared.
pub(crate) fn merge_peers_detect_new(
    seen: &mut std::collections::HashSet<String>,
    refreshed: Vec<String>,
) -> bool {
    let mut any_new = false;
    for zid in refreshed {
        if seen.insert(zid) {
            any_new = true;
        }
    }
    any_new
}
```

In the 5s refresh arm (:2944-2951), after `direct_peer_zids` is refreshed:

```rust
                if peer_refresh_counter.is_multiple_of(20) {
                    let refreshed: Vec<String> = session
                        .info()
                        .peers_zid()
                        .await
                        .map(|z| z.to_string())
                        .collect();
                    // ZEB-434 D6: any never-seen zid bumps the transport
                    // epoch — community/channel/mail latches re-arm.
                    if merge_peers_detect_new(&mut transport_seen_zids, refreshed.clone())
                    {
                        transport_epoch_tx.send_modify(|e| *e = e.wrapping_add(1));
                    }
                    direct_peer_zids = refreshed.into_iter().collect();
                }
```

(`transport_seen_zids: HashSet<String>` initialized empty next to `direct_peer_zids` at :2631; seed it from the initial `peers_zid()` read there WITHOUT bumping — boot-time peers are not "recovered" peers, and the spawn-time latch query already covers them.)

`transport_epoch_tx: tokio::sync::watch::Sender<u64>` arrives via the `run` params (add alongside `community_adapters`). lib.rs `start_node` creates `let (transport_epoch_tx, transport_epoch_rx) = tokio::sync::watch::channel(0u64);` before the registry construction (:4105 region) and threads: sender → event_loop run (which also derives receivers for its internal consumers via `transport_epoch_tx.subscribe()` — Task 9); receiver clones → Task 5's `spawn_engine_inner_now` calls and Task 7's channel-log registry config.

- [ ] **Step 4: Run lib gates.**

- [ ] **Step 5: Commit** — `git commit -m "feat(zeb-434): transport-epoch watch bumps on new peer zids"`

---

### Task 7: Channel-log backfill re-arm (P3a §9 closure)

**Files:**
- Modify: `src-tauri/src/channel_backfill.rs` — `run_backfill_driver` (:285-350)
- Modify: `src-tauri/src/community_channel_log_engine.rs` — driver spawn (:1742), registry/engine config (epoch rx field)
- Modify: `src-tauri/src/lib.rs` — thread the epoch receiver into the channel-log registry config
- Test: `channel_backfill.rs` tests mod

- [ ] **Step 1: Failing test:**

```rust
    #[tokio::test(start_paused = true)]
    async fn backfill_driver_rearms_on_epoch_bump_with_fresh_watermark() {
        let requests = Arc::new(AtomicUsize::new(0));
        let sinces: Arc<StdMutex<Vec<Option<Hlc>>>> = Arc::new(StdMutex::new(Vec::new()));
        let counter = Arc::clone(&requests);
        let since_log = Arc::clone(&sinces);
        // Every request answers with a short page (immediately satisfied).
        let request_page = move |since: Option<Hlc>| {
            let counter = Arc::clone(&counter);
            let since_log = Arc::clone(&since_log);
            async move {
                since_log.lock().unwrap().push(since);
                counter.fetch_add(1, Ordering::SeqCst);
                PageFetch::Completed(0, 256)
            }
        };
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let (epoch_tx, epoch_rx) = tokio::sync::watch::channel(0u64);
        let start = tokio::time::Instant::now();
        let driver = tokio::spawn(run_backfill_driver(
            BackfillLatch::new(Some(hlc(100))),
            request_page,
            // Watermark moved to 200 by the time the re-arm fires.
            || async { Some(hlc(200)) },
            shutdown_rx,
            Some(epoch_rx),
            move || start.elapsed().as_millis() as u64,
        ));
        while requests.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
        for _ in 0..16 { tokio::task::yield_now().await; }
        assert!(!driver.is_finished(), "must park on Idle with epoch watch");
        epoch_tx.send(1).expect("bump");
        tokio::time::advance(Duration::from_millis(ROOT_REARM_COOLDOWN_MS + 1)).await;
        while requests.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
        // Re-arm must reset() with the CURRENT log watermark, not the
        // spawn-time one.
        assert_eq!(
            sinces.lock().unwrap().clone(),
            vec![Some(hlc(100)), Some(hlc(200))]
        );
        driver.abort();
    }
```

- [ ] **Step 2: Run to verify failure** — `run_backfill_driver` has no epoch param.

- [ ] **Step 3: Implement.** Add `mut epoch_rx: Option<tokio::sync::watch::Receiver<u64>>` to `run_backfill_driver` between `shutdown_rx` and `now_ms`. Mirror Task 1's driver exactly: `last_request_at` tracking set in the `Request` arm; `Idle` parks (return when `epoch_rx.is_none()` — preserving every existing test, which passes `None`); `WaitUntil` gains the epoch arm; on bump → cooldown_wait → `latch.reset(current_watermark().await)`. Reuse the same `epoch_bump`/`cooldown_wait` helpers from Task 1 (hoist them to module scope — they are shared by both drivers; do that hoisting as part of this step, adjusting Task 1's driver to call the shared fns).

Update the 5 existing driver tests' call sites with `None,` for the new param. Production spawn at `community_channel_log_engine.rs:1742` passes the registry config's receiver: add `pub transport_epoch_rx: Option<tokio::sync::watch::Receiver<u64>>` to the channel-log registry config struct (grep the struct that carries `adapter_request_tx` / `engine_config` near :1698), default `None` in tests, threaded from lib.rs where the registry config is built; the spawn site clones it per driver.

- [ ] **Step 4: Run all `channel_backfill` + `community_channel_log` lib tests + lib gates.**

- [ ] **Step 5: Commit** — `git commit -m "feat(zeb-434): channel backfill drivers re-arm on transport epoch (P3a follow-up)"`

---

### Task 8: Community boot flush

**Files:**
- Modify: `src-tauri/src/community_state_sync.rs` (constant), `src-tauri/src/lib.rs` boot spawn loop (:4678-4710)

- [ ] **Step 1: Add the constant** next to `DEFAULT_COMMUNITY_DEBOUNCE_MS` (grep the debounce constant near :441):

```rust
/// ZEB-434 D5: delay before the boot-time unconditional root flush —
/// same value as `mint_sync::DEFAULT_BOOT_FLUSH_DELAY_MS` (500 ms,
/// long enough for the zenoh adapter to wire up, short enough to beat
/// any human interaction).
pub const COMMUNITY_BOOT_FLUSH_DELAY_MS: u64 = 500;
```

- [ ] **Step 2: Add the boot-hook** in the lib.rs loop body, after `community_adapter_requests.push(...)` (:4704-4710), mirroring the mint block (:3541-3552):

```rust
                        // ZEB-434 D5: boot-hook flush — emit the local
                        // snapshot shortly after startup so peers that
                        // are already online receive our current state
                        // (the dirty bit does not survive restarts and
                        // clears on publish-into-the-void, so this is
                        // deliberately unconditional; receivers dedup
                        // via AlreadyKnown → tracker-only persist).
                        // Mirrors the mint engine boot flush above.
                        {
                            let boot_registry = std::sync::Arc::clone(&registry);
                            tokio::spawn(async move {
                                tokio::time::sleep(std::time::Duration::from_millis(
                                    crate::community_state_sync::COMMUNITY_BOOT_FLUSH_DELAY_MS,
                                ))
                                .await;
                                // Ignore error — engine may have shut
                                // down before the boot delay elapsed.
                                let _ = boot_registry.flush_now(&space_id).await;
                            });
                        }
```

(`registry` is the `CommunitySyncRegistry` Arc in scope in that loop; `space_id` is `Copy` per the snapshot tuple — confirm and clone if not.)

- [ ] **Step 3: Run lib gates** (the callee `flush_now` is covered by existing engine tests; this is glue verified by compile + the final sweep).

- [ ] **Step 4: Commit** — `git commit -m "feat(zeb-434): unconditional community root flush at boot (mint pattern)"`

---

### Task 9: Mail-root retry driver

**Files:**
- Modify: `src-tauri/src/event_loop.rs` — the one-shot spawn (:2599-2610 region), using `query_mail_root` (:5637)
- Test: `event_loop.rs` tests mod (outcome-mapping helper)

- [ ] **Step 1: Failing test for the outcome mapping** (pure helper so the decision is pinned without a zenoh session):

```rust
    #[test]
    fn mail_root_outcome_mapping_discriminates_empty_from_none() {
        // Ok(Some(payload)) — a gateway answered (empty payload is the
        // valid "no mail yet" sentinel): satisfied.
        assert_eq!(
            map_mail_root_outcome(&Ok(Some(Vec::new()))),
            crate::channel_backfill::RootFetch::Answered
        );
        assert_eq!(
            map_mail_root_outcome(&Ok(Some(vec![1, 2, 3]))),
            crate::channel_backfill::RootFetch::Answered
        );
        // Ok(None) — zero responders: NOT satisfied, retry.
        assert_eq!(
            map_mail_root_outcome(&Ok(None)),
            crate::channel_backfill::RootFetch::NoReply
        );
        // Err — query failed (session hiccup): retry.
        assert_eq!(
            map_mail_root_outcome(&Err("boom".into())),
            crate::channel_backfill::RootFetch::NoReply
        );
    }
```

(Adjust the `Result` error type to `query_mail_root`'s actual signature at :5637.)

- [ ] **Step 2: Run to verify failure.**

- [ ] **Step 3: Implement.**

```rust
/// ZEB-434 D9: classify a mail-root query result for the retry latch.
/// An empty-payload reply is a VALID answer ("no mail yet" sentinel);
/// only zero-responders / query failure retries.
fn map_mail_root_outcome(
    result: &Result<Option<Vec<u8>>, String>,
) -> crate::channel_backfill::RootFetch {
    match result {
        Ok(Some(_)) => crate::channel_backfill::RootFetch::Answered,
        Ok(None) | Err(_) => crate::channel_backfill::RootFetch::NoReply,
    }
}
```

Replace the one-shot startup spawn (:2599) with the driver. The existing spawn calls `query_mail_root(&session_clone, &key, "startup")` then `sync.handle_startup_query_reply(Some(&payload))` / `sync.report_query_error(...)`. New form:

```rust
        {
            let session_mail = Arc::clone(&session);
            let key_mail = own_root_key.clone();
            let sync_mail = Arc::clone(&sync);
            // event_loop holds the watch SENDER (it does the bumping);
            // internal consumers derive receivers from it.
            let epoch_rx_mail = Some(transport_epoch_tx.subscribe());
            // Shutdown bridge: flip the watch when the loop's closing
            // flag flips (1s poll wrapper — mirrors the adapter tasks'
            // closing-poll discipline).
            let (mail_shutdown_tx, mail_shutdown_rx) =
                tokio::sync::watch::channel(false);
            {
                let closing_mail = Arc::clone(&closing);
                tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                        if closing_mail.load(Ordering::SeqCst) {
                            let _ = mail_shutdown_tx.send(true);
                            return;
                        }
                    }
                });
            }
            let request_root = move || {
                let session = Arc::clone(&session_mail);
                let key = key_mail.clone();
                let sync = Arc::clone(&sync_mail);
                async move {
                    let result = query_mail_root(&session, &key, "startup").await;
                    match &result {
                        Ok(Some(payload)) => {
                            Arc::clone(&sync).handle_startup_query_reply(Some(payload)).await;
                        }
                        Ok(None) => {
                            sync.report_query_error(
                                "no gateway responded to startup query".into(),
                            );
                        }
                        Err(e) => sync.report_query_error(e.clone()),
                    }
                    map_mail_root_outcome(&result)
                }
            };
            tokio::spawn(crate::channel_backfill::run_root_fetch_driver(
                crate::channel_backfill::RootFetchLatch::new(),
                request_root,
                mail_shutdown_rx,
                epoch_rx_mail,
                || {
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0)
                },
            ));
        }
```

Match the real surrounding variable names (`session_clone`, the key variable, the `sync` Arc, `closing`) from the :2599 region — keep the existing log line semantics (the "no responder" message now logs per retry attempt via `report_query_error`, which is the existing behavior class; `handle_startup_query_reply` clears error state on success). `handle_startup_query_reply` takes `Option<&[u8]>` — pass `Some(payload.as_slice())` if the type requires it.

- [ ] **Step 4: Run lib gates.**

- [ ] **Step 5: Commit** — `git commit -m "feat(zeb-434): mail-root startup query retries until a gateway answers"`

---

### Task 10: End-to-end repro test (pull heals an offline-created channel)

**Files:**
- Test: `src-tauri/src/community_state_sync.rs` tests mod (in-file two-engine pattern; NOT a new integration binary — avoids a fresh link target)

- [ ] **Step 1: Write the test** (this is the ticket's repro, sans zenoh — the serve/fetch bridge is exercised through the same channels production wires to the adapter):

```rust
    #[tokio::test]
    async fn offline_created_channel_heals_via_root_fetch_pull() {
        // A (creator, online): engine with root_serve_rx wired; insert
        // a ChannelCreate via insert_local_event; DRAIN A's
        // publisher_rx and drop the bytes — simulating the publish
        // that fired while B was offline (publish-into-the-void).
        //
        // B (member, rebooting): fresh engine, empty state, normal
        // subscriber_tx/rx pair.
        //
        // Pull: spawn run_root_fetch_driver with a request closure
        // that bridges to A — send RootServeRequest into A's serve
        // channel, forward the packet into B's subscriber_tx, return
        // Answered. (This is exactly what the adapter's fetch task +
        // queryable do across zenoh; the test collapses the wire.)
        //
        // Assert: within a bounded poll loop, B's state materializes
        // the channel (id + name match), and B's replay tracker
        // accepted exactly one publish. Then send a SECOND fetch
        // through the same bridge and assert B's CRDT is unchanged
        // (AlreadyKnown dedup — idempotent merges).
    }
```

Setup boilerplate comes from the Task 3 test (same fixtures); the **normative assertions** are: (1) channel materializes on B purely via the serve→fetch path, (2) second fetch is a no-op (`InsertOutcome::AlreadyKnown` / unchanged event count), (3) A's publisher_rx drain proves no pub/sub path was involved.

- [ ] **Step 2: Run it** — `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(offline_created_channel_heals)'` — PASS.

- [ ] **Step 3: Commit** — `git commit -m "test(zeb-434): repro pin — offline-created channel heals via root-fetch pull"`

---

### Task 11: Final sweep + docs

- [ ] **Step 1: Full gates** (ZEB-428's keychain gate is on main — unscoped sweeps are safe):

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures --no-fail-fast
```

Expected: fmt/clippy clean; nextest green except the known-local `rename_content_integration` failures (ZEB-420 set, 11 tests — ignore) and possible iroh/zenoh transport orphan-flakes (re-run individually before dismissing).

- [ ] **Step 2: Frontend untouched** — run `npx tsc --noEmit && npx vitest run` from repo root once to confirm no accidental coupling. Expected: green.

- [ ] **Step 3: Spec cross-check** — re-read the spec's D1-D12 against the diff; fix gaps.

- [ ] **Step 4: Commit any final polish** — `git commit -m "chore(zeb-434): final sweep polish"` (only if changes exist).

---

## Post-plan (controller, not implementer)

Push branch, open PR titled `ZEB-434: community-state reconnect catch-up (root-fetch pull + boot flush + transport re-arm)`. PR body references **ONLY ZEB-434** (no other ticket IDs — Linear's GH integration closes every ID in a merged PR body). Never write the at-mention form of Greptile. Run the autonomous bot+CI convergence loop; pushover Jake at ready-to-merge.
