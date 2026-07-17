# ZEB-702: fleet siblings as first-class sync peers — design

**Ticket:** ZEB-702 (High) — cert-only paired butler rejects ALL deposits because
`OwnerState.friend_graph` never replicates to community-less fleet siblings.
**Direction approved by Jake (2026-07-16):** same-owner sync channel (option 1 of the
ticket's three), realized on the EXISTING owner-scoped sync fabric — no new ALPN, no
wire-format change, no new crypto.

## Problem

A SAS-paired, cert-only butler (B2) fail-closed rejects every deposit
(`iroh_butler_acceptor.rs:88` — "sender is not authorized to deposit") because its
`OwnerState.friend_graph` is permanently empty. Measured live in the ZEB-689 D3
cross-WAN session: 300 s bilateral-alive, roster still `[]`; P's FleetNetDoc B2-row
stamp frozen at the pairing-seed HLC.

## Why the fabric already exists (recon findings, 2026-07-16)

- Owner-state syncs on zenoh key `harmony/owner/{addr}/state-root-v1`
  (`event_loop.rs:1507`); FleetNetDoc on `harmony/owner/{addr}/ds/fleet-net-v1`
  (`event_loop.rs:1921-1927`). Same key for every device of one owner — no
  community predicate in the key space.
- Payloads are ChaCha20Poly1305 under the fleet KeyTree (`fleet_sync.rs:582-628`);
  a cert-only butler already holds that KeyTree from ZEB-492 pairing
  (`pairing/persist.rs:112`, `owner_state_crypto.rs:224`). Decryption needs nothing new.
- Zenoh reaches WAN peers over iroh links (`zenoh_iroh_transport.rs`,
  `iroh_zenoh_registration.rs:52-79`) dialed by the reconnect supervisor; the default
  `tcp/[::]:0` LAN listener is preserved (`merge_iroh_listen_endpoints`), so LAN
  siblings can also link via multicast scouting.

Two precise gaps keep a community-less sibling dark:

1. **Boot-seed filter.** `seed_boot_peers_into_supervisor` →
   `boot_seed_node_ids_by_recency` enumerates via `durable_preferred()` =
   `durable.or(pkarr)` (`reachability_resolver.rs:196-198`) — the `fleet` slot
   (ZEB-510 `ReachabilitySource::FleetSibling`) is **excluded**, so a sibling known
   only from pairing is never re-dialed after a restart. (In-session seeds DO kick:
   the supervisor kick gate at `reachability_resolver.rs:440-449` evaluates the
   freshest view, which includes the fleet slot.)
2. **No re-offer on link-up.** Zenoh `put`s are fire-and-forget; the sync engines
   publish only on local dirty (debounced) or explicit `flush_now`
   (`fleet_sync.rs:393-540` — no periodic timer, inbound merge never re-publishes).
   A link that forms after the last publish carries nothing until the next local
   mutation. Boot flushes (mint/fleet-net pattern, `lib.rs:5038-5047`, `lib.rs:5733`)
   fire once, seconds before any sibling link exists — a startup race, observed as
   the D3 300 s roster stall.

## Design

### Component A — dial view includes the fleet slot (keystone)

Change the boot-seed enumeration in `iroh_zenoh_registration.rs`
(`boot_seed_node_ids_by_recency`) to a resolver view that includes fleet-slot-only
entries — a new `ReachabilityResolver` method (e.g. `list_dialable_peers()`)
returning the freshest entry per `(owner, node_id)` across durable/pkarr/fleet,
recency-ordered by `effective_announced_at_ms`, self-node excluded (existing
filter).

**Deliberately NOT widening `durable_preferred()`**: that view backs `resolve()` and
`list_active_peers()` whose callers (dial-by-owner, diagnostics, e2e barriers)
depend on durable/pkarr semantics. The new view is additive; existing callers are
untouched.

Result: a paired butler is re-dialed at every boot, forever, exactly like a
community peer. ZEB-510 already persists the sibling endpoint (fleet_net.cbor +
fleet_peer_seed.cbor) and re-seeds the resolver at boot (`lib.rs:5757-5800`); only
this filter drops it.

### Component B — republish owner datasets on transport up-edge

In `event_loop::run`, subscribe `transport_epoch_tx` (`watch::Sender<u64>`,
`event_loop.rs:1086`, bumped on every up-edge at `event_loop.rs:3830`; subscribe
pattern precedent: mail driver `event_loop.rs:3169`). On each epoch change, call
`notify_dirty()` on every owner-scoped dataset engine (owner-state, fleet-net,
dm-inbox, dm-outhold, owner-trust, fleet-keys, owner-quorum-req,
community-device-intro, mint, notes, relay-hold, relay-optin — the last two
added during T3: they are owner-scoped `ds/*` datasets the original
enumeration missed).

- `notify_dirty` (not `flush_now`): synchronous, non-blocking, no oneshot await in
  the event loop, and the engines' existing debounce coalesces bursts of up-edges.
  `publish_root_now` re-publishes the CURRENT root — a re-offer, byte-identical
  content, idempotent on receivers (LWW/HLC merge).
- Handles: the engine `Arc`s live in `start_node` (lib.rs); thread a
  `Vec<Arc<dyn ...>>`-style dirty-handle bundle (small trait, e.g. `RepublishDirty`,
  implemented by `FleetSyncEngine<T>` — enables unit-testing the listener with fake
  engines) into event_loop alongside the existing sync-handle structs.
- Cost bound: one debounced `put` per engine per up-edge burst; fleet scale is
  single-digit peers.

This closes the late-joiner hole for ALL owner datasets, both directions (each side
sees its own up-edge and re-offers), and independently of HOW the link formed
(supervisor-dialed iroh cross-WAN, or LAN multicast-scouted TCP co-located — the
latter explains D3's co-located stall, so B alone likely flips co-located roster
convergence; A is required for the true cross-WAN butler).

### Component C — acceptor unchanged

Once B2's `friend_graph` converges, the existing fail-closed authorization
(`iroh_butler_acceptor.rs:~235,~292`) admits deposits with correct revocation
semantics (unfriending propagates as roster state). Deposits arriving before first
convergence are benignly rejected; the sender's outbox retries (existing backoff) →
eventual success. No protocol or authorization change.

### Component D — reject observability (folded in per ticket finding)

The reject is DEBUG-only and wire-silent by design (`iroh_butler_acceptor.rs:1013`
— keep the no-oracle wire behavior). Add local-only visibility:

- Process-lifetime counters (`AtomicU64` struct mirroring the dial-outcome pattern,
  `network_health.rs:247-254`): deposits accepted / rejected-unauthorized /
  rejected-other, incremented at the acceptor decision sites.
- Surface in `network_health_snapshot` DTO (serde camelCase — e2e asserts read
  camelCase keys).
- Rate-limited local WARN (reuse the existing network_health rate-limiter seam,
  `network_health.rs:~3165` test precedent) when rejects recur — an always-rejecting
  butler must be distinguishable from transport failure at default log level.

## Security notes

- No wire-format change anywhere (zeb375/zeb376-class fixtures stay byte-identical).
- AEAD, roster semantics, fail-closed authorization, and the no-oracle reject all
  unchanged. The WARN/counters are local-only (no new wire detail).
- Re-publishing on up-edge sends only what the fleet KeyTree already protects, to a
  topic space only fleet-key holders can read.

## Testing

- **A:** resolver unit tests — fleet-only entry appears in the new view (and in
  boot-seed output), recency ordering, self-exclusion, durable/pkarr callers
  unaffected (`durable_preferred` untouched).
- **B:** listener unit tests with fake `RepublishDirty` engines + a real watch
  channel: epoch bump → all engines marked; no marks without a bump; burst
  coalescing left to engine debounce (assert call counts only). Tokio paused time
  where timing matters (wall-clock budget rule).
- **D:** counter increments per decision path; WARN rate-limiting (CountingEmitter
  precedent); snapshot DTO carries camelCase fields.
- **Gates:** `cargo fmt`, `clippy --locked --all-targets --features test-fixtures
  -D warnings`, `scripts/test-select --context task` per task, full
  `--workspace --all-targets` sweep at the end.
- **Live validation (post-merge):** `scripts/gce-xwan/run-tests.sh --mode open
  --test d3` — the standing ZEB-689 gate; HELD flips green when this works. The s7
  co-located HELD boundary stays as-is (do not weaken/harden in this PR).

## Out of scope

- ZEB-703 (dm_outbox restart durability) — separate ticket.
- ZEB-513 (cross-WAN sibling re-rendezvous via pkarr when the butler changes
  networks while P is offline) — this fix covers endpoints known from
  pairing/fleet-net.
- Any wire/protocol change; any butler-acceptor authorization change.
