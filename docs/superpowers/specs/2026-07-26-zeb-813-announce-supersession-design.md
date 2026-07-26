# ZEB-813 — Deterministic announce supersession in `VerifiedLog`

**Status:** design (awaiting review)
**Ticket:** ZEB-813 (Urgent) · **Branches:** core `zeb-813-verified-log-supersession` (harmony repo), client `zeb-813-announce-supersession`
**Bases:** client `main` `f8cbf351`, core `main` `4eb4208`
**Follow-ups filed by design decision:** ZEB-814 (root-blob scale cliff), ZEB-815 (address-book structural move, with ZEB-811)

## Problem

`ReachabilityAnnounce` events are LWW routing data stored as permanent membership history.
The `ReachabilityPublisher` re-announces into every joined community's CRDT on startup,
network change, and a 60-minute TTL-refresh backstop; nothing supersedes or compacts old
announces; `encode_root_packet` ships the entire event log as one blob under
`harmony_content::cid::MAX_PAYLOAD_SIZE` (1 MiB − 1).

Measured on the fleet community (created 2026-06-21): 1,688 of 1,700 events (94.6% of
1.085 MB) are announces. The blob crossed the cap 2026-07-24 21:41Z; every root publish
and every root query-serve has failed silently since. Any community dies this way at
age ~1–2 months at fleet-like announce rates.

## Verified constraints the design rests on

1. **Announces are materialize-neutral.** `materialize`'s arms for
   `ReachabilityAnnounce` and `CommunityRelayAnnounce` are explicit no-ops
   ("no membership-state effect"; `community_membership.rs:3560`). Their only consumers
   are `ReachabilityResolver` / `CommunityRelayResolver`, which want latest-per-(actor,
   node). Removing a superseded announce cannot change any membership verdict at any HLC.
2. **Announce verification is time-independent.** RCH4's ±30-minute skew check is
   internal (payload `announced_at_ms` vs the event's own HLC `wall_ms`), not against the
   local clock — old announces re-verify successfully forever. Therefore **one-shot GC
   cannot work**: remote root blobs merge as verified per-event union
   (`handle_incoming_publish` → `verify_event` + `insert_event`), so any uncompacted peer
   resurrects a naive deletion. The rule must live in insert/merge semantics.
3. **The log is the core kernel.** `CommunityState.log` is
   `harmony_crdt_sync::verified_log::VerifiedLog<MembershipPolicy>` (ZEB-748/750), so the
   deterministic rule belongs on `LogPolicy` — every replica applies it identically at
   every insert, merge, and load.

## Design

### 1. Core seam (harmony repo, `crates/harmony-crdt-sync/src/verified_log.rs`)

A defaulted method on `LogPolicy`:

```rust
/// Deterministic supersession: `newer` makes `older` redundant.
///
/// Contract (all four are policy obligations; the log cannot check them):
/// (a) `supersedes(n, o)` implies `cmp(n, o) == Ordering::Greater`;
/// (b) transitive along chains: if `supersedes(b, a)` and `supersedes(c, b)`
///     then `supersedes(c, a)`;
/// (c) pure — same inputs, same answer, on every replica;
/// (d) superseded-eligible events must be MATERIALIZE-NEUTRAL: dropping a
///     superseded event must not change `materialize`'s output for any
///     event subset, because removal changes the strictly-prior set that
///     future `verify` calls see.
///
/// Under (a)–(c) the retained set converges to "the per-supersession-key
/// maximum plus all never-superseded events" regardless of arrival order
/// or merge interleaving — a join-semilattice.
///
/// Default: nothing supersedes anything (current behavior for all
/// existing adopters).
fn supersedes(_newer: &Self::Event, _older: &Self::Event) -> bool {
    false
}
```

`InsertOutcome` gains a variant:

```rust
/// The event was new but an already-stored event supersedes it; it was
/// dropped WITHOUT running `verify` (mirrors `AlreadyKnown`'s
/// verify-skip: a stale record changes nothing, so proving it valid
/// buys nothing). Callers treat this exactly like `AlreadyKnown`:
/// no state change, no persistence, no dirty mark.
Superseded,
```

`insert` flow becomes:

1. Dedup by id → `AlreadyKnown` (unchanged).
2. If any stored event supersedes the candidate → `Superseded` (no verify).
3. Verify against the strictly-prior set (unchanged).
4. On `Ok`: insert, then remove every stored event the candidate supersedes
   (collect ids, then remove — `BTreeMap` borrow discipline is an implementation
   detail).

`from_verified_events` (the trusted load path) applies the same rule after
deduplication: drop every event superseded by another present event. **This is the heal
moment** — the first boot on new code shrinks an over-cap log in memory before any
publish is attempted. Implementation may be a simple pairwise pass: worst realistic
n = 1,700 → ~2.9 M cheap discriminant comparisons, one-time at boot; not a hot path.

Complexity of steps 2/4 is O(n) scan per insert. Post-compaction n is small
(≈ members × devices + true history); acceptable without indexing.

### 2. Client policy (`src-tauri/src/community_state_crdt.rs`)

```rust
fn supersedes(newer: &SignedMembershipEvent, older: &SignedMembershipEvent) -> bool {
    use MembershipEventKind::*;
    let same_key = match (&newer.kind, &older.kind) {
        (ReachabilityAnnounce { payload: pn }, ReachabilityAnnounce { payload: po }) => {
            newer.actor == older.actor && pn.iroh_node_id == po.iroh_node_id
        }
        (CommunityRelayAnnounce { payload: pn }, CommunityRelayAnnounce { payload: po }) => {
            newer.actor == older.actor
                && pn.relay.relay_device_id == po.relay.relay_device_id
        }
        _ => false,
    };
    same_key && event_sort_key(newer) > event_sort_key(older)
}
```

Supersession keys, from the actual payload structs: `ReachabilityAnnouncePayload.
iroh_node_id` (`[u8; 32]`, wire `nd`; defined in core `harmony-reachability`
`src/record.rs`) and `CommunityRelayAnnouncePayload.relay.relay_device_id`
(`[u8; 16]`, wire `rd`; `community_relay_announce.rs`).

Explicitly **excluded** from supersession: `DeviceAnnounce` (materializes enrolled
device keys — it is history), and every membership/config/governance kind.

`community_state_crdt::insert_event` maps core `Superseded` to the client's existing
`InsertOutcome::AlreadyKnown` at the boundary — every caller (engine dirty-marking,
persistence gating, cache bumping) already handles `AlreadyKnown` with exactly the
semantics `Superseded` needs. No new plumbing through the engine.

The ordering key reuses `event_sort_key` — the same canonical total order `cmp` uses —
satisfying contract (a) by construction; same-key transitivity (b) follows from total
order; (c) is trivially true (pure function of the two events).

### 3. Un-silence the cliff (`src-tauri/src/community_state_sync.rs`)

In `encode_root_packet`, after the encrypt step, measure `blob_ciphertext.len()`
against `MAX_PAYLOAD_SIZE`:

- ≥ 50%: `tracing::warn!` with community id, byte count, and percentage. Fires at most
  once per publish attempt (publishes are debounced), so no rate limiting needed.
- ≥ 80%: additionally `report_degraded(…, "state_root_near_cap", …)` — the existing
  degraded-path plumbing that reaches the frontend banner.
- On the `for_book` size failure itself: `report_degraded(…, "state_root_over_cap", …)`
  in both the publish path (today: warn + retry into the same wall) and the query-serve
  path (today: warn + silently withhold the reply).

No behavior change below 50%.

### 4. What is deliberately NOT here

- **No wire-format change.** Root blobs, event encoding, AEAD, ContentId derivation all
  unchanged. Old and new nodes interoperate: an old node's full log merges into a new
  node (stale announces land as `Superseded`, no churn); a new node's compacted blob
  merges into an old node as a plain subset (the old node keeps its superset and stays
  as broken as today until upgraded — no worse).
- **No migration tooling.** Load-time compaction heals over-cap communities on first
  boot. The rewritten `crdt.cbor` persists on the next ordinary mutation
  (the next hourly announce at the latest); until then the compacted view lives in
  memory, which is where `encode_root_packet` reads from — publishing recovers
  immediately after boot. This covers the **already-oversized-and-offline replica**
  (AVALON's `971bd814`, 118.7% of cap, frozen since 07-15, never once published):
  `crdt.cbor` deserialization rebuilds through `from_verified_events`
  (`community_state_crdt.rs:86`), so compaction runs on any persisted state — healthy,
  detonated, or long-offline — before the first publish is attempted.
- **No chunking, no address-book move** — ZEB-814 and ZEB-815 respectively.

## Testing

Core (harmony repo, `verified_log.rs` unit tests):

1. Permutation convergence: three same-key events inserted in all six orders → retained
   set is exactly the newest; outcomes are `Inserted`/`Superseded` as predicted.
2. Cross-key isolation: same kind, different key → both retained.
3. `Superseded` skips `verify` (verify-counting test policy).
4. `from_verified_events` compacts a mixed trusted set.
5. A policy without `supersedes` (default) — behavior byte-identical to today.

Client (`src-tauri`):

1. Policy matrix: same actor + same node supersedes; different actor retained; different
   node retained; relay-announce keyed independently; `DeviceAnnounce` and membership
   kinds never supersede.
2. Materialize-neutrality: for an announce-heavy generated log,
   `materialize(full) == materialize(compacted)` — enforces contract (d).
3. Over-cap heal: a fixture log whose encoded size exceeds `MAX_PAYLOAD_SIZE` fails
   `encode_root_packet` before compaction and succeeds after
   `from_verified_events`-based reload.
4. Engine ingest: a remote root blob carrying stale announces produces no dirty mark, no
   republish, no persist beyond replay-tracker (the `AlreadyKnown`-equivalent path).
5. Watermark surfacing: injected sizes trigger the 50% warn, the 80% degraded report,
   and the over-cap degraded report on both publish and serve paths.

Verification commands are the standard gates (fmt, clippy `--all-targets
--features test-fixtures`, nextest per-crate then full sweep pre-PR; core repo runs its
own crate gates).

## Rollout

1. **PR 1 (harmony repo):** core seam + core tests. Nothing else in the core repo
   changes — `supersedes` is defaulted and no core-repo code matches on
   `InsertOutcome`. The new variant is a compile-visible change for CLIENT matches,
   which is desirable: PR 2's boundary mapping is compiler-enforced at the rev bump,
   not discoverable-by-bug.
2. **PR 2 (harmony-client):** lockstep rev bump of ALL harmony crates to the PR-1 merge
   rev (single shared rev — split revs fail type unification), `MembershipPolicy`
   supersession, `Superseded`→`AlreadyKnown` boundary mapping, watermark surfacing,
   client tests.
3. **Fleet validation:** rebuild + restart fleet nodes; success = `payload too large`
   stops appearing, a successful root publish lands in the log, and a root query from a
   peer gets a reply (observable on the fleet board via the falsifier data Ildwyn/AVALON
   were asked for in ZEB-813).

## Risks

- **Prior-set drift** (verification after compaction sees fewer prior events): bounded
  by contract (d) + the materialize-neutrality test. The only removed events are ones
  `materialize` ignores.
- **Mixed-version fleet:** covered under "NOT here" — strictly no-worse, converges as
  nodes upgrade.
- **Replay admission / HLC trackers:** operate on publish-payload HLC watermarks, not
  event ids; unaffected by event removal.
- **Bootstrap-hint interaction:** `materialized()`'s hint guard keys on
  `log.is_empty()`; compaction never empties a log that had real events (membership
  events are never superseded), so the guard's semantics are untouched.
