# ZEB-616 — Deterministic stale zenoh-face teardown on same-zid iroh reconnect

**Status:** design approved 2026-07-02
**Ticket:** [ZEB-616](https://linear.app/zeblith/issue/ZEB-616) (narrow slice of [ZEB-321](https://linear.app/zeblith/issue/ZEB-321) Phase 3)
**Scope:** the stale-face-teardown slice only. The broader ZEB-321 Phase 3 liveness protocol (heartbeat cadence, address rebinding, offline-rediscovery after days, mobile push-wake, backoff policy) is explicitly **out of scope** and parked for a later pass.

## Goal

When an iroh transport to a peer drops mid-session and the peer reconnects, its fresh zenoh declarations must install cleanly instead of colliding with the stale face left by the dead connection. One sentence: **a reconnect must not be poisoned by the leftover face of the connection it replaces.**

## Background — the confirmed mechanism

Harmony assigns each node a **deterministic, stable zenoh zid** derived from its iroh node-id ([ZEB-390](https://linear.app/zeblith/issue/ZEB-390), `event_loop.rs:944-958`), so the dial driver can compute a peer's zid ahead of the handshake and `connect_peer`'s post-handshake transport lookup matches. A consequence: a peer that reconnects — after a network blip, NAT rebind, or relay-path change — comes back with the **same zid**, not a fresh one.

On a mid-session drop, teardown of the surviving peer's stale zenoh face relies entirely on upstream zenoh's default transport-lease timer. Harmony sets **no** transport lease/keepalive config (the entire `Config` block is `event_loop.rs:925-1023`: only `id`, `connect/endpoints`, `listen/endpoints`, and optional multicast-disable) and **no** iroh idle-timeout/keepalive (`iroh_endpoint.rs:125-142`, `presets::N0`, no custom `TransportConfig`). A silently-dead path (no pending I/O to error) therefore keeps its face alive until the lease expires.

The inbound accept loop admits a reconnect's fresh link **without first closing the old connection** for that peer. The zenoh-ALPN branch (`zenoh_iroh_transport.rs:399-421`) builds a new `IrohZenohLink` per inbound connection and sends it straight into zenoh's transport stack via `new_link_tx`, with no lookup of any prior connection for the same `peer_id`. When the reconnect's handshake completes and zenoh discovers the (identical, stable) zid, the new session's resource re-declarations collide with the lingering face's resource-id mappings, and upstream zenoh emits a burst of:

```text
zenoh::net::routing::dispatcher::resource: north/<peer>:N Resource K remapped. Remapping unsupported!
```

rejecting the reconnecting peer's fresh declarations — including its channel-log **queryable**. Until the stale face finally times out, backfill GETs to that peer route to a dead face and return `replies=0`.

This was observed live during the ZEB-599 D3 fleet run: **84** consecutive `Remapping unsupported` errors, all against a single peer's face (`north/62be45e9`), bursting on a reconnect. It is the real, code-grounded residue behind [ZEB-614](https://linear.app/zeblith/issue/ZEB-614) (whose broader "steady-state responder never answers" premise was separately refuted — steady-state full reconciles are answered; the failure is specifically the reconnect-window collision).

### Working reference already in-tree

The identical failure mode is already solved for the **clean-shutdown** path by [ZEB-468](https://linear.app/zeblith/issue/ZEB-468) (`event_loop.rs:5368-5391`). Because the session is built on an adopted runtime, `session.close()` alone only sends a session face-close and leaks the transport; the fix explicitly calls `zenoh_runtime.close()` → `manager.close()` so the peer drops our stable-zid face, letting a restarted node re-declare cleanly. That comment names this exact symptom ("the peer keeps our old (identity-stable, ZEB-390) zid's face → re-declarations are rejected 'Remapping unsupported'"). This slice adds the **mid-session-drop** equivalent, scoped to a single peer rather than the whole runtime.

## Design

Three components. All new logic lives in the transport module we own (`zenoh_iroh_transport.rs`) plus a small config addition in `event_loop.rs`; no zenoh-internal per-peer API is required by the primary path.

### Component A — close-old-on-reconnect (the core fix)

`IrohZenohLinkManager` gains a per-peer **live-connection registry**:

```rust
/// ZEB-616: the live inbound zenoh-ALPN iroh Connection per peer. A
/// reconnect for a peer already present here closes the prior connection
/// before the new link is admitted, so the stale zenoh face is reaped
/// before the reconnect's same-zid declarations install (avoiding the
/// "Remapping unsupported" collision).
zenoh_conns: std::sync::Mutex<std::collections::HashMap<EndpointId, iroh::endpoint::Connection>>,
```

In the zenoh-ALPN branch of `spawn_accept_loop`, immediately after `let peer_id = conn.remote_id();` and before the new link is sent to `new_link_tx`:

1. **Swap in** the new `conn` under the registry lock, taking out any prior connection for `peer_id` (last-writer-wins; the lock serializes concurrent reconnects for the same peer).
2. If a prior connection came out, **close it** — `old.close(0u32.into(), b"zeb616-reconnect")` — and **await its closure** with a bounded timeout (`STALE_CONN_CLOSE_TIMEOUT`, ~2s) via `old.closed()`. Closing the old connection makes its `IrohZenohLink::read`/`write` error on the next poll, which drives upstream zenoh to tear the stale link/face down. Awaiting the close (bounded) sequences that teardown **before** the new link's declarations are admitted.
3. Admit the new link exactly as today (`new_link_tx.send_async(LinkUnicast(NewLink::Single(link)))`).

The extraction is a small helper on the manager so the close/await logic is unit-addressable and reusable:

```rust
/// Register `conn` as the live zenoh-ALPN connection for `peer_id`,
/// returning the prior connection (if any) to be closed. Pure map op;
/// the caller performs the async close + await.
fn swap_zenoh_conn(&self, peer_id: EndpointId, conn: Connection) -> Option<Connection>;
```

**Ordering risk and fallback.** Closing the old iroh connection triggers zenoh's teardown *asynchronously* (via the link read-error). Awaiting `old.closed()` guarantees the iroh connection is gone, but upstream zenoh reaps the face a beat later when its rx task next polls the errored link. The local repro (see Testing) is the gate that confirms this ordering reliably beats the new declarations. **If** the repro shows the read-error path is too slow to win the race, the fallback is a zenoh-native per-peer transport close: plumb the adopted `Runtime` into the manager (a `OnceLock<Runtime>` installed post-`open_session_with_runtime`, mirroring the existing late-install `handshake_dispatcher` pattern) and call `runtime.manager().get_transport_unicast(&zid).await?.close().await` for the peer's deterministic zid before admitting the new link. The primary path avoids this to keep the fix inside APIs we already exercise (`Connection::close`), but the fallback is named here so the plan can reach for it without a redesign.

### Component B — drop housekeeping

For each admitted zenoh-ALPN connection, spawn a lightweight watcher:

```rust
// ZEB-616: evict the registry entry when this connection finally closes,
// so the map can't leak and a peer that drops without immediately
// returning still has its face reaped promptly (the useful half of a
// drop watcher, for free).
tokio::spawn(async move {
    conn_for_watch.closed().await;
    let mut map = mgr_for_watch.zenoh_conns.lock().unwrap();
    // Identity-guarded: only remove if the stored connection is still
    // THIS one, so a watcher for a superseded connection can't evict the
    // live one that replaced it. `stable_id()` is iroh's per-connection id.
    if map.get(&peer_id).map(|c| c.stable_id()) == Some(conn_id) {
        map.remove(&peer_id);
    }
});
```

The exact "is this still the live connection?" comparison is settled in the plan against iroh's `Connection` identity surface (`stable_id()` or equivalent); the invariant is what matters — a stale watcher must never evict the connection that superseded it. This keeps the map bounded to live peers and gives us prompt face reaping for the drop-and-never-return case without a separate mechanism.

### Component C — keepalive/lease config (defense-in-depth)

Add to the zenoh `Config` block (`event_loop.rs:925-1023`), **gated on iroh being enabled** (`iroh_handles.is_some()`) so the tuning only applies on runs that actually use the iroh transport whose stale faces it targets:

```rust
// ZEB-616: bound how long a silently-dead path's face can linger when no
// reconnect arrives to trigger the accept-loop teardown (Component A).
// GATED on iroh: zenoh's `transport/link/tx` lease is transport-GLOBAL (not
// per-link-kind), so applying it only on iroh-enabled runs keeps the blast
// radius off pure-non-iroh runs. keep_alive probes detect a dead path; a
// shorter lease reaps its face sooner. In zenoh 1.9.0 `keep_alive` is the
// number of keep-alive probes per lease (probe interval = lease /
// keep_alive): lease 4000ms (down from the ~10s default) with keep_alive 4
// → a probe every 1s, face reaped within ~4s of a dead path.
if iroh_handles.is_some() {
    config.insert_json5("transport/link/tx/lease", "4000")?;
    config.insert_json5("transport/link/tx/keep_alive", "4")?;
}
```

The values are conservative — large enough not to false-positive a briefly-quiet healthy link, small enough to shrink the stale-face window — and are validated in the plan (a too-aggressive lease could reap a healthy-but-idle link). Within an iroh-enabled run the transport-global lease also covers any coexisting LAN/TCP links, which is benign (1s keepalive probes ≪ the 4s lease keep a healthy link alive) and consistent with faster mesh convergence. This does not eliminate the reconnect-before-expiry race — that is Component A's job — but it caps the worst case for a peer that drops and does not reconnect, and it makes the whole mesh converge faster after any partition.

## Data flow

```text
peer path drops  ──►  (silently-dead: no error until next I/O or keepalive)
                       Component C keepalive probe eventually errors the link ──► zenoh reaps face
peer reconnects  ──►  inbound QUIC accept (zenoh ALPN)
                       │  peer_id = conn.remote_id()
                       ▼
                 swap_zenoh_conn(peer_id, new_conn) ──► Some(old_conn)?
                       │                                   │ yes
                       │                                   ▼
                       │                          old_conn.close(); await old_conn.closed() (≤2s)
                       │                                   │  ──► old link read errors ──► zenoh reaps stale face
                       ▼                                   ▼
                 new_link_tx.send_async(new link) ──► zenoh handshake ──► same zid ──► clean declaration install
                       │
                       ▼
                 spawn conn.closed() watcher ──► on close, identity-guarded evict from registry
```

## Error handling / edge cases

- **Concurrent reconnects for one peer:** serialized by the registry `Mutex`; last writer wins and closes the prior connection. A brief overlap degrades at worst to today's behavior (a transient remap), never to a lost live connection.
- **`old.close()` fails or `old.closed()` times out:** log at debug and proceed to admit the new link. Best-effort — the worst case is the pre-fix behavior (stale face lingers until lease/keepalive reaps it), never worse.
- **First contact (no prior connection):** `swap_zenoh_conn` returns `None`; the path is identical to today plus one map insert.
- **Registry eviction races the next reconnect:** the identity-guarded `remove_if_same` ensures a stale watcher never evicts the connection that superseded it.
- **Outbound vs inbound:** reconnects are observed **inbound** (the peer re-dials us; outbound is dial-once per ZEB-373). The accept loop covers this. The `swap_zenoh_conn`/close helper is written to be reusable so a future outbound re-dial path (full ZEB-321 Phase 3) can call it without duplication.
- **Non-zenoh ALPNs** (`handshake/v1`, `ping/v1`, content, DM tunnel): untouched — the registry and teardown are scoped to the `HARMONY_ZENOH_V1` branch only.

## Testing — local 2-node reconnect repro (headless, no fleet)

A new integration test stands up two zenoh-over-iroh sessions on one host (A and B), establishes A↔B, and drives a same-zid reconnect:

1. Bring up A and B; confirm A can issue a queryable GET that B answers (baseline reachability).
2. Force-drop B's underlying iroh connection to A **while B keeps its stable identity/zid** (via the new registry seam / a direct `Connection::close`), simulating the silent mid-session drop.
3. Reconnect B to A (same zid).
4. Assert A can issue a queryable GET that B answers after the reconnect.

**Pre-fix expectation:** step 3 produces the `Remapping unsupported` collision and step 4's GET returns `replies=0` / B's queryable is unreachable. **Post-fix expectation:** clean declaration install, GET answered. The test becomes a permanent cross-transport regression guard.

**Honest caveat:** making the *pre-fix failure* deterministic may require injecting the reconnect-before-teardown race precisely (reconnect immediately after the raw drop, before any lease/keepalive reaps the face). If that proves flaky, the test asserts the **fix's contract** — a post-reconnect GET succeeds — rather than reproducing the exact upstream remap log line; the collision itself is already evidenced by the D3 capture. Unit coverage for `swap_zenoh_conn` (returns prior conn; last-writer-wins; identity-guarded eviction) backs the integration test regardless.

## Files touched

- `src/zenoh_iroh_transport.rs` — add `zenoh_conns` registry field + `swap_zenoh_conn` helper + close/await logic and the `conn.closed()` watcher in `spawn_accept_loop`'s zenoh-ALPN branch. (Fallback path, only if the repro forces it: `runtime: OnceLock<Runtime>` field + post-open installer + per-peer transport close.)
- `src/event_loop.rs` — add the `transport/link/tx/keep_alive` + `transport/link/tx/lease` config keys (Component C). (Fallback only: install the runtime handle into the manager after `open_session_with_runtime`.)
- `tests/` — new headless 2-node reconnect integration test; unit tests for `swap_zenoh_conn`.

## Out of scope

- The full ZEB-321 Phase 3 liveness/rebinding/reconnection protocol (heartbeat cadence, backoff, offline-rediscovery, mobile push-wake).
- Outbound re-dial after a transport drop (dial-once stays; ZEB-321 Phase 3).
- Changing the deterministic-zid scheme (ZEB-390 is load-bearing for dialing; the fix works *with* stable zids, not against them).
- ZEB-615 (iroh `MaxPathIdReached`) — a sibling transport-churn observation, tracked separately.
