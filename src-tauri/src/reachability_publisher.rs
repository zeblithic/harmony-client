//! ZEB-321 Phase 1 Task 7: background task that re-emits this device's
//! `ReachabilityAnnounce` on a small set of triggers.
//!
//! See `docs/specs/2026-05-22-zeb-321-cross-wan-connectivity-design.md` §5.6
//! and `docs/plans/2026-05-22-zeb-321-phase1-iroh-foundation-plan.md` Task 7.
//!
//! ## Triggers implemented
//!
//! 1. **Startup** — publish immediately, no debounce.
//! 2. **Network change** — a single merged stream of two sources, both
//!    coalesced through one 2-second debounce window. Because they feed one
//!    debounce-drain, an if-watch tick and a `watch_addr` tick milliseconds
//!    apart collapse into a single publish by construction (no
//!    double-publish). The two sources:
//!    - `if-watch` IfEvent (Up/Down on a local `IpNet`), so a DHCP rebind
//!      that emits Down → Up in milliseconds collapses into one publish.
//!    - iroh `Endpoint::watch_addr` updates (home-relay flap, direct-address
//!      churn) — added in ZEB-621 so a relay change republishes within
//!      seconds instead of waiting on the idle backstop below.
//! 3. **Idle backstop** — every 60 minutes, to refresh against the 24h TTL
//!    on `ReachabilityRecord` even when nothing has changed. ZEB-621
//!    demoted this from the primary relay-change safety net (see trigger 2)
//!    to a pure long-interval backstop.
//! 4. **Force-notify** — `tokio::sync::Notify` wake from
//!    `connectivity_force_republish` IPC (Task 9) or test code.
//!
//! Home-relay-change (spec §5.6 bullet 4) IS handled as of ZEB-621, via the
//! iroh `watch_addr` arm of trigger 2 above — a boxed `stream_updates_only`
//! watcher merged into the same debounce window as the interface watcher.
//! The 60-minute idle tick (trigger 3) no longer carries the relay-flap
//! correctness case; it is now only the TTL-refresh backstop.
//!
//! ## Decoupling
//!
//! The publisher is given a [`PublishFn`] callback that signs and
//! inserts the record into each community's CRDT. This module knows
//! nothing about community-state internals — `event_loop.rs` (Task 8)
//! wires up the actual signing path.
//!
//! ## `tokio::select!` semantics
//!
//! Each branch's future is polled until it resolves OR another branch
//! wins; cancellation is cooperative. `biased;` makes the `force`
//! branch the first one polled each iteration so an IPC force-republish
//! is never starved by a continuously-firing if-watch stream.
//! `Notify::notified()` is constructed fresh each loop turn (correct —
//! that future represents "the next notification after this point").

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::Notify;
use tokio::time::{interval, timeout};

use crate::iroh_endpoint::IrohEndpoint;

/// How long to coalesce rapid `if-watch` events before publishing.
const NETWORK_CHANGE_DEBOUNCE: Duration = Duration::from_secs(2);
/// How often to re-publish even when nothing has changed.
const IDLE_REFRESH_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Callback invoked when the publisher decides it's time to publish a
/// fresh `ReachabilityAnnounce`. The async fn returns once the event
/// has been signed and inserted into every community CRDT this device
/// is in — the callback iterates internally.
///
/// Decoupled via a callback so the publisher module doesn't need to
/// know about community-state internals; `event_loop.rs` (Task 8) wires
/// up the actual emit.
pub type PublishFn =
    Arc<dyn Fn() -> futures::future::BoxFuture<'static, ()> + Send + Sync + 'static>;

/// Background republish driver. Construct via [`Self::new`], then call
/// [`Self::spawn`] (consumes the `Arc<Self>`) to start the loop.
pub struct ReachabilityPublisher {
    /// Held so the loop's lifetime is tied to the endpoint's. The iroh
    /// address watcher is passed to [`Self::new`] pre-boxed (see
    /// `addr_stream`) rather than derived from this field, so the loop body
    /// itself never reads it — silence the dead-code lint.
    #[allow(dead_code)]
    endpoint: Arc<IrohEndpoint>,
    publish: PublishFn,
    /// Wakes the publisher loop immediately. Cloned and exposed via
    /// `force_handle()` for the `connectivity_force_republish` IPC
    /// (Task 9) and the force-notify test below.
    force: Arc<Notify>,
    /// Optional iroh `watch_addr` update stream (ZEB-621), taken once by
    /// [`Self::spawn`] and merged into the network-change arm. Behind a
    /// `Mutex<Option<_>>` because `spawn` only holds a shared `Arc<Self>`;
    /// `None` (or after `take()`) means the interface watcher / idle
    /// backstop carry the loop alone.
    addr_stream: std::sync::Mutex<Option<futures::stream::BoxStream<'static, iroh::EndpointAddr>>>,
}

impl ReachabilityPublisher {
    pub fn new(
        endpoint: Arc<IrohEndpoint>,
        publish: PublishFn,
        addr_stream: Option<futures::stream::BoxStream<'static, iroh::EndpointAddr>>,
    ) -> Self {
        Self {
            endpoint,
            publish,
            force: Arc::new(Notify::new()),
            addr_stream: std::sync::Mutex::new(addr_stream),
        }
    }

    /// Clone the force-notify handle so external callers (IPC, tests)
    /// can wake the loop without holding the whole `Arc<Self>`.
    pub fn force_handle(&self) -> Arc<Notify> {
        Arc::clone(&self.force)
    }

    /// Spawn the publisher loop. Returns a `JoinHandle` the caller can
    /// optionally `abort()` on shutdown.
    pub fn spawn(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            // Take the iroh addr-update stream handed in at construction (if
            // any). Behind a `Mutex<Option<_>>` because `spawn` only holds a
            // shared `Arc<Self>`; `take()` drops the guard on the same line
            // so it never crosses an await.
            let addr_stream = self
                .addr_stream
                .lock()
                .expect("addr_stream mutex poisoned")
                .take();

            // 1. On startup, publish immediately.
            (self.publish)().await;

            // 2. Set up the interface watcher. `if-watch` 3.x exposes
            //    `IfWatcher` as a sync constructor returning
            //    `io::Result<Self>`, and the watcher itself implements
            //    `futures::Stream<Item = io::Result<IfEvent>>`. If init
            //    fails (e.g. no netlink / SystemConfiguration), we degrade
            //    to whatever remains — the iroh addr stream, else idle-only.
            let iface_stream = match if_watch::tokio::IfWatcher::new() {
                Ok(s) => Some(s),
                Err(e) => {
                    tracing::warn!(
                        "if-watch init failed: {e}; relying on iroh addr stream / idle-only republish"
                    );
                    None
                }
            };

            // 3. Merge every available change source into ONE stream, each
            //    event flattened to `()` — the loop only needs to know that
            //    *something* changed. A single merged stream feeding a single
            //    debounce-drain means an if-watch tick and a watch_addr tick
            //    milliseconds apart coalesce into one publish by construction
            //    (ZEB-621: closes the "if-watch fires, watch_addr fires 500ms
            //    later → two publishes" hole). `tracing::debug!` inside each
            //    map names the source that fired.
            let change_stream: Option<futures::stream::BoxStream<'static, ()>> = match (
                iface_stream,
                addr_stream,
            ) {
                (Some(iface), Some(addr)) => {
                    let iface = iface.map(|_ev| {
                        tracing::debug!("reachability change source: if-watch interface event");
                    });
                    let addr = addr.map(|_addr| {
                        tracing::debug!("reachability change source: iroh watch_addr event");
                    });
                    Some(futures::stream::select(iface, addr).boxed())
                }
                (Some(iface), None) => Some(
                    iface
                        .map(|_ev| {
                            tracing::debug!("reachability change source: if-watch interface event");
                        })
                        .boxed(),
                ),
                (None, Some(addr)) => {
                    // Degraded: interface watcher unavailable, but the
                    // iroh addr stream still drives fast republishes.
                    tracing::warn!(
                            "if-watch unavailable; using iroh watch_addr as the sole network-change source"
                        );
                    Some(
                        addr.map(|_addr| {
                            tracing::debug!("reachability change source: iroh watch_addr event");
                        })
                        .boxed(),
                    )
                }
                (None, None) => None,
            };

            let mut change_stream = match change_stream {
                Some(s) => s,
                None => {
                    self.idle_loop().await;
                    return;
                }
            };

            let mut idle_tick = interval(IDLE_REFRESH_INTERVAL);
            // `tokio::time::interval` fires the first tick immediately;
            // we already published at startup, so consume it.
            idle_tick.tick().await;

            loop {
                tokio::select! {
                    biased;

                    // Force republish (IPC trigger or test). Polled
                    // first each iteration so a force is never starved
                    // by a continuously-firing change stream.
                    _ = self.force.notified() => {
                        (self.publish)().await;
                    }

                    // Any network change — interface flap OR iroh
                    // addr/home-relay update. Drain rapid follow-ups
                    // within the debounce window, from BOTH sources, so a
                    // single DHCP rebind (Down → Up in ms) or relay flap
                    // collapses into one publish. `timeout` returns
                    // `Err(_)` on the elapsed deadline — exactly what we
                    // want, so we discard its `Result`.
                    item = change_stream.next() => {
                        if item.is_none() {
                            // Every source ended (watchers died) —
                            // degrade gracefully to idle-only.
                            tracing::warn!("network-change stream terminated; falling back to idle-only republish");
                            self.idle_loop().await;
                            return;
                        }
                        let _ = timeout(NETWORK_CHANGE_DEBOUNCE, async {
                            while change_stream.next().await.is_some() {
                                // Keep draining; we'll publish once the
                                // debounce window elapses.
                            }
                        }).await;
                        (self.publish)().await;
                    }

                    // 60-minute idle backstop.
                    _ = idle_tick.tick() => {
                        (self.publish)().await;
                    }
                }
            }
        })
    }

    /// Fallback path when `if-watch` can't be initialized (or its
    /// background thread dies mid-run). Same `force` + `idle_tick`
    /// branches as the main loop, minus the network-change arm.
    async fn idle_loop(self: Arc<Self>) {
        let mut idle_tick = interval(IDLE_REFRESH_INTERVAL);
        idle_tick.tick().await;
        loop {
            tokio::select! {
                biased;
                _ = self.force.notified() => {
                    (self.publish)().await;
                }
                _ = idle_tick.tick() => {
                    (self.publish)().await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iroh_endpoint::{alpn, IrohEndpoint};
    use iroh::endpoint::{presets, Endpoint, RelayMode};
    use iroh::SecretKey;
    use std::net::Ipv4Addr;

    /// Build a hermetic iroh endpoint on loopback with no
    /// address-lookup / relay traffic. Mirrors the pattern in
    /// `zenoh_iroh_transport::tests::build_hermetic_iroh_endpoint` —
    /// goes through `IrohEndpoint::from_endpoint_for_test` so the
    /// production path `new_with_secret` (which uses `presets::N0` +
    /// pkarr + DNS and hangs offline) stays untouched.
    async fn build_hermetic_iroh_endpoint() -> Arc<IrohEndpoint> {
        let secret = SecretKey::generate();
        let inner = Endpoint::builder(presets::Minimal)
            .secret_key(secret)
            .alpns(vec![alpn::HARMONY_ZENOH_V1.to_vec()])
            .relay_mode(RelayMode::Disabled)
            .clear_ip_transports()
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .expect("bind_addr loopback")
            .bind()
            .await
            .expect("bind iroh endpoint");
        Arc::new(IrohEndpoint::from_endpoint_for_test(inner))
    }

    /// Force-notify path: startup-publish lands, then a `notify_one()`
    /// triggers a second publish. Test scope is intentionally limited
    /// to startup + force per the implementer brief:
    /// - The `if-watch` path needs real network manipulation (root /
    ///   virtual interfaces) to exercise hermetically.
    /// - The 60-minute idle tick can't be driven with `tokio::pause()`
    ///   without rewriting the loop body (interval composes poorly
    ///   with `Notify` under paused time).
    ///
    /// 15-second outer timeout is the kill switch — same defense-in-
    /// depth pattern as Task 5's `paired_stream_roundtrip_via_loopback`.
    /// The force-notify path should fire in microseconds; if this test
    /// approaches its budget something is wrong with the select loop.
    /// 30s outer to absorb under-load iroh bind latency (the inner
    /// hermetic-endpoint helper takes ~10s solo, observed ~14-19s under
    /// full-suite parallelism — same flake-class as the paired_stream
    /// fix in c089127). PR #157 round 5.
    #[tokio::test]
    async fn force_notify_triggers_publish() {
        // ZEB-347: hoist the one-time process-global iroh bind init out of
        // the asserted timeout below so it can't trip the budget under load.
        crate::iroh_endpoint::warm_up_iroh_global_init().await;
        tokio::time::timeout(
            Duration::from_secs(30),
            force_notify_triggers_publish_inner(),
        )
        .await
        .expect("force_notify_triggers_publish must complete within 30s");
    }

    async fn force_notify_triggers_publish_inner() {
        // Each publish call signals this Notify; the test awaits it
        // rather than polling an atomic in a sleep loop. The startup
        // publish should fire immediately on spawn; the post-force one
        // should fire within microseconds of `notify_one()`.
        let published = Arc::new(Notify::new());
        let p2 = Arc::clone(&published);
        let publish: PublishFn = Arc::new(move || {
            let n = Arc::clone(&p2);
            Box::pin(async move {
                n.notify_one();
            }) as futures::future::BoxFuture<'static, ()>
        });

        let ep = build_hermetic_iroh_endpoint().await;
        // No addr stream here: this test exercises only the startup + force
        // paths, so the third parameter is `None`.
        let publisher = Arc::new(ReachabilityPublisher::new(ep, publish, None));
        let force = publisher.force_handle();
        let _handle = publisher.spawn();

        // Startup publish.
        tokio::time::timeout(Duration::from_secs(2), published.notified())
            .await
            .expect("startup publish must fire within 2s");

        // Force-notify publish.
        force.notify_one();
        tokio::time::timeout(Duration::from_secs(2), published.notified())
            .await
            .expect("force-notify publish must fire within 2s of notify_one()");
    }

    /// Build a throwaway `EndpointAddr` from a random identity — enough to
    /// stand in for a real `watch_addr` update on the fake addr stream. The
    /// publisher only cares that an item *arrived*, not its contents.
    fn fake_endpoint_addr() -> iroh::EndpointAddr {
        iroh::EndpointAddr::new(SecretKey::generate().public())
    }

    /// Adapt a `tokio::sync::mpsc::Receiver<EndpointAddr>` into the
    /// `BoxStream` the publisher consumes. `tokio_stream` isn't a dep, so we
    /// use a small `futures::stream::unfold` over the receiver (per Task 3
    /// brief). The stream ends when every sender is dropped.
    fn addr_stream_from_rx(
        rx: tokio::sync::mpsc::Receiver<iroh::EndpointAddr>,
    ) -> futures::stream::BoxStream<'static, iroh::EndpointAddr> {
        Box::pin(futures::stream::unfold(rx, |mut rx| async {
            rx.recv().await.map(|a| (a, rx))
        })) as futures::stream::BoxStream<'static, iroh::EndpointAddr>
    }

    /// ZEB-621 acceptance: a home-relay change (addr-stream event) triggers a
    /// publish within the debounce window — NOT the 60-minute backstop.
    #[tokio::test]
    async fn addr_change_triggers_publish_within_debounce() {
        crate::iroh_endpoint::warm_up_iroh_global_init().await;
        tokio::time::timeout(Duration::from_secs(40), async {
            let published = Arc::new(Notify::new());
            let p2 = Arc::clone(&published);
            let publish: PublishFn = Arc::new(move || {
                let n = Arc::clone(&p2);
                Box::pin(async move {
                    n.notify_one();
                }) as futures::future::BoxFuture<'static, ()>
            });
            let ep = build_hermetic_iroh_endpoint().await;
            let (tx, rx) = tokio::sync::mpsc::channel::<iroh::EndpointAddr>(8);
            let addr_stream = addr_stream_from_rx(rx);
            let publisher = Arc::new(ReachabilityPublisher::new(
                ep.clone(),
                publish,
                Some(addr_stream),
            ));
            let _handle = publisher.spawn();
            // startup publish
            tokio::time::timeout(Duration::from_secs(5), published.notified())
                .await
                .expect("startup publish");
            // inject an addr change → publish within debounce(2s) + slack
            tx.send(fake_endpoint_addr())
                .await
                .expect("send addr event");
            tokio::time::timeout(Duration::from_secs(10), published.notified())
                .await
                .expect("addr-change publish within 10s (2s debounce + slack)");
        })
        .await
        .expect("test must complete inside outer budget");
    }

    /// ZEB-621: three addr-stream events inside the debounce window collapse
    /// into a single publish. The drain arm coalesces rapid follow-ups (from
    /// *either* source), so a relay flap can't fan out into N republishes.
    #[tokio::test]
    async fn addr_flap_coalesces_to_one_publish() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        crate::iroh_endpoint::warm_up_iroh_global_init().await;
        tokio::time::timeout(Duration::from_secs(40), async {
            let count = Arc::new(AtomicUsize::new(0));
            let startup = Arc::new(Notify::new());
            let c2 = Arc::clone(&count);
            let s2 = Arc::clone(&startup);
            let publish: PublishFn = Arc::new(move || {
                let c = Arc::clone(&c2);
                let s = Arc::clone(&s2);
                Box::pin(async move {
                    // The startup publish is the first increment; signal it so
                    // the test can align before injecting the flap.
                    if c.fetch_add(1, Ordering::SeqCst) == 0 {
                        s.notify_one();
                    }
                }) as futures::future::BoxFuture<'static, ()>
            });
            let ep = build_hermetic_iroh_endpoint().await;
            let (tx, rx) = tokio::sync::mpsc::channel::<iroh::EndpointAddr>(8);
            let addr_stream = addr_stream_from_rx(rx);
            let publisher = Arc::new(ReachabilityPublisher::new(
                ep.clone(),
                publish,
                Some(addr_stream),
            ));
            let _handle = publisher.spawn();
            // Wait for the startup publish so the flap injection races nothing.
            tokio::time::timeout(Duration::from_secs(5), startup.notified())
                .await
                .expect("startup publish");
            // Three events 50ms apart — comfortably inside the 2s window.
            for _ in 0..3 {
                tx.send(fake_endpoint_addr())
                    .await
                    .expect("send addr event");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            // Sleep well past the debounce window so the single coalesced
            // publish has landed.
            tokio::time::sleep(Duration::from_secs(6)).await;
            // Startup (1) + exactly one coalesced flap publish (1) = 2.
            assert_eq!(
                count.load(Ordering::SeqCst),
                2,
                "3 rapid addr events must coalesce into a single publish"
            );
        })
        .await
        .expect("test must complete inside outer budget");
    }
}
