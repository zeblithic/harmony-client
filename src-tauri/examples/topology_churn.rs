//! ZEB-930 O1 — topology churn + reconvergence-sensitivity report.
//!
//! Neighbor-set churn under membership change for the R4 circulant vs the R3
//! ring vs pre-R4 full-mesh, plus a diameter + dial-concurrency cost model that
//! bounds reconvergence time. Deterministic (no wall-clock, no RNG). Run:
//!
//!     cd src-tauri && cargo run --example topology_churn
//!
//! Regenerate the findings-doc tables from this output.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use harmony_app::community_topology::{neighbors_on_ring, ring_order, FULL_MESH_THRESHOLD};

type Key = [u8; 32];
type Adj = BTreeMap<Key, BTreeSet<Key>>;

const SALT: &[u8] = b"zeb930-o1";

/// Distinct device key `i`; `ring_order` hashes it, so ring positions stay uniform.
fn dev(i: u64) -> Key {
    let mut k = [0u8; 32];
    k[..8].copy_from_slice(&i.to_be_bytes());
    k
}

fn base(n: usize) -> BTreeSet<Key> {
    (0..n as u64).map(dev).collect()
}

fn adj_r4(devices: &BTreeSet<Key>) -> Adj {
    let ring = ring_order(devices, SALT);
    ring.iter()
        .map(|d| (*d, neighbors_on_ring(&ring, d)))
        .collect()
}

fn adj_ring(devices: &BTreeSet<Key>) -> Adj {
    let ring = ring_order(devices, SALT);
    let n = ring.len();
    ring.iter()
        .enumerate()
        .map(|(r, d)| {
            let mut s = BTreeSet::new();
            if n >= 2 {
                s.insert(ring[(r + 1) % n]);
                s.insert(ring[(r + n - 1) % n]);
            }
            s.remove(d);
            (*d, s)
        })
        .collect()
}

fn adj_full(devices: &BTreeSet<Key>) -> Adj {
    devices
        .iter()
        .map(|d| (*d, devices.iter().copied().filter(|x| x != d).collect()))
        .collect()
}

fn edges(adj: &Adj) -> BTreeSet<(Key, Key)> {
    let mut e = BTreeSet::new();
    for (u, nbrs) in adj {
        for v in nbrs {
            e.insert(if u < v { (*u, *v) } else { (*v, *u) });
        }
    }
    e
}

struct Churn {
    edges: f64,
    affected: f64,
    max_adds: f64,
}

fn churn(before: &Adj, after: &Adj) -> Churn {
    let ec = edges(after).symmetric_difference(&edges(before)).count();
    let mut affected = 0usize;
    let mut max_adds = 0usize;
    for (k, b) in before {
        if let Some(a) = after.get(k) {
            if a != b {
                affected += 1;
            }
            max_adds = max_adds.max(a.difference(b).count());
        }
    }
    Churn {
        edges: ec as f64,
        affected: affected as f64,
        max_adds: max_adds as f64,
    }
}

/// Mean per-join churn over a deterministic batch of `joins` distinct new devices.
fn avg_join(n: usize, mk: fn(&BTreeSet<Key>) -> Adj, joins: u64) -> Churn {
    let before = base(n);
    let b = mk(&before);
    let (mut se, mut sa, mut sm) = (0f64, 0f64, 0f64);
    for j in 0..joins {
        let mut after = before.clone();
        after.insert(dev(1_000_000 + j));
        let c = churn(&b, &mk(&after));
        se += c.edges;
        sa += c.affected;
        sm += c.max_adds;
    }
    let k = joins as f64;
    Churn {
        edges: se / k,
        affected: sa / k,
        max_adds: sm / k,
    }
}

/// Unweighted graph diameter (max eccentricity) via BFS from every node.
fn diameter(adj: &Adj) -> usize {
    let mut best = 0usize;
    for src in adj.keys() {
        let mut dist: BTreeMap<Key, usize> = BTreeMap::new();
        dist.insert(*src, 0);
        let mut q = VecDeque::from([*src]);
        while let Some(u) = q.pop_front() {
            let du = dist[&u];
            best = best.max(du);
            for v in &adj[&u] {
                if !dist.contains_key(v) {
                    dist.insert(*v, du + 1);
                    q.push_back(*v);
                }
            }
        }
    }
    best
}

fn main() {
    // Self-check: R4 degree at N=200 is 14 (Part-1 T1). Validates the harness.
    let ring200 = ring_order(&base(200), SALT);
    assert_eq!(
        neighbors_on_ring(&ring200, &ring200[0]).len(),
        14,
        "R4 degree at N=200 must be 14 (Part-1 T1)"
    );

    let sizes = [32usize, 50, 64, 100, 128, 200];
    let joins = 64u64;

    println!("# ZEB-930 O1 — churn per join (mean over {joins} joiners)");
    println!("# FULL_MESH_THRESHOLD = {FULL_MESH_THRESHOLD}");
    println!("# columns: edg = community edge churn; aff = existing nodes re-dialing; maxA = worst node's new dials");
    println!();
    println!(
        "{:>5} | {:>8} {:>6} {:>6} | {:>8} {:>6} {:>6} | {:>8} {:>6} {:>6}",
        "N", "R4 edg", "aff", "maxA", "ring edg", "aff", "maxA", "mesh edg", "aff", "maxA"
    );
    for &n in &sizes {
        let r = avg_join(n, adj_r4, joins);
        let g = avg_join(n, adj_ring, joins);
        let f = avg_join(n, adj_full, joins);
        println!(
            "{:>5} | {:>8.0} {:>6.0} {:>6.0} | {:>8.0} {:>6.0} {:>6.0} | {:>8.0} {:>6.0} {:>6.0}",
            n,
            r.edges,
            r.affected,
            r.max_adds,
            g.edges,
            g.affected,
            g.max_adds,
            f.edges,
            f.affected,
            f.max_adds
        );
    }

    println!();
    println!("# Layer 2 — reconvergence-time band  T = max(D*L, ceil(maxAdds/C)*d),  C=4 dials");
    let hop_ms = [10.0f64, 40.0, 80.0];
    let link_ms = [300.0f64, 800.0, 1500.0];
    for &n in &[50usize, 200] {
        let d_r4 = diameter(&adj_r4(&base(n)));
        let d_rg = diameter(&adj_ring(&base(n)));
        let a_r4 = avg_join(n, adj_r4, joins).max_adds;
        let a_rg = avg_join(n, adj_ring, joins).max_adds;
        println!();
        println!("N={n}: diameter R4={d_r4} ring={d_rg}; maxAdds R4~{a_r4:.0} ring~{a_rg:.0}");
        for &l in &hop_ms {
            for &d in &link_ms {
                let t = |dia: usize, adds: f64| {
                    let flood = dia as f64 * l;
                    let storm = (adds / 4.0).ceil() * d;
                    flood.max(storm)
                };
                println!(
                    "  L={l:>4.0}ms d={d:>6.0}ms   R4~{:>8.0}ms   ring~{:>8.0}ms",
                    t(d_r4, a_r4),
                    t(d_rg, a_rg)
                );
            }
        }
    }
}
