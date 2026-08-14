//! SimBus — a partition-gated in-memory broadcast fabric for the CRDT plane.
//!
//! Each logical node publishes `Vec<u8>` frames onto its `publisher_tx`; the
//! bus drains them and re-delivers to every *same-partition* peer's
//! `subscriber_tx`. Cross-partition frames are DROPPED (modelling transport
//! loss) — there is no store-and-forward, so a dropped frame is gone and
//! post-heal convergence relies on a fresh publish (see the module-level
//! convergence note in `community.rs`).

use super::partition::Partition;
use tokio::sync::mpsc;

/// A partition-gated broadcast fabric. Holds one drainer task per source;
/// dropping the bus aborts them so they cannot outlive the sim (mirrors
/// `SimNode`'s Drop guard).
pub(crate) struct SimBus {
    drainers: Vec<tokio::task::JoinHandle<()>>,
}

impl SimBus {
    /// Spawn one drainer per source. `sources[i]` is node i's publisher
    /// receiver (the far end of its engine's `publisher_tx`); `sinks[i]` is
    /// node i's subscriber sender (feeds its engine's `subscriber_rx`);
    /// `tags[i]` is node i's partition key. A frame drained from source i is
    /// delivered to every sink j != i for which
    /// `partition.same_side(tags[i], tags[j])` holds AT DELIVERY TIME.
    pub(crate) fn spawn(
        sources: Vec<mpsc::Receiver<Vec<u8>>>,
        sinks: Vec<mpsc::Sender<Vec<u8>>>,
        tags: Vec<[u8; 32]>,
        partition: Partition,
    ) -> Self {
        assert_eq!(sources.len(), sinks.len(), "one sink per source");
        assert_eq!(sources.len(), tags.len(), "one tag per source");
        let drainers = sources
            .into_iter()
            .enumerate()
            .map(|(i, mut src)| {
                let sinks = sinks.clone();
                let tags = tags.clone();
                let partition = partition.clone();
                let src_tag = tags[i];
                tokio::spawn(async move {
                    while let Some(bytes) = src.recv().await {
                        for (j, sink) in sinks.iter().enumerate() {
                            if j == i {
                                continue;
                            }
                            // Evaluated per-frame so a mid-run split/heal takes
                            // effect immediately.
                            if partition.same_side(src_tag, tags[j]) {
                                // Non-blocking: a saturated sink drops the frame
                                // (consistent with the bus's transport-loss
                                // model) rather than parking the whole fan-out
                                // and starving healthy peers after `j`.
                                let _ = sink.try_send(bytes.clone());
                            }
                        }
                    }
                })
            })
            .collect();
        Self { drainers }
    }
}

impl Drop for SimBus {
    fn drop(&mut self) {
        for d in &self.drainers {
            d.abort();
        }
    }
}

#[cfg(test)]
mod bus_tests {
    use super::*;

    /// Yield the runtime enough times for the spawned drainers to forward,
    /// then non-blocking `try_recv`. Deterministic: no real sleep, and under a
    /// single-threaded runtime the drainer runs to its next `.await` point on
    /// each yield.
    async fn drained(rx: &mut mpsc::Receiver<Vec<u8>>) -> Option<Vec<u8>> {
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        rx.try_recv().ok()
    }

    #[tokio::test]
    async fn bus_gates_delivery_by_partition() {
        let partition = Partition::fully_connected();
        let tags = vec![[1u8; 32], [2u8; 32], [3u8; 32]];

        let (o0, or0) = mpsc::channel::<Vec<u8>>(64);
        let (o1, or1) = mpsc::channel::<Vec<u8>>(64);
        let (_o2, or2) = mpsc::channel::<Vec<u8>>(64);
        let (i0t, mut _i0r) = mpsc::channel::<Vec<u8>>(64);
        let (i1t, mut i1r) = mpsc::channel::<Vec<u8>>(64);
        let (i2t, mut i2r) = mpsc::channel::<Vec<u8>>(64);

        let _bus = SimBus::spawn(
            vec![or0, or1, or2],
            vec![i0t, i1t, i2t],
            tags.clone(),
            partition.clone(),
        );

        // Fully connected: node 0's frame reaches 1 and 2.
        o0.send(b"m1".to_vec()).await.unwrap();
        assert_eq!(drained(&mut i1r).await, Some(b"m1".to_vec()));
        assert_eq!(drained(&mut i2r).await, Some(b"m1".to_vec()));

        // Partition {0} | {1,2}: node 0's frame is dropped for 1 and 2. The
        // heal phase below re-delivers on these same channels — that positive
        // control proves the bus is live, so these `None`s are genuine drops,
        // not a dead/undelivered fabric.
        partition.set_split(vec![vec![[1u8; 32]], vec![[2u8; 32], [3u8; 32]]]);
        o0.send(b"drop".to_vec()).await.unwrap();
        assert_eq!(
            drained(&mut i1r).await,
            None,
            "cross-partition frame must drop"
        );
        assert_eq!(
            drained(&mut i2r).await,
            None,
            "cross-partition frame must drop"
        );

        // Intra-island still flows: node 1 -> node 2 (same island).
        o1.send(b"intra".to_vec()).await.unwrap();
        assert_eq!(drained(&mut i2r).await, Some(b"intra".to_vec()));

        // Heal: node 0's frame reaches 1 and 2 again.
        partition.heal();
        o0.send(b"back".to_vec()).await.unwrap();
        assert_eq!(drained(&mut i1r).await, Some(b"back".to_vec()));
        assert_eq!(drained(&mut i2r).await, Some(b"back".to_vec()));
    }
}
