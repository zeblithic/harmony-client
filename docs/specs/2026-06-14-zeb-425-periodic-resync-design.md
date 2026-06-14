# ZEB-425 — In-session no-holder healing: periodic watermark re-sync floor

> Parent: ZEB-418 SP2 (Butler). Follows the P3a channel-backfill spec
> (`2026-06-10-zeb-418-sp2-p3a-channel-backfill-design.md` §6/§9) and the
> ZEB-434 transport-recovery hook (D6/D7 + Task 7).

## 1. Goal

Guarantee that a long-lived device eventually re-queries channel history
(and community/mail state roots) even when no transport-epoch bump ever
fires — closing the residual no-holder gap that ZEB-434's edge-triggered
re-arm does not cover.

## 2. Background — what already heals, and what does not

A backfill/root-fetch query issued while **zero holders are online**
completes as a clean empty reply stream (zenoh resolves a GET against
currently-matched queryables), so the latch **satisfies** instead of
retrying (P3a §6 amendment). Healing today happens via:

- **Engine start / restart** — the spawn-time latch re-reads the local
  watermark and re-requests. (P3a)
- **Transport-recovery hook (ZEB-434 D6 + Task 7)** — `event_loop.rs`
  bumps a `transport_epoch` watch on **any never-before-seen zenoh zid**
  (`merge_peers_detect_new` over `session.info().peers_zid()`), and the
  satisfied latch's driver re-arms (`latch.reset(current_watermark())`)
  on that bump. Covers "a new holder appears as a new **direct** peer."

### Residual gap (this ticket)

The epoch signal is **edge-triggered on a never-seen *direct* zid**, so a
holder can become serveable without ever bumping it:

1. **Router/relay-mediated holders** — reachable only through a router,
   so their zid never enters `peers_zid()` (direct session peers only).
2. **Late-matching queryable** — a peer whose zid was already seen
   (already counted, no bump) declares its channel-log queryable only
   later (still booting at first query time).
3. **Same-zid reconnect** — `transport_seen_zids` fires once per zid; a
   holder that drops and reconnects with the same zid bumps nothing.

In all three, a satisfied latch on an up-for-days device never re-arms
until the next restart → a history/state gap persists.

## 3. Decision

Implement **option 2** from the ticket — a low-frequency **periodic
watermark re-sync floor** — as an unconditional anti-entropy backstop
under the edge-triggered epoch re-arm, across **all three latch types**
(per the 2026-06-14 design call):

- channel-backfill (`run_backfill_driver`), and
- community-root + mail-root (both share `run_root_fetch_driver`).

Rejected: option 1 (empty-marker reply) needs a new wire surface +
pinned fixtures — disproportionate. Option 3 (transport-recovery hook) is
already shipped (ZEB-434).

This is the minimal anti-entropy the P3a non-goals (§7) explicitly
allow at alpha scale ("watermark re-sync approximates true anti-entropy")
— not per-event completeness proofs.

## 4. Design

### 4.1 Mechanism

Both drivers already **park on satisfaction** (`Idle`) and re-arm on a
`transport_epoch` bump via a `tokio::select!`. Add a third arm: a
**periodic timer** that, on expiry, performs the same `latch.reset(…)`
re-arm the epoch path does, then loops (which re-issues the query).

- The timer lives **only in the `Idle` (satisfied) branch**. An
  unsatisfied latch is already retrying on its capped backoff (≤600 s),
  so it needs no floor.
- A re-armed latch with still-no-holders falls into the backoff
  `WaitUntil` loop (never straight back to `Idle`), so the periodic timer
  cannot storm — it only re-fires after the latch is satisfied again.
  No extra cooldown needed (the interval *is* the rate limit; unlike
  epoch bumps, which can flap and so keep `EPOCH_REARM_COOLDOWN_MS`).

### 4.2 Interval injection

Add `resync_interval_ms: Option<u64>` to both driver signatures,
positioned immediately before the `now_ms` closure:

- `None` → disabled, preserving the exact legacy contract (used by the
  existing driver unit tests, which isolate epoch/backoff/paging).
- `Some(ms)` → enabled; production passes `Some(PERIODIC_RESYNC_FLOOR_MS)`.

A new module helper mirrors the existing `epoch_bump` shape:

```rust
/// Fire after `interval_ms` when set; pend forever when `None`
/// (disabled) so the select! arm simply never wins.
async fn resync_tick(interval_ms: Option<u64>) {
    match interval_ms {
        Some(ms) => tokio::time::sleep(Duration::from_millis(ms)).await,
        None => std::future::pending().await,
    }
}
```

The `Idle` branch's early `return` (when there is nothing to wait on)
becomes: `if epoch_rx.is_none() && resync_interval_ms.is_none() { return }`
— so a `None`/`None` caller still returns on `Idle` (legacy), but a
resync-enabled caller parks even without an epoch watch.

### 4.3 Production constant

```rust
/// Anti-entropy floor: re-arm a satisfied backfill/root-fetch latch at
/// most this long after it last (re-)synced, regardless of transport
/// epoch bumps (ZEB-425). 1 h — well above EPOCH_REARM_COOLDOWN_MS.
pub const PERIODIC_RESYNC_FLOOR_MS: u64 = 3_600_000;
```

### 4.4 Call sites

Three production spawns switch from `…, epoch_rx, now_ms` to
`…, epoch_rx, Some(PERIODIC_RESYNC_FLOOR_MS), now_ms`:

- `community_channel_log_engine.rs` (`run_backfill_driver`)
- `community_state_sync.rs` (`run_root_fetch_driver`, community root)
- `event_loop.rs` (`run_root_fetch_driver`, mail root)

Existing driver unit tests pass `None` (one inserted arg each).

## 5. Testing

Pure paused-time unit tests (existing precedent — injected `now_ms` +
`tokio::time` paused, no real sleeps):

1. **backfill periodic re-sync fires** — satisfied latch, `epoch_rx`
   never bumps; advance virtual time past the interval → driver issues a
   fresh `request_page` (asserted via a request counter).
2. **root-fetch periodic re-sync fires** — same, on `run_root_fetch_driver`.
3. **`None` disables it** — satisfied latch, `resync_interval_ms = None`,
   `epoch_rx = None` → driver returns on `Idle` (legacy), no extra request
   even after advancing time (guards against accidental always-on).
4. **no storm on still-no-holders** — re-sync fires, holder still absent
   (`NoReply`) → latch backs off (`WaitUntil`), does not re-fire the
   periodic arm until satisfied again.
5. **shutdown still prompt while parked with resync enabled** — flipping
   `shutdown_rx` ends the driver during the periodic wait.

No new wire fixtures (no wire change). No integration-level test (the
floor is a unit-level driver-loop concern; a 1 h virtual wait through the
full engine adds nothing over the injected-interval unit tests).

## 6. Non-goals

- Empty-marker reply protocol (option 1) — no wire change in this ticket.
- Per-event completeness proofs / true anti-entropy (P3a §7, unchanged).
- Making the interval runtime-configurable — a const is sufficient for
  alpha; it is already test-injectable via the `Option<u64>` param.

## 7. Disposition

Closes ZEB-425. One harmony-client PR; no cross-repo change. Plain-text
ticket references in the PR body (ZEB-418 cascade gets the usual
post-merge reopen since the epic has open siblings).
