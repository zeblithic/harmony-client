# ZEB-917 (R6c) — SimNet: deterministic simulation harness — design

**Status:** design (approved to spec 2026-08-14). Study ticket ZEB-917, child of the
Freenet-inspired epic ZEB-909. This document is both the ticket's feasibility
deliverable (**verdict + evidence**) and the design for its v1.

**Goal:** a single-process, virtual-time, seed-replayable N-node simulation harness
("SimNet") that drives harmony-app's real connectivity and CRDT-convergence code
against an in-memory fabric with injectable partitions — so that reconvergence
behavior (R1 island repair, R4 topology, community/channel-log CRDT heal) can be
tested deterministically, without real iroh/zenoh transport or a live multi-machine
fleet.

**Why now:** R4's reconvergence "claim B" (ZEB-930 O1) is only *confirmed-in-direction*
via a hand-calibrated Layer-2 model, because the single-process probe conflates R4's
edge count with CPU contention and we lack live cross-WAN fleet infra. Virtual time
removes the contention confound *logically*. R1's island-repair logic (coverage
predicate + Dormant parole) is exactly the kind of behavior miserable to reproduce
with real NATs and trivial to reproduce as an injected partition. The two test layers
are complementary: the headless fleet exercises real binaries over real networks
(which a DST cannot); the DST exercises partition/reorder/heal determinism (which the
fleet cannot).

**Tech stack:** Rust, single crate `harmony-app` (`src-tauri/`); `tokio` with
`test-util` (already a dev-dependency); `#[tokio::test(start_paused = true)]` (already
the idiom, ~25+ existing tests). No new runtime dependency. **Turmoil is deliberately
NOT used** (see §8).

---

## Global Constraints

- Cargo runs from `src-tauri/`. Gates: `cargo nextest run --locked --workspace
  --all-targets --features test-fixtures`, `cargo clippy --locked --all-targets
  --features test-fixtures --no-deps -- -D warnings`, `cargo fmt --all -- --check`.
- The HLC clock seam (PR2) touches production code and **must preserve the ZEB-831
  `clock_trust` contracts** (§4.2): local-provenance only, and the `Option<u64>` skew
  sentinel (`None` = apply-all, never `0`).
- Deterministic identities come from the existing **production** API
  `PrivateIdentity::from_seed` (`identity.rs:84`) — never a `test-fixtures`-gated
  nonce helper on a production path.
- No worktrees; branch off latest `origin/main`.

---

## 1. Feasibility verdict: **GO**

A seven-pass seam inventory (transport, virtual clock, RNG/determinism, subsystem
testability, event-loop structure, CRDT engine plane, HLC seam) establishes that the
DST-relevant subsystems are already isolated, fake-injected, and clock-seamed for
in-process testing. The single reframing insight:

> **Don't boot nodes — compose subsystems.** `event_loop::run` is bolted to
> process-global singletons (the iroh↔zenoh transport ctx
> `iroh_zenoh_registration.rs:20`, the first-wins profile→data-dir `profile.rs:16`,
> an advisory one-node-per-data-dir lock `lib.rs:30791`) and is deliberately
> "one node per OS process." Instantiating `run` N times is a dead end. But the
> DST-relevant subsystems need **none** of those globals — they are already composed
> together at N=1 by the gateway-dial driver's `harness()`
> (`community_gateway_dial_driver.rs:1129`) and the community-sync two-engine bridge
> (`tests/community_sync/community_sync_integration.rs:192`). The v1 generalizes those
> existing harnesses from 1–2 nodes to N over a partitionable in-memory fabric.

### 1.1 Four-seam scorecard (evidence)

**Transport — PARTIAL, clean where v1 needs it.**
- Dial plane is a `trait PeerDialer` (`iroh_dial_driver.rs:27`) with two existing
  fakes (`RecordingDialer`, ~30 tests; `SupervisorLinkDialer`). Supervisor takes
  `dialer: Arc<dyn PeerDialer>`; production injects the real one at one line
  (`event_loop.rs:1680`).
- Reachability resolver is pure in-memory state; only network touch (pkarr) is behind
  `Arc<dyn ReachabilityFallback>` (`reachability_resolver.rs:244`, `set_fallback_source`).
- Gateway-dial driver is "a feeder, not a dialer" — network only via
  `Arc<dyn GatewayDialCtx>` + `Arc<dyn BeaconResolver>`, both stubbed in tests.
- **The community/channel-log CRDT engine is Sans-IO** — holds no `zenoh::Session`;
  takes `publisher_tx`/`subscriber_rx` as in-memory `mpsc<Vec<u8>>`
  (`community_state_sync.rs:1115-1116`). The zenoh session lives only in the event-loop
  adapter, which "only shuttles opaque sealed bytes" (`event_loop.rs:308`).
- Only *peripheral* sync tasks (address-book receive, voice, presence — ~23
  `zenoh::Session`-typed fns) are hardwired. **None are on the v1 path.** A DST sits
  above the iroh link layer and never instantiates it.

**Virtual clock — PARTIAL; test-only seams are exactly what a harness uses.**
- Time seams exist (`now_ms: u64` threading, `NowFn = Arc<dyn Fn()->u64>`,
  `trait Clock`, resolver clock field) but production never injects — always ambient
  `SystemTime`. For a harness that *constructs subsystems directly* (not via
  `start_node`), the test-only injection points (`with_now_fn`, `set_clock`,
  `jitter_seed`) are the intended vehicle.
- Scheduling is ~310 `tokio::time::sleep` / 32 `tokio::time::interval` (virtualizable
  via pause/advance); the supervisor's Dormant parole *already* runs on paused virtual
  time in tests (`reconnect_supervisor.rs:561`).
- The HLC is un-seamed at ~10 ambient call sites, but its mint kernel is already pure
  (§4). The minimal seam is small.

**RNG / determinism — PARTIAL→READY.**
- Supervisor jitter is already seeded (`jitter_seed: Option<u64>`, `ChaCha8Rng`,
  `reconnect_supervisor.rs:188/553`). Topology/resolver/gateway-driver have zero RNG and
  are uniformly BTree-ordered (**no HashMap-iteration behavioral dependency**).
- Deterministic identities are a production API (`PrivateIdentity::from_seed`,
  `identity.rs:84`), already used as the in-crate test-identity helper.
- The one genuine scheduling race — concurrent dial-completion interleaving in the
  supervisor's `select!` + unbounded `res_rx` — is neutralized by making `SimDialer`
  completions synchronous (no real I/O to race).
- Residual mechanical RNG gaps (`address_book_sync.rs:1031` woken-jitter,
  `open_join_dial.rs:230` OsRng nonce) are off the v1 path (v1 injects reachability
  directly and uses pre-generated identities).

**CRDT convergence plane — READY (data) / PARTIAL (control).**
- Engine `CommunitySyncEngine::new(cfg)` (`community_state_sync.rs:1314`) and
  `ChannelLogEngine` (`community_channel_log_engine.rs:491`) are channel-driven and
  build fully in-memory; `CommunitySyncRegistry` (`community_state_sync.rs:5654`) needs
  no node/zenoh (`content_store: Arc<dyn ContentStore>`, `identity_resolver` trait
  stub, tempdir).
- A partitionable transport is a small extension of the existing 3-line two-node
  forwarder (`community_sync_integration.rs:192`).
- Convergence primitives already exist: `CommunityState: PartialEq`
  (`community_state_crdt.rs:321`, reached via `registry.state_for(&id)`) and
  `RangeFingerprint::finalize() -> [u8;16]` (`channel_rbsr.rs:108`) — the closest analog
  to Freenet's state-root digest. No *unified* node-wide digest exists; the oracle ANDs
  the two per-plane primitives (§5).
- Control gap: each engine is an autonomous `tokio::spawn(internal_task)` with its own
  debounce timer; convergence is observed today by wall-clock polling. The DST drives it
  under `start_paused` + `advance` with a deterministic quiescence detector — **no
  production change required for that.**

---

## 2. Architecture: SimNet

SimNet is a test-only harness (a Rust module compiled into the test/example target)
that hosts **N logical nodes** over **one shared virtual clock** and **one partition
predicate**, with two routing planes:

```
                    ┌──────────────────────── SimNet ───────────────────────┐
                    │  SimClock (reads tokio virtual time)                   │
                    │  Partition predicate: fn(NodeId, NodeId) -> bool       │
                    │                                                        │
   Plane 1 (conn):  │  SimDialer:PeerDialer  ── routes dials, same-side only │
                    │      per node: Resolver + Supervisor + GatewayDriver   │
                    │                                                        │
   Plane 2 (CRDT):  │  SimBus  ── routes sealed mpsc<Vec<u8>>, same-side only │
                    │      per node: SyncRegistry + SyncEngine + ChanLogEngine│
                    └────────────────────────────────────────────────────────┘
```

Each **SimNode** owns one identity (`from_seed(seed_i)`), one injected `SimClock`, and
both planes' subsystem set. A test builds a SimNet of N nodes, drives virtual time with
`advance()`, toggles the partition predicate, and asserts via the plane's oracle.

### 2.1 Substrate: unified virtual clock + determinism

- **Runtime:** `#[tokio::test(start_paused = true)]` (current-thread, virtual time).
- **One clock, both planes.** `SimClock::now_ms()` reads the *tokio* virtual clock
  (`base_ms + tokio::time::Instant::now().duration_since(base).as_millis()`). It is
  injected into every seam: supervisor Instants (already tokio-based), resolver
  (`set_clock`), gateway driver (`with_now_fn`), and the new channel-log `NowFn` (§4).
  A single `tokio::time::advance(d)` then moves the scheduler **and** every HLC stamp
  coherently. This is the deliberate inverse of the documented trap
  (`community_sync_engine_unit.rs:2053`: an injected `std::Instant` clock freezes under
  `pause` because it does *not* read tokio's clock).
- **Determinism inputs:** `PrivateIdentity::from_seed([seed_i; 32])`; supervisor
  `jitter_seed: Some(derived_seed)`; BTree ordering throughout; synchronous `SimDialer`
  completions ⇒ no `select!`/mpsc-arrival race. Result: **same seed → same trace.**

---

## 3. Plane 1 — connectivity (R1 island repair). *(PR1; zero production changes.)*

Generalize the gateway-driver `harness()` from 1 node to N. Each SimNode runs:
- `ReachabilityResolver::new()` with `set_clock(sim_clock)` and
  `set_supervisor(handle)`;
- `run_reconnect_supervisor(handle, sim_dialer, resolver, telemetry, self_id,
  SupervisorConfig{ jitter_seed: Some(_), parole_interval: _, .. })` spawned;
- `CommunityGatewayDialDriver::new(stub_ctx, stub_beacons, resolver, joined_fn,
  traffic_fn).with_now_fn(sim_clock)` with `run_one_pass()` driven on wake/tick.

**SimDialer (`impl PeerDialer`):** `dial(target_id, locator) -> bool` returns
`partition.same_side(self_id, target_id)` **synchronously**; on success it marks the
target reachable/live in the dialing node's resolver/liveness view (mirroring what a
completed real dial feeds). This *is* the partition knob for plane 1.

**Test — `simnet_r1_partition_heal_reconverges`:** 6 nodes, one community, all
mutually reachable ⇒ every node's `coverage_verdict()` == Healthy. Inject a 3+3
partition (predicate drops cross-side dials). Advance virtual time; assert each side's
`coverage_verdict()` → Degraded/Starved for the unreachable coherent subset and those
peers transition Dormant. Heal (predicate all-true). Advance virtual time past
`parole_interval`; assert the **Dormant parole tick** re-arms the dormant peers,
re-resolves, re-dials, and every node returns to Healthy **without restart or external
churn** — the exact ZEB-910 property. Assert no oscillation (Healthy→Degraded→Healthy is
expected; Healthy→…→Degraded while healed is a failure).

---

## 4. Plane 2 — CRDT convergence. *(PR2; the only production change.)*

Each SimNode additionally runs an in-memory `CommunitySyncRegistry` +
`CommunitySyncEngine` (+ `ChannelLogEngine` per channel), constructed with:
- `CommunityRegistryConfig { device_id, content_store: in-mem, identity_resolver:
  MapResolver stub, identity_dir: tempdir, signing_key: from_seed, .. }`;
- engine pub/sub `mpsc<Vec<u8>>` halves wired into the SimBus.

**SimBus (partitionable transport):** extends the existing forwarder. For each node it
drains that node's `publisher_tx` and delivers each sealed frame to the `subscriber_rx`
of every *other same-side* node. Cross-partition frames are **dropped** for the partition
window (mirroring a real outage — the bus does *not* buffer and replay them on heal), so
post-heal reconvergence must come from the anti-entropy/RBSR heartbeat + pull-serve
resync, not from redelivery. That is precisely the heal mechanism the test exercises.
Channel-log engine pub/sub is bridged the same way. Delivery is synchronous within a
virtual-time step.

**Quiescence detector:** under `start_paused`, repeatedly `advance()` in small steps
and pump the SimBus until no engine emits new frames and no debounce/resync timer is
pending — a deterministic "network idle" signal to sample the oracle at.

### 4.1 The HLC clock seam (production change — ~6–8 mechanical edits, 3 files)

The DST needs `ChannelLogEngine` HLC stamps and the owner-state merge skew reference to
come from `SimClock`, not `SystemTime`. The mint kernels are already pure
(`reserve_next_hlc_for_device`, `HlcTick::next`, `merged_now`); only the call sites read
the OS clock. Minimal seam:

1. Add a `now: NowFn` (`Arc<dyn Fn() -> u64 + Send + Sync>`) field to
   `ChannelLogEngineParams` (`community_channel_log_engine.rs:457`), `ChannelLogEngine`
   (`:491`), and `ChannelLogRegistryConfig`; default it to a `SystemTime` closure.
2. Swap the two ambient reads `publish` (`:1086`) and `react` (`:1302`) from
   `SystemTime::now()` to `self.now()`.
3. Thread it through the params build (`:2651`) and the **single production wiring
   point** `ChannelLogRegistry::new(...)` (`lib.rs:7824`). Test config-build sites
   (`lib.rs:37484/40502/43544`) pass a `SystemTime` closure or the sim clock.
4. Owner-state merge: lift the ambient `receiver_now = receiver_now_ms()` sampled at
   `owner_state_sync.rs:290` to a function parameter `receiver_now: Option<u64>` (the
   merge is a stateless per-call function — param-threading, matching ZEB-212).

All other HLC mint paths (pex/relay/butler/profile/governance auto-exec/pending-clear,
`recovery_cli`) stay ambient — they are **not** on the reconvergence test path. Full
crate-wide clock consolidation (~15 modules, two clock contracts) is explicitly out of
scope.

### 4.2 Security contracts the seam MUST preserve (ZEB-831 / `clock_trust`)

`receiver_now_ms()` (`clock_trust.rs:129`) is only a local clock read; the skew-bounding
lives in parameterized helpers (`reject_future`, `clamp_future`,
`wall_exceeds_forward_skew`) with tiers `MAX_FORWARD_SKEW_MS` (5 min) /
`DISPLAY_SKEW_TOLERANCE_MS` (30 min). The seam swaps the clock *source* only and must:

1. **Local-provenance:** the injected clock is the receiver's own (a deterministic sim
   clock qualifies) — **never** a peer-supplied or HLC-adopt value flowing into the
   bound reference.
2. **Keep the `Option<u64>` sentinel:** `None` = "disable the forward bound (apply-all)";
   never collapse to a bare `u64`, and never substitute `Some(0)` (at `now = 0` every
   honest present-day wall exceeds the 5-min bound and *all* honest governance/owner
   state would be rejected — the inversion the invariant forbids). The sim clock returns
   a deterministic **present-day** `Some(now)`.
3. Keep calling the same gates — do not route around them.

A parity test asserts the seam is transparent on the real clock (default closure ⇒
byte-identical behavior to pre-seam).

### 4.3 CRDT test — `simnet_crdt_partition_heal_converges`

N nodes joined to one community with some channel history (all converged). Inject a
partition. During the window, mutate **both** sides (append channel events; a membership
op) — divergent state accrues. Heal. Advance to quiescence. Assert the **convergence
oracle** (§5) passes: all nodes' `CommunityState` are `PartialEq`-equal and every
channel's `RangeFingerprint::finalize()` matches across nodes, with no anomaly. A
companion **seed-replay** test runs the same scenario twice with the *same* seed and
asserts the resulting digests are byte-identical (the replayability property the HLC seam
buys); a different seed may legitimately yield a different trace but must still converge.

---

## 5. Convergence oracle + anomaly layer

No unified node digest exists, so the oracle is assembled per-plane:
- **Membership/owner-state:** for each community, `registry.state_for(&id)` clone and
  compare all nodes pairwise with `CommunityState: PartialEq`
  (`community_state_crdt.rs:332`, exact event-set equality). Cheap pre-check:
  `event_count()` / `materialized_version()`.
- **Channel-log:** for each channel, `range_fingerprint(min,max).finalize()` (16-byte
  digest) compared across nodes.
- **Anomaly layer** (mirrors Freenet's `StateOscillation`/`StalePeer`/`FinalDivergence`):
  - *FinalDivergence* — quiescent but two nodes' digests differ (hard fail).
  - *StalePeer* — one node never reaches the quorum digest within a bounded virtual-time
    budget while others have.
  - *StateOscillation* — a node's digest changes, reverts, and changes again after
    quiescence (indicates a merge non-idempotency or re-offer loop).

The oracle is a test-only helper; it reuses existing public accessors and adds no
production surface.

---

## 6. Scope boundaries (explicitly NOT in v1)

- The peripheral zenoh-receive abstraction (~23 `zenoh::Session` fns: address-book
  receive, voice, presence). The core engine is already Sans-IO; these are not needed to
  test reconvergence.
- Full crate-wide clock consolidation and the non-messaging HLC mint paths.
- Any real iroh/zenoh transport, `event_loop::run`, `start_node`, profile globals, disk
  persistence beyond per-engine tempdirs.
- Turmoil (§8).
- A general fault-injection DSL (latency distributions, reorder knobs, targeted crashes)
  beyond the partition predicate — a v2 extension once the two partition/heal tests prove
  the substrate.

---

## 7. Staging (approved: 2 PRs, one repo)

**PR1 — SimNet substrate + connectivity plane (ZERO production changes).**
SimNet core (SimClock, partition predicate, SimNode), `SimDialer`, N-node composition of
resolver+supervisor+gateway-driver, and `simnet_r1_partition_heal_reconverges`. Pure test
infrastructure on existing seams — low risk, lands fast, proves the harness shape.

**PR2 — CRDT plane + HLC seam (the one production change).**
The ~6–8-line HLC clock seam (§4.1) with its parity test and security-contract assertions,
SimBus, in-memory registry/engine wiring, quiescence detector, the convergence oracle +
anomaly layer, `simnet_crdt_partition_heal_converges`, and the seed-replay test. Isolates
the production seam (and its ZEB-831 contract) for focused review.

Each PR ends with an independently testable deliverable and its own green gate.

---

## 8. Turmoil vs. hand-rolled: hand-rolled, decisively

Turmoil simulates the network by shimming `tokio::net` (TCP/UDP). harmony-app's
transport is iroh 1.0 (QUIC/quinn) + zenoh 1.9 — Turmoil reaches nothing below them,
and SimNet sits *above* the transport at the `PeerDialer` and Sans-IO engine-channel
seams, so Turmoil would buy nothing while adding a heavy dependency. The codebase already
has deep `start_paused` investment and every seam SimNet needs. (Freenet themselves reserve
Turmoil for mid-simulation fault injection and use a *direct* paused runner for their
500-node scale path — the same split we adopt: hand-rolled direct runner now, richer
fault injection later if warranted.)

---

## 9. Open risks

- **Scheduling determinism across tokio versions.** current-thread + synchronous sim
  transport + seeded RNG gives same-binary same-seed reproducibility; strict cross-version
  bit-identity is not claimed for v1 and not required by either test (both assert
  *convergence*, and the replay test asserts same-binary reproducibility).
- **Quiescence detection correctness.** The detector must not sample the oracle before
  anti-entropy completes (a false FinalDivergence) nor loop forever (a genuinely stuck
  node must surface as StalePeer within a bounded virtual-time budget). The budget is an
  explicit test parameter.
- **Engine autonomy vs. stepping.** v1 drives the autonomous `internal_task` under
  `start_paused` + `advance` rather than extracting a synchronous `step(now)` seam. If
  quiescence proves flaky, a follow-up may extract a step seam (a larger production
  change, deferred).
- **HLC seam completeness.** The minimal seam covers the messaging + owner-state lane
  only. A test that mutates via an un-seamed mint path (e.g. governance auto-exec) would
  reintroduce clock nondeterminism; v1 tests deliberately exercise only channel-append +
  membership on the seamed lane.
