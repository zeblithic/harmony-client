//! One SimNet node: a real `ReachabilityResolver` + a spawned reconnect
//! supervisor wired through a partition-gated `SimDialer`.

use std::sync::Arc;

use super::{
    dialer::{HandleRegistry, SimDialer},
    node_identity,
    partition::Partition,
};
use crate::network_health::DialTelemetry;
use crate::owner_state_types::OwnerAddr;
use crate::reachability_resolver::ReachabilityResolver;
use crate::reconnect_supervisor::{
    run_reconnect_supervisor, PeerStateWire, ReconnectTrigger, SupervisorConfig, SupervisorHandle,
};

/// One logical node: resolver + spawned supervisor + a stable identity.
pub(crate) struct SimNode {
    pub(crate) seed: u8,
    pub(crate) node_id: [u8; 32],
    pub(crate) owner: OwnerAddr,
    pub(crate) handle: SupervisorHandle,
    pub(crate) resolver: Arc<ReachabilityResolver>,
    _task: tokio::task::JoinHandle<()>,
}

impl SimNode {
    pub(crate) fn spawn(
        seed: u8,
        partition: &Partition,
        fabric: &HandleRegistry,
        config: SupervisorConfig,
    ) -> Self {
        let (node_id, owner) = node_identity(seed);
        let resolver = Arc::new(ReachabilityResolver::new());
        let handle = SupervisorHandle::new();
        resolver.set_supervisor(handle.clone());
        fabric
            .lock()
            .expect("fabric lock")
            .insert(node_id, handle.clone());
        let dialer = SimDialer::new(node_id, partition.clone(), Arc::clone(fabric));
        let telemetry = Arc::new(DialTelemetry::new());
        let task = tokio::spawn(run_reconnect_supervisor(
            handle.clone(),
            dialer,
            Arc::clone(&resolver),
            telemetry,
            node_id,
            config,
        ));
        Self {
            seed,
            node_id,
            owner,
            handle,
            resolver,
            _task: task,
        }
    }

    pub(crate) fn state_of(&self, peer: [u8; 32]) -> Option<PeerStateWire> {
        self.handle
            .states_snapshot()
            .into_iter()
            .find(|(id, _)| *id == peer)
            .map(|(_, st)| st)
    }

    pub(crate) fn kick(&self, peer: [u8; 32], trigger: ReconnectTrigger) {
        self.handle.kick(peer, trigger);
    }
}

#[cfg(test)]
mod node_tests {
    use super::*;
    use crate::simnet::{seed_record, SimClock};
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn two_nodes_connect_when_unpartitioned() {
        let clock = SimClock::new();
        let partition = Partition::fully_connected();
        let fabric: HandleRegistry = Arc::new(Mutex::new(HashMap::new()));
        let cfg = SupervisorConfig {
            jitter_seed: Some(0xC0FFEE),
            ..Default::default()
        };

        let a = SimNode::spawn(1, &partition, &fabric, cfg.clone());
        let b = SimNode::spawn(2, &partition, &fabric, cfg);

        // Both nodes learn each other's record and are told to connect (the
        // realistic bidirectional pattern SimNet uses): the lower-id node dials
        // first and the inbound-accept marks both ends Connected.
        let now = clock.now_ms();
        seed_record(&a.resolver, b.owner, b.node_id, now);
        seed_record(&b.resolver, a.owner, a.node_id, now);
        a.kick(b.node_id, ReconnectTrigger::NewPeer);
        b.kick(a.node_id, ReconnectTrigger::NewPeer);

        // Let the dial ladder run under virtual time. `sleep` (not `advance`) so
        // start_paused auto-advances through the supervisor's dial timer.
        tokio::time::sleep(Duration::from_secs(5)).await;
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }

        assert!(
            matches!(a.state_of(b.node_id), Some(PeerStateWire::Connected { .. })),
            "A should mark B Connected after a successful same-side dial, got {:?}",
            a.state_of(b.node_id)
        );
    }
}
