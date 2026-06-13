//! ZEB-458 P4 Phase B: background publisher that re-emits this device's
//! CommunityRelayAnnounce for every opted-in community on a small set of
//! triggers (mirrors `reachability_publisher`, minus the if-watch network arm).
//!
//! Decoupled via a `PublishFn` callback that signs + inserts the announce into
//! each opted-in community's CRDT; this module knows nothing about
//! community-state internals (start_node wires the callback — T11).

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::time::interval;

/// Callback invoked when it's time to (re)publish. The async fn signs + inserts
/// a fresh CommunityRelayAnnounce into every opted-in community this device is a
/// Joined member of — it iterates internally. Same shape as the reachability
/// publisher's `PublishFn`.
pub type RelayPublishFn =
    Arc<dyn Fn() -> futures::future::BoxFuture<'static, ()> + Send + Sync + 'static>;

pub struct CommunityRelayPublisher {
    publish: RelayPublishFn,
    /// Periodic refresh cadence (default `COMMUNITY_RELAY_AD_REFRESH_MS`).
    refresh: Duration,
    /// Immediate-wake handle (opt-in toggle / membership change pokes this).
    force: Arc<Notify>,
}

impl CommunityRelayPublisher {
    pub fn new(publish: RelayPublishFn) -> Self {
        Self {
            publish,
            refresh: Duration::from_millis(
                crate::community_relay_announce::COMMUNITY_RELAY_AD_REFRESH_MS,
            ),
            force: Arc::new(Notify::new()),
        }
    }

    /// Test/override constructor with an explicit cadence.
    pub fn with_refresh(publish: RelayPublishFn, refresh: Duration) -> Self {
        Self {
            publish,
            refresh,
            force: Arc::new(Notify::new()),
        }
    }

    /// Clone the force-notify handle so external callers (opt-in IPC, the
    /// event-loop republish timer, tests) can wake the loop.
    pub fn force_handle(&self) -> Arc<Notify> {
        Arc::clone(&self.force)
    }

    /// Spawn the loop: publish once at startup, then on every force-notify and
    /// every `refresh` tick. Returns the JoinHandle (caller may abort on shutdown).
    pub fn spawn(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            (self.publish)().await; // startup publish
            let mut tick = interval(self.refresh);
            tick.tick().await; // consume the immediate first tick (already published)
            loop {
                tokio::select! {
                    biased;
                    _ = self.force.notified() => { (self.publish)().await; }
                    _ = tick.tick() => { (self.publish)().await; }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Startup + force-notify path: the startup publish fires immediately, then
    /// a `notify_one()` triggers a second publish. The interval is NOT driven
    /// (composes poorly with paused time — same reason the reachability test
    /// skips it).
    ///
    /// 10-second outer kill switch — the force-notify path fires in
    /// microseconds; if this approaches budget something is wrong.
    #[tokio::test]
    async fn startup_and_force_notify_trigger_publish() {
        tokio::time::timeout(
            Duration::from_secs(10),
            startup_and_force_notify_trigger_publish_inner(),
        )
        .await
        .expect("startup_and_force_notify_trigger_publish must complete within 10s");
    }

    async fn startup_and_force_notify_trigger_publish_inner() {
        // Each publish call signals this Notify; the test awaits it rather
        // than polling an atomic in a sleep loop.
        let published = Arc::new(Notify::new());
        let p2 = Arc::clone(&published);
        let publish: RelayPublishFn = Arc::new(move || {
            let n = Arc::clone(&p2);
            Box::pin(async move {
                n.notify_one();
            }) as futures::future::BoxFuture<'static, ()>
        });

        // Use a very long refresh cadence so the interval never fires during
        // the test — we only exercise startup + force-notify.
        let publisher = Arc::new(CommunityRelayPublisher::with_refresh(
            publish,
            Duration::from_secs(3600),
        ));
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
