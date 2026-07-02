# ZEB-321 Phase 3 — Liveness / Rebinding / Reconnection: Decision Record

**Status:** all areas decided + blessed 2026-07-01
**Ticket:** [ZEB-321](https://linear.app/zeblith/issue/ZEB-321) Phase 3
**Slices:** S0=ZEB-617 · S1=ZEB-613 · S2=ZEB-618 · S3=ZEB-619 · S4=ZEB-620 · S5=ZEB-621 · S6=ZEB-622 · S7=ZEB-623 · S8=ZEB-624 · S9=ZEB-522

## Settled scope (Jake, 2026-07-01)

- **Posture:** fleet-grade self-healing, desktop/headless. Success criterion: any two peers that were
  ever connected re-establish transport + data within ~a minute of both being online — across
  mid-session drops, network moves, and offline-return. Mobile push-wake parked; cadences chosen
  mobile-aware (battery/power ethos).
- **ZEB-599 sub-minute ambition folded in:** relay-mediated peers reconcile in seconds via
  presence-kick + re-dial, not the ~1h floor.
- **ZEB-613 (headless presence auto-subscribe) folded in** as a load-bearing dependency slice
  (presence-triggered re-dial is inert without it). Ticket's option 1 (auto-subscribe every joined
  community at serve startup + on join) was its own recommendation — confirm in area B.
- **Root-driver parity gap folded in:** mail-root + community-root fetch drivers get ZEB-584 floor
  persistence + #384 presence-kick (channel logs already have both).
- **Added areas:** protocol versioning/evolution (G); relay/infra strategy (H — pulls forward parts
  of Phase 5+: unused i.q8.fyi iroh relay, relay selection, canary-vs-pinned).
- **Deferred:** reconnect security posture (re-verify identity on re-dial, replay windows,
  stale-signed-record spoofing) → dedicated future pass, sequenced after A–C mechanics settle.
- **Out (stays sibling):** ZEB-615 MaxPathIdReached investigation (Low/observational; needs
  iroh=debug capture next cross-WAN run). ZEB-616 stale-face teardown is in flight in a parallel
  session — treat as landed for design purposes.
- **Deliverable:** umbrella decision record + ZEB-616-sized slice specs; Linear decomposition under
  ZEB-321 after Jake blesses the split.

## Ground truth (from 2026-07-01 code surveys; file:line refs verified against tree)

**Transport plane never reconnects:**
- Dial driver (ZEB-373): dial-once on first-learn of (owner,node_id); MAX_DIAL_ATTEMPTS=3,
  backoff 1s→2s, then terminal-for-session; DialedSet (cap 4096, FIFO) never un-claims —
  left-then-rejoined member never re-dialed; lossy try_send on 1024-deep hint channel can starve a
  peer permanently; dial tasks spawn unbounded. All deferrals annotated "ZEB-321 Phase 3"
  (iroh_dial_driver.rs:4-5,:33,:42-44,:166-167).
- Zenoh-over-iroh: no harmony-side reconnect; subscriber re-declare loops (5s→60s backoff) re-declare
  on a possibly-dead session, never re-establish the link. Zenoh session = Config::default() + id +
  endpoints only (ZEB-616 adds lease/keepalive).
- DM tunnel: reactive redial only on next send_dm (Closed/Closing arms re-dial with packet seeding;
  ZEB-485 single-dialer gate lower-NodeId-dials, FALLBACK_DIAL_DELAY=1s); no background reconnect;
  tunnel keepalive 25-35s jittered, dead ~110s.
- iroh endpoint: bound once, no TransportConfig/idle-timeout/keepalive overrides, no
  rebind-on-network-change path.

**Liveness signals are fragmented (4 disjoint notions):**
- Community presence: BEACON_INTERVAL_MS=10s, STALE_MS=30s, TTL-eviction, no gravestones.
  Presence-kick (#384) → FULL backfill reconcile (EPOCH_REARM_COOLDOWN_MS=60s shared gate), channel
  logs only. Activation is caller-driven: GUI subscribes all joined communities (App.svelte:1842-1860);
  headless nodes inert (ZEB-613).
- Transport epoch: direct (hop-1) zenoh peers only via peers_zid() polled ~5s; bump only on
  never-before-seen zid (TRANSPORT_SEEN_ZIDS_CAP=4096, never forgets → same-zid flap never bumps).
- Dial telemetry: process-lifetime counters + 32-deep ring.
- Network health: pull-only snapshot; connectionMode/NatClass/relay RTT are placeholders
  (NoConnection/Unknown/None); ping self-test on demand only, reports Direct unconditionally.

**Addressing can go stale for days:**
- ReachabilityAnnounce CRDT: publishes on start + network-change (2s debounce) + 60min idle;
  home-relay flap deliberately NOT a trigger (60min backstop). Durable CRDT entries never expire on
  read, only removed on Leave/Kick, and beat fresher pkarr records (should_replace source guard) —
  can pin a dead route until membership change or restart. Record carries node_id + home_relay_url +
  direct_addresses + butler_set (7-day TTL applied only on the pkarr side).
- pkarr: publish=first-success/resolve=first-hit across pool (q8.fyi → relay.pkarr.org → pubky);
  identity/community slots republish on epoch schedule (epoch_start+30min, then +3.5days) — NOT on
  address change (only Case-D friend slots ride the reachability tick); force_reachability_republish
  IPC wakes only the CRDT publisher. Signed valid_until (7d) enforced on read; positive cache 15min.
- Home relay: ZEB-521 fresh-read fix covers friend-accept signing + periodic publish; frozen
  SelfHandshakeReachability boot snapshot still exists for other consumers. DISCREPANCY TO VERIFY at
  design time: zenoh new_link reads record.home_relay_url fresh per dial, but DM tunnel contact
  production sites may feed home_relay_url=None (survey disagreement — check tunnel_manager.rs sites).

**Data plane (contrast — already self-heals):**
- Channel-log backfill: 1h floor + 10min jitter, persisted restart-aware (ZEB-584); retry 30s→600s cap;
  epoch re-arm incremental, presence-kick + floor full; RBSR 32-round cap → FullReconcile.
- Root-fetch drivers (mail-root, community-root): have the floor but NOT persisted, NOT presence-kicked
  → the parity gap.
- Relay pull driver: 7.5min interval + Notify wake.

## Primitives landing via PR #389 (ZEB-616, open, other session — treat as available)

- **Per-peer live-connection registry**: `IrohZenohLinkManager.zenoh_conns: Mutex<HashMap<EndpointId, Connection>>`
  with identity-guarded `should_evict_on_close` watcher eviction. Natural home for Phase 3 per-peer
  transport state (A2 un-claim signals, drop detection hooks). `STALE_CONN_CLOSE_TIMEOUT=2s`.
- **Zenoh dead-path detection now bounded ~4s**: `transport/link/tx/lease=4000` + `keep_alive=4`
  (probe/1s). Phase 3 liveness (area B) gets a zenoh-level dead-link floor for free; the
  `lease_and_keepalive_keys_are_valid` schema-pin test is the pattern for any further config keys.
- **Hermetic 2-endpoint same-zid reconnect test** (`zenoh_reconnect_closes_stale_connection`;
  two endpoints sharing one secret = socket rebind with stable node-id) — the seed pattern for the
  F2 scenario suite (drop/rebind/offline-return).
- Registry eviction on `conn.closed()` = a *drop event* observable per peer → candidate A1 trigger
  ("re-dial on drop") already has its detection point; what's missing is only the policy + dialer.

## Decision areas & open decisions

### A. Transport reconnection policy (core)
- A1. Re-dial triggers: drop-detect / presence-reappear / record-update / periodic-retry — which set?
- A2. DialedSet lifecycle: what un-claims (drop, leave, failure-with-retry-after)?
- A3. Who dials: extend ZEB-485 lower-NodeId single-dialer to zenoh path vs inbound-only reconnects
  (ZEB-616 assumes inbound; its swap_zenoh_conn helper is written to be reusable for outbound).
- A4. Retry ladder: cadence, jitter, terminal states, per-peer backoff persistence.
- A5. Hygiene: dial concurrency cap; fix lossy hint channel.
- Research gate: does zenoh 1.9 retry dropped endpoints itself (connect/retry/*)? Does connect_peer
  join that machinery? → zenoh-research.

### B. Liveness detection & signal unification
- B1. One per-peer liveness state machine (populate PeerHealth for real) vs layered signals?
- B2. Active probes (HARMONY_PING_V1 background loop) vs derive from presence beacons vs
  lease/keepalive only? Power-aware.
- B3. Presence auto-subscribe (ZEB-613 option 1/2/3) — recommend option 1 (auto-subscribe all joined
  at serve start + on join; handle per-session drop on relaunch).
- B4. Should transport epoch forget zids (allow same-zid flap to re-bump) or is presence-kick + A's
  re-dial sufficient?
- Research gate: iroh conn-type watcher / path events; zenoh transport up/down events.

### C. Rebinding
- C1. Self: iroh rebind/network-change handling — automatic in 0.98? Do we need explicit handling for
  sleep/wake? (research gate)
- C2. Peer record updates: emit re-dial hint on CHANGED record (relay/addrs delta), not just first-learn.
- C3. Fix durable-CRDT-never-expires + beats-fresher-pkarr (looks like a straight bug; decide expiry
  window + precedence rule).
- C4. Home-relay flap: keep 60min backstop or make it a debounced trigger?
- C5. pkarr identity/community address-change republish (extend the Case-D pattern? force-republish
  fan-out to PkarrPublisher?).
- C6. Retire the frozen SelfHandshakeReachability boot snapshot (ZEB-521 completion).

### D. Offline-return
- D1. Root-driver parity: floor persistence + presence-kick for mail-root + community-root
  (follow-the-pattern; near-mechanical).
- D2. Boot reconciliation: persisted-peer static seeds staleness policy; dial ordering/parallelism;
  interaction with A's retry ladder.
- D3. Sub-minute target: presence-kick → (data reconcile ∥ transport re-dial); verify end-to-end
  budget vs 60s cooldown gate (is one shared EPOCH_REARM_COOLDOWN_MS still right?).

### E. Mobile posture (parked; constraints only)
- E1. Cadence ceilings chosen so a future mobile client isn't hostile (no <10s periodic wakeups added).
- E2. Desktop sleep/wake resume detection in scope for v1? (Likely yes — cheap, same machinery as C1.)

### F. Validation
- F1. ZEB-522 GCE node plan — prerequisite for cross-WAN proof (hairpin trap; ZEB-512/520 blockers
  since closed).
- F2. Extend ZEB-616 2-node reconnect repro into a Phase 3 scenario suite (drop/rebind/offline-return).
- F3. Logical-time (tokio paused) tests for every cadence policy; wall-clock budgets << regression max.
- F4. Fleet validation protocol (Koya/Ildwyn/AVALON) with true-separate-egress requirement.

### G. Protocol versioning/evolution (NEW)
- G1. ALPN version policy: currently harmony/zenoh/v1, tunnel/v1, handshake/v1, ping/v1 — bump rules,
  multi-version accept windows.
- G2. Capability negotiation on (re)connect vs pure ALPN-per-version.
- G3. Rolling-upgrade posture for the fleet (N, N-1 compatibility?); CRDT payload evolution already has
  serde/UNKNOWN-fields conventions — transport framing needs the equivalent.

### H. Relay/infra strategy (NEW)
- H1. iroh relay: n0 default (fleet observed on CANARY usw1-1.relay.n0.iroh-canary.iroh.link — why?
  research gate) vs pin prod vs revive self-hosted i.q8.fyi. Cost/latency/rate-limit (public relays
  4KiB/s steady-state per research report).
- H2. Relay selection/failover policy + asymmetric-relay-isolation risk (report Q2).
- H3. pkarr relay pool posture is settled (q8.fyi primary, ZEB-513) — only revisit if H1 changes topology.

## Area G — protocol versioning/evolution: blessed 2026-07-01

Current state: five per-protocol ALPNs suffixed /v1 (zenoh, tunnel, handshake, ping, friend-pex).
QUIC/TLS ALPN negotiation is server-picks-from-client-list, but iroh connect() takes ONE alpn per
attempt → cross-version fallback via ALPN = extra connect round-trips.

**Recommendation (hybrid, two mechanisms for two rates of change):**
- **ALPN version suffix = wire-incompatible generations only** (rare; bump mints /v2, acceptors
  register {v2, v1} during a deprecation window, dialers try newest then fall back on
  connect-failure).
- **Versioned hello/capabilities frame inside each protocol = feature evolution** (common; first
  frame carries {protocol_version: u16, capabilities: bitmap}; unknown capability bits ignored;
  enables feature-gating without new ALPNs or extra dials). Exemplar implementation on ONE protocol
  (tunnel or handshake) in the slice; others adopt as they next change.
- **Fleet compatibility policy: N / N-1** — a node supports the current and previous protocol
  generation; MIN_SUPPORTED constant; incompatibility surfaces loudly in network health (not a
  silent connect failure).
- **CRDT/payload rule codified**: additive-only fields, unknown-field tolerance (serde defaults) —
  already de-facto; write it down as a spec rule.

## Area H — relay/infra strategy: blessed 2026-07-01

- **v1: stay on n0 STABLE relays** (post canary-pin/Area 0). Fleet is ~4 nodes; free tier fine;
  self-hosting is ops burden without current need. Keep i.q8.fyi decommissioned.
- **Make the iroh relay list config-surfaced** (mirroring pkarr_settings relays: persisted,
  runtime-editable, validated) so topology changes become config, not code. This is the H slice's
  main code deliverable.
- **Deterministic-overlap insurance deferred**: if/when custom relays enter, adopt the pkarr.q8.fyi
  pattern (shared primary guarantees publisher/resolver — here dialer/acceptor — relay overlap;
  avoids the asymmetric-relay-isolation failure from the research report). Not needed while all
  nodes share the n0 stable map.
- **Self-hosted iroh-relay + community-hosted relay governance stays Phase 5+** (polycentric
  pattern; revisit with real scale or rate-limit pain; verify current n0 rate limits at that point).
- Relay *selection* policy: keep iroh default (lowest-latency net-report probe); 1.0's configurable
  path selection revisited in B if relay-path bias proves needed.

## Area E — mobile posture (constraints adopted, no decision needed)
- No new periodic wire traffic added by Phase 3 (B is passive fusion; pings stay on-demand).
- All new cadences ≥ existing 10s presence beacon; supervisor dormancy bounds retry energy.
- Desktop sleep/wake handled in C's pipeline. Push-wake/gateway explicitly deferred (research
  report Design B exists when its time comes).

## Area F — validation plan (execution structure, no decision needed)
- ZEB-522 GCE cross-WAN node **plan doc** = early Phase 3 deliverable (plan-only per ticket;
  ZEB-512/520 blockers noted there are since Done). True-separate-egress is REQUIRED for any
  cross-WAN claim (hairpin trap).
- Scenario suite grows from ZEB-616's hermetic 2-endpoint reconnect test: mid-session drop,
  same-zid rebind, offline-return, address-change re-dial, boot-seed reconciliation.
- Every cadence/ladder policy gets paused-time (tokio) tests; wall-clock budgets << regression max.
- Fleet validation measures D3's end-to-end recovery distribution (incl. deferred-kick tail) —
  evidence gate for any cooldown split.

## Slice decomposition (v2 draft — supersedes earlier sketch)
- **S0 — canary pin** (RelayMode::Custom → n0 stable). Tiny; immediate. [Area 0]
- **S1 — ZEB-613 auto-subscribe** (option 1 + re-subscribe on relaunch). Small; immediate. [B]
- **S2 — root-driver parity** (mail-root + community-root: persisted floor + presence-kick).
  Small-medium; immediate. [D1]
- **S3 — iroh 0.98.2 → 1.0.1 upgrade**. Large; early. Closes ZEB-615; re-validate ZEB-616
  post-upgrade; unblocks S6's path-watcher inputs. [Area 0]
- **S4 — reconnect supervisor core**: per-peer state machine replacing DialedSet; triggers = drop
  events (ZEB-616 registry + zenoh unstable listeners) + presence edges + changed-record hints;
  jittered ladder w/ dormancy; single-dialer rule; boot-seed migration + concurrency cap.
  Large. Needs PR #389 merged; zenoh `unstable` feature. [A + D2 + C-hint-emission]
- **S5 — address-change pipeline + record freshness**: watch_addr fusion, debounced fan-out (CRDT +
  pkarr republish + supervisor), freshest-wins precedence + staleness windows, boot-snapshot
  retirement, sleep/wake. Medium. [C]
- **S6 — liveness state machine + real PeerHealth**: fusion component, network-health fields live,
  seen-zid epoch gate replaced by state edges. Medium-large; after S3. [B]
- **S7 — versioning policy + hello-frame exemplar**. Small-medium; anytime. [G]
- **S8 — iroh relay list config-surfacing**. Small; anytime. [H]
- **S9 — GCE cross-WAN node plan doc** (ZEB-522). Doc-only; early, parallel. [F]
- Final: fleet validation pass across S4-S6 (D3 budget measurement). [F]
- Ordering: S0-S2, S7-S9 free; S3 → S6; #389 → S4; S4 ↔ S5 loosely coupled (hint emission in S4,
  consumption of freshness in S5).

## Research findings — zenoh 1.9.0 (delivered 2026-07-01; source-verified against cargo cache)

1. **Zenoh will NOT re-dial harmony's iroh peers. Phase 3 owns the reconnect loop.** The 1.9.0
   auto-reconnect machinery (closed_link/closed_session → peers_connector_retry, backoff
   1s→4s ×2, peer-mode retries forever) is gated on `session.endpoints`, populated ONLY by the
   config `connect/endpoints` path. `Runtime::connect_peer` (harmony's dial path) never joins it —
   strictly one-shot. No public API to add retry-set entries at runtime.
   - Alternative shape (b): inject iroh locators into config connect endpoints to ride zenoh's
     retry. Mechanically works through the fork's new_link, but couples timing to zenoh's backoff
     and requires resolver freshness at each attempt. Default recommendation: own the loop (a).
2. **Clean drop-detection exists, one feature-flag away**: `session.info().transport_events_listener()`
   / `link_events_listener()` stream Put(opened)/Delete(closed) per peer zid, with optional history
   replay. Gated `unstable`; harmony compiles `features=["internal"]` which does NOT include it —
   adding `"unstable"` is a one-line enabler. `peers_zid()` (stable, already used) = point-in-time
   direct transports (still lists a mid-lease-expiry link); pair snapshot + listener for edges.
3. **Lease semantics confirmed**: lease is announced to the peer; local keepalive interval =
   lease/keep_alive (defaults 10000/4 → 2.5s; ZEB-616's 4000/4 → 1s sends, ~4s dead-detect).
   KeepAlive rides the link so an idle iroh conn stays warm — lease fires only on true silence.
   Expiry chain ends in TransportPeerEventHandler::closed → RuntimeSession::closed_session.
4. **"Remapping unsupported" is an upstream design limitation** (tracing::error only, non-fatal;
   no fix/knob found in/after 1.9.0 — UNVERIFIED exhaustively). ZEB-616's close-old-first registry
   is the right mitigation; any Phase 3 outbound re-dial must route through that registry so stale
   faces close before new declarations.
5. **Fork re-dial dependency**: `new_link` resolves the peer via ReachabilityResolver at dial time —
   re-dial correctness depends on record freshness → couples area A to area C.
6. Handshake knobs: open_timeout/accept_timeout 10s, accept_pending 100, max_sessions 1000,
   max_links 1. Post-1.9 horizon: 1.9 is current; no evidence later releases change connect_peer
   retry or face-remap (UNVERIFIED).

## Research findings — iroh 0.98.2 (delivered 2026-07-01; docs.rs + GitHub verified)

1. **iroh 1.0.x upgrade = highest-leverage single move** (1.0.0 shipped 2026-06-15; 1.0.1 latest).
   Gets: MaxPathIdReached recovery (#4271/#4272/#4267/#4268/#4284, landed 1.0.0-rc.1), configurable
   path selection (#4232/#4233 — bias toward/away from relay), off-canary stable relay defaults
   (#4341), abandon-worse-RTT paths (#4296). Cost: real breaking churn — path-observation API
   redesign (#4188), FourTuple, relay-config changes.
2. **ZEB-615 ROOT-CAUSED**: `MaxPathIdReached` = noq per-connection path budget (12) exhaustion;
   issue #4124 (filed by Jake vs 0.97) — open_path takes NO corrective action on it in ≤0.98.2, so
   path events re-fire the WARN forever (261k warnings/45min case). Fix confirmed only at 1.0.0-rc.1.
   Pinned 0.98.2 IS exposed; no workaround except capping path churn ourselves or upgrading.
3. **Canary mystery solved**: 0.98.2's default_relay_map()/presets::N0 is HARD-CODED to n0's canary
   cluster (use1/usw1/euc1/aps1 .relay.n0.iroh-canary.iroh.link) — no SLA. 1.0 defaults move to
   stable *.relay.iroh.link. Can pin off canary on 0.98.2 today via RelayMode::Custom / insert_relay.
4. **Path observability exists (0.98 shape)**: Connection::paths() → PathWatcher (live), PathInfo
   {is_ip/is_relay/is_selected/is_closed/rtt()/stats()} — per-path RTT INCLUDING relay paths (our
   "no relay-RTT API" claim is right only for standalone/home-relay RTT with no active conn).
   Old conn_type()/latency() removed in 0.96. NB: this exact API is what 1.0 redesigns (#4188) —
   building B's surface on 0.98's shape invites churn; another argument for upgrade-first.
5. **Rebind/network-change**: magicsock auto-detects on desktop; Endpoint::network_change() =
   best-effort hint (mainly Android). KNOWN pre-1.0 regression: holepunch often NOT re-triggered
   after network change → silent relay fallback until re-probe. watch_addr() Watcher = home-relay +
   direct-addr change stream (C4's missing trigger primitive). online() = relay-handshake gate.
6. **No connection pooling**: each connect() = new Connection (paths/holepunch shared per-remote
   underneath). App must dedup by (NodeId[,ALPN]) — validates A's single-dialer + registry design.
7. **QUIC idle-timeout**: overridable per-connection via ConnectOptions::with_transport_config;
   exact 0.98.2 default UNVERIFIED (quinn baseline ~30s). Connection::closed() fires after idle
   timeout on silent death (bounded, not instant; multipath keeps conn up while any path lives).
8. **Discovery republish**: changing user-data for address-lookup triggers republish (documented);
   auto-republish-on-address-change not a documented guarantee (PARTIAL) → C5 should not rely on it.
9. **Self-hosted iroh-relay (0.98.2)**: TOML config; HTTPS relay :443 + optional QUIC
   addr-discovery :7842 (TLS required); LetsEncrypt or manual certs; allow-all or HTTP bearer
   access control; metrics :9090. 1.0 adds auth tokens/AccessControl trait/multi-hostname LE.

## Approved decisions log
- **2026-07-01 Area 0 (iroh posture): canary pin now + 1.0.x upgrade early.** Micro-slice pins
  RelayMode::Custom to stable relays immediately (no upgrade needed); dedicated 0.98.2→1.0.1
  upgrade slice sequenced BEFORE area-B observability (B builds on the redesigned 1.0 path API
  once); Area A supervisor is iroh-API-light and may proceed in parallel; ZEB-615 closes with the
  upgrade slice (root cause = upstream #4124, documented on the ticket 2026-07-01).
- **2026-07-01 Area D: blessed 2026-07-01 (recommended option adopted while AFK, then blessed)** —
  D1 root-driver parity (mail-root + community-root get ZEB-584 persisted floor + #384
  presence-kick); D2 supervisor owns boot seeds (persisted peers seed supervisor as Disconnected,
  dialed via the ladder with C's staleness gate, prioritized by recency + shared-community count,
  global dial-concurrency cap; config connect/endpoints emptied — replaces zenoh's accidental
  forever-retry for boot peers with one bounded uniform policy); D3 keep shared 60s re-arm cooldown
  for v1, fleet validation (F) measures the deferred-kick tail before any per-trigger-class split.
- **2026-07-01 Area C: approved as proposed** — freshest-wins record precedence across
  CRDT/pkarr (per-source staleness windows; durable record >~24h → async pkarr re-resolve before
  re-dial attempts); unified self-address-change pipeline (if-watch + iroh watch_addr → debounce →
  fan-out: CRDT publish, pkarr identity/community republish [own it; iroh auto-republish not
  guaranteed], supervisor notify); home-relay flap = debounced publish trigger (60-min backstop
  demoted to backstop); resolver emits changed-record re-dial hints (delta = node_id | relay |
  direct-addr set); frozen SelfHandshakeReachability boot snapshot retired (ZEB-521 completion);
  desktop sleep/wake resume feeds the same pipeline.
- **2026-07-01 Area B: Approach 1 approved as proposed** — passive-fusion per-peer liveness state
  machine (Connected(Direct|Relay,rtt)/Degraded/Disconnected(since)/Dormant); inputs = zenoh
  transport listeners (enable `unstable`) + ZEB-616 registry + iroh 1.0 path watcher + presence
  edges + dial outcomes; zero new wire traffic; backend-side, feeds network-health-changed;
  consumers = A supervisor + real PeerHealth fields + backfill fast-path (seen-zid epoch gate
  replaced by Disconnected→Connected edges — fixes same-zid-flap re-arm); pings stay on-demand
  diagnostics; ZEB-613 resolved as option 1 (auto-subscribe all joined communities at serve start +
  on join, re-subscribe on relaunch).
- **2026-07-01 Area A: Approach 1 approved as proposed** — harmony-owned reconnect supervisor;
  triggers = drop events (ZEB-616 registry eviction + zenoh transport_events_listener Delete) +
  presence reappearance + record-update hints; jittered exponential ladder (1s base, ×2, cap ~5min),
  DORMANT after ~15min w/o trigger (never terminal); DialedSet replaced by per-peer state machine;
  outbound re-dials adopt ZEB-485 lower-NodeId single-dialer rule; all re-dials route through the
  ZEB-616 registry; zenoh `unstable` feature enabled for listeners; DM tunnel stays demand-driven.
