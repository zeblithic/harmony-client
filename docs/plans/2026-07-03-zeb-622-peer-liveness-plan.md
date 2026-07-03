# ZEB-622 Per-Peer Liveness State Machine + Real PeerHealth — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** One passive per-peer liveness state machine (`Connected(Direct|Relay, rtt)` / `Degraded` / `Disconnected(since)`) fusing registry connect/drop edges, iroh 1.0 path events, and zenoh transport events — making `PeerHealth.connection_mode`/`rtt_ms`/`last_seen_ms` live, replacing the seen-zid transport-epoch backfill gate with real Disconnected→Connected edges (fixes same-zid-flap never-re-arms), and wiring the three deferred dial-ring markers.

**Architecture:** New pure-logic module `peer_liveness.rs` mirroring `reconnect_supervisor.rs`'s shipped pattern (cheap-clone handle + lock-guarded state map + wire-projection enum + producers wired from outside). Producers: the `swap_zenoh_conn` choke point (both directions hold the live iroh `Connection`) feeds up-edges and spawns a per-connection path-watcher task; the existing drop watcher feeds down-edges; the ZEB-620 zenoh listener grows a `Put` arm. Consumers: `transport_epoch_tx` (backfill re-arm), the rate-limited `network-health-changed` pipeline, and `NetworkHealthService::snapshot` (the single fusion read point — liveness transport states + supervisor Retrying/Dormant + presence last-seen).

**Tech Stack:** Rust (tokio, iroh 1.0.1 path API, zenoh 1.9.0 `unstable` transport events), Svelte/TS panel, cargo-nextest (paused-time tests + hermetic 2-endpoint tests under the `iroh-endpoint` throttle group).

## Global Constraints

- **Deliberate scope reductions vs the ticket text (documented deviations, cite in PR):** (a) `Dormant` stays supervisor-owned — the fused view lives in `NetworkHealthService::snapshot`, which joins liveness transport states with the existing supervisor snapshot; duplicating Dormant in liveness would create two sources of truth. (b) Presence roster edges stay identity-free as liveness *inputs* (the roster is keyed by device signing key, not iroh node id — no mapping exists); presence contributes **owner-level** `last_seen_ms` via a sync cache instead. (c) `nat_classification` stays `Unknown` (passive design has no NAT probe; out of scope).
- **iroh 1.0.1 path API (exact, source-verified):** `Connection::paths() -> PathList<'_>` (snapshot), `Connection::path_events() -> PathEventStream` (`'static`, movable into a task; `impl Stream<Item = PathEvent>`); `PathEvent::{Opened, Closed{last_stats}, Selected, Lagged{missed}}` (`#[non_exhaustive]` — match must have a wildcard arm); per-path view `Path<'a>`: `.is_selected()`, `.is_ip()`, `.is_relay()`, `.rtt() -> Duration`, `.remote_addr() -> &TransportAddr` (`TransportAddr::{Relay(RelayUrl), Ip(SocketAddr), Custom(..)}`, `#[non_exhaustive]`). `Path` borrows the `Connection` — it CANNOT cross a task boundary; the watcher task owns a `Connection` clone and re-reads `paths()` on each event. There is NO whole-connection RTT and NO selected-path accessor: filter `paths().iter().find(|p| p.is_selected())`. Streams need `use n0_future::StreamExt;`. `Connection::stable_id() -> usize` is the identity guard (already used by the registry).
- **State machine states (exact):** `Connected { mode: Direct|Relay, rtt_ms: Option<u32>, since_ms }` / `Degraded { since_ms }` (conn registered but no selected path known — startup transient or selected-path lost) / `Disconnected { since_ms }`. Per-slot: `conn_id: Option<usize>` identity guard (only the current connection's reports apply), `ever_connected: bool`, `last_connected_ms: Option<u64>`.
- **Transport-epoch bump rule (exact):** bump `transport_epoch_tx` (`watch::Sender<u64>`, `send_modify(|e| *e = e.wrapping_add(1))`) on every up-edge — a peer entering `Degraded`/`Connected` from absent or `Disconnected`. The zid-poll gate in `event_loop.rs` switches from an accumulating never-forgets set to a **previous-snapshot diff** (`detect_up_edges`), covering LAN-only zenoh peers; `TRANSPORT_SEEN_ZIDS_CAP` and `merge_peers_detect_new` are deleted. A LAN flap shorter than the ~5s poll can be missed (documented); iroh peers are covered event-driven by liveness.
- **Ring markers (exact gating):** supervisor-side, on real state transitions only — `record_reconnected` on (non-Connected)→Connected when `ever_connected` was already true; `record_retrying` only on Connected→Retrying (the drop re-arm, NOT every ladder rung); `record_dormant` on entering Dormant. Supervisor slots gain `ever_connected: bool`.
- **Wire compatibility:** `ConnectionMode` gains `Degraded` (additive; Rust serde camelCase → `"degraded"`, TS union extends). `PeerHealth` keys stay camelCase via struct-level `rename_all` — poll/e2e assertions use the DTO's serde camelCase key. `NetworkHealthSnapshot.schema_version` bumps 3→4 (update the pin tests that assert 3).
- **Rate limiting:** liveness never emits events directly — it bumps a `changed` `watch::Sender<u64>`; a bridge task in `lib.rs` forwards bumps to `nh.notify()` (existing 2s-window rate limiter). No new event names.
- **Hermetic-test conventions:** real-endpoint tests live in `zenoh_iroh_transport.rs` `#[cfg(test)]` (matched by the nextest `iroh-endpoint` throttle group) or carry names matching `test(liveness)` — Task 2 adds `test(liveness)` to `.config/nextest.toml`'s group filter. Real-time test configs use `dial_timeout: Duration::from_secs(300)` convention (slow-under-throttle ≠ hung). Poll loops assert with bounded `poll_until(cond, 60s, 25ms)`.
- **Gates per task:** `cd src-tauri && cargo fmt --all` + `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` + targeted `cargo nextest run --locked --features test-fixtures -E '<filter>'`. ONE cargo invocation at a time in `src-tauri`. Full sweep only in the final task. Frontend tasks: `npx tsc --noEmit` + `npx vitest run` from repo root.
- **Commit style:** `feat|fix|test|docs: <what> (ZEB-622)`, commit at the end of every task.

---

## File Structure

- **Create** `src-tauri/src/peer_liveness.rs` — state machine, `LivenessHandle`, `LivenessStateWire`, `run_conn_path_watcher`, unit tests. Pure logic; iroh types appear ONLY in `run_conn_path_watcher` (the task fn), so the state machine is testable without endpoints.
- **Modify** `src-tauri/src/lib.rs` — `pub mod peer_liveness;`, notify bridge task at NetworkHealthService boot.
- **Modify** `src-tauri/src/zenoh_iroh_transport.rs` — liveness OnceLock + up/down calls at the registry choke points + path-watcher spawn; hermetic tests.
- **Modify** `src-tauri/src/event_loop.rs` — construct/install handle, epoch-tx registration, zenoh listener `Put` arm, zid-poll gate replacement.
- **Modify** `src-tauri/src/reachability_resolver.rs` — `set_liveness`/`liveness()` (mirrors `set_supervisor`/`supervisor()`).
- **Modify** `src-tauri/src/reconnect_supervisor.rs` — `ever_connected` + ring-marker calls.
- **Modify** `src-tauri/src/network_health.rs` — `LivenessSnapshot` trait + fusion in `snapshot()`, `ConnectionMode::Degraded`, `PresenceLastSeenCache`, honest self-test mode, schema_version 4.
- **Modify** `src-tauri/src/community_presence.rs` — feed `PresenceLastSeenCache` on beacon apply.
- **Modify** `src/lib/types/network-health.ts` + `src/lib/components/NetworkHealthView.svelte` (+ its test) — Degraded, supervisor counts, marker icons.
- **Modify** `.config/nextest.toml` — add `test(liveness)` to the throttle-group filter.

---

### Task 1: `peer_liveness.rs` core state machine

**Files:**
- Create: `src-tauri/src/peer_liveness.rs`
- Modify: `src-tauri/src/lib.rs` (add `pub mod peer_liveness;` next to `pub mod reconnect_supervisor;`)

**Interfaces:**
- Consumes: nothing (pure logic; `now_ms()` helper copied from `reconnect_supervisor.rs`).
- Produces (later tasks rely on these EXACT signatures):
  - `LivenessHandle::new() -> Self` (Clone)
  - `on_transport_up(&self, peer: [u8; 32], conn_id: usize)`
  - `on_transport_up_external(&self, peer: [u8; 32])` — zenoh-view up-edge with no `Connection`; acts only if the peer is absent or `Disconnected` (never clobbers a conn-backed state)
  - `report_path(&self, peer: [u8; 32], conn_id: usize, selected: Option<(LivenessMode, u32)>, min_relay_rtt_ms: Option<u32>)`
  - `on_transport_down(&self, peer: [u8; 32], conn_id: usize)`
  - `set_transport_epoch_tx(&self, tx: tokio::sync::watch::Sender<u64>)` (install-once, idempotent-ignore on second call)
  - `changed_rx(&self) -> tokio::sync::watch::Receiver<u64>`
  - `states_snapshot(&self) -> Vec<([u8; 32], LivenessStateWire)>`
  - `min_relay_rtt_ms(&self) -> Option<u32>`
  - `pub enum LivenessMode { Direct, Relay }` (Copy, serde camelCase)
  - `pub enum LivenessStateWire { Connected { mode: LivenessMode, rtt_ms: Option<u32>, since_ms: u64 }, Degraded { since_ms: u64 }, Disconnected { since_ms: u64 } }` (serde camelCase, `tag = "kind"` — same encoding as `PeerStateWire`)
  - `pub async fn run_conn_path_watcher(handle: LivenessHandle, peer: [u8; 32], conn: iroh::endpoint::Connection)` (implemented in Task 2; declare here NOTHING — the fn lives in this module but lands in Task 2 so Task 1 stays endpoint-free)

- [ ] **Step 1: Write the failing tests** — create the module with types + stub methods (`todo!()`-free: implement state transitions as you go; TDD at function granularity). Tests to write first (all `#[tokio::test(start_paused = true)]` except pure ones), in `#[cfg(test)] mod tests`:

```rust
fn peer(n: u8) -> [u8; 32] { [n; 32] }

#[tokio::test(start_paused = true)]
async fn up_edge_bumps_epoch_and_changed() {
    let h = LivenessHandle::new();
    let (tx, rx) = tokio::sync::watch::channel(0u64);
    h.set_transport_epoch_tx(tx);
    let mut changed = h.changed_rx();
    let before_change = *changed.borrow_and_update();
    h.on_transport_up(peer(1), 11);
    assert_eq!(*rx.borrow(), 1, "up-edge bumps transport epoch");
    assert!(*changed.borrow_and_update() > before_change, "changed watch bumped");
    let snap = h.states_snapshot();
    assert!(matches!(snap.as_slice(), [(p, LivenessStateWire::Degraded { .. })] if *p == peer(1)),
        "up-edge without a path report is Degraded (link up, path unknown)");
}

#[tokio::test(start_paused = true)]
async fn duplicate_up_same_conn_does_not_double_bump() {
    let h = LivenessHandle::new();
    let (tx, rx) = tokio::sync::watch::channel(0u64);
    h.set_transport_epoch_tx(tx);
    h.on_transport_up(peer(1), 11);
    h.on_transport_up(peer(1), 11);
    assert_eq!(*rx.borrow(), 1, "same conn re-report is not a new up-edge");
}

#[tokio::test(start_paused = true)]
async fn path_report_promotes_to_connected_and_stale_conn_ignored() {
    let h = LivenessHandle::new();
    h.on_transport_up(peer(1), 11);
    h.report_path(peer(1), 11, Some((LivenessMode::Direct, 12)), None);
    assert!(matches!(h.states_snapshot().as_slice(),
        [(_, LivenessStateWire::Connected { mode: LivenessMode::Direct, rtt_ms: Some(12), .. })]));
    // A superseded connection's watcher must not clobber the fresh state.
    h.report_path(peer(1), 10, Some((LivenessMode::Relay, 99)), None);
    assert!(matches!(h.states_snapshot().as_slice(),
        [(_, LivenessStateWire::Connected { mode: LivenessMode::Direct, .. })]),
        "stale conn_id report ignored");
    // Selected path lost on the CURRENT conn → Degraded.
    h.report_path(peer(1), 11, None, None);
    assert!(matches!(h.states_snapshot().as_slice(), [(_, LivenessStateWire::Degraded { .. })]));
}

#[tokio::test(start_paused = true)]
async fn down_edge_and_same_zid_flap_re_bumps_epoch() {
    let h = LivenessHandle::new();
    let (tx, rx) = tokio::sync::watch::channel(0u64);
    h.set_transport_epoch_tx(tx);
    h.on_transport_up(peer(1), 11);
    h.on_transport_down(peer(1), 11);
    assert!(matches!(h.states_snapshot().as_slice(), [(_, LivenessStateWire::Disconnected { .. })]));
    assert_eq!(*rx.borrow(), 1, "down is not an up-edge");
    // SAME peer reconnects (new conn id) — the exact case the seen-zid gate missed.
    h.on_transport_up(peer(1), 12);
    assert_eq!(*rx.borrow(), 2, "same-peer flap re-bumps the epoch");
    // Stale down from the OLD conn must not kill the fresh link.
    h.on_transport_down(peer(1), 11);
    assert!(matches!(h.states_snapshot().as_slice(), [(_, LivenessStateWire::Degraded { .. })]),
        "superseded conn's down-edge ignored");
}

#[tokio::test(start_paused = true)]
async fn external_up_only_acts_when_disconnected_or_absent() {
    let h = LivenessHandle::new();
    let (tx, rx) = tokio::sync::watch::channel(0u64);
    h.set_transport_epoch_tx(tx);
    h.on_transport_up_external(peer(1)); // absent → Degraded + bump
    assert_eq!(*rx.borrow(), 1);
    h.on_transport_up(peer(2), 22);
    h.report_path(peer(2), 22, Some((LivenessMode::Relay, 40)), Some(40));
    h.on_transport_up_external(peer(2)); // conn-backed Connected → no-op
    assert_eq!(*rx.borrow(), 2, "external up on a live peer is a no-op");
    assert!(h.states_snapshot().iter().any(|(p, s)|
        *p == peer(2) && matches!(s, LivenessStateWire::Connected { mode: LivenessMode::Relay, .. })));
}

#[tokio::test(start_paused = true)]
async fn min_relay_rtt_across_peers() {
    let h = LivenessHandle::new();
    h.on_transport_up(peer(1), 11);
    h.report_path(peer(1), 11, Some((LivenessMode::Direct, 5)), Some(80));
    h.on_transport_up(peer(2), 22);
    h.report_path(peer(2), 22, Some((LivenessMode::Relay, 60)), Some(60));
    assert_eq!(h.min_relay_rtt_ms(), Some(60));
    h.on_transport_down(peer(2), 22);
    assert_eq!(h.min_relay_rtt_ms(), Some(80), "disconnected peer's relay rtt drops out");
}
```

Also a serde pin test (pure `#[test]`): `serde_json::to_value(LivenessStateWire::Connected { mode: LivenessMode::Direct, rtt_ms: Some(12), since_ms: 5 })` == `json!({"kind":"connected","mode":"direct","rttMs":12,"sinceMs":5})`.

- [ ] **Step 2: Run tests, verify they fail** — `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(peer_liveness)'`. Expected: compile errors first, then assertion failures as you scaffold.

- [ ] **Step 3: Implement the module.** Core shape (complete; mirror `reconnect_supervisor.rs` idioms — `Arc<Inner>`, `std::sync::Mutex`, `now_ms()`):

```rust
//! peer_liveness.rs — ZEB-622: passive per-peer transport liveness.
//!
//! One state machine fuses the ZEB-616 registry's connect/drop edges (both
//! directions), iroh 1.0 path events (Direct vs Relay + per-path RTT), and
//! zenoh transport events into per-peer `Connected/Degraded/Disconnected`.
//! Consumers: the transport-epoch backfill re-arm (up-edges REPLACE the
//! accumulating seen-zid gate — a same-zid flap now re-arms), the rate-limited
//! network-health-changed pipeline (via `changed_rx`), and
//! `NetworkHealthService::snapshot`, which joins these transport states with
//! the reconnect supervisor's Retrying/Dormant for the fused PeerHealth view.
//! `Dormant` deliberately stays supervisor-owned (single source of truth).

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LivenessMode { Direct, Relay }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum LivenessStateWire {
    Connected { mode: LivenessMode, rtt_ms: Option<u32>, since_ms: u64 },
    Degraded { since_ms: u64 },
    Disconnected { since_ms: u64 },
}

#[derive(Debug, Clone)]
enum SlotState {
    Connected { mode: LivenessMode, rtt_ms: Option<u32>, since_ms: u64 },
    Degraded { since_ms: u64 },
    Disconnected { since_ms: u64 },
}

#[derive(Debug)]
struct PeerSlot {
    state: SlotState,
    conn_id: Option<usize>,          // identity guard — only this conn's reports apply
    min_relay_rtt_ms: Option<u32>,   // min over the CURRENT conn's relay paths
    ever_connected: bool,
    last_connected_ms: Option<u64>,
}

struct Inner {
    slots: Mutex<HashMap<[u8; 32], PeerSlot>>,
    epoch_tx: OnceLock<tokio::sync::watch::Sender<u64>>,
    changed_tx: tokio::sync::watch::Sender<u64>,
}

#[derive(Clone)]
pub struct LivenessHandle { inner: Arc<Inner> }
```

Transition rules (implement in the handle methods; every method takes the lock once, computes `was_up = matches!(state, Connected|Degraded)` before mutating, and after releasing the lock: if `!was_up && is_now_up` → `epoch_tx.send_modify(+1)`; on ANY slot change → `changed_tx.send_modify(+1)`):
  - `on_transport_up(peer, conn_id)`: if slot exists with `slot.conn_id == Some(conn_id)` → no-op. Else set `conn_id = Some(conn_id)`, `min_relay_rtt_ms = None`, and if not already `Connected` under a DIFFERENT conn (superseding swap: keep Connected-ness? NO — a swap means the old conn is being replaced; reset to `Degraded { since_ms: now }` until the new conn's first path report) → `Degraded`.
  - `report_path(peer, conn_id, selected, min_relay)`: no slot or `slot.conn_id != Some(conn_id)` → ignore. `selected = Some((mode, rtt))` → `Connected { mode, rtt_ms: Some(rtt), since_ms: keep-if-already-Connected-else-now }`, `ever_connected = true`, `last_connected_ms = Some(now_ms())`; `selected = None` → `Degraded { since_ms: now }`. Always store `min_relay_rtt_ms = min_relay`.
  - `on_transport_down(peer, conn_id)`: only if `slot.conn_id == Some(conn_id)` → `Disconnected { since_ms: now }`, `conn_id = None`, `min_relay_rtt_ms = None`.
  - `on_transport_up_external(peer)`: only if absent or `Disconnected` → `Degraded { since_ms: now }` with `conn_id = None` (a later registry up installs the real conn id).
  - `min_relay_rtt_ms()`: min over slots whose state is `Connected`/`Degraded`.

- [ ] **Step 4: Run tests to green** — same filter. Expected: all pass (7 tests).
- [ ] **Step 5: fmt + clippy + commit** — `cargo fmt --all`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `git add -A && git commit -m "feat: peer_liveness core state machine (ZEB-622)"`.

---

### Task 2: transport wiring — registry edges + per-connection path watcher

**Files:**
- Modify: `src-tauri/src/peer_liveness.rs` (add `run_conn_path_watcher`)
- Modify: `src-tauri/src/zenoh_iroh_transport.rs` (OnceLock field ~`:238`, install fn ~`:304`, `swap_zenoh_conn` ~`:278`, drop watcher ~`:329-352`; hermetic test in `#[cfg(test)]`)
- Modify: `.config/nextest.toml` (add `| test(liveness)` to the `iroh-endpoint` group filter)

**Interfaces:**
- Consumes: Task 1's `LivenessHandle` (`on_transport_up`, `report_path`, `on_transport_down`), iroh 1.0.1 path API per Global Constraints.
- Produces: `IrohZenohLinkManager::set_liveness_handle(&self, h: LivenessHandle) -> Result<(), LivenessHandle>`; every registry install (inbound accept AND outbound `new_link`) reports an up-edge + spawns exactly one path watcher for the new conn; every identity-guarded eviction reports the down-edge.

- [ ] **Step 1: `run_conn_path_watcher` in `peer_liveness.rs`** (the ONLY iroh-typed item in the module):

```rust
/// Refresh cadence for RTT while a connection is quiet (path events fire on
/// open/close/selection change, not on RTT drift).
const RTT_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

/// Watch one connection's paths and feed `handle`. Owns a `Connection` clone:
/// `Path<'_>` borrows the connection and cannot cross tasks, so each event
/// re-reads `conn.paths()`. Exits when the event stream ends (conn closed) —
/// the registry drop watcher owns the Disconnected edge.
pub async fn run_conn_path_watcher(
    handle: LivenessHandle,
    peer: [u8; 32],
    conn: iroh::endpoint::Connection,
) {
    use n0_future::StreamExt;
    let conn_id = conn.stable_id();
    let report = |h: &LivenessHandle| {
        let paths = conn.paths();
        let selected = paths.iter().find(|p| p.is_selected()).map(|p| {
            let mode = if p.is_relay() { LivenessMode::Relay } else { LivenessMode::Direct };
            (mode, p.rtt().as_millis().min(u32::MAX as u128) as u32)
        });
        let min_relay = paths
            .iter()
            .filter(|p| p.is_relay())
            .map(|p| p.rtt().as_millis().min(u32::MAX as u128) as u32)
            .min();
        h.report_path(peer, conn_id, selected, min_relay);
    };
    report(&handle);
    let mut events = conn.path_events();
    let mut tick = tokio::time::interval(RTT_REFRESH_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            ev = events.next() => match ev {
                Some(_) => report(&handle),   // any event → re-read snapshot (incl. Lagged)
                None => break,
            },
            _ = tick.tick() => report(&handle),
        }
    }
}
```

- [ ] **Step 2: link-manager wiring.** Field next to `reconnect` (~`:238`): `liveness: Arc<std::sync::OnceLock<crate::peer_liveness::LivenessHandle>>`. Install fn next to `set_reconnect_handle` (~`:304`), same shape. In `swap_zenoh_conn` (~`:278`, the single choke point both directions pass through with the `Connection` in hand) after the registry insert: 

```rust
if let Some(lh) = self.liveness.get() {
    lh.on_transport_up(*peer_id.as_bytes(), conn.stable_id());
    tokio::spawn(crate::peer_liveness::run_conn_path_watcher(
        lh.clone(),
        *peer_id.as_bytes(),
        conn.clone(),
    ));
}
```

In `spawn_drop_watcher` (~`:329-352`), inside the identity-guarded eviction arm (where the `Dropped` kick fires, ~`:348`): `if let Some(lh) = mgr.liveness.get() { lh.on_transport_down(*peer_id.as_bytes(), conn_id); }` (use whatever self/mgr binding that closure already has).

- [ ] **Step 3: nextest filter** — in `.config/nextest.toml` append `| test(liveness)` inside the existing `filter = '...'` string.

- [ ] **Step 4: hermetic test (real endpoints)** in `zenoh_iroh_transport.rs` `#[cfg(test)]`, modeled on the existing 2-endpoint tests (alice manager + accept loop, bob bare acceptor; reuse the module's existing helpers):

```rust
/// ZEB-622 acceptance (transport half): a real link registers Connected with a
/// live mode + RTT in the liveness map; an explicit remote close lands
/// Disconnected; a re-link (same zid) re-registers Connected — and every
/// up-edge bumps the registered transport-epoch watch (the same-zid flap the
/// seen-zid gate could never re-arm).
#[tokio::test]
async fn liveness_tracks_link_lifecycle_and_flap_bumps_epoch() { /* structure:
    1. build alice + bob endpoints/managers exactly like the ZEB-620 acceptance test;
    2. let liveness = LivenessHandle::new();
       let (etx, erx) = tokio::sync::watch::channel(0u64);
       liveness.set_transport_epoch_tx(etx);
       assert!(alice_mgr.set_liveness_handle(liveness.clone()).is_ok());
    3. alice new_link(bob) → poll_until(60s): states_snapshot has bob as
       Connected { mode: LivenessMode::Direct, rtt_ms: Some(_) } (hermetic
       loopback = Ip path) AND *erx.borrow() == 1;
    4. close bob's inbound conn explicitly (same pattern as the ZEB-620
       acceptance test's Phase 2) → poll_until: bob Disconnected, epoch still 1;
    5. alice new_link(bob) again → poll_until: Connected again AND
       *erx.borrow() == 2  ← the flap re-arm proof at the transport level. */ }
```

Write the real code following the ZEB-620 acceptance test (`supervisor_redials_after_drop_and_get_answers`) for endpoint/manager construction and `poll_until`. No supervisor needed — drive `new_link` directly.

- [ ] **Step 5: run** — `cargo nextest run --locked --features test-fixtures -E 'test(liveness_tracks_link)'` (expect PASS, ~60-120s under throttle). Then fmt + clippy + commit `feat: wire liveness into the zenoh-conn registry + iroh path watcher (ZEB-622)`.

---

### Task 3: event-loop wiring — handle install, zenoh Put arm, zid-poll gate replacement

**Files:**
- Modify: `src-tauri/src/event_loop.rs` (supervisor block ~`:1093-1210`; listener task ~`:1155-1206`; zid poll ~`:3399-3413`; gate fn ~`:6377-6397`; seed ~`:2890-2904`)
- Modify: `src-tauri/src/reachability_resolver.rs` (`set_liveness`/`liveness()` next to `set_supervisor` ~`:124-133`)

**Interfaces:**
- Consumes: `LivenessHandle` (Task 1), `set_liveness_handle` (Task 2), existing `transport_epoch_tx: watch::Sender<u64>` (~`:902`), existing listener's `zid_to_node` resolution (~`:1169-1191`).
- Produces: `ReachabilityResolver::{set_liveness, liveness}` (exact mirror of `set_supervisor`/`supervisor`: `Mutex<Option<LivenessHandle>>`, setter overwrites, getter clones); `detect_up_edges(prev: &mut HashSet<String>, current: &[String]) -> bool` replacing `merge_peers_detect_new`.

- [ ] **Step 1: resolver seam + tests.** Copy the `set_supervisor`/`supervisor()` implementation shape for `set_liveness`/`liveness()`. Unit test: set → get returns a handle whose `states_snapshot()` works.

- [ ] **Step 2: event-loop install.** In `run()`, immediately BEFORE the reconnect-supervisor block (~`:1093`), unconditionally (liveness is useful even if the supervisor block's install-order gate trips):

```rust
let liveness = crate::peer_liveness::LivenessHandle::new();
liveness.set_transport_epoch_tx(transport_epoch_tx.clone());
if let Some(ref ih) = iroh_handles {
    if ih.link_manager.set_liveness_handle(liveness.clone()).is_err() {
        tracing::error!("ZEB-622: liveness handle already installed; keeping the existing one");
    }
    ih.link_manager.resolver().set_liveness(liveness.clone());
}
```

(Note: `transport_epoch_tx` is declared later today (~`:902` region binds it before the loop; the boot zid seed is ~`:2890`) — place this install where BOTH `iroh_handles` and `transport_epoch_tx` are in scope; just before the supervisor block if `transport_epoch_tx` already exists there, otherwise immediately after `transport_epoch_tx` is created. Verify at implementation time and note placement in the report.)

- [ ] **Step 3: zenoh listener Put arm.** In the ZEB-620 listener task (~`:1171`), replace `if event.kind() != SampleKind::Delete { continue; }` with a match on `event.kind()`: `Delete` keeps today's Dropped-kick body; `Put` resolves zid→node with the SAME `zid_to_node` machinery and calls `liveness.on_transport_up_external(node_id)` (clone the handle into the task); other kinds → `continue`. Keep the debug lines symmetrical (`"transport Put for zid {zid} …"`).

- [ ] **Step 4: gate replacement.** Delete `merge_peers_detect_new` (~`:6377-6397`) + `TRANSPORT_SEEN_ZIDS_CAP` (~`:6375`) and their unit tests; add:

```rust
/// ZEB-622: up-edge detector over zid-poll snapshots. Replaces the accumulating
/// seen-zid set (which never forgot, so a same-zid reconnect never re-armed the
/// backfill epoch). An up-edge = a zid present now that was absent in the
/// previous snapshot; `prev` is REPLACED by the current snapshot each call, so
/// a flap longer than one poll interval (~5s) re-fires. Sub-interval LAN flaps
/// can be missed here — iroh peers are covered event-driven by peer_liveness.
fn detect_up_edges(prev: &mut std::collections::HashSet<String>, current: &[String]) -> bool {
    let cur: std::collections::HashSet<String> = current.iter().cloned().collect();
    let any_new = current.iter().any(|z| !prev.contains(z));
    *prev = cur;
    any_new
}
```

Unit tests (pure): new zid → true; unchanged → false; **drop-then-return across two calls → true** (the regression the old gate failed); simultaneous add+remove → true. At the call site (~`:3410`) swap in `detect_up_edges(&mut transport_seen_zids, &refreshed)`; rename the variable to `transport_prev_zids`; keep the seed (~`:2904`) as-is minus the CAP comment.

- [ ] **Step 5: run + commit** — `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(detect_up_edges) | test(peer_liveness) | test(reachability_resolver)'`; fmt + clippy; commit `feat: liveness event-loop wiring — Put arm + up-edge epoch gate (ZEB-622)`.

---

### Task 4: supervisor ring markers (`reconnected` / `retrying` / `dormant`)

**Files:**
- Modify: `src-tauri/src/reconnect_supervisor.rs` (PeerSlot ~`:177-185`, `mark_connected` ~`:260`, `apply_trigger` ~`:544-556`, `apply_result` ~`:666`, `ladder_after_failure` dormant arm ~`:598-600`, the dial task's success arm — REMOVE the "NOTE: no reconnected marker" comment block, it ships now)

**Interfaces:**
- Consumes: `DialTelemetry::{record_reconnected, record_retrying, record_dormant}` (`network_health.rs:270-283`, exact existing signatures `(&self, node_id: [u8; 32], owner: [u8; 16])`).
- Produces: `PeerSlot.ever_connected: bool`; marker emission rules per Global Constraints. **Owner resolution**: the supervisor loop already resolves `owner: [u8; 16]` at dial time; for transitions where no owner is at hand (`mark_connected`, trigger-driven re-arms), resolve via `resolver.resolve_by_node_id(&peer).map(|(o, _)| o.0)` — the loop holds the resolver; `SupervisorHandle::mark_connected` does NOT (it's called from the transport). So: markers that need the loop's context (`retrying`, `dormant`, dial-success `reconnected`) are emitted from the loop/dial task; the inbound-connect `reconnected` marker is emitted from the loop when it OBSERVES the mark (add a `pending_reconnected_marker: bool` slot flag set by `mark_connected` when `ever_connected && !was_connected`, drained by the loop's next pass with resolver+telemetry in hand).

- [ ] **Step 1: failing tests** (extend the module's paused-time suite; `telemetry.summary().recent` exposes the ring):

```rust
#[tokio::test(start_paused = true)]
async fn ring_markers_fire_on_real_edges_only() {
    // succeeding dialer, 1 peer:
    // kick NewPeer → first dial succeeds → NO reconnected marker (first connect);
    // kick Dropped → Connected→Retrying edge → exactly one "retrying" marker;
    // next dial succeeds → "reconnected" marker (ever_connected was true).
    // Assert ring order/counts via telemetry.summary().recent outcomes.
}

#[tokio::test(start_paused = true)]
async fn dormant_marker_fires_once_on_dormancy() {
    // failing dialer, dormant_after small: run ladder past dormancy;
    // exactly ONE "dormant" marker; revival kick + more failures → a second
    // dormancy → second marker (marker-per-transition, not per-rung).
}

#[tokio::test(start_paused = true)]
async fn inbound_reconnect_emits_marker_via_mark_connected() {
    // parking dialer so no dial success interferes: kick NewPeer, mark_connected
    // (first connect — no marker), kick Dropped ("retrying" marker),
    // mark_connected again → "reconnected" marker drained by the loop.
}
```

- [ ] **Step 2: implement.** Slot fields: `ever_connected: bool` (set in every →Connected transition: `mark_connected`, `apply_result` ok-arm) + `pending_reconnected_marker: bool` (set by `mark_connected` when `ever_connected && !matches!(state, Connected)` at entry). Emission sites: (a) dial task success arm — replace the NOTE comment with `if ever_connected_before { telemetry.record_reconnected(peer, owner); }`; thread `ever_connected_before` from the dispatch pass (read slot before `dial_in_flight = true`); (b) `apply_trigger` Dropped-on-Connected re-arm → `record_retrying` — the fn doesn't take telemetry today: pass `&DialTelemetry` + owner-resolution closure down from the loop (both `apply_trigger` call sites are inside the loop where `resolver`+`telemetry` live; test-only callers update accordingly); (c) `ladder_after_failure`'s Dormant transition → `record_dormant` (same threading); (d) the loop's drain pass: any slot with `pending_reconnected_marker` → resolve owner, `record_reconnected`, clear flag.
- [ ] **Step 3: run** — `-E 'test(reconnect_supervisor)'` → all pass (14 = 11 existing + 3 new; existing tests updated for new fn signatures only, no behavior re-blessing). fmt + clippy + commit `feat: wire dial-ring state-edge markers in the supervisor (ZEB-622)`.

---

### Task 5: network-health fusion — live PeerHealth + Degraded + presence last-seen

**Files:**
- Modify: `src-tauri/src/network_health.rs` (ConnectionMode ~`:387-393`; snapshot ~`:777-919`; ProdSupervisorSnapshot region ~`:669-695`; schema_version `:866`; pin tests)
- Modify: `src-tauri/src/community_presence.rs` (beacon apply arm ~`:590-600`)
- Modify: `src-tauri/src/lib.rs` (nh boot ~`:9540-9563`: presence cache + liveness bridge task)

**Interfaces:**
- Consumes: `LivenessStateWire`/`LivenessMode` (Task 1), `resolver.liveness()` (Task 3), `CommunityPresenceMap` beacon apply site, `nh.notify()`.
- Produces:
  - `trait LivenessSnapshot: Send + Sync { fn peer_states(&self) -> Vec<([u8; 32], crate::peer_liveness::LivenessStateWire)>; fn min_relay_rtt_ms(&self) -> Option<u32>; }` + `ProdLivenessSnapshot(Arc<ReachabilityResolver>)` (lazy resolver read, exact `ProdSupervisorSnapshot` pattern) + `NetworkHealthService::set_liveness_source(...)`.
  - `ConnectionMode::Degraded` (wire `"degraded"`).
  - `PresenceLastSeenCache` — `RwLock<HashMap<[u8; 16], u64>>`; `note_seen(owner: [u8; 16], last_seen_ms: u64)` (max-merge), `last_seen(owner) -> Option<u64>`; `NetworkHealthService::set_presence_source(Arc<PresenceLastSeenCache>)`. Fed from `community_presence.rs`: in the subscriber's `if changed` arm (and also on unchanged beacon refreshes — call it right after `apply()` regardless of `changed`), `cache.note_seen(signed.beacon.owner-bytes, now_ms)`. Wired in `lib.rs` next to `MembershipProjection` and threaded into `spawn_community_presence_subscriber` as `Option<Arc<PresenceLastSeenCache>>` (None in tests that don't care).
- Fusion rules in `snapshot()` (single place, after the existing supervisor fold): join liveness states by `iroh_node_id`; per peer — `Connected{mode,rtt_ms,..}` → `connection_mode = Direct|Relay`, `peer.rtt_ms = rtt_ms`; `Degraded{..}` → `connection_mode = Degraded`, rtt untouched; `Disconnected`/absent → leave `NoConnection`. `last_seen_ms = max(existing-record-value, supervisor since_ms fallback (existing), liveness last_connected? — NOT exposed on wire; use Connected.since_ms when connected, presence cache value)`. Self-test overlay (~`:838-857`) moves BEFORE the liveness join so live transport data wins over a stale cached self-test. `MyNetworkSummary.relay_rtt_ms`: if the iroh snapshot returns None (it always does today), use `liveness.min_relay_rtt_ms()`. `schema_version: 4`.

- [ ] **Step 1: failing tests** (extend network_health's test module; it already has mock-source patterns — reuse them): (a) `snapshot_fuses_liveness_states_into_peer_health`: mock LivenessSnapshot with one Connected(Relay, 42) + one Degraded + one absent → assert `connectionMode` `relay`/`degraded`/`noConnection` and `rttMs` 42/None/None; (b) `liveness_overrides_stale_self_test_mode`: cached self-test says Direct, liveness says Relay → Relay wins; (c) `relay_rtt_falls_back_to_liveness_min`; (d) `last_seen_prefers_freshest_source` (record ts < presence cache ts → cache wins; both < Connected.since_ms → since_ms wins); (e) serde pin: `ConnectionMode::Degraded` → `"degraded"`; (f) update the schema_version pin to 4; (g) `PresenceLastSeenCache` max-merge unit test.
- [ ] **Step 2: implement** per the interface block. Presence threading: `spawn_community_presence_subscriber` gains the `Option<Arc<PresenceLastSeenCache>>` param; `event_loop.rs` passes the cache (constructed in lib.rs, reachable via a new field on the existing config/state struct that carries `MembershipProjection` — mirror exactly how the projection reaches its writer today; verify at implementation time and record in the report). The lib.rs bridge task: after `spawn_rate_limiter`, `tokio::spawn` a loop that polls `resolver.liveness()` every 500ms for up to 60s; once Some, `let mut rx = lh.changed_rx(); loop { if rx.changed().await.is_err() { break; } nh.notify(); }`.
- [ ] **Step 3: run** — `-E 'test(network_health) | test(community_presence)'`; fmt + clippy; commit `feat: fuse liveness into PeerHealth — live mode/rtt/last-seen, Degraded, presence cache (ZEB-622)`.

---

### Task 6: honest self-test mode + TS types + panel

**Files:**
- Modify: `src-tauri/src/network_health.rs` (`ProdPingDispatcher::ping` ~`:1535-1557`)
- Modify: `src/lib/types/network-health.ts` (`ConnectionMode` `:13`, `DialHealthSummary` `:81-87`, `DynamicDialHit` `:74-77`)
- Modify: `src/lib/components/NetworkHealthView.svelte` (`peerStatusIcon` `:172-176`, dial section `:255-296`)
- Modify: `src/lib/components/__tests__/NetworkHealthView.test.ts`

**Interfaces:**
- Consumes: iroh path API (the ping dispatcher opens its own `Connection` — read `conn.paths()` after the echo), ZEB-620's Rust-side `retrying/dormant/connected` counts (already serialized).
- Produces: `ProdPingDispatcher::ping` returns `(rtt, mode)` with a REAL mode: change the `Ok((rtt, ConnectionMode::Direct))` at `:1555` to read the ping connection's selected path exactly like `run_conn_path_watcher` does (`is_relay()` → `Relay`, `is_ip()` → `Direct`; if no path shows as selected, keep `Direct` as the documented fallback with a comment — the echo just succeeded so a path exists, the snapshot merely raced it). TS: `ConnectionMode = 'direct' | 'relay' | 'noConnection' | 'degraded'`; `DialHealthSummary` gains `retrying: number; dormant: number; connected: number;`; `DynamicDialHit.outcome` comment lists all five outcomes.
- Panel: `peerStatusIcon` gains `'degraded' → '⚠'` (and keep relay ⚠ vs direct ✓; give degraded its own title text); dial section adds one summary row `connected / retrying / dormant` from the new fields; recent-hit icon map extends: `succeeded ✓, failed ✗, reconnected ↻, retrying …, dormant zzz` (plain text, match existing style).

- [ ] **Step 1: failing tests** — vitest: extend `NetworkHealthView.test.ts` fixtures with `retrying/dormant/connected` + a `degraded` peer + a `reconnected` hit; assert the new row + icons render. Rust: a unit test for the mode-derivation helper if you extract one (extract `fn mode_from_conn(conn: &Connection) -> ConnectionMode` next to the dispatcher — hermetic iroh test optional; the transport-level behavior is already covered by Task 2's test, so a pure extraction without a dedicated endpoint test is acceptable).
- [ ] **Step 2: implement; run `npx tsc --noEmit` + `npx vitest run` (repo root) + `cargo clippy` (src-tauri).** Commit `feat: honest self-test mode + panel liveness surfaces (ZEB-622)`.

---

### Task 7: acceptance integration — same-zid flap re-arms backfill epoch end-to-end

**Files:**
- Modify: `src-tauri/src/zenoh_iroh_transport.rs` (extend Task 2's hermetic test OR add a sibling — decision: extend, keeping ONE endpoint-pair build)

**Interfaces:** consumes everything shipped in Tasks 1-3.

- [ ] **Step 1:** Extend `liveness_tracks_link_lifecycle_and_flap_bumps_epoch` with the supervisor in the loop (the full production topology): install BOTH handles (`set_reconnect_handle` + `set_liveness_handle`) on alice, spawn the real `run_reconnect_supervisor` with the `SupervisorLinkDialer` (copy the ZEB-620 acceptance test's config incl. `dial_timeout: Duration::from_secs(300)`), kick once, and verify the DROP phase recovery is now SUPERVISOR-driven (no manual re-link): after the explicit acceptor-side close, poll_until the liveness map returns to `Connected` AND the epoch watch reads 2 — proving drop → supervisor re-dial → same-zid reinstall → liveness up-edge → backfill epoch re-arm, the ticket's acceptance chain, with zero manual intervention. Assert the supervisor snapshot agrees (`PeerStateWire::Connected`).
- [ ] **Step 2: run it** (`-E 'test(liveness_tracks_link)'`, expect PASS ≤ ~180s under throttle; budget the poll windows like the ZEB-620 test: 60s establish / 60s recover). Commit `test: ZEB-622 acceptance — flap re-arms epoch through supervisor recovery (ZEB-622)`.

---

### Task 8: full sweep + docs

- [ ] **Step 1:** `cd src-tauri && cargo fmt --all -- --check`
- [ ] **Step 2:** `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
- [ ] **Step 3:** `cargo nextest run --locked --workspace --all-targets --features test-fixtures` (expect ~4000 tests, 50-60 min under the throttle group; SLOW lines on iroh-endpoint tests are normal)
- [ ] **Step 4:** repo root: `npx tsc --noEmit && npx vitest run`
- [ ] **Step 5:** commit any stragglers; the controller takes over for PR prep (body, evidence, converge loop).
