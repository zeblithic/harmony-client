# ZEB-803: Self-healing watchdog for the community-relay acceptor — Design

**Ticket:** ZEB-803 — "Community relay acceptor silently stops serving ALL peers
while the process stays alive."
**Status:** Design approved 2026-08-05 (Jake). Two-tier remediation, defaults confirmed.
**Branch:** `zeb-803-relay-acceptor-watchdog` off `origin/main` @ `03a5d1ae`.
**Scope:** client-only (`harmony-client/src-tauri`). No cross-repo.

## Problem

A node's community-relay serving can silently stop: all peers cease being served
within minutes of each other, the process keeps running and logging, and no error
or health surface reflects it. Only a restart recovers — and a manual restart
"does not hold" (the ticket documents a recurrence ~24 min later). Two failure
modes are documented and indistinguishable from the log:

1. **The accept loop stopped accepting** (dead loop).
2. **Inbound reachability changed** (WAN path / endpoint rebind) so peers can no
   longer reach a live acceptor.

The now-primary ask is a **watchdog** that detects the stall and remediates it
automatically.

### Premise verification (against current source, 2026-08-05)

- **No watchdog / rebind / respawn exists.** Only *observability* shipped:
  PR #556 (ZEB-803, `CommunityRelayServingTelemetry`) and PR #566 (ZEB-804,
  per-peer `PeerTrafficRegistry.last_relay_pull_served_ms`).
- ZEB-813/814 (CRDT announce dedup, segmented community-state root) and
  ZEB-864/866 (handshake-*acceptor-family* DoS shed/concurrency gates) are all
  **orthogonal** — none touch the community-relay accept path. ZEB-814 fixed one
  *trigger* (oversized root blob → fetch failure), but this watchdog is
  defense-in-depth for the residual/unknown stall class, not a root-cause fix.

### The pivotal architectural facts (from source investigation)

- **The "acceptor" is not a self-owned task.** It is a stateless dispatcher
  (`IrohCommunityRelayPullAcceptor`) installed into the single shared
  `IrohZenohLinkManager` accept loop (`zenoh_iroh_transport.rs:585-589`), which
  multiplexes *every* inbound ALPN on one iroh `Endpoint`. The loop exits
  silently only when `ep.accept().await` returns `None` (endpoint closed). A
  symptom-based trigger covers both failure modes without diagnosing which
  occurred.
- **There is no in-place iroh rebind, and no partial seam for one.** The
  `link_manager` is welded into the live zenoh session inside the event-loop
  thread (`event_loop.rs:1355-1512`), with no hot-swap API; ~9 long-lived
  subsystems capture the endpoint Arc at construction with no setter to swap it;
  releasing the socket needs `Endpoint::close()`, not an Arc drop. The **only**
  stale-clone-safe "restart" is the full-node `stop_inner` + `start_node_inner`
  path (the same seam mint-restart uses: `owner_commands.rs:1493/1561`;
  identity-rebuild precedent `lib.rs:4003-4018`).
- **A cheap in-place primitive already exists:** `endpoint.network_change().await`
  (`lib.rs:9351`, fired today by the sleep/wake resume detector) re-probes iroh
  paths/relays on the same endpoint with no teardown — a targeted remedy for
  failure mode 2.

## Approach

A **two-tier, guarded, symptom-triggered watchdog**:

- **Tier 1 — `network_change()`** (cheap, in-place). Targets reachability
  (mode 2, the likely cause given the "both directions dead, restart clears it
  temporarily" signature).
- **Tier 2 — full-node restart** (`stop_inner` + `start_node_inner`). The proven
  hammer; covers a genuinely dead loop (mode 1) and anything Tier 1 missed.
- **Escalate** — after `max_restarts` full restarts without recovery, stop acting
  and raise a loud, visible alert (never an infinite storm).

All correctness lives in a **pure decision core**; the live levers sit behind
traits so the harness is fully testable with no iroh (the fleet is stopped — no
live repro).

## Architecture

One new module, `src-tauri/src/relay_acceptor_watchdog.rs`, in two layers:

1. **Pure decision core** — config, input, memory types and `evaluate(...)`. No
   I/O, no clock, no iroh. This carries the entire state machine and the bulk of
   the tests.
2. **Harness** — a background loop that, each tick, samples injected sensors,
   calls `evaluate`, and drives injected actuators. Generic over the
   sensor/actuator/clock traits.

Production sensor/actuator impls live in `lib.rs` (near the boot wiring) so they
can touch `NodeState`, the endpoint Arc, the resolver, the telemetry Arc, and the
restart machinery — keeping the watchdog module free of Tauri/`NodeState` types.

The escalation state lives in a **process-global**, not `NodeState` (see
Component 4) — the single subtlety that makes the max-restart guardrail hold
across a Tier-2 restart.

## Component 1 — the pure decision core

```rust
#[derive(Clone, Copy)]
pub struct WatchdogConfig {
    pub cadence_ms: u64,          // = COMMUNITY_RELAY_PULL_INTERVAL_MS (~450_000)
    pub stale_multiplier: u32,    // N; stall threshold = N * cadence_ms
    pub eval_interval_ms: u64,    // watchdog tick period
    pub tier1_cooldown_ms: u64,   // grace after network_change before re-judging
    pub tier2_cooldown_ms: u64,   // grace after full restart before re-judging
    pub max_restarts: u32,        // consecutive full restarts before Escalate
}

pub struct WatchdogInputs {
    pub now_ms: u64,
    pub last_served_ms: Option<u64>,  // None = never served this telemetry-lifetime
    pub connected_peers: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tier { Probe, Restart }

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Normal,
    Cooldown { since_ms: u64, tier: Tier },
    Escalated,
}

pub struct WatchdogMemory {
    pub served_ever: bool,            // sticky across restarts (process-lifetime)
    pub phase: Phase,
    pub consecutive_restarts: u32,    // Tier-2 fires since last recovery
    pub baseline_served_ms: Option<u64>, // last_served_ms captured at the last action
    pub last_action_ms: Option<u64>,  // for the health surface
    pub last_action_tier: Option<Tier>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict { Hold, ProbeNetwork, RestartNode, Escalate }

pub fn evaluate(cfg: &WatchdogConfig, inputs: &WatchdogInputs, mem: &mut WatchdogMemory) -> Verdict;
```

### State machine (exact semantics `evaluate` implements)

Sticky first: `if inputs.last_served_ms.is_some() { mem.served_ever = true }`.

**Recovery test** (never raw staleness — a successful remedy can't clear staleness
until the next remote pull, ~1 cadence away):

```text
recovered = match inputs.last_served_ms {
    Some(c) => mem.baseline_served_ms.map_or(true, |b| c > b),
    None    => false,
}
```

(Post-restart the telemetry Arc is fresh so `last_served_ms` resets to `None`;
once the rebuilt node serves anyone, its fresh timestamp exceeds the pre-restart
baseline → recovered. During the None window → not recovered → keep waiting.)

**Stall detection** (Normal phase only — all three gates must hold):

```text
stall = mem.served_ever
     && inputs.connected_peers > 0
     && match inputs.last_served_ms {
            Some(ts) => inputs.now_ms.saturating_sub(ts) > cfg.stale_multiplier as u64 * cfg.cadence_ms,
            None     => false,
        }
```

The `served_ever` gate is the false-positive guard: a node nobody ever pulls from
never fires.

**Transitions:**

- `Escalated`: if `recovered` → `reset(mem)` (Normal, counters 0), `Hold`. Else
  `Hold` (terminal; no further actions — the health surface stays red).
- `Cooldown { since_ms, tier }`:
  - if `recovered` → `reset(mem)`, `Hold`.
  - else if `now - since_ms < cooldown_for(tier)` → `Hold` (still waiting).
  - else (cooldown elapsed, not recovered) → **fire_restart** (Tier 1 failure
    escalates straight to a restart; Tier 2 failure repeats a restart).
- `Normal`: if not `stall` → `Hold`. Else fire **Tier 1**:
  `mem.baseline_served_ms = inputs.last_served_ms`;
  `mem.phase = Cooldown { since_ms: now, tier: Probe }`;
  record `last_action_*`; return `ProbeNetwork`.

**`fire_restart(cfg, inputs, mem, now)`:**

```text
if mem.consecutive_restarts >= cfg.max_restarts {
    mem.phase = Escalated;
    return Verdict::Escalate;
}
mem.consecutive_restarts += 1;
mem.baseline_served_ms = inputs.last_served_ms;
mem.phase = Cooldown { since_ms: now, tier: Tier::Restart };
record last_action_*;
return Verdict::RestartNode;
```

With `max_restarts = 3`, a persistent stall produces exactly: 1 probe → 3 full
restarts → escalate.

**`reset(mem)`:** `phase = Normal; consecutive_restarts = 0; baseline_served_ms =
None` (leaves `served_ever` sticky, keeps `last_action_*` for the health surface
until the next action overwrites them).

## Component 2 — the harness

```rust
pub trait ServingSensor: Send + Sync {
    fn sample(&self, now_ms: u64) -> WatchdogInputs;
}
pub trait RemediationActuator: Send + Sync {
    fn probe_network(&self) -> BoxFuture<'_, ()>;   // Tier 1
    fn restart_node(&self) -> BoxFuture<'_, ()>;    // Tier 2
}
pub trait Clock: Send + Sync { fn now_ms(&self) -> u64; }

pub struct RelayAcceptorWatchdog<S, A, C> {
    cfg: WatchdogConfig,
    memory: Arc<Mutex<WatchdogMemory>>,   // the process-global handle (Component 4)
    sensor: S,
    actuator: A,
    clock: C,
}
```

The run loop ticks on `tokio::time::interval(cfg.eval_interval_ms)`
(`MissedTickBehavior::Skip`), watching a `watch::Receiver<bool>` shutdown:

```text
loop { select! {
    _ = interval.tick() => self.tick().await,
    _ = shutdown.changed() => break,
}}
```

`tick()`:
1. `let inputs = self.sensor.sample(self.clock.now_ms());`
2. `let verdict = { let mut m = self.memory.lock(); evaluate(&self.cfg, &inputs, &mut m) };`
   (lock released before any await).
3. match: `Hold` → nothing; `ProbeNetwork` → `warn!(...); actuator.probe_network().await`;
   `RestartNode` → `warn!(...); actuator.restart_node().await`;
   `Escalate` → `error!(...)` (memory already marked `Escalated`; health surface reflects it).

`probe_network` / `restart_node` never hold the memory lock (it is dropped in
step 2 before the await).

## Component 3 — production sensor & actuator (in `lib.rs`)

**`ProdWatchdogSensor`** holds `Arc<CommunityRelayServingTelemetry>` and a
`ReachabilityResolver` handle. `sample(now_ms)`:
- `last_served_ms` = the telemetry's `last_served_ms` atomic, mapped `0 → None`
  (the existing "never served" sentinel, `network_health.rs:955`).
- `connected_peers` = `count_peer_states(&resolver.supervisor()?.states_snapshot()).connected`
  (`network_health.rs:600`), `0` if no supervisor yet.

**`ProdWatchdogActuator`** holds `Arc<IrohEndpoint>` and a restart capability:
- `probe_network()` → `endpoint.network_change().await` (same call as
  `lib.rs:9351`); log + swallow errors (the recovery test catches a no-op).
- `restart_node()` → the mint-restart sequence, generation-guarded:
  1. read the current node generation under a short `NodeState` lock;
  2. `stop_inner(state, Some(gen))` — a generation mismatch means someone else
     (mint / user) already restarted → no-op, skip;
  3. `start_node_inner(...)` to rebuild.
  Modeled as an injected `Arc<dyn RestartCapability>` whose prod impl captures the
  `AppHandle` (GUI) or the owned `Arc<Mutex<NodeState>>` (headless) — mirroring
  mint's injected `restart: F` (`owner_commands.rs:1467`). Keeps Tauri/`NodeState`
  types out of the watchdog module. On restart error: `error!` + mark `Escalated`.

The restart runs on the watchdog's spawned task and reuses mint's exact
stop→start sequence; `start_node_inner` locks `NodeState` only in scoped blocks and
releases before awaits, so it is safe to drive from a spawned task (the event-loop
thread `join()` in `stop_inner` follows mint's established pattern; wrap in
`spawn_blocking` if a runtime-worker block proves an issue — an implementation
detail, not a design change).

## Component 4 — process-global escalation memory

```rust
static WATCHDOG_MEMORY: OnceLock<Arc<Mutex<WatchdogMemory>>> = OnceLock::new();
fn watchdog_memory() -> Arc<Mutex<WatchdogMemory>> {
    WATCHDOG_MEMORY.get_or_init(|| Arc::new(Mutex::new(WatchdogMemory::default()))).clone()
}
```

**Why process-global, not a `NodeState` field:** a Tier-2 restart re-runs
`start_node_inner`, which re-spawns the watchdog fresh. If the counters lived in
`NodeState` they would reset to zero every restart, the `max_restarts` cap would
never be reached, and the watchdog would restart forever — the exact storm the
guardrail exists to prevent. A `static` survives the in-process restart, so
`consecutive_restarts`, `phase`, and `served_ever` persist across it. `Default`
= `{ served_ever: false, phase: Normal, consecutive_restarts: 0, baseline_served_ms:
None, last_action_ms: None, last_action_tier: None }`.

## Component 5 — self-observability

Add to the network-health snapshot a block read straight off the process-global:

```rust
pub struct RelayAcceptorWatchdogHealth {
    pub staleness_ms: Option<u64>,       // now - last_served_ms, None if never served
    pub connected_peers: u32,
    pub phase: &'static str,             // "normal" | "cooldown" | "escalated"
    pub consecutive_restarts: u32,
    pub last_action_ms: Option<u64>,
    pub last_action_tier: Option<&'static str>, // "probe" | "restart"
    pub escalated: bool,
}
```

Folded into `network_health_snapshot` next to the existing `community_relay`
field (`network_health.rs:2622-2631`). The block is assembled from two sources —
`phase`, `consecutive_restarts`, `last_action_*`, and `escalated` come from the
process-global `WatchdogMemory`; `staleness_ms` and `connected_peers` are computed
from the live serving telemetry + resolver at snapshot time (the same sources
`ProdWatchdogSensor` reads). Tier actions log WARN; escalation logs ERROR and sets
`escalated: true`. The watchdog's own decisions are thus never a new silent
surface.

## Data flow

1. **Boot** (`start_node_inner`, after the acceptors + health service are wired,
   near `lib.rs:11007-13160`): build `ProdWatchdogSensor` + `ProdWatchdogActuator`,
   fetch `watchdog_memory()`, spawn `RelayAcceptorWatchdog::run(shutdown_rx)` on a
   tokio task, store its handle as `AbortOnDrop` in `NodeState` (torn down on stop,
   re-spawned on restart). The health snapshot also captures a clone of the memory
   handle.
2. **Tick** (every `eval_interval_ms`): sample → `evaluate` → act.
3. **Tier 1**: `network_change()`; baseline recorded; Cooldown{Probe}.
4. **Recovery**: any serve past the baseline → reset to Normal.
5. **Tier 2**: after Tier-1 cooldown without recovery → generation-guarded full
   restart; Cooldown{Restart}; `consecutive_restarts++`.
6. **Escalate**: after `max_restarts` restarts without recovery → ERROR + red
   health field; no further action until a serve resets it.

## Error handling / edge cases

- **`network_change()` / restart failure** → logged; the recovery test drives the
  next tier or escalation; restart failure marks `Escalated` directly.
- **Generation guard** prevents the watchdog from racing a concurrent
  mint/user restart into a double-restart.
- **Post-restart `None` telemetry** is handled by the recovery test's Option
  semantics (a fresh serve's timestamp exceeds the pre-restart baseline).
- **Wall-clock assumption:** `last_served_ms` is wall-clock (`network_health::now_ms`);
  the recovery test assumes forward progress. A backward NTP jump could delay
  recovery detection by ≤ one cycle — acceptable.
- **Memory lock never held across an await** (dropped in `tick` step 2).

## Testing (all local; fleet stopped)

**Unit — `evaluate` state-machine matrix (the bulk of coverage), pure, injected
`now`/inputs:**
1. Healthy (fresh serve) → Hold.
2. `served_ever == false` → Hold regardless of staleness.
3. `connected_peers == 0` → Hold.
4. Stall (all gates) from Normal → `ProbeNetwork`, phase Cooldown{Probe}, baseline set.
5. Cooldown{Probe}, within cooldown, not recovered → Hold.
6. Cooldown{Probe}, recovered → reset → Hold, counters 0.
7. Cooldown{Probe} elapsed, not recovered → `RestartNode`, `consecutive_restarts == 1`.
8. Cooldown{Restart} elapsed, not recovered, under cap → `RestartNode`, counter increments.
9. At cap (`consecutive_restarts == max_restarts`) → `Escalate`, phase Escalated.
10. Escalated + recovered → reset → Hold; Escalated + not recovered → Hold (terminal).
11. `served_ever` sticky across a simulated restart (last_served None then Some-fresh).
12. Full persistent-stall sequence asserts exactly 1 probe + 3 restarts + escalate.

**Harness integration (mocked, logical time):** `tokio::time` paused (per the
wall-clock-budget rule — logical time, not real sleeps); a scripted `Clock`, a
mock `ServingSensor` yielding a sequence, and a recording `RemediationActuator`.
Assert the exact action sequence and that no action fires during a cooldown and
escalation fires at the cap.

**Live remediation** (`network_change()` and the `stop_inner`+`start_node_inner`
restart) is review-verified against the mint seam it reuses — the actuator trait
keeps it out of unit tests; the harness asserts only that the watchdog *invokes*
it correctly.

## Config defaults

| knob | default | rationale |
|---|---|---|
| `cadence_ms` | `COMMUNITY_RELAY_PULL_INTERVAL_MS` (~7m30s) | track the real constant, don't hardcode |
| `stale_multiplier` (N) | **3** (→ ~22m30s) | matches the ticket's "~3× cadence" warning threshold |
| `eval_interval_ms` | cadence / 3 (~2m30s) | detection latency ≈ threshold + one tick (~25m vs. the observed 46m silent tail) |
| `tier1_cooldown_ms` | **2 × cadence** (~15m) | must exceed re-probe + one pull cycle, or we re-fire before the fix can show |
| `tier2_cooldown_ms` | **2 × cadence** (~15m) | must exceed restart + one pull cycle |
| `max_restarts` | **3** | after 3 full restarts without recovery, stop and alert |

## Known limitation (bounded, documented)

A node that served at least once, then had all its relay-pull peers permanently
leave while *other* peers stay connected, could false-trigger (last-served goes
stale, `served_ever` and `connected_peers > 0` hold). Mapping "which connected
peers are relay-pull clients" is not readily available, so the design relies on
the `max_restarts` cap + terminal `Escalate` to bound the damage to ≤ 1 probe + 3
restarts followed by a loud alert — never an infinite loop — and YAGNI-defers peer
classification.

## Files

- **Create:** `src-tauri/src/relay_acceptor_watchdog.rs` — pure decision core +
  generic harness + their unit/harness tests.
- **Modify:** `src-tauri/src/lib.rs` — `pub mod relay_acceptor_watchdog;`; the
  `ProdWatchdogSensor` / `ProdWatchdogActuator` / `RestartCapability` prod impls;
  the process-global `WATCHDOG_MEMORY`; the boot spawn + `AbortOnDrop` handle in
  `NodeState`.
- **Modify:** `src-tauri/src/network_health.rs` — `RelayAcceptorWatchdogHealth`
  struct + its wire field in the snapshot, read from the memory handle.

## Global constraints

- Rust; build/test from `src-tauri/`.
- CI parity gates: `cargo fmt --all -- --check`; `cargo clippy --locked
  --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest
  run --locked --workspace --all-targets --features test-fixtures`.
- No new dependencies. No production behavior change except the watchdog itself
  (the acceptor, accept loop, and existing telemetry are untouched — the watchdog
  only reads existing signals and invokes the two existing levers).
