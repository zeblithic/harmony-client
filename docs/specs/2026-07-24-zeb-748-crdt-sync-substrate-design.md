# ZEB-748 — CRDT-sync substrate (ZEB-571 item 6) — design

**Ticket:** ZEB-748 (ZEB-571 item 6). **Author:** Koya. **Approved:** Jake, 2026-07-24.

**Goal:** Give core Harmony the reusable verified-CRDT-sync substrate the platform lacks, by extracting the client's proven engines into `harmony-crdt-sync` and proving each with one real, byte-pinned adopter — *without* forcing the five domains that structurally resist a clean abstraction.

**Scope decision (Jake, 2026-07-24):** kernel-lift + prove-on-membership — over substrate-only, lift-plus-converge-mint/community, and the full six-domain monolith. The audit's "converge all six" premise was falsified by recon (see ZEB-748 for the per-domain table); only owner-state (snapshot) and community-membership (event-log) are clean fits.

---

## Phasing (refined from deep recon, Jake-approved 2026-07-24)

Item 6 decomposes into two independently-shippable engine extractions of comparable size. They are sequenced, not bundled — mixing a pure net-new primitive with a heavy async-engine abstraction in one PR would be large and hard to review.

- **Phase 6a — `VerifiedLog<P>` event-log engine + community-membership adopter.** *This document.* The core primitive is pure `no_std`; membership maps to the trait almost 1:1. Plannable now.
- **Phase 6b — lift `FleetSyncEngine<S>` snapshot engine to core + owner-state adopter.** Deferred to its own design pass — `FleetSyncEngine` is welded to client-only seams (`CanonicalPayload`, `ContentStore`, `FleetKeySet`, `Hlc`, a spawned tokio task, Zenoh-bridged mpsc channels) across 10+ call sites; moving it means abstracting each of those into core. Sketched at the end of this doc; full design when 6a lands.

Both phases close ZEB-748.

---

## Phase 6a architecture

Two things land, both in the existing core crate `harmony-crdt-sync` (created by item 3 / ZEB-737 for the backfill latches; its own `lib.rs` doc already names `VerifiedLog`/`FleetSyncEngine` as its growth target):

1. **`LogPolicy` trait + `InsertOutcome<E>` + `VerifiedLog<P>`** — a pure, `no_std`, I/O-free verified-event-log kernel: an in-memory event set keyed by a policy-supplied id, a total order by a policy-supplied sort key, per-event `verify` against the materialized prior state, and a cached materialized view. No async, no transport, no persistence, no crypto — those stay caller-side (the `password_envelope` I/O-free-kernel pattern from item 8).
2. **community-membership adopts it** — `CommunityState` (client `community_state_crdt.rs`) holds a `VerifiedLog<MembershipPolicy>` in place of its hand-rolled `events` map + `insert_event` + materialized cache; `MembershipPolicy` supplies the existing `verify_event` / `materialize_with_now` / `event_sort_key` bodies unchanged.

### The trait

```rust
// harmony-crdt-sync, no_std + alloc, no new deps.
pub trait LogPolicy {
    type Event;                       // e.g. SignedMembershipEvent
    type EventId: Ord + Clone;        // e.g. [u8; 16] — dedup key
    type State;                       // e.g. MaterializedMembership
    type Context;                     // per-community config threaded into BOTH verify and materialize
    type Error;                       // e.g. VerifyError

    fn event_id(e: &Self::Event) -> Self::EventId;
    fn sort_key(e: &Self::Event) -> impl Ord;     // total order; membership's 5-tuple (wall_ms, logical, device_id, id, sig)
    fn verify(e: &Self::Event, prior: &Self::State, ctx: &Self::Context) -> Result<(), Self::Error>;
    fn materialize(events: &[Self::Event], ctx: &Self::Context) -> Self::State;
}

pub enum InsertOutcome<E> { Inserted, AlreadyKnown, Rejected(E) }
```

**Refinement vs. the audit sketch:** `Context` is threaded into `materialize` too, not only `verify`. This is required by membership — `materialize` needs `admin_addr` (bootstrap seed) and an optional wall-clock `now_ms` (PendingJoin 30-day expiry), both per-community config that is not part of any event. The one `Context` type carries what both stages need.

### The engine

```rust
pub struct VerifiedLog<P: LogPolicy> {
    events: BTreeMap<P::EventId, P::Event>,   // dedup by id
    cache: Option<P::State>,                  // materialized view, invalidated on mutation
}

impl<P: LogPolicy> VerifiedLog<P> {
    pub fn new() -> Self;

    /// dedup by event_id (AlreadyKnown short-circuits with no verify work);
    /// else compute prior = materialize(strictly-prior-by-sort_key prefix, ctx),
    /// run verify(e, &prior, ctx), insert + invalidate cache on Ok, else Rejected(e).
    pub fn insert(&mut self, e: P::Event, ctx: &P::Context) -> InsertOutcome<P::Error>;

    /// materialized view over all events, computed on demand and cached until next mutation.
    pub fn materialized(&mut self, ctx: &P::Context) -> &P::State;

    pub fn contains(&self, id: &P::EventId) -> bool;
    pub fn len(&self) -> usize;
    pub fn events(&self) -> impl Iterator<Item = &P::Event>;   // sorted by sort_key — for whole-log serialization + backfill
}
```

`insert` reproduces `CommunityState::insert_event`'s exact algorithm: `contains_key` → `AlreadyKnown`; else materialize the strictly-prior prefix (this is membership's `prior_state_at_event`), `verify_event`, insert on `Ok` and bump the cache version, else `Rejected(err)`. The prior-state replay is O(events) per insert — unchanged from today; membership already pays exactly this.

### What stays client-side (the app binding)

Everything that is not the pure log kernel:

- **The whole `CommunitySyncEngine`** (`community_state_sync.rs`) — root-blob publish/fetch/decrypt/merge, `RootFetchLatch` wiring, `CommunityStateAtHlcAdapter`, `CommunityRootHlcTracker`.
- **Persistence** (`community_state_persist`), **transport**, **AEAD**.
- **The post-outcome side-effect dispatch** in `insert_event_with_resolved_pubs` (auto-counter-sign on `Inserted`/still-pending-`AlreadyKnown`, pending-join clear, redemption oneshot, delta emission, `notify_dirty`) — layered on top of the generic insert, keyed on the returned `InsertOutcome` + the event's kind.
- **`insert_local_event_pair`** (atomic Kick+Rotation / Leave+Rotation under one lock) — the caller already holds the `CommunityState` lock and calls `verified_log.insert` twice; the engine need not model atomic pairs.
- **The forward-incompatible whole-blob decode policy** (`ProposalKind::ChangeThresholds`) — a decode-layer concern; the engine only ever holds already-decoded events.

### Byte-transparency (the non-negotiable gate)

Same discipline as ZEB-746. The adoption wraps today's **exact** `SignedMembershipEvent` serde type and the **exact** `event_sort_key` comparator — no new envelope, no field reordering. Proof: every existing membership wire fixture stays green with **zero regeneration**:

- `tests/wire_format/community_fixtures.rs` (~22 `*_wire_bytes_pinned`)
- `zeb250_fixtures.rs` / `zeb254_fixtures.rs` / `zeb285_fixtures.rs` / `zeb290_fixtures.rs` / `zeb291_fixtures.rs`
- the `CommunityState` CBOR skip-default pins inline in `community_state_crdt.rs`

The same-length-keys canonical-CBOR invariant is a correctness property, not style — the refactor must not perturb it.

---

## Sequencing (2 PRs, core-first — mirrors items 2/3/5/7/8)

1. **Core PR** (`zeblithic/harmony`, branch `zeb-748-crdt-sync-substrate`): add `LogPolicy` + `InsertOutcome<E>` + `VerifiedLog<P>` to `harmony-crdt-sync` (pure `no_std` + `alloc`, no new deps) with rustdoc + unit tests over a toy policy (insert / dedup / reject / materialize / cache-invalidation); `no_std` default-build check; gates + CI + bots → merge → rev **R**.
2. **Client PR** (`zeblithic/harmony-client`, branch `zeb-748-crdt-sync-substrate`): bump the lockstep harmony pins → **R** (harmony-pkarr stays on its own pin); implement `MembershipPolicy`; refactor `CommunityState` onto `VerifiedLog<MembershipPolicy>`; retain all side-effect dispatch + `insert_local_event_pair`; every membership wire fixture green with zero regeneration; gates + CI + bots → merge.

## Definition of done (6a)

1. `LogPolicy` / `InsertOutcome<E>` / `VerifiedLog<P>` in core `harmony-crdt-sync` with rustdoc + unit tests; `no_std` default build green (22 dependents unaffected).
2. `CommunityState` on `VerifiedLog<MembershipPolicy>`; every existing membership wire fixture green with zero regeneration; all side-effect behavior preserved.
3. Both repos' gates green (fmt, clippy `--all-targets -D warnings`, scoped nextest, `no_std` default build for the crate); CI + bots converged.

---

## Phase 6b — snapshot engine lift (design-deferred, sketch only)

Not part of 6a. Recorded so the umbrella scope is legible.

`FleetSyncEngine<S>` (`fleet_sync.rs:245`) is a mature debounced-publish + replay-protected whole-state-merge engine, `S: CanonicalPayload + DeserializeOwned + Clone + Send + 'static`, with 10 uniform instantiations + 2 hand-rolled siblings (`owner_state_sync::SyncEngine`, `mint_sync::MintSyncEngine`). Lifting it to core requires abstracting its client-only seams:

- `CanonicalPayload` (ZEB-220 CBOR canonicalization) → a core canonical-encode trait, or inject a serialize/deserialize closure.
- `ContentStore` → reconcile against core `harmony-content`'s trait (confirm identity before assuming reuse).
- `FleetKeySet` → inject a key-provider trait.
- `Hlc` → move to / mirror in core, or make the replay-tracker key generic.
- tokio task + mpsc channels → live behind the crate's `std` feature (the pure merge/replay core can stay `no_std`; only the publish driver needs a runtime).

Adopter: owner-state (already rides the engine) + the 10 doc-sync users, via an import swap once the core-abstraction seams exist. Its `owner_publish_envelope_is_byte_identical_to_legacy` pin + `CRDT_FILE_SCHEMA_V1/V2` disk prefix are the behavior-preservation anchors. Full 6b design lands when 6a merges.

## Deferred domains (JC2 — filed as ZEB-571 children)

mint (ZEB-749), community-state (ZEB-750), channels (ZEB-751), voting (ZEB-752), dfrost (ZEB-753). Each carries the concrete structural reason it resists the generic engines. Channels/voting/dfrost additionally depend on a future async/cross-domain `LogPolicy` extension (the seam 6a deliberately omits per JC1).
