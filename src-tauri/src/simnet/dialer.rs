//! A `PeerDialer` whose success is governed entirely by the partition predicate.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::partition::Partition;
use crate::iroh_dial_driver::PeerDialer;
use crate::reconnect_supervisor::SupervisorHandle;

/// Shared `node_id -> supervisor handle` map. A successful dial marks the
/// *target's* inbound side Connected too, because a real transport dial
/// establishes a bidirectional link (the accepting peer sees an inbound accept).
pub(crate) type HandleRegistry = Arc<Mutex<HashMap<[u8; 32], SupervisorHandle>>>;

/// A `PeerDialer` whose success is governed entirely by the partition predicate.
/// Completion is synchronous (no awaited yield point), so concurrent dials from
/// one supervisor complete in a deterministic order.
pub(crate) struct SimDialer {
    self_id: [u8; 32],
    partition: Partition,
    fabric: HandleRegistry,
}

impl SimDialer {
    pub(crate) fn new(
        self_id: [u8; 32],
        partition: Partition,
        fabric: HandleRegistry,
    ) -> Arc<Self> {
        Arc::new(Self {
            self_id,
            partition,
            fabric,
        })
    }
}

#[async_trait::async_trait]
impl PeerDialer for SimDialer {
    async fn dial(&self, node_id: [u8; 32], _locator: String) -> bool {
        if !self.partition.same_side(self.self_id, node_id) {
            return false;
        }
        // Model the inbound accept: the dialed peer sees an inbound link from us.
        if let Some(h) = self.fabric.lock().expect("fabric lock").get(&node_id) {
            h.mark_connected(self.self_id);
        }
        true
    }
}

#[cfg(test)]
mod dialer_tests {
    use super::*;

    fn id(n: u8) -> [u8; 32] {
        [n; 32]
    }

    fn empty_fabric() -> HandleRegistry {
        Arc::new(Mutex::new(HashMap::new()))
    }

    #[tokio::test]
    async fn dial_succeeds_same_side_fails_across() {
        let partition = Partition::fully_connected();
        let dialer = SimDialer::new(id(1), partition.clone(), empty_fabric());
        assert!(
            dialer.dial(id(2), "iroh/x".into()).await,
            "connected -> dial ok"
        );

        partition.set_split(vec![vec![id(1)], vec![id(2)]]);
        assert!(
            !dialer.dial(id(2), "iroh/x".into()).await,
            "partitioned -> dial fails"
        );
    }
}
