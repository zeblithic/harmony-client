# ZEB-624 — User-Configurable iroh Transport Relays: Design

**Status:** blessed design (S8 of ZEB-321 Phase 3)
**Ticket:** [ZEB-624](https://linear.app/zeblith/issue/ZEB-624) (S8 of ZEB-321 Phase 3)
**Depends on / expands:** [`2026-07-02-zeb-321-phase3-decision-record.md`](2026-07-02-zeb-321-phase3-decision-record.md)
**Modules:** `src-tauri/src/connectivity_settings.rs` (persistence/validation, Task 4),
`src-tauri/src/iroh_endpoint.rs` (endpoint plumbing), `src-tauri/src/lib.rs` (IPC + boot)

The iroh endpoint chooses a *home relay* for NAT traversal and as a
last-resort tunnel path. Until now that choice was hard-wired to the
`presets::N0` default relay map (n0's stable production cluster). This slice
makes the relay list **persisted, per-node configuration** a user can edit at
runtime — the transport-side sibling of the ZEB-380 pkarr relay pool.

## 1. Decision

- **Config, not code.** The custom relay list lives in `iroh_relays` inside
  `connectivity-settings.json`, not in a recompiled constant. Operators steer a
  node onto private/regional relays without a new build.
- **Empty = follow the preset defaults (sentinel).** An empty `iroh_relays`
  list means "use the iroh preset's built-in relay map (n0 stable)", *not* "no
  relays". This keeps the first-run and post-`reset` experience identical to
  the pre-ZEB-624 default, and lets a fresh settings file (no key) upgrade
  silently. `reset_iroh_relays` persists `[]`; `set_iroh_relays` requires ≥1
  (the validator rejects an empty list) so a user can never *accidentally*
  strand the endpoint with zero relays.
- **Live diff-apply, no restart.** A configured list is applied at endpoint
  build (`RelayMode::custom`) and, for a running node, hot-swapped by diffing
  the target against the endpoint's current relay map and calling
  `insert_relay` / `remove_relay` for exactly the delta. The diff is
  `RelayUrl`-valued (canonical URL equality), so re-applying an unchanged set is
  a genuine no-op. A boot-time reconcile closes the race where an IPC persists a
  new list while the endpoint is still binding.
- **Five IPC verbs** mirror the pkarr relay verbs exactly:
  `get/set/add/remove/reset_iroh_relays`, each returning
  `IrohRelayWire { relays, custom }` and serialized by a dedicated
  `IROH_RELAY_WRITE_LOCK`. `add`/`remove` on a defaults-following node first
  materialize the preset defaults into a custom list, then mutate — so removing
  one unwanted default relay is possible, and `remove` refuses to empty a custom
  list (it tells the user to `reset` instead).

## 2. The deterministic-overlap pattern (operator guidance)

A relay only helps two peers if **both** of them can reach it: iroh's relay
path works when the dialer's and acceptor's relay sets *intersect*. If one node
narrows its `iroh_relays` to a single private relay the other node has never
heard of, the relay rung silently disappears and only direct/hole-punched paths
remain. This is the exact partition ZEB-513 hit on the pkarr side — two peers
each publishing to a *different* reachable relay and so never resolving each
other — and the reason `default_pkarr_relays()` leads with the self-hosted
`pkarr.q8.fyi` primary. The same discipline applies here: when a fleet moves
onto custom iroh relays, **share at least one primary relay across every node**
so the dialer/acceptor relay sets are guaranteed to overlap. Regional or
private relays are fine as *additions* layered on top of that shared primary,
not as mutually-exclusive replacements.

## 3. Non-goals (explicit)

- **No self-hosted iroh relay** ships in this slice. Configuring a private
  relay is supported; *operating* one is out of scope (a fleet can point at any
  reachable iroh-relay-compatible server).
- **No headless verbs.** The five commands are Tauri IPC only; the agent /
  headless RPC surface does not expose relay editing in this slice.
- **No relay governance.** Community- or fleet-level relay policy (who may set
  what, quorum on relay changes) is a Phase 5+ concern; ZEB-624 is per-node,
  operator-local configuration only.
