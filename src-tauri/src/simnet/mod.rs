//! SimNet — a deterministic, single-process, virtual-time simulation harness.
//!
//! Test-only. The design principle is **compose subsystems, don't boot nodes**:
//! `event_loop::run` is pinned to process-global singletons (one iroh/zenoh
//! transport ctx, one profile→data-dir, one advisory lock per process), so
//! running N real nodes in one process is impossible. But the connectivity
//! subsystems need none of those globals — SimNet composes N logical nodes,
//! each = { `ReachabilityResolver` + a spawned reconnect supervisor }, wired
//! through a partition-gated [`SimDialer`], all under
//! `#[tokio::test(start_paused = true)]` so advancing virtual time (see
//! [`SimNet::advance`]) drives every node's schedule coherently.
//!
//! PR1 (this module) covers the **connectivity plane** (R1 island repair:
//! Dormant→parole reconvergence). PR2 adds the **CRDT convergence plane**
//! (SimBus over the Sans-IO sync engines + an HLC clock seam + a convergence
//! oracle). See
//! `docs/superpowers/specs/2026-08-14-zeb917-r6c-deterministic-simulation-harness-design.md`.

mod clock;
mod dialer;
mod node;
mod partition;
mod tests;

pub(crate) use clock::SimClock;
pub(crate) use dialer::HandleRegistry;
pub(crate) use node::SimNode;
pub(crate) use partition::Partition;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::owner_state_types::{Hlc, OwnerAddr};
use crate::reachability_record::ReachabilityAnnouncePayload;
use crate::reachability_resolver::ReachabilityResolver;
use crate::reconnect_supervisor::{PeerStateWire, ReconnectTrigger, SupervisorConfig};

/// Deterministic `(node_id, owner_addr)` for a small integer seed, via the
/// production `NodeIdentity::from_seed` (same seed → byte-identical keys).
pub(crate) fn node_identity(seed: u8) -> ([u8; 32], OwnerAddr) {
    let ni = crate::identity::NodeIdentity::from_seed(&[seed; 32]);
    let node_id = *ni.ed25519.identity.verifying_key.as_bytes();
    let owner = OwnerAddr(ni.ed25519.identity.address_hash);
    (node_id, owner)
}

/// Seed a fresh, dialable routing record for `node_id` under `owner`.
pub(crate) fn seed_record(
    resolver: &ReachabilityResolver,
    owner: OwnerAddr,
    node_id: [u8; 32],
    now_ms: u64,
) {
    let payload = ReachabilityAnnouncePayload {
        iroh_node_id: node_id,
        home_relay_url: "https://derp.example/".into(),
        direct_addresses: vec![],
        announced_at_ms: now_ms,
        identity_signature: [0u8; 64],
        butler_set: Vec::new(),
        bs_at: 0,
    };
    let hlc = Hlc {
        wall_ms: now_ms,
        logical: 0,
        device_id: String::new(),
    };
    resolver.update(owner, payload, hlc);
}

/// N logical nodes over one shared partition + virtual clock.
pub(crate) struct SimNet {
    #[allow(dead_code)] // read at build; retained for PR2 (HLC-stamped seeding).
    clock: SimClock,
    partition: Partition,
    #[allow(dead_code)] // shared into every SimDialer; kept alive by SimNet.
    fabric: HandleRegistry,
    nodes: Vec<SimNode>,
}

impl SimNet {
    /// Spawn `n` nodes (seeds `1..=n`), give every node every *other* node's
    /// record, and tell each node to connect to all its peers.
    pub(crate) fn build(n: u8, config: SupervisorConfig) -> Self {
        let clock = SimClock::new();
        let now_fn = clock.as_now_fn();
        let partition = Partition::fully_connected();
        let fabric: HandleRegistry = Arc::new(Mutex::new(HashMap::new()));
        let nodes: Vec<SimNode> = (1..=n)
            .map(|s| SimNode::spawn(s, &partition, &fabric, &now_fn, config.clone()))
            .collect();

        let now = clock.now_ms();
        for a in &nodes {
            for b in &nodes {
                if a.node_id == b.node_id {
                    continue;
                }
                seed_record(&a.resolver, b.owner, b.node_id, now);
                a.kick(b.node_id, ReconnectTrigger::NewPeer);
            }
        }
        Self {
            clock,
            partition,
            fabric,
            nodes,
        }
    }

    pub(crate) fn node(&self, seed: u8) -> &SimNode {
        self.nodes
            .iter()
            .find(|nd| nd.seed == seed)
            .expect("seed exists")
    }

    /// Advance virtual time, letting spawned supervisors run. Under
    /// `start_paused`, `sleep().await` parks the test task so tokio auto-advances
    /// through each supervisor timer (arm → fire → dial → apply) — unlike
    /// `advance()`, which jumps the clock in one shot and can skip a timer that
    /// gets armed only after the jump.
    pub(crate) async fn advance(&self, d: Duration) {
        tokio::time::sleep(d).await;
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
    }

    /// Partition by seed-number groups, and drop every now-cross-partition known
    /// peer (models transport loss so severed peers re-enter the dial ladder).
    pub(crate) fn split(&self, groups: Vec<Vec<u8>>) {
        let id_groups: Vec<Vec<[u8; 32]>> = groups
            .iter()
            .map(|g| g.iter().map(|s| self.node(*s).node_id).collect())
            .collect();
        self.partition.set_split(id_groups);
        for a in &self.nodes {
            for b in &self.nodes {
                if a.node_id != b.node_id && !self.partition.same_side(a.node_id, b.node_id) {
                    a.kick(b.node_id, ReconnectTrigger::Dropped);
                }
            }
        }
    }

    pub(crate) fn heal(&self) {
        self.partition.heal();
    }

    /// True iff every *other* node is `Connected` in `seed`'s view.
    pub(crate) fn all_connected(&self, seed: u8) -> bool {
        let me = self.node(seed);
        self.nodes
            .iter()
            .filter(|nd| nd.node_id != me.node_id)
            .all(|peer| {
                matches!(
                    me.state_of(peer.node_id),
                    Some(PeerStateWire::Connected { .. })
                )
            })
    }
}

#[cfg(test)]
mod net_tests {
    use super::*;

    fn fast_cfg() -> SupervisorConfig {
        SupervisorConfig {
            retry_base: Duration::from_millis(500),
            retry_cap: Duration::from_secs(4),
            dormant_after: Duration::from_secs(10),
            parole_interval: Duration::from_secs(30),
            parole_batch: 8,
            jitter_seed: Some(0xC0FFEE),
            ..Default::default()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn all_nodes_connect_when_fully_connected() {
        let net = SimNet::build(6, fast_cfg());
        net.advance(Duration::from_secs(5)).await;
        for s in 1..=6u8 {
            assert!(
                net.all_connected(s),
                "node {s} should see all peers Connected"
            );
        }
    }
}
