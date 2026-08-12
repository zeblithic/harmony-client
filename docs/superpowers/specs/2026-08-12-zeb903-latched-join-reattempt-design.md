# ZEB-903: Reachability-driven re-attempt for latched-pending iroh joins — design

**Ticket:** [ZEB-903](https://linear.app/zeblith/issue/ZEB-903) (Part A only — Part B shipped via ZEB-906 / PR #655).
**Scope approved:** 2026-08-12. Option A1 (reachability-driven re-attempt), in-memory only. Option A2 (`EPOCH_REARM_COOLDOWN_MS` bypass) declined — see §5.

## 1. Problem and confirmed premises

When an iroh invite redeem fails post-write (admin unreachable / response lost), ZEB-899 now latches a pending Space (`joined` + `pending: true`) instead of falsely reporting `inviter_unreachable`. But convergence from that latched state is passive: the joiner waits for CRDT-sync gossip on the next session (session setup + sync cadence + the 60 s root-fetch epoch re-arm cooldown + a publish cycle for the ZEB-906 ingest re-drive) — minutes, versus the ~10 s direct handshake when the admin is reachable.

All the pieces for an active fast path already exist on main (`920631b3`):

1. **The retry primitive is test-pinned.** `zeb889_retry_reuses_mint_and_redeems_zombie_invite` (ZEB-899): re-running the iroh redeem over a latched Space reuses the ZEB-889 cached mint, hits the host's AlreadyKnown-retransmit, and completes to `pending: false` in one round-trip, burning the invite and evicting the cache.
2. **The reachability signal exists.** `transport_epoch_tx` bumps on Zenoh peer up-edges (`event_loop.rs:4593-4595`); sessions ride iroh tunnels, so a recovering WAN peer produces an up-edge. Root-fetch / backfill / mail drivers already consume this watch.
3. **Losing the race is harmless.** If the countersign arrives via gossip first, `maybe_spawn_pending_clear_rescan_for_pending_join` (`community_state_sync.rs:2792`) clears `pending_join_at` independently; a background retry checks the Space and no-ops.
4. **Background-safety seams exist.** `connectivity_redeem_invite_iroh_inner` takes injected `progress_sink` / `nav_emit_sink` closures and a `fence_check` closure — a driver passes a no-op progress sink (no ghost dialog stages), the real nav sink, and its own fence.
5. **The gap:** nothing retains the invite URL after a latched redeem. The mint cache holds only the minted redemption keyed by payload digest; the URL dies with the dialog and is persisted nowhere (verified: no invite fields in `owner_state_types.rs` / `owner_state_crdt.rs`).

Post-ZEB-911, the fast handshake can complete via **any Joined member** (witness ladder), so an up-edge from any community peer — not just the admin — can complete the join.

## 2. Design

### 2.1 New module: `src/latched_join_reattempt.rs`

A `ReattemptContext` struct owning clones of every handle `connectivity_redeem_invite_iroh_inner` needs (the IPC impl already snapshots all of them as owned clones), plus the invite URL and community id. A driver task per latched community:

```text
loop:
  select { shutdown flip → exit; epoch bump → continue }
  cooldown_wait (deferred-not-dropped, 30 s, tokio::time — mirrors
                 channel_backfill::cooldown_wait)
  if Space missing OR pending_join_at is None → exit   (demand collapsed)
  run one attempt: connectivity_redeem_invite_iroh_inner(
      no-op progress sink, real nav sink, fence = shutdown-watch check)
  if outcome is joined + pending:false → exit          (converged)
  else loop                                            (wait for next up-edge)
```

`REATTEMPT_COOLDOWN_MS = 30_000`. The cooldown uses `tokio::time::Instant` (paused-clock testable; no wall-clock reads — per the wall-clock-budget rule). No attempt fires at spawn time: the latch was committed seconds after a failed handshake; the driver waits for a fresh reachability signal.

### 2.2 Lifecycle: registry-hosted, mirroring `root_fetch_shutdowns`

`CommunitySyncRegistry` gains `latched_reattempt_shutdowns: Mutex<HashMap<SpaceId, watch::Sender<bool>>>` with the same lock discipline as `root_fetch_shutdowns` (never held with the engines lock; `watch::Sender::send` is sync):

- `register_latched_reattempt(community_id) → watch::Receiver<bool>` — **latest-wins**: an existing entry's sender is flipped (old driver exits gracefully) and replaced. Covers the user re-pasting a *fresh* invite URL for the same community while an old driver is parked on a revoked one.
- `unregister_latched_reattempt(community_id)` — driver self-removal on exit.
- `stop_engine` and `shutdown_all` flip + remove the entry alongside the root-fetch shutdown, so leaving a community or node teardown collapses the driver.

**Why no NodeState capture / generation fence:** the driver's lifecycle IS the registry's lifecycle. Node restart runs `shutdown_all` on the old registry, which flips every driver's shutdown watch — the stale-handle window the generation fence protects against cannot outlive the registry that hosts the driver. The fence passed into the inner checks the driver's own shutdown watch, so a teardown that races an in-flight attempt suppresses the commit exactly like a generation trip. This deliberately avoids adding a new static-closure NodeState seam.

### 2.3 Spawn site: the IPC impl, gated on the latched outcome

`connectivity_redeem_invite_iroh_impl` (lib.rs) clones the context bundle **before** the inner call (the inner consumes its arguments), and after `Ok(outcome)`:

```text
if outcome.status == "joined" && outcome.pending && transport_epoch_rx is Some:
    spawn_reattempt_driver(ctx, epoch_rx clone)
```

- Spawning from the impl (not the inner) keeps the inner's signature and every existing test untouched, and keeps the driver production-wired (env dial config, real NodeEventSink).
- `transport_epoch_rx = None` (some tests / degraded boot) → no driver; the passive paths still converge.
- The headless `serve` RPC shares this impl, so agents get the same behavior.
- The LAN/Zenoh `redeem_invite` path is out of scope (§5).

### 2.4 Attempt semantics

Each attempt is the full `connectivity_redeem_invite_iroh_inner` — resolve, witness ladder, dial, handshake — with the cached mint making it idempotent (same `bootstrap_join.id`; host AlreadyKnown-retransmits; P6 already-engaged cannot trigger). Possible outcomes per attempt:

- `joined` + `pending: false` — converged; the nav sink fires (sidebar updates); the existing completion logic evicts the mint cache and burns the invite host-side. Driver exits and unregisters.
- `joined` + `pending: true` — another post-write failure; the latch re-commit is idempotent. Driver keeps waiting.
- `inviter_unreachable` / `no_member_reachable` / `Err` — pre-write failure or degrade; nothing committed. Driver keeps waiting.

Concurrent manual redeem (user re-pastes the URL) is safe: the mint cache makes both attempts carry the same event id; CRDT inserts are idempotent; the pending-clear rescan reconciles the Space either way.

## 3. Unchanged surface

No frontend, RPC/IPC, wire, or acceptor changes. No new persistence. `RedemptionOutcome` semantics unchanged. The inner's signature unchanged. `EPOCH_REARM_COOLDOWN_MS` and the root-fetch driver unchanged.

## 4. Tests

Integration (in `pkarr_net/pkarr_iroh_redeem_full_integration.rs`, reusing the two-party harness):

- **T1 — happy path:** seed a latched-pending Space (inner-with-overrides, as the ZEB-899 retry test does), spawn the driver against the live acceptor, bump the epoch watch → poll until the Space's `pending_join_at` clears; assert invite burned, mint evicted, registry entry removed.
- **T2 — demand collapsed:** complete the join first, then spawn the driver with an attempt-poisoned context (`iroh_endpoint: None`) and bump the epoch → driver exits without attempting (an attempt would observably fail), entry removed.
- **T3 — shutdown:** seed latch, spawn driver, `shutdown_all` → driver exits, entry removed.

Unit (module-local):

- **U1 — cooldown:** paused clock; two triggers inside the window defer (not drop) to the boundary; shutdown during the wait aborts.
- **U2 — latest-wins registration:** second `register_latched_reattempt` flips the first receiver.

## 5. Out of scope / declined

- **A2 (cooldown bypass):** with the driver, an up-edge triggers the one-round-trip handshake directly, strictly dominating the root-fetch path it would have sped up; carving a cooldown exception adds query-storm risk on flapping links. Declined.
- **Persisting invite URLs:** bearer-secret material; restart convergence is already covered (ZEB-906 ingest re-drive + boot healing, gossip). The registry is process-lifetime by design, mirroring the mint cache.
- **LAN/Zenoh `redeem_invite` path:** its latch (ZEB-501) converges via always-on LAN gossip; no evidence of a latency problem there.
