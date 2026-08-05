# ZEB-803 Relay-Acceptor Watchdog Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a self-healing watchdog that detects a silently-stalled community-relay acceptor and remediates it in two tiers (in-place `network_change()`, then a full-node restart) before loudly escalating.

**Architecture:** A new module `relay_acceptor_watchdog.rs` with a *pure decision core* (`evaluate()` state machine — all correctness, all unit tests) and a *generic harness* (a tick loop over injected `ServingSensor` / `RemediationActuator` / `Clock` traits). Production sensor/actuator impls live in `lib.rs` where they can touch `NodeState`, the endpoint, the resolver, and the telemetry Arc. Escalation state lives in a process-global `OnceLock` so it survives the in-process Tier-2 restart. A health field surfaces the watchdog's own decisions.

**Tech Stack:** Rust, tokio, `async-trait` (already a direct dep), `tracing`. No new dependencies.

## Global Constraints

- Rust; all cargo commands run from `src-tauri/`.
- CI parity gates (run all three before every commit that touches Rust):
  - `cargo fmt --all -- --check`
  - `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
  - `cargo nextest run --locked --workspace --all-targets --features test-fixtures`
- During iterative dev, `scripts/test-select --context task` is acceptable per task; the **final pre-PR sweep must be the full `--workspace --all-targets` nextest run**.
- No new dependencies. No production behavior change beyond the watchdog itself — the acceptor, the accept loop, and the existing telemetry are untouched; the watchdog only *reads* existing signals and *invokes* two existing levers.
- Confirmed config defaults: `stale_multiplier N = 3`, `max_restarts = 3`, `tier1/tier2_cooldown = 2 × cadence`, `eval_interval = cadence / 3`, `cadence = COMMUNITY_RELAY_PULL_INTERVAL_MS`.
- Wire structs that cross serde use `#[serde(rename_all = "camelCase")]` (matches the existing health DTOs).
- `std::sync::Mutex` guards the watchdog memory (O(1), never held across an `.await`), mirroring the ZEB-866 gate pattern.

---

### Task 1: Module scaffold + core types

**Files:**
- Create: `src-tauri/src/relay_acceptor_watchdog.rs`
- Modify: `src-tauri/src/lib.rs` (add `pub mod relay_acceptor_watchdog;` beside the other `pub mod` declarations)

**Interfaces:**
- Produces: `WatchdogConfig`, `WatchdogInputs`, `Tier`, `Phase`, `WatchdogMemory` (+ `Default`), `Verdict` — consumed by every later task.

- [ ] **Step 1: Create the module with the core types**

```rust
//! ZEB-803: self-healing watchdog for the community-relay acceptor.
//!
//! Pure decision core (`evaluate`) + a generic harness. The core is a state
//! machine over three observed signals (last-served-pull time, connected-peer
//! count, wall clock) that decides Hold / tier-1 probe / tier-2 restart /
//! escalate. All correctness lives here and is unit-tested with injected values;
//! the live levers sit behind the traits in the harness section.

use std::sync::{Arc, Mutex};

/// Tunable knobs. Production values are derived from the relay pull cadence at
/// boot; tests pass small values.
#[derive(Clone, Copy, Debug)]
pub struct WatchdogConfig {
    /// Relay serve cadence (= `COMMUNITY_RELAY_PULL_INTERVAL_MS`, ~7m30s).
    pub cadence_ms: u64,
    /// Stall threshold = `stale_multiplier * cadence_ms`.
    pub stale_multiplier: u32,
    /// Watchdog tick period.
    pub eval_interval_ms: u64,
    /// Grace after a tier-1 `network_change()` before re-judging.
    pub tier1_cooldown_ms: u64,
    /// Grace after a tier-2 full restart before re-judging.
    pub tier2_cooldown_ms: u64,
    /// Consecutive full restarts without recovery before escalating.
    pub max_restarts: u32,
}

/// One sample of the observed signals.
#[derive(Clone, Copy, Debug)]
pub struct WatchdogInputs {
    pub now_ms: u64,
    /// `None` = never served in this telemetry lifetime (the `0 → None` sentinel).
    pub last_served_ms: Option<u64>,
    pub connected_peers: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tier {
    Probe,
    Restart,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Normal,
    Cooldown { since_ms: u64, tier: Tier },
    Escalated,
}

/// Persistent watchdog state. Lives in a process-global so it survives the
/// in-process Tier-2 restart (see the prod wiring). `served_ever` is sticky.
#[derive(Clone, Copy, Debug)]
pub struct WatchdogMemory {
    pub served_ever: bool,
    pub phase: Phase,
    pub consecutive_restarts: u32,
    /// `last_served_ms` captured at the last action — the recovery baseline.
    pub baseline_served_ms: Option<u64>,
    pub last_action_ms: Option<u64>,
    pub last_action_tier: Option<Tier>,
}

impl Default for WatchdogMemory {
    fn default() -> Self {
        Self {
            served_ever: false,
            phase: Phase::Normal,
            consecutive_restarts: 0,
            baseline_served_ms: None,
            last_action_ms: None,
            last_action_tier: None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict {
    Hold,
    ProbeNetwork,
    RestartNode,
    Escalate,
}
```

- [ ] **Step 2: Declare the module**

In `src-tauri/src/lib.rs`, add beside the other module declarations (e.g. next to `pub mod network_health;`):

```rust
pub mod relay_acceptor_watchdog;
```

- [ ] **Step 3: Add a default-shape unit test**

Append to `relay_acceptor_watchdog.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_default_is_normal_and_zeroed() {
        let m = WatchdogMemory::default();
        assert!(!m.served_ever);
        assert_eq!(m.phase, Phase::Normal);
        assert_eq!(m.consecutive_restarts, 0);
        assert_eq!(m.baseline_served_ms, None);
    }
}
```

- [ ] **Step 4: Verify it compiles and passes**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(memory_default_is_normal_and_zeroed)'`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/relay_acceptor_watchdog.rs src-tauri/src/lib.rs
git commit -m "ZEB-803: watchdog module scaffold + core types"
```

---

### Task 2: `evaluate()` state machine + full unit matrix

**Files:**
- Modify: `src-tauri/src/relay_acceptor_watchdog.rs`

**Interfaces:**
- Consumes: all Task 1 types.
- Produces: `pub fn evaluate(cfg: &WatchdogConfig, inputs: &WatchdogInputs, mem: &mut WatchdogMemory) -> Verdict`.

- [ ] **Step 1: Write the first test batch (gates + Tier-1 fire) — expect FAIL (no `evaluate`)**

Add inside the `tests` module:

```rust
fn test_cfg() -> WatchdogConfig {
    // threshold = 3 * 1000 = 3000ms; cooldowns 2000ms
    WatchdogConfig {
        cadence_ms: 1000,
        stale_multiplier: 3,
        eval_interval_ms: 300,
        tier1_cooldown_ms: 2000,
        tier2_cooldown_ms: 2000,
        max_restarts: 3,
    }
}

fn served_mem() -> WatchdogMemory {
    // a node that has served at least once, sitting Normal
    WatchdogMemory { served_ever: true, ..WatchdogMemory::default() }
}

#[test]
fn healthy_serve_holds() {
    let mut m = served_mem();
    let v = evaluate(&test_cfg(), &WatchdogInputs { now_ms: 5000, last_served_ms: Some(4000), connected_peers: 2 }, &mut m);
    assert_eq!(v, Verdict::Hold);
    assert_eq!(m.phase, Phase::Normal);
}

#[test]
fn never_served_holds_even_when_stale() {
    let mut m = WatchdogMemory::default(); // served_ever = false
    let v = evaluate(&test_cfg(), &WatchdogInputs { now_ms: 100_000, last_served_ms: None, connected_peers: 5 }, &mut m);
    assert_eq!(v, Verdict::Hold);
    assert!(!m.served_ever);
}

#[test]
fn no_connected_peers_holds() {
    let mut m = served_mem();
    let v = evaluate(&test_cfg(), &WatchdogInputs { now_ms: 10_000, last_served_ms: Some(1000), connected_peers: 0 }, &mut m);
    assert_eq!(v, Verdict::Hold);
}

#[test]
fn stall_fires_tier1_probe() {
    let mut m = served_mem();
    let v = evaluate(&test_cfg(), &WatchdogInputs { now_ms: 10_000, last_served_ms: Some(1000), connected_peers: 2 }, &mut m);
    assert_eq!(v, Verdict::ProbeNetwork);
    assert_eq!(m.phase, Phase::Cooldown { since_ms: 10_000, tier: Tier::Probe });
    assert_eq!(m.baseline_served_ms, Some(1000));
    assert_eq!(m.last_action_tier, Some(Tier::Probe));
}
```

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(relay_acceptor_watchdog)'`
Expected: FAIL to compile (`evaluate` not found).

- [ ] **Step 2: Implement `evaluate()` + helpers**

Add above the `tests` module:

```rust
fn cooldown_for(cfg: &WatchdogConfig, tier: Tier) -> u64 {
    match tier {
        Tier::Probe => cfg.tier1_cooldown_ms,
        Tier::Restart => cfg.tier2_cooldown_ms,
    }
}

/// A remedy is confirmed only when a serve has landed *after* the action's
/// baseline — never raw staleness, since a successful remedy can't clear
/// staleness until the next remote pull (~1 cadence away).
fn recovered(inputs: &WatchdogInputs, mem: &WatchdogMemory) -> bool {
    match inputs.last_served_ms {
        Some(c) => mem.baseline_served_ms.map_or(true, |b| c > b),
        None => false,
    }
}

fn stall_detected(cfg: &WatchdogConfig, inputs: &WatchdogInputs, mem: &WatchdogMemory) -> bool {
    mem.served_ever
        && inputs.connected_peers > 0
        && match inputs.last_served_ms {
            Some(ts) => {
                inputs.now_ms.saturating_sub(ts) > cfg.stale_multiplier as u64 * cfg.cadence_ms
            }
            None => false,
        }
}

fn fire_restart(cfg: &WatchdogConfig, inputs: &WatchdogInputs, mem: &mut WatchdogMemory) -> Verdict {
    if mem.consecutive_restarts >= cfg.max_restarts {
        mem.phase = Phase::Escalated;
        return Verdict::Escalate;
    }
    mem.consecutive_restarts += 1;
    mem.baseline_served_ms = inputs.last_served_ms;
    mem.last_action_ms = Some(inputs.now_ms);
    mem.last_action_tier = Some(Tier::Restart);
    mem.phase = Phase::Cooldown { since_ms: inputs.now_ms, tier: Tier::Restart };
    Verdict::RestartNode
}

fn reset(mem: &mut WatchdogMemory) {
    mem.phase = Phase::Normal;
    mem.consecutive_restarts = 0;
    mem.baseline_served_ms = None;
}

/// The pure decision core. Mutates `mem` (phase/counters/baselines) and returns
/// the action the harness should take.
pub fn evaluate(cfg: &WatchdogConfig, inputs: &WatchdogInputs, mem: &mut WatchdogMemory) -> Verdict {
    if inputs.last_served_ms.is_some() {
        mem.served_ever = true; // sticky
    }

    match mem.phase {
        Phase::Escalated => {
            if recovered(inputs, mem) {
                reset(mem);
            }
            Verdict::Hold // terminal until a serve resets it
        }
        Phase::Cooldown { since_ms, tier } => {
            if recovered(inputs, mem) {
                reset(mem);
                return Verdict::Hold;
            }
            if inputs.now_ms.saturating_sub(since_ms) < cooldown_for(cfg, tier) {
                return Verdict::Hold; // still waiting to see if the remedy took
            }
            // cooldown elapsed, still not recovered → escalate to a restart
            // (tier-1 failure → first restart; tier-2 failure → repeat/Escalate)
            fire_restart(cfg, inputs, mem)
        }
        Phase::Normal => {
            if !stall_detected(cfg, inputs, mem) {
                return Verdict::Hold;
            }
            mem.baseline_served_ms = inputs.last_served_ms;
            mem.last_action_ms = Some(inputs.now_ms);
            mem.last_action_tier = Some(Tier::Probe);
            mem.phase = Phase::Cooldown { since_ms: inputs.now_ms, tier: Tier::Probe };
            Verdict::ProbeNetwork
        }
    }
}
```

- [ ] **Step 3: Run batch 1 — expect PASS**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(relay_acceptor_watchdog)'`
Expected: the four batch-1 tests PASS.

- [ ] **Step 4: Write the second test batch (cooldown / restart / escalate / sticky / full sequence)**

Add to the `tests` module:

```rust
#[test]
fn cooldown_probe_within_holds() {
    let mut m = served_mem();
    evaluate(&test_cfg(), &WatchdogInputs { now_ms: 10_000, last_served_ms: Some(1000), connected_peers: 2 }, &mut m);
    // Δ = 1000 < 2000 cooldown, still stale → Hold
    let v = evaluate(&test_cfg(), &WatchdogInputs { now_ms: 11_000, last_served_ms: Some(1000), connected_peers: 2 }, &mut m);
    assert_eq!(v, Verdict::Hold);
    assert_eq!(m.phase, Phase::Cooldown { since_ms: 10_000, tier: Tier::Probe });
}

#[test]
fn probe_recovery_resets() {
    let mut m = served_mem();
    evaluate(&test_cfg(), &WatchdogInputs { now_ms: 10_000, last_served_ms: Some(1000), connected_peers: 2 }, &mut m);
    // a serve landed after the baseline (10_500 > 1000) → recovered
    let v = evaluate(&test_cfg(), &WatchdogInputs { now_ms: 11_000, last_served_ms: Some(10_500), connected_peers: 2 }, &mut m);
    assert_eq!(v, Verdict::Hold);
    assert_eq!(m.phase, Phase::Normal);
    assert_eq!(m.consecutive_restarts, 0);
    assert_eq!(m.baseline_served_ms, None);
}

#[test]
fn probe_failure_escalates_to_restart() {
    let mut m = served_mem();
    evaluate(&test_cfg(), &WatchdogInputs { now_ms: 10_000, last_served_ms: Some(1000), connected_peers: 2 }, &mut m);
    // Δ = 2500 >= 2000 cooldown, still stale → tier-2 restart
    let v = evaluate(&test_cfg(), &WatchdogInputs { now_ms: 12_500, last_served_ms: Some(1000), connected_peers: 2 }, &mut m);
    assert_eq!(v, Verdict::RestartNode);
    assert_eq!(m.consecutive_restarts, 1);
    assert_eq!(m.phase, Phase::Cooldown { since_ms: 12_500, tier: Tier::Restart });
}

#[test]
fn restart_cap_escalates() {
    let mut m = WatchdogMemory {
        served_ever: true,
        phase: Phase::Cooldown { since_ms: 0, tier: Tier::Restart },
        consecutive_restarts: 3, // already at cap
        baseline_served_ms: Some(1000),
        ..WatchdogMemory::default()
    };
    // cooldown elapsed, not recovered, at cap → Escalate
    let v = evaluate(&test_cfg(), &WatchdogInputs { now_ms: 5000, last_served_ms: Some(1000), connected_peers: 2 }, &mut m);
    assert_eq!(v, Verdict::Escalate);
    assert_eq!(m.phase, Phase::Escalated);
}

#[test]
fn escalated_holds_until_recovery() {
    let mut m = WatchdogMemory { served_ever: true, phase: Phase::Escalated, baseline_served_ms: Some(1000), ..WatchdogMemory::default() };
    // not recovered → stays Escalated
    let v1 = evaluate(&test_cfg(), &WatchdogInputs { now_ms: 5000, last_served_ms: Some(1000), connected_peers: 2 }, &mut m);
    assert_eq!(v1, Verdict::Hold);
    assert_eq!(m.phase, Phase::Escalated);
    // a fresh serve (2000 > 1000) → reset to Normal
    let v2 = evaluate(&test_cfg(), &WatchdogInputs { now_ms: 6000, last_served_ms: Some(2000), connected_peers: 2 }, &mut m);
    assert_eq!(v2, Verdict::Hold);
    assert_eq!(m.phase, Phase::Normal);
}

#[test]
fn served_ever_is_sticky_across_none() {
    let mut m = WatchdogMemory::default();
    evaluate(&test_cfg(), &WatchdogInputs { now_ms: 1000, last_served_ms: Some(500), connected_peers: 1 }, &mut m);
    assert!(m.served_ever);
    // a later None sample (fresh telemetry after a restart) must not clear it
    evaluate(&test_cfg(), &WatchdogInputs { now_ms: 2000, last_served_ms: None, connected_peers: 1 }, &mut m);
    assert!(m.served_ever);
}

#[test]
fn persistent_stall_is_one_probe_three_restarts_then_escalate() {
    let cfg = test_cfg();
    let mut m = served_mem();
    let mut actions = Vec::new();
    // stale last_served fixed at 1000; step now by 3000 (> cooldown 2000) each tick
    for now in [5000u64, 8000, 11_000, 14_000, 17_000, 20_000] {
        let v = evaluate(&cfg, &WatchdogInputs { now_ms: now, last_served_ms: Some(1000), connected_peers: 2 }, &mut m);
        if v != Verdict::Hold {
            actions.push(v);
        }
    }
    assert_eq!(
        actions,
        vec![Verdict::ProbeNetwork, Verdict::RestartNode, Verdict::RestartNode, Verdict::RestartNode, Verdict::Escalate]
    );
}
```

- [ ] **Step 5: Run the full matrix — expect PASS**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(relay_acceptor_watchdog)'`
Expected: all 12 tests PASS.

- [ ] **Step 6: fmt + clippy, then commit**

```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd .. && git add src-tauri/src/relay_acceptor_watchdog.rs
git commit -m "ZEB-803: watchdog decision core (evaluate state machine) + unit matrix"
```

---

### Task 3: Generic harness (traits + tick loop) + harness test

**Files:**
- Modify: `src-tauri/src/relay_acceptor_watchdog.rs`

**Interfaces:**
- Consumes: `WatchdogConfig`, `WatchdogInputs`, `WatchdogMemory`, `Verdict`, `evaluate`.
- Produces: traits `Clock`, `ServingSensor`, `RemediationActuator`; struct `RelayAcceptorWatchdog<S, A, C>` with `new(...)`, `run(self, shutdown)`, and `pub(crate) async fn tick(&self)`.

- [ ] **Step 1: Add the traits + harness struct**

Add to the module (top-level, above `tests`):

```rust
use std::time::Duration;

pub trait Clock: Send + Sync {
    fn now_ms(&self) -> u64;
}

pub trait ServingSensor: Send + Sync {
    fn sample(&self, now_ms: u64) -> WatchdogInputs;
}

#[async_trait::async_trait]
pub trait RemediationActuator: Send + Sync {
    /// Tier 1: in-place re-probe (`endpoint.network_change()`).
    async fn probe_network(&self);
    /// Tier 2: full-node restart (`stop_inner` + `start_node_inner`).
    async fn restart_node(&self);
}

pub struct RelayAcceptorWatchdog<S, A, C> {
    cfg: WatchdogConfig,
    memory: Arc<Mutex<WatchdogMemory>>,
    sensor: S,
    actuator: A,
    clock: C,
}

impl<S: ServingSensor, A: RemediationActuator, C: Clock> RelayAcceptorWatchdog<S, A, C> {
    pub fn new(
        cfg: WatchdogConfig,
        memory: Arc<Mutex<WatchdogMemory>>,
        sensor: S,
        actuator: A,
        clock: C,
    ) -> Self {
        Self { cfg, memory, sensor, actuator, clock }
    }

    /// Background loop: evaluate every `eval_interval_ms` until shutdown.
    pub async fn run(self, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        let mut interval = tokio::time::interval(Duration::from_millis(self.cfg.eval_interval_ms));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = interval.tick() => self.tick().await,
                _ = shutdown.changed() => break,
            }
        }
    }

    /// One evaluation: sample → decide → act. `pub(crate)` so harness tests can
    /// drive it directly without coordinating tokio timers.
    pub(crate) async fn tick(&self) {
        let inputs = self.sensor.sample(self.clock.now_ms());
        let verdict = {
            let mut m = self.memory.lock().expect("watchdog memory poisoned");
            evaluate(&self.cfg, &inputs, &mut m)
        };
        match verdict {
            Verdict::Hold => {}
            Verdict::ProbeNetwork => {
                let staleness = inputs.last_served_ms.map(|t| inputs.now_ms.saturating_sub(t));
                tracing::warn!(
                    staleness_ms = ?staleness,
                    connected = inputs.connected_peers,
                    "ZEB-803 watchdog: relay serving stalled — tier 1 network_change()"
                );
                self.actuator.probe_network().await;
            }
            Verdict::RestartNode => {
                tracing::warn!(
                    connected = inputs.connected_peers,
                    "ZEB-803 watchdog: tier-1 probe did not restore serving — tier 2 full-node restart"
                );
                self.actuator.restart_node().await;
            }
            Verdict::Escalate => {
                tracing::error!(
                    "ZEB-803 watchdog: relay serving still stalled after max restarts — escalating, no further automatic action"
                );
            }
        }
    }
}
```

- [ ] **Step 2: Write the harness test (mocks + direct `tick()` driving) — expect FAIL**

Add to the `tests` module:

```rust
use std::sync::atomic::{AtomicU64, Ordering};

struct MockClock(Arc<AtomicU64>);
impl Clock for MockClock {
    fn now_ms(&self) -> u64 { self.0.load(Ordering::Relaxed) }
}

/// Serves back a fixed `last_served_ms` + connected count.
struct MockSensor { last_served_ms: Option<u64>, connected: u32 }
impl ServingSensor for MockSensor {
    fn sample(&self, now_ms: u64) -> WatchdogInputs {
        WatchdogInputs { now_ms, last_served_ms: self.last_served_ms, connected_peers: self.connected }
    }
}

struct RecordingActuator(Arc<Mutex<Vec<&'static str>>>);
#[async_trait::async_trait]
impl RemediationActuator for RecordingActuator {
    async fn probe_network(&self) { self.0.lock().unwrap().push("probe"); }
    async fn restart_node(&self) { self.0.lock().unwrap().push("restart"); }
}

#[tokio::test]
async fn harness_drives_probe_then_restarts_then_escalate() {
    let clock = Arc::new(AtomicU64::new(0));
    let actions = Arc::new(Mutex::new(Vec::new()));
    let wd = RelayAcceptorWatchdog::new(
        test_cfg(),
        Arc::new(Mutex::new(served_mem())),
        MockSensor { last_served_ms: Some(1000), connected: 2 }, // persistently stale
        RecordingActuator(actions.clone()),
        MockClock(clock.clone()),
    );
    // step the injected clock past a cooldown each tick
    for now in [5000u64, 8000, 11_000, 14_000, 17_000, 20_000] {
        clock.store(now, Ordering::Relaxed);
        wd.tick().await;
    }
    assert_eq!(*actions.lock().unwrap(), vec!["probe", "restart", "restart", "restart"]);
    // and no action fires during a cooldown tick
    let actions2 = Arc::new(Mutex::new(Vec::new()));
    let clock2 = Arc::new(AtomicU64::new(10_000));
    let wd2 = RelayAcceptorWatchdog::new(
        test_cfg(),
        Arc::new(Mutex::new(served_mem())),
        MockSensor { last_served_ms: Some(1000), connected: 2 },
        RecordingActuator(actions2.clone()),
        MockClock(clock2.clone()),
    );
    wd2.tick().await;                       // stall → probe
    clock2.store(10_500, Ordering::Relaxed); // Δ 500 < 2000 cooldown
    wd2.tick().await;                       // cooldown → no action
    assert_eq!(*actions2.lock().unwrap(), vec!["probe"]);
}
```

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(harness_drives_probe)'`
Expected: FAIL to compile until Step 1 is in (if written first) — otherwise PASS.

- [ ] **Step 3: Run the harness test — expect PASS**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(relay_acceptor_watchdog)'`
Expected: all watchdog tests PASS (13 total).

- [ ] **Step 4: fmt + clippy, then commit**

```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd .. && git add src-tauri/src/relay_acceptor_watchdog.rs
git commit -m "ZEB-803: watchdog harness (sensor/actuator/clock traits + tick loop) + harness test"
```

---

### Task 4: Health surface + telemetry accessor (`network_health.rs`)

**Files:**
- Modify: `src-tauri/src/network_health.rs`

**Interfaces:**
- Consumes: `crate::relay_acceptor_watchdog::{WatchdogMemory, Phase, Tier}`.
- Produces: `RelayAcceptorWatchdogHealth` (Serialize) + a `CommunityRelayServingTelemetry::last_served_ms()` accessor + a `RelayAcceptorWatchdogHealth::from_parts(...)` constructor.

- [ ] **Step 1: Add the lightweight telemetry accessor**

In the `impl CommunityRelayServingTelemetry` block (near `network_health.rs:893`), add:

```rust
/// The process-wide last-served-pull time, `0 → None` (the "never served"
/// sentinel), without allocating the full peer summary. Used by the ZEB-803
/// watchdog sensor.
pub(crate) fn last_served_ms(&self) -> Option<u64> {
    match self.last_served_ms.load(std::sync::atomic::Ordering::Relaxed) {
        0 => None,
        ms => Some(ms),
    }
}
```

- [ ] **Step 2: Add the health struct + constructor (with a serde-shape test first)**

Add the test to `network_health.rs`'s test module — expect FAIL (types absent):

```rust
#[test]
fn watchdog_health_serializes_camel_case() {
    use crate::relay_acceptor_watchdog::{Phase, Tier, WatchdogMemory};
    let mem = WatchdogMemory {
        served_ever: true,
        phase: Phase::Cooldown { since_ms: 1, tier: Tier::Restart },
        consecutive_restarts: 2,
        baseline_served_ms: Some(1000),
        last_action_ms: Some(5000),
        last_action_tier: Some(Tier::Restart),
    };
    let h = RelayAcceptorWatchdogHealth::from_parts(&mem, Some(4200), 3);
    let v = serde_json::to_value(&h).unwrap();
    assert_eq!(v["phase"], "cooldown");
    assert_eq!(v["consecutiveRestarts"], 2);
    assert_eq!(v["lastActionTier"], "restart");
    assert_eq!(v["stalenessMs"], 4200);
    assert_eq!(v["connectedPeers"], 3);
    assert_eq!(v["escalated"], false);
}
```

Then add the production types (near the other health DTOs, e.g. after `CommunityRelayServingHealth` ~`network_health.rs:524`):

```rust
/// ZEB-803: the relay-acceptor watchdog's own state, so its decisions are never
/// a silent surface. `phase`/counters come from `WatchdogMemory`; `staleness_ms`
/// and `connected_peers` are computed live at snapshot time.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RelayAcceptorWatchdogHealth {
    pub staleness_ms: Option<u64>,
    pub connected_peers: u32,
    pub phase: &'static str,
    pub consecutive_restarts: u32,
    pub last_action_ms: Option<u64>,
    pub last_action_tier: Option<&'static str>,
    pub escalated: bool,
}

impl RelayAcceptorWatchdogHealth {
    pub fn from_parts(
        mem: &crate::relay_acceptor_watchdog::WatchdogMemory,
        staleness_ms: Option<u64>,
        connected_peers: u32,
    ) -> Self {
        use crate::relay_acceptor_watchdog::{Phase, Tier};
        let phase = match mem.phase {
            Phase::Normal => "normal",
            Phase::Cooldown { .. } => "cooldown",
            Phase::Escalated => "escalated",
        };
        let tier_str = |t: Tier| match t {
            Tier::Probe => "probe",
            Tier::Restart => "restart",
        };
        Self {
            staleness_ms,
            connected_peers,
            phase,
            consecutive_restarts: mem.consecutive_restarts,
            last_action_ms: mem.last_action_ms,
            last_action_tier: mem.last_action_tier.map(tier_str),
            escalated: matches!(mem.phase, Phase::Escalated),
        }
    }
}
```

- [ ] **Step 3: Run the shape test — expect PASS**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(watchdog_health_serializes_camel_case)'`
Expected: PASS.

- [ ] **Step 4: Add the field to the snapshot DTO**

Find the top-level snapshot struct that carries the `community_relay` field (assembled near `network_health.rs:2622-2631`). Add a sibling optional field to the struct definition:

```rust
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relay_acceptor_watchdog: Option<RelayAcceptorWatchdogHealth>,
```

Populate it in the snapshot assembly to `None` for now (the live value is wired in Task 6, which owns the memory handle + telemetry access):

```rust
    relay_acceptor_watchdog: None,
```

- [ ] **Step 5: fmt + clippy + scoped test, then commit**

```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --features test-fixtures -E 'test(watchdog_health) + test(relay_acceptor_watchdog)'
cd .. && git add src-tauri/src/network_health.rs
git commit -m "ZEB-803: RelayAcceptorWatchdogHealth surface + telemetry last_served_ms accessor"
```

---

### Task 5: Process-global memory + `ProdWatchdogSensor` (`lib.rs`)

**Files:**
- Modify: `src-tauri/src/lib.rs`

**Interfaces:**
- Consumes: `relay_acceptor_watchdog::{WatchdogMemory, WatchdogInputs, ServingSensor}`, `network_health::{CommunityRelayServingTelemetry, count_peer_states, now_ms}`, `reconnect_supervisor` `states_snapshot`, `ReachabilityResolver`.
- Produces: `fn watchdog_memory() -> Arc<Mutex<WatchdogMemory>>`; `struct ProdWatchdogSensor` impl `ServingSensor`.

- [ ] **Step 1: Add the process-global memory**

Near the top-level statics in `lib.rs`:

```rust
use std::sync::OnceLock as StdOnceLock; // if not already imported

/// ZEB-803 watchdog escalation state. Process-global (NOT a `NodeState` field)
/// so `consecutive_restarts`/`phase` survive the in-process Tier-2 restart —
/// otherwise the max-restart cap would reset every restart and the watchdog
/// would restart forever.
static WATCHDOG_MEMORY: StdOnceLock<Arc<Mutex<crate::relay_acceptor_watchdog::WatchdogMemory>>> =
    StdOnceLock::new();

fn watchdog_memory() -> Arc<Mutex<crate::relay_acceptor_watchdog::WatchdogMemory>> {
    WATCHDOG_MEMORY
        .get_or_init(|| Arc::new(Mutex::new(crate::relay_acceptor_watchdog::WatchdogMemory::default())))
        .clone()
}
```

(Use the crate's existing `Mutex` alias/import; if `lib.rs` uses `parking_lot`, match it — the memory ops are sync and O(1) either way.)

- [ ] **Step 2: Add `ProdWatchdogSensor`**

```rust
struct ProdWatchdogSensor {
    telemetry: Arc<crate::network_health::CommunityRelayServingTelemetry>,
    resolver: crate::reachability::ReachabilityResolver, // clone-able handle; adjust path to the actual type
}

impl crate::relay_acceptor_watchdog::ServingSensor for ProdWatchdogSensor {
    fn sample(&self, now_ms: u64) -> crate::relay_acceptor_watchdog::WatchdogInputs {
        let last_served_ms = self.telemetry.last_served_ms();
        let connected_peers = self
            .resolver
            .supervisor()
            .map(|h| crate::network_health::count_peer_states(&h.states_snapshot()).connected)
            .unwrap_or(0);
        crate::relay_acceptor_watchdog::WatchdogInputs { now_ms, last_served_ms, connected_peers }
    }
}
```

> Note for the implementer: verify the exact `ReachabilityResolver` type path and that `supervisor()` returns an `Option<H>` where `H::states_snapshot() -> Vec<([u8;32], PeerStateWire)>` (confirmed at `reconnect_supervisor.rs:388`; the resolver accessor pattern is `self.resolver.supervisor().map(|h| h.states_snapshot())`, mirrored from `network_health.rs:1972-1979`). `count_peer_states` takes `&[([u8;32], PeerStateWire)]` and returns `PeerStateCounts { connected, .. }` (`network_health.rs:600`).

- [ ] **Step 3: Add a sensor unit test**

```rust
#[cfg(test)]
mod watchdog_sensor_tests {
    use super::*;

    #[test]
    fn sensor_reports_last_served_and_maps_zero_to_none() {
        let tel = Arc::new(crate::network_health::CommunityRelayServingTelemetry::default());
        // never served yet → None
        let sensor = ProdWatchdogSensor { telemetry: tel.clone(), resolver: crate::reachability::ReachabilityResolver::new() };
        let s0 = <ProdWatchdogSensor as crate::relay_acceptor_watchdog::ServingSensor>::sample(&sensor, 42);
        assert_eq!(s0.last_served_ms, None);
        assert_eq!(s0.connected_peers, 0); // no supervisor without a live node
        // record a serve → Some
        tel.record_served(&[7u8; 32]);
        let s1 = <ProdWatchdogSensor as crate::relay_acceptor_watchdog::ServingSensor>::sample(&sensor, 99);
        assert!(s1.last_served_ms.is_some());
    }
}
```

> Note: confirm `CommunityRelayServingTelemetry` has a `Default`/`new()` and that `record_served(&[u8;32])` is callable from a `lib.rs` test (both used at `iroh_community_relay_acceptor.rs:983` and constructed at `lib.rs:11062`). Confirm `ReachabilityResolver::new()` exists (`network_health.rs:5031` region uses `ReachabilityResolver::new()`). If either constructor is not test-reachable, drive the sensor test through whatever constructor the boot path uses; the load-bearing assertion is the `0 → None` mapping and that a recorded serve makes it `Some`.

- [ ] **Step 4: Run + fmt + clippy, then commit**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(sensor_reports_last_served)'
cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd .. && git add src-tauri/src/lib.rs
git commit -m "ZEB-803: process-global watchdog memory + ProdWatchdogSensor"
```

---

### Task 6: `ProdWatchdogActuator` + boot spawn + health wiring (`lib.rs`, `network_health.rs`)

**Files:**
- Modify: `src-tauri/src/lib.rs` (actuator, restart capability, boot spawn, `NodeState` handle field)
- Modify: `src-tauri/src/network_health.rs` (populate the snapshot field from the memory handle + live telemetry/resolver)

**Interfaces:**
- Consumes: everything above; `stop_inner(&Mutex<NodeState>, Option<u64>) -> bool` (`lib.rs:2526`), `start_node_inner` (`lib.rs:3667`), `IrohEndpoint::network_change()` (used at `lib.rs:9351`), `AbortOnDrop` (`iroh_transport_lifecycle.rs:88`), the node generation field `stop_inner` compares against, mint's injected-restart pattern (`owner_commands.rs:1435/1467`).
- Produces: the spawned watchdog task; a populated `relay_acceptor_watchdog` snapshot field. This is the live-integration task — mostly review-verified.

- [ ] **Step 1: Add the restart capability + actuator**

`RemediationActuator` needs a way to restart the node that works in both GUI (`AppHandle`) and headless (owned `Arc<Mutex<NodeState>>`). Model it as a boxed capability, mirroring mint's injected `restart: F` (`owner_commands.rs:1467`):

```rust
/// Performs a generation-guarded full-node restart. Constructed at boot with
/// whatever restart machinery the caller has (AppHandle sink or owned state).
type RestartCapability = Arc<dyn Fn() -> futures::future::BoxFuture<'static, ()> + Send + Sync>;

struct ProdWatchdogActuator {
    endpoint: Arc<crate::iroh_endpoint::IrohEndpoint>, // the wrapper `network_change()` lives on
    restart: RestartCapability,
}

#[async_trait::async_trait]
impl crate::relay_acceptor_watchdog::RemediationActuator for ProdWatchdogActuator {
    async fn probe_network(&self) {
        // same primitive the sleep/wake resume detector uses (lib.rs:9351)
        self.endpoint.network_change().await;
    }
    async fn restart_node(&self) {
        (self.restart)().await;
    }
}
```

The `restart` closure body performs the generation-guarded restart, exactly as mint does:

```rust
// built at boot, capturing the owned Arc<Mutex<NodeState>> (headless) or an
// AppHandle-derived equivalent, plus the params start_node_inner needs.
let restart: RestartCapability = {
    let state = owned_state.clone(); // Arc<Mutex<NodeState>>
    // ...capture the other start_node_inner inputs the mint restart closure captures...
    Arc::new(move || {
        let state = state.clone();
        Box::pin(async move {
            // read the current generation the same way mint/stop_inner expect
            let gen = { state.lock().current_generation() }; // adjust to the real accessor
            if !crate::stop_inner(&state, Some(gen)) {
                tracing::warn!("ZEB-803 watchdog: restart skipped — generation changed (another restart in flight)");
                return;
            }
            if let Err(e) = crate::start_node_inner(/* endpoint, sink, app, &state, owned */).await {
                tracing::error!(error = ?e, "ZEB-803 watchdog: start_node_inner failed after stop — marking escalated");
                let mut m = watchdog_memory().lock();
                m.phase = crate::relay_acceptor_watchdog::Phase::Escalated;
            }
        })
    })
};
```

> Implementer note: this closure is the one genuinely codebase-specific piece. Read mint's restart closure (`owner_commands.rs:1435-1467`) and copy its capture set and its `stop_inner` + `start_node_inner` call shape verbatim, substituting the watchdog's escalation-on-error. Do NOT invent a new restart path. Confirm the generation accessor name (`stop_inner`'s `expected_gen` is compared against a `NodeState` generation counter — find its getter). If `stop_inner`/`start_node_inner`'s `join()` on the event-loop thread blocks a runtime worker when called from the watchdog task, wrap the call in `tokio::task::spawn_blocking` (implementation detail — the mint path's behavior is the reference).

- [ ] **Step 2: Spawn the watchdog at boot + store the handle**

In `start_node_inner`, after the community-relay acceptors are installed and the `NetworkHealthService` is built (near `lib.rs:11007-13160`), and where the endpoint Arc, serving telemetry Arc, and reachability resolver are all in scope:

```rust
{
    use crate::relay_acceptor_watchdog::*;
    let cadence_ms = crate::community_relay_pull_driver::COMMUNITY_RELAY_PULL_INTERVAL_MS; // verify path/const
    let cfg = WatchdogConfig {
        cadence_ms,
        stale_multiplier: 3,
        eval_interval_ms: cadence_ms / 3,
        tier1_cooldown_ms: cadence_ms.saturating_mul(2),
        tier2_cooldown_ms: cadence_ms.saturating_mul(2),
        max_restarts: 3,
    };
    let sensor = ProdWatchdogSensor { telemetry: Arc::clone(&serving_telemetry), resolver: reachability_resolver.clone() };
    let actuator = ProdWatchdogActuator { endpoint: Arc::clone(&ep_arc), restart: restart.clone() };
    struct SystemClock;
    impl Clock for SystemClock { fn now_ms(&self) -> u64 { crate::network_health::now_ms() } }
    let wd = RelayAcceptorWatchdog::new(cfg, watchdog_memory(), sensor, actuator, SystemClock);
    let handle = tokio::spawn(wd.run(shutdown_rx.clone())); // reuse the node's existing shutdown watch
    guard.relay_watchdog_handle = Some(crate::iroh_transport_lifecycle::AbortOnDrop::new(handle));
}
```

Add the field to `NodeState`:

```rust
    relay_watchdog_handle: Option<crate::iroh_transport_lifecycle::AbortOnDrop>,
```

> Implementer note: use the node's existing shutdown `watch::Receiver<bool>` (the same one the event loop uses, `event_loop.rs:995`) so a `stop_inner` cleanly ends the loop; the `AbortOnDrop` in `NodeState` is the backstop. Confirm `COMMUNITY_RELAY_PULL_INTERVAL_MS` is `pub`/reachable (`community_relay_pull_driver.rs:54-59`); if private, promote it or read the same source the driver uses. Confirm `AbortOnDrop::new` exists (or construct the tuple struct directly, `iroh_transport_lifecycle.rs:88`).

- [ ] **Step 3: Populate the health snapshot field**

In `network_health.rs`, where the snapshot is assembled (the `None` placeholder from Task 4), compute the live block from the memory handle + telemetry + resolver:

```rust
    relay_acceptor_watchdog: {
        let mem = *crate::watchdog_memory().lock(); // WatchdogMemory is Copy
        let staleness_ms = /* telemetry last_served_ms */.map(|t| now_ms().saturating_sub(t));
        let connected = /* count_peer_states(&supervisor.states_snapshot()).connected, or 0 */;
        Some(RelayAcceptorWatchdogHealth::from_parts(&mem, staleness_ms, connected))
    },
```

> Implementer note: the `NetworkHealthService` already holds the serving-telemetry source (`set_community_relay_serving_source`, `lib.rs:13160`) and the resolver — reuse those exact handles rather than re-plumbing. If `watchdog_memory()` is private to `lib.rs`, expose a `pub(crate)` accessor or pass the memory `Arc` into the health service at construction (preferred — mirrors how the serving telemetry source is injected). Whichever is cleaner in the surrounding code.

- [ ] **Step 4: Full-gate verification**

```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```
Expected: fmt clean, clippy clean, all tests pass (watchdog unit + harness + sensor + health-shape green; no regressions).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/src/network_health.rs
git commit -m "ZEB-803: ProdWatchdogActuator (network_change + gen-guarded restart) + boot spawn + health wiring"
```

---

## Notes for the executor

- Tasks 1–4 are fully self-contained and unit-testable with no live node — do them first and get them green.
- Task 5's sensor test's load-bearing assertion is the `0 → None` mapping; the connected-peer path is exercised for real only with a live supervisor (Task 6 boot), so the unit test asserts the `0` fallback.
- Task 6 is the only live-integration task. Its correctness is primarily review-verified against the mint restart seam it reuses (`owner_commands.rs:1435-1467`) and the `network_change()` call site (`lib.rs:9351`). Do not invent a new restart or rebind path — the design's whole feasibility rests on reusing the proven `stop_inner` + `start_node_inner` sequence.
- If any signature in Tasks 5–6 differs from the anchors here, the anchors are from a read at `main@03a5d1ae`; trust the current source and adjust, keeping the behavior identical.
