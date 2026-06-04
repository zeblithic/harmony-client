//! ZEB-373: dynamic mid-session iroh dial driver. Consumes `DialHint`s from the
//! resolver notify seam, dedups by node-id (bounded, FIFO-evicting), and dials each
//! newly-learned peer once through a `PeerDialer` with bounded backoff. Re-dialing a
//! peer whose dial failed, or whose transport later drops, is out of scope — that is
//! liveness/reconnection (ZEB-321 Phase 3).
use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::network_health::DialTelemetry;
use crate::reachability_resolver::DialHint;

use zenoh::internal::runtime::Runtime;
use zenoh_protocol::core::{Locator, ZenohIdProto};

/// Capacity of the resolver→driver dial-hint channel. Bounded so reachability
/// discovery cannot become unbounded heap growth if a peer floods unique node-ids
/// faster than the driver drains (CodeRabbit). Generously sized to absorb a normal
/// burst (a community's worth of first-learns at once); the driver drains in O(µs)
/// per hint, so this only backs up under a genuine flood, where dropping excess
/// hints (`try_send`) is the intended back-pressure.
pub const DIAL_HINT_CHANNEL_CAP: usize = 1024;

/// Cap on the "already dialed this session" dedup set. Bounds memory under a
/// long-lived session or adversarial churn of unique node-ids (CodeAnt). A node
/// realistically dials at most its community-membership's worth of peers, well
/// under this; the oldest entries evict (FIFO) past the cap.
const DIALED_SET_CAP: usize = 4096;

/// Max dial attempts per hint (initial try + retries). A persistently-unreachable
/// peer is left for ZEB-321 Phase 3 (liveness), not retried across the session.
const MAX_DIAL_ATTEMPTS: u32 = 3;

/// Bounded dedup of node-ids already dialed this session. Membership is a `HashSet`;
/// insertion order is tracked in a `VecDeque` so the oldest node-id evicts once the
/// cap is exceeded, keeping memory bounded regardless of how many distinct peers are
/// seen.
struct DialedSet {
    seen: HashSet<[u8; 32]>,
    order: VecDeque<[u8; 32]>,
}

impl DialedSet {
    fn new() -> Self {
        Self {
            seen: HashSet::new(),
            order: VecDeque::new(),
        }
    }

    /// Claim a node-id for dialing. Returns true if newly claimed (caller should
    /// dial), false if already present (caller skips it as a duplicate).
    fn claim(&mut self, id: [u8; 32]) -> bool {
        if !self.seen.insert(id) {
            return false;
        }
        self.order.push_back(id);
        if self.order.len() > DIALED_SET_CAP {
            if let Some(old) = self.order.pop_front() {
                self.seen.remove(&old);
            }
        }
        true
    }
}

/// Abstraction over "dial this iroh peer". Production wraps a zenoh `Runtime`
/// (`connect_peer`); tests use a mock. `locator` is `iroh/<hex>`.
#[async_trait::async_trait]
pub trait PeerDialer: Send + Sync {
    async fn dial(&self, node_id: [u8; 32], locator: String) -> bool;
}

fn iroh_locator(node_id: &[u8; 32]) -> String {
    format!("iroh/{}", hex::encode(node_id))
}

/// Run the dial driver until the hint channel closes (node stop drops the resolver's
/// sender). `backoff_base` is the first retry delay (doubles each retry); tests pass
/// ZERO.
pub async fn run_dial_driver(
    mut rx: tokio::sync::mpsc::Receiver<DialHint>,
    dialer: Arc<dyn PeerDialer>,
    telemetry: Arc<DialTelemetry>,
    self_node_id: [u8; 32],
    backoff_base: Duration,
) {
    let dialed = Arc::new(Mutex::new(DialedSet::new()));
    while let Some(hint) = rx.recv().await {
        if hint.node_id == self_node_id {
            tracing::debug!("ZEB-373: skip dial to self");
            continue;
        }
        if !dialed.lock().expect("dialed set lock").claim(hint.node_id) {
            telemetry.record_skipped_duplicate();
            continue;
        }
        let dialer = Arc::clone(&dialer);
        let telemetry = Arc::clone(&telemetry);
        tokio::spawn(async move {
            let loc = iroh_locator(&hint.node_id);
            // Count EVERY actual dial() call (incl. retries) so the attempts metric
            // matches real network operations, not hints handled (CodeAnt).
            telemetry.record_attempt();
            let mut ok = dialer.dial(hint.node_id, loc.clone()).await;
            let mut attempts = 1u32;
            let mut delay = backoff_base;
            while !ok && attempts < MAX_DIAL_ATTEMPTS {
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                delay = delay.saturating_mul(2);
                telemetry.record_attempt();
                ok = dialer.dial(hint.node_id, loc.clone()).await;
                attempts += 1;
            }
            if ok {
                telemetry.record_succeeded(hint.node_id, hint.owner.0);
                tracing::info!(
                    "ZEB-373: dialed iroh peer {}",
                    hex::encode(&hint.node_id[..4])
                );
            } else {
                // Terminal for this session: the node-id stays claimed (no re-dial).
                // A persistently-unreachable peer is reconnected by ZEB-321 Phase 3
                // (liveness/rebinding), not retried here — ZEB-373 stays dial-once.
                telemetry.record_failed(hint.node_id, hint.owner.0);
                tracing::warn!(
                    "ZEB-373: dial failed ({MAX_DIAL_ATTEMPTS} attempts) for {}",
                    hex::encode(&hint.node_id[..4])
                );
            }
        });
    }
    tracing::debug!("ZEB-373: dial driver stopping (hint channel closed)");
}

/// Production `PeerDialer`: dials through the live zenoh `Runtime` via the
/// un-filtered `connect_peer` path. The placeholder zid is FRESH per dial (zenoh
/// uses it only for pre-dial dedup; the real peer zid is negotiated on the wire).
pub struct RuntimePeerDialer {
    runtime: Runtime,
}
impl RuntimePeerDialer {
    pub fn new(runtime: Runtime) -> Self {
        Self { runtime }
    }
}
#[async_trait::async_trait]
impl PeerDialer for RuntimePeerDialer {
    async fn dial(&self, _node_id: [u8; 32], locator: String) -> bool {
        let loc = match locator.parse::<Locator>() {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!("ZEB-373: bad iroh locator {locator}: {e}");
                return false;
            }
        };
        let placeholder = ZenohIdProto::rand();
        self.runtime.connect_peer(&placeholder, &[loc]).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state_types::OwnerAddr;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct MockDialer {
        calls: AtomicU32,
        fail_first_n: u32,
    }
    #[async_trait::async_trait]
    impl PeerDialer for MockDialer {
        async fn dial(&self, _node_id: [u8; 32], _locator: String) -> bool {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            n >= self.fail_first_n
        }
    }

    fn hint(node_id: u8) -> DialHint {
        DialHint {
            node_id: [node_id; 32],
            owner: OwnerAddr([0xAA; 16]),
        }
    }

    #[test]
    fn dialed_set_evicts_oldest_past_cap() {
        let mut d = DialedSet::new();
        // Fill to capacity with distinct node-ids.
        for i in 0..DIALED_SET_CAP {
            let mut id = [0u8; 32];
            id[0] = (i & 0xff) as u8;
            id[1] = (i >> 8) as u8;
            assert!(d.claim(id), "first claim is new");
        }
        assert_eq!(d.seen.len(), DIALED_SET_CAP);
        let first = {
            let mut id = [0u8; 32];
            id[0] = 0;
            id[1] = 0;
            id
        };
        assert!(!d.claim(first), "still present before overflow");
        // One more distinct id overflows the cap and evicts the oldest.
        let overflow = [0xFFu8; 32];
        assert!(d.claim(overflow), "overflow id is new");
        assert_eq!(d.seen.len(), DIALED_SET_CAP, "size stays capped");
        // The oldest (first) was evicted, so it can be claimed afresh.
        assert!(d.claim(first), "oldest evicted → reclaimable");
    }

    #[tokio::test]
    async fn dials_new_peer_once_and_skips_self_and_duplicates() {
        let dialer = Arc::new(MockDialer {
            calls: AtomicU32::new(0),
            fail_first_n: 0,
        });
        let telemetry = Arc::new(DialTelemetry::new());
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let self_id = [0xEE; 32];
        let driver = tokio::spawn(run_dial_driver(
            rx,
            dialer.clone(),
            telemetry.clone(),
            self_id,
            Duration::ZERO,
        ));
        tx.send(hint(0x11)).await.unwrap();
        tx.send(hint(0x11)).await.unwrap();
        tx.send(DialHint {
            node_id: self_id,
            owner: OwnerAddr([0xAA; 16]),
        })
        .await
        .unwrap();
        drop(tx);
        driver.await.unwrap();
        let s = telemetry.summary();
        assert_eq!(s.attempts, 1, "one real dial attempt");
        assert_eq!(s.succeeded, 1);
        assert_eq!(s.skipped_duplicate, 1);
        assert_eq!(dialer.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retries_then_succeeds_within_three_attempts() {
        let dialer = Arc::new(MockDialer {
            calls: AtomicU32::new(0),
            fail_first_n: 2,
        });
        let telemetry = Arc::new(DialTelemetry::new());
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let driver = tokio::spawn(run_dial_driver(
            rx,
            dialer.clone(),
            telemetry.clone(),
            [0xEE; 32],
            Duration::ZERO,
        ));
        tx.send(hint(0x22)).await.unwrap();
        drop(tx);
        driver.await.unwrap();
        let s = telemetry.summary();
        assert_eq!(s.succeeded, 1);
        // attempts now counts each dial() call, including the 2 retries (CodeAnt).
        assert_eq!(s.attempts, 3, "3 dial attempts recorded");
        assert_eq!(
            dialer.calls.load(Ordering::SeqCst),
            3,
            "3 dial calls: fail,fail,succeed"
        );
    }

    #[tokio::test]
    async fn exhausted_failure_is_terminal_no_redial() {
        let dialer = Arc::new(MockDialer {
            calls: AtomicU32::new(0),
            fail_first_n: 100,
        });
        let telemetry = Arc::new(DialTelemetry::new());
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let driver = tokio::spawn(run_dial_driver(
            rx,
            dialer.clone(),
            telemetry.clone(),
            [0xEE; 32],
            Duration::ZERO,
        ));
        tx.send(hint(0x33)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        // A second hint for the SAME node-id is skipped (still claimed): a failed
        // dial is terminal for the session, not re-armed (cross-refresh retry is
        // ZEB-321 Phase 3).
        tx.send(hint(0x33)).await.unwrap();
        drop(tx);
        driver.await.unwrap();
        let s = telemetry.summary();
        assert_eq!(s.failed, 1, "one terminal failure");
        assert_eq!(s.skipped_duplicate, 1, "second hint skipped as duplicate");
        assert_eq!(s.attempts, 3, "3 dial attempts from the single round");
        assert_eq!(dialer.calls.load(Ordering::SeqCst), 3, "no re-dial");
    }
}
