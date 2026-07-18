# ZEB-707: owner-dataset query-side pull — plan

**Ticket:** ZEB-707 (High). Live D3 (2026-07-17, post-ZEB-706) is green when P's live
push fires (Mode A) and red when it doesn't (Mode B): the butler B2 has no way to
**pull** P's owner-state root when it missed the push. Owner datasets are
acquisition-passive; community/mail have a query-side pull, owner datasets don't.

## Design — mirror the community pull path

The push path (publisher_tx → `session.put`; subscriber_rx ← subscription) is
symmetric across all dataset classes. The **pull path** exists for community
(server-side root queryable + receiver-side `run_root_fetch_driver`) and is absent
for owner datasets. Add it for the **owner-state dataset** (the D3-critical one,
carries `friend_graph`), building reusable fleet_sync infra.

Note (ticket correction): the "startup root query: no responder" log seen in D3 is
the **mail-only** cold-start query (`event_loop.rs:3245`); owner datasets emit no
root query at all today. Mirror **community** (`event_loop.rs:8936-9126`,
`community_state_sync.rs:2460-2518,4942-4966`), not mail (mail's server is an
external gateway).

### Component 1 — server side (fleet_sync engine answers a root query)

- Extract the "encode current root → wire bytes (+ `put_serveable` the content
  blob)" logic out of `publish_root_now` (`fleet_sync.rs:689-774`) into a shared
  helper so both publish and the new serve arm produce the identical wire a PUSH
  carries.
- Add an optional `root_serve_rx: mpsc::Receiver<oneshot::Sender<Result<Vec<u8>,_>>>`
  to `FleetSyncConfig`/`Ctx` and a `select!` arm in the engine loop
  (`fleet_sync.rs:562`) that encodes the current root on demand and replies the
  wire. Modeled on the community serve arm; consistent with the existing inline
  publish (which already awaits `put_serveable` in-loop). Absent channel → no arm
  (owner-state wires it; other datasets opt in later).

### Component 2 — receiver side (butler pulls the root)

- In the owner-state zenoh adapter, add: (a) a **queryable** task on the owner-state
  topic that forwards each query to the engine via `root_serve_tx` and
  `query.reply()`s the wire (mirror `event_loop.rs:9004-9054`); (b) a **GET-driver**
  task that issues `session.get(topic)` and pipes replies into the engine's inbound
  channel (mirror `rf_handle`, `event_loop.rs:9065-9126`); (c) a
  `run_root_fetch_driver` (`channel_backfill.rs:982`, reused verbatim) wired to the
  transport-epoch watch + presence kick + ZEB-425 restart-aware hourly floor
  (mirror `community_state_sync.rs:4942-4966`).

### Scope

Owner-state dataset only (fixes D3 Mode B). The fleet_sync serve infra is generic
and reusable; the other 11 owner datasets can adopt it as a follow-up (they are not
D3-blocking). Considered-and-rejected alternative: fix P's epoch-republish to fire
reliably on butler reconnect (smaller, but push is fundamentally timing-dependent —
the pull path is the robust, architecturally-consistent fix).

## Boot-safety

Queries arrive post-boot (a butler pulling), so the serve arm's `put_serveable`
cas-roundtrip is safe (event loop is up). Adapter tasks are spawned, never
inline-awaited in `start_node` ([[start_node inline-await hazard]]). The serve arm
must not stall the `flush_now` shutdown fence — snapshot state under brief lock,
same shape as `publish_root_now`.

## Testing

- Engine unit test: a `root_serve_rx` query returns the current root wire, decodes
  to the expected `FleetRootPublish`, and the content CID is `put_serveable`.
- Receiver: fetch-driver issues a GET on epoch bump / periodic floor (paused-time).
- Gates: fmt; clippy `--all-targets`; `scripts/test-select --context task`; full
  sweep pre-PR.
- **Live D3** (the determinism gate): run multiple times / at INFO timing (Mode B)
  and assert green — B2 pulls the root even when P's push doesn't fire.

## Out of scope

- The other 11 owner datasets (generalize later).
- Push-reliability fix (alternative, not needed once pull lands).
