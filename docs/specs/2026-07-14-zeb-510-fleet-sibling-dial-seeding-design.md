# ZEB-510 — Same-owner fleet-sibling dial-seeding

**Status:** Design approved (Koya + Jake, 2026-07-14). Ready for implementation plan.
**Ticket:** [ZEB-510](https://linear.app/zeblith/issue/ZEB-510) (parent [ZEB-451](https://linear.app/zeblith/issue/ZEB-451); butler-rung counterpart of the already-fixed [ZEB-493](https://linear.app/zeblith/issue/ZEB-493)).

## Problem

An owner runs a fleet of devices under one identity. Device **P** publishes a durable **butler-set** (its online butler devices + their iroh endpoints) into the community CRDT so a sender **A** can deposit an async-DM for offline P by dialing a butler **B2**. Observed failure (e2e `s7_butler_deposit_recover`, cross-WAN D3): P's published butler-set contains only **P itself** (`[P-self]`), never B2 — so A dials P's own offline endpoint and the deposit never reaches B2 (`HELD` times out).

Root cause: **P never learns sibling B2's per-device iroh endpoint for dialing.** The endpoint data exists but is never wired to the dialer, and a circular bootstrap keeps it from converging.

## Root cause (re-confirmed on current `main` @ `20af02b3`)

- `FleetNetDoc.devices` rows (`fleet_net.rs:38-60`) already carry `iroh_endpoint_id: [u8;32]` (`"ep"`), `home_relay` (`"hr"`), `seen_at: Hlc` (`"sa"`), and are **durably persisted** to `fleet_net.cbor` (`fleet_net_persist.rs`, plaintext CBOR, atomic-rename + fsync). But those rows are consumed **only** to build the pkarr butler-set advert (`build_butler_set`) — they are **never fed into the `ReachabilityResolver`, the reconnect-supervisor, `connect/endpoints`, or a boot dial-seed** (`grep fleet_net.*resolver` = nothing).
- The dialer's targets come **exclusively** from the `ReachabilityResolver`, populated only by (a) the community-membership `ReachabilityAnnounce` projection (RCH5-gated to community members — `community_membership.rs:3765`) and (b) pkarr resolves of *other* owners. A same-owner sibling is in neither: it shares no community with P and its `owner_addr == self`, so nothing dials it.
- **Circular bootstrap:** `FleetNetDoc` only *gains* B2's row via B2's inbound fleet-net publish, which requires P↔B2 already to be peered on the `harmony/owner/{addr}/ds/fleet-net-v1` zenoh topic — but nothing seeds that peering for same-owner siblings. Endpoints ride on peering that is never established.

Nothing shipped since the June diagnosis (ZEB-373→620 supervisor refactor, ZEB-368 seed retirement, ZEB-637 self-owner display filter, ZEB-492 keytree distribution) closes this. ZEB-509 (the entangled co-located convergence question) is separately Done (#324).

## The reframe

The gap is **not** "where do we store endpoints" — `FleetNetDoc` already persists them durably. The gap is a single unbuilt wire: **`FleetNetDoc` sibling rows → `ReachabilityResolver`.** This collapses the store question for the steady state (no new store) and reduces the first cut to that wiring.

## Design decisions (approved)

- **(a) Scope:** lead with the FleetNetDoc→resolver wiring; defer the SAS first-contact seed to a validation-gated step 2; defer the cross-WAN pkarr rendezvous fallback (~ZEB-513).
- **(b) Store:** the steady state uses the **existing `FleetNetDoc`** (no new store). Rejected: **B1** extend the ZEB-492 keytree persistence (`fleet_keytree.enc` is an encrypted, `ZeroizeOnDrop`, golden-pinned secret written once at pairing; folding mutable non-secret endpoints in forces the event loop to rewrite a secret file and take the owner-state lock on every refresh — a sensitivity/write-cadence mismatch, ZEB-428 blast-radius class). Rejected: **B3** inject a foreign-authored B2 row into `FleetNetDoc` (rows are self-stamped-by-subject by convention; `merge_from` is pure LWW-by-`seen_at` with no author check, so P's injected row is stamped with P's `device_id` and is silently overwritten by B2's real self-row; the pairing site can't reach the runtime doc anyway). If a first-contact seed store is needed (step 2), it is **B2**: a new dedicated plaintext store using the `*_persist.rs` idiom.
- **(c) Resolver shape:** a new `ReachabilitySource::FleetSibling`.
- **Trust model:** `FleetSibling` resolver entries carry a **zero-filled `identity_signature` and are verification-exempt.** This is consistent with the resolver's architecture — the resolver **never verifies signatures** (`reachability_resolver.rs` has zero `verify` calls); each source verifies at its own *ingest boundary* (community projection checks RCH2; pkarr checks on resolve). A fleet row's ingest boundary is **fleet-net's symmetric-key decrypt** (`fleet_sync.rs`): only a device holding the owner's fleet KeyTree — i.e. a genuinely-enrolled sibling — can produce a decryptable row. So `FleetSibling` trades a per-record identity signature for channel-level fleet-membership auth.

## Architecture (step 1 — the first cut, no new store)

### 1. New resolver source

`reachability_resolver.rs`:
- Add `ReachabilitySource::FleetSibling` to the enum (`:84`); add its `as_dto_str` tag (e.g. `"fleetSibling"`, `:96`).
- Add a **distinct `fleet` slot** to `ResolverSlots` (`:135-139`, currently `durable`/`pkarr`) — do **not** reuse the `durable` slot. Rationale: the resolver key is `(OwnerAddr, iroh_node_id)`, and a sibling that is *also* a community co-member P shares would land its community `DurableCrdt` record under the **same** key `(self_owner, B2_node_id)`. Reusing the durable slot would let `FleetSibling` and `DurableCrdt` clobber each other, violating the per-source-slot invariant (`:120-134`) that dual-slot storage exists to protect. A separate `fleet` cell keeps each source isolated.
- Extend the two views: `freshest()` (`:147`, the **dial authority**) must include the `fleet` slot in the max-`effective_announced_at_ms` comparison, so a fleet entry is dial-able (and a fresher community/pkarr record still wins the route when present). `durable_preferred()` (`:163`, the **butler/diagnostics authority**) — fleet entries carry an empty `butler_set`, so they never affect butler authority; the plan decides whether `durable_preferred()` falls back to the fleet slot for the diagnostics view (informational only) or stays `durable ?? pkarr`.
- `update_with_source(actor, payload, hlc, FleetSibling)` (`:335`) writes the `fleet` slot and kicks the supervisor exactly as the other sources do — no new dial path.

### 2. `FleetNetRow → ReachabilityAnnouncePayload` mapper

A pure helper (home: `fleet_net.rs`, e.g. `fn sibling_reachability_payload(row: &FleetNetRow) -> ReachabilityAnnouncePayload`):

| `ReachabilityAnnouncePayload` field | Source |
|---|---|
| `iroh_node_id` (`"nd"`) | `row.iroh_endpoint_id` |
| `home_relay_url` (`"rl"`) | `row.home_relay` |
| `direct_addresses` (`"da"`) | `vec![]` (node_id-based dialing holepunches/relays; fleet rows carry no direct addrs) |
| `announced_at_ms` (`"ts"`) | `row.seen_at.wall_ms` |
| `identity_signature` (`"sg"`) | `[0u8; 64]` (verification-exempt — see trust model) |
| `butler_set` (`"bs"`) | `vec![]` (a sibling is a dial target, not advertising its own butlers here) |
| `bs_at` (`"ba"`) | `0` |

The resolver keys the entry `(self_owner, row.iroh_endpoint_id)` — distinct from P's own entry, so N siblings → N keys.

### 3. Boot-replay hook

`lib.rs` `start_node`, in the owner-loaded block right after `fleet_net.cbor` loads (`:5516`) and the self-row is stamped (`:5600-5667`): iterate `doc.devices`, **skip P's own `device_id`** (the self-row), and for each sibling row call `resolver.update_with_source(self_owner, sibling_reachability_payload(row), row.seen_at, FleetSibling)`. This mirrors the existing `ReachabilityAnnounce` boot-replay (`:7797-7918`, `resolver.update(...)` at `:7912`).

### 4. Live-merge hook

`event_loop.rs`, in the `fleet-net-v1` adapter that applies inbound merges (`:1921-1929`): **after** each `FleetNetDoc::merge_from`, fan the merged sibling rows (skip self) into the resolver via the same mapper, so a sibling that comes online / changes endpoint mid-session propagates to the dialer once the topic is peered. Use the merge result / changed-key set if available to avoid redundant re-feeds; otherwise re-feed all sibling rows (LWW makes re-feed idempotent).

### 5. Filters & invariants

- **RCH5 bypass is automatic:** we feed the resolver via the new source, not through the community projection, so the community-member gate (`community_membership.rs:3765`) never applies to siblings.
- **Self-owner display filter untouched:** `filter_peers_by_shared_membership` (`network_health.rs:567`, ZEB-637) is **display-only** (it shapes the Network Health snapshot, not the dial layer), so it neither blocks the fleet dial nor is regressed. The new source must **not** be routed through it.
- **Self-row exclusion:** never feed P's own `device_id` row (avoid P dialing itself).
- **No pkarr refresh for fleet entries:** the resolver's `maybe_refresh_stale` async re-resolve (ZEB-621) re-fetches a peer's **pkarr** blob; a same-owner sibling is not in pkarr (that's the deferred cross-WAN path). A stale `FleetSibling` entry should **not** trigger a pkarr re-resolve for `self_owner` (it would no-op or wrongly resolve P's own record). The plan must ensure the refresh path skips / is inert for `FleetSibling` entries.

## Validation

- **Primary (local acceptance test):** promote `s7_butler_deposit_recover` (`e2e-harness/tests/e2e_two_node.rs`) `HELD` from a soft "characterize" fallback to a **hard assert**, co-located, and confirm the full `HELD → RECV → CLEARED` chain runs. (`REACHABILITY` and Boundary-0b are already hard asserts.) If `RECV`/`CLEARED` still need the recover-half work, promote at least `HELD` and note the residual.
- **Unit/integration:** a deterministic test that a merged sibling `FleetNetRow` produces a `ReachabilityResolver` entry keyed `(self_owner, sibling_node_id)` with `source == FleetSibling` and a dial-able `freshest()` view; and that P's own row is excluded.
- **Full gates:** `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`, `cargo nextest run --locked --workspace --all-targets --features test-fixtures --no-fail-fast`.

### The step-1-vs-step-2 gate

Step 1 relies on P and B2 having converged fleet-net **at least once** (so P's `FleetNetDoc` holds B2's real self-authored row), then re-dialing B2 from the persisted row on subsequent boots (B2's node_id is stable via persisted `iroh_sk.enc`). Co-located, multicast peering should establish that first convergence. **If `s7` goes green on step 1 alone, we stop there.** If it does not converge (no first-contact path co-located), proceed to step 2.

## Step 2 — SAS first-contact seed (GATED; build only if step 1 doesn't converge)

Breaks the circular bootstrap for the very first contact before any fleet-net convergence:
- **SAS endpoint-exchange (new protocol wiring):** the SAS pairing handshake currently carries **no** iroh endpoint (`pairing/` enroll payloads carry owner_state + sealed keytree only). Add the peer's iroh node_id + endpoint to the `InviterEnrollResult` / `JoinerEnrollResult` payloads so each side observes the other's dialing coordinates first-hand.
- **B2 dedicated seed store:** persist the observed `(self_owner, sibling_node_id, endpoint, last_seen)` to a new plaintext store (`fleet_peer_seed.cbor`-style, `*_persist.rs` idiom — atomic CBOR + version byte, mirrors `fleet_net_persist`/`dm_outhold_persist`). Fed into the resolver at boot as `FleetSibling`, superseded by the real `FleetNetDoc` self-row once fleet-net converges.

## Out of scope / deferred

- **Cross-WAN pkarr rendezvous fallback** (~ZEB-513): same-owner discovery over pkarr for siblings never SAS-paired on a shared LAN. Case-B pkarr identity is per-owner-key today and would LWW-clobber siblings — a distinct fix.
- Owner-state, community-membership, or DM-signing changes — untouched.

## File-touch map (step 1)

- `src-tauri/src/reachability_resolver.rs` — `ReachabilitySource::FleetSibling` + `as_dto_str`; new `fleet` slot in `ResolverSlots` + `freshest()` (and `durable_preferred()` per plan); (no change to `update_with_source` signature).
- `src-tauri/src/fleet_net.rs` — `sibling_reachability_payload` mapper.
- `src-tauri/src/lib.rs` — boot-replay hook in `start_node` (~`5516`/`5600`, near the reachability replay ~`7797`).
- `src-tauri/src/event_loop.rs` — live-merge hook in the `fleet-net-v1` adapter (~`1921-1929`).
- `e2e-harness/tests/e2e_two_node.rs` — `s7` `HELD` promotion + any residual notes.
- New unit/integration test for the mapper + resolver wiring.

## Risks

- **Stale endpoint:** a persisted sibling row may carry a drifted endpoint; the dial fails and the supervisor's normal retry/liveness handles it (node_id-based dialing tolerates address churn via relay/holepunch). No worse than any stale record.
- **Multiple siblings / re-feed churn:** resolver keys by node_id (N siblings → N keys); LWW makes live re-feeds idempotent.
- **Downstream signature assumption:** an audit item — confirm no consumer verifies `identity_signature` on a `FleetSibling` entry (resolver has none today; check DTO/diagnostics/butler-set readers).
