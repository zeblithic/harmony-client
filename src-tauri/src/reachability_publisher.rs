//! ZEB-321 Phase 1 Task 7: background task that re-emits this device's
//! `ReachabilityAnnounce` on a small set of triggers.
//!
//! See `docs/specs/2026-05-22-zeb-321-cross-wan-connectivity-design.md` §5.6
//! and `docs/plans/2026-05-22-zeb-321-phase1-iroh-foundation-plan.md` Task 7.
//!
//! ## Triggers implemented
//!
//! 1. **Startup** — publish immediately, no debounce.
//! 2. **Network-interface change** — `if-watch` IfEvent (Up/Down on a local
//!    `IpNet`), coalesced with a 2-second debounce window so a single
//!    DHCP rebind that emits Down → Up in milliseconds collapses into
//!    one publish.
//! 3. **Idle tick** — every 60 minutes, to refresh against the 24h TTL on
//!    `ReachabilityRecord` even when nothing has changed.
//! 4. **Force-notify** — `tokio::sync::Notify` wake from
//!    `connectivity_force_republish` IPC (Task 9) or test code.
//!
//! Home-relay-change (spec §5.6 bullet 4) is intentionally NOT handled
//! here: iroh exposes its watch via `Endpoint::home_relay()` returning a
//! `Watcher` whose stream surface is awkward to compose with the rest of
//! the select loop, and the idle-60min cadence backstops the
//! correctness case. Phase-1 ships without it; revisit in Phase 2 if a
//! relay-flap is observed dropping calls in practice.
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
    /// Kept as a field for future expansion (home-relay watcher, direct
    /// addresses watcher) and so the loop's lifetime is tied to the
    /// endpoint's. Not read by the current loop body — silence the
    /// dead-code lint until Phase 2 / Task 8 plumbs it through.
    #[allow(dead_code)]
    endpoint: Arc<IrohEndpoint>,
    publish: PublishFn,
    /// Wakes the publisher loop immediately. Cloned and exposed via
    /// `force_handle()` for the `connectivity_force_republish` IPC
    /// (Task 9) and the force-notify test below.
    force: Arc<Notify>,
}

impl ReachabilityPublisher {
    pub fn new(endpoint: Arc<IrohEndpoint>, publish: PublishFn) -> Self {
        Self {
            endpoint,
            publish,
            force: Arc::new(Notify::new()),
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
            // 1. On startup, publish immediately.
            (self.publish)().await;

            // 2. Set up the network-change watcher. `if-watch` 3.x
            //    exposes `IfWatcher` as a sync constructor returning
            //    `io::Result<Self>`, and the watcher itself implements
            //    `futures::Stream<Item = io::Result<IfEvent>>` — so we
            //    drive it with `StreamExt::next()` rather than the
            //    `poll_fn` ceremony in the plan draft. If init fails
            //    (e.g. the host has no netlink / SystemConfiguration
            //    available), we degrade to the idle-only loop.
            let mut iface_stream = match if_watch::tokio::IfWatcher::new() {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        "if-watch init failed: {e}; falling back to idle-only republish"
                    );
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
                    // by a continuously-firing if-watch stream.
                    _ = self.force.notified() => {
                        (self.publish)().await;
                    }

                    // Network-interface change. Drain any rapid
                    // follow-ups within the debounce window so a single
                    // DHCP rebind (Down → Up in milliseconds) collapses
                    // into one publish. `timeout` returns `Err(_)` on
                    // the elapsed deadline — exactly what we want, so
                    // we discard its `Result`.
                    item = iface_stream.next() => {
                        if item.is_none() {
                            // Stream ended (background watcher died) —
                            // degrade gracefully to idle-only.
                            tracing::warn!("if-watch stream terminated; falling back to idle-only republish");
                            self.idle_loop().await;
                            return;
                        }
                        let _ = timeout(NETWORK_CHANGE_DEBOUNCE, async {
                            while iface_stream.next().await.is_some() {
                                // Keep draining; we'll publish once the
                                // debounce window elapses.
                            }
                        }).await;
                        (self.publish)().await;
                    }

                    // 60-minute idle refresh.
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
        let publisher = Arc::new(ReachabilityPublisher::new(ep, publish));
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
}
