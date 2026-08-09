# ZEB-758 — Per-source inbound tunnel admission (Sybil fairness) — Design

> **Status:** implemented + shipped (PR #649, merged). Design record committed post-hoc.
> **Ticket:** ZEB-758 (Low). Branch: `zeblith/zeb-758-harmony-client-per-sourceper-identity-inbound-tunnel`.
> **Builds on:** ZEB-757 (PR #545, the global inbound population `Semaphore(64)`).

## Problem

`IrohTunnelAcceptor` (`src-tauri/src/iroh_tunnel_acceptor.rs`) gates inbound PQ tunnel
connections with the ZEB-757 total-population semaphore: one lifetime-held permit per
admitted tunnel, capping the *total* live inbound population at
`MAX_INBOUND_TUNNEL_SESSIONS = 64`. That bounds host resource use (fd/memory/CPU), but
admission is **per-connection with no source identity**: the PQ `peer_node_id` is only
known *after* the handshake, and the acceptor has no contact allowlist. So a single
hostile network source that mints many distinct ephemeral PQ identities can occupy all
64 slots concurrently — a **service-degradation** DoS: honest peers are rejected while
the Sybil holds the slots. (Not a resource-exhaustion DoS — the population stays bounded,
and rejected peers' DMs still land via the always-on deposit rung.)

The gap the fix exploits: the **iroh transport identity** (`conn.remote_id()`, a `[u8;32]`
ed25519 endpoint key) is authenticated by the QUIC/TLS handshake, available *before* the
PQ tunnel handshake, and **does not change when the attacker rotates PQ identities**. It
is the same un-spoofable seam the friend/invite/pex acceptors already gate on
(`friend_intro.rs`, "un-spoofable, runs before any signature verification").

## Approach (defense-in-depth: rate shield + per-source concurrency cap)

Two composed per-source layers added to `InboundAdmission`, keyed on
`source = *conn.remote_id().as_bytes()`, evaluated **cheapest-first** in
`handle_connection` before the responder task spawns.

### Layer 1 — per-source rate shield (outermost, no permit held)

Reuse the audited `pub(crate) KeyedSlidingWindow<[u8;32]>` from `friend_intro.rs`
(bounded-eviction, ZEB-853). Sheds a *fast* per-source flood
(`> MAX_INBOUND_TUNNEL_HANDSHAKES` within `INBOUND_TUNNEL_RATE_WINDOW_MS`) **before any
semaphore acquisition or PQ-crypto work** → `conn.close(0, b"tunnel-rate-cap")`.

This is the *rate* axis: it bounds how fast one source may open tunnels, shedding a
connect/close/connect hammering flood cheaply.

### Layer 2 — per-source concurrency sub-cap + global cap (under one mutex)

The *population* axis — the layer that actually addresses "occupy all 64 slots":

1. Reject if the source already holds `PER_SOURCE_INBOUND_TUNNEL_MAX = 8` live permits
   → `conn.close(0, b"per-source-tunnel-cap")`.
2. Else `try_acquire_owned()` the global `Semaphore(MAX_INBOUND_TUNNEL_SESSIONS = 64)`;
   `None` → `conn.close(0, b"tunnel-population-cap")` (unchanged from ZEB-757).
3. Else increment the per-source count and hand back an RAII `InboundPermit`.

A source rotating PQ identities can now hold at most 8/64 slots, so saturating the host
requires **8 distinct iroh endpoint keys** — an 8× Sybil bar — while the total-population
bound still caps host resources.

### RAII permit — `InboundPermit`

```rust
pub struct InboundPermit {
    _global: OwnedSemaphorePermit,                 // frees the global slot on drop
    per_source: Arc<Mutex<HashMap<[u8; 32], u32>>>,
    source: [u8; 32],
}
impl Drop for InboundPermit {
    fn drop(&mut self) {
        let mut counts = lock_poison_tolerant(&self.per_source);
        if let Some(n) = counts.get_mut(&self.source) {
            *n -= 1;
            if *n == 0 { counts.remove(&self.source); } // keep the map self-bounded
        }
        // _global drops after this, releasing the global-population slot.
    }
}
```

Moved into the responder task, so **both slots free exactly when the tunnel dies**. The
per-source map is **self-bounded at ≤64 entries**: a source is present only while it holds
≥1 global permit, and total permits ≤ 64 — no eviction logic needed (unlike the rate
window, whose aged history must be evicted, which `KeyedSlidingWindow` already handles).

## Admission flow

```rust
fn try_admit(&self, source: [u8; 32], now_ms: u64) -> Result<InboundPermit, ShedReason> {
    // Layer 1: rate shield (records on admit; no permit held).
    if !self.rate.lock().admit(source, now_ms) {
        return Err(ShedReason::Rate);
    }
    // Layer 2: per-source concurrency cap + global cap, atomically under one lock.
    let mut counts = self.per_source.lock();
    if counts.get(&source).copied().unwrap_or(0) >= self.per_source_max {
        return Err(ShedReason::PerSource);
    }
    let global = Arc::clone(&self.sem)
        .try_acquire_owned()
        .map_err(|_| ShedReason::Population)?;
    *counts.entry(source).or_insert(0) += 1;
    Ok(InboundPermit { _global: global, per_source: Arc::clone(&self.per_source), source })
}
```

**Ordering (peek-then-commit; revised after CodeAnt review).** The rate shield *peeks*
(`KeyedSlidingWindow::would_admit`, non-recording) first, and the token is *committed*
(`admit`) only after the per-source and global capacity gates pass. So a connection shed
by the concurrency/population cap does **not** spend the source's rate budget. This
matters inside this fix's own threat model: a Sybil that saturates the global cap must not
also cause honest peers' retries to be rate-punished for tunnels they never got. The
peek→commit is not one atomic step, so N concurrent admits can overshoot the window by up
to N — acceptable for a soft DoS-hygiene bound (same posture as `open_join_admit`'s
ZEB-865 `would_admit`→`admit` composition). The rate shield still sheds a churn flood:
successful opens (capacity available) record tokens and trip the peek after the cap; a
flood during saturation is shed by the population cap anyway (cheap, no responder spawned).

*(An earlier draft recorded before the capacity gates; CodeAnt (Major) correctly flagged
that this drains an honest source's budget during saturation. The peek-then-commit above
is strictly better and reuses the existing `would_admit` primitive.)*

**Concurrency safety.** No two of `{rate, per_source}` locks are ever held at once (the
rate lock is released before the per_source lock is taken; `Drop` takes only per_source),
so there is no lock-order hazard, and nothing is held across `.await` (all admission is
synchronous). Locks are poison-tolerant (`unwrap_or_else(|p| p.into_inner())`), matching
the existing `IntroRateLimiter`/`InboundAdmission` posture — the guarded state is plain
counters, safe to keep using after an unrelated panic.

## Error handling / logging (no silent truncation)

`ShedReason { Rate, PerSource, Population }`; each maps to a distinct `close()` reason
string (above). The existing 30 s throttled warn (`REJECT_WARN_INTERVAL_MS`,
first-shed-logs-immediately, on the limiter's own monotonic clock) is extended to carry a
**per-reason breakdown** `{rate, per_source, population}` of sheds since the last warn.
Rationale: a targeted Sybil is legible (rate/per-source counts dominate) vs. benign
capacity pressure (population dominates), without handing a flood an unbounded
log-write amplifier. Every warn notes that rejected DMs still deliver via the deposit rung.

## Sizing (co-located consts, tunable)

| Const | Value | Rationale |
|---|---|---|
| `MAX_INBOUND_TUNNEL_SESSIONS` | 64 | unchanged (ZEB-757 global cap) |
| `PER_SOURCE_INBOUND_TUNNEL_MAX` | 8 | honest endpoint opens ~1 concurrent inbound tunnel (bidirectional, reused); 8 gives generous reconnect headroom, forces 8 distinct iroh keys to saturate |
| `MAX_INBOUND_TUNNEL_HANDSHAKES` | 30 | per-source handshakes per window; sheds a real hammering flood, never honest reconnects |
| `INBOUND_TUNNEL_RATE_WINDOW_MS` | 60_000 | 30/min/source |
| `REJECT_WARN_INTERVAL_MS` | 30_000 | unchanged (ZEB-757 throttle) |

## Testing

Full `handle_connection` needs a real iroh `Connection` (impractical to fabricate), so the
admission *decision* is the tested seam via a small-cap `InboundAdmission::with_caps(...)`
constructor (mirrors ZEB-757's testable slice; the `close()`+`return` glue is
review-verified). `now_ms` is an explicit parameter — the rate window is tested without
sleeping.

1. **Per-source concurrency cap:** one source admitted up to the per-source cap; next is
   `Err(PerSource)` *while the global cap still has room*; drop one held permit → the same
   source is re-admitted.
2. **Global cap:** distinct sources fill the global cap; next distinct source is
   `Err(Population)` *while its per-source count is 0*.
3. **RAII teardown:** dropping a permit decrements the per-source count *and* frees a
   global slot (a subsequent admission from a *different* source succeeds); after all
   permits drop, the per-source map is empty (self-bounded).
4. **Rate shield:** `MAX_INBOUND_TUNNEL_HANDSHAKES + 1` rapid same-source attempts at a
   fixed `now_ms` → the last is `Err(Rate)`; advancing `now_ms` past
   `INBOUND_TUNNEL_RATE_WINDOW_MS` re-admits.
5. **Throttled warn breakdown:** the extended `note_rejection` reports per-reason counts;
   first shed logs immediately, subsequent sheds inside the window fold into the next
   warn's counts (extends the existing ZEB-757 throttle test).

## Non-goals / rev bump

- No harmony rev bump — self-contained in the client, reuses the existing
  `KeyedSlidingWindow` primitive (matches ZEB-757's "no rev bump").
- Not an *authorization* decision (no allowlist / contact prioritization — that is the
  ticket's option 3, a larger friend-graph design, explicitly out of scope).
- Outbound (`iroh_tunnel_dm_transport.rs`) unchanged — contact-bounded and already
  concurrency-capped (ZEB-757 rationale holds).
