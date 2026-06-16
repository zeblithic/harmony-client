# ZEB-485 — Deterministic single-dialer for the PQ DM tunnel (design)

**Status:** approved 2026-06-16
**Ticket:** [ZEB-485](https://linear.app/zeblith/issue/ZEB-485) (under [ZEB-321](https://linear.app/zeblith/issue/ZEB-321) transport coalescence)
**Predecessors:** ZEB-473 (Move 1a tunnel carrier), ZEB-482 (Move 1b DmInvite carrier), ZEB-484 (Move 1c blob carrier)

## Problem

Co-located 1:1 DM delivery over the PQ `harmony-tunnel` fails ~deterministically
(`s2_dm_delivery_over_tunnel_hard_assert`, 5/5). The recipient's `ingest_dm_packet`
never runs; the outbound side logs `ZEB-473: outbound tunnel handshake failed
reason=read TunnelAccept: connection lost` ~64 ms after the friend link lands.

### Root cause (investigation, 2026-06-16)

The failure is a **race in a simultaneous bidirectional dial**, *not* a regression in
ZEB-482's `note_active` change. Evidence from the on-disk e2e debug logs:

- **A working run (`s2-hard-1781588996`, 05:50):** `note_active role=Initiator
  state=Dialing pending=1` then `register_inbound keep_new=false` on one node;
  `register_inbound keep_new=true` then **`tunnel DM rejected … reason=CAS fetch: no
  successful reply`** on the other. The tunnel established and *delivered* — the DM
  reached ingest and failed only at the blob fetch (the gap ZEB-484 since closed).
- **Failing runs (`s2-hard-1781600629` / `…857`, 09:04 / 09:08):** `outbound tunnel
  handshake failed reason=read TunnelAccept: connection lost` + `tunnel session closed`,
  no `note_active`, no ingest.

The `connection lost` error originates at `tunnel_task.rs::initiator_handshake`
(`read TunnelAccept`), strictly **upstream** of `note_active` — the only production code
ZEB-482 touched in the tunnel layer. Same scenario, opposite outcomes ⇒ a race, not a
logic regression. **The "ZEB-482 broke live DM" branch is ruled out.**

**Mechanism.** `send_dm` calls `spawn_dial` **unconditionally** on the first DM to an
unknown peer (`tunnel_manager.rs`, the `None` arm). The instant two friends' owners
both `send_dm` each other (exactly what ZEB-482's invite-on-friend triggers on *both*
sides), both peers dial simultaneously over their **persistent** iroh endpoints,
creating **two** QUIC connections between one NodeId pair. iroh maintains ~one
connection per remote NodeId, so the second collides; the app-level lower-NodeId dedup
*sometimes* converges cleanly (delivery works) and *sometimes* the teardown kills the
connection that should survive (`connection lost`, no delivery). ZEB-482's invite-on-
friend turned a rare race into a near-deterministic one.

## Goal

Make tunnel establishment deterministic by ensuring **exactly one** iroh connection is
ever created per NodeId pair: only one peer dials; the other accepts.

Non-goals: changing the PQ handshake, the deposit/durability rung, the content-serve
queryable, or anything in the (frozen, unused) harmony-core tunnel stack. Cross-WAN
first-contact reliability beyond this collision (ZEB-330 class) is out of scope.

## Design

### The rule (in `send_dm`, the no-session `None` arm)

Compare our own NodeId to the peer's (reusing the same ordering as the existing
`keep_new` lower-wins dedup):

- **`self_node_id < peer_node_id` (we are the lower NodeId):** we are the designated
  dialer → `spawn_dial(peer, contact, [packet])` immediately (today's behavior).
- **`self_node_id > peer_node_id` (we are the higher NodeId):** do **not** dial. Buffer
  the DM and wait to *accept* the lower peer's inbound dial. Arm a **fallback dial**:
  if no inbound tunnel has registered within `FALLBACK_DIAL_DELAY = 1s`, dial anyway.

NodeIds are `blake3` of the ML-DSA pubkey; equality is impossible for distinct peers, so
there is no tie case.

### New handle state: `AwaitingInbound`

The higher peer's buffer handle. It holds `pending` and has no live dial yet. It is
tagged `role = Initiator` (the role it *would* take if the fallback fires), so the
**existing `keep_new` dedup math is unchanged**:

- `send_dm` while `AwaitingInbound` → `push_pending` (same as the `Dialing` arm).
- When the lower peer's inbound dial arrives, `register_inbound` runs the existing
  collision path: `keep_new(new_initiator = peer = lower, existing_initiator = self =
  higher) = true` → keep the inbound (Responder) session, `drain_pending_into` redirects
  the buffered DMs onto it, close the `AwaitingInbound` handle.

### The fallback dial: `spawn_fallback_dial(peer, contact, seed_pending)`

1. Insert an `AwaitingInbound` handle (role `Initiator`, holding `seed_pending`,
   carrying a fresh `cmd_tx`/`cmd_rx`). Apply the same double-check as `spawn_dial`: if a
   session already exists for the peer (an inbound raced in), reroute the seeds onto it
   and return without arming a timer.
2. Spawn a task: `sleep(FALLBACK_DIAL_DELAY)`, then re-lock `sessions` and inspect the
   peer's handle:
   - **Still `AwaitingInbound`** (no inbound arrived) → promote it to a live dial: flip
     the handle to `Dialing` and `spawn(run_tunnel_initiator(... cmd_rx ...))`, reusing
     the held `cmd_rx` and the handle's `pending`. The lower peer is not dialing, so
     there is no second connection.
   - **Anything else** (Active/Responder via a landed inbound, or evicted) → no-op; drop
     the held `cmd_rx`.

### Data flow — all three cases converge to one connection

| Who sends | Lower peer A | Higher peer B | Result |
|---|---|---|---|
| Both (S2) | dials A→B now | buffers; B's 1s timer is cancelled when A's inbound lands (~tens of ms) → drains pending onto it | one conn A→B, both directions, **no added latency** |
| Only lower | dials A→B now | — | one conn A→B |
| Only higher | not dialing | 1s fallback fires → dials B→A (A isn't dialing ⇒ no collision) | one conn B→A, ≤1 s liveness |

### Defense-in-depth

The existing `keep_new` dedup, `note_active`, `register_inbound`, and
`drain_pending_into` stay exactly as they are. They cover the only residual overlap: the
lower peer's dial and the higher peer's fallback dial both being in flight within the 1 s
window (possible only when the lower peer's dial is slow). The lower-wins dedup converges
that to the same single survivor, and `apply_invite`/`apply_inbox` idempotency absorbs
any at-most-once duplicate — identical to the protection ZEB-482 already added.

**`note_active` invariant preserved.** The `unreachable!()` guard ("a Responder handle
won dedup against our own completing dial") still holds: only a *higher* peer ever enters
`AwaitingInbound`/fallback, and `keep_new` never lets a higher peer's connection win over
a lower peer's, so a foreign Responder never sits under a *lower* peer's key when its own
dial completes. A higher peer whose fallback dial loses to the lower peer's inbound hits
the existing `!keep_ours` early-return in `note_active`, not the `unreachable!()`.

### Error handling

A fallback dial that fails (handshake error/timeout) flows through the existing
`note_dial_failed` path: the `Dialing` handle is removed and its pending drops to the
always-deposit durability rung. No new failure modes; durability is unchanged.

## Scope / files

- `src-tauri/src/tunnel_manager.rs` — the `TunnelHandleState::AwaitingInbound` variant,
  the `send_dm` lower/higher gate, `spawn_fallback_dial`, the `FALLBACK_DIAL_DELAY`
  const, and `register_inbound`/`note_active` adjustments if any are needed beyond the
  role tagging (expected: none — the dedup math already covers `AwaitingInbound`).
- `e2e-harness/tests/e2e_two_node.rs` — un-ignore `s2_dm_delivery_over_tunnel_hard_assert`
  once it passes reliably.

No frontend, no harmony-core, no wire-format changes (purely a dial-timing policy; the
on-wire `TunnelInit`/`TunnelAccept`/DM frames are untouched).

## Testing

**Unit (tunnel_manager.rs), current-thread tokio with paused time where a timer is
involved):**

1. `send_dm` to an unknown **lower** peer → inserts a `Dialing` Initiator handle
   immediately (today's `send_dm_buffers_while_dialing` analog).
2. `send_dm` to an unknown **higher** peer → inserts an `AwaitingInbound` handle holding
   the packet; no initiator task is dialing yet.
3. Fallback fires when no inbound arrives → after `FALLBACK_DIAL_DELAY` the handle is
   `Dialing` (use `tokio::time::pause`/`advance`).
4. Fallback is a no-op when an inbound arrived first → after a `register_inbound` for the
   peer, advancing past the delay leaves the inbound Responder session intact (no second
   dial).
5. `register_inbound` drains an `AwaitingInbound` handle's pending onto the survivor
   (extends the existing `register_inbound_keeps_lower_initiator_on_collision`).

**Validation (the real proof) — S2 e2e:** run
`s2_dm_delivery_over_tunnel_hard_assert` (`--features e2e`) **5× before** the fix (expect
flaky/red with `connection lost`) and **5× after** (expect 5/5 green: recipient fires
`dm-received`, plaintext lands). Then un-ignore it. CI never runs the `e2e` feature, so
un-ignoring is CI-safe.

## DoD

`s2_dm_delivery_over_tunnel_hard_assert` passes reliably under `--features e2e`
(un-ignored): two co-located peers friend → DM → recipient fires `dm-received` +
plaintext lands, with the PQ tunnel establishing deterministically (one connection, no
`connection lost` on the surviving path).
