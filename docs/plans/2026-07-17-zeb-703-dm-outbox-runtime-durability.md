# ZEB-703 — dm_outbox runtime durability (pending entries survive graceful restart)

**Ticket:** [ZEB-703](https://linear.app/zeblith/issue/ZEB-703) · **Branch:** `zeb-703-dm-outbox-restart-durability` off `main@d46b5a54`

## Root cause (confirmed by two independent code sweeps; details in ticket comment)

DM-outbox durability hinges on a **single save point** — the unconditional `persist_now`
inside the owner-state `SyncEngine::shutdown()` — because **no DM-outbox mutation ever
marks the engine dirty**:

| Site | Where | Dirty-marked? |
|---|---|---|
| ENQUEUE | `send_dm_impl` (lib.rs ~13209 → `DmOutbox::send_dm`, dm_outbox.rs:903) | ✗ (snapshot doesn't even capture `sync_engine`) |
| ACK | `mark_ack_delivered` (dm_outbox.rs:972) via `handle_cidnotify_lifted` / drain Phase C | ✗ |
| DELETE + ZEB-243 tombstone | `delete_outbox_entry` (lib.rs ~13715 → dm_outbox.rs:1042/1076) | ✗ |
| DRAIN status transitions | `drain_lifted` (event_loop.rs ~4058 → dm_outbox.rs Phase C) | ✗ (no engine handle in signature) |

There is **no periodic persist**; the debounced flush (250 ms, `DEFAULT_DEBOUNCE_MS`)
only arms via `notify_dirty()`. In a quiet session (stuck deposit-retry — exactly the
observed state) nothing writes `owner_state_crdt.cbor` until shutdown. The sole save
point is then raceable: `/v1/shutdown` acks **200 before** `stop_inner` runs
(api/mod.rs:156-170 vs lib.rs:23959), so any supervisor acting on the 200 (incl. our
own `curl … ; sleep 3; relaunch` harness recipe) can truncate the only persist. Flush
failure logs a warn only — matching "no error surfaced."

Two corrections to the ticket's hypotheses: the shutdown **persist** leg is
unconditional (only the *publish* leg is dirty-gated), and headless serve **does** run
the flush fence — it's the GUI quit path (`quit_app` = `app.exit(0)`) that runs no
flush at all (follow-up ticket, out of scope here).

**Bonus bug fixed for free:** ZEB-243 delete-tombstones are never *published* either
(no dirty-mark on delete) — a paired device can resurrect a deleted entry until an
unrelated flush fires.

## Fix design (one PR)

The codebase's documented discipline (ZEB-685 drain comment; working examples
`iroh_friend_acceptor.rs:1902`, `fleet_net.rs:1425`): local owner-state CRDT mutations
must call `notify_dirty()`. Apply it to all four sites, gated on *actual* mutation:

**F1 — IPC sites (lib.rs).** Capture `g.sync_engine.clone()`
(`Option<Arc<owner_state_sync::SyncEngine>>`, NodeState:853) in the handler snapshots:
- `send_dm_impl`: notify after the mutation block (mutation certain on Ok).
- `delete_outbox_entry`: notify only when the outcome reports a change (same signal
  that gates the `dm-deleted` IPC event; idempotent re-delete = no notify).

**F2 — engine-side async orchestrators (dm_outbox.rs / event_loop.rs).** Thread
`owner_sync: Option<Arc<SyncEngine>>` (friend-acceptor pattern) into:
- `drain_lifted`: notify once per tick iff Phase C actually mutated `state.outbox`
  (status transitions / delivered_to / expiry). Never notify on idle ticks — an
  unconditional notify would republish a byte-identical root every debounce forever.
- `handle_cidnotify_lifted`: notify iff `mark_ack_delivered` returned true (newly
  delivered).
The event loop receives the Arc clone at spawn (engine exists before spawn;
constructed lib.rs ~4878, handles built ~4908).

**F3 — `/v1/shutdown` pre-ack flush (api/mod.rs).** `shutdown_handler` reaches
NodeState via the existing `ctx.state.node_state()` (status_handler pattern): snapshot
the engine Arc through a small `pub(crate)` accessor, then `persist_now()` bounded by
a 5 s timeout **before** sending on `shutdown_tx` and responding 200. On
timeout/error: warn + proceed (the process is going down either way; `stop_inner`'s
unconditional persist remains the backstop). Double-persist is idempotent. Result:
entries enqueued before the call survive even a kill-on-200 supervisor.

## Tests (TDD — red first)

- **T-red (the repro):** real `SyncEngine` + tempdir (template:
  `marked_left_survives_fenced_flush_to_disk`, lib.rs:38578) + NodeState with the
  send-path handles; call `send_dm_impl`; poll `owner_state_crdt.cbor` (≤5 s budget vs
  250 ms debounce) for the minted entry **without any shutdown/fence call**. Fails on
  main; greens with F1.
- Delete: after persisted send, delete → poll disk for tombstone present + entry gone.
- Ack/drain: lifted-fn level with stub transport; assert disk (or dirty flag) advances
  only when a real mutation occurred, never on idle ticks.
- F3: handler-level — flush observed before the 200/shutdown-signal ordering.
- Full sweep: existing dm_outbox/owner_state_sync/fleet_sync suites must stay green
  (notify_dirty additions must not perturb replay/merge semantics — they don't touch
  state, only the flag+timer).

## Out of scope / follow-ups (to file)

1. GUI quit path runs **no** Rust-side shutdown flush at all (`app.exit(0)`, no
   `RunEvent` teardown) — separate ticket.
2. `drain_lifted` mutations aren't covered by the ZEB-234 fence permit → a drain-tick
   mutation can land after `persist_now`'s snapshot during shutdown (residual: one
   status transition, not an entry) — note/ticket.
3. Butler-deposit no-oracle reject visibility (ZEB-702 residue) — unrelated here.
