# ZEB-910: Island-Aware Community Repair — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make community split repair a steady-state behavior: coverage-based split detection with an upgraded bridging repair (driver), periodic Dormant-slot parole (supervisor), and 8 rendezvous slots.

**Architecture:** Three coordinated client-side parts per the spec (`docs/superpowers/specs/2026-08-12-zeb910-island-aware-repair-design.md`): the gateway-dial driver's predicate becomes a proven-traffic coverage measure with a `Degraded` verdict driving an all-slots/seed-all repair plus member-record refresh and targeted Dormant revival; the reconnect supervisor gains a loop-internal parole tick; `RENDEZVOUS_SLOT_COUNT` decouples from the relay-read cap and rises to 8. `harmony_pkarr` and core stay untouched.

**Tech Stack:** Rust (tokio, paused-clock tests, cargo-nextest), existing test harnesses (`StubCtx`/`StubBeacons` in `community_gateway_dial_driver.rs`, `RecordingDialer`/`cfg()` in `reconnect_supervisor.rs`, `MockPkarrRelay` in `tests/pkarr_net/`).

## Global Constraints

- All cargo commands from `src-tauri/`, always `--locked --features test-fixtures`; clippy adds `--all-targets --no-deps -- -D warnings`; fmt gate `cargo fmt --all -- --check`.
- Iterative gates may use `scripts/test-select --context task`; the FINAL pre-PR sweep is the full `cargo nextest run --locked --workspace --all-targets --features test-fixtures`.
- Commit after every green task; trailers: `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>` + `Claude-Session: https://claude.ai/code/session_01DUvg7gyEHqXrVU85Eg2D8D`.
- e2e/DTO assertions use serde camelCase keys.
- No changes under `src-tauri/vendor/`, no `harmony-*` dependency rev bumps.

---

### Task 1: `ReachabilityResolver::refresh_if_older_than`

**Files:**
- Modify: `src-tauri/src/reachability_resolver.rs` (method at ~line 630; tests module at end)

**Interfaces:**
- Produces: `pub(crate) fn refresh_if_older_than(&self, owner: OwnerAddr, node_id: [u8; 32], now_ms: u64, older_than_ms: u64)` — identical semantics to `maybe_refresh_stale` with the step-(1) staleness bar parameterized. `maybe_refresh_stale` delegates with `STALE_RECORD_REFRESH_MS`. Consumed by Task 6 (driver repair) and unchanged supervisor dispatch.

- [ ] **Step 1: Write the failing tests** (in the existing resolver test module, mirroring its `maybe_refresh_stale` tests — find them with `grep -n "maybe_refresh_stale" src/reachability_resolver.rs`; reuse the module's existing counting-fallback fixture, or add one matching the `ReachabilityFallback<OwnerAddr>` impl pattern from `reconnect_supervisor.rs::tests::CountingFallback`):

```rust
#[tokio::test]
async fn refresh_if_older_than_honors_custom_threshold() {
    // Entry aged 20 min: fresh under the 24h bar, stale under a 10-min bar.
    let (resolver, calls) = resolver_with_counting_fallback();
    let now = 100 * 60 * 60 * 1000u64;
    seed_entry_at(&resolver, OWNER, NODE, now - 20 * 60 * 1000);
    resolver.maybe_refresh_stale(OWNER, NODE, now);
    tokio::task::yield_now().await;
    assert_eq!(calls.load(Ordering::SeqCst), 0, "24h bar: 20-min entry is fresh");
    resolver.refresh_if_older_than(OWNER, NODE, now, 10 * 60 * 1000);
    wait_for_calls(&calls, 1).await;
}

#[tokio::test]
async fn refresh_if_older_than_keeps_owner_cooldown() {
    let (resolver, calls) = resolver_with_counting_fallback();
    let now = 100 * 60 * 60 * 1000u64;
    seed_entry_at(&resolver, OWNER, NODE, now - 20 * 60 * 1000);
    resolver.refresh_if_older_than(OWNER, NODE, now, 10 * 60 * 1000);
    wait_for_calls(&calls, 1).await;
    resolver.refresh_if_older_than(OWNER, NODE, now, 10 * 60 * 1000);
    tokio::task::yield_now().await;
    assert_eq!(calls.load(Ordering::SeqCst), 1, "second call inside PKARR_REFRESH_COOLDOWN must not fire");
}

#[tokio::test]
async fn refresh_if_older_than_still_skips_fleet_sibling() {
    // Seed via the FleetSibling source; even an ancient entry must not refresh.
    let (resolver, calls) = resolver_with_counting_fallback();
    let now = 100 * 60 * 60 * 1000u64;
    seed_fleet_entry_at(&resolver, OWNER, NODE, now - 48 * 60 * 60 * 1000);
    resolver.refresh_if_older_than(OWNER, NODE, now, 10 * 60 * 1000);
    tokio::task::yield_now().await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}
```

Helper notes: `seed_entry_at` = `update_with_source(owner, payload_with(node, announced_at), hlc(announced_at), ReachabilitySource::PkarrLive)`; `seed_fleet_entry_at` same with `FleetSibling`; `wait_for_calls` = loop `yield_now` (bounded ~100 iterations) until the counter reaches the target (the refresh runs in a spawned task behind a semaphore). Adapt names to fixtures the module already has.

- [ ] **Step 2: Run to verify failure** — `cargo nextest run --locked --features test-fixtures -E 'test(refresh_if_older_than)'` → FAIL (method missing).

- [ ] **Step 3: Implement.** Rename the body of `maybe_refresh_stale` to the new method with the threshold parameter; step (1)'s gate becomes:

```rust
match now_ms.checked_sub(entry.effective_announced_at_ms) {
    Some(age) if age <= older_than_ms => return, // fresh under this caller's bar
    _ => {}
}
```

and re-add `maybe_refresh_stale` as a delegating wrapper (keep its full doc comment, add one line noting the parameterized variant):

```rust
pub fn maybe_refresh_stale(&self, owner: OwnerAddr, node_id: [u8; 32], now_ms: u64) {
    self.refresh_if_older_than(owner, node_id, now_ms, STALE_RECORD_REFRESH_MS)
}
```

Doc the new method: repair callers (ZEB-910) lower only the staleness bar; the fleet-sibling skip, no-fallback bail, per-owner `PKARR_REFRESH_COOLDOWN`, and `PKARR_REFRESH_MAX_CONCURRENT` semaphore are deliberately kept — "force" refresh is still rate-bounded.

- [ ] **Step 4: Run** the new tests + `-E 'test(maybe_refresh_stale) + test(refresh)'` in this file → PASS; existing stale-refresh tests unchanged-green.

- [ ] **Step 5: Commit** `feat(zeb910): parameterized staleness bar for pkarr refresh (refresh_if_older_than)`.

---

### Task 2: `resolve_rendezvous_all_slots` (client-side all-slots scan)

**Files:**
- Modify: `src-tauri/src/community_rendezvous.rs`

**Interfaces:**
- Produces:
```rust
pub struct AllSlotsResolve {
    pub hits: Vec<IdentifiedBeacon>,   // deduped by iroh_node_id, current-epoch hits preferred
    pub resolve_errors: usize,
    pub membership_rejects: usize,
}
pub async fn resolve_rendezvous_all_slots(
    pkarr: &Arc<PkarrResolver>, epoch_key: &EpochKey, self_endpoint_id: [u8; 32],
    community_id: SpaceId, enrolled_keys: Arc<HashSet<[u8; 32]>>, now_ms: u64,
    cfg: &RendezvousResolveConfig,
) -> AllSlotsResolve
```
- Consumed by Task 4's `ProdBeaconResolver` and the Task 9 e2e. Internal generic scan `pub(crate) async fn all_slots_scan<R: harmony_pkarr::rendezvous::SlotResolver<IdentifiedBeacon>>(resolver: &R, now_ms: u64, per_probe_deadline: Duration) -> (Vec<IdentifiedBeacon>, usize /*timeouts*/)` so unit tests inject a map-backed resolver.

- [ ] **Step 1: Write the failing unit tests** (in the file's test module; implement `SlotResolver<IdentifiedBeacon>` over a `HashMap<(u16, u64), IdentifiedBeacon>`):

```rust
struct MapSlotResolver { hits: HashMap<(u16, u64), IdentifiedBeacon> }
#[async_trait::async_trait]
impl harmony_pkarr::rendezvous::SlotResolver<IdentifiedBeacon> for MapSlotResolver {
    async fn resolve_slot(&self, slot: u16, epoch: u64) -> Option<IdentifiedBeacon> {
        self.hits.get(&(slot, epoch)).cloned()
    }
}
fn beacon(node: u8) -> IdentifiedBeacon { /* minimal payload with iroh_node_id [node;32], zeroed sigs */ }

#[tokio::test]
async fn all_slots_scan_collects_every_distinct_hit() {
    // slots 0 and 3 in the current epoch, slot 1 only in the previous epoch → 3 hits.
}
#[tokio::test]
async fn all_slots_scan_dedups_same_node_across_epochs_preferring_current() {
    // same node in (0, current) and (0, previous) → 1 hit.
}
#[tokio::test]
async fn all_slots_scan_empty_when_no_slot_has_a_beacon() { /* hits empty */ }
```

Compute epochs with `harmony_pkarr::current_epoch_id(now_ms)` / `.saturating_sub(1)` exactly as the scan will.

- [ ] **Step 2: Verify failure** — `cargo nextest run --locked --features test-fixtures -E 'test(all_slots_scan)'` → FAIL.

- [ ] **Step 3: Implement.** Scan shape (concurrent, per-probe deadline; a timed-out probe counts as a resolve error — infrastructure trouble, not proof of absence):

```rust
let current = harmony_pkarr::current_epoch_id(now_ms);
let epochs = [current, current.saturating_sub(1)];
let probes = epochs.iter().flat_map(|e| (0..RENDEZVOUS_SLOT_COUNT as u16).map(move |s| (s, *e)));
let results = futures::future::join_all(probes.map(|(s, e)| async move {
    match tokio::time::timeout(per_probe_deadline, resolver.resolve_slot(s, e)).await {
        Ok(hit) => (e, hit, false),
        Err(_) => (e, None, true),
    }
})).await;
// Dedup by iroh_node_id; iterate current-epoch results first so a node seen in
// both epochs keeps its current-epoch beacon. timeouts counted separately and
// added to the IdentifiedSlotResolver's own resolve_errors counter by the wrapper.
```

The `pub` wrapper builds `IdentifiedSlotResolver` exactly as `resolve_rendezvous_identified` does (same field set), calls the scan with `cfg.per_batch_deadline`, and returns `AllSlotsResolve { hits, resolve_errors: counter + timeouts, membership_rejects: counter }`. Doc: this deliberately skips the escalating economy — repair passes are ladder-paced and need every reachable island's beacon, not the first one (spec §3.3.1).

- [ ] **Step 4: Run → PASS.**
- [ ] **Step 5: Commit** `feat(zeb910): all-slots rendezvous resolve collecting every verified hit`.

---

### Task 3: `RENDEZVOUS_SLOT_COUNT` 4 → 8 (decoupled)

**Files:**
- Modify: `src-tauri/src/community_rendezvous.rs:31` (+ the `assert_eq!(RENDEZVOUS_SLOT_COUNT, COMMUNITY_RELAY_ADVERTISERS_MAX)` pin test at ~:423)

**Interfaces:** `pub const RENDEZVOUS_SLOT_COUNT: usize = 8;` — no longer aliasing `COMMUNITY_RELAY_ADVERTISERS_MAX` (which stays 4).

- [ ] **Step 1: Flip the pin test first** — replace the equality assert with:

```rust
#[test]
fn slot_count_exceeds_relay_read_cap_by_design() {
    // ZEB-910: slots bound DISCOVERY (bridgeable beacons), the relay cap bounds
    // SERVICE reads (relays_for_community fan-out). Decoupled deliberately —
    // advertisers ranked 4..8 publish beacons without joining the pull set.
    assert_eq!(RENDEZVOUS_SLOT_COUNT, 8);
    assert!(RENDEZVOUS_SLOT_COUNT >= COMMUNITY_RELAY_ADVERTISERS_MAX);
}
```

Run it → FAIL (still 4).

- [ ] **Step 2: Implement** — change the constant to `8`, rewrite its doc comment with the §5 rationale (different consumers; old resolvers probing 0-3 still interoperate; slot index is only a key-derivation input). Keep the `use` of `COMMUNITY_RELAY_ADVERTISERS_MAX` for the `>=` pin.
- [ ] **Step 3: Sweep for hidden couplings** — `grep -rn "RENDEZVOUS_SLOT_COUNT\|ADVERTISERS_MAX" src/ tests/`; verify each site's semantics under the split (batch curve `[1,2,8]` clamps, publisher rank cap, relay truncation stays 4). Run `cargo nextest run --locked --features test-fixtures -E 'test(rendezvous) + test(relay_announce) + test(open_join)'` → PASS.
- [ ] **Step 4: Commit** `feat(zeb910): 8 rendezvous slots, decoupled from the relay-read cap`.

---

### Task 4: `BeaconResolver` goes multi-hit; driver seeds every verified beacon

**Files:**
- Modify: `src-tauri/src/community_gateway_dial_driver.rs` (trait ~:111, prod impl ~:131, `classify_resolution` ~:96, resolve/seed section ~:485-575, `StubBeacons` + tests)
- Modify: any other `impl BeaconResolver` (`grep -rn "impl BeaconResolver" src/ tests/` — expect stubs in `tests/misc/community_open_join_cross_wan_integration.rs`)

**Interfaces:**
- Produces (replaces the single-hit method — no dual API):
```rust
pub struct BeaconsOutcome {
    pub hits: Vec<IdentifiedBeacon>,
    pub resolve_errors: usize,
    pub membership_rejects: usize,
}
#[async_trait::async_trait]
pub trait BeaconResolver: Send + Sync {
    async fn resolve_beacons(&self, epoch_key: &EpochKey, community_id: SpaceId,
        enrolled_keys: Arc<HashSet<[u8; 32]>>, now_ms: u64) -> BeaconsOutcome;
}
```
- `classify_resolution` becomes `fn classify_empty(resolve_errors: usize, membership_rejects: usize) -> GatewayBootstrapOutcome` (RejectedNonMember > ResolveError > NoBeacon precedence unchanged), used only when `hits` is empty.
- Consumes: Task 2's `resolve_rendezvous_all_slots` (prod impl).

- [ ] **Step 1: Write/adapt failing tests.** Existing driver tests constructing `BeaconResolution::Found(hit)` / stub overrides become `BeaconsOutcome { hits: vec![hit], .. }` etc. (`Default` impl with empty hits/zero counts keeps stubs terse). Add two new tests:

```rust
#[tokio::test]
async fn due_pass_seeds_every_verified_hit_and_kicks_each() {
    // StubBeacons returns 2 hits with distinct node ids → both seeded (2 resolver
    // rows), both kicked (supervisor pending or dial-count 2), outcome BeaconSeeded once.
}
#[tokio::test]
async fn cr2_revalidation_filters_per_hit_not_per_pass() {
    // 2 hits; ctx's fresh enrolled set drops hit B's device key between resolve
    // and seed → only A seeded; outcome BeaconSeeded (≥1 passed).
}
```

The ZEB-918 candidate-ladder tests keep their shape: first candidate with ≥1 hit short-circuits; when every candidate is empty the CURRENT-key attempt's `classify_empty` outcome is recorded (`no_beacon_under_any_candidate_records_current_key_outcome` adapts mechanically).

- [ ] **Step 2: Verify failure** (compile errors are the failure here) — `cargo nextest run --locked --features test-fixtures -E 'binary(harmony-app) and test(gateway)'`.

- [ ] **Step 3: Implement.** Prod impl calls `resolve_rendezvous_all_slots(...)` and maps fields 1:1. Driver resolve section becomes:

```rust
let mut outcome_when_empty = GatewayBootstrapOutcome::NoBeacon;
let mut hits: Vec<IdentifiedBeacon> = Vec::new();
for (i, candidate) in epoch_keys.iter().enumerate() {
    let out = self.beacons
        .resolve_beacons(candidate, community, Arc::clone(&enrolled_keys), now_ms).await;
    if i == 0 {
        outcome_when_empty = classify_empty(out.resolve_errors, out.membership_rejects);
    }
    if !out.hits.is_empty() { hits = out.hits; break; }
}
if hits.is_empty() {
    self.record(&community, outcome_when_empty);
    continue;
}
// CR-2 per hit against ONE fresh snapshot:
let fresh_enrolled = self.ctx.enrolled_device_keys_of(&community).await;
let mut seeded = 0usize;
for hit in hits {
    if !fresh_enrolled.contains(&hit.membership_device_vk) { continue; }
    let Ok(identity) = harmony_identity::Identity::from_public_bytes(&hit.beacon_identity_pub) else { continue; };
    let node_id = hit.payload.iroh_node_id;
    self.reachability.seed_from_pkarr(OwnerAddr(identity.address_hash), DeviceIdentityHash([0u8; 16]), hit.payload).await;
    if let Some(sup) = self.reachability.supervisor() { sup.kick(node_id, ReconnectTrigger::NewPeer); }
    seeded += 1;
}
self.record(&community, if seeded > 0 { GatewayBootstrapOutcome::BeaconSeeded }
                        else { GatewayBootstrapOutcome::RejectedNonMember });
```

(Keep the existing tracing lines per seeded hit; the ZEB-918 comment block about candidate ordering moves atop the loop unchanged. Task 5 later swaps the `BeaconSeeded` literal for the verdict-aware arm.)

- [ ] **Step 4: Run driver + open-join test groups → PASS.**
- [ ] **Step 5: Commit** `feat(zeb910): beacon repair seeds every verified hit (multi-hit BeaconResolver)`.

---

### Task 5: Coverage verdicts (Healthy / Degraded / Starved) + telemetry arms

**Files:**
- Modify: `src-tauri/src/community_gateway_dial_driver.rs` (constants ~:41, ctor ~:280, `run_one_pass` predicate section ~:352-447, tests)
- Modify: `src-tauri/src/network_health.rs` (enum ~:1651, `wire()` ~:1685, `record_outcome` ~:1711, telemetry struct ~:1631, `summary` ~:1738, `GatewayBootstrapHealth` ~:1767; make `STALENESS_QUIET_MS` `pub(crate)` if private)
- Modify: `src-tauri/src/reachability_resolver.rs` only if `ResolverEntry.effective_announced_at_ms` needs a `pub(crate)` accessor

**Interfaces:**
- Produces:
```rust
pub type TrafficEvidenceFn = Arc<dyn Fn(&[u8; 32]) -> Option<u64> + Send + Sync>;
pub const GATEWAY_PROVEN_TRAFFIC_MS: u64 = crate::network_health::STALENESS_QUIET_MS; // 5 min
pub const GATEWAY_COVERAGE_RECORD_FRESH_MS: u64 = crate::reachability_resolver::STALE_RECORD_REFRESH_MS; // 24 h
// CommunityGatewayDialDriver::new gains `traffic_evidence: TrafficEvidenceFn` (5th arg, before joined_communities? — append LAST to minimize churn).
enum CoverageVerdict { Healthy, Degraded { unproven: Vec<OwnerAddr> }, Starved }
fn coverage_verdict(members: &[OwnerAddr], rows_by_owner: &HashMap<OwnerAddr, Vec<PeerRow>>, traffic: &TrafficEvidenceFn, now_ms: u64) -> CoverageVerdict
struct PeerRow { node_id: [u8; 32], effective_announced_at_ms: u64 }
```
- New outcome arms: `DegradedWaiting` (`"degradedWaiting"`, row-only) and `DegradedSeeded` (`"degradedSeeded"`, counter `degraded_seeded`, DTO key `degradedSeeded`).
- Consumed by: Task 6 (repair uses `unproven`), Task 8 (prod closure).

- [ ] **Step 1: Write the failing verdict unit tests** (pure `coverage_verdict` — no async needed) + pass-level tests on the stub harness with an injected `Mutex<HashMap<[u8;32], u64>>` traffic map:

```rust
// Pure classification:
// - all members proven → Healthy
// - one member fresh-record + traffic-stale → Degraded{unproven=[m]}
// - member with STALE record (announced > 24h ago) and no traffic → NOT in unproven; others proven → Healthy
// - member with NO rows → not in unproven (record-less ≠ split evidence)
// - zero proven members (fresh records, no traffic) → Starved
// - proven purely via traffic while record stale → counts toward P (prevents Starved)
// Pass-level (run_one_pass):
// - degraded community, ladder not due → outcome "degradedWaiting", no resolve call
// - degraded + due + stub returns a hit → outcome "degradedSeeded", degraded_seeded counter 1
// - degraded + due + no hits → classify_empty arm (e.g. "noBeacon") — resolve-shaped arms stay uniform
// - starved + due + hit → "beaconSeeded" (starved seeding arm unchanged)
// - connected-but-dark: supervisor says Connected for m's device, traffic map stale → Degraded (ZEB-805 pin)
```

For pass-level tests the existing harness seeds the resolver (rows carry `announced_at_ms` → control `effective_announced_at_ms`) and installs a supervisor handle; the traffic closure reads the map. Verdict boundary: stamp age `<= GATEWAY_PROVEN_TRAFFIC_MS` is proven.

- [ ] **Step 2: Verify failure.**
- [ ] **Step 3: Implement.**
  - `run_one_pass`: replace the `connected`/`connected_owners` block with `rows_by_owner` built once per pass from `list_dialable_peers()` (owner → Vec<PeerRow>); keep the supervisor snapshot (Task 6 needs Dormant states; `connected_owners` itself is no longer consulted).
  - Verdict math (sets per spec §3.1): `E` = owners with any row fresh under `GATEWAY_COVERAGE_RECORD_FRESH_MS`; `P` = owners with any node where `traffic(node)` is `Some(t)` with `now_ms.saturating_sub(t) <= GATEWAY_PROVEN_TRAFFIC_MS`; Solo/Starved/Degraded/Healthy per the table. Healthy clears the ladder (existing block); Starved and Degraded share the existing ladder arm/due logic; not-due records `StarvedWaiting` or `DegradedWaiting` by verdict; a due seeded pass records `BeaconSeeded` (starved) or `DegradedSeeded` (degraded).
  - `network_health.rs`: add the two enum arms + wire strings; `record_outcome` counts `DegradedSeeded` into a new `degraded_seeded: AtomicU64` and lists `DegradedWaiting` in the row-only arm; `summary()` + `GatewayBootstrapHealth` gain `pub degraded_seeded: u64`.
- [ ] **Step 4: Run driver + network_health groups → PASS.** Also `cargo nextest run --locked --features test-fixtures -E 'test(network_health)'` for DTO serde pins.
- [ ] **Step 5: Commit** `feat(zeb910): coverage-based community health (proven-traffic numerator, Degraded verdict)`.

---

### Task 6: Degraded/Starved repair actions 2-4 (refresh, discovery, Dormant revival)

**Files:**
- Modify: `src-tauri/src/community_gateway_dial_driver.rs` (due-pass section, after Task 4's seed loop; tests)

**Interfaces:**
- Consumes: Task 1's `refresh_if_older_than`, Task 5's `CoverageVerdict::Degraded { unproven }` + `rows_by_owner` + the supervisor snapshot; `ReachabilityResolver::resolve_async_with_source` (existing cache-miss path).
- Produces: on every DUE non-Healthy pass, before the beacon resolve:

```rust
// (2) member-record refresh for the unproven set (Starved: every member with rows).
//     Sync + fire-and-forget internally; cooldown/semaphore bounded (Task 1).
for owner in &refresh_targets {
    for row in rows_by_owner.get(owner).into_iter().flatten() {
        self.reachability.refresh_if_older_than(*owner, row.node_id, now_ms, GATEWAY_DIAL_RETRY_CAP_MS);
    }
}
// (3) record-less member discovery: cache-miss pkarr resolve, spawned so the
//     pass never blocks on network. resolve_async short-circuits on any cached
//     row, so this fires real network I/O only for truly record-less members,
//     and only while the community is non-Healthy (ladder-paced).
for owner in members.iter().filter(|m| !rows_by_owner.contains_key(*m)) {
    let r = Arc::clone(&self.reachability); let o = *owner;
    tokio::spawn(async move { let _ = r.resolve_async(&o).await; });
}
// (4) revive ONLY Dormant slots of unproven members (Retrying peers keep their
//     ladders — resetting live backoff every pass would multiply dial pressure).
if let Some(sup) = self.reachability.supervisor() {
    let dormant: HashSet<[u8; 32]> = sup.states_snapshot().into_iter()
        .filter(|(_, st)| matches!(st, PeerStateWire::Dormant { .. }))
        .map(|(id, _)| id).collect();
    for owner in &refresh_targets {
        for row in rows_by_owner.get(owner).into_iter().flatten() {
            if dormant.contains(&row.node_id) {
                sup.kick(row.node_id, ReconnectTrigger::RecordChanged);
            }
        }
    }
}
```

where `refresh_targets` = `unproven` for Degraded, and every member with rows for Starved.

- [ ] **Step 1: Write the failing tests:**

```rust
// - degraded due pass calls refresh for the unproven member's rows only
//   (CountingFallback as the resolver fallback; proven member's owner never resolved)
// - record-less member triggers resolve_async (fallback called with that owner)
//   while a member WITH rows does not go through the cache-miss path
// - dormant device of an unproven member gets kicked (pending_trigger == RecordChanged
//   or a dial fires after the pass); a Retrying device of the same owner is NOT re-armed
//   (its attempt counter / next_at unchanged via states_snapshot before/after)
// - actions run on starved passes too (refresh_targets = all members with rows)
```

Fire-and-forget assertions: bounded `yield_now`/`sleep` polling under the paused clock (same pattern as Task 1's `wait_for_calls`).

- [ ] **Step 2: Verify failure.**
- [ ] **Step 3: Implement** per the block above (place actions before the beacon resolve so refreshes overlap the resolve's network time).
- [ ] **Step 4: Run driver group → PASS.**
- [ ] **Step 5: Commit** `feat(zeb910): repair refreshes unproven member records and revives their Dormant slots`.

---

### Task 7: Supervisor Dormant parole

**Files:**
- Modify: `src-tauri/src/reconnect_supervisor.rs` (config ~:154-190, loop ~:508-695, tests)
- Modify: `src-tauri/src/network_health.rs` (`DialTelemetry` ~:890, `DialHealthSummary` + its serde struct)

**Interfaces:**
- `SupervisorConfig` gains `pub parole_interval: Duration` (Default: `Duration::from_secs(900)`) and `pub parole_batch: usize` (Default: `2`). The test `cfg()` helper sets `parole_interval: Duration::from_secs(3600), parole_batch: 2` so existing tests never see a parole; parole tests override via struct-update syntax.
- `DialTelemetry::record_paroled(&self, node_id: [u8; 32], owner: [u8; 16])` — bumps a new `paroled: AtomicU64` + pushes a `"paroled"` ring marker; `DialHealthSummary` gains `pub paroled: u64` (wire `paroled`).

- [ ] **Step 1: Write the failing tests** (existing paused-clock harness):

```rust
// - parole_revives_record_backed_dormant_peer: failing dialer, dormant_ms small →
//   peer Dormant, dial count frozen; advance past parole_interval → dial count
//   grows again; telemetry summary().paroled >= 1.
// - parole_batch_bound_and_oldest_first: 3 dormant peers with staggered since_ms,
//   batch=2 → after one tick exactly the 2 oldest re-armed (3rd still Dormant in
//   states_snapshot); after a second tick the 3rd revives.
// - parole_skips_record_less_dormant: dormant peer with NO resolver record stays
//   Dormant across ticks (dialing is record-gated; churn without a record is pointless).
// - parolee_re_dormants_after_window: revived peer keeps failing → Dormant again
//   once dormant_after elapses (parole is self-bounding).
// - parole_fires_stale_refresh: CountingFallback + ancient record (announced_at 1)
//   → fallback call count grows on parole (maybe_refresh_stale semantics).
// - parole_leaves_retrying_and_connected_alone: a Retrying peer's attempt/next_at
//   and a Connected peer's state are unchanged across a parole tick.
```

- [ ] **Step 2: Verify failure** (config fields missing → compile fail).
- [ ] **Step 3: Implement.**
  - Loop: `let mut next_parole = Instant::now() + config.parole_interval;` before the loop; at the top (beside sweep gating): `if now >= next_parole { run_parole(&inner, &resolver, &telemetry, now, &self_node_id, &config, &mut rng); next_parole = now + config.parole_interval; }`; fold into the sleep via `let deadline = min_opt(deadline, Some(next_parole));` — including the capacity-exhausted `None` branch (a parole wake during exhaustion is harmless: dispatch skips in-flight slots, and the held-permit wake guarantee is unaffected).
  - `run_parole`:

```rust
#[allow(clippy::too_many_arguments)]
fn run_parole(inner: &SupervisorInner, resolver: &ReachabilityResolver, telemetry: &DialTelemetry,
              now: Instant, self_node_id: &[u8; 32], config: &SupervisorConfig, rng: &mut ChaCha8Rng) {
    let mut states = inner.states.lock().expect("states lock");
    // Record-backed Dormant peers, oldest first. resolve_by_node_id is an O(N)
    // scan, but the candidate set is Dormant-only and the batch is tiny.
    let mut dormant: Vec<([u8; 32], u64, [u8; 16])> = states.iter()
        .filter_map(|(peer, slot)| match slot.state {
            PeerState::Dormant { since_ms } =>
                resolver.resolve_by_node_id(peer).map(|(owner, _)| (*peer, since_ms, owner.0)),
            _ => None,
        }).collect();
    dormant.sort_by_key(|(_, since, _)| *since);
    for (peer, _, owner) in dormant.into_iter().take(config.parole_batch) {
        // Paired stale-refresh (standard 24h/15min gates — background hygiene, not urgent repair).
        resolver.maybe_refresh_stale(OwnerAddr(owner), peer, now_ms());
        let slot = states.get_mut(&peer).expect("slot exists: collected above");
        let role = dial_role(self_node_id, &peer);
        slot.epoch = slot.epoch.wrapping_add(1);
        slot.last_fresh_trigger = now; // grants one dormant_after window of retries
        let delay = schedule_delay(0, role, config, rng);
        slot.state = PeerState::Retrying { attempt: 0, next_at: now + delay };
        telemetry.record_paroled(peer, owner);
        tracing::debug!(peer = %hex::encode(&peer[..8]), "ZEB-910: dormant peer paroled");
    }
}
```

  (`resolver.maybe_refresh_stale` under the states lock is fine — it is sync and non-blocking by contract; if clippy objects to lock-across-call, collect the refresh list and fire after the loop but before releasing re-arm decisions.)
  - Telemetry additions per Interfaces.
- [ ] **Step 4: Run supervisor + network_health groups → PASS** (`clippy --all-targets` for the test-module changes).
- [ ] **Step 5: Commit** `feat(zeb910): periodic dormant-slot parole in the reconnect supervisor`.

---

### Task 8: Production wiring (lib.rs)

**Files:**
- Modify: `src-tauri/src/lib.rs` (driver construction ~:12308-12341)

**Interfaces:**
- Consumes: Task 5's `TrafficEvidenceFn` ctor arg. The prod closure fuses the SAME two sources `network_health.rs:2919-2930` fuses for the snapshot (`last_traffic_ms` = max of the peer-liveness rx stamp and `PeerTrafficRegistry::last_any_served_ms`). Locate the two `Arc`s at the construction site by following what `snapshot()` reads (`set_*_source` installs) — capture those same Arcs:

```rust
let traffic_evidence: crate::community_gateway_dial_driver::TrafficEvidenceFn = {
    let liveness = /* Arc of the peer-liveness registry used by network_health snapshot */;
    let served = /* Arc<PeerTrafficRegistry> used by network_health snapshot */;
    std::sync::Arc::new(move |node: &[u8; 32]| {
        let a = liveness.last_traffic_ms(node);      // exact accessor per network_health.rs:2919
        let b = served.last_any_served_ms(node);     // exact accessor per network_health.rs:2924
        match (a, b) { (Some(x), Some(y)) => Some(x.max(y)), (x, None) => x, (None, y) => y }
    })
};
```

If either Arc is not yet in scope at :12308, thread it the same way `crdt_state` was threaded for ZEB-918 (clone before the closure/task that builds the driver). If an accessor doesn't exist on the registry type, add a minimal `pub(crate) fn last_any_served_ms(&self, peer: &[u8; 32]) -> Option<u64>` (read-only, mirrors what the snapshot loop reads inline).

- [ ] **Step 1:** Wire the closure + new ctor arg; `cargo check --locked --features test-fixtures` → compiles.
- [ ] **Step 2:** `scripts/test-select --context task` → green.
- [ ] **Step 3: Commit** `feat(zeb910): wire proven-traffic evidence into the gateway dial driver`.

---

### Task 9: E2E — all-slots resolve bridges multiple publishers

**Files:**
- Create: `src-tauri/tests/pkarr_net/zeb910_all_slots.rs`
- Modify: `src-tauri/tests/pkarr_net_tests.rs` (add `#[path = "pkarr_net/zeb910_all_slots.rs"] mod zeb910_all_slots;`)

**Interfaces:** Consumes Task 2's `resolve_rendezvous_all_slots` + the `zeb918_epoch_rotation.rs` harness shapes (MockPkarrRelay strict, `CommunityRendezvousPublisher`, `small_blob`).

- [ ] **Step 1: Write the test** (60 s outer timeout, poll pattern from zeb918):

```rust
// Two publishers (distinct identity/device keys, distinct blobs with node ids
// [0x03;32] / [0x04;32]), advertisers = [owner_a, owner_b] so ranks 0 and 1 →
// slots 0 and 1 under ONE epoch key. Both refresh_slot. Then poll
// resolve_rendezvous_all_slots (fresh PkarrResolver per attempt, enrolled set
// = both device vks) until it returns BOTH hits (assert node-id set equality,
// vouches verified — i.e. hits.len() == 2 with distinct iroh_node_ids).
// This is the bridge-both-islands shape the single-hit resolve cannot produce.
```

- [ ] **Step 2: Run it** — `cargo nextest run --locked --features test-fixtures -E 'test(zeb910_all_slots)'` → PASS (build the harness-binary first if stale: this is an integration test compiled via `pkarr_net_tests`).
- [ ] **Step 3: Commit** `test(zeb910): e2e — all-slots resolve returns every publisher's beacon`.

---

### Task 10: Docs touch-up, full gates, PR

- [ ] **Step 1:** Re-read the spec vs. the diff; fix drift (constants, arm names) in whichever is wrong.
- [ ] **Step 2:** `cargo fmt --all`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; full sweep `cargo nextest run --locked --workspace --all-targets --features test-fixtures` (background with wall-clock supervision; foreground gates for everything else).
- [ ] **Step 3:** `git status --porcelain` clean; push branch; open PR (`gh --repo zeblithic/harmony-client`) with the prepared body (problem/design/rejected-alternatives/tests, "Closes ZEB-910", session footer); fire `@coderabbitai review` once; pushover; converge.
