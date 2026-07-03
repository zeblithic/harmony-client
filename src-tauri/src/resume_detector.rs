//! Sleep/wake resume detector (ZEB-621, Task 6).
//!
//! There is no OS-level sleep/wake notification in this stack, so a laptop that
//! suspends for hours would otherwise wait up to the reachability publisher's
//! ~60min idle backstop before it re-probes its network path and republishes
//! its pkarr records. During that window the node advertises stale addresses
//! and is effectively unreachable to peers that resolve it fresh.
//!
//! This module provides a small, portable, dependency-free detector: a loop
//! that samples a monotonic-ish wall clock once per `tick`. Because a single
//! [`tokio::time::sleep`] of `tick` should advance the wall clock by roughly
//! `tick`, an observed wall-clock delta far larger than one tick means the
//! process was frozen (suspended) — or the clock was stepped. Either way the
//! safe, idempotent response is to re-probe the network path and republish.
//!
//! The loop is deliberately clock-injectable ([`run_resume_detector`] takes a
//! `now_ms` closure) so the paused-time test can drive a wall clock that jumps
//! independently of tokio's virtual time. Production wires a `SystemTime`-backed
//! `now_ms` in `lib.rs` next to the publisher spawn.

use std::sync::Arc;
use std::time::Duration;

/// Production sampling interval for the resume-detector loop.
///
/// One missed tick plus a 5s margin is the detection threshold (see
/// [`resume_gap_detected`]); at 30s that means a jump of >65s trips the
/// detector, which no ordinary scheduling jitter can produce.
pub const RESUME_DETECTOR_TICK: Duration = Duration::from_secs(30);

/// Returns `true` when the wall-clock delta observed across one loop tick is
/// large enough to conclude the process was suspended (or the clock stepped).
///
/// Rule: `observed_wall_delta_ms > expected_tick*2 + 5_000ms`. Allowing two
/// ticks plus a 5s margin absorbs ordinary timer jitter and one fully missed
/// tick without false-firing, while any real multi-minute suspend clears it by
/// orders of magnitude.
pub fn resume_gap_detected(expected_tick: Duration, observed_wall_delta_ms: u64) -> bool {
    observed_wall_delta_ms > (expected_tick.as_millis() as u64) * 2 + 5_000
}

/// Runs the resume-detector loop forever. Intended to be `tokio::spawn`ed.
///
/// `now_ms` yields the current wall-clock time in milliseconds; the loop uses it
/// exclusively (never reads the system clock directly) so tests can inject a
/// jumpable clock. On each tick the loop sleeps `tick` of *virtual/real* time,
/// then compares the wall-clock delta against [`resume_gap_detected`]; on a
/// detected gap it invokes `on_resume`.
pub async fn run_resume_detector(
    now_ms: Arc<dyn Fn() -> u64 + Send + Sync>,
    tick: Duration,
    on_resume: Arc<dyn Fn() + Send + Sync>,
) {
    let mut prev = now_ms();
    loop {
        tokio::time::sleep(tick).await;
        let cur = now_ms();
        let delta = cur.saturating_sub(prev);
        if resume_gap_detected(tick, delta) {
            tracing::info!(
                observed_wall_delta_ms = delta,
                tick_ms = tick.as_millis() as u64,
                "resume detector: wall-clock jump — firing network_change re-probe + immediate republish"
            );
            on_resume();
        }
        prev = cur;
    }
}

#[cfg(test)]
mod tests {
    use super::{resume_gap_detected, run_resume_detector};
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn no_gap_below_threshold() {
        assert!(!resume_gap_detected(Duration::from_secs(30), 30_000));
        assert!(!resume_gap_detected(Duration::from_secs(30), 64_999)); // 2*30s+5s boundary
    }

    #[test]
    fn gap_above_threshold_detected() {
        assert!(resume_gap_detected(Duration::from_secs(30), 65_001));
        assert!(resume_gap_detected(
            Duration::from_secs(30),
            3 * 60 * 60 * 1000
        )); // 3h suspend
    }

    /// Paused-time loop test with an injected, jumpable clock.
    #[tokio::test(start_paused = true)]
    async fn loop_fires_on_resume_and_not_on_normal_ticks() {
        let wall = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let w2 = Arc::clone(&wall);
        let now_ms: Arc<dyn Fn() -> u64 + Send + Sync> =
            Arc::new(move || w2.load(std::sync::atomic::Ordering::SeqCst));
        let fired = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let f2 = Arc::clone(&fired);
        let on_resume: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            f2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
        let tick = Duration::from_secs(30);
        tokio::spawn(run_resume_detector(now_ms, tick, on_resume));

        // Normal tick: wall advances in lockstep with virtual time → no fire.
        wall.fetch_add(30_000, std::sync::atomic::Ordering::SeqCst);
        tokio::time::advance(tick).await;
        tokio::task::yield_now().await;
        assert_eq!(fired.load(std::sync::atomic::Ordering::SeqCst), 0);

        // Suspend: virtual sleep elapses once, but the wall clock jumped 2h.
        wall.fetch_add(2 * 60 * 60 * 1000, std::sync::atomic::Ordering::SeqCst);
        tokio::time::advance(tick).await;
        tokio::task::yield_now().await;
        assert_eq!(fired.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
