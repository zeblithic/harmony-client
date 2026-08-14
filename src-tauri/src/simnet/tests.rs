//! R1 (ZEB-910) island-repair scenario: partition drives cross-island peers
//! Dormant; after heal, the supervisor's periodic *parole* tick revives them
//! into real dials and the mesh reconverges — no restart, no external churn.

#![cfg(test)]

use std::time::Duration;

use super::SimNet;
use crate::reconnect_supervisor::{PeerStateWire, SupervisorConfig};

fn r1_cfg() -> SupervisorConfig {
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
async fn simnet_r1_partition_heal_reconverges() {
    // 6 nodes, fully connected by ~t=5s (dormant_after 10s, parole every 30s).
    let net = SimNet::build(6, r1_cfg());
    net.advance(Duration::from_secs(5)).await;
    for s in 1..=6u8 {
        assert!(
            net.all_connected(s),
            "precondition: node {s} fully connected"
        );
    }

    // Partition 1-2-3 | 4-5-6 at ~t=5s. Cross pairs are severed (Dropped) and
    // cannot redial across the partition.
    net.split(vec![vec![1, 2, 3], vec![4, 5, 6]]);

    // Advance to ~t=20s: past dormant_after (severed peers Dormant by ~t=16s) but
    // before the first post-split parole revival (parole ticks at t=30s absolute).
    // This is a Dormant window, so the observation is not racing a parole revival.
    net.advance(Duration::from_secs(15)).await;

    let a = net.node(1);
    let far = net.node(4).node_id;
    assert!(
        matches!(a.state_of(far), Some(PeerStateWire::Dormant { .. })),
        "cross-island peer must be Dormant while partitioned, got {:?}",
        a.state_of(far)
    );
    // Intra-island peer stays Connected throughout.
    let near = net.node(2).node_id;
    assert!(
        matches!(a.state_of(near), Some(PeerStateWire::Connected { .. })),
        "intra-island peer stays Connected, got {:?}",
        a.state_of(near)
    );

    // Heal at ~t=20s while the cross peers are Dormant. Do NOT kick, re-seed, or
    // restart — a Dormant peer has no pending retry, so ONLY the periodic parole
    // tick (next at t=30s absolute) can revive it. Advance past that tick.
    net.heal();
    net.advance(Duration::from_secs(20)).await;

    for s in 1..=6u8 {
        assert!(
            net.all_connected(s),
            "node {s} must reconverge to fully-connected via parole alone"
        );
    }
}
