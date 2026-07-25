# ZEB-750 — Converge `community_state_sync` onto the core CRDT-sync decision kernels

**Status:** design (awaiting review)
**Ticket:** ZEB-750 (parent ZEB-571 seam audit, CRDT-sync family) · **Branch:** `zeb-750-community-sync-convergence`
**Scope decision:** *All three remaining kernels* — replay admission, HLC tick, and debounce —
adopted into `community_state_sync.rs`, bringing it to parity with `fleet_sync.rs`. Single repo
(harmony-client); **zero core changes**. Bases: client `main` `48dffce7`, core `main` `4eb42086`.

## The ticket's premise is stale — read this first

ZEB-750 was written against client `main` `e63fc085` and framed its scope as a decision:

> Decide whether community-state adopts the **event-log** engine (`VerifiedLog<P>`, likely via the
> async/cross-domain `LogPolicy` extension deferred per JC1) or a generalized snapshot engine.

Three PRs landed after it was filed — #544, #546, #547 — and **#544 (ZEB-748 phase 6a) already made
that decision and implemented it**. `CommunityState`'s event log is a core `VerifiedLog` today:

```
community_state_crdt.rs:18    use harmony_crdt_sync::verified_log::{LogPolicy, VerifiedLog};
community_state_crdt.rs:173       log: VerifiedLog<MembershipPolicy>,
community_state_crdt.rs:324   impl LogPolicy for MembershipPolicy { … }
```

The ticket predicted the answer ("it's actually event-log-shaped, a candidate for `VerifiedLog<P>`
more than the snapshot engine") and ZEB-748 acted on it. The `6282` production-LOC figure in the
ticket sized the whole community subsystem, whose *state* half has since converged.

What remains is narrow and specific: the **sync driver** is still hand-rolled. Current adoption
across the client:

| Module | Adopted from `harmony-crdt-sync` | Still hand-rolled |
|---|---|---|
| `fleet_sync.rs` | `Admission`, `DebounceLatch`, `DirtySignal`, `HlcTick`, `MonotoneMap`, `PublishClaim`, `PublishOutcome`, `ReplayTracker`, `RetryBackoff` | — |
| `owner_state_sync.rs` | `MonotoneMap`, `ReplayTracker` | — |
| `community_state_crdt.rs` | `VerifiedLog`, `LogPolicy` | — |
| **`community_state_sync.rs`** | **`RetryBackoff` only** (ZEB-761 / #547) | **replay admission, HLC tick, debounce** |

This spec covers exactly those three.

## Problem

### 1. Apply-before-advance is enforced by a `debug_assert` across a 354-line window

`CommunityRootHlcTracker` (`community_state_sync.rs:823`) is a per-`(publisher_addr, device_id)`
watermark map with a deliberate non-mutating/mutating split — `would_accept` (`:842`) then `record`
(`:862`). Its own doc names the pattern:

> The split implements the "advance-after-success" idiom that owner-state's call sites apply
> manually to a bare BTreeMap.

That is precisely core `ReplayTracker`'s job (`replay_admission.rs:213`), which the crate root
describes as *"per-source replay protection with apply-before-advance enforced by the types."* The
difference is enforcement strength:

- **Community:** `record` carries a `debug_assert!` that the caller checked `would_accept`
  (`:862`). Compiled out in release builds. The ordering is a human-maintained invariant.
- **Core:** `commit` (`:261`) consumes a `CommitTicket` (`:176`), which only `admit` (`:240`) can
  mint. The ordering is unforgeable.

In the receive pipeline the two halves are **354 lines apart** — `would_accept` at `:3701`, `record`
at `:4055` — with membership gating, TOCTOU re-verification, CRDT merge, and persistence in between.
Every early-return branch in that window is a place the current code trusts a comment.

### 2. The HLC tick diverges from core, because of #1

`next_hlc` (`:3210`) carries a saturation branch core does not have:

```rust
Some(p) if p.logical == u32::MAX => Hlc {
    // Saturation escape: bump wall (vanishingly unlikely in
    // production — 4B publishes within one wall-millisecond —
    // but the alternative is debug-mode panic).
    wall_ms: p.wall_ms.saturating_add(1),
    logical: 0,
    device_id: ctx.device_id.clone(),
},
```

Its own comment gives the reason: *"Otherwise the resulting HLC would equal prev exactly, and
`record()` would panic via debug_assert."* The branch exists to dodge the assertion described in #1.

Core `HlcTick::next` (`hlc.rs:98`) makes the opposite choice — a saturating add that **ties** `prev`
(`hlc.rs:103`), documented as *"a stall, which is strictly preferable to admitting a replay."*
`fleet_sync` already uses that rule (it was extracted from `fleet_sync::compute_next_hlc`).

So the two engines disagree on one pathological case, and the disagreement is a downstream artifact
of the weaker enforcement rather than a considered domain difference.

### 3. Two vocabularies for one publish decision

`fleet_sync` speaks `DebounceLatch` / `PublishClaim` / `DirtySignal`. `community_state_sync` hand-rolls
the same decision with a raw `has_pending_dirty: Arc<AtomicBool>` plus a manually tracked
`next_wakeup`. ZEB-761 (#547) added a `settle_publish` helper to *both* engines and had to give them
different signatures — `PublishClaim` for `fleet_sync`, a bare `was_dirty: bool` for community —
flagged in that PR's convergence comment as "same decision, two vocabularies."

## Non-goals

Explicitly out of scope, and unchanged by this work:

1. `CommunitySyncRegistry` — the multi-instance registry. `fleet_sync`/`owner_state` are
   per-identity singletons; community is per-community. Genuinely domain-unique.
2. The membership-gated verify-on-receive pipeline and its TOCTOU re-verification, including its
   deliberately-different documented step order versus `FleetSyncEngine`'s.
3. `CommunityMembershipDelta` structured notifications.
4. The `replay.cbor` / `crdt.cbor` on-disk format and both byte-pin fixture files.
5. Retry policy. ZEB-761 settled the schedule and the precedence rule (a fresh mutation supersedes a
   pending retry); this spec only re-expresses the surrounding decision in kernel vocabulary.
6. Any change to `harmony-crdt-sync`. Every kernel this needs already exists on core `main`.

## Design

Three swaps, in dependency order. Swap 2 is unblocked by swap 1 (the assertion that forced the
divergent branch is gone); swap 3 is independent.

### Component 1 — replay admission

The type splits by role. This is what keeps the byte-pins intact by construction rather than by
careful maintenance.

| Role | Type | Notes |
|---|---|---|
| Persistence DTO | `CommunityRootHlcTracker { per_device: BTreeMap<(OwnerAddr, String), Hlc> }` | keeps `CanonicalPayload` + `Serialize`/`Deserialize`; identical CBOR |
| Runtime | `ReplayTracker<(OwnerAddr, String), Hlc>` | held in `Arc<Mutex<…>>` on `InternalCtx` |

`ReplayTracker::new` requires a `local: K`. Community's is `(ctx.self_owner, ctx.device_id.clone())`.
It is **not persisted** — it is reconstructed from ctx at load — so the on-disk shape is unchanged.

- **Load:** `ReplayTracker::from_accepted(local, dto.per_device)` (`replay_admission.rs:231`)
- **Save:** `CommunityRootHlcTracker { per_device: tracker.accepted().clone() }` (`:295`)
- **Receive `:3701`:** `would_accept` → `admit`, matched on `Admission`
- **Receive `:4055`:** `record` → `commit(ticket)`
- **Local mint `:3255`:** `record(ctx.self_owner, now)` → `observe_local(now)`

The `CommunityRootHlcTracker` struct keeps its name and its serde derives but loses `would_accept`,
`record`, and the `debug_assert`. It becomes a DTO with no logic.

The `:3255` migration belongs to **this** component, not Component 2: removing `record` forces it,
because `next_hlc` is a `record` call site. Component 2 changes only the tick *arithmetic*. Keeping
the two separable is what makes the split in Risk 1 viable.

**Load-time constraint.** `from_accepted` needs `local` at the moment the tracker is built, so
`ctx.self_owner` and `ctx.device_id` must be in hand wherever `load_replay` is called today. The
implementation plan must confirm that call site has both, or thread them to it; if the tracker is
constructed before the identity is known, this becomes a two-step build (load DTO, attach `local`)
rather than a one-liner.

**The admission predicates are already semantically identical**, so this is a swap and not a
behaviour change. Community accepts iff `candidate.is_strictly_newer_than(prev)`, which delegates to
`Hlc`'s derived lexicographic `Ord` (`owner_state_types.rs:342`, pinned by
`derived_order_matches_is_strictly_newer_than`). Core answers `Duplicate` iff `existing >= clock`
(`replay_admission.rs:240`). Those are complements of one comparison. `Hlc`'s third field
(`device_id`) participates in the derived `Ord`, but it is constant within a
`(OwnerAddr, device_id)` key, so it cannot change the verdict.

**Ticket threading is the main implementation cost, and the main benefit.** `CommitTicket` is
deliberately not `Clone` and carries
`#[must_use = "dropping a CommitTicket leaves the source's watermark un-advanced (correct after a
FAILED apply, a silent bug after a successful one)"]`. Moving it across the 354-line window means
every early return drops it — which is exactly right: the watermark stays put, so the peer's next
delivery of that frame is admitted again. Where the borrow checker objects, that is an audit
finding, not an obstacle.

### Component 2 — HLC tick

Component 1 already moved `next_hlc`'s two tracker touches — the read (`tracker.per_device.get(&key)`
→ `accepted_from(&local)`, forced because `per_device` is now private to the DTO) and the write
(`record` → `observe_local`, `:3255`). **This component changes only the arithmetic in between**,
which is what lets Risk 1's split work:

1. read `prev` via `tracker.accepted_from(&local)` — *from Component 1*
2. convert `Hlc → HlcTick { wall_ms, logical }`
3. `HlcTick::next(prev_tick, wall_ms)` — replacing the hand-rolled three-branch match
4. rebuild `Hlc { wall_ms, logical, device_id: ctx.device_id.clone() }`
5. `tracker.observe_local(hlc.clone())` — *from Component 1*

Steps 2–4 are this component's entire diff. The `logical == u32::MAX` branch is **deleted**. Saturation now ties and stalls, matching core and
`fleet_sync`. `observe_local` (`replay_admission.rs:284`) returns `false` in that case rather than
tripping an assertion.

This is a deliberate behaviour change in a pathological case (~4 billion publishes inside one
wall-millisecond, or a sustained backward clock correction of equivalent length). It must be called
out in the PR body, not buried in a diff.

The `Hlc` ↔ `HlcTick` conversion must not disturb `Hlc`'s locked wire field order.

### Component 3 — debounce

A direct mirror of `fleet_sync`, using all three arms:

| Site | Today | After |
|---|---|---|
| notify wake | set `has_pending_dirty`, recompute `next_wakeup` | `latch.mark_dirty(now_ms)` (cf. `fleet_sync.rs:794`) |
| debounce fire | manual `swap(false)` + inline logic | `latch.on_deadline(swap(false))` → `PublishClaim` (cf. `:814`) |
| `flush_now` | manual `swap(false)` + inline logic | `latch.on_flush(swap(false))` → `PublishClaim` (cf. `:835`) |
| shutdown | manual `load()` + inline logic | `latch.on_shutdown(load())` → `PublishClaim` (cf. `:894`) |

`has_pending_dirty` **stays caller-owned**. The latch holds only the window — the kernel-owns-only-
uncontended-state boundary the crate root documents: a dirty flag poked by every mutation site is
shared, concurrently-written caller state, and two copies of one signal drift.

The existing `Arc<Notify>` wake seam (`:1887` sets the flag and notifies; the select loop re-arms at
`:2634`) already delivers what `mark_dirty` needs — no new plumbing.

`settle_publish` changes signature from `was_dirty: bool` to `claim: PublishClaim`, matching
`fleet_sync`'s and closing the two-vocabularies gap. The ZEB-761 `wake_at` min-selection between
`latch.deadline()` and `retry.pending_at()` (cf. `fleet_sync.rs:779`) carries over unchanged, as does
the arm guard `if wake_at.is_some()`.

## Error handling

One genuinely new branch: `Admission::Echo`. Today a self-echoed publish is rejected because it is
not strictly newer than the watermark `next_hlc` just recorded — right outcome, wrong reason. `admit`
distinguishes the two.

`Echo` maps to the same ignore-outcome as `Duplicate` but takes a quieter log level: a replay is
worth noticing, a transport loopback is not. `Echo` fires **only for our own device** — a publish
from the same owner's *other* device is a normal peer, because the key includes `device_id`.

`Admission::Duplicate` maps onto the existing `:3701` reject path with no change in behaviour.

## Testing

**The gate:** both byte-pin fixture files — `tests/wire_format/community_sync_fixtures.rs` and
`community_fixtures.rs` — stay green with **zero regeneration**. A regenerated fixture in this PR is
a failure, not a fix.

New tests:

1. **Apply-before-advance:** the watermark does not advance when the receive pipeline rejects
   mid-window. Now type-enforced, but pin the behaviour so a future refactor that reintroduces a
   bare map is caught.
2. **Saturation ties:** a writer at `logical == u32::MAX` produces a tick equal to its previous
   stamp, and `observe_local` reports `false`. Pins the deliberate behaviour change.
3. **Echo:** a publish whose source equals `local` is classified `Echo`, not `Duplicate`.
4. **Round-trip:** DTO → `from_accepted` → `accepted()` → DTO is byte-identical.

Each new test needs a **negative control** — deleting the corresponding fix must make it fail — and
the control must be recorded in the PR body. The two ZEB-761 tests
(`a_failed_community_publish_retries_itself_on_a_quiescent_community_zeb761`,
`a_persistently_failing_community_publish_paces_its_retries_zeb761`) must stay green untouched.

Gates: `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --features test-fixtures
--no-deps -- -D warnings`, `cargo nextest run --locked --workspace --all-targets --features
test-fixtures`.

## Risks

1. **Ticket threading may force pipeline restructuring** across `:3701`–`:4055`. This is the risk
   that could make the PR materially larger than it looks. If the restructuring grows past the
   convergence itself, stop and split: land replay admission alone, defer HLC and debounce.
2. **The saturation change is a real behaviour change.** Deliberate and approved, but it belongs in
   the PR body prominently.
3. **`Hlc` ↔ `HlcTick` conversion** must preserve the locked wire field order.

## Sequencing

One PR, three commits in dependency order — replay admission, then HLC tick, then debounce — so that
the HLC commit's diff visibly rests on the assertion removed by the replay commit. Bundled per the
one-PR-per-repo rule; no core PR is needed.
