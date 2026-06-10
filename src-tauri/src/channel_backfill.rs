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
//!   its page cap and more history may remain: advance `since` to the max
//!   HLC seen and re-request **immediately** — no backoff between pages.
//!   This is the paging loop.
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
//! space is testable without a runtime. The async driver that owns the
//! transport lands in a later task and merely interprets the actions.
//! Precedent: `dm_outhold_apply`'s sweeper core.
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
}

impl BackfillLatch {
    /// New, unsatisfied latch requesting history after `watermark`.
    pub fn new(watermark: Option<Hlc>) -> Self {
        Self {
            since: watermark,
            satisfied: false,
            in_flight: false,
            next_retry_at: 0,
            retry_delay_ms: 0,
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

    /// Record a completed reply page (clears in-flight, resets backoff).
    ///
    /// Full page (`events >= limit`, `limit > 0`) advances `since` to the
    /// max HLC seen and leaves the latch unsatisfied so the next
    /// `next_action(now)` re-requests immediately. Short or empty page
    /// satisfies the latch (spec D24: a served "nothing" is an answer).
    pub fn on_page_complete(&mut self, outcome: PageOutcome, now_ms: u64) {
        self.in_flight = false;
        // Any answer resets the no-reply backoff and makes the next
        // request immediately eligible.
        self.retry_delay_ms = 0;
        self.next_retry_at = now_ms;

        let full_page = outcome.limit > 0 && outcome.events >= outcome.limit;
        if full_page {
            // Paging loop: more history may remain behind the cap.
            if let Some(max_hlc) = outcome.max_hlc_seen {
                self.since = Some(max_hlc);
            }
        } else {
            self.satisfied = true;
        }
    }

    /// Record an unanswered request (clears in-flight, arms backoff).
    ///
    /// Delay schedule: 30 s, doubling per consecutive no-reply, capped
    /// at 600 s; retries forever (driver enforces shutdown).
    pub fn on_no_reply(&mut self, now_ms: u64) {
        self.in_flight = false;
        self.retry_delay_ms = if self.retry_delay_ms == 0 {
            BACKFILL_RETRY_BASE_MS
        } else {
            (self.retry_delay_ms * 2).min(BACKFILL_RETRY_CAP_MS)
        };
        self.next_retry_at = now_ms + self.retry_delay_ms;
    }

    /// Re-arm a satisfied latch with a new watermark (transport-recovery
    /// hook); clears in-flight and backoff state.
    pub fn reset(&mut self, watermark: Option<Hlc>) {
        *self = Self::new(watermark);
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
}
