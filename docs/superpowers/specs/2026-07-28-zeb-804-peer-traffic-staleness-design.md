# ZEB-804: honest per-peer traffic staleness in network_health_snapshot — design

**Ticket:** ZEB-804 (High). **Decisions of record (Jake, 2026-07-28):** traffic signal =
stats-tick + acceptor stamps; `lastSeenMs` absorbed (made honest) + explicit new fields;
all five remedies in one PR.

**Recon basis:** `.superpowers/zeb804/recon-report.md` (file:line for every claim below;
controller-verified: `iroh-1.0.2 Connection::stats()` exists at
`src/endpoint/connection.rs:1022`, returning quinn `ConnectionStats` with per-frame-type
`frame_rx` counters).

## 1. Problem

A peer dark for 2h15m reads `connectionMode=direct, rtt 14ms, transportDisabledReason:
null` — indistinguishable from a healthy peer. Root causes, all confirmed:

1. `peers[].lastSeenMs` is a max-merge of four sources, **every one** an
   establishment/announce stamp; none observes data exchange. The liveness machine
   deliberately preserves `since_ms` across `Connected→Connected`, so the value freezes at
   boot.
2. `connectionMode`/`rtt` come from two overlays: the liveness overlay honestly reports
   iroh's QUIC-path belief (which cannot decay without traffic), and the ZEB-595 self-test
   overlay has **no TTL and no staleness check** — a boot-era manual self-test survives
   process-lifetime, and when liveness has no slot for a peer (the `None => {}` arm) it is
   never cleared at all.
3. `dialStatus.succeeded` (boot-monotonic, supervisor-ladder dials only) sits beside
   `connected` (live states, including `mark_connected` entries from inbound accepts and
   zenoh `new_link` dials that touch no telemetry) — two correct answers to two
   unasked-apart questions.
4. Nothing anywhere records "when did this peer last actually exchange data with us."

## 2. Design overview

Two new per-peer traffic signals, merged into the snapshot by the existing
`iroh_node_id` join; `lastSeenMs` becomes honest by absorbing them; a derived staleness
tier degrades the UI badge on silence; the self-test overlay gets a TTL; `dialStatus`
gets a scope-labeling counter. All wire changes are **additive** (camelCase, `Option` +
`#[serde(default)]`).

## 3. `lastTrafficMs` — application-frame diff on the existing liveness tick

The `run_conn_path_watcher` in `peer_liveness.rs` already owns the peer's `[u8;32]`
endpoint id, a `Connection` clone, and a 30s `RTT_REFRESH_INTERVAL` tick. Each tick
additionally samples `conn.stats()` and computes an **application-frame count**:

```
app_frames = stats.frame_rx.stream + stats.frame_rx.datagram
```

- Counting **frames, not UDP bytes**, is load-bearing: QUIC keepalives/ACKs/PINGs advance
  byte counters, and a reachable-but-mute peer (the ZEB-804 incident shape) would read
  fresh forever — the same lie one layer down. STREAM/DATAGRAM frames are application
  payload only.
- **RX only, deliberately.** quinn counts a frame as sent when it is *transmitted*, not
  when it is acknowledged — retransmissions into a blackholed path keep advancing
  `frame_tx` forever, which would re-create the exact lie being fixed on the send side.
  A received application frame is the one signal that cannot be counterfeited by a dead
  path. (A peer that genuinely receives from us but never sends — half-mute — reads
  stale; that is the correct verdict for "no evidence of exchange".)
- **First-sample semantics:** the first tick after connect stores the baseline and does
  NOT stamp (handshake-era frames must not create an establishment-flavored stamp — the
  exact defect being fixed). Stamps happen only on a later tick observing `delta > 0`.
- The stamp lands in `PeerSlot.last_traffic_ms` — **repurposing the existing dead field**
  `last_connected_ms` (`peer_liveness.rs:136-137`, written-never-read, `#[allow(dead_code)]`):
  rename it, change its write from "every path report" to "app-frame delta observed",
  drop the allow. The slot-level field survives `Connected↔Degraded` transitions.
- **Surface change:** `LivenessHandle::states_snapshot()` currently returns
  `Vec<([u8;32], LivenessStateWire)>`-shaped data; it becomes per-peer
  `PeerLivenessView { state: LivenessStateWire, last_traffic_ms: Option<u64> }` (exact
  current signature pinned in the plan). The wire enum itself is unchanged — the
  timestamp is state-independent.
- Testing seam: the watcher's stats read goes through a small `ConnStatsProbe` trait
  (prod = `conn.stats()`; tests inject a scripted sequence), because `Connection` is not
  constructible in unit tests.

Scope note: this tick observes the **zenoh-transport connection** (the one the watcher
tracks). Service-plane connections (relay pull, butler, pex, friend, invite, vine) are
separate per-ALPN iroh connections — covered by §4.

## 4. `lastRelayPullServedMs` + acceptor traffic stamps

New `PeerTrafficRegistry` (in `network_health.rs`, beside the other telemetry types):
`Mutex<HashMap<[u8;32], PeerTrafficStamps>>` with
`PeerTrafficStamps { last_any_served_ms: u64, last_relay_pull_served_ms: Option<u64> }`,
methods `record_relay_pull_served(peer_id, now_ms)` and `record_served(peer_id, now_ms)`.
(Low call rate — one stamp per served request — so a mutex map is fine; no hot-path
atomics needed here.)

- `iroh_community_relay_acceptor.rs` stamps `record_relay_pull_served` at the existing
  `record_served` site, using the **full** `conn.remote_id()` bytes — deliberately NOT
  the existing `CommunityRelayServingTelemetry` map, which truncates to 4 bytes at the
  writer (ZEB-329) and cannot be joined back to `peers[]`. The redaction rule is about
  what we *persist/log*; this registry is in-memory-only and feeds a surface that already
  displays full owner addrs.
- The five sibling acceptors (`iroh_butler_acceptor`, `iroh_pex_acceptor` ×2 sites,
  `iroh_friend_acceptor`, `iroh_invite_acceptor`, `vine_relay`) stamp `record_served` at
  their equivalent served-a-request sites.
- Wiring: the registry `Arc` rides each acceptor's existing `with_telemetry`-style
  builder injection from boot (`lib.rs`), same shape as
  `CommunityRelayServingTelemetry`.

## 5. Snapshot merge + new DTO fields

In `NetworkHealthService::snapshot`, after the liveness overlay:

- `lastTrafficMs` = max(liveness `last_traffic_ms`, registry `last_any_served_ms`),
  joined by `record.iroh_node_id` (the join that already exists for the liveness
  overlay).
- `lastRelayPullServedMs` = registry value, verbatim.
- `connectedSinceMs` = the establishment stamp (what `lastSeenMs` effectively was),
  surfaced under its honest name.
- `lastSeenMs` (existing key, kept) = max(its current four sources, `lastTrafficMs`) —
  the field finally means "most recent evidence this peer exists", and precise readers
  use the explicit fields.

## 6. Derived staleness tier + UI degradation

Per-peer `staleness: "fresh" | "quiet" | "dark"`, computed server-side at snapshot
assembly (as-of `now_ms`, the ZEB-212 as-of seam pattern):

- `fresh`: any traffic evidence (`lastTrafficMs`) within `STALENESS_QUIET_MS` (5 min).
- `quiet`: within `STALENESS_DARK_MS` (30 min).
- `dark`: older, or no traffic evidence ever while `connectionMode != NoConnection`.
- A peer with `connectionMode == NoConnection` gets no tier (`null`) — absence of a
  connection is already honest.

Constants are named, doc-commented against the relay-pull cadence they're derived from,
and deliberately generous (false-"dark" on a genuinely idle-but-healthy peer is
acceptable; the tier says "no evidence", not "down").

UI (`NetworkHealthView.svelte`): the ✓/⚠/✗ badge derivation takes `staleness` into
account — `dark` forces ⚠ with a "no traffic for Xm" annotation regardless of
`connectionMode`; `connectionMode`/`rtt` render with a "last confirmed" qualifier when
not `fresh`. (Svelte + vitest changes; no new IPC.)

## 7. Self-test overlay TTL

The ZEB-595 overlay refuses to apply when
`now_ms - report.finished_at_ms > SELF_TEST_OVERLAY_MAX_AGE_MS` (10 min).
`finished_at_ms` already exists on the report; it is simply unconsulted today. Stale
reports still render in the dedicated self-test section (that surface is explicitly a
"most recent test result" memo) — only the **per-peer connectionMode/rtt dressing**
expires. This also fixes the liveness-miss arm: a peer absent from liveness with only a
stale self-test now falls back to `NoConnection` instead of wearing boot-era `direct/14ms`
forever.

## 8. `dialStatus` legibility

- New additive counter `connectedViaRegistry` (monotonic, process-lifetime): as
  implemented, stamped inside `SupervisorHandle::mark_connected` itself
  (`reconnect_supervisor.rs`; chosen over the `zenoh_iroh_transport.rs` call site — fewer
  files, one authoritative choke point), counting every Connected entry that arrives via
  the registry swap: inbound accepts AND all zenoh `new_link` outcomes — including the
  connection a successful ladder dial produces. It is therefore a SUPERSET of
  ladder-success connections, not their complement; `succeeded` and `connectedViaRegistry`
  can both move on one dial (final-review as-implemented correction).
- DTO doc comments (and `docs/headless-install.md` surface list if it documents the
  block) pin the scopes: `attempts/succeeded/failed` = "supervisor-ladder outbound dial
  outcomes since process start"; `connected/retrying/dormant` = "live supervisor peer
  states at snapshot time"; `connectedViaRegistry` = "lifetime Connected entries via
  registry swap". No renames, no removals.

## 9. Wire compatibility

Every new field: camelCase, `Option<...>` + `#[serde(default)]`. Existing keys and
semantics preserved except `lastSeenMs`, which only gains freshness (its value is now ≥
the old value — monotone improvement, no consumer can regress).

## 10. Testing

- **Liveness stats-diff (unit, `peer_liveness.rs`):** scripted `ConnStatsProbe`
  sequences — no stamp on first sample; stamp on rx stream-frame delta; NO stamp on
  byte-only/PING-only deltas (the keepalive counterfeit case) and NO stamp on
  tx-only deltas (the retransmission-into-blackhole counterfeit case — both
  load-bearing tests); timestamp survives `Connected→Degraded`.
- **Registry + acceptor stamps (unit):** relay acceptor records both stamps with the
  full 32-byte id; sibling stamp sites record `last_any`.
- **Snapshot merge (unit, `network_health.rs`):** max-merge precedence across
  liveness/registry sources; `lastSeenMs` absorbs `lastTrafficMs`;
  `connectedSinceMs` carries the establishment stamp; camelCase key pinning +
  snake-leak sweep for every new field (repo convention).
- **Staleness tier (unit):** threshold boundaries with injected clock; `NoConnection` →
  `null` tier; the incident replay: `Connected{Direct,14ms}` + no traffic evidence →
  `dark` (the test that would have caught ZEB-804).
- **Self-test TTL (unit):** fresh report applies; stale report does not; stale +
  liveness-miss → `NoConnection`.
- **dialStatus (unit):** `mark_connected` increments `connectedViaRegistry` and not
  `succeeded`.
- **UI (vitest):** badge degrades on `dark` regardless of `connectionMode`.
- **Live evidence (post-merge, on the running fleet + xwan soak):** snapshot excerpt
  showing a healthy peer `fresh` with advancing `lastTrafficMs`, posted to ZEB-804.

## 11. Non-goals

- Zenoh link-layer stamping (`IrohZenohLink`) — rejected as redundant with the
  stats-tick (zenoh rides the same connection); revisit only if soak shows gaps.
- Renaming/removing `lastSeenMs` — rejected for compat.
- ZEB-790 (HLC wall-stamp merging) — the ticket's "adjacent" note; separate ticket.
- Fixing the drop-watcher's silent-blackhole `Connected` pinning beyond what the
  staleness tier surfaces — if soak shows it matters, it's a follow-up with its own
  design (touching QUIC idle/keepalive config, not observability).
