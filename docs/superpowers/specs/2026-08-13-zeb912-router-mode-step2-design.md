# ZEB-912 step 2: env-gated router-mode sessions + severed-pair proof — design

**Ticket:** ZEB-912 (R3) · **Basis:** spike findings `docs/research/2026-08-12-zeb912-r3-zenoh-multihop-spike.md` (main `187436ad`) · **Verified on:** main @ `187436ad`

## 1. Goal

Make it possible to run a community's zenoh sessions in **router mode** (the only
mode with linkstate multi-hop in zenoh 1.9.0, per the spike), gated behind an env
knob so production behavior is untouched, and **prove at the Harmony layer** that
a channel message crosses a severed pair through an intermediate member. Explicitly
NOT in scope: flipping any default, dial-policy changes, the 10–50 session scale
sounding (feeds R4), app-level merge-then-forward.

## 2. Design

### 2a. `HARMONY_ZENOH_MODE` knob (event_loop.rs config build, near the ZEB-809 block)

New pure helper + one insert:

```rust
/// ZEB-912: session mode for the zenoh runtime. Default (unset/empty) = "peer",
/// today's production mode. "router" opts a node into zenoh's router routing
/// hat — the only hat with linkstate multi-hop data routing in zenoh 1.9.0
/// (routing.peer.mode is a deprecated no-op; see the R3 spike doc). Any other
/// value logs a warning and falls back to "peer" — misconfiguration must fail
/// toward current behavior, not toward a novel topology.
pub(crate) fn zenoh_session_mode() -> &'static str {
    match std::env::var("HARMONY_ZENOH_MODE") {
        Ok(v) if v.trim() == "router" => "router",
        Ok(v) if v.trim().is_empty() => "peer",
        Ok(v) => {
            tracing::warn!(value = %v, "HARMONY_ZENOH_MODE: unrecognized value; using \"peer\"");
            "peer"
        }
        Err(_) => "peer",
    }
}
```

Config build inserts `mode` and pins `timestamping/enabled` to `false` in **both**
modes: the peer default is already false, so the pin is a no-op today, and it
prevents router mode's silent default flip to true (HLC-stamping every data
message — wire-visible; `zenoh-config-1.9.0/src/defaults.rs:139-147`). Insert
failures follow the existing knob pattern (log, continue).

### 2b. Mode-aware listen-endpoint merge (`iroh_zenoh_registration.rs`)

`merge_iroh_listen_endpoints(current_json, self_loc)` hardcodes the `"peer"` key
for zenoh's per-mode object form (`:184-189`). Signature gains `mode: &str`;
object form appends under `map.entry(mode)`. Call site passes
`zenoh_session_mode()`. Flat-array and fallback branches unchanged. Existing unit
tests updated; new test pins the `"router"` key path.

### 2c. `routers_zid` union (event_loop.rs, two sites)

Zenoh's session info partitions direct links by the REMOTE node's mode: an
all-router mesh reports links under `routers_zid()`, not `peers_zid()`
(probe-verified). The direct-peer set at `event_loop.rs:3843-3851` (eager init)
and `:4581-4590` (5s refresh) feeds hop-distance classification AND ZEB-622
transport up-edge detection (which re-arms root-fetch/backfill/mail latches) —
under router mode both would silently see an empty set. Fix: one helper,
used at both sites:

```rust
/// ZEB-912: all directly-linked zids, regardless of the REMOTE session's mode.
/// zenoh partitions session-info by remote whatami (peers_zid vs routers_zid);
/// hop-distance and up-edge detection care about "directly linked", not mode.
async fn direct_link_zids(session: &zenoh::Session) -> std::collections::HashSet<String> {
    let info = session.info();
    let mut set: std::collections::HashSet<String> =
        info.peers_zid().await.map(|z| z.to_string()).collect();
    set.extend(info.routers_zid().await.map(|z| z.to_string()));
    set
}
```

(`detect_up_edges` consumes `Vec` today — the refresh site collects the set into
the existing shapes; the helper is the single source.)

### 2d. Test-only link denylist: `HARMONY_TEST_ZENOH_DENYLIST`

Comma-separated 64-hex iroh node ids (lowercased, exact). Parsed once
(`OnceLock<HashSet<[u8; 32]>>`) in `iroh_dial_driver.rs`; unparseable entries log
a warning and are skipped. Production-inert when unset. Gates BOTH directions of
the zenoh-over-iroh link layer, so a one-sided config fully severs a pair (no
respawn dance in the e2e):

1. **Dial side** (`RuntimePeerDialer::dial`, `iroh_dial_driver.rs:84-105`): the
   locator is `iroh/<64-hex>`; parse the hex, on denylist hit log
   `ZEB-912 test denylist: refusing dial` and return `Err`. The supervisor
   ladders/Dormants normally — that noise is the simulation of an unreachable
   pair, not a bug.
2. **Accept side** (`zenoh_iroh_transport.rs`, immediately after the ALPN check
   and `conn.remote_id()` at `:616-617`, BEFORE the registry swap /
   `mark_supervisor_connected`): on hit log
   `ZEB-912 test denylist: rejecting inbound` and `conn.close(...)` + continue.

Both hit-paths log at `info!` — the e2e asserts the deny log's PRESENCE as
positive evidence the sever engaged (log-absence alone proves nothing).

Scope note: this gates only the zenoh link layer (ALPN `harmony/zenoh/v1`). The
invite first-contact and butler/DM ALPNs are untouched — the e2e routes joins so
the severed pair never needs them (§2f).

### 2e. `nodeId` on `GET /v1/status` (additive)

`StatusDto` (`api/mod.rs:66-74`) gains `node_id: Option<String>` (serde renames
to `nodeId`) — the running node's iroh node id, hex. Getter
`node_id_hex_for_status()` on NodeState mirrors `owner_id_hex_for_status()`
(`lib.rs:1817`), sourcing the running node's iroh endpoint; `None` when not
running. Needed by the e2e (denylist config requires A's node id before spawning
C) and independently useful for fleet ops, which today scrape logs for node ids.

### 2f. E2E `s14_router_mode_severed_pair_delivery` (feature `e2e`, manual suite)

Topology: A, B, C all spawned with `extra_env: HARMONY_ZENOH_MODE=router`; C
additionally gets `HARMONY_TEST_ZENOH_DENYLIST=<A's nodeId>` (read from A's
`/v1/status` after A's mint — spawn order makes the one-sided sever race-free:
C's denylist is set at spawn, and C's accept gate rejects A's dials from first
boot).

Flow (cribbing s9, `e2e_two_node.rs:2189-2309`): A mints + creates community +
invites B; B joins via `poll_join_iroh`; **B generates the invite for C** (never
A — keeps the severed pair off the invite first-contact path); C joins via B.
Assertions:

1. Roster converges to 3 joined on ALL nodes (C sees A only via B — this alone
   is CRDT-sync crossing the sever).
2. A creates a channel; it converges on C (`syncing: false`).
3. A posts; C receives (WS `channel-message-received` + read-back). C posts; A
   receives. Both cross the sever.
4. Sever evidence: C's stderr contains the accept-side deny log (and/or A's the
   dial-refusal after A learns C's record via B); `NodeHandle` grows a small
   log-grep helper if one doesn't exist.

Budgets follow s9 (join 240s, roster 120s, channel 90s, message 60s). Suite is
manually invoked (not in CI); the run's result is recorded on ZEB-912.

## 3. Declined / out of scope

1. Flipping the default mode (needs the R4 scale sounding first).
2. Any dial-policy/topology change — the dense mesh + healers stay as-is; router
   mode routes around holes.
3. Making the denylist a product feature (it is a test seam; name says TEST).
4. zenoh adminspace exposure, gossip re-enablement, `connect/listen`
   ModeDependent timeout tuning (enumerated in the spike; nothing consumes them
   with our empty/explicit values).

## 4. Tests

1. Unit: `zenoh_session_mode` — unset/empty→peer, `router`→router, junk→peer.
2. Unit: config-key validity for `mode` + `timestamping/enabled` (zeb616 pattern,
   fails loudly if a zenoh bump renames the schema).
3. Unit: `merge_iroh_listen_endpoints` — object form appends under `"router"`
   when mode=router; existing peer/flat/fallback tests updated for the new param.
4. Unit: denylist parse (valid, mixed-junk, empty/unset) + dial-gate refusal for
   a denylisted locator.
5. E2E s14 as §2f (run locally; not a CI gate).

## 5. Risks

1. Router-mode sessions flood linkstate + recompute trees on link churn — cost
   unmeasured beyond 4 sessions (accepted; that IS the R4 sounding, and the knob
   confines exposure to opted-in runs).
2. The deny seam adds a per-dial/per-accept hash lookup — `OnceLock` + empty-set
   fast path keeps production cost ~zero.
3. Mixed-mode fleets (some nodes flipped) don't broker — spike-proven equal to
   today's behavior, so the knob cannot regress delivery below status quo.
