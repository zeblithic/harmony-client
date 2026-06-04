//! ZEB-373: dynamic mid-session iroh dial driver. Consumes `DialHint`s from the
//! resolver notify seam, dedups by node-id, and dials each newly-learned peer once
//! through a `PeerDialer` with bounded backoff. Re-dial on transport drop is out of
//! scope (ZEB-321 Phase 3).
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::network_health::DialTelemetry;
use crate::reachability_resolver::DialHint;

/// Abstraction over "dial this iroh peer". Production wraps a zenoh `Runtime`
/// (`connect_peer`); tests use a mock. `locator` is `iroh/<hex>`.
#[async_trait::async_trait]
pub trait PeerDialer: Send + Sync {
    async fn dial(&self, node_id: [u8; 32], locator: String) -> bool;
}

fn iroh_locator(node_id: &[u8; 32]) -> String {
    format!("iroh/{}", hex::encode(node_id))
}

/// Run the dial driver until the hint channel closes (node stop drops the sender).
/// `backoff_base` is the first retry delay (doubles each retry); tests pass ZERO.
pub async fn run_dial_driver(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<DialHint>,
    dialer: Arc<dyn PeerDialer>,
    telemetry: Arc<DialTelemetry>,
    self_node_id: [u8; 32],
    backoff_base: Duration,
) {
    let dialed: Arc<Mutex<HashSet<[u8; 32]>>> = Arc::new(Mutex::new(HashSet::new()));
    while let Some(hint) = rx.recv().await {
        if hint.node_id == self_node_id {
            tracing::debug!("ZEB-373: skip dial to self");
            continue;
        }
        {
            let mut d = dialed.lock().expect("dialed set lock");
            if !d.insert(hint.node_id) {
                telemetry.record_skipped_duplicate();
                continue;
            }
        }
        let dialer = Arc::clone(&dialer);
        let telemetry = Arc::clone(&telemetry);
        let dialed = Arc::clone(&dialed);
        tokio::spawn(async move {
            telemetry.record_attempt();
            let loc = iroh_locator(&hint.node_id);
            let mut ok = dialer.dial(hint.node_id, loc.clone()).await;
            let mut delay = backoff_base;
            let mut attempts = 1u32;
            while !ok && attempts < 3 {
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                delay = delay.saturating_mul(2);
                ok = dialer.dial(hint.node_id, loc.clone()).await;
                attempts += 1;
            }
            if ok {
                telemetry.record_succeeded(hint.node_id, hint.owner);
                tracing::info!(
                    "ZEB-373: dialed iroh peer {}",
                    hex::encode(&hint.node_id[..4])
                );
            } else {
                telemetry.record_failed(hint.node_id, hint.owner);
                dialed
                    .lock()
                    .expect("dialed set lock")
                    .remove(&hint.node_id);
                tracing::warn!(
                    "ZEB-373: dial failed (3 attempts) for {}",
                    hex::encode(&hint.node_id[..4])
                );
            }
        });
    }
    tracing::debug!("ZEB-373: dial driver stopping (hint channel closed)");
}

#[cfg(test)]
mod tests {
    use super::*;
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
            owner: [0xAA; 16],
        }
    }

    #[tokio::test]
    async fn dials_new_peer_once_and_skips_self_and_duplicates() {
        let dialer = Arc::new(MockDialer {
            calls: AtomicU32::new(0),
            fail_first_n: 0,
        });
        let telemetry = Arc::new(DialTelemetry::new());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let self_id = [0xEE; 32];
        let driver = tokio::spawn(run_dial_driver(
            rx,
            dialer.clone(),
            telemetry.clone(),
            self_id,
            Duration::ZERO,
        ));
        tx.send(hint(0x11)).unwrap();
        tx.send(hint(0x11)).unwrap();
        tx.send(DialHint {
            node_id: self_id,
            owner: [0xAA; 16],
        })
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
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let driver = tokio::spawn(run_dial_driver(
            rx,
            dialer.clone(),
            telemetry.clone(),
            [0xEE; 32],
            Duration::ZERO,
        ));
        tx.send(hint(0x22)).unwrap();
        drop(tx);
        driver.await.unwrap();
        let s = telemetry.summary();
        assert_eq!(s.succeeded, 1);
        assert_eq!(
            dialer.calls.load(Ordering::SeqCst),
            3,
            "3 attempts: fail,fail,succeed"
        );
    }

    #[tokio::test]
    async fn exhausted_failure_rearms_for_redial() {
        let dialer = Arc::new(MockDialer {
            calls: AtomicU32::new(0),
            fail_first_n: 100,
        });
        let telemetry = Arc::new(DialTelemetry::new());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let driver = tokio::spawn(run_dial_driver(
            rx,
            dialer.clone(),
            telemetry.clone(),
            [0xEE; 32],
            Duration::ZERO,
        ));
        tx.send(hint(0x33)).unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        tx.send(hint(0x33)).unwrap();
        drop(tx);
        driver.await.unwrap();
        let s = telemetry.summary();
        assert_eq!(s.failed, 2, "both rounds recorded a terminal failure");
        assert_eq!(dialer.calls.load(Ordering::SeqCst), 6, "3 + 3 attempts");
    }
}
