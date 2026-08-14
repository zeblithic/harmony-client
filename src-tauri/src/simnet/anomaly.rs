//! Convergence anomaly taxonomy for the SimNet CRDT plane. Pure data analysis
//! over per-round, per-node `Sample`s — mirrors the Freenet reference-review
//! anomaly classes (final divergence, stale peer, state oscillation) so a
//! failed reconvergence produces a diagnosis, not just a bare `assert_eq!`
//! mismatch.

/// One node's convergence fingerprint at one observation round.
/// `count` = event-log length (grow-only under a correct CRDT).
/// `digest` = order-independent hash of the event-id set (distinguishes two
/// logs of equal length but different content).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Sample {
    pub count: usize,
    pub digest: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Anomaly {
    /// A node's final digest differs from the leader's (the node with the most
    /// events in the final round). The terminal convergence failure.
    FinalDivergence {
        node: usize,
        count: usize,
        expected: usize,
    },
    /// A node's (count, digest) never changed across the window while the
    /// leader advanced past it — stuck behind, never caught up.
    StalePeer {
        node: usize,
        stuck_at: usize,
        leader_at: usize,
    },
    /// A node's event count DECREASED between consecutive rounds. A grow-only
    /// CRDT log must never shrink; any decrease is a bug.
    StateOscillation { node: usize, from: usize, to: usize },
}

/// Analyze a trajectory: `trajectory[round][node]`. Every row is expected to
/// have the same node count. Returns all anomalies found (empty == a healthy
/// convergence).
///
/// Divergence is flagged only on the FINAL row — divergence *during* a
/// partition is expected and healthy; only a divergent terminal state is an
/// anomaly. Oscillation is checked across every consecutive pair; stale is a
/// first-vs-last comparison.
pub(crate) fn analyze(trajectory: &[Vec<Sample>]) -> Vec<Anomaly> {
    let mut out = Vec::new();

    // The documented precondition: every row has the same node count. `zip` and
    // the index-bounded stale block would otherwise silently truncate a ragged
    // trajectory into a partial diagnosis. Test-only, so a debug assertion is
    // enough.
    if let Some(first) = trajectory.first() {
        debug_assert!(
            trajectory.iter().all(|row| row.len() == first.len()),
            "analyze requires a rectangular trajectory: {:?}",
            trajectory.iter().map(Vec::len).collect::<Vec<_>>()
        );
    }

    // Oscillation: a node's count decreased between consecutive rounds.
    for pair in trajectory.windows(2) {
        for (node, (prev, next)) in pair[0].iter().zip(pair[1].iter()).enumerate() {
            if next.count < prev.count {
                out.push(Anomaly::StateOscillation {
                    node,
                    from: prev.count,
                    to: next.count,
                });
            }
        }
    }

    // Final divergence: leader = node with max count in the final round (last
    // on ties, per `Iterator::max_by_key`); any node whose digest differs is
    // divergent. The tie-break only picks the reference digest — if any two
    // nodes disagree, at least one differs from whichever is chosen leader.
    if let Some(final_row) = trajectory.last() {
        if let Some(leader) = final_row.iter().max_by_key(|s| s.count).copied() {
            for (node, s) in final_row.iter().enumerate() {
                if s.digest != leader.digest {
                    out.push(Anomaly::FinalDivergence {
                        node,
                        count: s.count,
                        expected: leader.count,
                    });
                }
            }
        }
    }

    // Stale peer: unchanged first->last while the leader advanced past it.
    if let (Some(first), Some(last)) = (trajectory.first(), trajectory.last()) {
        let leader_at = last.iter().map(|s| s.count).max().unwrap_or(0);
        for node in 0..last.len() {
            let unchanged = first
                .get(node)
                .zip(last.get(node))
                .is_some_and(|(f, l)| f == l);
            if unchanged && last[node].count < leader_at {
                out.push(Anomaly::StalePeer {
                    node,
                    stuck_at: last[node].count,
                    leader_at,
                });
            }
        }
    }

    out
}

#[cfg(test)]
mod anomaly_tests {
    use super::*;

    fn row(pairs: &[(usize, u64)]) -> Vec<Sample> {
        pairs
            .iter()
            .map(|&(count, digest)| Sample { count, digest })
            .collect()
    }

    #[test]
    fn healthy_convergence_has_no_anomalies() {
        // All nodes grow 4->6->8 in lockstep with identical digests.
        let t = vec![
            row(&[(4, 0xA), (4, 0xA), (4, 0xA)]),
            row(&[(6, 0xB), (6, 0xB), (6, 0xB)]),
            row(&[(8, 0xC), (8, 0xC), (8, 0xC)]),
        ];
        assert_eq!(analyze(&t), vec![]);
    }

    #[test]
    fn expected_mid_partition_divergence_is_not_flagged() {
        // Middle row diverges (partition), final row reconverges. Healthy.
        let t = vec![
            row(&[(6, 0x1), (6, 0x1), (6, 0x1)]),
            row(&[(7, 0xAA), (7, 0xAA), (7, 0xBB)]), // island split — expected
            row(&[(8, 0xC), (8, 0xC), (8, 0xC)]),    // reconverged
        ];
        assert_eq!(analyze(&t), vec![]);
    }

    #[test]
    fn final_divergence_is_flagged() {
        // Node 2 ends with a different digest than the leader (nodes 0/1).
        let t = vec![
            row(&[(6, 0x1), (6, 0x1), (6, 0x1)]),
            row(&[(8, 0xC), (8, 0xC), (7, 0x77)]),
        ];
        let found = analyze(&t);
        assert!(
            found.contains(&Anomaly::FinalDivergence {
                node: 2,
                count: 7,
                expected: 8
            }),
            "expected FinalDivergence for node 2, got {found:?}"
        );
    }

    #[test]
    fn oscillation_is_flagged() {
        // Node 1's count drops 6 -> 5 (grow-only violated).
        let t = vec![row(&[(4, 0xA), (6, 0xB)]), row(&[(6, 0xC), (5, 0xD)])];
        let found = analyze(&t);
        assert!(
            found.contains(&Anomaly::StateOscillation {
                node: 1,
                from: 6,
                to: 5
            }),
            "expected StateOscillation for node 1, got {found:?}"
        );
    }

    #[test]
    fn stale_peer_is_flagged() {
        // Node 2 never advances (stuck at 6) while the leader reaches 8.
        let t = vec![
            row(&[(6, 0x1), (6, 0x1), (6, 0x9)]),
            row(&[(8, 0xC), (8, 0xC), (6, 0x9)]),
        ];
        let found = analyze(&t);
        assert!(
            found.contains(&Anomaly::StalePeer {
                node: 2,
                stuck_at: 6,
                leader_at: 8
            }),
            "expected StalePeer for node 2, got {found:?}"
        );
    }
}
