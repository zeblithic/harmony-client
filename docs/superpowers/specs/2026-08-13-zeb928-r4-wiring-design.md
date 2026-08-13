# ZEB-928 — R4 wiring: enforce the bounded-degree dial set

**Ticket:** ZEB-928 (under ZEB-909). **Depends on:** ZEB-914 (shipped the pure engine
`src/community_topology.rs`). **Design predecessor:** `2026-08-13-zeb914-bounded-degree-topology-design.md` §"The wiring".

## Goal

Wire the pure topology engine into the live dial path so per-community degree is
actually bounded in router mode. ZEB-914 selects, deterministically, which enrolled
*device keys* a node should keep persistent links to; this ticket turns that selection
into concrete iroh dials and stops the emergent O(members²) mesh from forming.

Scope this pass (agreed): **bridge + filter, merge.** The N=50/N=200 harness
reconvergence validation is deferred to a follow-up ticket; this PR's correctness rests
on deterministic unit tests.

## What ZEB-914 gave us (the interface we wire)

- `FULL_MESH_THRESHOLD: usize = 32`
- `ring_order(devices: &BTreeSet<[u8;32]>, community_salt: &[u8]) -> Vec<[u8;32]>` (sort once)
- `neighbors_on_ring(ring, self_device) -> BTreeSet<[u8;32]>` (select per node)
- `community_neighbors(devices, self_device, community_salt) -> BTreeSet<[u8;32]>` (convenience;
  returns all-but-self below the threshold, empty if `self` is absent)

## The corrected architecture (choke-point, not per-site)

The reviewed ZEB-914 wiring sketch said "consult a per-community neighbor cache at the two
inflow sites." Grounding against the current tree corrected this. There are **seven**
production `kick()` producers, not two, and they are heterogeneous in the identity context
they hold:

| Context at the site | Sites |
|---|---|
| Full community (owner + community + device vk) | `community_gateway_dial_driver.rs:641`, `:747` |
| `OwnerAddr` only, no community | `reachability_resolver.rs:493`, `:495` |
| Bare `node_id [u8;32]` only | `iroh_zenoh_registration.rs:142` (boot seed), `zenoh_iroh_transport.rs:410` (drop), `event_loop.rs:1669` (drop) |

Three of seven sites carry nothing but the node_id, and the persistent dialer
(`reconnect_supervisor`) is itself keyed **purely** on `node_id [u8;32]` — no owner,
community, or device key anywhere in its state. A per-site consult is therefore impossible
at three sites and duplicated at the rest.

**All seven inflows converge on one node_id-keyed choke point: `SupervisorHandle::kick`.**
The filter belongs there — a single guarded entry point classifying a node_id — with
identity resolved out-of-band. (The ZEB-914 spec already anticipated this: "the filter must
wrap the `kick`, not any single healer.")

This forces a three-part decomposition, each part living where the data it needs actually
exists:

### 1. Bridge — resolver-side reverse index `node_id → device_key`

The engine picks *enrolled device keys*; the supervisor dials *iroh node_ids* — a distinct
transport key. A reachability record binds them **by signature, not co-storage**: it carries
`iroh_node_id` + an `identity_signature`, and the signing enrolled key is supplied
*externally* at verify time (`reachability_record.rs:241` `verify_inner_signature(payload,
actor, hlc, enrolled_vk)`). So the record alone cannot yield the mapping — but the enrolled
key is in hand, already verified, at every ingest seam, and dropped one line later:

- `address_book_sync.rs:215` verifies against `row.device` (the enrolled key), then `:232`
  calls `resolver.update(actor, payload, hlc)` and **discards `row.device`**.
- Beacon seam: `IdentifiedBeacon.membership_device_vk` (`community_rendezvous.rs:220`) —
  verified, discarded.
- Pkarr fallback seam: `verify_inner_signature` runs against a resolved enrolled key.

The bridge is: thread the already-verified enrolled key into the resolver's single write
funnel `update_with_source` (`reachability_resolver.rs:406`) as an added
`enrolled_vk: Option<[u8;32]>` param, and maintain a new reverse index
`node_id -> BTreeSet<[u8;32]> device_keys` under the same write lock as `inner`; clear entries
in `remove_owner` (`:767`). No new crypto, no new trust — we stop discarding a post-verification
value. The reverse direction (node_id → device) is what the kick path needs, because the kick
path only ever holds a node_id. A `node_id` may be asserted by more than one enrolled key
(delegate/butler devices, multi-owner), so the value is a set and admission is "intersects the
admitted set."

### 2. Controller — event-loop-side recompute of the admitted device-key set

Community materialized state is reachable **only** from the main event loop; the
resolver/supervisor have no handle to it (verified: grep of the resolver/supervisor for
`CommunitySyncRegistry`/`CommunityState` is empty). The controller therefore lives in the
`lib.rs` boot scope (~`lib.rs:12424`), the one place where `Arc<CommunitySyncRegistry>`, the
self device verify key (`community_signing_key_arc.verifying_key().to_bytes()`), and the
resolver all co-exist.

It is a lightweight task that, per joined community, polls the O(1) delta signal
`CommunityState::materialized_version()` (`community_state_crdt.rs:562`; bumped only on an
applied membership event) and **recomputes only on a delta**:

```
admitted_device_keys = ⋃ over joined communities C of
    community_neighbors(active_enrolled_keys(C), self_vk, &C.community_id.0[..])
```

where `active_enrolled_keys(C)` = `enrolled_keys_from_members(materialized(C).members)`
(`community_gateway_dial_driver.rs:162`; `Joined`-only, already post-revocation) collected into
a `BTreeSet`, and the community salt is the stable `community_id: SpaceId(pub [u8;16])`
(`&id.0[..]`). After publishing the new set to the oracle it calls `kick_sweep()` so already-
resolved admitted neighbors get (re-)dialed.

### 3. Oracle — the shared classifier read in the hot path

```rust
pub struct AdmissionOracle {
    enabled: bool,                                    // router-mode gate, set once at boot
    admitted: ArcSwap<BTreeSet<[u8;32]>>,             // device keys; written by the controller
    node_to_devices: RwLock<HashMap<[u8;32], BTreeSet<[u8;32]>>>, // reverse bridge; written by resolver
}

impl AdmissionOracle {
    /// Hot path. peer mode → always admit. router mode → admit iff the node_id's
    /// device key(s) intersect the admitted set; fail-open on unknown node_id.
    pub fn admit(&self, node_id: &[u8;32]) -> bool;
    pub fn publish_admitted(&self, keys: BTreeSet<[u8;32]>);   // controller
    pub fn bind(&self, node_id: [u8;32], device_key: [u8;32]); // resolver
    pub fn unbind_owner(&self, node_ids: &[[u8;32]]);          // resolver remove_owner
}
```

The supervisor holds `Option<Arc<AdmissionOracle>>` and consults it at **two** sites: the
public `kick` (`reconnect_supervisor.rs:291`) *and* the internal `do_sweep` re-arm loop —
sweeps re-arm every known non-connected slot without going through `kick`, so filtering only
`kick` would let a `kick_sweep()` blow the bound. Two consult sites, one oracle. When the
oracle is absent (unwired) or disabled (peer mode), behavior is exactly today's.

## Data flow (and why it is race-free)

*Peer resolves:* `update_with_source` writes the record → **binds** `node_id → device_key`
→ fires `kick(node_id)` → oracle classifies against the **just-written** binding. The bind
precedes the kick in the same code path, so a freshly-resolved neighbor is always classifiable
when it is kicked — no window where an admitted neighbor is dropped for lack of a binding.

*Membership changes:* controller sees the `materialized_version` delta → recomputes the
device-key set (rare; identity-fixed churn) → publishes → `kick_sweep()` re-arms; the two
filtered consult sites dial exactly the admitted, resolved neighbors and drop the rest.

*Degree converges downward, not by eviction:* existing over-threshold connections are not torn
down. A non-neighbor that disconnects fires a `Dropped` re-dial kick, which the filter drops,
so it is not re-dialed — the bound is reached via filtered inflow + natural churn.

## The three policy calls (approved)

1. **Fail-open on unknown node_id** at `kick` — an unclassifiable node_id (no binding: infra,
   non-community peers) is admitted. The bound is best-effort; fail-closed would risk starving
   legitimate dials. In steady router-mode state most node_ids are classifiable (their records
   resolved and bound).
2. **Self-not-on-ring → that community is treated as full mesh** (admit all its active devices).
   `community_neighbors` returns empty when `self_vk` is absent from the roster; the controller
   substitutes the full active set for that community to prevent islanding during the join window
   before local enrollment materializes.
3. **Inflow-filter only, no active eviction** — see "converges downward" above.

## Router-mode gate

`AdmissionOracle.enabled = (zenoh_session_mode() == "router")`
(`event_loop.rs:13237`/`parse_zenoh_mode`), set once at boot. Peer mode keeps full mesh (opt-in
router default unchanged, per the R3 decision).

## Touch-list (one focused PR)

- **New** `src/admission_oracle.rs` — `AdmissionOracle` (pure `admit`, synchronously testable)
  and the pure roster→admitted-set computation used by the controller.
- `src/reachability_resolver.rs` — reverse-index field; `enrolled_vk` param on
  `update`/`update_with_source`; populate under the `inner` write lock; evict in `remove_owner`;
  `bind`/lookup wiring to the oracle.
- `src/address_book_sync.rs` + beacon + pkarr seams — pass the verified enrolled key instead of
  dropping it.
- `src/reconnect_supervisor.rs` — hold `Option<Arc<AdmissionOracle>>`; guard `kick` and the
  `do_sweep` re-arm.
- `src/lib.rs` — construct the oracle (enabled from zenoh mode), thread into resolver +
  supervisor, spawn the controller task in the boot scope.

## Test strategy (deterministic; no harness this pass)

- `AdmissionOracle::admit` — peer-mode passthrough; admitted vs denied device key; fail-open on
  unknown node_id; multi-device node_id (set intersection); post-`publish_admitted` transition.
- `do_sweep` filtering — a `kick_sweep()` with a mixed admitted/denied known set re-arms only the
  admitted node_ids (guards the "sweep bypasses kick" hole).
- Controller compute (pure) — union across communities; `FULL_MESH_THRESHOLD` boundary;
  self-exclusion; self-not-on-ring → full-mesh fallback; empty when not a member.
- Bridge — `update_with_source` with `Some(enrolled_vk)` populates the reverse index; a bound
  node_id resolves to its device key; `remove_owner` evicts.

## Non-goals / deferred

- N=50/N=200 agent-testing harness reconvergence validation → dedicated follow-up ticket.
- No change to the router-mode default (stays opt-in).
- No cross-community global degree cap (per-community bounding only).
- No presence-driven healing (offline neighbors are R1/R3's holes to route around).
- No active eviction of existing over-threshold connections.

## Risks

- **Fail-open weakens the bound if many node_ids are unclassifiable.** Mitigated: in router mode
  the bridge binds on every verified record ingest, so the classifiable fraction is high; the
  follow-up harness quantifies realized degree.
- **Reverse-index multiplicity** (one node_id, several enrolled keys). Handled by a set-valued
  reverse index + intersection admission rather than an LWW single value.
- **Controller poll cadence.** Version polling is O(joined communities) cheap `u64` reads; the
  expensive O(N log N) recompute is delta-gated. If joined-community count grows large, revisit
  with a push signal off `insert_event`.

## As-built (PR #674) — deviations from the sketch above

Two facts surfaced while grounding against the live tree changed the plan; both are reflected in
the shipped code, not this section's antecedent prose.

- **Three dial-arming paths, not two.** The choke-point framing named `kick` + `do_sweep`. Parole
  (ZEB-910, `run_parole`) is a *third* path — it revives Dormant slots straight to `Retrying`,
  bypassing both. All three consult the one oracle; filtering only the first two would pass unit
  tests yet let parole slowly re-admit non-neighbors, eroding the bound over time.
- **Controller lives in `event_loop::run` (the supervisor block), not `lib.rs`.** That block is
  where the `SupervisorHandle`, the resolver, `community_registry` (a `run` param), and the self
  device key (via `dm_outbox.lock().community_signing_key.verifying_key()`) co-exist. The controller
  is `event_loop::run_admission_controller`, spawned there when the oracle is enabled (router mode)
  and an owner is present. It caches per-community rosters so unchanged communities are not
  re-`materialized()` each tick.
- **Only the DurableCrdt (address-book) bind seam is wired this pass.** The beacon
  (`membership_device_vk`) and pkarr seams are deferred: the pkarr path holds only a
  `DeviceIdentityHash` (not the enrolled verify key), and the beacon kick originates in the gateway
  driver, not the resolver. The address-book path is the steady-state feed the bound rests on; the
  others fail open (dialed) until an address-book row binds them.
- **Boot looseness.** Records ingested before the oracle is installed carry no binding, so
  boot-seed dials them (fail-open) until a fresh ingest re-binds. Correctness holds (fail-open
  never starves); realized boot/steady degree is what the deferred N=50/N=200 harness measures.
