# ZEB-418 SP2 P3a — Channel-log Backfill Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Members who were offline — or who joined after the fact — receive channel history as soon as any holder is online: engine-start + new-channel backfill triggers with a paging retry latch on the existing zenoh queryable path.

**Architecture:** Spec `docs/specs/2026-06-10-zeb-418-sp2-p3a-channel-backfill-design.md` (D19–D26, step-zero gate RESOLVED — serve-time re-seal under current ChannelKey). New pure-logic module `channel_backfill.rs` (latch + paging state machine); completion plumbing through the existing `BackfillQueryRequest` qr-driver; triggers wired at registry `spawn` (engine start — covers join, since a fresh log gives `since=None`) and at mid-session channel discovery. Verification parity is already PRESENT (`verify_channel_event` does author-auth + `snapshot_at` membership-at-HLC) — backfilled replies enter `process_inbound_packet` unchanged. No new wire formats.

**Tech Stack:** Rust (tokio, zenoh queryables), existing `ChannelLogEngine`/`ChannelLogRegistry`/`RegistryFixture` test harness. No frontend work.

**Verified code anchors (deep-explore 2026-06-10, branch base `266cd823`):**
- `request_backfill(self: Arc<Self>, since: Option<Hlc>)` — `community_channel_log_engine.rs:671-679`; sends `BackfillQueryRequest { since, limit: 0 }` on `query_request_tx`. Callers today: IPC `request_channel_backfill` (lib.rs:17425) + 2 unit tests. No automatic trigger exists.
- qr-driver: `event_loop.rs:7191-7223` — `session.get(&key).consolidation(ConsolidationMode::None)`, replies via `receiver.recv_async()` until stream close (**completion observable**), fed into the engine's inbound path.
- Serving resolver: `event_loop.rs:7076-7143` — parses `since/{hlc}/{limit}`, clamps limit (`CHANNEL_BACKFILL_MAX_LIMIT` = 1000, default `backfill_default_limit`), calls `read_for_query(since, limit)` = `list_messages` + `encrypt_channel_packet(channel_key_ref(), ev)` per event (engine.rs:~1505-1530). Empty result → zero replies, stream still completes.
- Verification: `process_inbound_packet(self: &Arc<Self>, packet: Vec<u8>)` engine.rs:758 → `verify_channel_event` (`community_channel_log.rs:644-770`): decrypt → replay `would_accept` → misroute → `snapshot_at(channel_id, author, at)` membership-at-HLC + channel-existence-at-HLC → device-key sig. Replay tracker dedupe key `(channel_id, author, device_id)`.
- Engine lifecycle: constructed `ChannelLogEngine::new` (engine.rs:330-435 — reloads disk log, rebuilds replay tracker); spawned by `ChannelLogRegistry::spawn` (engine.rs:1403-1439, idempotent) from `reconcile_from_state` (boot, engine.rs:1779-1847), `create_community_inner`, `redeem_invite_inner`.
- Transport-recovery hook: ABSENT (subscriber declared once, event_loop.rs:7023-7066). Spec v1 fallback applies: engine-start-only; noted gap.
- Max-HLC watermark accessor: ABSENT — must be added (Task 1).
- Test harness: `RegistryFixture` engine.rs:2890-2998 (in-process zenoh session + adapter drainer), `AlwaysJoinedState` stub engine.rs:1863-1910, `build_engine_fixture` engine.rs:1939-1997.

**House rules for implementers:** work directly on branch `zeb-418-sp2-p3a-channel-backfill` (no worktrees); commit BEFORE gates; per-task gates from `src-tauri` with `set -o pipefail`: `cargo fmt --all && cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(backfill) + test(channel_log)'` (Task 6 adds `--test channel_backfill_integration`); `cargo nextest list --locked -p harmony-app --all-targets --features test-fixtures > /dev/null` as fast full-compile check; Bash timeout param 600000 (macOS has no `timeout`); NO pushes. Pinned wire fixtures (`EXPECTED_*_HEX`) must NEVER be regenerated. clippy denies `field_reassign_with_default` — struct-literal init in tests.

---

### Task 1: `max_hlc()` watermark accessor

**Files:**
- Modify: `src-tauri/src/community_channel_log.rs` (ChannelLog impl block)
- Test: same file, tests module

The engine-start trigger needs "highest locally-persisted HLC for this channel". Today that requires manually walking `manifest.segments` + `tail`.

- [ ] **Step 1: Write the failing tests** (in the existing `community_channel_log.rs` tests module, following its fixture style for building a `ChannelLog` with events — reuse the module's existing event-builder helpers):

```rust
#[test]
fn max_hlc_none_on_empty_log() {
    let log = ChannelLog::default(); // or the module's empty-log fixture
    assert_eq!(log.max_hlc(), None);
}

#[test]
fn max_hlc_reads_tail_when_present() {
    // Build a log with 2 tail events at HLC wall 100 and 200 (module fixture style).
    // assert_eq!(log.max_hlc(), Some(hlc_200));
}

#[test]
fn max_hlc_reads_last_segment_bound_when_tail_empty() {
    // Build a log whose manifest has one sealed segment with range ending at
    // HLC wall 300 and an empty tail.
    // assert_eq!(log.max_hlc(), Some(hlc_300));
}

#[test]
fn max_hlc_prefers_tail_over_segments() {
    // Segment range ends at 300, tail has an event at 400 → Some(hlc_400).
}
```

(The exact fixture constructors exist in this module's tests — mirror whichever helper `publish_appends_to_tail`-style tests use to mint events; do NOT invent new event shapes.)

- [ ] **Step 2: Run to verify failure**

Run (from `src-tauri`): `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(max_hlc)'`
Expected: FAIL — `max_hlc` not found.

- [ ] **Step 3: Implement**

```rust
/// ZEB-418 P3a: highest locally-persisted event HLC for this channel —
/// the `since` watermark for the engine-start backfill trigger. Tail events
/// are strictly newer than any sealed segment (seal order), so prefer the
/// tail's last event; fall back to the last segment's upper range bound.
/// `None` on a completely empty log (fresh joiner) — which makes the
/// engine-start trigger request full history (spec D23, unified triggers).
pub fn max_hlc(&self) -> Option<Hlc> {
    if let Some(ev) = self.tail.last() {
        return Some(ev.at().clone());
    }
    self.manifest
        .segments
        .last()
        .map(|seg| seg.range.1.clone())
}
```

Adapt field/accessor names to the real `ChannelLog` shape (`tail`, `manifest.segments`, `range`, and the event's HLC accessor — verify against the struct; if events expose `at` as a field, use `ev.at.clone()`). If `Hlc` is `Copy`, drop the `.clone()`s.

- [ ] **Step 4: Run tests to verify pass** — same command, expected PASS (4/4).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/community_channel_log.rs
git commit -m "feat(zeb-418-p3a): ChannelLog::max_hlc watermark accessor"
```

---

### Task 2: `channel_backfill.rs` — latch + paging state machine (pure logic)

**Files:**
- Create: `src-tauri/src/channel_backfill.rs`
- Modify: `src-tauri/src/lib.rs` (one `pub mod channel_backfill;` line in the module list)
- Test: inside the new file

Pure logic, no zenoh/tokio-time dependencies in the core: decisions in, actions out — so it unit-tests without a runtime (mirrors the `dm_outhold_apply` separation style).

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn hlc(w: u64) -> Hlc {
        // mirror the cheapest Hlc constructor used in dm_outhold tests
        // (wall_ms = w, logical 0, device "t")
        Hlc { wall_ms: w, logical: 0, device_id: "t".into() }
    }

    #[test]
    fn fresh_latch_requests_from_watermark() {
        let mut l = BackfillLatch::new(Some(hlc(100)));
        assert_eq!(l.next_action(0), BackfillAction::Request { since: Some(hlc(100)) });
    }

    #[test]
    fn full_page_advances_since_and_rerequests_immediately() {
        let mut l = BackfillLatch::new(None);
        l.on_page_complete(PageOutcome { events: 1000, max_hlc_seen: Some(hlc(500)), limit: 1000 }, 0);
        assert_eq!(l.next_action(0), BackfillAction::Request { since: Some(hlc(500)) });
        assert!(!l.is_satisfied());
    }

    #[test]
    fn short_page_satisfies() {
        let mut l = BackfillLatch::new(None);
        l.on_page_complete(PageOutcome { events: 3, max_hlc_seen: Some(hlc(50)), limit: 1000 }, 0);
        assert!(l.is_satisfied());
        assert_eq!(l.next_action(0), BackfillAction::Idle);
    }

    #[test]
    fn empty_completed_page_satisfies() {
        // "a served nothing is an answer" (spec D24)
        let mut l = BackfillLatch::new(Some(hlc(7)));
        l.on_page_complete(PageOutcome { events: 0, max_hlc_seen: None, limit: 1000 }, 0);
        assert!(l.is_satisfied());
    }

    #[test]
    fn no_reply_backs_off_exponentially_with_cap() {
        let mut l = BackfillLatch::new(None);
        l.on_no_reply(0); // first failure at t=0
        assert_eq!(l.next_action(0), BackfillAction::WaitUntil(30_000));
        assert_eq!(l.next_action(30_000), BackfillAction::Request { since: None });
        l.on_no_reply(30_000);
        assert_eq!(l.next_action(30_000), BackfillAction::WaitUntil(90_000)); // +60s
        l.on_no_reply(90_000);
        l.on_no_reply(250_000);
        l.on_no_reply(600_000);
        l.on_no_reply(1_300_000);
        // backoff is capped at 600_000 ms between attempts
        if let BackfillAction::WaitUntil(t) = l.next_action(1_300_000) {
            assert_eq!(t, 1_900_000);
        } else {
            panic!("expected WaitUntil at cap");
        }
    }

    #[test]
    fn reset_unsatisfies_with_new_watermark() {
        let mut l = BackfillLatch::new(None);
        l.on_page_complete(PageOutcome { events: 0, max_hlc_seen: None, limit: 1000 }, 0);
        assert!(l.is_satisfied());
        l.reset(Some(hlc(900)));
        assert!(!l.is_satisfied());
        assert_eq!(l.next_action(0), BackfillAction::Request { since: Some(hlc(900)) });
    }
}
```

- [ ] **Step 2: Run to verify failure** — `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(backfill)'` → FAIL (module/types missing).

- [ ] **Step 3: Implement**

```rust
//! ZEB-418 SP2 P3a: backfill retry latch + paging state machine.
//!
//! Pure logic — no zenoh, no tokio time. The driver (Task 4) feeds it
//! page outcomes and wall-clock millis; it answers "what next": request a
//! page, wait, or idle. Spec D24: satisfied = a COMPLETED short or empty
//! page (a served "nothing" is an answer; an unanswered query is not).
//! The serving side caps each reply page (CHANNEL_BACKFILL_MAX_LIMIT), so
//! full history is a paging loop: full page → advance `since` to the max
//! HLC seen → re-request immediately.

use crate::owner_state_types::Hlc;

/// First retry delay after an unanswered request (spec D24).
pub const BACKFILL_RETRY_BASE_MS: u64 = 30_000;
/// Backoff cap between retries (spec D24).
pub const BACKFILL_RETRY_CAP_MS: u64 = 600_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackfillAction {
    /// Fire `request_backfill(since)` now.
    Request { since: Option<Hlc> },
    /// Nothing to do before this wall-clock instant (ms).
    WaitUntil(u64),
    /// Satisfied — no work until `reset()`.
    Idle,
}

/// Outcome of one COMPLETED query (the zenoh get receiver closed).
#[derive(Debug, Clone)]
pub struct PageOutcome {
    /// Events received on this page (post-dedupe count is fine; only the
    /// full-page comparison below uses it).
    pub events: usize,
    /// Highest HLC among the received events (None when events == 0).
    pub max_hlc_seen: Option<Hlc>,
    /// The limit the page was requested with (server clamp).
    pub limit: usize,
}

#[derive(Debug)]
pub struct BackfillLatch {
    since: Option<Hlc>,
    satisfied: bool,
    /// In-flight guard: set when a Request was handed out, cleared by
    /// on_page_complete / on_no_reply.
    in_flight: bool,
    retry_delay_ms: u64,
    next_retry_at_ms: u64,
}

impl BackfillLatch {
    pub fn new(watermark: Option<Hlc>) -> Self {
        Self {
            since: watermark,
            satisfied: false,
            in_flight: false,
            retry_delay_ms: 0,
            next_retry_at_ms: 0,
        }
    }

    pub fn is_satisfied(&self) -> bool {
        self.satisfied
    }

    /// What should the driver do at wall-clock `now_ms`?
    pub fn next_action(&mut self, now_ms: u64) -> BackfillAction {
        if self.satisfied {
            return BackfillAction::Idle;
        }
        if self.in_flight {
            // Driver awaits the in-flight page; nothing new to start.
            return BackfillAction::WaitUntil(self.next_retry_at_ms.max(now_ms));
        }
        if now_ms < self.next_retry_at_ms {
            return BackfillAction::WaitUntil(self.next_retry_at_ms);
        }
        self.in_flight = true;
        BackfillAction::Request { since: self.since.clone() }
    }

    /// A query COMPLETED (stream closed) with `outcome`.
    pub fn on_page_complete(&mut self, outcome: PageOutcome, _now_ms: u64) {
        self.in_flight = false;
        self.retry_delay_ms = 0;
        self.next_retry_at_ms = 0;
        if outcome.events >= outcome.limit && outcome.limit > 0 {
            // Full page: more history may remain — advance and loop.
            if outcome.max_hlc_seen.is_some() {
                self.since = outcome.max_hlc_seen;
            }
            return; // stays unsatisfied; next_action fires immediately
        }
        // Short or empty completed page: a holder answered and had no more.
        self.satisfied = true;
    }

    /// The query window elapsed with zero responders (no holder online).
    pub fn on_no_reply(&mut self, now_ms: u64) {
        self.in_flight = false;
        self.retry_delay_ms = if self.retry_delay_ms == 0 {
            BACKFILL_RETRY_BASE_MS
        } else {
            (self.retry_delay_ms * 2).min(BACKFILL_RETRY_CAP_MS)
        };
        self.next_retry_at_ms = now_ms + self.retry_delay_ms;
    }

    /// Re-arm after satisfaction (e.g. a future transport-recovery hook),
    /// with a fresh watermark.
    pub fn reset(&mut self, watermark: Option<Hlc>) {
        self.since = watermark;
        self.satisfied = false;
        self.in_flight = false;
        self.retry_delay_ms = 0;
        self.next_retry_at_ms = 0;
    }
}
```

Adjust `Hlc` construction/field names to the real type (check `owner_state_types.rs`; if `Hlc` lacks a public 3-field literal, use its constructor). Note the backoff doubling expectation in the test (`30s → 60s → cap 600s`) — `90_000 = 30_000 + 60_000` in the test is `now + delay`; keep test arithmetic consistent with the implementation.

- [ ] **Step 4: Run tests** — same filter, expected PASS (7/7).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/channel_backfill.rs src-tauri/src/lib.rs
git commit -m "feat(zeb-418-p3a): BackfillLatch paging + retry state machine"
```

---

### Task 3: Page-completion plumbing through the qr-driver

**Files:**
- Modify: `src-tauri/src/community_channel_log_engine.rs` (`BackfillQueryRequest`, `request_backfill`)
- Modify: `src-tauri/src/event_loop.rs` (qr-driver, ~7191-7223)
- Test: engine tests module (extend the two existing `request_backfill_*` tests)

Today the qr-driver fires `session.get`, feeds replies into the inbound path, and tells no one when the stream closes. The latch needs `PageOutcome`.

- [ ] **Step 1: Write/extend failing tests** — extend `request_backfill_queues_query_request` (engine.rs:2840) and `request_backfill_passes_since_through` (engine.rs:2856) for the new field, plus a new test:

```rust
#[tokio::test]
async fn request_backfill_with_outcome_carries_responder_channel() {
    // build_engine_fixture, call engine.request_backfill_with_outcome(None, tx)
    // where (tx, rx) = tokio::sync::oneshot::channel::<BackfillPageReport>();
    // assert the BackfillQueryRequest received on query_request_rx carries
    // outcome_tx = Some(..) and limit == 0.
}
```

- [ ] **Step 2: Run to verify failure** — `-E 'test(request_backfill)'` → FAIL.

- [ ] **Step 3: Implement**

In the engine:

```rust
/// What the qr-driver reports back when one backfill query completes
/// (ZEB-418 P3a). `replies` counts raw packets received (pre-verification —
/// the latch only needs full-page detection); `responded` is false when the
/// stream closed with zero replies AND zenoh reported no responders
/// (no holder online).
#[derive(Debug)]
pub struct BackfillPageReport {
    pub replies: usize,
    pub max_hlc_seen: Option<Hlc>,
    pub limit: usize,
}

pub struct BackfillQueryRequest {
    pub since: Option<Hlc>,
    pub limit: usize,
    /// ZEB-418 P3a: when Some, the qr-driver sends exactly one report after
    /// the reply stream closes. None preserves the existing fire-and-forget
    /// behavior (IPC `request_channel_backfill` keeps working unchanged).
    pub outcome_tx: Option<tokio::sync::oneshot::Sender<BackfillPageReport>>,
}

pub async fn request_backfill_with_outcome(
    self: Arc<Self>,
    since: Option<Hlc>,
    outcome_tx: tokio::sync::oneshot::Sender<BackfillPageReport>,
) -> Result<(), ChannelLogEngineError> {
    // identical to request_backfill but with outcome_tx: Some(outcome_tx)
}
```

Keep `request_backfill` as a thin wrapper sending `outcome_tx: None` (existing callers unchanged). In the qr-driver (event_loop.rs ~7191-7223): count replies as they arrive, track the max event HLC (the reply packet's HLC is only knowable post-decrypt — instead have the driver report `max_hlc_seen: None` and let Task 4's driver compute the watermark from the LOG after the page lands; simplify `BackfillPageReport` accordingly if cleaner: `replies` + `limit` is sufficient because the post-page watermark is just `log.max_hlc()`). After `recv_async()` errors (stream closed), send the report if `outcome_tx` was Some. **Design note for the implementer:** the simpler shape — report = `{ replies, limit }`, watermark re-read from `ChannelLog::max_hlc()` after the page — is PREFERRED; adjust `PageOutcome.max_hlc_seen` plumbing in Task 4 to read the log instead. Update Task 2's latch only if its tests need the field renamed; the state machine logic is unchanged either way.

Also thread `limit` for the GET selector: the driver builds the key with the same clamped default the server uses; pass the effective limit back in the report so the latch's full-page comparison is correct.

- [ ] **Step 4: Run tests** — `-E 'test(request_backfill)'` → PASS; also `-E 'test(channel_log)'` for regressions.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/community_channel_log_engine.rs src-tauri/src/event_loop.rs
git commit -m "feat(zeb-418-p3a): qr-driver page-completion report for backfill queries"
```

---

### Task 4: Engine-start backfill driver (triggers 1+2 unified)

**Files:**
- Modify: `src-tauri/src/community_channel_log_engine.rs` (registry `spawn`, engine fields)
- Modify: `src-tauri/src/channel_backfill.rs` (the async driver fn)
- Test: `channel_backfill.rs` tests (driver loop with stub requester) + engine tests

- [ ] **Step 1: Write the failing driver test** (in `channel_backfill.rs`; stub requester closure, `tokio::time::pause()` logical time per house rule — wall-clock budgets must be ≪ thresholds or use paused time):

```rust
#[tokio::test(start_paused = true)]
async fn driver_retries_until_holder_appears_then_satisfies() {
    // Stub requester: first 2 calls report no-reply; 3rd reports a short page.
    // run_backfill_driver(latch, requester, shutdown_rx)
    // advance time past 30s and 90s; assert exactly 3 requests were made and
    // the driver future completes (satisfied).
}

#[tokio::test(start_paused = true)]
async fn driver_pages_through_full_pages_without_backoff() {
    // Stub: two full pages (replies == limit) then a short page; assert 3
    // requests with NO time advancement needed between pages 1→2→3.
}

#[tokio::test(start_paused = true)]
async fn driver_stops_on_shutdown_signal() {
    // Stub: always no-reply. Send shutdown after first backoff arm; assert
    // the driver future returns without further requests.
}
```

- [ ] **Step 2: Run to verify failure** — `-E 'test(driver_)'` → FAIL.

- [ ] **Step 3: Implement the driver** (in `channel_backfill.rs`):

```rust
/// Drive one channel's latch to satisfaction (or shutdown). `request_page`
/// fires a backfill query and resolves with the completed-page report
/// (replies, limit) or None when zero responders answered. Watermark is
/// re-read via `current_watermark` after each page (the log is the source
/// of truth — replies landed through the verified inbound path).
pub async fn run_backfill_driver<Rq, Wm, RqFut>(
    mut latch: BackfillLatch,
    request_page: Rq,
    current_watermark: Wm,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) where
    Rq: Fn(Option<Hlc>) -> RqFut,
    RqFut: std::future::Future<Output = Option<(usize, usize)>>, // (replies, limit)
    Wm: Fn() -> Option<Hlc>,
{
    loop {
        let now_ms = now_wall_ms();
        match latch.next_action(now_ms) {
            BackfillAction::Idle => return,
            BackfillAction::WaitUntil(t) => {
                let dur = std::time::Duration::from_millis(t.saturating_sub(now_ms).max(250));
                tokio::select! {
                    _ = tokio::time::sleep(dur) => {}
                    _ = shutdown_rx.changed() => { if *shutdown_rx.borrow() { return; } }
                }
            }
            BackfillAction::Request { since } => {
                match request_page(since).await {
                    Some((replies, limit)) => {
                        let mut outcome = PageOutcome { events: replies, max_hlc_seen: None, limit };
                        if replies >= limit && limit > 0 {
                            outcome.max_hlc_seen = current_watermark();
                        }
                        latch.on_page_complete(outcome, now_wall_ms());
                    }
                    None => latch.on_no_reply(now_wall_ms()),
                }
            }
        }
    }
}

fn now_wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
```

(For paused-time tests, `now_wall_ms` must come from a injectable clock OR the tests assert via request counts + `tokio::time::advance` with the driver reading `tokio::time::Instant` — implementer picks the house-consistent shape: `dm_outhold_apply`'s sweeper tests are the precedent. The non-negotiable: tests use logical time, no real sleeps.)

Wire into registry `spawn` (engine.rs:1403-1439): after the engine entry is inserted, spawn `run_backfill_driver` with `latch = BackfillLatch::new(log.max_hlc())`, `request_page` = closure over `request_backfill_with_outcome` + oneshot await (map zero-replies-no-responder to `None`), `current_watermark` = closure over the engine's log `max_hlc()`, and a shutdown receiver tied to the engine entry's `closing` flag (mirror how the existing adapter drainer observes `closing`). One driver per spawned engine; it self-terminates at satisfaction — total steady-state cost = nothing.

- [ ] **Step 4: Run tests** — `-E 'test(backfill) + test(channel_log)'` → PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/channel_backfill.rs src-tauri/src/community_channel_log_engine.rs
git commit -m "feat(zeb-418-p3a): engine-start backfill driver (join + reconnect unified)"
```

---

### Task 5: Mid-session new-channel discovery (trigger 3)

**Files:**
- Modify: `src-tauri/src/community_channel_log_engine.rs` and/or the community-state CRDT apply path (locate precisely — see Step 1)
- Test: engine tests module

The deep-explore found NO incremental hook: `reconcile_from_state` runs only at boot, and there is no Tauri/internal event on channel-config updates. **First verify** how a channel created mid-session by a REMOTE member gets an engine today (a live message for an unknown channel must hit SOMETHING — find it: grep the inbound community-state apply path for `registry.spawn` / `ChannelCreated` handling). Two cases:

- **(a) A spawn-on-demand path exists** (e.g. community-state materialize → reconcile-like diff): add the backfill driver to that spawn (it comes free if Task 4 hooked `spawn` itself — then this task is just the TEST proving it).
- **(b) No path exists** (mid-session remote channel creation leaves no local engine until restart): implement the hook — after the community-state CRDT applies a batch, diff `materialized.channels` against `registry` keys and `spawn` the missing ones (idempotent, engine.rs:1403 spawn already guards double-insert). Task 4's hook then fires backfill automatically.

- [ ] **Step 1: Investigate and write the failing test** — a registry-fixture test: materialize a channel-config containing a channel the registry has no engine for, run the hook, assert the engine exists and a `BackfillQueryRequest` with `since: None` was emitted on its query channel.

- [ ] **Step 2: Run to verify failure.**

- [ ] **Step 3: Implement** per case (a) or (b) above. Keep the diff minimal: the hook calls the SAME `registry.spawn` used at boot — no second spawn recipe.

- [ ] **Step 4: Run** `-E 'test(backfill) + test(channel_log)'` → PASS.

- [ ] **Step 5: Commit**

```bash
git add -A src-tauri/src
git commit -m "feat(zeb-418-p3a): mid-session new-channel discovery spawns engine + backfill"
```

---

### Task 6: Two-engine integration tests (spec §8 scenarios)

**Files:**
- Create: `src-tauri/tests/channel_backfill_integration.rs`

Build on the `RegistryFixture` pattern (engine.rs:2890-2998): one in-process zenoh session, TWO registries (A = holder, B = joiner/reconnector), each with its own data dir + adapter drainer, sharing the session so pub/sub + queryables route. Use real `verify_channel_event` state (the fixture's stub state must let A's author pass membership-at-HLC on B's side — reuse `AlwaysJoinedState` for the accept cases and a "not joined at HLC" variant for the reject case).

- [ ] **Step 1: Write the four tests** (each follows: build A, publish K events through A's engine, then build/start B per scenario):

```rust
#[tokio::test]
async fn pre_join_history_backfills_to_new_member() {
    // ZEB-403 repro: A publishes 3 posts. THEN B's registry spawns the
    // channel engine (empty log → since None). Await until B's
    // list_messages returns all 3 (bounded condition-poll, logical retries —
    // no fixed sleeps). Assert order + replay-tracker accepted each once.
}

#[tokio::test]
async fn reconnect_catch_up_fetches_exactly_missed_events() {
    // B starts, receives 2 live posts, B's registry stops (engine shutdown).
    // A publishes 3 more. B respawns (watermark = HLC of post 2) → backfill
    // returns only the 3 missed; total 5, no duplicates (replay tracker).
}

#[tokio::test]
async fn eventual_convergence_when_holder_appears_late() {
    // B spawns FIRST with A's registry not yet started → first page
    // no-reply → latch backoff. Then start A (holder). Advance/poll until
    // B converges. Proves D21 retry. Use the shortened-backoff knob if the
    // 30s base makes the test slow: expose BACKFILL_RETRY_BASE_MS override
    // via ChannelLogEngineConfig for tests (default 30_000) — mirror how
    // seal-threshold/flush-debounce are already config params.
}

#[tokio::test]
async fn backfilled_event_from_non_member_at_hlc_is_rejected() {
    // A's log contains an event whose author the B-side state reports as
    // NOT joined at that HLC (stub state variant). Backfill serves it;
    // B's verify_channel_event rejects; B's list_messages excludes it.
}
```

- [ ] **Step 2: Run to verify failure / build out fixtures** — `cargo nextest run --locked -p harmony-app --features test-fixtures --test channel_backfill_integration` (FAIL → iterate).

- [ ] **Step 3: Make all four pass.** Condition-based waiting only (poll with deadline, logical time where possible); the eventual-convergence test MUST use the config-injected retry base (e.g. 200ms) — never a 30s real-time wait, and the wall-clock budget must be ≪ any assert threshold.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/tests/channel_backfill_integration.rs src-tauri/src
git commit -m "test(zeb-418-p3a): two-engine backfill integration — pre-join, catch-up, convergence, rejection"
```

---

### Task 7: Final sweep + docs

**Files:**
- Modify: `docs/plans/2026-06-10-zeb-418-sp2-p3a-channel-backfill-plan.md` (check boxes), implementation-notes doc if deviations accumulated

- [ ] **Step 1:** From `src-tauri`, full gates (each command Bash timeout 600000, `set -o pipefail`):
  - `cargo fmt --all -- --check` → exit 0
  - `cargo clippy --locked -p harmony-app --all-targets --features test-fixtures --no-deps -- -D warnings` → exit 0
  - `cargo nextest run --locked -p harmony-app --all-targets --features test-fixtures` → only permissible failures: `rename_content_integration` port-4242 locals (ZEB-420, never chase)
- [ ] **Step 2:** From repo root: `npx tsc --noEmit` and `npx vitest run` → clean (no frontend changes expected; this is the regression check).
- [ ] **Step 3:** Commit any sweep fixes; write `docs/plans/…-implementation-notes.md` only if tasks deviated from plan.

---

## Implementation notes (pre-known)

1. **Transport-recovery hook is ABSENT** (event_loop.rs:7023-7066 declares the subscriber once). Spec §4.1's plan-time check is resolved: v1 = engine-start-only, gap documented here. The latch's `reset()` exists for the future hook.
2. **IPC `request_channel_backfill` (lib.rs:17425)** stays fire-and-forget (`outcome_tx: None`) — do not change its signature.
3. **Replay tracker pre-population at engine start** (engine.rs:373-384) means re-served events the log already holds are dropped before append — the reconnect test's "no duplicates" assertion rides on this; do not bypass it.
4. **`BackfillPageReport` simplification** (Task 3): prefer `{ replies, limit }` + post-page `log.max_hlc()` re-read over decrypt-aware HLC tracking in the qr-driver. Replies counted pre-verification — fine: worst case a hostile holder serves N garbage packets → full-page loop re-requests from an unchanged watermark and the next short page satisfies; verification still gates every append.
5. **Backoff-base config knob** (Task 6): expose via `ChannelLogEngineConfig` like the existing seal/flush params; production default 30_000; integration tests inject ~200ms.
6. **No new wire formats** (D26): the only wire surface touched is the existing GET selector; `EXPECTED_*_HEX` fixtures untouched.
