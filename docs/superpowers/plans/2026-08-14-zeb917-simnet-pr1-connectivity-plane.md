# SimNet PR1 — substrate + connectivity plane Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the SimNet deterministic-simulation substrate and its connectivity plane, and land the R1 island-repair partition/heal test — proving a record-backed Dormant peer is revived by the supervisor's parole tick after a partition heals, with no restart or external churn.

**Architecture:** SimNet is a `#[cfg(test)]`-only, in-crate module hosting N logical nodes over one partition predicate. Each node runs a real `ReachabilityResolver` + a spawned `run_reconnect_supervisor`, wired through a `SimDialer` (`impl PeerDialer`) whose dial succeeds iff both endpoints are on the same side of the partition. The supervisor's own state machine (ladder → Dormant → parole revival) does the rest; the test observes it through the public `SupervisorHandle::states_snapshot()`. Everything runs under `#[tokio::test(start_paused = true)]` and is driven by `tokio::time` advancement — no real transport, no globals, no disk.

**Tech Stack:** Rust, single crate `harmony-app` (`src-tauri/`); `tokio` `test-util` (`start_paused`), `async_trait`. No new dependencies.

## Global Constraints

- Cargo runs from `src-tauri/`. Gates (CI parity): `cargo nextest run --locked --workspace --all-targets --features test-fixtures`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo fmt --all -- --check`.
- **Zero production-code changes in PR1.** Everything is under `#[cfg(test)]`. If any needed seam turns out not to be reachable (e.g. `ReachabilityResolver::update` or `DialTelemetry` is not at least `pub(crate)`), STOP and surface it — do not add a production `pub` bump in PR1 (those belong to PR2).
- Deterministic identities come from the existing production API `NodeIdentity::from_seed(&[u8;32])` (`src/identity.rs:59`) — never a `test-fixtures` nonce helper.
- Supervisor determinism: always pass `jitter_seed: Some(_)` in `SupervisorConfig`.
- Branch off latest `origin/main` (`git checkout -b zeblith/zeb-917-simnet-pr1`), no worktrees.

## Verified API surface (copy from here — these are the real current signatures)

```rust
// src/iroh_dial_driver.rs:24 — the trait SimDialer implements
#[async_trait::async_trait]
pub trait PeerDialer: Send + Sync {
    async fn dial(&self, node_id: [u8; 32], locator: String) -> bool;
}

// src/reconnect_supervisor.rs — supervisor entry + observation
pub async fn run_reconnect_supervisor(
    handle: SupervisorHandle,
    dialer: Arc<dyn PeerDialer>,
    resolver: Arc<ReachabilityResolver>,
    telemetry: Arc<DialTelemetry>,
    self_node_id: [u8; 32],
    config: SupervisorConfig,
);
impl SupervisorHandle {
    pub fn new() -> Self;
    pub fn kick(&self, peer: [u8; 32], trigger: ReconnectTrigger);
    pub fn mark_connected(&self, peer: [u8; 32]);
    pub fn states_snapshot(&self) -> Vec<([u8; 32], PeerStateWire)>;
}
pub enum ReconnectTrigger { NewPeer, RecordChanged, Dropped, PresenceSweep }
#[serde(tag = "kind")] pub enum PeerStateWire {
    Connected { since_ms: u64 },
    Retrying  { attempt: u32, retry_in_ms: u64 },
    Dormant   { since_ms: u64 },
}
// SupervisorConfig: all fields Duration except max_concurrent_dials: usize,
// parole_batch: usize, jitter_seed: Option<u64>. Default::default() exists.

// src/reachability_resolver.rs — construct + seed a routing record (synchronous)
impl ReachabilityResolver {
    pub fn new() -> Self;
    pub fn set_supervisor(&self, handle: SupervisorHandle);
    // update(actor, payload, hlc) is what the supervisor tests use to seed a record:
    //   resolver.update(OwnerAddr([..;16]), ReachabilityAnnouncePayload{..}, Hlc{..});
    pub fn resolve_by_node_id(&self, node_id: &[u8; 32])
        -> Option<(OwnerAddr, ReachabilityAnnouncePayload)>;
}
// ReachabilityAnnouncePayload fields (harmony-reachability): iroh_node_id:[u8;32],
//   home_relay_url:String, direct_addresses:Vec<SocketAddr>, announced_at_ms:u64,
//   identity_signature:[u8;64], butler_set:Vec<_> (default empty), bs_at:u64.
// Hlc { wall_ms:u64, logical:u64, device_id:String } — src/owner_state_types.rs.
// OwnerAddr(pub [u8;16]) — src/owner_state_types.rs:411.

// src/identity.rs:59 — deterministic identity
pub struct NodeIdentity { pub pq: PqPrivateIdentity, pub ed25519: PrivateIdentity }
impl NodeIdentity { pub fn from_seed(seed: &[u8; 32]) -> Self; }
// node_id  = ni.ed25519.identity.verifying_key.as_bytes()  -> &[u8;32]
// owner    = OwnerAddr(ni.ed25519.identity.address_hash)    ([u8;16])
```

> **Confirm-at-first-use (do NOT assume):** `ReachabilityResolver::update` and `DialTelemetry` (`::new`, `.summary().paroled`) are called from the in-crate `reconnect_supervisor::tests` module today, so they compile from a sibling module only if they are `pub(crate)`+. Task 4 uses `update`; Task 6 optionally reads `paroled`. If either is narrower than `pub(crate)`, STOP and report (per Global Constraints — no PR1 production bump).

---

## File Structure

All under `src-tauri/`, all reachable only under `#[cfg(test)]`:

- `src/lib.rs` — add one line: `#[cfg(test)] mod simnet;` (the module declaration; not production code — compiled only under test).
- `src/simnet/mod.rs` — module root: the `SimNet` orchestrator (build N nodes, seed all-pairs records, partition/heal, advance), shared identity/seed helpers, and `pub(crate) use` of submodule types. Declares `mod clock; mod partition; mod dialer; mod node; mod tests;`.
- `src/simnet/clock.rs` — `SimClock` (virtual wall-ms reading tokio's paused clock).
- `src/simnet/partition.rs` — `Partition` (mutable same-side predicate over node ids).
- `src/simnet/dialer.rs` — `SimDialer` (`impl PeerDialer`, partition-gated).
- `src/simnet/node.rs` — `SimNode` (one node's resolver + spawned supervisor + handle, plus state observation).
- `src/simnet/tests.rs` — the R1 partition/heal scenario test.

> **Note on the `lib.rs` line:** `#[cfg(test)] mod simnet;` is the ONLY edit to a production file, and it is inert outside test builds (the module never compiles into a release binary). This keeps PR1's "zero production changes" property: no production item's behavior, signature, or visibility changes.

---

### Task 1: SimClock

**Files:**
- Create: `src/simnet/clock.rs`
- Modify: `src/simnet/mod.rs` (declare `mod clock;`, re-export), `src/lib.rs` (add `#[cfg(test)] mod simnet;`)

**Interfaces:**
- Produces: `struct SimClock` with `fn new() -> Self`, `fn now_ms(&self) -> u64`, and `fn as_now_fn(&self) -> Arc<dyn Fn() -> u64 + Send + Sync>`. `now_ms` returns a base wall-ms plus the tokio virtual time elapsed since construction, so `tokio::time::advance(d)` moves it.

- [ ] **Step 1: Create the module wiring**

In `src/lib.rs`, add near the other `mod` declarations:
```rust
#[cfg(test)]
mod simnet;
```
Create `src/simnet/mod.rs` with:
```rust
//! SimNet — a deterministic, single-process, virtual-time simulation harness.
//! Test-only. See docs/superpowers/specs/2026-08-14-zeb917-r6c-deterministic-simulation-harness-design.md
mod clock;
mod partition;
mod dialer;
mod node;
mod tests;

pub(crate) use clock::SimClock;
```

- [ ] **Step 2: Write the failing test**

In `src/simnet/clock.rs`:
```rust
use std::sync::Arc;
use std::time::Duration;

/// A virtual wall clock that reads tokio's (paused) clock, so a single
/// `tokio::time::advance` moves it in lockstep with the scheduler.
pub(crate) struct SimClock {
    base_ms: u64,
    origin: tokio::time::Instant,
}

#[cfg(test)]
mod clock_tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn now_ms_tracks_virtual_time() {
        let clock = SimClock::new();
        let t0 = clock.now_ms();
        tokio::time::advance(Duration::from_millis(5_000)).await;
        assert_eq!(clock.now_ms(), t0 + 5_000, "now_ms must track tokio virtual time");
    }

    #[tokio::test(start_paused = true)]
    async fn as_now_fn_matches_now_ms() {
        let clock = SimClock::new();
        let f = clock.as_now_fn();
        tokio::time::advance(Duration::from_millis(1_234)).await;
        assert_eq!(f(), clock.now_ms());
    }
}
```

- [ ] **Step 3: Run it to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(now_ms_tracks_virtual_time)'`
Expected: FAIL to COMPILE — `SimClock::new`/`now_ms`/`as_now_fn` not defined.

- [ ] **Step 4: Implement SimClock**

In `src/simnet/clock.rs`, above the test module:
```rust
impl SimClock {
    /// Base wall-ms is a fixed present-day constant so seeded record timestamps
    /// and any HLC stamps (PR2) look like real epoch-ms, never near-zero.
    pub(crate) fn new() -> Self {
        Self { base_ms: 1_700_000_000_000, origin: tokio::time::Instant::now() }
    }

    pub(crate) fn now_ms(&self) -> u64 {
        self.base_ms + self.origin.elapsed().as_millis() as u64
    }

    pub(crate) fn as_now_fn(&self) -> Arc<dyn Fn() -> u64 + Send + Sync> {
        let base = self.base_ms;
        let origin = self.origin;
        Arc::new(move || base + origin.elapsed().as_millis() as u64)
    }
}
```

- [ ] **Step 5: Run to verify it passes**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(now_ms_tracks_virtual_time) + test(as_now_fn_matches_now_ms)'`
Expected: PASS (2 tests).

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/src/simnet/mod.rs src-tauri/src/simnet/clock.rs
git commit -m "ZEB-917 PR1: SimClock — virtual wall-ms over tokio paused time"
```

---

### Task 2: Partition predicate

**Files:**
- Create: `src/simnet/partition.rs`
- Modify: `src/simnet/mod.rs` (declare `mod partition;`, re-export)

**Interfaces:**
- Produces: `struct Partition` (cheaply `Clone`, shares state via `Arc<RwLock<..>>`). API:
  - `fn fully_connected() -> Self`
  - `fn same_side(&self, a: [u8; 32], b: [u8; 32]) -> bool` (a node is always on its own side; `same_side(x, x) == true`)
  - `fn set_split(&self, groups: Vec<Vec<[u8; 32]>>)` (each inner Vec is one island; two ids are same-side iff in the same group; an id in no group is isolated — same-side only with itself)
  - `fn heal(&self)` (returns to fully-connected)

- [ ] **Step 1: Write the failing test**

In `src/simnet/partition.rs`:
```rust
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

#[cfg(test)]
mod partition_tests {
    use super::*;

    fn id(n: u8) -> [u8; 32] { [n; 32] }

    #[test]
    fn fully_connected_all_same_side() {
        let p = Partition::fully_connected();
        assert!(p.same_side(id(1), id(2)));
        assert!(p.same_side(id(1), id(1)));
    }

    #[test]
    fn split_isolates_across_groups() {
        let p = Partition::fully_connected();
        p.set_split(vec![vec![id(1), id(2), id(3)], vec![id(4), id(5), id(6)]]);
        assert!(p.same_side(id(1), id(2)), "same group -> reachable");
        assert!(!p.same_side(id(1), id(4)), "cross group -> partitioned");
        assert!(p.same_side(id(4), id(4)), "self is always same-side");
        p.heal();
        assert!(p.same_side(id(1), id(4)), "heal restores reachability");
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(split_isolates_across_groups)'`
Expected: FAIL to COMPILE — `Partition` not defined.

- [ ] **Step 3: Implement Partition**

In `src/simnet/partition.rs`, above the test module:
```rust
/// Mutable, shareable network-reachability predicate over node ids.
/// `None` group map == fully connected.
#[derive(Clone)]
pub(crate) struct Partition {
    // node_id -> group index; absent == fully connected (when `split` is None).
    split: Arc<RwLock<Option<HashMap<[u8; 32], usize>>>>,
}

impl Partition {
    pub(crate) fn fully_connected() -> Self {
        Self { split: Arc::new(RwLock::new(None)) }
    }

    pub(crate) fn set_split(&self, groups: Vec<Vec<[u8; 32]>>) {
        let mut map = HashMap::new();
        for (gi, group) in groups.iter().enumerate() {
            for id in group {
                map.insert(*id, gi);
            }
        }
        *self.split.write().expect("partition lock") = Some(map);
    }

    pub(crate) fn heal(&self) {
        *self.split.write().expect("partition lock") = None;
    }

    pub(crate) fn same_side(&self, a: [u8; 32], b: [u8; 32]) -> bool {
        if a == b {
            return true;
        }
        let guard = self.split.read().expect("partition lock");
        match guard.as_ref() {
            None => true,
            Some(map) => match (map.get(&a), map.get(&b)) {
                (Some(ga), Some(gb)) => ga == gb,
                // An id not placed in any group is isolated from everyone but itself.
                _ => false,
            },
        }
    }
}
```
Add `pub(crate) use partition::Partition;` to `src/simnet/mod.rs` and `mod partition;`.

- [ ] **Step 4: Run to verify it passes**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(fully_connected_all_same_side) + test(split_isolates_across_groups)'`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/simnet/partition.rs src-tauri/src/simnet/mod.rs
git commit -m "ZEB-917 PR1: Partition — mutable same-side reachability predicate"
```

---

### Task 3: SimDialer

**Files:**
- Create: `src/simnet/dialer.rs`
- Modify: `src/simnet/mod.rs` (declare `mod dialer;`, re-export)

**Interfaces:**
- Consumes: `Partition` (Task 2); `crate::iroh_dial_driver::PeerDialer`.
- Produces: `struct SimDialer { self_id: [u8; 32], partition: Partition }` with `fn new(self_id, partition) -> Arc<Self>`, implementing `PeerDialer` so that `dial(target, _locator)` returns `partition.same_side(self_id, target)` synchronously (no `.await` that yields — the completion is immediate, which is what makes concurrent dials deterministic).

- [ ] **Step 1: Write the failing test**

In `src/simnet/dialer.rs`:
```rust
use std::sync::Arc;

use crate::iroh_dial_driver::PeerDialer;
use super::partition::Partition;

#[cfg(test)]
mod dialer_tests {
    use super::*;

    fn id(n: u8) -> [u8; 32] { [n; 32] }

    #[tokio::test]
    async fn dial_succeeds_same_side_fails_across() {
        let partition = Partition::fully_connected();
        let dialer = SimDialer::new(id(1), partition.clone());
        assert!(dialer.dial(id(2), "iroh/x".into()).await, "connected -> dial ok");

        partition.set_split(vec![vec![id(1)], vec![id(2)]]);
        assert!(!dialer.dial(id(2), "iroh/x".into()).await, "partitioned -> dial fails");
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(dial_succeeds_same_side_fails_across)'`
Expected: FAIL to COMPILE — `SimDialer` not defined.

- [ ] **Step 3: Implement SimDialer**

In `src/simnet/dialer.rs`, above the test module:
```rust
/// A `PeerDialer` whose success is governed entirely by the partition predicate.
/// Completion is synchronous (no awaited yield point), so concurrent dials from
/// one supervisor complete in a deterministic order.
pub(crate) struct SimDialer {
    self_id: [u8; 32],
    partition: Partition,
}

impl SimDialer {
    pub(crate) fn new(self_id: [u8; 32], partition: Partition) -> Arc<Self> {
        Arc::new(Self { self_id, partition })
    }
}

#[async_trait::async_trait]
impl PeerDialer for SimDialer {
    async fn dial(&self, node_id: [u8; 32], _locator: String) -> bool {
        self.partition.same_side(self.self_id, node_id)
    }
}
```
Add `mod dialer; pub(crate) use dialer::SimDialer;` to `src/simnet/mod.rs`.

- [ ] **Step 4: Run to verify it passes**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(dial_succeeds_same_side_fails_across)'`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/simnet/dialer.rs src-tauri/src/simnet/mod.rs
git commit -m "ZEB-917 PR1: SimDialer — partition-gated PeerDialer"
```

---

### Task 4: SimNode + identity/seed helpers

**Files:**
- Create: `src/simnet/node.rs`
- Modify: `src/simnet/mod.rs` (declare `mod node;`, add the shared identity/seed helpers below)

**Interfaces:**
- Consumes: `SimClock`, `Partition`, `SimDialer`; `ReachabilityResolver`, `SupervisorHandle`, `run_reconnect_supervisor`, `SupervisorConfig`, `ReconnectTrigger`, `PeerStateWire`, `DialTelemetry` (from `crate::reconnect_supervisor`); `NodeIdentity` (from `crate::identity`); `OwnerAddr`, `Hlc`, `ReachabilityAnnouncePayload`.
- Produces:
  - In `mod.rs`: `fn node_identity(seed: u8) -> ([u8; 32], OwnerAddr)` returning `(node_id, owner_addr)` from `NodeIdentity::from_seed(&[seed; 32])`; and `fn seed_record(resolver: &ReachabilityResolver, owner: OwnerAddr, node_id: [u8; 32], now_ms: u64)` which calls `resolver.update(owner, payload, hlc)` with a fresh routing record.
  - In `node.rs`: `struct SimNode { seed: u8, node_id: [u8;32], owner: OwnerAddr, handle: SupervisorHandle, resolver: Arc<ReachabilityResolver>, _task: tokio::task::JoinHandle<()> }` with:
    - `fn spawn(seed: u8, partition: &Partition, config: SupervisorConfig) -> Self` (constructs resolver + handle, wires `set_supervisor`, spawns `run_reconnect_supervisor` with a `SimDialer`).
    - `fn state_of(&self, peer: [u8; 32]) -> Option<PeerStateWire>` (reads `states_snapshot()`).
    - `fn kick(&self, peer: [u8; 32], trigger: ReconnectTrigger)`.

- [ ] **Step 1: Add the shared helpers to `mod.rs`**

In `src/simnet/mod.rs`:
```rust
use std::sync::Arc;

use crate::owner_state_types::{Hlc, OwnerAddr};
use crate::reachability_record::ReachabilityAnnouncePayload; // re-export path; adjust if needed
use crate::reachability_resolver::ReachabilityResolver;

/// Deterministic `(node_id, owner_addr)` for a small integer seed.
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
    let hlc = Hlc { wall_ms: now_ms, logical: 0, device_id: String::new() };
    resolver.update(owner, payload, hlc);
}
```
> If the `ReachabilityAnnouncePayload` import path differs, use the same `use` the resolver's own tests use (`grep -n "use .*ReachabilityAnnouncePayload" src/reachability_resolver.rs`).

- [ ] **Step 2: Write the failing test**

In `src/simnet/node.rs`:
```rust
use std::sync::Arc;

use crate::reconnect_supervisor::{
    run_reconnect_supervisor, DialTelemetry, PeerStateWire, ReconnectTrigger, SupervisorConfig,
    SupervisorHandle,
};
use crate::reachability_resolver::ReachabilityResolver;
use crate::owner_state_types::OwnerAddr;
use super::{dialer::SimDialer, node_identity, partition::Partition, seed_record, SimClock};

#[cfg(test)]
mod node_tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn two_nodes_connect_when_unpartitioned() {
        let clock = SimClock::new();
        let partition = Partition::fully_connected();
        let cfg = SupervisorConfig { jitter_seed: Some(0xC0FFEE), ..Default::default() };

        let a = SimNode::spawn(1, &partition, cfg.clone());
        let b = SimNode::spawn(2, &partition, cfg);

        // A knows B's record and is told to connect.
        seed_record(&a.resolver, b.owner, b.node_id, clock.now_ms());
        a.kick(b.node_id, ReconnectTrigger::NewPeer);

        // Let the dial ladder run under virtual time.
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;

        assert!(
            matches!(a.state_of(b.node_id), Some(PeerStateWire::Connected { .. })),
            "A should mark B Connected after a successful same-side dial, got {:?}",
            a.state_of(b.node_id)
        );
    }
}
```

- [ ] **Step 3: Run to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(two_nodes_connect_when_unpartitioned)'`
Expected: FAIL to COMPILE — `SimNode` not defined. (If instead it fails because `ReachabilityResolver::update` or `DialTelemetry` is not visible, STOP and report — see Global Constraints.)

- [ ] **Step 4: Implement SimNode**

In `src/simnet/node.rs`, above the test module:
```rust
pub(crate) struct SimNode {
    pub(crate) seed: u8,
    pub(crate) node_id: [u8; 32],
    pub(crate) owner: OwnerAddr,
    pub(crate) handle: SupervisorHandle,
    pub(crate) resolver: Arc<ReachabilityResolver>,
    _task: tokio::task::JoinHandle<()>,
}

impl SimNode {
    pub(crate) fn spawn(seed: u8, partition: &Partition, config: SupervisorConfig) -> Self {
        let (node_id, owner) = node_identity(seed);
        let resolver = Arc::new(ReachabilityResolver::new());
        let handle = SupervisorHandle::new();
        resolver.set_supervisor(handle.clone());
        let dialer = SimDialer::new(node_id, partition.clone());
        let telemetry = Arc::new(DialTelemetry::new());
        let task = tokio::spawn(run_reconnect_supervisor(
            handle.clone(),
            dialer,
            Arc::clone(&resolver),
            telemetry,
            node_id,
            config,
        ));
        Self { seed, node_id, owner, handle, resolver, _task: task }
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
```
Add `mod node; pub(crate) use node::SimNode;` to `src/simnet/mod.rs`.

- [ ] **Step 5: Run to verify it passes**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(two_nodes_connect_when_unpartitioned)'`
Expected: PASS. If it times out or stays `Retrying`, increase the `advance` window and add a second `yield_now` (the spawned supervisor needs scheduler turns to service the kick and the dial-result channel); do NOT change production code.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/simnet/node.rs src-tauri/src/simnet/mod.rs
git commit -m "ZEB-917 PR1: SimNode + deterministic identity/record seeding"
```

---

### Task 5: SimNet orchestrator

**Files:**
- Modify: `src/simnet/mod.rs` (add the `SimNet` struct + methods)

**Interfaces:**
- Consumes: everything above.
- Produces: `struct SimNet { clock: SimClock, partition: Partition, nodes: Vec<SimNode> }` with:
  - `fn build(n: u8, config: SupervisorConfig) -> Self` — spawns `n` nodes (seeds `1..=n`), seeds every node's resolver with every *other* node's record, and `kick`s each known peer `NewPeer`.
  - `fn node(&self, seed: u8) -> &SimNode`.
  - `async fn advance(&self, d: Duration)` — advance virtual time and yield so spawned supervisors run (`tokio::time::advance(d).await; for _ in 0..4 { tokio::task::yield_now().await; }`).
  - `fn split(&self, groups: Vec<Vec<u8>>)` — set the partition by seed-number groups, and `kick(Dropped)` every now-cross-partition known peer on each node (models transport loss).
  - `fn heal(&self)` — `partition.heal()`.
  - `fn all_connected(&self, seed: u8) -> bool` — every *other* node is `Connected` in `seed`'s view.

- [ ] **Step 1: Write the failing test**

In `src/simnet/mod.rs` (its own `#[cfg(test)] mod net_tests`):
```rust
#[cfg(test)]
mod net_tests {
    use super::*;
    use std::time::Duration;
    use crate::reconnect_supervisor::SupervisorConfig;

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
            assert!(net.all_connected(s), "node {s} should see all peers Connected");
        }
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(all_nodes_connect_when_fully_connected)'`
Expected: FAIL to COMPILE — `SimNet` not defined.

- [ ] **Step 3: Implement SimNet**

In `src/simnet/mod.rs`:
```rust
use std::time::Duration;
use crate::reconnect_supervisor::{ReconnectTrigger, SupervisorConfig};

pub(crate) struct SimNet {
    clock: SimClock,
    partition: Partition,
    nodes: Vec<SimNode>,
}

impl SimNet {
    pub(crate) fn build(n: u8, config: SupervisorConfig) -> Self {
        let clock = SimClock::new();
        let partition = Partition::fully_connected();
        let nodes: Vec<SimNode> =
            (1..=n).map(|s| SimNode::spawn(s, &partition, config.clone())).collect();

        // Full all-pairs knowledge: each node learns every other node's record and
        // is told to connect to it.
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
        Self { clock, partition, nodes }
    }

    pub(crate) fn node(&self, seed: u8) -> &SimNode {
        self.nodes.iter().find(|nd| nd.seed == seed).expect("seed exists")
    }

    pub(crate) async fn advance(&self, d: Duration) {
        tokio::time::advance(d).await;
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
    }

    pub(crate) fn split(&self, groups: Vec<Vec<u8>>) {
        let id_groups: Vec<Vec<[u8; 32]>> = groups
            .iter()
            .map(|g| g.iter().map(|s| self.node(*s).node_id).collect())
            .collect();
        self.partition.set_split(id_groups);
        // Model transport loss: drop every now-cross-partition known peer so it
        // re-enters the dial ladder (and eventually goes Dormant while severed).
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

    pub(crate) fn all_connected(&self, seed: u8) -> bool {
        let me = self.node(seed);
        self.nodes.iter().filter(|nd| nd.node_id != me.node_id).all(|peer| {
            matches!(
                me.state_of(peer.node_id),
                Some(crate::reconnect_supervisor::PeerStateWire::Connected { .. })
            )
        })
    }
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(all_nodes_connect_when_fully_connected)'`
Expected: PASS. Tune the `advance` window / `fast_cfg` rungs if peers are still `Retrying` (virtual time is free — a larger window costs nothing).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/simnet/mod.rs
git commit -m "ZEB-917 PR1: SimNet orchestrator — N-node build, split, heal"
```

---

### Task 6: R1 partition/heal reconvergence test

**Files:**
- Create: `src/simnet/tests.rs`

**Interfaces:**
- Consumes: `SimNet`, `SupervisorConfig`, `PeerStateWire`.

- [ ] **Step 1: Write the scenario test**

In `src/simnet/tests.rs`:
```rust
use std::time::Duration;

use crate::reconnect_supervisor::{PeerStateWire, SupervisorConfig};
use super::SimNet;

/// R1 (ZEB-910) island repair: a partition drives cross-island peers Dormant;
/// after the partition heals, the supervisor's *parole* tick revives them into
/// real dials and the mesh reconverges — with no restart and no external churn
/// (only virtual time advances).
#[tokio::test(start_paused = true)]
async fn simnet_r1_partition_heal_reconverges() {
    let cfg = SupervisorConfig {
        retry_base: Duration::from_millis(500),
        retry_cap: Duration::from_secs(4),
        dormant_after: Duration::from_secs(10),
        parole_interval: Duration::from_secs(30),
        parole_batch: 8,
        jitter_seed: Some(0xC0FFEE),
        ..Default::default()
    };

    // 6 nodes, fully connected.
    let net = SimNet::build(6, cfg);
    net.advance(Duration::from_secs(5)).await;
    for s in 1..=6u8 {
        assert!(net.all_connected(s), "precondition: node {s} fully connected");
    }

    // Partition 1-2-3 | 4-5-6. Cross pairs are severed (Dropped) and cannot redial.
    net.split(vec![vec![1, 2, 3], vec![4, 5, 6]]);
    // Ladder past dormant_after (10s) so severed cross-island peers go Dormant.
    net.advance(Duration::from_secs(60)).await;

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

    // Heal. Do NOT kick, do NOT re-seed, do NOT restart — only advance time so the
    // periodic parole tick (every 30s) fires and revives the Dormant peers.
    net.heal();
    net.advance(Duration::from_secs(90)).await;

    for s in 1..=6u8 {
        assert!(
            net.all_connected(s),
            "node {s} must reconverge to fully-connected via parole alone"
        );
    }
}
```

- [ ] **Step 2: Run to verify it fails first (drive-time calibration)**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(simnet_r1_partition_heal_reconverges)'`
Expected: initially may FAIL on the Dormant or reconverge assertion if the advance windows don't cross `dormant_after` / `parole_interval`. This is calibration, not a code bug: widen the `advance` windows (virtual time is free) until the transitions are crossed. The severed peer MUST reach Dormant before heal, and MUST return to Connected after ≥ one post-heal parole interval.

- [ ] **Step 3: Make it pass by calibration only**

Adjust the three `advance` durations (post-split ≥ `dormant_after` + a few ladder rungs; post-heal ≥ `parole_interval` + a dial). No production code changes. Confirm the intra-island `Connected` and cross-island `Dormant`→`Connected` transitions all hold.

- [ ] **Step 4: Run to verify it passes**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(simnet)'`
Expected: PASS (all SimNet tests).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/simnet/tests.rs
git commit -m "ZEB-917 PR1: R1 partition/heal reconvergence test (parole revival)"
```

---

### Task 7: Full gate + module doc

**Files:**
- Modify: `src/simnet/mod.rs` (expand the module doc-comment into a short usage note)

- [ ] **Step 1: Write the module doc**

Expand the `//!` header in `src/simnet/mod.rs` to briefly describe: the two-plane vision (connectivity now, CRDT in PR2), the "compose subsystems, don't boot nodes" principle, the virtual-time model, and a one-line pointer to the spec. Keep it under ~20 lines.

- [ ] **Step 2: Run the full CI-parity gate**

```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```
Expected: fmt clean; clippy clean; full suite green (existing count + the new SimNet tests). Confirm `git status` is clean (working tree == committed) before declaring green.

- [ ] **Step 3: Commit + open PR**

```bash
git add src-tauri/src/simnet/mod.rs
git commit -m "ZEB-917 PR1: SimNet module doc"
git push -u origin zeblith/zeb-917-simnet-pr1
gh pr create --repo zeblithic/harmony-client --base main \
  --title "ZEB-917 PR1: SimNet deterministic-sim substrate + connectivity plane (R1 partition/heal)" \
  --body "<see PR body below>"
```

---

## Deferred to PR2 (do not attempt in PR1)

- The gateway-dial driver + `coverage_verdict` integration (asserting the coverage-health flip Healthy→Degraded→Healthy). `coverage_verdict`/`CoverageVerdict` are module-private today; wiring an observable assertion wants a 1-line `pub(crate)` bump, which belongs with PR2's production-touch increment (the HLC seam), not PR1's zero-change PR.
- The CRDT convergence plane, SimBus, HLC clock seam, convergence oracle, and seed-replay test (the whole of §4–§5 of the spec).

## Self-Review

- **Spec coverage (§2.1 substrate, §3 connectivity):** Task 1 (SimClock/virtual clock), Task 2 (partition predicate), Task 3 (SimDialer), Tasks 4–5 (N-node composition of resolver+supervisor), Task 6 (R1 partition/heal, the ZEB-910 parole property). The spec's §3 mention of asserting `coverage_verdict` is explicitly deferred (gateway driver → PR2) with the visibility rationale — a documented, surfaced scope change, not a silent gap.
- **Placeholder scan:** every code step has real, copy-ready code from the verified API surface; the only "tune this" steps are virtual-time window calibration (Tasks 5–6), which is legitimate and cost-free, not a code placeholder.
- **Type consistency:** `node_id: [u8;32]` and `OwnerAddr([u8;16])` used consistently; `PeerStateWire` (not `PeerState`) is the observation type throughout; `SupervisorConfig`/`ReconnectTrigger`/`run_reconnect_supervisor` signatures match the verified surface; `SimClock`/`Partition`/`SimDialer`/`SimNode`/`SimNet` names stable across tasks.
- **Zero-production-change invariant:** the only non-test-file edit is `#[cfg(test)] mod simnet;` in `lib.rs`, inert outside test builds. Two confirm-at-first-use visibility risks (`ReachabilityResolver::update`, `DialTelemetry`) are flagged with an explicit STOP-and-report rather than a silent production bump.
