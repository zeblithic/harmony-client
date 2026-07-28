# ZEB-804 Peer Traffic Staleness Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `network_health_snapshot` gains honest per-peer traffic evidence (`lastTrafficMs`, `lastRelayPullServedMs`, `connectedSinceMs`), a derived staleness tier that degrades the UI badge on silence, a TTL on the self-test overlay, and a legible `dialStatus` — so a two-hour-dark peer can never again read `direct/14ms/healthy`.

**Architecture:** The liveness watcher's existing 30s tick samples `Connection::stats()` received application frames and reports cumulative counts to the liveness handle, which owns baseline/delta tracking per peer (`report_traffic`). A new `PeerTrafficRegistry` is stamped by the six iroh acceptors at their served-a-request sites. `NetworkHealthService::snapshot` max-merges both into new additive DTO fields and derives the staleness tier.

**Tech Stack:** Rust (tokio, iroh 1.0.2 / quinn stats), Svelte + vitest for the panel.

**Spec:** `docs/superpowers/specs/2026-07-28-zeb-804-peer-traffic-staleness-design.md`. Recon anchors: `.superpowers/zeb804/recon-report.md`.

## Global Constraints

- All cargo commands from `src-tauri/`, always `--locked`, tests always `--features test-fixtures` (CLAUDE.md). Clippy gate CI-exact: `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; fmt gate `cargo fmt --all -- --check`.
- Every new wire field: camelCase via the struct's existing `#[serde(rename_all = "camelCase")]`, typed `Option<...>` with `#[serde(default)]`. Existing keys/semantics preserved (spec §9); `lastSeenMs` may only gain freshness.
- **App-frame counting is RX-only, frames-only** (`frame_rx.stream + frame_rx.datagram`): bytes count keepalives, tx counts retransmissions into blackholes — both re-create the lie (spec §3). Never "improve" this to bytes or tx.
- No wall-clock in tests: liveness tests use the handle's real `now_ms` only where the assertion is presence/absence of a stamp (not its value); snapshot/tier tests inject `now` explicitly.
- Run every gate in the FOREGROUND with the Bash tool `timeout` parameter — never `run_in_background`. Do not end your turn until your report file exists.
- Iterate with `-E` filters; gate each task with `scripts/test-select --context task` from repo root — paste its `round=… bucket=…` summary line into the task report; commit per task with `ZEB-804:` prefix.
- macOS: no `timeout` binary; use the Bash tool timeout parameter. Never pipe a gate through `tail`/`head` without zsh `${pipestatus[1]}`.

---

### Task 1: Liveness traffic stamp (`report_traffic` + `views_snapshot`)

**Files:**
- Modify: `src-tauri/src/peer_liveness.rs`

**Interfaces:**
- Produces: `pub struct PeerLivenessView { pub state: LivenessStateWire, pub last_traffic_ms: Option<u64> }`; `LivenessHandle::views_snapshot(&self) -> Vec<([u8; 32], PeerLivenessView)>`; `LivenessHandle::report_traffic(&self, peer: [u8; 32], conn_id: usize, cumulative_app_frames: u64)`; `pub(crate) fn app_frame_count(stats: &iroh::endpoint::ConnectionStats) -> u64`. Task 3 consumes `views_snapshot` verbatim.
- `states_snapshot()` is kept unchanged (existing tests + any other callers).

- [ ] **Step 1: Write the failing tests** (append to the existing `#[cfg(test)] mod tests`):

```rust
#[test]
fn app_frame_count_ignores_keepalives_and_tx() {
    use iroh::endpoint::ConnectionStats;
    let mut stats = ConnectionStats::default();
    stats.frame_rx.ping = 500;
    stats.frame_rx.acks = 900;
    stats.udp_rx.bytes = 1_000_000;
    stats.frame_tx.stream = 700; // retransmission-into-blackhole counterfeit
    assert_eq!(app_frame_count(&stats), 0, "keepalives/acks/bytes/tx must not count");
    stats.frame_rx.stream = 3;
    stats.frame_rx.datagram = 2;
    assert_eq!(app_frame_count(&stats), 5, "rx stream+datagram frames count");
}

#[tokio::test(start_paused = true)]
async fn traffic_first_sample_baselines_without_stamp() {
    let h = LivenessHandle::new();
    h.on_transport_up(peer(1), 11);
    h.report_traffic(peer(1), 11, 40); // handshake-era frames: baseline only
    let views = h.views_snapshot();
    assert!(
        matches!(views.as_slice(), [(p, v)] if *p == peer(1) && v.last_traffic_ms.is_none()),
        "first sample must baseline, never stamp"
    );
}

#[tokio::test(start_paused = true)]
async fn traffic_delta_stamps_and_zero_delta_does_not() {
    let h = LivenessHandle::new();
    h.on_transport_up(peer(1), 11);
    h.report_traffic(peer(1), 11, 40);
    h.report_traffic(peer(1), 11, 41); // delta > 0 → stamp
    let stamped = h.views_snapshot()[0].1.last_traffic_ms;
    assert!(stamped.is_some(), "rx app-frame delta stamps last_traffic_ms");
    h.report_traffic(peer(1), 11, 41); // delta == 0 → no change
    assert_eq!(h.views_snapshot()[0].1.last_traffic_ms, stamped);
}

#[tokio::test(start_paused = true)]
async fn traffic_stale_conn_ignored_and_baseline_resets_on_swap() {
    let h = LivenessHandle::new();
    h.on_transport_up(peer(1), 11);
    h.report_traffic(peer(1), 11, 40);
    h.report_traffic(peer(1), 10, 90); // superseded conn → ignored entirely
    assert!(h.views_snapshot()[0].1.last_traffic_ms.is_none());
    h.on_transport_up(peer(1), 12); // conn swap resets the baseline
    h.report_traffic(peer(1), 12, 7); // NEW conn's first sample: baseline, no stamp
    assert!(
        h.views_snapshot()[0].1.last_traffic_ms.is_none(),
        "a fresh conn's first cumulative sample must baseline, not diff against the old conn"
    );
    h.report_traffic(peer(1), 12, 8);
    assert!(h.views_snapshot()[0].1.last_traffic_ms.is_some());
}

#[tokio::test(start_paused = true)]
async fn traffic_stamp_survives_degraded_and_disconnect() {
    let h = LivenessHandle::new();
    h.on_transport_up(peer(1), 11);
    h.report_traffic(peer(1), 11, 1);
    h.report_traffic(peer(1), 11, 2);
    let stamped = h.views_snapshot()[0].1.last_traffic_ms;
    assert!(stamped.is_some());
    h.report_path(peer(1), 11, None, None); // Connected→Degraded
    assert_eq!(h.views_snapshot()[0].1.last_traffic_ms, stamped);
    h.on_transport_down(peer(1), 11); // evidence of past exchange persists
    assert_eq!(h.views_snapshot()[0].1.last_traffic_ms, stamped);
}
```

- [ ] **Step 2: Run to verify they fail** — `cargo nextest run --locked --features test-fixtures -E 'test(peer_liveness)'`; expect compile failures on `app_frame_count` / `views_snapshot` / `report_traffic`.

- [ ] **Step 3: Implement.**

In `PeerSlot` (fields at `peer_liveness.rs:122-138`):
- Rename `last_connected_ms` → `last_traffic_ms`, delete its `#[allow(dead_code)]`, and rewrite its doc: "Wall-clock ms when an rx application-frame delta was last observed for this peer (ZEB-804). Survives conn swaps and disconnects — it is evidence of past exchange, not connection state."
- Add `app_frames_baseline: Option<u64>` with doc: "Cumulative rx app-frame count at the last `report_traffic` sample for the CURRENT conn. Reset to `None` on any conn swap/down so a new connection's counters are never diffed against the old one's."
- **Delete** the `slot.last_connected_ms = Some(now_ms());` write in `report_path` (line 304) — that stamp is establishment-flavored, the exact defect being fixed. `ever_connected` stays.
- Reset `app_frames_baseline = None` at every site that installs or clears a conn: both arms of `on_transport_up` (replace + insert), the insert arm of `on_transport_up_external`, and `on_transport_down` (the two insert sites construct the struct — add the field there).

New items (place `app_frame_count` next to `run_conn_path_watcher`, the module's one iroh-touching region):

```rust
/// Received application frames (STREAM + DATAGRAM) on a connection — the
/// ZEB-804 traffic-evidence basis. RX-only and frames-only, both load-bearing:
/// byte counters advance on QUIC keepalives/ACKs, and tx frame counters advance
/// on retransmissions into a blackholed path — either would let a dead-or-mute
/// peer read fresh forever, the exact lie this exists to fix.
pub(crate) fn app_frame_count(stats: &iroh::endpoint::ConnectionStats) -> u64 {
    stats.frame_rx.stream + stats.frame_rx.datagram
}
```

On `LivenessHandle`:

```rust
/// ZEB-804: cumulative rx app-frame sample for `conn_id`'s connection.
/// Conn-guarded like `report_path`. First sample for a conn baselines without
/// stamping (handshake-era frames must not masquerade as exchange evidence);
/// a later sample with a positive delta stamps `last_traffic_ms` and bumps the
/// changed watch.
pub fn report_traffic(&self, peer: [u8; 32], conn_id: usize, cumulative_app_frames: u64) {
    let changed = {
        let mut slots = self.inner.slots.lock().expect("slots lock");
        match slots.get_mut(&peer) {
            Some(slot) if slot.conn_id == Some(conn_id) => match slot.app_frames_baseline {
                None => {
                    slot.app_frames_baseline = Some(cumulative_app_frames);
                    false
                }
                Some(base) if cumulative_app_frames > base => {
                    slot.app_frames_baseline = Some(cumulative_app_frames);
                    slot.last_traffic_ms = Some(now_ms());
                    true
                }
                Some(_) => false,
            },
            _ => false,
        }
    };
    if changed {
        self.inner.changed_tx.send_modify(|e| *e += 1);
    }
}
```

View type + snapshot (beside `states_snapshot`):

```rust
/// ZEB-804: per-peer liveness projection carrying the state AND the
/// state-independent traffic-evidence stamp.
#[derive(Debug, Clone, PartialEq)]
pub struct PeerLivenessView {
    pub state: LivenessStateWire,
    pub last_traffic_ms: Option<u64>,
}

pub fn views_snapshot(&self) -> Vec<([u8; 32], PeerLivenessView)> {
    let slots = self.inner.slots.lock().expect("slots lock");
    slots
        .iter()
        .map(|(p, slot)| {
            (*p, PeerLivenessView {
                state: slot.state.to_wire(),
                last_traffic_ms: slot.last_traffic_ms,
            })
        })
        .collect()
}
```

In `run_conn_path_watcher`, extend ONLY the tick arm (line 429) — path events fire on topology change and must not double-sample:

```rust
_ = tick.tick() => {
    report(&handle);
    handle.report_traffic(peer, conn_id, app_frame_count(&conn.stats()));
}
```

- [ ] **Step 4: Run the module tests** — same `-E 'test(peer_liveness)'` filter; expect all green including the 8 pre-existing tests.
- [ ] **Step 5: Gate + commit** — `scripts/test-select --context task` (repo root), `cargo fmt --all`, CI-exact clippy; `git add src-tauri/src/peer_liveness.rs` and commit `ZEB-804: liveness traffic stamp — rx app-frame delta on the RTT tick`.

---

### Task 2: `PeerTrafficRegistry` + acceptor stamps + boot wiring

**Files:**
- Modify: `src-tauri/src/network_health.rs` (registry type, near `DialTelemetry` ~line 450)
- Modify: `src-tauri/src/iroh_community_relay_acceptor.rs` (~line 938 served site; builder ~line 928)
- Modify: `src-tauri/src/iroh_butler_acceptor.rs` (~line 1372), `src-tauri/src/iroh_pex_acceptor.rs` (~lines 614, 756), `src-tauri/src/iroh_friend_acceptor.rs` (~line 2081), `src-tauri/src/iroh_invite_acceptor.rs` (~line 543), `src-tauri/src/vine_relay.rs` (~line 747)
- Modify: `src-tauri/src/lib.rs` (one shared `Arc` at boot; injection into each acceptor + the health service)

**Interfaces:**
- Produces: `pub struct PeerTrafficRegistry` with `record_relay_pull_served(&self, peer: [u8; 32], now_ms: u64)`, `record_served(&self, peer: [u8; 32], now_ms: u64)`, `stamps(&self, peer: &[u8; 32]) -> Option<PeerTrafficStamps>`; `#[derive(Debug, Clone, Copy, PartialEq)] pub struct PeerTrafficStamps { pub last_any_served_ms: u64, pub last_relay_pull_served_ms: Option<u64> }`. Task 3 consumes `stamps`.

- [ ] **Step 1: Write the failing registry unit tests** (in `network_health.rs` tests):

```rust
#[test]
fn peer_traffic_registry_stamps_and_relay_pull_specificity() {
    let reg = PeerTrafficRegistry::default();
    let p = [7u8; 32];
    assert!(reg.stamps(&p).is_none());
    reg.record_served(p, 1_000);
    assert_eq!(
        reg.stamps(&p),
        Some(PeerTrafficStamps { last_any_served_ms: 1_000, last_relay_pull_served_ms: None })
    );
    reg.record_relay_pull_served(p, 2_000);
    assert_eq!(
        reg.stamps(&p),
        Some(PeerTrafficStamps { last_any_served_ms: 2_000, last_relay_pull_served_ms: Some(2_000) }),
        "a relay pull is also 'any' traffic"
    );
    reg.record_served(p, 3_000);
    assert_eq!(
        reg.stamps(&p).unwrap().last_relay_pull_served_ms,
        Some(2_000),
        "a non-relay serve must not advance the relay-pull stamp"
    );
}
```

- [ ] **Step 2: Verify failure**, then implement the registry:

```rust
/// ZEB-804: in-memory per-peer served-traffic stamps, written by the iroh
/// acceptors at their served-a-request sites and read by `snapshot`. Keyed by
/// the FULL 32-byte iroh endpoint id — deliberately not the 4-byte-truncated
/// ZEB-329 relay-serving map, which cannot be joined back to `peers[]`. The
/// redaction rule governs what we log/persist; this map is in-memory only and
/// feeds a surface that already shows full owner addrs. Call rate is one stamp
/// per served request, so a mutex map is fine (no hot-path atomics).
#[derive(Debug, Default)]
pub struct PeerTrafficRegistry {
    stamps: Mutex<HashMap<[u8; 32], PeerTrafficStamps>>,
}
```

with the three methods (`record_served` upserts `last_any_served_ms = now_ms`; `record_relay_pull_served` sets both; `stamps` clones out). `now_ms` is a parameter (injected-clock convention), stamped by callers with their existing time source.

- [ ] **Step 3: Acceptor stamps.** In `iroh_community_relay_acceptor.rs`, at the existing served site (beside `record_served`/the ZEB-458 log, ~line 938): `if let Some(reg) = traffic.as_ref() { reg.record_relay_pull_served(*conn.remote_id().as_bytes(), now_ms); }` — using the SAME full-id expression the sibling acceptors use (`*conn.remote_id().as_bytes()`), and the file's existing time source. Injection: a `with_traffic_registry(Arc<PeerTrafficRegistry>)` builder method copying the exact `with_telemetry` shape (~line 928). Repeat the same pattern (field + builder + one stamp line, `record_served` flavor) in the five siblings at the listed anchors. If a listed anchor's surrounding code has moved or a sibling genuinely lacks `conn.remote_id()` in scope at its served site, stamp at the nearest point where it IS in scope and record the deviation in your report — do not thread new parameters deep through helper fns.
- [ ] **Step 4: Boot wiring** in `lib.rs`: construct `let peer_traffic = Arc::new(PeerTrafficRegistry::default());` once where the other health telemetry Arcs are built; pass clones into each acceptor's builder; install into the health service via a `set_peer_traffic_source(Arc<PeerTrafficRegistry>)` following the existing `set_gateway_bootstrap_source` install pattern. (Recon: snapshot-source installs cluster at `lib.rs:12656-12671`; acceptor constructions are found by grepping each acceptor's `::new`/builder call.)
- [ ] **Step 5: Run** `-E 'test(network_health)'` + compile checks; then gate (`test-select --context task`, fmt, CI-exact clippy) and commit `ZEB-804: PeerTrafficRegistry + acceptor served-traffic stamps`.

---

### Task 3: Snapshot merge + three new DTO fields

**Files:**
- Modify: `src-tauri/src/network_health.rs`

**Interfaces:**
- Consumes: `views_snapshot` (Task 1), `PeerTrafficRegistry::stamps` (Task 2).
- Produces on `PeerHealth` (struct at line 116) AND `ResolverPeerRecord` (line 1421): `last_traffic_ms: Option<u64>`, `last_relay_pull_served_ms: Option<u64>`, `connected_since_ms: Option<u64>` — all `#[serde(default)]` on the wire type. Task 4 consumes these for the tier.

- [ ] **Step 1: Failing tests.** Extend the existing snapshot-assembly tests (the fakes at ~line 4710/5891 implement the source traits):
  - a fake liveness source reporting `PeerLivenessView { state: Connected{..}, last_traffic_ms: Some(T) }` + a registry with a later `last_any_served_ms` → `peers[0].lastTrafficMs == max`, `lastSeenMs >= lastTrafficMs` (absorption), `connectedSinceMs == the liveness since_ms`;
  - registry-only traffic (liveness `None`) still surfaces;
  - serde pin: serialize a `PeerHealth` and assert the exact camelCase keys `lastTrafficMs`, `lastRelayPullServedMs`, `connectedSinceMs` are present and no `last_traffic_ms` snake leak exists (follow the file's existing key-sweep test idiom).
- [ ] **Step 2: Verify failures.**
- [ ] **Step 3: Implement.**
  - `LivenessSnapshot` trait (line 1557): add `fn peer_views(&self) -> Vec<([u8; 32], PeerLivenessView)> { Vec::new() }` — **default-bodied so existing test fakes compile unchanged**; implement it on `ProdLivenessSnapshot` via `views_snapshot()`. The assembly switches its `liveness_states` map (line 1886) to build from `peer_views()`, deriving the old `LivenessStateWire` map from `.state` (all existing joins keep working) plus a parallel `HashMap<[u8;32], u64>` of traffic stamps.
  - New service field `peer_traffic: Option<Arc<PeerTrafficRegistry>>` + `set_peer_traffic_source` (wired in Task 2 Step 4; add the setter here if Task 2 stubbed it).
  - In the assembly, immediately after the existing last-seen fold (lines 2018-2031):
    1. `connected_since_ms` = liveness `Connected.since_ms` for the record's node id, else the supervisor `Connected.since_ms` fallback (reuse the `connected_since` map built at line 1930) — the establishment stamp under its honest name; the presence cache does NOT feed it.
    2. `last_traffic_ms` = max(liveness traffic stamp, registry `last_any_served_ms`); `last_relay_pull_served_ms` = registry value verbatim.
    3. `record.last_seen_ms` = max(its current value, `last_traffic_ms`) — the absorption.
  - Plumb the three fields through `ResolverPeerRecord` (default `None` at the `list_records` construction, line ~3050) and copy onto `PeerHealth` in `filter_peers_by_shared_membership`.
- [ ] **Step 4: Run** `-E 'test(network_health)'`; green including all pre-existing snapshot tests.
- [ ] **Step 5: Gate + commit** `ZEB-804: snapshot merge — lastTrafficMs / lastRelayPullServedMs / connectedSinceMs`.

---

### Task 4: Staleness tier + self-test overlay TTL + dialStatus legibility

**Files:**
- Modify: `src-tauri/src/network_health.rs`
- Modify: `src-tauri/src/zenoh_iroh_transport.rs` (~line 373, `mark_supervisor_connected`)
- Modify: `docs/headless-install.md` only if it documents the `dialStatus` block (grep first)

**Interfaces:**
- Produces on `PeerHealth`: `staleness: Option<PeerStaleness>` (`#[serde(default)]`), `#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)] #[serde(rename_all = "camelCase")] pub enum PeerStaleness { Fresh, Quiet, Dark }` (wire: `"fresh" | "quiet" | "dark"`). On `DialHealthSummary`: `connected_via_registry: u64` (`#[serde(default)]`). Task 5 consumes `staleness` from the TS side.

- [ ] **Step 1: Failing tests.**
  - **The incident replay (load-bearing):** a peer whose liveness state is `Connected { mode: Direct, rtt_ms: Some(14), .. }` with NO traffic evidence anywhere → snapshot shows `connectionMode: direct` AND `staleness: dark`. Doc comment on the test: "the test that would have caught ZEB-804".
  - Tier boundaries with injected `now`: traffic age < 5 min → `fresh`; 5–30 min → `quiet`; > 30 min → `dark`; `connection_mode == NoConnection` → `staleness: None`.
  - Self-test TTL: a `SelfTestReport` with `finished_at_ms` 11 minutes old must NOT dress a record (mode/rtt stay `NoConnection`/`None`); a 5-minute-old one still does; existing overlay tests keep passing with fresh reports.
  - `DialTelemetry`: `record_connected_via_registry()` increments the new counter and nothing else; `summary()` carries it; serde pin for `connectedViaRegistry`.
- [ ] **Step 2: Verify failures.**
- [ ] **Step 3: Implement.**
  - Constants beside the other module constants, doc-commented: `const STALENESS_QUIET_MS: u64 = 300_000;` `const STALENESS_DARK_MS: u64 = 1_800_000;` `const SELF_TEST_OVERLAY_MAX_AGE_MS: u64 = 600_000;` (generous by design — the tier says "no evidence", not "down"; derived from the relay-pull cadence, spec §6).
  - Tier derivation in the assembly, after Task 3's merge: `None` when `connection_mode == NoConnection`; else by `now - last_traffic_ms` against the two thresholds; `last_traffic_ms == None` → `Dark`.
  - TTL: wrap the overlay read (line 1956-1957) — `if let Some(report) = last.as_ref().filter(|r| now.saturating_sub(r.finished_at_ms) <= SELF_TEST_OVERLAY_MAX_AGE_MS)`. The dedicated self-test report section of the snapshot is untouched (it is explicitly a memo surface).
  - `DialTelemetry`: `connected_via_registry: AtomicU64` field + recorder + `summary()` load; doc comments on `DialHealthSummary` pinning the scopes verbatim from spec §8 (`attempts/succeeded/failed` = supervisor-ladder outbound dials since process start; `connected/retrying/dormant` = live states at snapshot; `connectedViaRegistry` = lifetime Connected entries via registry swap).
  - `zenoh_iroh_transport.rs` `mark_supervisor_connected` (~line 373): call `record_connected_via_registry()` on the dial telemetry if that function has (or can cheaply receive) the `Arc<DialTelemetry>` — recon says the supervisor handle is in scope there; the telemetry Arc rides the same wiring the supervisor got. If threading the Arc requires touching more than the transport's construction site + this call, stamp instead at `SupervisorHandle::mark_connected` itself (`reconnect_supervisor.rs:309`) where the handle can own an optional telemetry Arc — pick whichever touches fewer files and record the choice.
- [ ] **Step 4: Run** `-E 'test(network_health) or test(reconnect_supervisor) or test(zenoh_iroh)'`.
- [ ] **Step 5: Gate + commit** `ZEB-804: staleness tier, self-test overlay TTL, dialStatus scopes`.

---

### Task 5: UI badge degradation + TS types

**Files:**
- Modify: `src/lib/components/NetworkHealthView.svelte` (badge fns at lines 174-201)
- Create: `src/lib/networkHealthStaleness.ts` (pure badge/label helpers)
- Test: a vitest file beside the repo's existing frontend test layout (`git grep -l "vitest" src/` first; follow the existing component-test pattern)
- Modify: the TS `PeerHealth` type — find it with `git grep -n "lastSeenMs" src/ gen/` and extend with the four new optional fields. If the type is GENERATED (under `gen/`), run the repo's schema regeneration (check `package.json` scripts / `gen/README`) instead of hand-editing, and say so in the report.

**Interfaces:**
- Consumes: `staleness`, `lastTrafficMs`, `connectedSinceMs` (Task 4/3 wire fields).

- [ ] **Step 1: Failing vitest** for the new pure helpers:

```ts
import { describe, expect, it } from 'vitest';
import { peerBadge, stalenessLabel } from '$lib/networkHealthStaleness';

describe('peerBadge', () => {
  it('dark forces the warn badge over a direct connection', () => {
    expect(peerBadge('direct', 'dark')).toBe('⚠');   // the ZEB-804 lie, fixed
    expect(peerBadge('direct', 'fresh')).toBe('✓');
    expect(peerBadge('direct', null)).toBe('✓');      // pre-field snapshots degrade gracefully
    expect(peerBadge('noConnection', null)).toBe('✗');
    expect(peerBadge('relay', 'quiet')).toBe('⚠');
  });
});

describe('stalenessLabel', () => {
  it('annotates non-fresh tiers with the age', () => {
    expect(stalenessLabel('dark', 8_100_000)).toContain('no traffic');
    expect(stalenessLabel('fresh', 30_000)).toBe('');
  });
});
```

- [ ] **Step 2: Verify failure** — `npx vitest run` (repo root) for the new file.
- [ ] **Step 3: Implement** `networkHealthStaleness.ts`: `peerBadge(mode, staleness)` reproducing the current `peerStatusIcon` mapping (direct→✓, relay/degraded→⚠, else ✗) with one override — `staleness === 'dark'` forces ⚠ unless the mode is already `noConnection`; `stalenessLabel(staleness, ageMs)` returning `''` for fresh/null and a "no traffic for Xm" / "quiet for Xm" annotation otherwise (mirror the file's existing age-formatting helper if one exists). Rewire `peerStatusIcon`/`peerStatusTitle` in the Svelte component to call the helpers, appending the annotation to the title and rendering rtt/mode with a "last confirmed" qualifier when not fresh (spec §6).
- [ ] **Step 4: Run** `npx vitest run` + `npx tsc --noEmit` (repo root) — both green.
- [ ] **Step 5: Gate + commit** `ZEB-804: panel badge degrades on traffic silence`.

---

### Final verification (controller)

- Full sweep: `cargo nextest run --locked --workspace --all-targets --features test-fixtures` (or 3-shard equivalent), fmt, CI-exact clippy, `npx tsc --noEmit`, `npx vitest run`.
- Live evidence on the running fleet after merge (spec §10): healthy peer `fresh` with advancing `lastTrafficMs`; posted to ZEB-804.
