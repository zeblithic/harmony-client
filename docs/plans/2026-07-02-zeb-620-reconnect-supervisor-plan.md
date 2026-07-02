# ZEB-620: Reconnect Supervisor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the dial-once `DialedSet` machinery with a per-peer reconnect supervisor: jittered-ladder re-dials driven by drop events, presence edges, and changed-record hints; lower-NodeId single-dialer rule; boot seeds migrated off zenoh's config-endpoint forever-retry; bounded dial concurrency; lossless (coalescing) trigger delivery.

**Architecture:** New module `src-tauri/src/reconnect_supervisor.rs` owns a per-peer state machine (`Connected / Retrying / Dormant`) and ONE supervisor loop. Triggers arrive via a coalescing dirty-set (peer-id set + `tokio::sync::Notify` — lossless by construction, replacing the lossy `try_send` mpsc pattern). Sources: (a) ZEB-616 registry drop-watchers (extended to raise events, and added to the outbound path which today doesn't register at all), (b) zenoh `transport_events_listener` Delete events (needs the `unstable` feature), (c) resolver first-learn AND new changed-record hints, (d) presence-edge sweeps (identity-free counter → cooldown-gated re-arm of all non-connected peers). Re-dials go through the existing `PeerDialer`/`RuntimePeerDialer` + `deterministic_zid_hex` seams and the ZEB-616 registry. Ground truth for every touch point: `scratchpad/zeb-620-survey.md` (session artifact); design provenance: `docs/specs/2026-07-02-zeb-321-phase3-decision-record.md` Areas A + D2.

**Tech Stack:** Rust/tokio (paused-time tests for all cadences), zenoh 1.9.0 (`internal` + `unstable`), iroh 1.0.1.

## Global Constraints

- Branch `zeb-620-reconnect-supervisor` (created off main `063e34a4`). Conventional commits referencing ZEB-620.
- All cargo commands from `src-tauri/`, always `--locked`; per-task gates `-p harmony-app --lib --features test-fixtures`; the full `--all-targets` sweep runs ONCE at Task 8.
- Every cadence/ladder test uses paused tokio time (`#[tokio::test(start_paused = true)]`); wall-clock budgets far below regression thresholds — never assert real sleeps.
- **NEVER kill or restart the running `harmony-app --profile fleet-koya serve` process** (it deliberately runs a pre-upgrade build; wire flag-day pending).
- ONE cargo invocation at a time in this target dir. Long gates: background with `EXIT=$?` stamp + poll; commit BEFORE long gates; hard per-gate ceiling 30 min (full sweep 75 min).
- Policy constants (from the approved design — use these exact values):
  - `RETRY_BASE: Duration = 1s`, multiplier ×2, `RETRY_CAP: Duration = 300s` (5 min)
  - Jitter: uniform in `[0.5, 1.5) × delay` (deterministic seed injectable for tests)
  - `DORMANT_AFTER: Duration = 900s` (15 min without a fresh trigger) — Dormant is never terminal: any trigger re-arms from base
  - `PRESENCE_SWEEP_COOLDOWN: Duration = 60s` (mirrors `EPOCH_REARM_COOLDOWN_MS`)
  - `MAX_CONCURRENT_DIALS: usize = 4` (global semaphore)
  - `HIGHER_ID_FALLBACK_DELAY: Duration = 5s` — single-dialer refinement: the higher-NodeId side does not dial immediately on a trigger; it waits this long for the lower side's inbound, then runs its own ladder anyway (prevents permanent partition when the lower side is the dead one; same concept as ZEB-485's `FALLBACK_DIAL_DELAY`, scaled for zenoh)
- The existing `PeerDialer` trait, `RuntimePeerDialer`, `deterministic_zid_hex`, and `DialTelemetry` seams are REUSED, not rewritten. `DialedSet`, `MAX_DIAL_ATTEMPTS`, and `run_dial_driver`'s terminal-failure semantics are retired.
- PR body (Task 8) may ID-reference ONLY ZEB-620 (bare or otherwise). Every other ticket in prose without its number.

---

### Task 1: zenoh `unstable` feature + event-listener schema pin

**Files:**
- Modify: `src-tauri/Cargo.toml:44` (zenoh features array)
- Test: new `#[cfg(test)]` test in `src-tauri/src/event_loop.rs` (or `iroh_zenoh_registration.rs` if event_loop's test module is unwieldy)

**Interfaces:**
- Produces: `zenoh = { version = "=1.9.0", features = ["internal", "unstable"] }`; compile-verified access to `session.info().transport_events_listener()`.

- [ ] **Step 1:** Edit the features array to `["internal", "unstable"]`, extending the existing ZEB-373 pin comment with one line: `// ZEB-620: "unstable" exposes transport/link event listeners for the reconnect supervisor.`
- [ ] **Step 2:** Write a compile-pin test proving the API surface exists (mirrors the ZEB-616 `lease_and_keepalive_keys_are_valid` pattern — the point is a loud failure if a zenoh bump drops the surface):

```rust
/// ZEB-620 schema pin: the reconnect supervisor consumes zenoh's unstable
/// transport-event listener surface. If a zenoh bump removes or renames it,
/// fail here, not in the supervisor wiring.
#[test]
fn zenoh_unstable_transport_events_surface_exists() {
    // Type-level pin only — constructing a Session needs a runtime; we
    // just need the path to resolve and the item to be nameable.
    fn _assert_surface(info: &zenoh::session::SessionInfo) {
        let _ = zenoh::session::SessionInfo::transport_events_listener;
        let _ = info; // silence unused
    }
}
```

(Implementer: verify the actual method path/receiver in zenoh 1.9.0 source — `~/.cargo/registry/src/*/zenoh-1.9.0/` — and adjust the pin to whatever compiles as a pure name-resolution check. If the listener returns a builder, pin the builder type name too.)
- [ ] **Step 3:** Gate: `cargo check --locked -p harmony-app --lib --features test-fixtures` (background if slow — feature change recompiles zenoh subtree) then `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(zenoh_unstable)'`. Expected: green.
- [ ] **Step 4:** Commit: `feat: enable zenoh unstable feature + event-listener schema pin (ZEB-620)`

---

### Task 2: supervisor core — state machine, ladder, dirty-set, gate (pure logic)

**Files:**
- Create: `src-tauri/src/reconnect_supervisor.rs`
- Modify: `src-tauri/src/lib.rs` (one `mod reconnect_supervisor;` line)

**Interfaces (produces — later tasks consume these exact items):**
```rust
/// Why a peer needs attention. Idempotent kicks — duplicates are harmless.
pub enum ReconnectTrigger {
    NewPeer,        // resolver first-learn (today's DialHint)
    RecordChanged,  // resolver LWW-replaced an existing record with new relay/addrs
    Dropped,        // registry eviction or zenoh transport Delete
    PresenceSweep,  // identity-free roster edge — re-arm ALL non-connected peers
}

pub enum PeerState {
    Connected { since_ms: u64 },
    Retrying { attempt: u32, next_at: tokio::time::Instant },
    Dormant { since_ms: u64 },
}

pub struct SupervisorHandle { /* Arc<Inner> */ }
impl SupervisorHandle {
    /// Lossless, non-async, callable from sync contexts (drop-watchers,
    /// resolver): inserts into the dirty set + notifies. Never blocks,
    /// never drops.
    pub fn kick(&self, peer: [u8; 32], trigger: ReconnectTrigger);
    pub fn kick_sweep(&self);                       // PresenceSweep for all known
    pub fn mark_connected(&self, peer: [u8; 32]);   // inbound accept or dial success
    pub fn states_snapshot(&self) -> Vec<([u8; 32], PeerStateWire)>; // telemetry
}

pub struct SupervisorConfig {
    pub retry_base: Duration, pub retry_cap: Duration,
    pub dormant_after: Duration, pub presence_sweep_cooldown: Duration,
    pub max_concurrent_dials: usize, pub higher_id_fallback_delay: Duration,
    pub jitter_seed: Option<u64>,   // Some(seed) in tests for determinism
}

pub async fn run_reconnect_supervisor(
    handle: SupervisorHandle,           // shared with producers
    dialer: Arc<dyn PeerDialer>,        // existing trait, iroh_dial_driver.rs:76
    resolver: Arc<ReachabilityResolver>,// locator lookup at dial time (record freshness)
    telemetry: Arc<DialTelemetry>,
    self_node_id: [u8; 32],
    config: SupervisorConfig,
);

/// ZEB-485 gate, generalized: lower id dials immediately; higher id waits
/// `higher_id_fallback_delay` first. Pure fn — unit-tested directly.
pub fn dial_role(self_id: &[u8; 32], peer_id: &[u8; 32]) -> DialRole; // Dialer | DelayedDialer
```
- Consumes: `PeerDialer`, `DialTelemetry`, `deterministic_zid_hex` (existing, survey §1), `ReachabilityResolver::resolve_by_node_id` (survey §6).

Implementation notes (binding):
- Dirty set = `Mutex<HashMap<[u8;32], ReconnectTrigger>>` (strongest trigger wins on merge: Dropped/NewPeer/RecordChanged > PresenceSweep) + `Notify`. The supervisor loop drains the set, computes per-peer actions, and sleeps until the earliest `next_at` or the next notify — a single loop, no per-peer tasks except bounded in-flight dials (semaphore `max_concurrent_dials`).
- Ladder: `delay(attempt) = min(base × 2^attempt, cap) × jitter`. On dial success → `Connected` (+ telemetry `record_succeeded`). On failure → next rung. When `now - last_fresh_trigger > dormant_after` → `Dormant` (stop scheduling; stays in map). Fresh trigger on Dormant/Retrying → reset `attempt = 0`, schedule at base.
- `PresenceSweep`: gate on `presence_sweep_cooldown` since the last sweep (a sweep that arrives during cooldown sets a pending flag that fires when the cooldown lapses — mirrors the backfill kick's deferred re-arm).
- Self-dial guard: ignore kicks for `self_node_id`.
- No dial while `Connected` — a kick on a Connected peer is recorded (fresh-trigger timestamp) but does not dial; `Dropped` moves Connected → Retrying at base.

- [ ] **Step 1:** Write failing paused-time tests FIRST (all `#[tokio::test(start_paused = true)]`, deterministic `jitter_seed`):
  - `ladder_escalates_and_caps`: MockDialer always-fail; assert dial timestamps ≈ base, 2×, 4×… capped at `retry_cap` (assert within jitter bounds).
  - `trigger_rearms_from_base`: peer mid-ladder at 64s rung; `kick(Dropped)` → next dial ≈ base.
  - `dormancy_after_15min_and_revival`: no triggers → no dials after `dormant_after`; a later kick revives at base.
  - `presence_sweep_cooldown_gates_and_defers`: two sweeps 10s apart → one immediate re-arm, second deferred to cooldown lapse.
  - `dial_role_gate`: pure-fn cases — self<peer → Dialer, self>peer → DelayedDialer, and the DelayedDialer's first attempt lands after `higher_id_fallback_delay`.
  - `concurrent_dials_bounded`: 10 kicked peers, MockDialer that parks until released → at most `max_concurrent_dials` in flight.
  - `kick_is_lossless_and_coalescing`: 1000 kicks for one peer before the loop drains → exactly one scheduled dial; no event lost for 3 distinct peers kicked once each.
  - `connected_peer_not_dialed_until_dropped`.
- [ ] **Step 2:** Run: `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(reconnect_supervisor)'`. Expected: FAIL (module absent).
- [ ] **Step 3:** Implement the module to the interface above.
- [ ] **Step 4:** Re-run Step 2 filter. Expected: all PASS. Then `cargo fmt --all` + `cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings`.
- [ ] **Step 5:** Commit: `feat: reconnect supervisor core — per-peer ladder state machine (ZEB-620)`

---

### Task 3: drop-event wiring — registry watchers raise kicks; outbound registration parity

**Files:**
- Modify: `src-tauri/src/zenoh_iroh_transport.rs` (drop-watcher ~lines 484-500; `new_link` ~lines 702-748; manager fields ~line 220)

**Interfaces:**
- Consumes: `SupervisorHandle::{kick, mark_connected}` (Task 2).
- Produces: `IrohZenohLinkManager::set_reconnect_handle(handle: SupervisorHandle)` (OnceLock-style optional install, mirroring the existing factory-injection idiom); outbound connections registered in `zenoh_conns` with identity-guarded drop-watchers (parity with inbound).

- [ ] **Step 1:** Failing test first: extend the registry unit-test module (survey: tests at zenoh_iroh_transport.rs ~826-1000) with
  - `drop_watcher_kicks_supervisor`: install a recording fake handle; register a conn; close it; assert eviction AND a `Dropped` kick for the peer id.
  - `outbound_new_link_registers_and_watches`: hermetic two-endpoint pair (reuse the file's existing loopback helpers); outbound `new_link` → `zenoh_conns` contains the peer; drop the accept side → watcher evicts + kicks.
  - `superseded_conn_drop_does_not_kick`: same-zid swap; old conn's close must NOT kick (the `should_evict_on_close` guard extends to kick suppression).
- [ ] **Step 2:** Run filter `-E 'test(drop_watcher) | test(outbound_new_link) | test(superseded)'` → FAIL.
- [ ] **Step 3:** Implement: (a) optional `reconnect: OnceLock<SupervisorHandle>` on the manager; (b) inbound watcher: after the guarded `map.remove`, `handle.kick(peer, Dropped)` (only when the guard passed); also `mark_connected(peer)` at inbound `swap_zenoh_conn` success; (c) outbound `new_link`: after successful `connect`, `swap_zenoh_conn` + spawn the same watcher + `mark_connected`. Close-stale-first semantics identical to inbound (reuse the existing swap/close pattern verbatim).
- [ ] **Step 4:** Re-run Step 2 filter → PASS; fmt + clippy lib scope.
- [ ] **Step 5:** Commit: `feat: registry drop events kick the supervisor; outbound links join the registry (ZEB-620)`

---

### Task 4: resolver triggers — changed-record hints + richer first-learn routing

**Files:**
- Modify: `src-tauri/src/reachability_resolver.rs` (update_with_source ~149-189; DialHint ~26-30)

**Interfaces:**
- Consumes: `SupervisorHandle::kick` (Task 2).
- Produces: `ReachabilityResolver::set_supervisor(handle)` alongside (eventually replacing) `set_dial_hint_sender`; kicks `NewPeer` on first-learn and `RecordChanged` when an LWW replace materially changes `iroh_relay_url` or the direct-address set for an existing `(owner, node_id)` key.

- [ ] **Step 1:** Failing tests in the resolver's existing test module:
  - `first_learn_kicks_new_peer` (existing first-learn path routed to the fake handle).
  - `changed_relay_kicks_record_changed`: same key, newer HLC, different relay URL → `RecordChanged` kick.
  - `changed_direct_addrs_kicks_record_changed`.
  - `identical_payload_replay_does_not_kick`: same key, newer HLC, byte-identical addressing → NO kick (beacon republish must not thrash ladders).
  - `lww_rejected_stale_record_does_not_kick`: `should_replace == false` → no kick.
- [ ] **Step 2:** Run filter `-E 'test(kicks_record_changed) | test(first_learn_kicks) | test(does_not_kick)'` → FAIL.
- [ ] **Step 3:** Implement: in `update_with_source`, capture the prior payload's `(relay_url, direct_addrs)` before replace; after a successful replace with `was_present`, compare and kick `RecordChanged` on delta. Keep the legacy `dial_hint_tx` path functional during this task (event_loop still uses it until Task 5) — the supervisor handle is additive here.
- [ ] **Step 4:** Re-run filter → PASS; fmt + clippy lib scope.
- [ ] **Step 5:** Commit: `feat: resolver kicks supervisor on first-learn and changed records (ZEB-620)`

---

### Task 5: integration — event_loop swap, presence sweeps, zenoh listener, boot-seed migration

**Files:**
- Modify: `src-tauri/src/event_loop.rs` (config build ~984-1009; dial-driver spawn ~1085-1102; presence subscriber plumbing ~3057/3147)
- Modify: `src-tauri/src/iroh_zenoh_registration.rs` (boot-seed helper retargeting)
- Modify: `src-tauri/src/community_presence.rs` (subscriber fire site ~556-573 — add supervisor sweep alongside the existing resync bump)
- Modify: `src-tauri/src/iroh_dial_driver.rs` (retire `DialedSet`/`run_dial_driver`; keep `PeerDialer`/`RuntimePeerDialer`/`deterministic_zid_hex`/`DIAL_HINT_CHANNEL_CAP` consumers compiling — move what survives, delete what doesn't, update module doc)

**Interfaces:**
- Consumes: everything from Tasks 2-4.
- Produces: one running supervisor per node session; `connect/endpoints` no longer carries iroh boot seeds; boot seeds enter as `Disconnected` peers ordered by record recency under the dial semaphore.

Binding integration points (from the survey — implementer verifies lines against tree):
1. event_loop config build: drop the `iroh_connect_locators` injection into `connect/endpoints` (keep the LAN/`endpoint`-JSON path). `merge_iroh_listen_endpoints` stays untouched.
2. Replace the `run_dial_driver` spawn with: build `SupervisorHandle` + `run_reconnect_supervisor` spawn (same `RuntimePeerDialer`, telemetry, self id; production `SupervisorConfig` from the Global Constraints values); install the handle into the link manager (Task 3 seam) and resolver (Task 4 seam).
3. Boot seeds: after supervisor spawn, `resolver.list_active_peers()` → `handle.kick(peer, NewPeer)` for each, ordered by record recency (newest first — the dirty-set drain preserves no order, so seed by staggered kicks or give the supervisor a seeded-queue entry point; implementer picks the simpler, TEST-OBSERVABLE variant and documents it). DOCUMENTED DEVIATION from the Area D2 text: the approved design says "recency + shared-community count"; v1 orders by recency only — shared-community weighting needs membership plumbing at seed time and only affects dial ORDER under the cap (fleet-scale impact nil). Called out in the PR body; the weighting lands with the liveness slice if fleet validation shows it matters.
4. Presence sweep: in the presence subscriber where `resync_tx.send_modify` fires today, also `supervisor.kick_sweep()` (clone of the handle threaded the same way `presence_resync_tx` was in PR #390 — survey §5 has the exact plumbing chain).
5. zenoh transport listener: spawn a task consuming `session.info().transport_events_listener()` Delete events → map zid → node_id (helper: iterate `resolver.list_active_peers()` computing `deterministic_zid_hex`; cache the map, refresh on miss) → `kick(peer, Dropped)`. Treat listener failure as non-fatal (warn-log once): the registry watchers are the primary drop source.
6. The legacy resolver→driver mpsc (`DIAL_HINT_CHANNEL_CAP`, `set_dial_hint_sender`) is retired if nothing else consumes it — the resolver kicks the supervisor directly. Delete dead code rather than leaving both paths.

- [ ] **Step 1:** Failing integration-shaped lib tests (paused-time where cadence-relevant):
  - `boot_seeds_kick_supervisor_not_config`: build the config the way event_loop does with a resolver holding 2 peers → assert `connect/endpoints` has NO `iroh/` locators; assert both peers got `NewPeer` kicks.
  - `presence_edge_triggers_sweep`: drive the presence subscriber path (existing presence test fixtures) → fake handle records `kick_sweep`.
- [ ] **Step 2:** Run new-test filter → FAIL.
- [ ] **Step 3:** Implement points 1-6.
- [ ] **Step 4:** New-test filter PASS + targeted regression filter: `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(reconnect_supervisor) | test(dial) | test(zenoh_iroh) | test(presence) | test(reachability)'` (background + poll). fmt + clippy lib scope.
- [ ] **Step 5:** Commit: `feat: supervisor owns dials — event-loop swap, presence sweeps, boot-seed migration (ZEB-620)`

---

### Task 6: telemetry + PeerHealth feeds

**Files:**
- Modify: `src-tauri/src/network_health.rs` (DialTelemetry ~201-256; DynamicDialHit ~180-185; DialHealthSummary ~187-195; PeerHealth assembly ~472-518)

**Interfaces:**
- Consumes: `SupervisorHandle::states_snapshot` (Task 2).
- Produces: `DynamicDialHit.outcome` gains `"retrying" | "dormant" | "reconnected"`; `DialHealthSummary` gains `{ retrying: u32, dormant: u32, connected: u32 }` (from the snapshot); `PeerHealth.last_seen_ms` fed from `Connected.since_ms` when the resolver record lacks one. `rtt_ms` stays untouched (S6 scope).

- [ ] **Step 1:** Failing tests: summary counts derived from a fake snapshot; wire-shape test asserting the new camelCase fields serialize (`retrying`, `dormant`, `connected`) — remember DTO keys are camelCase on the wire.
- [ ] **Step 2:** FAIL → implement → PASS; fmt + clippy lib scope.
- [ ] **Step 3:** Commit: `feat: supervisor state feeds dial telemetry + PeerHealth (ZEB-620)`

---

### Task 7: hermetic end-to-end reconnect acceptance test

**Files:**
- Modify: `src-tauri/src/zenoh_iroh_transport.rs` tests (extend the ZEB-616 `zenoh_reconnect_closes_stale_connection` scenario pattern)

**Interfaces:** consumes the full stack (Tasks 2-5). No new production code — this is the ticket's acceptance scenario.

- [ ] **Step 1:** Write `supervisor_redials_after_drop_and_get_answers` (real-time test, budget generous per the ZEB-616 pattern; it inherits the `iroh-endpoint` nextest group throttle): two hermetic loopback endpoints with a supervisor on the dialer side; establish zenoh-over-iroh; hard-drop the connection (close the acceptor's conn); assert WITHOUT any manual re-dial: supervisor re-dials through the registry → same-zid reinstall (stale face closed first) → a zenoh GET round-trips; telemetry shows ≥1 supervisor attempt + a `reconnected` hit.
- [ ] **Step 2:** Run it: `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(supervisor_redials_after_drop)'`. Expected: PASS (iterate until deterministic — use the ZEB-616 test's polling/timeout idioms; per-IO timeouts fat, outer budget fatter, assertions never weakened).
- [ ] **Step 3:** Commit: `test: hermetic drop -> supervisor re-dial -> same-zid reinstall -> GET acceptance (ZEB-620)`

---

### Task 8: full sweep + PR

**Files:** none beyond fallout fixes.

- [ ] **Step 1:** Full gates, sequentially, backgrounded with stamps: `cargo check --locked --workspace --all-targets --features test-fixtures` → `cargo fmt --all -- --check` → `cargo clippy --locked --workspace --all-targets --features test-fixtures --no-deps -- -D warnings` → `cargo nextest run --locked --workspace --all-targets --features test-fixtures`. Expected: all green (suite baseline 3962+ with the new tests; the `iroh-endpoint` group throttle from the previous slice applies). Unrelated pre-existing failures → follow-up ticket rule, never fixed here.
- [ ] **Step 2:** Push branch; open PR titled `ZEB-620: reconnect supervisor — per-peer ladder replaces dial-once, boot seeds migrate off config endpoints`. Body: summary per area (triggers/ladder/single-dialer/boot-seeds/hygiene/telemetry), test evidence, the acceptance scenario named, a note that presence sweeps are identity-free by design (v1) and record-freshness hardening arrives in the next slice — ALL other tickets referenced in prose without ID numbers. ZEB-620 is auto-linked by the branch name.
- [ ] **Step 3:** Trigger CodeRabbit once; converge loop per standing protocol.
