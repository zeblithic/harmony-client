// ZEB-355: shared voice media-subscriber reconnect machinery.
//
// The channel-media and DM-media subscribers in event_loop.rs carried two
// byte-identical copies of the reconnect skeleton refined across the ZEB-353
// V5 review rounds (made_progress backoff reset, emit-lost-before-sleep,
// immediate-first re-declare). Any future fix had to land in both places —
// and the V5 rounds showed how easy it is to touch one and miss the other.
// This module owns the skeleton once.

use std::time::Duration;

/// Initial (and post-progress reset) re-declare delay.
const BACKOFF_FLOOR: Duration = Duration::from_secs(5);
/// Growth ceiling for a flapping subscription.
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Progress-aware exponential backoff for a re-declared subscriber.
///
/// Only a connection that made real progress (carried at least one sample) may
/// reset the delay; one that declares OK but never receives is flapping and
/// must rate-limit, so the lost/restored event rate stays bounded instead of
/// spinning. Both the drop path and the re-declare-failure path advance the
/// same counter, exactly like the single `backoff` variable each inline copy
/// used.
pub(crate) struct ProgressBackoff {
    current: Duration,
}

impl ProgressBackoff {
    pub(crate) fn new() -> Self {
        Self {
            current: BACKOFF_FLOOR,
        }
    }

    /// The inner receive loop ended. With progress: reset to the floor and
    /// re-declare immediately (`None` — a healthy blip). Without: the caller
    /// sleeps the returned delay before re-declaring, and the delay doubles
    /// toward the cap.
    pub(crate) fn on_drop(&mut self, made_progress: bool) -> Option<Duration> {
        if made_progress {
            self.current = BACKOFF_FLOOR;
            None
        } else {
            Some(self.grow())
        }
    }

    /// A re-declare attempt failed: sleep the returned delay, doubling toward
    /// the cap.
    pub(crate) fn on_redeclare_failure(&mut self) -> Duration {
        self.grow()
    }

    fn grow(&mut self) -> Duration {
        let d = self.current;
        self.current = std::cmp::min(self.current * 2, MAX_BACKOFF);
        d
    }
}

pub(crate) type MediaSubscriber =
    zenoh::pubsub::Subscriber<zenoh::handlers::FifoChannelHandler<zenoh::sample::Sample>>;

/// Plumbing for one resilient media subscription: the session to re-declare
/// against, the key to re-declare, a log label, and the shutdown flag.
pub(crate) struct MediaSubscriberCtx {
    pub(crate) session: std::sync::Arc<zenoh::Session>,
    pub(crate) sub_key: String,
    pub(crate) label: &'static str,
    pub(crate) closing: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// Shared reconnect skeleton for a voice media subscriber (ZEB-353 semantics,
/// extracted in ZEB-355). Runs until `closing` is observed.
///
/// - Each received sample is fed to `on_sample` and marks the connection as
///   having made progress (see [`ProgressBackoff`]).
/// - When the inner receive loop ends on a transport error, `on_lost` fires
///   BEFORE any backoff sleep, so a flapping (no-progress) reconnect shows
///   "reconnecting" in the UI for the whole backoff window rather than only
///   flipping right before "...restored". The sleep still rate-limits the
///   re-declare; it just doesn't gate the UI signal.
/// - The re-declare loop tries immediately on its first attempt and only backs
///   off *after* a failed re-declare, so a momentary blip recovers without
///   waiting out the backoff. `on_restored` fires on each successful
///   re-declare; the backoff reset is owned by the progress logic, not by mere
///   re-subscription.
pub(crate) async fn run_media_subscriber<F>(
    ctx: MediaSubscriberCtx,
    initial: MediaSubscriber,
    mut on_sample: F,
    on_lost: impl Fn(),
    on_restored: impl Fn(),
) where
    F: AsyncFnMut(zenoh::sample::Sample),
{
    let MediaSubscriberCtx {
        session,
        sub_key,
        label,
        closing,
    } = ctx;
    let mut sub = initial;
    let mut backoff = ProgressBackoff::new();
    loop {
        let mut made_progress = false;
        while let Ok(sample) = sub.recv_async().await {
            made_progress = true;
            on_sample(sample).await;
        }
        // Inner receive loop ended on a transport error. Stop if we're
        // shutting down.
        if closing.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
        tracing::warn!(
            key = %sub_key,
            "{label} subscriber closed unexpectedly; reconnecting"
        );
        on_lost();
        if let Some(delay) = backoff.on_drop(made_progress) {
            tokio::time::sleep(delay).await;
        }
        loop {
            if closing.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            match session.declare_subscriber(&sub_key).await {
                Ok(s) => {
                    sub = s;
                    on_restored();
                    break;
                }
                Err(e) => {
                    let delay = backoff.on_redeclare_failure();
                    tracing::warn!(
                        key = %sub_key,
                        error = %e,
                        backoff_s = delay.as_secs(),
                        "{label} subscriber re-declare failed; retrying"
                    );
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn drop_with_progress_resets_to_floor_without_sleeping() {
        let mut b = ProgressBackoff::new();
        // Grow it first so the reset is observable.
        assert_eq!(b.on_redeclare_failure(), Duration::from_secs(5));
        assert_eq!(b.on_redeclare_failure(), Duration::from_secs(10));
        // A drop after real progress: no sleep, back to the 5s floor.
        assert_eq!(b.on_drop(true), None);
        assert_eq!(b.on_redeclare_failure(), Duration::from_secs(5));
    }

    #[test]
    fn drop_without_progress_sleeps_current_then_doubles_to_cap() {
        let mut b = ProgressBackoff::new();
        assert_eq!(b.on_drop(false), Some(Duration::from_secs(5)));
        assert_eq!(b.on_drop(false), Some(Duration::from_secs(10)));
        assert_eq!(b.on_drop(false), Some(Duration::from_secs(20)));
        assert_eq!(b.on_drop(false), Some(Duration::from_secs(40)));
        assert_eq!(b.on_drop(false), Some(Duration::from_secs(60)));
        // Capped: stays at MAX_BACKOFF forever after.
        assert_eq!(b.on_drop(false), Some(Duration::from_secs(60)));
    }

    #[test]
    fn redeclare_failure_growth_matches_drop_growth_and_shares_state() {
        let mut b = ProgressBackoff::new();
        // Mixed sequence: the drop path and the re-declare path advance the
        // SAME counter (the original inline code used one `backoff` variable).
        assert_eq!(b.on_redeclare_failure(), Duration::from_secs(5));
        assert_eq!(b.on_drop(false), Some(Duration::from_secs(10)));
        assert_eq!(b.on_redeclare_failure(), Duration::from_secs(20));
        assert_eq!(b.on_redeclare_failure(), Duration::from_secs(40));
        assert_eq!(b.on_redeclare_failure(), Duration::from_secs(60));
        assert_eq!(b.on_redeclare_failure(), Duration::from_secs(60));
    }

    #[test]
    fn progress_reset_applies_even_from_cap() {
        let mut b = ProgressBackoff::new();
        for _ in 0..6 {
            b.on_redeclare_failure();
        }
        assert_eq!(b.on_redeclare_failure(), Duration::from_secs(60));
        assert_eq!(b.on_drop(true), None);
        assert_eq!(b.on_drop(false), Some(Duration::from_secs(5)));
    }
}
