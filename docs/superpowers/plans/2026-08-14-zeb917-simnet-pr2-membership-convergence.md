# SimNet PR2 — Membership CRDT Convergence Plane Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extend the SimNet harness (PR1) with a virtual-time, single-process, N-node **membership CRDT convergence plane**: compose N `CommunitySyncEngine`s over a partitionable in-memory SimBus, drive a partition → divergent-mutations → heal → reconverge cycle, and assert all nodes converge to an identical `CommunityState` — plus an anomaly analyzer that classifies convergence failures.

**Architecture:** Compose subsystems, don't boot nodes (the PR1 principle). Each logical node = one `CommunitySyncEngine` + its `Arc<Mutex<CommunityState>>` + replay tracker, wired through per-node `publisher_tx`/`subscriber_rx` mpsc pairs. A `SimBus` owns per-source drainer tasks that forward published bytes to every *same-partition* peer's subscriber sink (reusing PR1's `Partition` predicate, keyed by a synthetic per-node `[u8;32]` tag). All engines share one in-memory content store (CAS), so the wire carries only a CID and heal-time re-publishes reconstruct state from the shared blob store. Convergence is driven explicitly by `flush_now()` (the pub/sub plane has no anti-entropy — see Global Constraints), and the oracle is `CommunityState`'s exact-event-set `PartialEq`.

**Tech Stack:** Rust, `tokio` (`#[tokio::test(start_paused = true)]` virtual time), `mpsc` channels, the existing `CommunitySyncEngine` sans-IO CRDT engine, `mint_*` test fixtures.

## Global Constraints

- **ZERO production changes.** PR2 is pure test infrastructure. It adds files under `src-tauri/src/simnet/` (all `#[cfg(test)]`) and touches only `src-tauri/src/simnet/mod.rs` (module declarations). It must NOT edit any non-test production code. If any task appears to require a production edit, STOP — that belongs to the deferred HLC-seam increment, not PR2.
- **In-crate module, not `tests/`.** PR2 lives in the existing `#[cfg(test)] mod simnet` tree. This is load-bearing: `mint_test_owner` is `#[cfg(any(test, feature = "test-fixtures"))]` and is reachable from in-crate `#[cfg(test)]` code directly, whereas a `tests/` integration crate only sees it via `--features test-fixtures`. Keeping the whole SimNet harness in one module matches PR1.
- **No anti-entropy on the pub/sub plane.** The `CommunitySyncEngine` `publisher_tx`/`subscriber_rx` plane has no engine-internal periodic re-publish. A publish that is dropped downstream leaves the sender neither dirty nor retry-armed. **Post-heal convergence MUST be driven by an explicit `flush_now()`** on each node that mutated during the partition (it re-ships the full state root unconditionally; the shared CAS still holds every blob). Advancing virtual time alone will NOT reconverge a dropped-bytes partition. Do not "fix" this by wiring the zenoh catch-up plane — that is out of scope (a future ticket).
- **Shared CAS is global (not partitioned).** All N engines share ONE in-memory content-store servicer. The partition seam is the SimBus byte-forwarding, not the CAS. This models real transport loss (packets dropped) while blobs remain fetchable once a CID is re-advertised post-heal.
- **Determinism is at the `CommunityState` level, not wire-byte level.** Event HLCs come from deterministic `mint_*` helpers; publish-envelope HLCs use host `SystemTime::now()` (harmless — always sort after the tiny mint HLCs, so the membership gate always passes). The oracle asserts `CommunityState` equality, which is deterministic. Full wire-replay determinism awaits the deferred HLC seam and is explicitly NOT a PR2 goal.
- **Cargo commands run from `src-tauri/`.** Always `--locked` and `--features test-fixtures` (CI parity).
- **Commit trailers** (every commit), exactly:
  ```
  Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01DUvg7gyEHqXrVU85Eg2D8D
  ```

---

## Scope & Boundaries

**In scope (this plan):** SimBus (partition-gated broadcast), the N-node `SimCommunity` builder (shared CAS, mint ceremony, O(N²) Join cross-seed, baseline convergence), the anomaly analyzer (`FinalDivergence` / `StalePeer` / `StateOscillation` over per-node `(count, digest)` samples), and the membership partition/heal convergence test.

**Explicitly deferred (NOT this plan):**
- Channel-log RBSR convergence plane → PR3 (may need a new Linear ticket, filed via describe→get-ID).
- The HLC seam (`NowFn` threaded into the channel-log engine + owner-state `receiver_now`) → later increment; invasive, security-sensitive (ZEB-831 `clock_trust` contracts), and buys only bit-replay which reconvergence does not need.
- Wiring the zenoh catch-up plane (`root_serve_rx` / fetch driver) into SimNet.

---

## File Structure

- **Create `src-tauri/src/simnet/bus.rs`** — `SimBus`: partition-gated broadcast forwarders over raw `Vec<u8>` frames. One drainer task per source; `impl Drop` aborts them (mirrors `SimNode`). Reuses `Partition`. Self-contained; unit-tested with raw channels (no engine).
- **Create `src-tauri/src/simnet/anomaly.rs`** — `Sample { count, digest }`, the `Anomaly` enum, and the pure `analyze(&[Vec<Sample>]) -> Vec<Anomaly>` function. Unit-tested with synthetic trajectories. Zero dependency on the engine — a plain data analyzer.
- **Create `src-tauri/src/simnet/community.rs`** — `SimIdentityResolver`, `spawn_shared_cas`, `SimCommunityNode`, `SimCommunity` (builder + convergence helpers), and the membership partition/heal test + baseline test in its `#[cfg(test)]` submodule.
- **Modify `src-tauri/src/simnet/mod.rs`** — add `mod bus;`, `mod anomaly;`, `mod community;` and the `pub(crate) use` re-exports the tasks below reference.

---

## Task 1: SimBus — partition-gated broadcast forwarders

**Files:**
- Create: `src-tauri/src/simnet/bus.rs`
- Modify: `src-tauri/src/simnet/mod.rs` (add `mod bus;` and `pub(crate) use bus::SimBus;`)

**Interfaces:**
- Consumes: `crate::simnet::partition::Partition` and its `fn same_side(&self, a: [u8; 32], b: [u8; 32]) -> bool`, `fn set_split(&self, groups: Vec<Vec<[u8; 32]>>)`, `fn heal(&self)`, `fn fully_connected() -> Self` (all from PR1's `partition.rs`).
- Produces:
  ```rust
  pub(crate) struct SimBus { /* drainer JoinHandles */ }
  impl SimBus {
      /// Spawn one drainer per source. `sources[i]` is node i's publisher
      /// receiver (the far end of its engine's `publisher_tx`); `sinks[i]`
      /// is node i's subscriber sender (feeds its engine's `subscriber_rx`);
      /// `tags[i]` is node i's partition key. A frame drained from source i
      /// is delivered to every sink j != i for which
      /// `partition.same_side(tags[i], tags[j])` holds AT DELIVERY TIME.
      pub(crate) fn spawn(
          sources: Vec<tokio::sync::mpsc::Receiver<Vec<u8>>>,
          sinks: Vec<tokio::sync::mpsc::Sender<Vec<u8>>>,
          tags: Vec<[u8; 32]>,
          partition: Partition,
      ) -> Self;
  }
  ```

- [ ] **Step 1: Write the failing test.**

Create `src-tauri/src/simnet/bus.rs` with only the test module (no impl yet):

```rust
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

#[cfg(test)]
mod bus_tests {
    use super::*;

    /// Yield the runtime enough times for the spawned drainers to forward,
    /// then non-blocking `try_recv`. Deterministic: no real sleep, and under
    /// a single-threaded runtime the drainer runs to its next `.await` point
    /// on each yield.
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

        // Partition {0} | {1,2}: node 0's frame is dropped for 1 and 2.
        partition.set_split(vec![vec![[1u8; 32]], vec![[2u8; 32], [3u8; 32]]]);
        o0.send(b"drop".to_vec()).await.unwrap();
        assert_eq!(drained(&mut i1r).await, None, "cross-partition frame must drop");
        assert_eq!(drained(&mut i2r).await, None, "cross-partition frame must drop");

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
```

- [ ] **Step 2: Run the test to confirm it fails to compile (`SimBus` not defined).**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(bus_gates_delivery_by_partition)'`
Expected: compile error — `cannot find type/struct SimBus`.

- [ ] **Step 3: Implement `SimBus`.**

Add above the test module in `bus.rs`:

```rust
/// A partition-gated broadcast fabric. Holds one drainer task per source;
/// dropping the bus aborts them so they cannot outlive the sim (mirrors
/// `SimNode`'s Drop guard).
pub(crate) struct SimBus {
    drainers: Vec<tokio::task::JoinHandle<()>>,
}

impl SimBus {
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
                            // Evaluated per-frame so a mid-run split/heal
                            // takes effect immediately.
                            if partition.same_side(src_tag, tags[j]) {
                                let _ = sink.send(bytes.clone()).await;
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
```

- [ ] **Step 4: Wire the module.**

In `src-tauri/src/simnet/mod.rs`, add `mod bus;` alongside the existing `mod` lines and `pub(crate) use bus::SimBus;` alongside the existing re-exports.

- [ ] **Step 5: Run the test to confirm it passes.**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(bus_gates_delivery_by_partition)'`
Expected: PASS.

- [ ] **Step 6: Lint + format the new file.**

Run: `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && cargo fmt --all`
Expected: clean (no warnings; fmt makes no further changes after you re-check).

- [ ] **Step 7: Commit.**

```bash
git add src-tauri/src/simnet/bus.rs src-tauri/src/simnet/mod.rs
git commit -m "$(cat <<'EOF'
ZEB-917 PR2: SimBus — partition-gated broadcast fabric for the CRDT plane

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01DUvg7gyEHqXrVU85Eg2D8D
EOF
)"
```

---

## Task 2: Anomaly analyzer

**Files:**
- Create: `src-tauri/src/simnet/anomaly.rs`
- Modify: `src-tauri/src/simnet/mod.rs` (add `mod anomaly;` and `pub(crate) use anomaly::{analyze, Anomaly, Sample};`)

**Interfaces:**
- Consumes: nothing (pure data analyzer).
- Produces:
  ```rust
  /// One node's convergence fingerprint at one observation round.
  /// `count` = event-log length (grow-only under a correct CRDT).
  /// `digest` = order-independent hash of the event-id set (distinguishes
  /// two logs of equal length but different content).
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub(crate) struct Sample { pub count: usize, pub digest: u64 }

  #[derive(Debug, Clone, PartialEq, Eq)]
  pub(crate) enum Anomaly {
      /// A node's final digest differs from the leader's (the node with the
      /// most events in the final round). The terminal convergence failure.
      FinalDivergence { node: usize, count: usize, expected: usize },
      /// A node's (count, digest) never changed across the window while the
      /// leader advanced past it — stuck behind, never caught up.
      StalePeer { node: usize, stuck_at: usize, leader_at: usize },
      /// A node's event count DECREASED between consecutive rounds. A
      /// grow-only CRDT log must never shrink; any decrease is a bug.
      StateOscillation { node: usize, from: usize, to: usize },
  }

  /// Analyze a trajectory: `trajectory[round][node]`. Every row must have the
  /// same node count. Returns all anomalies found (empty == healthy converge).
  pub(crate) fn analyze(trajectory: &[Vec<Sample>]) -> Vec<Anomaly>;
  ```

**Interface note:** `analyze` deliberately flags divergence only on the FINAL row — divergence *during* a partition is expected and healthy; only a divergent terminal state is an anomaly. Oscillation is checked across all consecutive rounds. Stale is a first-vs-last comparison.

- [ ] **Step 1: Write the failing tests.**

Create `src-tauri/src/simnet/anomaly.rs` with only the test module:

```rust
//! Convergence anomaly taxonomy for the SimNet CRDT plane. Pure data
//! analysis over per-round, per-node `Sample`s — mirrors the Freenet
//! reference-review anomaly classes (final divergence, stale peer, state
//! oscillation) so a failed reconvergence produces a diagnosis, not just a
//! bare `assert_eq!` mismatch.

#[cfg(test)]
mod anomaly_tests {
    use super::*;

    fn row(pairs: &[(usize, u64)]) -> Vec<Sample> {
        pairs.iter().map(|&(count, digest)| Sample { count, digest }).collect()
    }

    #[tokio::test]
    async fn healthy_convergence_has_no_anomalies() {
        // All nodes grow 4->6->8 in lockstep with identical digests.
        let t = vec![
            row(&[(4, 0xA), (4, 0xA), (4, 0xA)]),
            row(&[(6, 0xB), (6, 0xB), (6, 0xB)]),
            row(&[(8, 0xC), (8, 0xC), (8, 0xC)]),
        ];
        assert_eq!(analyze(&t), vec![]);
    }

    #[tokio::test]
    async fn expected_mid_partition_divergence_is_not_flagged() {
        // Middle row diverges (partition), final row reconverges. Healthy.
        let t = vec![
            row(&[(6, 0x1), (6, 0x1), (6, 0x1)]),
            row(&[(7, 0xAA), (7, 0xAA), (7, 0xBB)]), // island split — expected
            row(&[(8, 0xC), (8, 0xC), (8, 0xC)]),    // reconverged
        ];
        assert_eq!(analyze(&t), vec![]);
    }

    #[tokio::test]
    async fn final_divergence_is_flagged() {
        // Node 2 ends with a different digest than the leader (nodes 0/1).
        let t = vec![
            row(&[(6, 0x1), (6, 0x1), (6, 0x1)]),
            row(&[(8, 0xC), (8, 0xC), (7, 0xZZ_u64)]),
        ];
        let found = analyze(&t);
        assert!(
            found.contains(&Anomaly::FinalDivergence { node: 2, count: 7, expected: 8 }),
            "expected FinalDivergence for node 2, got {found:?}"
        );
    }

    #[tokio::test]
    async fn oscillation_is_flagged() {
        // Node 1's count drops 6 -> 5 (grow-only violated).
        let t = vec![
            row(&[(4, 0xA), (6, 0xB)]),
            row(&[(6, 0xC), (5, 0xD)]),
        ];
        let found = analyze(&t);
        assert!(
            found.contains(&Anomaly::StateOscillation { node: 1, from: 6, to: 5 }),
            "expected StateOscillation for node 1, got {found:?}"
        );
    }

    #[tokio::test]
    async fn stale_peer_is_flagged() {
        // Node 2 never advances (stuck at 6) while the leader reaches 8.
        let t = vec![
            row(&[(6, 0x1), (6, 0x1), (6, 0x9)]),
            row(&[(8, 0xC), (8, 0xC), (6, 0x9)]),
        ];
        let found = analyze(&t);
        assert!(
            found.contains(&Anomaly::StalePeer { node: 2, stuck_at: 6, leader_at: 8 }),
            "expected StalePeer for node 2, got {found:?}"
        );
    }
}
```

- [ ] **Step 2: Run the tests to confirm they fail to compile.**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(anomaly_tests)'`
Expected: compile error — `analyze`, `Anomaly`, `Sample` not defined.

- [ ] **Step 3: Implement the analyzer.**

Add above the test module in `anomaly.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Sample {
    pub count: usize,
    pub digest: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Anomaly {
    FinalDivergence { node: usize, count: usize, expected: usize },
    StalePeer { node: usize, stuck_at: usize, leader_at: usize },
    StateOscillation { node: usize, from: usize, to: usize },
}

pub(crate) fn analyze(trajectory: &[Vec<Sample>]) -> Vec<Anomaly> {
    let mut out = Vec::new();

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

    // Final divergence: leader = node with max count in the final round
    // (first on ties); any node whose digest differs is divergent.
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
```

- [ ] **Step 4: Wire the module.**

In `src-tauri/src/simnet/mod.rs`, add `mod anomaly;` and `pub(crate) use anomaly::{analyze, Anomaly, Sample};`.

- [ ] **Step 5: Run the tests to confirm they pass.**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(anomaly_tests)'`
Expected: all 5 PASS.

- [ ] **Step 6: Lint + format.**

Run: `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && cargo fmt --all`
Expected: clean.

- [ ] **Step 7: Commit.**

```bash
git add src-tauri/src/simnet/anomaly.rs src-tauri/src/simnet/mod.rs
git commit -m "$(cat <<'EOF'
ZEB-917 PR2: SimNet convergence anomaly analyzer (divergence/stale/oscillation)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01DUvg7gyEHqXrVU85Eg2D8D
EOF
)"
```

---

## Task 3: SimCommunity — N-node builder + baseline convergence

**Files:**
- Create: `src-tauri/src/simnet/community.rs`
- Modify: `src-tauri/src/simnet/mod.rs` (add `mod community;` — no re-export needed; the tests live inside `community.rs`)

**Interfaces:**
- Consumes: `SimBus` (Task 1); `crate::simnet::partition::Partition`; and the following production/library symbols (all reachable in-crate):
  - `crate::community_membership::{mint_test_owner, TestOwner, materialize, MaterializedMembership, MemberStatus, SignedMembershipEvent}`
  - `crate::community_state_crdt::{CommunityState, InsertOutcome}`
  - `crate::community_state_sync::{CommunitySyncEngine, CommunitySyncEngineConfig, CommunityReplayTracker, IdentityResolver, PersistPaths, DEFAULT_DEBOUNCE_MS}`
  - `crate::content_store::{CasOp, ContentStore, RuntimeContentStore}`
  - `crate::hlc_adopt_floor::HlcAdoptFloor`
  - `crate::community_invite::{CommunityInvitePayload, InviteEpochSnapshot, MaterializedCommunityState}`
  - `crate::owner_state_types::{Hlc, OwnerAddr, SpaceId}`
  - `crate::mint_community_creation`, `crate::mint_redemption` (both `pub fn` in `lib.rs`)
- Produces (used by Task 4):
  ```rust
  pub(crate) struct SimCommunityNode {
      pub index: usize,          // 1-based
      pub owner: OwnerAddr,
      pub device_id: String,     // "n{index}-dev"
      pub signing_key: Arc<ed25519_dalek::SigningKey>,
      pub state: Arc<tokio::sync::Mutex<CommunityState>>,
      pub engine: CommunitySyncEngine,
      pub tag: [u8; 32],         // [index as u8; 32]
      pub join_hlc: Hlc,         // this node's bootstrap Join HLC
  }

  pub(crate) struct SimCommunity {
      pub community_id: SpaceId,
      pub admin_owner: OwnerAddr,
      partition: Partition,
      _bus: SimBus,              // kept alive; drop aborts drainers
      _cas_tx: mpsc::Sender<CasOp>, // kept alive; drop stops the servicer
      _tmpdirs: Vec<tempfile::TempDir>,
      nodes: Vec<SimCommunityNode>,
  }
  impl SimCommunity {
      /// Build N nodes (index 1..=n): node 1 mints an OPEN community + its
      /// bootstrap Join; nodes 2..=n redeem an open invite. Every node then
      /// insert-locals every OTHER node's bootstrap Join (O(N^2) cross-seed
      /// for the membership-at-HLC gate). Returns with all nodes holding all
      /// N Joins (baseline convergence achieved with NO bus traffic).
      pub(crate) async fn build(n: u8) -> Self;

      pub(crate) fn node(&self, index: usize) -> &SimCommunityNode;
      pub(crate) fn split(&self, groups: Vec<Vec<usize>>); // 1-based indices
      pub(crate) fn heal(&self);
      pub(crate) async fn advance(&self, d: std::time::Duration); // sleep + yields
      pub(crate) async fn counts(&self) -> Vec<usize>;            // per node event_count
      pub(crate) async fn sample(&self) -> Vec<Sample>;           // per node (count, digest)
      pub(crate) async fn all_states_equal(&self) -> bool;        // pairwise CommunityState ==
  }
  ```

**Design notes for the implementer:**
- **Open community.** Build with `is_invite_only: false`. Node 1 is admin; nodes 2..=n redeem an *open* invite (`is_invite_only: false` in the payload, `admin_addr = node 1`). This mirrors `task3_kick_setpower_round_trip::build_fixture` exactly (a proven template — read `tests/community_sync/community_sync_integration.rs:2248-2512`).
- **Identity resolver.** A trivial `SimIdentityResolver` returning `Some([0u8; 64])` for any address. This matches the fixtures, which pass dummy `[0u8;64]` pubkeys and satisfy verification from the `EnrollmentCert` carried on each Join event — the resolver's return value is never the deciding verifier.
- **Shared CAS.** One `spawn_shared_cas()` servicer + one `cas_tx`; every node gets its own `RuntimeContentStore::new(cas_tx.clone(), Duration::from_secs(2))`. The servicer is the exact 4-arm `match` from the fixture (`PutLocal`/`GetOrFetch`/`GetLocal`/`AllowServeSubtree`).
- **HLCs.** Creation Join at `wall_ms: 100_000`; each redemption Join at `wall_ms: 200_000 + index` (distinct, all after creation). Mutation HLCs (Task 4) use `wall_ms >= 300_000`, strictly after every setup HLC and authored by a device with exactly one prior event (its own Join) — so monotonicity holds by construction, no `reserve_next_hlc_for_device` ceremony needed.
- **`digest`.** Order-independent hash of the event-id set: fold each event's id into a `u64` (e.g. XOR of per-id `DefaultHasher` outputs) so two logs with identical event sets in any insertion order produce the same digest. `SignedMembershipEvent` exposes its event id — inspect the type to use the right accessor (grep `event_id` on `SignedMembershipEvent`).
- **`build` performs NO bus traffic.** Cross-seed is by direct `insert_local_event`. Baseline convergence (all N Joins on every node) is therefore synchronous — assert it immediately after `build`, before any `advance`.

- [ ] **Step 1: Write the failing baseline test.**

At the bottom of `community.rs`, add:

```rust
#[cfg(test)]
mod community_tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn baseline_all_nodes_hold_all_joins() {
        let c = SimCommunity::build(4).await;
        // Every node insert-locals its own Join + the other 3 -> 4 events each.
        let counts = c.counts().await;
        assert_eq!(counts, vec![4, 4, 4, 4], "each node holds all 4 bootstrap Joins");
        assert!(
            c.all_states_equal().await,
            "all nodes must share an identical baseline CommunityState"
        );
        // Sanity: no bus advance was needed for baseline.
        let _ = Duration::from_secs(0);
    }
}
```

- [ ] **Step 2: Run to confirm it fails to compile.**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(baseline_all_nodes_hold_all_joins)'`
Expected: compile error — `SimCommunity` not defined.

- [ ] **Step 3: Implement the resolver + shared-CAS helper.**

At the top of `community.rs` (module body), add the imports listed under **Interfaces → Consumes**, then:

```rust
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

use super::anomaly::Sample;
use super::bus::SimBus;
use super::partition::Partition;

/// Dummy resolver: verification is satisfied from each Join's EnrollmentCert,
/// so a constant pubkey is never the deciding verifier (matches the 2-node
/// fixtures, which pass `[0u8; 64]`).
struct SimIdentityResolver;

#[async_trait::async_trait]
impl IdentityResolver for SimIdentityResolver {
    async fn resolve(&self, _addr: &OwnerAddr) -> Option<[u8; 64]> {
        Some([0u8; 64])
    }
}

/// One shared in-memory CAS servicer for all engines. Returns the op sender;
/// each node wraps its own clone in a `RuntimeContentStore`.
fn spawn_shared_cas() -> mpsc::Sender<CasOp> {
    let cas: Arc<Mutex<std::collections::HashMap<harmony_content::cid::ContentId, Vec<u8>>>> =
        Arc::new(Mutex::new(std::collections::HashMap::new()));
    let (cas_tx, mut cas_rx) = mpsc::channel::<CasOp>(256);
    tokio::spawn(async move {
        while let Some(op) = cas_rx.recv().await {
            match op {
                CasOp::PutLocal { cid, blob, reply, .. } => {
                    cas.lock().await.insert(cid, blob);
                    if let Some(r) = reply {
                        let _ = r.send(Ok(()));
                    }
                }
                CasOp::GetOrFetch { cid, timeout: _, reply } => {
                    let v = cas.lock().await.get(&cid).cloned();
                    let _ = reply.send(Ok(v));
                }
                CasOp::GetLocal { cid, reply } => {
                    let v = cas.lock().await.get(&cid).cloned();
                    let _ = reply.send(v);
                }
                CasOp::AllowServeSubtree { reply, .. } => {
                    let _ = reply.send(Ok(0));
                }
            }
        }
    });
    cas_tx
}
```

**Note:** Confirm the `CasOp` variant field names against `src/content_store.rs:245` (the fixture at `community_sync_integration.rs:2270` is the authoritative shape — copy it verbatim). If `harmony_content::cid::ContentId` is not the exact path, grep `ContentId` imports in that fixture.

- [ ] **Step 4: Implement `SimCommunity::build` and helpers.**

Add the `SimCommunityNode` / `SimCommunity` structs (signatures under **Interfaces → Produces**) and:

```rust
impl SimCommunity {
    pub(crate) async fn build(n: u8) -> Self {
        assert!((2..=12).contains(&n), "SimCommunity supports 2..=12 nodes");
        let resolver: Arc<dyn IdentityResolver> = Arc::new(SimIdentityResolver);
        let cas_tx = spawn_shared_cas();

        // Per-node identities (seed = index; avoid seed^0xFF collisions per
        // mint_test_owner's doc by staying in 1..=12).
        struct Ident {
            owner: OwnerAddr,
            signing: Arc<ed25519_dalek::SigningKey>,
            identity: TestOwner,
            device_id: String,
            tag: [u8; 32],
        }
        let idents: Vec<Ident> = (1..=n)
            .map(|i| {
                let identity = mint_test_owner(i);
                Ident {
                    owner: identity.owner,
                    signing: Arc::new(identity.device_key.clone()),
                    identity,
                    device_id: format!("n{i}-dev"),
                    tag: [i; 32],
                }
            })
            .collect();

        // Node 1 mints the OPEN community + bootstrap Join.
        let admin = &idents[0];
        let minted_admin = crate::mint_community_creation(
            "SimCommunity",
            false,
            admin.owner,
            &admin.signing,
            &admin.identity.cert,
            Hlc { wall_ms: 100_000, logical: 0, device_id: admin.device_id.clone() },
        )
        .expect("mint create");
        let community_id = minted_admin.community_id;
        let membership_key = minted_admin.membership_key.clone();

        // Bootstrap Join per node: admin's is `minted_admin.bootstrap_join`;
        // each other node redeems an open invite.
        let mut bootstrap_joins: Vec<SignedMembershipEvent> =
            Vec::with_capacity(n as usize);
        bootstrap_joins.push(minted_admin.bootstrap_join.clone());
        for (offset, id) in idents.iter().enumerate().skip(1) {
            let invite = CommunityInvitePayload {
                inviter_signer_certs: Vec::new(),
                community_id,
                epoch_snapshot: InviteEpochSnapshot {
                    epoch: 0,
                    sealed_epoch_key: membership_key.as_bytes().to_vec(),
                    sealed_epoch_keys: Vec::new(),
                    state_snapshot: MaterializedCommunityState::default(),
                },
                admin_addr: admin.owner,
                community_name: "SimCommunity".into(),
                is_invite_only: false,
                expires_at: None,
                invite_token: None,
                admin_bootstrap: None,
                admin_identity_pub: None,
                forked_from: None,
                pre_fork_snapshot: None,
                inviter_enrollment: None,
                untargeted_decrypt_key: None,
            };
            let minted = crate::mint_redemption(
                &invite,
                id.owner,
                &id.signing,
                &id.identity.cert,
                Hlc {
                    wall_ms: 200_000 + offset as u64,
                    logical: 0,
                    device_id: id.device_id.clone(),
                },
            )
            .expect("mint redeem");
            bootstrap_joins.push(minted.bootstrap_join.clone());
        }

        // Per-node channels: engine.publisher_tx = out_tx; bus drains out_rx.
        //                    bus delivers to in_tx; engine.subscriber_rx = in_rx.
        let mut out_txs = Vec::new();
        let mut out_rxs = Vec::new();
        let mut in_txs = Vec::new();
        let mut in_rxs = Vec::new();
        for _ in 0..n {
            let (o_tx, o_rx) = mpsc::channel::<Vec<u8>>(256);
            let (i_tx, i_rx) = mpsc::channel::<Vec<u8>>(256);
            out_txs.push(o_tx);
            out_rxs.push(o_rx);
            in_txs.push(i_tx);
            in_rxs.push(i_rx);
        }

        // Build engines.
        let mut tmpdirs = Vec::new();
        let mut nodes = Vec::new();
        let mut in_rxs_iter = in_rxs.into_iter();
        let mut out_txs_iter = out_txs.into_iter();
        for (idx0, id) in idents.iter().enumerate() {
            let state = Arc::new(Mutex::new(CommunityState::new(community_id)));
            let tracker = Arc::new(Mutex::new(CommunityReplayTracker::new((
                id.owner,
                id.device_id.clone(),
            ))));
            let tmp = tempfile::tempdir().expect("tmp");
            let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
                cas_tx.clone(),
                std::time::Duration::from_secs(2),
            ));
            let engine = CommunitySyncEngine::new(CommunitySyncEngineConfig {
                adopt_floor: HlcAdoptFloor::new(),
                community_id,
                membership_key: membership_key.clone(),
                admin_addr: admin.owner,
                is_invite_only: false,
                device_id: id.device_id.clone(),
                self_owner: id.owner,
                signing_key: Arc::clone(&id.signing),
                state: Arc::clone(&state),
                tracker,
                content_store: cs,
                publisher_tx: out_txs_iter.next().unwrap(),
                subscriber_rx: in_rxs_iter.next().unwrap(),
                paths: PersistPaths {
                    crdt: tmp.path().join("crdt.cbor"),
                    replay: tmp.path().join("replay.cbor"),
                },
                debounce_ms: DEFAULT_DEBOUNCE_MS,
                identity_resolver: Some(Arc::clone(&resolver)),
                error_tx: None,
                delta_tx: None,
                pending_redemptions: None,
                crdt_state: None,
                admin_identity_pub: None,
                nav_emitter: None,
                root_serve_rx: None,
            });
            tmpdirs.push(tmp);
            nodes.push(SimCommunityNode {
                index: idx0 + 1,
                owner: id.owner,
                device_id: id.device_id.clone(),
                signing_key: Arc::clone(&id.signing),
                state,
                engine,
                tag: id.tag,
                join_hlc: bootstrap_joins[idx0].at.clone(),
            });
        }

        // O(N^2) cross-seed: every node insert-locals EVERY bootstrap Join
        // (including its own). Duplicate self-Join inserts are AlreadyKnown
        // no-ops. Satisfies the membership-at-HLC gate for every publisher.
        for node in &nodes {
            for join in &bootstrap_joins {
                let outcome = node
                    .engine
                    .insert_local_event(join.clone())
                    .await
                    .expect("cross-seed insert");
                assert!(matches!(
                    outcome,
                    InsertOutcome::Inserted | InsertOutcome::AlreadyKnown
                ));
            }
        }

        // Assemble the bus.
        let partition = Partition::fully_connected();
        let tags: Vec<[u8; 32]> = nodes.iter().map(|nd| nd.tag).collect();
        let bus = SimBus::spawn(out_rxs, in_txs, tags, partition.clone());

        Self {
            community_id,
            admin_owner: admin.owner,
            partition,
            _bus: bus,
            _cas_tx: cas_tx,
            _tmpdirs: tmpdirs,
            nodes,
        }
    }

    pub(crate) fn node(&self, index: usize) -> &SimCommunityNode {
        self.nodes
            .iter()
            .find(|nd| nd.index == index)
            .expect("node index exists")
    }

    pub(crate) fn split(&self, groups: Vec<Vec<usize>>) {
        let id_groups: Vec<Vec<[u8; 32]>> = groups
            .iter()
            .map(|g| g.iter().map(|i| self.node(*i).tag).collect())
            .collect();
        self.partition.set_split(id_groups);
    }

    pub(crate) fn heal(&self) {
        self.partition.heal();
    }

    pub(crate) async fn advance(&self, d: std::time::Duration) {
        tokio::time::sleep(d).await;
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
    }

    pub(crate) async fn counts(&self) -> Vec<usize> {
        let mut v = Vec::with_capacity(self.nodes.len());
        for nd in &self.nodes {
            v.push(nd.state.lock().await.event_count());
        }
        v
    }

    pub(crate) async fn sample(&self) -> Vec<Sample> {
        let mut v = Vec::with_capacity(self.nodes.len());
        for nd in &self.nodes {
            let s = nd.state.lock().await;
            let count = s.event_count();
            let digest = event_set_digest(&s);
            v.push(Sample { count, digest });
        }
        v
    }

    pub(crate) async fn all_states_equal(&self) -> bool {
        if self.nodes.len() < 2 {
            return true;
        }
        let first = self.nodes[0].state.lock().await.clone();
        for nd in &self.nodes[1..] {
            if *nd.state.lock().await != first {
                return false;
            }
        }
        true
    }
}

/// Order-independent u64 digest of a state's event-id set. Two logs with the
/// same event set (any insertion order) hash equal; different sets differ.
fn event_set_digest(state: &CommunityState) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut acc: u64 = 0;
    for ev in state.events() {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        ev.event_id().hash(&mut h); // adjust accessor to the real field/method
        acc ^= h.finish();
    }
    acc
}
```

**Implementer checkpoints while writing Step 4:**
1. `CommunityState` must be `Clone` for `all_states_equal` (grep `derive` on `CommunityState`; the fixtures clone events, and `PartialEq` is hand-impl'd — confirm `Clone` too, else compare via `*a == *b` without cloning by locking both in a fixed order).
2. `event_set_digest`'s `ev.event_id()` is a placeholder for the real accessor — grep `SignedMembershipEvent` for its id field/method (likely `event_id` or `.id`) and use it. The digest only needs to be a deterministic function of the event-id set.
3. If `membership_key.as_bytes()` differs from the fixture, copy the fixture's exact call (`community_sync_integration.rs:2447`).

- [ ] **Step 5: Wire the module + run the baseline test.**

In `src-tauri/src/simnet/mod.rs`, add `mod community;`.
Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(baseline_all_nodes_hold_all_joins)'`
Expected: PASS (`counts == [4,4,4,4]`, states equal).

If it fails, debug with systematic-debugging (Phase 1 first): print `c.counts().await` and any `insert_local_event` error. A count below 4 means a cross-seed insert was rejected — check the mint ceremony HLCs and the open-community flag.

- [ ] **Step 6: Lint + format.**

Run: `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && cargo fmt --all`
Expected: clean. (Watch for `dead_code` on helpers Task 4 will use — if clippy flags `sample`/`advance`/`split`/`heal` as unused before Task 4 lands, that is expected mid-plan; they are consumed by Task 4. Do NOT `#[allow(dead_code)]` them away permanently — commit Task 3 and Task 4 close together, or add a temporary `#[allow(dead_code)]` on the impl block that Task 4's final gate removes.)

- [ ] **Step 7: Commit.**

```bash
git add src-tauri/src/simnet/community.rs src-tauri/src/simnet/mod.rs
git commit -m "$(cat <<'EOF'
ZEB-917 PR2: SimCommunity — N-node CRDT engine composition + baseline converge

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01DUvg7gyEHqXrVU85Eg2D8D
EOF
)"
```

---

## Task 4: Membership partition/heal convergence test

**Files:**
- Modify: `src-tauri/src/simnet/community.rs` (add the test + a mutation helper to the `community_tests` module)

**Interfaces:**
- Consumes: everything from Tasks 1–3 (`SimCommunity`, `SimBus`, `analyze`, `Anomaly`, `Sample`), plus `crate::mint_kick_event`, `crate::mint_leave_event`, `crate::community_membership::{materialize, MemberStatus}`.
- Produces: no new public API — a `#[tokio::test(start_paused = true)]` deliverable.

**Scenario (6 nodes, islands `{1,2,3}` | `{4,5,6}`):**
1. Baseline: all 6 hold all 6 Joins; states equal.
2. Split into two islands.
3. Island A: admin (node 1) kicks node 2 → Banned (authored by admin, verifies within island A).
4. Island B: node 4 self-Leaves → Left (self-authored, needs no admin — the admin is in island A).
5. Each mutating node `flush_now()`s to publish within its island; `advance` lets same-island peers fetch+merge.
6. Assert **island divergence**: every node reaches count 7, but island A's `CommunityState` != island B's (A has the kick, B has the leave). This proves the partition genuinely isolated the islands.
7. Heal.
8. `flush_now()` on node 1 AND node 4 (each carries its island's unique event; the shared CAS holds both blobs). This is REQUIRED — the pub/sub plane has no anti-entropy.
9. `advance` lets cross-island delivery+merge run.
10. Assert **global convergence**: all 6 reach count 8, all `CommunityState` equal, and the shared state materializes node 2 Banned + node 4 Left.
11. Record a trajectory across the phases; assert `analyze(&trajectory)` is empty (no terminal anomaly).

- [ ] **Step 1: Write the failing convergence test.**

Add to the `community_tests` module in `community.rs`:

```rust
    /// Hand-build a monotone HLC for a mutation: the author has exactly one
    /// prior event (its Join, wall <= 200_000+idx), so any wall >= 300_000
    /// sorts strictly after it. No reserve ceremony needed.
    fn mutation_hlc(device_id: &str, wall_ms: u64) -> Hlc {
        Hlc { wall_ms, logical: 0, device_id: device_id.to_string() }
    }

    async fn poll_counts_eq(c: &SimCommunity, target: usize, rounds: u32) -> bool {
        for _ in 0..rounds {
            if c.counts().await.iter().all(|&x| x == target) {
                return true;
            }
            c.advance(std::time::Duration::from_millis(100)).await;
        }
        c.counts().await.iter().all(|&x| x == target)
    }

    #[tokio::test(start_paused = true)]
    async fn membership_partition_heal_reconverges() {
        let c = SimCommunity::build(6).await;
        let mut trajectory: Vec<Vec<Sample>> = vec![c.sample().await]; // baseline
        assert_eq!(c.counts().await, vec![6; 6], "baseline: all 6 Joins everywhere");
        assert!(c.all_states_equal().await, "baseline states equal");

        // Partition {1,2,3} | {4,5,6}.
        c.split(vec![vec![1, 2, 3], vec![4, 5, 6]]);

        // Island A: admin (node 1) kicks node 2.
        let n1 = c.node(1);
        let kick = crate::mint_kick_event(
            c.community_id,
            n1.owner,
            c.node(2).owner,
            Some("sim-kick".into()),
            &n1.signing_key,
            mutation_hlc(&n1.device_id, 300_000),
        )
        .expect("mint kick");
        assert!(matches!(
            n1.engine.insert_local_event(kick).await.expect("insert kick"),
            InsertOutcome::Inserted
        ));

        // Island B: node 4 self-leaves.
        let n4 = c.node(4);
        let leave = crate::mint_leave_event(
            c.community_id,
            n4.owner,
            &n4.signing_key,
            mutation_hlc(&n4.device_id, 400_000),
        )
        .expect("mint leave");
        assert!(matches!(
            n4.engine.insert_local_event(leave).await.expect("insert leave"),
            InsertOutcome::Inserted
        ));

        // Force intra-island publishes and let same-island peers merge.
        n1.engine.flush_now().await.expect("flush n1");
        n4.engine.flush_now().await.expect("flush n4");
        assert!(
            poll_counts_eq(&c, 7, 50).await,
            "each island should reach 7 events (baseline + its own mutation): {:?}",
            c.counts().await
        );
        trajectory.push(c.sample().await); // partitioned phase

        // Divergence proof: island A holds the kick, island B holds the leave.
        assert!(
            !c.all_states_equal().await,
            "islands must diverge under partition (A has kick, B has leave)"
        );

        // Heal + REQUIRED post-heal republish from each mutator.
        c.heal();
        c.node(1).engine.flush_now().await.expect("reflush n1");
        c.node(4).engine.flush_now().await.expect("reflush n4");

        assert!(
            poll_counts_eq(&c, 8, 50).await,
            "all nodes should reconverge to 8 events after heal: {:?}",
            c.counts().await
        );
        trajectory.push(c.sample().await); // healed phase

        // Global convergence: identical CommunityState, both mutations applied.
        assert!(c.all_states_equal().await, "all nodes converge to identical state");
        let events: Vec<_> = {
            let s = c.node(6).state.lock().await;
            s.events().cloned().collect()
        };
        let mat = materialize(&events, c.admin_owner);
        assert_eq!(
            mat.members.get(&c.node(2).owner).map(|m| m.status),
            Some(MemberStatus::Banned),
            "node 2 kicked -> Banned everywhere"
        );
        assert_eq!(
            mat.members.get(&c.node(4).owner).map(|m| m.status),
            Some(MemberStatus::Left),
            "node 4 left -> Left everywhere"
        );

        // The anomaly analyzer sees a clean terminal state.
        let anomalies = analyze(&trajectory);
        assert!(anomalies.is_empty(), "no terminal anomalies expected, got {anomalies:?}");
    }
```

- [ ] **Step 2: Run to confirm it fails.**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(membership_partition_heal_reconverges)'`
Expected: FAIL (compile: `analyze`/`Sample`/mint helpers not imported into the test module) or assertion — resolve imports first.

- [ ] **Step 3: Fix imports and any accessor mismatches.**

Ensure `community_tests` imports `use super::super::anomaly::{analyze, Anomaly, Sample};` (or via the mod re-exports), `use crate::community_membership::{materialize, MemberStatus};`, and that `mint_kick_event`/`mint_leave_event`/`mint_redemption`/`mint_community_creation` resolve as `crate::mint_*`. Re-run.

- [ ] **Step 4: Run to confirm it passes.**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(membership_partition_heal_reconverges)'`
Expected: PASS.

**If it hangs or times out** (systematic-debugging Phase 1 — do NOT guess-patch): the likely root cause is the delivery+merge chain not progressing under `start_paused`. Evidence to gather before any fix: (a) print `c.counts().await` inside `poll_counts_eq` each round — is it stuck at 6 (publish never delivered) or 7 (intra-island only, heal republish missing)?; (b) confirm both `flush_now()` calls returned `Ok`; (c) confirm the shared CAS is genuinely shared (one `cas_tx`, cloned per node). A stall at 7 after heal means the post-heal `flush_now` republish isn't reaching the far island — verify `heal()` ran before the reflush and that the bus drainer re-evaluates `same_side` per frame.

- [ ] **Step 5: Remove any temporary `#[allow(dead_code)]` from Task 3** now that all helpers are consumed, and run the anomaly + baseline + partition tests together:

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(simnet)'`
Expected: all SimNet tests (PR1 + PR2) PASS.

- [ ] **Step 6: Full-file lint + format.**

Run: `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && cargo fmt --all -- --check`
Expected: clean; fmt reports no diff.

- [ ] **Step 7: Commit.**

```bash
git add src-tauri/src/simnet/community.rs
git commit -m "$(cat <<'EOF'
ZEB-917 PR2: membership partition/heal reconvergence test + anomaly diagnosis

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01DUvg7gyEHqXrVU85Eg2D8D
EOF
)"
```

---

## Final Gate (before opening the PR)

Run the full CI-parity sweep from `src-tauri/` (git tree must be clean first — local gates run the working tree, not the commit):

```bash
cd src-tauri
git status --porcelain   # must be empty
cargo nextest run --locked --workspace --all-targets --features test-fixtures
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo fmt --all -- --check
```

All three must be green. The frontend/MSRV jobs are unaffected (no non-test Rust or TS changed), but CI runs them anyway.

**PR opening (per standing workflow):**
- Branch: `zeblith/zeb-917-simnet-pr2-membership-convergence` (create off latest `origin/main`; post the 🔒 claim comment on ZEB-917 with machine `Koya-Zeblith` + branch before pushing).
- Open the PR against `main` on `zeblithic/harmony-client`, reference ZEB-917, note it's PR2 of the SimNet series (PR1 = #678), and state **zero production changes**.
- Fire exactly ONE `@coderabbitai review`, then converge all bot findings in one bundled push. Never merge (Jake merges). Pushover at ready-for-merge.

---

## Self-Review (completed during authoring)

**1. Spec coverage.** The design spec's PR2 = "CRDT convergence plane (SimBus over the sans-IO sync engines + convergence oracle)." This plan covers: SimBus (Task 1), N-engine composition + oracle (`CommunityState: PartialEq`, Task 3), partition/heal reconvergence (Task 4), anomaly layer (Task 2). The HLC seam and channel-log RBSR plane are explicitly deferred per the user's "membership plane only (lean)" scope decision — documented under Scope & Boundaries.

**2. Placeholder scan.** Two intentional, flagged accessor confirmations remain (`ev.event_id()` in `event_set_digest`, and `CommunityState: Clone`), each with a labeled implementer checkpoint and a grep target — these are "verify the exact name against the type" notes, not unresolved logic. All test bodies, struct fields, config wiring, and the CAS servicer are concrete and copied from the proven `build_fixture` template.

**3. Type consistency.** `Sample`/`Anomaly`/`analyze` signatures are identical across Tasks 2 and 4. `SimCommunity`/`SimCommunityNode` fields used in Task 4 (`.owner`, `.device_id`, `.signing_key`, `.engine`, `.state`, `.index`) all appear in the Task 3 struct definition. `flush_now()`/`insert_local_event()`/`InsertOutcome` match the engine API extracted from `community_state_sync.rs`. Channel direction (`publisher_tx=out`, `subscriber_rx=in`; bus drains `out_rxs`, delivers to `in_txs`) is consistent between Task 1's interface and Task 3's `build`.

**4. Load-bearing correctness.** The post-heal `flush_now()` requirement (no pub/sub anti-entropy) is stated in Global Constraints AND enforced in Task 4's Step 8 — the single most likely place a future editor would "simplify" into a hang. The divergence-proof assertion (Step 6, `!all_states_equal`) guards against a false-green where the partition never actually isolated the islands.
