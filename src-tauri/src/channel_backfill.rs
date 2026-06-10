//! ZEB-418 SP2 P3a: channel-history backfill latch — paging + retry
//! state machine.
//!
//! When a device joins a channel (or reconnects after downtime) it asks
//! online holders for the channel history it missed. The serving side
//! caps every reply page (hard cap 1000 events per request), and a query
//! can go entirely unanswered when no holder is online. This module is
//! the pure decision core for the requesting side.
//!
//! ## Spec D24 semantics (as amended)
//!
//! - **Satisfied** means a *completed* short or empty page was received:
//!   a served "nothing more" is an answer; an unanswered query is not.
//! - **Full page** (`events >= limit`, `limit > 0`) means the holder hit
//!   its page cap and more history may remain. If the verified watermark
//!   moved (`max_hlc_seen` is `Some` and differs from the requested
//!   `since`), advance `since` and re-request **immediately** — no
//!   backoff between progressing pages. This is the paging loop. A full
//!   page that does NOT move the watermark (hostile holder serving
//!   garbage that fails verification, all-duplicate replies,
//!   cross-config limits) instead arms the same backoff as a no-reply:
//!   the latch stays unsatisfied (history may genuinely remain) but
//!   stops hammering the same window.
//! - **No reply** (zero responders) backs off exponentially: first retry
//!   after [`BACKFILL_RETRY_BASE_MS`] (30 s), doubling per consecutive
//!   miss, capped at [`BACKFILL_RETRY_CAP_MS`] (600 s) between attempts,
//!   retrying forever — the async driver enforces shutdown, not the
//!   latch.
//! - [`BackfillLatch::reset`] re-arms a satisfied latch with a new
//!   watermark (future transport-recovery hook).
//!
//! ## Why pure logic
//!
//! The latch holds no zenoh handle and no tokio types: decisions go in
//! (page outcomes, wall-clock milliseconds) and actions come out
//! ([`BackfillAction::Request`] / [`BackfillAction::WaitUntil`] /
//! [`BackfillAction::Idle`]), so the full paging/backoff/in-flight state
//! space is testable without a runtime. [`run_backfill_driver`] is the
//! async interpreter of those actions — transport access stays behind
//! injected closures, so the driver too is testable with stubs.
//! Precedent: `dm_outhold_apply`'s sweeper core + sweeper task split.
//!
//! Spec: `docs/specs/2026-06-10-zeb-418-sp2-p3a-channel-backfill-design.md`.

use crate::owner_state_types::Hlc;

/// First retry delay after an unanswered backfill request (30 s).
pub const BACKFILL_RETRY_BASE_MS: u64 = 30_000;

/// Maximum delay between retry attempts (600 s). Doubling stops here;
/// the latch retries forever at this cadence until answered.
pub const BACKFILL_RETRY_CAP_MS: u64 = 600_000;

/// What the driver should do next, as decided by [`BackfillLatch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackfillAction {
    /// Send a backfill request for events strictly after `since`
    /// (`None` = from the beginning of channel history).
    Request { since: Option<Hlc> },
    /// Nothing to do until the given wall-clock instant (ms); re-poll
    /// `next_action` at or after that time.
    WaitUntil(u64),
    /// The latch is satisfied — no further requests needed until
    /// [`BackfillLatch::reset`].
    Idle,
}

/// Result of a *completed* backfill reply page (a holder answered).
///
/// An unanswered query is NOT a `PageOutcome` — report that via
/// [`BackfillLatch::on_no_reply`] instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageOutcome {
    /// Number of events the page carried.
    pub events: usize,
    /// Maximum HLC among the page's events (`None` for an empty page).
    pub max_hlc_seen: Option<Hlc>,
    /// The per-request limit the serving side was asked for (page cap).
    pub limit: usize,
}

/// Retry latch + paging state machine for one channel's backfill.
///
/// Drive it by polling [`next_action`](Self::next_action) with the
/// current wall clock and feeding outcomes back via
/// [`on_page_complete`](Self::on_page_complete) /
/// [`on_no_reply`](Self::on_no_reply). At most one request is
/// outstanding at a time (in-flight guard).
#[derive(Debug, Clone)]
pub struct BackfillLatch {
    /// Request events strictly after this HLC (`None` = from start).
    since: Option<Hlc>,
    /// Set by a completed short/empty page; cleared by `reset`.
    satisfied: bool,
    /// A `Request` has been handed out and neither `on_page_complete`
    /// nor `on_no_reply` has been called yet.
    in_flight: bool,
    /// Earliest wall-clock ms at which the next request may be sent.
    next_retry_at: u64,
    /// Current backoff delay (ms); 0 = no consecutive no-reply yet.
    retry_delay_ms: u64,
    /// First-retry delay after an unanswered request (ms). Production
    /// = [`BACKFILL_RETRY_BASE_MS`]; tests inject smaller values via
    /// [`Self::new_with_backoff`] (threaded from
    /// `ChannelLogEngineConfig.backfill_retry_base_ms`).
    retry_base_ms: u64,
    /// Backoff ceiling (ms). Production = [`BACKFILL_RETRY_CAP_MS`].
    retry_cap_ms: u64,
}

impl BackfillLatch {
    /// New, unsatisfied latch requesting history after `watermark`,
    /// with the production (spec D24) backoff schedule.
    pub fn new(watermark: Option<Hlc>) -> Self {
        Self::new_with_backoff(watermark, BACKFILL_RETRY_BASE_MS, BACKFILL_RETRY_CAP_MS)
    }

    /// New latch with an explicit backoff schedule (ZEB-418 P3a Task 6:
    /// test-injectable; spec D24 base/cap are the production values —
    /// see [`Self::new`]).
    pub fn new_with_backoff(watermark: Option<Hlc>, base_ms: u64, cap_ms: u64) -> Self {
        Self {
            since: watermark,
            satisfied: false,
            in_flight: false,
            next_retry_at: 0,
            retry_delay_ms: 0,
            retry_base_ms: base_ms,
            retry_cap_ms: cap_ms,
        }
    }

    /// True once a completed short/empty page has answered the query.
    pub fn is_satisfied(&self) -> bool {
        self.satisfied
    }

    /// Decide the next driver action at wall-clock `now_ms`.
    pub fn next_action(&mut self, now_ms: u64) -> BackfillAction {
        if self.satisfied {
            return BackfillAction::Idle;
        }
        if self.in_flight {
            // A request is already outstanding; nothing new until an
            // outcome lands. `next_retry_at` may sit in the past here
            // (it is only re-armed by `on_no_reply`), so clamp to `now`.
            return BackfillAction::WaitUntil(self.next_retry_at.max(now_ms));
        }
        if now_ms < self.next_retry_at {
            return BackfillAction::WaitUntil(self.next_retry_at);
        }
        self.in_flight = true;
        BackfillAction::Request {
            since: self.since.clone(),
        }
    }

    /// Record a completed reply page (clears in-flight).
    ///
    /// Short or empty page satisfies the latch (spec D24: a served
    /// "nothing" is an answer) and resets backoff. Full page (`events
    /// >= limit`, `limit > 0`) means more history may remain:
    ///
    /// - **Progress** (`max_hlc_seen` is `Some` and differs from the
    ///   current `since`): advance `since`, reset backoff, and leave
    ///   the latch unsatisfied so the next `next_action(now)`
    ///   re-requests immediately — the paging loop.
    /// - **No progress** (`max_hlc_seen` is `None` or equals the
    ///   current `since`): see the no-progress branch below.
    pub fn on_page_complete(&mut self, outcome: PageOutcome, now_ms: u64) {
        self.in_flight = false;

        let full_page = outcome.limit > 0 && outcome.events >= outcome.limit;
        if !full_page {
            // Spec D24: a served "nothing more" is an answer.
            self.satisfied = true;
            self.retry_delay_ms = 0;
            self.next_retry_at = now_ms;
            return;
        }
        let progressed = outcome.max_hlc_seen.is_some() && outcome.max_hlc_seen != self.since;
        if progressed {
            // Paging loop: more history may remain behind the cap and
            // the verified watermark moved — re-request immediately
            // from the new window, resetting the no-reply backoff.
            self.since = outcome.max_hlc_seen;
            self.retry_delay_ms = 0;
            self.next_retry_at = now_ms;
        } else {
            // No-progress full page: the verified watermark did not
            // move past the window we asked for, so an immediate
            // re-request would replay the exact same window — a
            // hostile holder serving garbage that fails verification
            // (or one that keeps serving already-held duplicates)
            // would otherwise drive a tight zero-backoff request loop
            // until shutdown. Arm the same escalating backoff as
            // [`Self::on_no_reply`] WITHOUT satisfying the latch:
            // history may genuinely remain, and the holder set can
            // change, so backing off (rather than declaring done)
            // keeps liveness.
            self.arm_backoff(now_ms);
        }
    }

    /// Record an unanswered request (clears in-flight, arms backoff).
    ///
    /// Delay schedule: `retry_base_ms` (production 30 s), doubling per
    /// consecutive no-reply, capped at `retry_cap_ms` (production
    /// 600 s); retries forever (driver enforces shutdown).
    pub fn on_no_reply(&mut self, now_ms: u64) {
        self.in_flight = false;
        self.arm_backoff(now_ms);
    }

    /// Escalate the retry backoff and arm `next_retry_at`. Shared by
    /// [`Self::on_no_reply`] and the no-progress full-page branch of
    /// [`Self::on_page_complete`]. Delegates the step computation to
    /// [`arm_backoff_step`].
    fn arm_backoff(&mut self, now_ms: u64) {
        self.retry_delay_ms =
            arm_backoff_step(self.retry_delay_ms, self.retry_base_ms, self.retry_cap_ms);
        self.next_retry_at = now_ms + self.retry_delay_ms;
    }

    /// Re-arm a satisfied latch with a new watermark (transport-recovery
    /// hook); clears in-flight and backoff state. Preserves the
    /// configured backoff schedule.
    pub fn reset(&mut self, watermark: Option<Hlc>) {
        *self = Self::new_with_backoff(watermark, self.retry_base_ms, self.retry_cap_ms);
    }
}

// ── Async driver (ZEB-418 P3a Task 4) ───────────────────────────────────────

/// Outcome of one page-fetch attempt, as seen by the driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageFetch {
    /// Query completed cleanly (replies, effective_limit).
    Completed(usize, usize),
    /// No responders / query aborted — transient; back off and retry.
    NoReply,
    /// The engine/adapter is gone for good — stop the driver.
    EngineGone,
}

/// Floor for the driver's backoff sleeps (ms). Defensive: guards
/// against a hot loop if the injected clock ever lags the latch's
/// `next_retry_at` (the latch's in-flight `WaitUntil` clamp can hand
/// back a target equal to `now`).
const BACKFILL_DRIVER_MIN_WAIT_MS: u64 = 250;

/// Drive one channel's [`BackfillLatch`] to satisfaction, then return.
///
/// Spawned by `ChannelLogRegistry::spawn` for every freshly inserted
/// engine entry — running it unconditionally at engine start unifies
/// the spec's join + reconnect triggers: a fresh joiner's empty log
/// gives a `None` watermark (request full history), a reconnecting
/// device's reloaded log gives `Some(watermark)` (catch-up).
///
/// - `request_page(since)` issues one backfill query and resolves to
///   a [`PageFetch`]: `Completed(replies, limit)` after the reply
///   stream closes cleanly; `NoReply` on no-reply/abort (mapped to
///   backoff via [`BackfillLatch::on_no_reply`]); `EngineGone` when
///   the engine/adapter is permanently gone (no recovery hook exists
///   — the driver returns instead of retrying forever).
/// - `current_watermark()` re-reads the max HLC from the LOG after a
///   full page. The watermark is deliberately NOT taken from the raw
///   reply packets: only verified events land in the log, so a hostile
///   holder serving garbage can't advance `since` past history it
///   never actually delivered. (Async because the log sits behind a
///   `tokio::sync::Mutex` — a sync `try_lock` here could miss under
///   contention and stall `since`, spinning the no-backoff paging
///   loop.)
/// - `shutdown_rx` flipping to `true` — or its sender dropping —
///   ends the driver promptly during a backoff wait.
/// - `now_ms` injects the wall clock (dm_outhold_apply testability
///   precedent): production passes a `SystemTime`-based closure;
///   tests pair a `tokio::time::Instant`-based closure with paused
///   time so no test ever sleeps for real.
pub async fn run_backfill_driver<Rq, RqFut, Wm, WmFut>(
    mut latch: BackfillLatch,
    request_page: Rq,
    current_watermark: Wm,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
    now_ms: impl Fn() -> u64,
) where
    Rq: Fn(Option<Hlc>) -> RqFut,
    RqFut: std::future::Future<Output = PageFetch>,
    Wm: Fn() -> WmFut,
    WmFut: std::future::Future<Output = Option<Hlc>>,
{
    loop {
        // Cheap pre-check: covers "stopped before the driver's first
        // poll" (spawn/stop race) without waiting for a `changed()`.
        if *shutdown_rx.borrow() {
            return;
        }
        match latch.next_action(now_ms()) {
            BackfillAction::Idle => return,
            BackfillAction::WaitUntil(target) => {
                let wait_ms = target
                    .saturating_sub(now_ms())
                    .max(BACKFILL_DRIVER_MIN_WAIT_MS);
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_millis(wait_ms)) => {}
                    changed = shutdown_rx.changed() => {
                        // Err = sender dropped (registry entry gone):
                        // same as an explicit shutdown signal.
                        if changed.is_err() || *shutdown_rx.borrow() {
                            return;
                        }
                    }
                }
            }
            BackfillAction::Request { since } => match request_page(since).await {
                PageFetch::Completed(replies, limit) => {
                    let full_page = limit > 0 && replies >= limit;
                    // Watermark re-read from the LOG (only verified
                    // events land there) rather than trusted from the
                    // raw reply packets — a hostile holder serving
                    // garbage can't advance `since`. See the
                    // `current_watermark` doc above.
                    let max_hlc_seen = if full_page {
                        current_watermark().await
                    } else {
                        None
                    };
                    latch.on_page_complete(
                        PageOutcome {
                            events: replies,
                            max_hlc_seen,
                            limit,
                        },
                        now_ms(),
                    );
                }
                PageFetch::NoReply => latch.on_no_reply(now_ms()),
                // Permanent: the engine/adapter is gone for good and
                // no recovery hook exists — stop instead of burning
                // eternal futile retries until engine stop.
                PageFetch::EngineGone => return,
            },
        }
    }
}

// ── Shared backoff helper ────────────────────────────────────────────────────

/// One escalation step of the shared retry-backoff schedule: first
/// retry waits `base` clamped to `cap` (a misconfigured base > cap
/// must not violate the cap), then doubles per consecutive miss up to
/// the cap. Shared by [`BackfillLatch`] and [`RootFetchLatch`].
fn arm_backoff_step(current_delay_ms: u64, base_ms: u64, cap_ms: u64) -> u64 {
    if current_delay_ms == 0 {
        base_ms.min(cap_ms)
    } else {
        (current_delay_ms * 2).min(cap_ms)
    }
}

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
/// Shares the spec backoff schedule (30 s base doubling to a 600 s
/// cap, retrying forever — the driver enforces shutdown).
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
    /// New, unsatisfied latch with the production (spec D3) backoff schedule.
    pub fn new() -> Self {
        Self::new_with_backoff(BACKFILL_RETRY_BASE_MS, BACKFILL_RETRY_CAP_MS)
    }

    /// New latch with an explicit backoff schedule (test-injectable; mirrors
    /// [`BackfillLatch::new_with_backoff`]).
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

    /// True once at least one responder has replied.
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
        self.retry_delay_ms =
            arm_backoff_step(self.retry_delay_ms, self.retry_base_ms, self.retry_cap_ms);
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

/// Wait for a transport-epoch bump. Pends forever when no watch is
/// wired; returns false when the epoch sender dropped.
async fn epoch_bump(epoch_rx: &mut Option<tokio::sync::watch::Receiver<u64>>) -> bool {
    match epoch_rx.as_mut() {
        Some(rx) => rx.changed().await.is_ok(),
        None => std::future::pending().await,
    }
}

/// Defer an epoch-triggered re-arm to the cooldown boundary (deferred,
/// not dropped). Returns false if shutdown fires during the wait.
async fn cooldown_wait(
    last_request_at: Option<u64>,
    now: u64,
    shutdown_rx: &mut tokio::sync::watch::Receiver<bool>,
) -> bool {
    let Some(last) = last_request_at else {
        return true;
    };
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Tiny Hlc builder for tests (mirrors `dm_outhold_apply` tests).
    fn hlc(wall_ms: u64) -> Hlc {
        Hlc {
            wall_ms,
            logical: 0,
            device_id: "dev".into(),
        }
    }

    #[test]
    fn fresh_latch_requests_from_watermark() {
        let mut latch = BackfillLatch::new(Some(hlc(100)));
        assert!(!latch.is_satisfied());
        assert_eq!(
            latch.next_action(0),
            BackfillAction::Request {
                since: Some(hlc(100))
            }
        );
    }

    #[test]
    fn full_page_advances_since_and_rerequests_immediately() {
        let mut latch = BackfillLatch::new(None);
        // Arm in-flight with the first request.
        assert_eq!(
            latch.next_action(0),
            BackfillAction::Request { since: None }
        );
        latch.on_page_complete(
            PageOutcome {
                events: 1000,
                max_hlc_seen: Some(hlc(500)),
                limit: 1000,
            },
            0,
        );
        // Full page → paging loop: re-request immediately from max HLC.
        assert_eq!(
            latch.next_action(0),
            BackfillAction::Request {
                since: Some(hlc(500))
            }
        );
        assert!(!latch.is_satisfied());
    }

    #[test]
    fn full_page_without_progress_backs_off_instead_of_spinning() {
        let mut latch = BackfillLatch::new(Some(hlc(100)));
        assert_eq!(
            latch.next_action(0),
            BackfillAction::Request {
                since: Some(hlc(100))
            }
        );
        // Full page whose verified watermark equals the requested
        // `since`: nothing new landed in the log (hostile holder
        // serving garbage that fails verification, or all-duplicate
        // replies). Re-requesting immediately would replay the same
        // window in a tight zero-backoff loop.
        latch.on_page_complete(
            PageOutcome {
                events: 1000,
                max_hlc_seen: Some(hlc(100)),
                limit: 1000,
            },
            0,
        );
        assert!(
            !latch.is_satisfied(),
            "no-progress must NOT satisfy — history may genuinely remain"
        );
        // Backs off like a no-reply instead of an immediate Request.
        assert_eq!(
            latch.next_action(0),
            BackfillAction::WaitUntil(BACKFILL_RETRY_BASE_MS)
        );
        // Once past the delay, the request fires again (same window) —
        // the holder set can change, so liveness is preserved.
        assert_eq!(
            latch.next_action(BACKFILL_RETRY_BASE_MS),
            BackfillAction::Request {
                since: Some(hlc(100))
            }
        );
    }

    #[test]
    fn first_retry_respects_cap_when_base_exceeds_cap() {
        // Misconfigured base > cap: the first retry must clamp to the
        // cap rather than wait the full (over-cap) base.
        let mut latch = BackfillLatch::new_with_backoff(None, 1_000, 300);
        assert!(matches!(
            latch.next_action(0),
            BackfillAction::Request { .. }
        ));
        latch.on_no_reply(0);
        assert_eq!(latch.next_action(0), BackfillAction::WaitUntil(300));
    }

    #[test]
    fn short_page_satisfies() {
        let mut latch = BackfillLatch::new(None);
        assert!(matches!(
            latch.next_action(0),
            BackfillAction::Request { .. }
        ));
        latch.on_page_complete(
            PageOutcome {
                events: 3,
                max_hlc_seen: Some(hlc(42)),
                limit: 1000,
            },
            0,
        );
        assert!(latch.is_satisfied());
        assert_eq!(latch.next_action(0), BackfillAction::Idle);
    }

    #[test]
    fn empty_completed_page_satisfies() {
        let mut latch = BackfillLatch::new(Some(hlc(7)));
        assert!(matches!(
            latch.next_action(0),
            BackfillAction::Request { .. }
        ));
        latch.on_page_complete(
            PageOutcome {
                events: 0,
                max_hlc_seen: None,
                limit: 1000,
            },
            0,
        );
        assert!(latch.is_satisfied());
        assert_eq!(latch.next_action(0), BackfillAction::Idle);
    }

    #[test]
    fn no_reply_backs_off_exponentially_with_cap() {
        let mut latch = BackfillLatch::new(None);

        // t=0: first request goes out.
        assert!(matches!(
            latch.next_action(0),
            BackfillAction::Request { .. }
        ));
        // No reply at t=0 → delay becomes 30_000;
        // next_retry_at = 0 + 30_000 = 30_000.
        latch.on_no_reply(0);
        assert_eq!(latch.next_action(0), BackfillAction::WaitUntil(30_000));

        // t=30_000: eligible again.
        assert!(matches!(
            latch.next_action(30_000),
            BackfillAction::Request { .. }
        ));
        // No reply at t=30_000 → delay doubles to 60_000;
        // next_retry_at = 30_000 + 60_000 = 90_000.
        latch.on_no_reply(30_000);
        assert_eq!(latch.next_action(30_000), BackfillAction::WaitUntil(90_000));

        // t=90_000: request; no reply → delay 120_000;
        // next_retry_at = 90_000 + 120_000 = 210_000.
        assert!(matches!(
            latch.next_action(90_000),
            BackfillAction::Request { .. }
        ));
        latch.on_no_reply(90_000);
        assert_eq!(
            latch.next_action(90_000),
            BackfillAction::WaitUntil(210_000)
        );

        // t=210_000: request; no reply → delay 240_000;
        // next_retry_at = 210_000 + 240_000 = 450_000.
        assert!(matches!(
            latch.next_action(210_000),
            BackfillAction::Request { .. }
        ));
        latch.on_no_reply(210_000);
        assert_eq!(
            latch.next_action(210_000),
            BackfillAction::WaitUntil(450_000)
        );

        // t=450_000: request; no reply → delay 480_000;
        // next_retry_at = 450_000 + 480_000 = 930_000.
        assert!(matches!(
            latch.next_action(450_000),
            BackfillAction::Request { .. }
        ));
        latch.on_no_reply(450_000);
        assert_eq!(
            latch.next_action(450_000),
            BackfillAction::WaitUntil(930_000)
        );

        // t=930_000: request; no reply → doubling would give 960_000,
        // CAPPED at 600_000; next_retry_at = 930_000 + 600_000
        // = 1_530_000 — interval is exactly the cap.
        assert!(matches!(
            latch.next_action(930_000),
            BackfillAction::Request { .. }
        ));
        latch.on_no_reply(930_000);
        assert_eq!(
            latch.next_action(930_000),
            BackfillAction::WaitUntil(1_530_000)
        );

        // t=1_530_000: request; no reply → stays capped at 600_000;
        // next_retry_at = 1_530_000 + 600_000 = 2_130_000.
        assert!(matches!(
            latch.next_action(1_530_000),
            BackfillAction::Request { .. }
        ));
        latch.on_no_reply(1_530_000);
        assert_eq!(
            latch.next_action(1_530_000),
            BackfillAction::WaitUntil(2_130_000)
        );
    }

    #[test]
    fn reset_unsatisfies_with_new_watermark() {
        let mut latch = BackfillLatch::new(None);
        assert!(matches!(
            latch.next_action(0),
            BackfillAction::Request { .. }
        ));
        latch.on_page_complete(
            PageOutcome {
                events: 0,
                max_hlc_seen: None,
                limit: 1000,
            },
            0,
        );
        assert!(latch.is_satisfied());

        latch.reset(Some(hlc(900)));
        assert!(!latch.is_satisfied());
        assert_eq!(
            latch.next_action(0),
            BackfillAction::Request {
                since: Some(hlc(900))
            }
        );
    }

    // ── ZEB-418 P3a Task 4: async driver ─────────────────────────────

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex as StdMutex};
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn driver_retries_until_holder_appears_then_satisfies() {
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        // Calls 1-2: no reply (no holder online). Call 3: a holder
        // answers with a short page (3 < 256) → satisfied.
        let request_page = move |_since: Option<Hlc>| {
            let counter = Arc::clone(&counter);
            async move {
                let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                if n < 3 {
                    PageFetch::NoReply
                } else {
                    PageFetch::Completed(3, 256)
                }
            }
        };
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let start = tokio::time::Instant::now();
        run_backfill_driver(
            BackfillLatch::new(None),
            request_page,
            || async { None::<Hlc> },
            shutdown_rx,
            move || start.elapsed().as_millis() as u64,
        )
        .await;
        assert_eq!(
            requests.load(Ordering::SeqCst),
            3,
            "exactly three requests: two unanswered, one satisfied"
        );
        // Backoff schedule: no-reply at t=0 → retry at 30s; no-reply at
        // 30s → retry at 90s. Paused time must have advanced past both
        // marks for the driver future to have completed.
        assert!(
            start.elapsed() >= Duration::from_millis(90_000),
            "driver must wait out the 30s and 90s backoff marks, \
             elapsed {:?}",
            start.elapsed()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn driver_pages_through_full_pages_without_backoff() {
        let requests = Arc::new(AtomicUsize::new(0));
        let sinces: Arc<StdMutex<Vec<Option<Hlc>>>> = Arc::new(StdMutex::new(Vec::new()));
        let counter = Arc::clone(&requests);
        let since_log = Arc::clone(&sinces);
        // Two full pages (256/256) then a short page (5/256).
        let request_page = move |since: Option<Hlc>| {
            let counter = Arc::clone(&counter);
            let since_log = Arc::clone(&since_log);
            async move {
                since_log.lock().unwrap().push(since);
                let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                if n < 3 {
                    PageFetch::Completed(256, 256)
                } else {
                    PageFetch::Completed(5, 256)
                }
            }
        };
        // The log-side watermark advances on each read — the driver
        // must re-read it after every FULL page and pass it as the
        // next `since`.
        let watermark_reads = Arc::new(AtomicUsize::new(0));
        let wm_counter = Arc::clone(&watermark_reads);
        let current_watermark = move || {
            let n = wm_counter.fetch_add(1, Ordering::SeqCst) as u64;
            async move { Some(hlc(1_000 + n)) }
        };
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let start = tokio::time::Instant::now();
        run_backfill_driver(
            BackfillLatch::new(None),
            request_page,
            current_watermark,
            shutdown_rx,
            move || start.elapsed().as_millis() as u64,
        )
        .await;
        assert_eq!(requests.load(Ordering::SeqCst), 3);
        // Immediate paging: under paused time the future completes
        // without ever sleeping — any backoff between pages would
        // show up as auto-advanced elapsed time.
        assert_eq!(
            start.elapsed(),
            Duration::ZERO,
            "full-page paging must re-request immediately with no backoff"
        );
        assert_eq!(
            *sinces.lock().unwrap(),
            vec![None, Some(hlc(1_000)), Some(hlc(1_001))],
            "since must advance to the log watermark after each full page"
        );
        assert_eq!(
            watermark_reads.load(Ordering::SeqCst),
            2,
            "watermark is re-read once per FULL page only"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn driver_stops_on_shutdown_signal() {
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        let request_page = move |_since: Option<Hlc>| {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                PageFetch::NoReply
            }
        };
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let start = tokio::time::Instant::now();
        let driver = tokio::spawn(run_backfill_driver(
            BackfillLatch::new(None),
            request_page,
            || async { None::<Hlc> },
            shutdown_rx,
            move || start.elapsed().as_millis() as u64,
        ));
        // Let the driver issue request #1 and arm its 30s backoff
        // sleep. yield_now keeps the test task runnable so paused time
        // does NOT auto-advance here.
        while requests.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        shutdown_tx.send(true).expect("send shutdown");
        driver.await.expect("driver task");
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "no retry may fire after the shutdown signal"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn driver_aborted_query_backs_off() {
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        // Call 1: aborted query (oneshot RecvError maps to NoReply).
        // Call 2: clean empty page → satisfied.
        let request_page = move |_since: Option<Hlc>| {
            let counter = Arc::clone(&counter);
            async move {
                let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                if n == 1 {
                    PageFetch::NoReply
                } else {
                    PageFetch::Completed(0, 256)
                }
            }
        };
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let start = tokio::time::Instant::now();
        let driver = tokio::spawn(run_backfill_driver(
            BackfillLatch::new(None),
            request_page,
            || async { None::<Hlc> },
            shutdown_rx,
            move || start.elapsed().as_millis() as u64,
        ));
        while requests.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        // Just shy of the 30s backoff: the retry must NOT have fired.
        tokio::time::advance(Duration::from_millis(29_999)).await;
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "second request must wait out the full 30s backoff"
        );
        // Cross the 30s mark: retry fires, clean empty page satisfies.
        tokio::time::advance(Duration::from_millis(2)).await;
        driver.await.expect("driver task");
        assert_eq!(requests.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn driver_exits_on_engine_gone() {
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        // The request closure reports the engine/adapter permanently
        // gone (query bridge closed): the driver must return instead
        // of arming eternal futile retries.
        let request_page = move |_since: Option<Hlc>| {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                PageFetch::EngineGone
            }
        };
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let start = tokio::time::Instant::now();
        run_backfill_driver(
            BackfillLatch::new(None),
            request_page,
            || async { None::<Hlc> },
            shutdown_rx,
            move || start.elapsed().as_millis() as u64,
        )
        .await;
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "EngineGone must end the driver after exactly one request"
        );
        // Under paused time any backoff sleep would auto-advance the
        // clock — zero elapsed proves the driver exited immediately.
        assert_eq!(
            start.elapsed(),
            Duration::ZERO,
            "driver must exit without arming any backoff wait"
        );
    }

    #[test]
    fn in_flight_guard_blocks_second_request() {
        let mut latch = BackfillLatch::new(None);
        assert!(matches!(
            latch.next_action(0),
            BackfillAction::Request { .. }
        ));
        // Second poll while in-flight must NOT hand out another Request.
        let second = latch.next_action(0);
        assert!(
            matches!(second, BackfillAction::WaitUntil(_)),
            "expected WaitUntil while in-flight, got {second:?}"
        );
        // Completing a short page clears in-flight and satisfies.
        latch.on_page_complete(
            PageOutcome {
                events: 1,
                max_hlc_seen: Some(hlc(10)),
                limit: 1000,
            },
            0,
        );
        assert_eq!(latch.next_action(0), BackfillAction::Idle);
    }

    // ── ZEB-434: RootFetchLatch + root-fetch driver tests ────────────────

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

        // t=0: first request goes out.
        assert_eq!(latch.next_action(0), RootFetchAction::Request);
        // No reply at t=0 → delay becomes BACKFILL_RETRY_BASE_MS (30_000);
        // next_retry_at = 0 + 30_000 = 30_000.
        latch.on_no_reply(0);
        assert_eq!(
            latch.next_action(0),
            RootFetchAction::WaitUntil(BACKFILL_RETRY_BASE_MS)
        );

        // t=30_000: eligible again.
        assert_eq!(latch.next_action(30_000), RootFetchAction::Request);
        // No reply at t=30_000 → delay doubles to 60_000;
        // next_retry_at = 30_000 + 60_000 = 90_000.
        latch.on_no_reply(30_000);
        assert_eq!(
            latch.next_action(30_000),
            RootFetchAction::WaitUntil(90_000)
        );

        // t=90_000: request; no reply → delay 120_000;
        // next_retry_at = 90_000 + 120_000 = 210_000.
        assert_eq!(latch.next_action(90_000), RootFetchAction::Request);
        latch.on_no_reply(90_000);
        assert_eq!(
            latch.next_action(90_000),
            RootFetchAction::WaitUntil(210_000)
        );

        // t=210_000: request; no reply → delay 240_000;
        // next_retry_at = 210_000 + 240_000 = 450_000.
        assert_eq!(latch.next_action(210_000), RootFetchAction::Request);
        latch.on_no_reply(210_000);
        assert_eq!(
            latch.next_action(210_000),
            RootFetchAction::WaitUntil(450_000)
        );

        // t=450_000: request; no reply → delay 480_000;
        // next_retry_at = 450_000 + 480_000 = 930_000.
        assert_eq!(latch.next_action(450_000), RootFetchAction::Request);
        latch.on_no_reply(450_000);
        assert_eq!(
            latch.next_action(450_000),
            RootFetchAction::WaitUntil(930_000)
        );

        // t=930_000: request; no reply → doubling would give 960_000,
        // CAPPED at BACKFILL_RETRY_CAP_MS (600_000);
        // next_retry_at = 930_000 + 600_000 = 1_530_000 — interval is exactly the cap.
        assert_eq!(latch.next_action(930_000), RootFetchAction::Request);
        latch.on_no_reply(930_000);
        assert_eq!(
            latch.next_action(930_000),
            RootFetchAction::WaitUntil(1_530_000)
        );

        // t=1_530_000: request; no reply → stays capped at 600_000;
        // next_retry_at = 1_530_000 + 600_000 = 2_130_000.
        assert_eq!(latch.next_action(1_530_000), RootFetchAction::Request);
        latch.on_no_reply(1_530_000);
        assert_eq!(
            latch.next_action(1_530_000),
            RootFetchAction::WaitUntil(2_130_000)
        );
    }

    #[test]
    fn root_latch_in_flight_guard() {
        let mut latch = RootFetchLatch::new();
        assert_eq!(latch.next_action(0), RootFetchAction::Request);
        // Second poll while in-flight must NOT hand out another Request.
        let second = latch.next_action(0);
        assert!(
            matches!(second, RootFetchAction::WaitUntil(_)),
            "expected WaitUntil while in-flight, got {second:?}"
        );
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

    #[tokio::test(start_paused = true)]
    async fn root_driver_retries_until_answered_then_parks_when_epoch_some() {
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        let request_root = move || {
            let counter = Arc::clone(&counter);
            async move {
                let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                if n < 2 {
                    RootFetch::NoReply
                } else {
                    RootFetch::Answered
                }
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
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_millis(30_001)).await;
        while requests.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        // Satisfied + epoch_rx Some → the driver must PARK, not return.
        assert!(
            !driver.is_finished(),
            "driver must park on Idle when epoch watch present"
        );
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
        let request_root = move || async move { RootFetch::Answered };
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let start = tokio::time::Instant::now();
        run_root_fetch_driver(
            RootFetchLatch::new(),
            request_root,
            shutdown_rx,
            None,
            move || start.elapsed().as_millis() as u64,
        )
        .await;
        // If we get here without hanging, the driver returned on Idle.
    }

    #[tokio::test(start_paused = true)]
    async fn root_driver_stops_on_shutdown_while_parked() {
        let answered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let answered_clone = Arc::clone(&answered);
        let request_root = move || {
            let answered_clone = Arc::clone(&answered_clone);
            async move {
                answered_clone.store(true, Ordering::SeqCst);
                RootFetch::Answered
            }
        };
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
        // Wait deterministically until the request has been answered,
        // then let the driver advance from the completed request into
        // the parked select!.
        while !answered.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        shutdown_tx.send(true).expect("send shutdown");
        driver.await.expect("driver task");
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
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "EngineGone must end the driver after exactly one request"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn root_driver_epoch_bump_mid_backoff_requeries_after_cooldown() {
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
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
        // Wait for request #1 to fire.
        while requests.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        // Bump epoch mid-backoff (driver is sleeping through 30s backoff).
        epoch_tx.send(1).expect("epoch bump");
        // Advance past the cooldown (60s since request #1 at t=0).
        tokio::time::advance(Duration::from_millis(ROOT_REARM_COOLDOWN_MS + 1)).await;
        while requests.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
        assert_eq!(requests.load(Ordering::SeqCst), 2);
        driver.abort();
    }
}
