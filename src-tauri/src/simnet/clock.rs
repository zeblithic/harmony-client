//! Virtual wall clock for SimNet — reads tokio's (paused) clock so a single
//! `tokio::time::advance` moves it in lockstep with the scheduler.

use std::sync::Arc;

/// An injectable `now_ms` closure — matches `ReachabilityResolver::set_clock`.
pub(crate) type NowFn = Arc<dyn Fn() -> u64 + Send + Sync>;

/// A virtual wall clock that reads tokio's (paused) clock. Under
/// `#[tokio::test(start_paused = true)]` a `tokio::time::advance(d)` moves both
/// the scheduler and this clock by `d`, so seeded record timestamps (and, in
/// PR2, HLC stamps) stay coherent with the simulated schedule.
pub(crate) struct SimClock {
    base_ms: u64,
    origin: tokio::time::Instant,
}

impl SimClock {
    /// Base wall-ms is a fixed present-day constant so seeded record timestamps
    /// and any HLC stamps (PR2) look like real epoch-ms, never near-zero.
    pub(crate) fn new() -> Self {
        Self {
            base_ms: 1_700_000_000_000,
            origin: tokio::time::Instant::now(),
        }
    }

    pub(crate) fn now_ms(&self) -> u64 {
        self.base_ms + self.origin.elapsed().as_millis() as u64
    }

    /// A `now_ms` closure sharing this clock's virtual time — injected into each
    /// node's `ReachabilityResolver` so record freshness / stale-refresh /
    /// future-skew all evaluate in sim-time, not host time.
    pub(crate) fn as_now_fn(&self) -> NowFn {
        let base = self.base_ms;
        let origin = self.origin;
        Arc::new(move || base + origin.elapsed().as_millis() as u64)
    }
}

#[cfg(test)]
mod clock_tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn now_ms_tracks_virtual_time() {
        let clock = SimClock::new();
        let t0 = clock.now_ms();
        tokio::time::advance(Duration::from_millis(5_000)).await;
        assert_eq!(
            clock.now_ms(),
            t0 + 5_000,
            "now_ms must track tokio virtual time"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn as_now_fn_matches_now_ms() {
        let clock = SimClock::new();
        let f = clock.as_now_fn();
        tokio::time::advance(Duration::from_millis(1_234)).await;
        assert_eq!(f(), clock.now_ms());
    }
}
