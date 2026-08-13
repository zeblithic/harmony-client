# ZEB-914 (R4): Bounded-degree community topology — design

**Date:** 2026-08-13 · **Ticket:** ZEB-914 (R4, parent epic ZEB-909) ·
**Branch:** `zeblith/zeb-914-bounded-degree-topology` · **Depends on:** R3
(ZEB-912, Done — router-mode multi-hop + the scale sounding
`docs/research/2026-08-13-zeb912-r3-scale-sounding.md`).

## Goal

Give each community a **bounded-degree connectivity topology** so a member holds
O(K + log N) persistent links instead of O(N), while R3's router-mode multi-hop
carries delivery over the sparser graph. The R3 sounding proved the current
emergent full mesh is super-linear and breaks by N≈50 (200 MB join flood,
delivery failure); this is the scale path that makes large communities viable up
to the product ceiling (N≈200).

**This pass builds two things** (scope chosen 2026-08-13): (1) this spec, and (2)
a self-contained, property-tested **topology engine** — a pure function computing
the bounded neighbor set from the roster — with **zero live call sites**. The
hot-path wiring is specified here but built in a follow-up ticket.

## What R3 established (the mandate and the target)

- **Full mesh (degree N−1) does not scale.** Per-join linkstate flood is
  super-linear: 47 KB@N10 → 843 KB@N25 → ~200 MB + delivery failure @N50.
- **Bounded degree is the fix, empirically.** A ring (degree 2) kept join flood
  linear (~0.6 KB/node) and idle CPU at zero to N=200. Its *only* cost was
  **diameter-proportional convergence latency**: ~4.6 s reconvergence at N=200,
  because a ring's diameter is ~N/2 ≈ 100 hops.
- **R4's target** (recorded on ZEB-914): a small constant degree ~6–10, tuned to
  **shrink the diameter** that drives the ring's multi-second reconvergence — the
  tuning metric is membership-change reconvergence latency at N≈200, *not* flood
  (bounded degree solves flood at any small constant).

## The corrected architecture (why the naive framing is wrong)

There is **no per-community "dial every member" function** to replace. The
persistent dialer — `reconnect_supervisor` — is a **process-global** slot table
keyed by 32-byte iroh `node_id`, with no community scoping. The O(members²) mesh
is *emergent*: every member publishes a reachability record → every member's
`ReachabilityResolver` learns it → **every resolved peer is `kick()`'d into the
supervisor and dialed for process life** (`reachability_resolver.rs:491-498`).
At least six independent healers feed that same `kick` inflow (resolver
first-learn, boot seed, presence roster-edge sweep, address-change fanout,
gateway/starvation dial, transport-down re-kick).

**Consequences that shape R4:**

1. The bound must be enforced at the **`kick`/seed inflow**, not in a roster
   walk. A filter at one healer is defeated by the other five; it has to wrap the
   inflow (`reachability_resolver.rs:491-498` runtime + `iroh_zenoh_registration.rs:92/135`
   boot seed).
2. Because dialing is process-global, the realized dial set is the **union across
   all shared communities** of each community's bounded ring-neighbor set. The
   engine is per-community; the union is a wiring concern.
3. The dial set flows `node_id → iroh link → zenoh face`; it does **not** go
   through zenoh `connect/endpoints` (retired, ZEB-620). In **router mode**
   (`HARMONY_ZENOH_MODE=router`, `event_loop.rs:1375`), zenoh multi-hops over the
   sparse iroh mesh, restoring full logical reachability. This is precisely why
   R4 depends on R3 and is only safe in router mode.

## Design

### Ring membership: over active devices, not members

The roster is `BTreeMap<OwnerAddr, MemberState>` — keyed **per-member**, with
devices nested as `MemberState::enrolled_device_keys: BTreeSet<[u8;32]>`
(ed25519 device verify keys, already post-revocation in materialized state,
`community_membership.rs:2099-2111`, `community_gateway_dial_driver.rs:160`).

**The ring is over active *devices*** — the flattened set of Joined members'
`enrolled_device_keys`. Rationale: devices are the actual forwarding nodes in
R3's mesh; each independently needs bounded connectivity; a multi-device member
naturally occupies several **decorrelated** ring positions (resilience for that
member). The set is roster-derivable, so every node computes the identical ring.
The existing `enrolled_keys_from_members` helper
(`community_gateway_dial_driver.rs:165`) already produces exactly this set.

### Ring position: identity-derived, per-community

Each device's position is `H(community_salt ‖ device_key)` where `community_salt`
is the community id bytes. Two properties fall out:

- **Identity-derived, never address-derived** (the ticket's departure #1): the
  position is a hash of a stable enrollment-certified identity key, so it is
  neither Sybil-grindable nor CGNAT-collapsible.
- **Per-community decorrelation:** the salt makes the same device sit at
  unrelated positions in different communities, spreading hub load — no device is
  globally "rank 0."

Ties (hash collisions) break by raw `device_key` bytes. Everyone derives the same
total cyclic order, so the ring is canonical.

### Identity-fixed, not presence-healed

The ring changes **only when the roster changes** (a member/device is
enrolled or removed — rare, human-timescale). A device going *offline* does
**not** trigger a topology recompute: it is a "hole" that R3 multi-hop routes
around, and the existing reconnect supervisor keeps trying it. This is the
logical end of "identity-derived": the ring is a stable structural backbone;
liveness is an overlay handled by R3 + the supervisor + R1 (island repair), not
by re-ringing. It also keeps R4 a pure function of the roster, recomputed only on
rare membership deltas (signalled O(1) by `materialized_version()`,
`community_state_crdt.rs:562`).

### Topology: symmetric circulant (protected lattice + power-of-two fingers)

Because we are **informed** (departure #2 — every node has the exact roster),
we do not need Freenet's blind statistical topology. We use the canonical
informed structure: a **circulant graph over the roster-sorted device ring**.

Sort all active devices by ring position into a canonical cycle. Let `r` be this
node's rank. Its neighbors are the devices at ranks `r ± o` for each offset `o`
in an offset set `O`:

- **Protected lattice:** `o = 1` is always present. The union of all ±1 edges is a
  Hamiltonian cycle → the graph is **connected by construction**, always. This is
  Freenet's protected successor/predecessor lattice, made exact.
- **Fingers:** the larger offsets (geometrically spaced powers across
  `[1, ⌊N/2⌋]`) are the long links that give **O(log N) diameter**.

Why circulant-in-rank rather than hash-space Chord fingers:

- **Symmetric by construction.** The device at rank `r+o` has this node at its
  own rank `−o`, so both independently compute the *same* undirected edge. That
  gives a **hard degree bound** (`2·|O|`), no dial duplication, no in-degree
  inflation, and no capacity/pruning heuristics — and it makes symmetry a
  testable invariant.
- **Balanced regardless of hash distribution** (rank space is uniform by
  definition; hash-space Chord can cluster).
- The one cost — a roster change shifts ranks and perturbs many neighbor sets
  network-wide (vs hash-space Chord's O(log N) per join) — is **neutralized by
  the identity-fixed model**: roster changes are rare, and each node just
  recomputes its own ~10 neighbors locally from the CRDT it already holds, with
  no coordination. (Hash-space Chord remains the fallback if churn ever proves
  frequent; noted, not built.)

### Degree ↔ diameter tuning

The offset set `O` is the knob. Given a `degree_budget` D, pick `⌊D/2⌋` offsets,
always including 1, geometrically spaced across `[1, ⌊N/2⌋]` so greedy routing
composes them to cover the ring. Trading D against diameter:

- Full power set `O = {1,2,4,…,2^k}` with `2^k ≈ N/2`: degree `2⌈log₂N⌉` (≈14 at
  N=200 devices), diameter ≈ `⌈log₂N⌉` (~7 hops).
- Subsampled, D=10 (5 offsets spanning to N/2): diameter ~5–10 hops.

**Payoff:** at N=200, diameter drops from the ring's ~100 hops to ~5–12, so
reconvergence drops from ~4.6 s to an estimated **sub-second** — the quantified
R4 win. The engine's output feeds directly back into the R3 probe to verify this
numerically (plan task).

**A note on N (device-count, not member-count).** The topology math above uses
`N` = active *device* count, which is what the ring is built over. The R3
sounding's `N=200` was likewise device/session count. The **product ceiling of
200 is members**, so a full 200-member community with multi-device users may
reach `N ≈ 300–400` devices — beyond the sounding's measured range. This barely
moves the circulant: `⌈log₂400⌉ = 9` vs `⌈log₂200⌉ = 8`, one extra hop. Log
scaling is exactly why bounded degree is the right structure — it absorbs the
device multiplier the ring's linear diameter could not.

**Default:** `degree_budget = 10` (top of the R3-recorded ~6–10 target; the
full power set that minimises diameter would be ~14).

### Cross-community composition

The realized process-global dial set is `⋃_C neighbors(C)` over communities `C`
this node shares with each peer. A node in few communities pays a small multiple
of the per-community degree. A **global cap** across many communities (Freenet
caps total connections 25–200) is a real concern for a user in dozens of
communities, but it is **out of scope for R4** — per-community bounding is the
mandate. Flagged for a future ticket; not built.

### Full-mesh threshold

Below a threshold the current full mesh is correct and simpler (ticket + sounding
agree). The sounding puts the crossover between cheap (N≤25) and painful (N≥50),
so the default threshold is **32 active devices**: below it, `community_neighbors`
returns all-but-self (full mesh, today's behavior); at/above it, the circulant
kicks in. Tunable; measured on device-count, not member-count.

## The engine (this pass — the code deliverable)

A new pure module `src-tauri/src/community_topology.rs`. No I/O, no async, no app
state — takes the flattened device set so it is trivially testable with synthetic
keys and decoupled from the roster type.

### Interface

```rust
/// Target neighbor count above the full-mesh threshold.
pub const TOPOLOGY_DEFAULT_DEGREE: usize = 10;
/// Below this many active devices, a community stays full mesh.
pub const FULL_MESH_THRESHOLD: usize = 32;

/// Deterministic bounded-degree neighbor selection for one community's device ring.
///
/// - `devices`: all active (Joined, post-revocation) enrolled device keys in the
///   community, INCLUDING `self_device`.
/// - `self_device`: this node's enrolled device key; must be in `devices`.
/// - `community_salt`: community id bytes — decorrelates ring positions per community.
/// - `degree_budget`: target max neighbors above the full-mesh threshold.
///
/// Returns the subset of `devices` this node should keep persistent links to
/// (never includes `self_device`). Below `FULL_MESH_THRESHOLD` devices, returns
/// all-but-self. Deterministic and symmetric: for any a,b in `devices`,
/// `b ∈ community_neighbors(a)` ⟺ `a ∈ community_neighbors(b)`.
pub fn community_neighbors(
    devices: &BTreeSet<[u8; 32]>,
    self_device: &[u8; 32],
    community_salt: &[u8],
    degree_budget: usize,
) -> BTreeSet<[u8; 32]>;
```

Position hashing uses the crate's existing `harmony_crypto::hash` (BLAKE3/SHA-256
truncated to a u64 for the ring coordinate); ties broken by `device_key` bytes.

### Properties (the test suite — this is a TDD deliverable)

1. **Self-exclusion:** `self_device ∉ result`.
2. **Membership:** `result ⊆ devices`.
3. **Symmetry:** ∀ a,b ∈ devices: `b ∈ neighbors(a) ⟺ a ∈ neighbors(b)`. *(Core
   correctness invariant.)*
4. **Degree bound:** above threshold, `|result| ≤ degree_budget`; below, `= |devices|−1`.
5. **Connectivity:** the graph over all devices is connected (BFS reaches all) —
   guaranteed by the ±1 lattice; tested across N and degree budgets.
6. **Diameter:** measured graph diameter ≤ the expected O(log N) bound for the
   chosen degree (regression-style assertion at representative N).
7. **Determinism:** identical inputs → identical output; independent of insertion
   order.
8. **Per-community decorrelation:** different `community_salt` yields a
   low-correlation neighbor set for the same device (statistical).
9. **Churn-delta (bounded per node):** adding/removing one device changes any
   single node's neighbor set by a bounded amount; documents the rank-churn cost.

### Wiring status

**None.** The module is compiled and tested but has zero call sites this pass.
That is the whole point of the staged scope: the algorithm lands proven and
risk-free, and the hot-path change is a separate reviewable PR.

## The wiring (follow-up ticket — specified, not built)

A new ticket under ZEB-909 will:

1. **Kick-inflow filter.** Consult a per-community neighbor cache at
   `reachability_resolver.rs:491-498` (runtime kick) and
   `iroh_zenoh_registration.rs:92/135` (boot seed): a `node_id` is admitted to the
   supervisor iff its `(OwnerAddr, device)` is in `community_neighbors(...)` for
   ≥1 shared community, or the community is below threshold.
2. **The identity→node_id bridge** *(the load-bearing risk).* The engine selects
   *device keys*; the supervisor dials *`iroh_node_id`* — a different key, not in
   the roster, known only once a peer's reachability record resolves (bound to the
   enrolled key at `reachability_record.rs:196-201`). The follow-up must add a
   resolver-side index `enrolled_device_key ↔ current iroh_node_id` so a chosen
   ring neighbor becomes a concrete dial. Unresolvable neighbors (likely offline)
   stay holes that R3 routes around — consistent with the identity-fixed model.
3. **Router-mode gate.** Filter only when `HARMONY_ZENOH_MODE=router`; in peer
   mode a bounded physical mesh would lose reachability, so keep full mesh.
   Likely a combined/dedicated flag.
4. **Threshold gate.** Bound only communities with ≥ `FULL_MESH_THRESHOLD` active
   devices.
5. **Harness validation.** Agent-testing e2e at N=50 and N=200: confirm delivery
   survives over the bounded router mesh and measure reconvergence vs the R3 ring
   baseline (feed engine output into the harness/probe topology).

## Non-goals (this pass)

- No wiring, no flag, no resolver/supervisor changes, no reachability-record
  changes.
- No change to the router-mode default (stays opt-in per the R3 decision).
- No cross-community global degree cap.
- No presence-driven healing (offline devices are R3/R1's job, not R4's).

## Risks and caveats

- **The bridge.** The resolver has no `enrolled_device_key → node_id` index today
  (keys on `(OwnerAddr, iroh_node_id)`). Building it is the follow-up's main work
  and its main risk; the engine deliberately stops short of it.
- **Router-mode maturity.** R4 is only safe in router mode, which is still
  opt-in/experimental. The follow-up gates on it explicitly.
- **Rank-churn at network scale.** A roster change perturbs many neighbor sets
  network-wide; acceptable because membership churn is rare (identity-fixed) and
  each recompute is local. Revisit with hash-space Chord only if churn proves
  frequent.
- **Single-community modelling.** The engine is per-community; the cross-community
  union and any global cap are explicitly deferred.
```