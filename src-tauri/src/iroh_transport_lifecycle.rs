//! ZEB-834: iroh transport lifecycle helpers — graceful endpoint teardown on
//! node stop, plus relay-health observability on node start.
//!
//! # Root cause (ZEB-834)
//!
//! `stop_node` used to release the running `IrohEndpoint` by dropping its `Arc`
//! (`self.iroh_endpoint = None`) without ever calling `Endpoint::close()`.
//! `iroh::Endpoint` is an `Arc`-style handle whose relay/QUIC actor is reaped
//! **only** by an explicit graceful `close()` — a bare `Arc`-drop does not tear
//! it down. So the stale relay connection leaked onto the persistent main
//! runtime, and because `start_node` re-binds the **same persisted secret key**
//! (hence the same `EndpointId`), the freshly-bound endpoint collided with the
//! leaked ghost ("Another endpoint connected with same endpoint id"), producing
//! home-relay flapping + `MultipathNotNegotiated` dial failures that only a full
//! process restart cleared.
//!
//! This module provides the two halves of the fix:
//!
//! * [`close_iroh_endpoint`] — drive `IrohEndpoint::shutdown()` (idempotent
//!   `Endpoint::close`) bounded by a timeout, so `stop_node` reaps the relay
//!   actor in-process. Called from `stop_inner` on an ephemeral runtime.
//! * [`spawn_relay_health_observer`] — a bounded, **log-only** task that samples
//!   the endpoint's `home_relay_status()` over a settling window after start and
//!   emits an actionable `WARN` if the home relay never connects, flaps, or
//!   errors persistently. Turns silent relay degradation into an observable
//!   signal. No behavior change, no auto-retry.
//!
//! # Testability seam
//!
//! `iroh::endpoint::RelayStatus::new` is `pub(crate)` — not constructible
//! outside iroh — so the observer projects each `RelayStatus` snapshot into an
//! owned [`RelaySample`] at the iroh boundary. All the stability *judgment*
//! then lives in the pure [`evaluate_relay_stability`], which unit tests can
//! exercise exhaustively without a live endpoint or relay.

use std::sync::Arc;
use std::time::Duration;

use crate::iroh_endpoint::IrohEndpoint;

/// Timeout bounding the endpoint close on node stop. `close()` is normally
/// near-instant (it CONNECTION_CLOSEs open streams and stops the actor); the
/// bound only guards against a wedged close hanging `stop_node`.
pub(crate) const ENDPOINT_CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

/// Interval between relay-status samples in the start-time settling window.
pub(crate) const RELAY_OBSERVE_INTERVAL: Duration = Duration::from_secs(5);
/// Number of samples taken across the settling window (6 × 5s ≈ 30s).
pub(crate) const RELAY_OBSERVE_SAMPLE_COUNT: usize = 6;
/// Minimum samples required to render a verdict; below this it is `Inconclusive`
/// (e.g. the observer was aborted early by a stop).
pub(crate) const RELAY_MIN_SAMPLES: usize = 3;
/// Adjacent-snapshot signature changes (connect/disconnect or home-relay URL
/// churn) at or above this count over the window are judged `Flapping`.
pub(crate) const RELAY_FLAP_THRESHOLD: usize = 4;

/// Outcome of a bounded endpoint close. Returned (rather than `()`) so the sync
/// call site can log which path it took, and so [`close_iroh_endpoint`] is
/// observable at the `IrohEndpoint::from_endpoint_for_test` seam in unit tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CloseOutcome {
    /// `shutdown()` completed within the timeout.
    Closed,
    /// The timeout elapsed before `shutdown()` returned. Teardown proceeds; the
    /// relay actor may linger until process exit (strictly better than the
    /// pre-ZEB-834 behavior, which never closed at all).
    TimedOut,
}

/// Drive `endpoint.shutdown()` (idempotent `iroh::Endpoint::close`) bounded by
/// `timeout`.
///
/// `shutdown()` is idempotent, so calling this on an already-closed endpoint is
/// safe and returns [`CloseOutcome::Closed`] immediately. Takes the `Arc` by
/// value: closing works regardless of how many other clones remain live
/// (`close()` shuts down the shared underlying actor), and consuming the caller's
/// snapshot documents that this is the teardown of that handle.
pub(crate) async fn close_iroh_endpoint(
    endpoint: Arc<IrohEndpoint>,
    timeout: Duration,
) -> CloseOutcome {
    match tokio::time::timeout(timeout, endpoint.shutdown()).await {
        Ok(()) => CloseOutcome::Closed,
        Err(_elapsed) => {
            tracing::warn!(
                timeout_ms = timeout.as_millis() as u64,
                "ZEB-834: iroh endpoint close() did not complete within the timeout \
                 on node stop; proceeding with teardown (the relay actor may linger \
                 until process exit)"
            );
            CloseOutcome::TimedOut
        }
    }
}

/// An owned projection of `iroh::endpoint::RelayStatus`, captured at the iroh
/// boundary so the stability judgment in [`evaluate_relay_stability`] stays pure
/// and unit-testable (`RelayStatus` itself is not constructible outside iroh).
#[derive(Debug, Clone, PartialEq, Eq)]
struct RelaySample {
    /// The relay URL this status refers to.
    url: String,
    /// Whether the endpoint is currently connected to this relay.
    connected: bool,
    /// Whether this status carried a recent connection error.
    had_error: bool,
}

/// The verdict rendered over a settling window of relay-status samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RelayStabilityVerdict {
    /// A home relay connected and the connection did not churn — nominal.
    Healthy,
    /// No snapshot in the window ever showed a connected home relay, and no
    /// connection error was reported — the relay simply never came up.
    NeverConnected,
    /// The connected home-relay signature (URL, or connected-vs-not) changed
    /// repeatedly across the window — the classic same-`EndpointId` collision
    /// symptom.
    Flapping,
    /// No snapshot ever showed a connected relay, but a connection error was
    /// reported — the relay is actively failing to connect.
    PersistentError,
    /// Too few samples to render a verdict (e.g. the observer was aborted early
    /// by a node stop).
    Inconclusive,
}

/// Pure stability judgment over an ordered sequence of relay-status snapshots.
///
/// Each element of `samples` is one snapshot of the endpoint's home-relay
/// status set (usually zero or one entries). The verdict is derived from a
/// per-snapshot *signature* — the URL of the first connected relay in that
/// snapshot, or `None` if none is connected — which captures both
/// connect/disconnect churn (`Some`↔`None`) and home-relay URL churn
/// (`Some(A)`↔`Some(B)`) in a single metric.
fn evaluate_relay_stability(
    samples: &[Vec<RelaySample>],
    min_samples: usize,
    flap_threshold: usize,
) -> RelayStabilityVerdict {
    if samples.len() < min_samples {
        return RelayStabilityVerdict::Inconclusive;
    }

    let signatures: Vec<Option<&str>> = samples
        .iter()
        .map(|snap| snap.iter().find(|s| s.connected).map(|s| s.url.as_str()))
        .collect();

    // Count adjacent signature changes. Flapping is judged first: a churning
    // relay is the reported degradation even if some snapshots show it briefly
    // connected.
    let toggles = signatures.windows(2).filter(|w| w[0] != w[1]).count();
    if toggles >= flap_threshold {
        return RelayStabilityVerdict::Flapping;
    }

    if signatures.iter().any(|s| s.is_some()) {
        return RelayStabilityVerdict::Healthy;
    }

    // Never connected across the whole window. Distinguish a relay that is
    // actively erroring from one that simply hasn't come up.
    if samples.iter().any(|snap| snap.iter().any(|s| s.had_error)) {
        RelayStabilityVerdict::PersistentError
    } else {
        RelayStabilityVerdict::NeverConnected
    }
}

/// Configuration for the start-time relay-health observer. All fields are
/// `Copy`, so the whole config moves cheaply into the observer task.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RelayObserveConfig {
    /// Delay between successive relay-status samples.
    pub interval: Duration,
    /// How many samples to take before rendering a verdict.
    pub sample_count: usize,
    /// Minimum samples for a non-`Inconclusive` verdict.
    pub min_samples: usize,
    /// Signature-change count over the window that counts as flapping.
    pub flap_threshold: usize,
}

impl Default for RelayObserveConfig {
    fn default() -> Self {
        Self {
            interval: RELAY_OBSERVE_INTERVAL,
            sample_count: RELAY_OBSERVE_SAMPLE_COUNT,
            min_samples: RELAY_MIN_SAMPLES,
            flap_threshold: RELAY_FLAP_THRESHOLD,
        }
    }
}

impl RelayObserveConfig {
    /// Approximate wall-clock length of the settling window, for log messages.
    fn window_secs(&self) -> u64 {
        self.interval
            .as_secs()
            .saturating_mul(self.sample_count as u64)
    }
}

/// Sample the endpoint's `home_relay_status()` over the settling window and
/// render a stability verdict. Kept separate from [`spawn_relay_health_observer`]
/// so the sampling + projection + judgment pipeline is unit-testable against a
/// hermetic endpoint without spawning a task.
async fn observe_relay_stability(
    endpoint: &IrohEndpoint,
    config: RelayObserveConfig,
) -> RelayStabilityVerdict {
    // `home_relay_status()` returns an owned (`'static`) watcher; `get()` is a
    // trait method (`iroh::Watcher`) taking `&mut self`.
    use iroh::Watcher as _;
    let mut watcher = endpoint.inner().home_relay_status();
    let mut samples: Vec<Vec<RelaySample>> = Vec::with_capacity(config.sample_count);
    for _ in 0..config.sample_count {
        tokio::time::sleep(config.interval).await;
        let projected = watcher
            .get()
            .iter()
            .map(|s| RelaySample {
                url: s.url().to_string(),
                connected: s.is_connected(),
                had_error: s.last_error().is_some(),
            })
            .collect();
        samples.push(projected);
    }
    evaluate_relay_stability(&samples, config.min_samples, config.flap_threshold)
}

/// Spawn the bounded, log-only relay-health observer on the current runtime.
///
/// The task samples the endpoint over the settling window, renders a verdict,
/// logs it (`WARN` for any degraded verdict, `DEBUG` otherwise), then returns —
/// it is bounded, so even if its `JoinHandle` is never aborted it self-
/// terminates. The caller **must** still store the returned handle and abort it
/// on node stop (`clear_iroh_handles`), so it can't pin the old endpoint `Arc`
/// across a restart (the leaked-task class this whole ticket is about).
pub(crate) fn spawn_relay_health_observer(
    endpoint: Arc<IrohEndpoint>,
    config: RelayObserveConfig,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let verdict = observe_relay_stability(&endpoint, config).await;
        match verdict {
            RelayStabilityVerdict::Healthy | RelayStabilityVerdict::Inconclusive => {
                tracing::debug!(?verdict, "ZEB-834: relay-health observer settled");
            }
            RelayStabilityVerdict::NeverConnected => tracing::warn!(
                window_secs = config.window_secs(),
                "ZEB-834: home relay never connected within the settling window after \
                 node start — outbound reachability may be degraded. If this followed \
                 an in-process restart, suspect a leaked prior endpoint (same \
                 EndpointId collision)."
            ),
            RelayStabilityVerdict::Flapping => tracing::warn!(
                window_secs = config.window_secs(),
                "ZEB-834: home relay is flapping (repeated connect/disconnect or URL \
                 churn) within the settling window after node start — the classic \
                 same-EndpointId collision symptom; transport may degrade until a \
                 process restart."
            ),
            RelayStabilityVerdict::PersistentError => tracing::warn!(
                window_secs = config.window_secs(),
                "ZEB-834: home relay reported a persistent connection error and never \
                 connected within the settling window after node start — outbound \
                 reachability is degraded."
            ),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(url: &str, connected: bool, had_error: bool) -> RelaySample {
        RelaySample {
            url: url.to_string(),
            connected,
            had_error,
        }
    }

    /// Build a fully hermetic, relay-disabled endpoint (mirrors the
    /// `iroh_endpoint_inits_with_ephemeral_secret` test): no real DNS, no relay
    /// dialing, so it binds fast and deterministically in CI.
    async fn hermetic_disabled_endpoint() -> Arc<IrohEndpoint> {
        use iroh::endpoint::{presets, Endpoint, RelayMode};
        use iroh::SecretKey;
        let inner = Endpoint::builder(presets::N0)
            .secret_key(SecretKey::generate())
            .alpns(crate::iroh_endpoint::all_client_alpns())
            .relay_mode(RelayMode::Disabled)
            .dns_resolver(crate::iroh_endpoint::hermetic_dns_resolver())
            .bind()
            .await
            .expect("bind hermetic endpoint");
        Arc::new(IrohEndpoint::from_endpoint_for_test(inner))
    }

    // ── evaluate_relay_stability ────────────────────────────────────────────

    #[test]
    fn too_few_samples_is_inconclusive() {
        let samples = vec![vec![sample("https://r1/", true, false)]];
        assert_eq!(
            evaluate_relay_stability(&samples, 3, 4),
            RelayStabilityVerdict::Inconclusive
        );
    }

    #[test]
    fn stable_connected_is_healthy() {
        let samples = vec![
            vec![sample("https://r1/", true, false)],
            vec![sample("https://r1/", true, false)],
            vec![sample("https://r1/", true, false)],
        ];
        assert_eq!(
            evaluate_relay_stability(&samples, 3, 4),
            RelayStabilityVerdict::Healthy
        );
    }

    #[test]
    fn a_single_late_disconnect_is_not_flapping() {
        // One signature change (connected → none) is below the flap threshold.
        let samples = vec![
            vec![sample("https://r1/", true, false)],
            vec![sample("https://r1/", true, false)],
            vec![sample("https://r1/", false, false)],
        ];
        assert_eq!(
            evaluate_relay_stability(&samples, 3, 4),
            RelayStabilityVerdict::Healthy
        );
    }

    #[test]
    fn never_connected_no_error_is_never_connected() {
        let samples = vec![vec![], vec![], vec![]];
        assert_eq!(
            evaluate_relay_stability(&samples, 3, 4),
            RelayStabilityVerdict::NeverConnected
        );
    }

    #[test]
    fn never_connected_with_error_is_persistent_error() {
        let samples = vec![
            vec![sample("https://r1/", false, true)],
            vec![sample("https://r1/", false, true)],
            vec![sample("https://r1/", false, true)],
        ];
        assert_eq!(
            evaluate_relay_stability(&samples, 3, 4),
            RelayStabilityVerdict::PersistentError
        );
    }

    #[test]
    fn connect_disconnect_churn_is_flapping() {
        // 5 adjacent signature changes over 6 samples >= threshold 4.
        let samples = vec![
            vec![sample("https://r1/", true, false)],
            vec![],
            vec![sample("https://r1/", true, false)],
            vec![],
            vec![sample("https://r1/", true, false)],
            vec![],
        ];
        assert_eq!(
            evaluate_relay_stability(&samples, 3, 4),
            RelayStabilityVerdict::Flapping
        );
    }

    #[test]
    fn url_churn_among_connected_is_flapping() {
        // Home-relay URL oscillates A/B/A/B/A — 4 changes >= threshold 4 — even
        // though a relay is "connected" in every snapshot.
        let samples = vec![
            vec![sample("https://a/", true, false)],
            vec![sample("https://b/", true, false)],
            vec![sample("https://a/", true, false)],
            vec![sample("https://b/", true, false)],
            vec![sample("https://a/", true, false)],
        ];
        assert_eq!(
            evaluate_relay_stability(&samples, 3, 4),
            RelayStabilityVerdict::Flapping
        );
    }

    // ── close_iroh_endpoint ─────────────────────────────────────────────────

    #[tokio::test]
    async fn close_endpoint_closes_and_is_idempotent() {
        let ep = hermetic_disabled_endpoint().await;
        assert_eq!(
            close_iroh_endpoint(Arc::clone(&ep), Duration::from_secs(5)).await,
            CloseOutcome::Closed
        );
        // `Endpoint::close` is idempotent — a second close on an already-closed
        // endpoint still returns promptly.
        assert_eq!(
            close_iroh_endpoint(ep, Duration::from_secs(5)).await,
            CloseOutcome::Closed
        );
    }

    // ── observer pipeline (sampling + projection + judgment) ─────────────────

    #[tokio::test]
    async fn observe_relay_disabled_endpoint_is_never_connected() {
        let ep = hermetic_disabled_endpoint().await;
        let config = RelayObserveConfig {
            interval: Duration::from_millis(1),
            sample_count: 3,
            min_samples: 3,
            flap_threshold: 4,
        };
        // Relay disabled → every snapshot is empty → never connected, no error.
        assert_eq!(
            observe_relay_stability(&ep, config).await,
            RelayStabilityVerdict::NeverConnected
        );
    }

    #[tokio::test]
    async fn spawn_observer_completes_without_panic() {
        let ep = hermetic_disabled_endpoint().await;
        let config = RelayObserveConfig {
            interval: Duration::from_millis(1),
            sample_count: 3,
            min_samples: 3,
            flap_threshold: 4,
        };
        // The spawned, bounded observer runs its window and returns cleanly
        // (exercising the projection + every non-panicking log arm).
        spawn_relay_health_observer(ep, config)
            .await
            .expect("observer task joins cleanly");
    }
}
