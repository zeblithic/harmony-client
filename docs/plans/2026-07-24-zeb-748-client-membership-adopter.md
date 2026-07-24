# ZEB-748 Phase 6a — Client membership adopter (VerifiedLog) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Refactor `CommunityState` (client `community_state_crdt.rs`) to hold a `harmony_crdt_sync::verified_log::VerifiedLog<MembershipPolicy>` in place of its hand-rolled `events` `BTreeMap` + `insert_event` mechanics, proving the core VerifiedLog engine (merged in harmony#294) with one real, byte-pinned adopter.

**Architecture:** `MembershipPolicy` implements core's `LogPolicy` by delegating to the *unchanged* membership functions (`verify_event`, `materialize_with_now`, `event_sort_key`). `CommunityState` keeps every field except `events`, which becomes a private `log: VerifiedLog<MembershipPolicy>` serialized byte-identically through a `#[serde(with=…)]` shim. `insert_event` collapses to a thin wrapper over `self.log.insert(...)` that fires the existing client-side side effects (cache version bump + `admin_quorum` re-derive) only on `Inserted`. All 138 external `.events` field accesses migrate to a small accessor set first (accessor-first refactor: every intermediate task compiles and every wire fixture stays green), then the field's backing is flipped in one final task.

**Tech Stack:** Rust (edition 2021, MSRV 1.85), serde + ciborium (canonical CBOR), `harmony-crdt-sync` (core, rev R = `43186a2`).

## Global Constraints

- **Byte-transparency is non-negotiable.** Every existing CommunityState wire/persist fixture MUST stay green with **ZERO regeneration**: `tests/wire_format/zeb285_fixtures.rs`, `tests/wire_format/zeb250_fixtures.rs`, the in-module tests in `src/community_state_crdt.rs`, and `tests/community_sync/community_state_persist_unit.rs`. The `"ev"` CBOR field name, position, and the `BTreeMap<EventId, SignedMembershipEvent>` encoding must not change. (`community_fixtures.rs`/`community_sync_fixtures.rs` pin a *different* type — `MaterializedCommunityState` — leave them be.)
- **Behavior preservation.** `insert_event` must produce identical `InsertOutcome` results and identical side effects. The insert-time prior state is materialized with `now = Some(candidate.at.wall_ms)` (the R4-6 now-floor) — NOT `None`. `MembershipPolicy::materialize` MUST reproduce this via the per-insert Context.
- **No core changes.** Core `harmony-crdt-sync` is frozen at rev R; everything lands client-side. Production API stays clean: `insert_event` (verifying) + serde `from_verified_events` (trusted load) only. All trusted-write seams are `#[cfg(any(test, feature = "test-fixtures"))]`-gated (production never direct-populates — recon Category 4).
- **Exact-site inventories** live in `/private/tmp/claude-501/-Users-zeblith-work/2500b964-6db1-47fa-a7cc-9550e0f242e2/scratchpad/z748-client-recon-blastradius.md` (all 138 `.events` sites, file:line) and `z748-client-recon-membership.md` (membership fn signatures). Implementers: read the relevant inventory section for your task's exact site list.
- **Gates** (run from `src-tauri/`): `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --features test-fixtures` (scoped per task via `-E 'test(...)'` or `-p harmony-app`; full `--workspace --all-targets` only at the final gate). Always `--locked` and `--features test-fixtures`.

---

### Task 1: Lockstep pin bump R1→R — ALREADY COMPLETE (commit `4d4f06c7`)

Done before this plan: the 13-crate harmony lockstep group bumped `31ec347`→`43186a2` in `src-tauri/Cargo.toml`; `Cargo.lock` updated surgically (git-source strings only, no registry churn); `cargo check --locked --workspace --features test-fixtures` green. No action — listed for ledger continuity.

---

### Task 2: `MembershipPolicy` + `MembershipInsertCtx` (core-adopter glue)

**Files:**
- Modify: `src/community_state_crdt.rs` (add the policy + Context + a `mod policy_tests`)
- Modify: `src/community_membership.rs` (add `#[derive(Clone, Copy)]` to `VerifyContext`; ensure `event_sort_key` and the membership fns the policy calls are `pub(crate)`-visible from `community_state_crdt.rs`)

**Interfaces:**
- Consumes (from core, rev R): `harmony_crdt_sync::verified_log::{LogPolicy, VerifiedLog, InsertOutcome}`.
- Consumes (membership, unchanged): `verify_event(&SignedMembershipEvent, &MaterializedMembership, &VerifyContext) -> Result<(), VerifyError>`; `materialize_with_now(&[SignedMembershipEvent], OwnerAddr, Option<u64>) -> MaterializedMembership`; `event_sort_key(&SignedMembershipEvent) -> (u64,u64,DeviceId,EventId,[u8;64])` (or its actual tuple type — confirm at `community_membership.rs:2250`); types `EventId`, `SignedMembershipEvent`, `MaterializedMembership`, `VerifyContext`, `VerifyError`.
- Produces (for Task 7): `struct MembershipPolicy;` implementing `LogPolicy`, and `pub(crate) struct MembershipInsertCtx { pub verify: VerifyContext, pub now_floor_ms: u64 }`.

- [ ] **Step 1: Make `VerifyContext` cloneable.** In `src/community_membership.rs` at the `VerifyContext` definition (~:3930), add `#[derive(Clone, Copy)]`. Its three fields are `expected_community_id: SpaceId`, `admin_addr: OwnerAddr`, `is_invite_only: bool`. If `OwnerAddr`/`SpaceId` are not `Copy`, use `#[derive(Clone)]` only and adjust the Context to hold a `VerifyContext` by value (clone at the one construction site). Run `cargo check --features test-fixtures` to confirm the derive compiles.

- [ ] **Step 2: Ensure comparator + fns are reachable.** Confirm `event_sort_key`, `verify_event`, `materialize_with_now` are at least `pub(crate)`. If `event_sort_key` is private (`fn` not `pub(crate) fn`), widen it to `pub(crate)`. (No behavior change.)

- [ ] **Step 3: Write the failing policy unit test.** In `src/community_state_crdt.rs`, add a `#[cfg(test)] mod policy_tests` that builds a `VerifiedLog<MembershipPolicy>`, inserts a valid bootstrap event (Inserted), re-inserts the same id (AlreadyKnown, no re-verify), and inserts an event that `verify_event` rejects (Rejected). Use the existing test helpers that other tests in this crate use to fabricate signed events (search `community_state_crdt_unit.rs` / `community_membership.rs` test helpers for an event builder). Assert `core::InsertOutcome` variants.

```rust
// sketch — adapt event construction to the crate's existing signed-event test helper
#[test]
fn membership_policy_insert_dedup_reject() {
    let ctx = /* MembershipInsertCtx from a VerifyContext + now_floor_ms = event.at.wall_ms */;
    let mut log: VerifiedLog<MembershipPolicy> = VerifiedLog::new();
    assert_eq!(log.insert(bootstrap_event.clone(), &ctx), harmony_crdt_sync::verified_log::InsertOutcome::Inserted);
    assert_eq!(log.insert(bootstrap_event.clone(), &ctx), harmony_crdt_sync::verified_log::InsertOutcome::AlreadyKnown);
    // an event whose verify_event fails against empty prior:
    assert!(matches!(log.insert(unauthorized_event, &ctx), harmony_crdt_sync::verified_log::InsertOutcome::Rejected(_)));
}
```

- [ ] **Step 4: Run it — expect FAIL** (MembershipPolicy undefined): `cargo nextest run --locked --features test-fixtures -E 'test(membership_policy_insert_dedup_reject)'`.

- [ ] **Step 5: Implement `MembershipInsertCtx` + `MembershipPolicy`.** In `src/community_state_crdt.rs`:

```rust
use harmony_crdt_sync::verified_log::LogPolicy;
use core::cmp::Ordering;

/// Per-insert policy context. `now_floor_ms` is the candidate event's own
/// `at.wall_ms`, threaded so the prior-state materialization ages out
/// time-driven state exactly as `prior_state_at_event` does today (R4-6).
pub(crate) struct MembershipInsertCtx {
    pub verify: VerifyContext,
    pub now_floor_ms: u64,
}

pub(crate) struct MembershipPolicy;

impl LogPolicy for MembershipPolicy {
    type Event = SignedMembershipEvent;
    type EventId = EventId;
    type State = MaterializedMembership;
    type Context = MembershipInsertCtx;
    type Error = VerifyError;

    fn event_id(e: &SignedMembershipEvent) -> EventId { e.id }

    fn cmp(a: &SignedMembershipEvent, b: &SignedMembershipEvent) -> Ordering {
        event_sort_key(a).cmp(&event_sort_key(b))
    }

    fn verify(e: &SignedMembershipEvent, prior: &MaterializedMembership, ctx: &MembershipInsertCtx)
        -> Result<(), VerifyError> {
        verify_event(e, prior, &ctx.verify)
    }

    fn materialize(events: &[&SignedMembershipEvent], ctx: &MembershipInsertCtx) -> MaterializedMembership {
        // Core hands events in unspecified order; membership `materialize_with_now`
        // sorts internally by event_sort_key, so order-in is irrelevant.
        // now = Some(candidate.wall_ms) reproduces prior_state_at_event's R4-6 floor.
        let owned: Vec<SignedMembershipEvent> = events.iter().map(|e| (*e).clone()).collect();
        materialize_with_now(&owned, ctx.verify.admin_addr, Some(ctx.now_floor_ms))
    }
}
```

Adjust imports at the top of `community_state_crdt.rs` to bring in `event_sort_key` and `materialize_with_now` from `crate::community_membership`.

- [ ] **Step 6: Run policy test — expect PASS.** `cargo nextest run --locked --features test-fixtures -E 'test(membership_policy)'`.

- [ ] **Step 7: Gates.** `cargo fmt --all`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` (scoped build ok). Expect green — this task is purely additive; `CommunityState` is untouched, all existing fixtures unaffected.

- [ ] **Step 8: Commit.**
```bash
git add src-tauri/src/community_state_crdt.rs src-tauri/src/community_membership.rs
git commit -m "feat(zeb-748): add MembershipPolicy: LogPolicy adopter glue

MembershipPolicy delegates to unchanged membership verify_event/
materialize_with_now/event_sort_key. MembershipInsertCtx threads the
candidate's wall_ms as the R4-6 now-floor so insert-time prior state
matches prior_state_at_event. VerifyContext gains Clone/Copy. Additive:
CommunityState still uses its BTreeMap; no fixture affected."
```

---

### Task 3: Read accessors + trusted-write seams (delegating to the still-present `events` field)

**Files:**
- Modify: `src/community_state_crdt.rs` (add accessor methods delegating to `self.events`)
- Modify: `src/community_membership.rs` (`count_signers` + `plan_admin_proposal_auto_exec` signatures → accept an iterator instead of `&BTreeMap`)

**Interfaces:**
- Produces (used by Tasks 4–6): `pub fn events(&self) -> impl Iterator<Item = &SignedMembershipEvent>`; `pub fn get_event(&self, id: &EventId) -> Option<&SignedMembershipEvent>`; `pub fn contains_event(&self, id: &EventId) -> bool`; `pub fn event_count(&self) -> usize`; `pub fn events_is_empty(&self) -> bool`; `pub fn into_events(self) -> Vec<SignedMembershipEvent>`; and `#[cfg(any(test, feature = "test-fixtures"))] pub fn insert_verified_for_test(&mut self, e: SignedMembershipEvent)` + `#[cfg(any(test, feature = "test-fixtures"))] pub fn set_event_log_for_test(&mut self, events: BTreeMap<EventId, SignedMembershipEvent>)`.
- `count_signers` becomes `pub(crate) fn count_signers<'a>(events: impl Iterator<Item = &'a SignedMembershipEvent>, …) -> …` (keep the rest of its signature/body). `plan_admin_proposal_auto_exec` likewise takes `impl Iterator<Item = &SignedMembershipEvent>` where it currently takes `&BTreeMap`.

- [ ] **Step 1: Add the read accessors** to `impl CommunityState`, each delegating to `self.events` (field unchanged):
```rust
pub fn events(&self) -> impl Iterator<Item = &SignedMembershipEvent> { self.events.values() }
pub fn get_event(&self, id: &EventId) -> Option<&SignedMembershipEvent> { self.events.get(id) }
pub fn contains_event(&self, id: &EventId) -> bool { self.events.contains_key(id) }
pub fn event_count(&self) -> usize { self.events.len() }
pub fn events_is_empty(&self) -> bool { self.events.is_empty() }
pub fn into_events(self) -> Vec<SignedMembershipEvent> { self.events.into_values().collect() }
```

- [ ] **Step 2: Add the gated trusted-write seams** (test/bootstrap only):
```rust
#[cfg(any(test, feature = "test-fixtures"))]
pub fn insert_verified_for_test(&mut self, e: SignedMembershipEvent) {
    self.events.insert(e.id, e);
    self.cache.lock().expect("cache mutex poisoned").version += 1;
}
#[cfg(any(test, feature = "test-fixtures"))]
pub fn set_event_log_for_test(&mut self, events: BTreeMap<EventId, SignedMembershipEvent>) {
    self.events = events;
    self.cache.lock().expect("cache mutex poisoned").version += 1;
}
```
(The version bumps mirror what direct callers relied on `materialized()` re-materializing after; harmless for the pre-flip field, required after the flip.)

- [ ] **Step 3: Convert `count_signers` to an iterator param.** In `src/community_membership.rs`, change `count_signers(events: &BTreeMap<EventId, SignedMembershipEvent>, …)` to take `impl Iterator<Item = &SignedMembershipEvent>`; inside, replace `events.values()` with the passed iterator (or `events` directly). Update its existing internal callers (if any within membership) to pass `.values()`/`.iter()`. Do the same for `plan_admin_proposal_auto_exec`'s `&state_g.events` parameter.

- [ ] **Step 4: Build + regression gate.** `cargo check --locked --features test-fixtures`; then `cargo nextest run --locked --features test-fixtures -p harmony-app -E 'test(community)'`. Everything still compiles (accessors are additive; the `events` field is still public so un-migrated sites are unaffected) and all community fixtures stay green.

- [ ] **Step 5: Commit.**
```bash
git add src-tauri/src/community_state_crdt.rs src-tauri/src/community_membership.rs
git commit -m "refactor(zeb-748): add CommunityState event accessors + iterator count_signers

Additive read accessors (events/get_event/contains_event/event_count/
events_is_empty/into_events) + cfg-gated trusted-write seams, all
delegating to the existing BTreeMap. count_signers/plan_admin_proposal_auto_exec
take an iterator so callers need not touch the raw map. No behavior change."
```

---

### Task 4: Migrate production `src/` read sites to accessors

**Files (exact sites in blast-radius recon §B):**
- Modify: `src/community_state_sync.rs` (10 reads: 1800, 2072, 2159, 2321, 2488, 3543, 3705, 3820, 3864, 5647)
- Modify: `src/community_membership.rs` (:6239 — now passes an iterator to the Task-3 `plan_admin_proposal_auto_exec`)
- Modify: `src/community_fork.rs` (:351), `src/community_invite.rs` (:2197), `src/iroh_invite_acceptor.rs` (:399, :556)

**Mapping (mechanical):** `X.events.values()` → `X.events()`; `X.events.values().cloned().collect()` → `X.events().cloned().collect()`; `X.events.get(&id)` → `X.get_event(&id)`; `X.events.contains_key(&id)` → `X.contains_event(&id)`; `remote.events.into_values().collect()` → `remote.into_events()` (:3705, consumes); `&state_g.events` (:6239) → `state_g.events()`.

- [ ] **Step 1:** Apply the mapping at each site listed above. `X` is the `CommunityState` (behind a `state_g`/`g`/`state`/`remote`/`s` guard or ref — verify each receiver is a `CommunityState`).
- [ ] **Step 2: Build.** `cargo check --locked --features test-fixtures`. Fix any type mismatches (e.g. an iterator needed where a `Vec` was — add `.collect()`).
- [ ] **Step 3: Regression.** `cargo nextest run --locked --features test-fixtures -p harmony-app -E 'test(community) or test(iroh_invite) or test(fork)'`. Green.
- [ ] **Step 4: Commit** (`refactor(zeb-748): migrate production src .events reads to accessors`).

---

### Task 5: Migrate `src/lib.rs` sites to accessors

**Files (exact sites in blast-radius recon §C):**
- Modify: `src/lib.rs` — 34 reads + 7 writes.

**Read mapping:** as Task 4, plus `g.events.clone()` (:27572) → `g.events().cloned().collect::<std::collections::BTreeMap<_,_>>()` *only if a map is needed* — inspect the use; if it just needs the events, prefer `g.events().cloned().collect::<Vec<_>>()`. `for ev in st.events.values()` (:8204) → `for ev in st.events()`. `.events.len()` → `.event_count()`; `.events.is_empty()` → `.events_is_empty()`; `count_signers(&g.events, …)` (45148/45179/45218, 74076/74109/74139/74172/74216/74245) → `count_signers(g.events(), …)` (or `state.events()`).

**Write mapping (all test/bootstrap — confirm each is under `#[cfg(test)]` or a test-only fn; the trusted seams are cfg-gated so they resolve in test builds):** `st.events.insert(x.id, x)` (34486/34487, 73767/73842, 38907/38909) → `st.insert_verified_for_test(x)`; `on_disk.events = events` (38548) → `on_disk.set_event_log_for_test(events)`.

- [ ] **Step 1:** Apply read mapping to the 34 read sites. Use the recon line numbers; line numbers drift as you edit, so anchor edits on the surrounding code, not raw line numbers.
- [ ] **Step 2:** Apply write mapping to the 7 write sites. If any write site is NOT in a test/`cfg(test)` context, STOP and report it (recon says production never direct-populates; a prod write would be a real finding requiring the controller's decision).
- [ ] **Step 3: Build.** `cargo check --locked --features test-fixtures` and `cargo check --locked --features test-fixtures --all-targets` (lib.rs has both prod + `#[cfg(test)]` sites).
- [ ] **Step 4: Regression.** `cargo nextest run --locked --features test-fixtures -p harmony-app -E 'test(community) or test(recovery) or test(voting)'` (lib.rs sites span these areas). Green.
- [ ] **Step 5: Commit** (`refactor(zeb-748): migrate lib.rs .events sites to accessors`).

---

### Task 6: Migrate `tests/` + `community_state_sync` in-file test sites

**Files (exact sites in blast-radius recon §D, §E):**
- Modify (in-file tests): `src/community_state_sync.rs` (6 reads: 6253, 6333, 7612, 7651, 7891, 7966; 4 writes: 6245, 6320, 7952, 7953)
- Modify (tests/): `tests/community_sync/community_sync_integration.rs` (12 R + W 525), `community_fork_integration.rs` (13 R), `community_open_flow_integration.rs` (11 R), `community_state_crdt_unit.rs` (5 R), `community_state_persist_unit.rs` (3 R), `community_sync_engine_unit.rs` (1 R); `tests/community_misc/community_pending_join_integration.rs` (10 R), `community_invite_only_integration.rs` (2 R), `community_backward_secrecy_integration.rs` (2 R); `tests/misc/community_open_join_cross_wan_integration.rs` (7 R); `tests/pkarr_net/pkarr_iroh_redeem_full_integration.rs` (5 R).

**Mapping:** reads as Tasks 4–5; writes (`X.events.insert(id, ev)`, `lock().await.events.insert(...)`, `bad_state.events.insert(...)`) → `X.insert_verified_for_test(ev)`. Test crates compile against the public API with `--features test-fixtures`, so the cfg-gated seams are visible.

- [ ] **Step 1:** Migrate `src/community_state_sync.rs` in-file test sites (§D).
- [ ] **Step 2:** Migrate the `tests/` read sites (§E) — anchor on surrounding code, not line numbers.
- [ ] **Step 3:** Migrate the `tests/` write site (community_sync_integration.rs:525) → `insert_verified_for_test`.
- [ ] **Step 4: Build all targets.** `cargo check --locked --features test-fixtures --all-targets`. Green.
- [ ] **Step 5: Regression.** `cargo nextest run --locked --features test-fixtures -E 'test(community) or test(pkarr) or test(open_join)'`. Green (fixtures unchanged; field still `BTreeMap`).
- [ ] **Step 6: Commit** (`refactor(zeb-748): migrate test .events sites to accessors`).

**Checkpoint after Task 6:** ALL 138 external + the in-file test sites now use accessors. The `events` field is referenced ONLY inside `impl CommunityState` (the 9 sites in §A) and the accessor bodies. The field can now be flipped.

---

### Task 7: The flip — `events: BTreeMap` → private `log: VerifiedLog<MembershipPolicy>` + serde shim

**Files:**
- Modify: `src/community_state_crdt.rs` (the whole struct + impl)

**Interfaces:**
- Consumes: `MembershipPolicy`, `MembershipInsertCtx` (Task 2); the accessor signatures (Task 3) — only their *bodies* change here.
- Produces: `CommunityState` backed by `VerifiedLog`, byte-identical wire form.

- [ ] **Step 1: Add the serde shim module.** In `src/community_state_crdt.rs`:
```rust
mod membership_log_serde {
    use super::*;
    use serde::{Deserializer, Serializer, Deserialize};
    use serde::ser::SerializeMap;
    use std::collections::BTreeMap;

    pub fn serialize<S: Serializer>(log: &VerifiedLog<MembershipPolicy>, s: S) -> Result<S::Ok, S::Error> {
        // Rebuild the exact BTreeMap<EventId, &SignedMembershipEvent> the derive
        // used to emit for `events`. Keyed by event id, iterated in id order →
        // byte-identical CBOR map.
        let map: BTreeMap<EventId, &SignedMembershipEvent> =
            log.events().map(|e| (e.id, e)).collect();
        let mut m = s.serialize_map(Some(map.len()))?;
        for (k, v) in map { m.serialize_entry(&k, v)?; }
        m.end()
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<VerifiedLog<MembershipPolicy>, D::Error> {
        let map = BTreeMap::<EventId, SignedMembershipEvent>::deserialize(d)?;
        Ok(VerifiedLog::from_verified_events(map.into_values()))
    }
}
```
NOTE: The `serialize_map` + `serialize_entry` path must reproduce exactly what `#[derive(Serialize)] BTreeMap<EventId, SignedMembershipEvent>` emitted. If in doubt, serialize the owned `BTreeMap<EventId, SignedMembershipEvent>` directly (`map.serialize(s)` after cloning values) and optimize later — byte-identity is the hard requirement, not zero-copy. The `community_state_forked_from_cbor_skip` in-module test + zeb285/zeb250 fixtures are the proof.

- [ ] **Step 2: Swap the field.** Replace lines 101–105 (the `events` field) with:
```rust
    /// Append-only signed event log, verified on insert. Serialized
    /// byte-identically to the legacy `BTreeMap<EventId, SignedMembershipEvent>`
    /// under CBOR field "ev" via `membership_log_serde`.
    #[serde(rename = "ev", with = "membership_log_serde")]
    log: VerifiedLog<MembershipPolicy>,
```

- [ ] **Step 3: Rewrite accessor bodies** to delegate to `self.log`:
```rust
pub fn events(&self) -> impl Iterator<Item = &SignedMembershipEvent> { self.log.events() }
pub fn get_event(&self, id: &EventId) -> Option<&SignedMembershipEvent> { self.log.get(id) }
pub fn contains_event(&self, id: &EventId) -> bool { self.log.contains(id) }
pub fn event_count(&self) -> usize { self.log.len() }
pub fn events_is_empty(&self) -> bool { self.log.is_empty() }
pub fn into_events(self) -> Vec<SignedMembershipEvent> { self.log.events().cloned().collect() }
```
And the gated write seams:
```rust
#[cfg(any(test, feature = "test-fixtures"))]
pub fn insert_verified_for_test(&mut self, e: SignedMembershipEvent) {
    let mut evs: Vec<SignedMembershipEvent> = self.log.events().cloned().collect();
    evs.push(e);
    self.log = VerifiedLog::from_verified_events(evs);
    self.cache.lock().expect("cache mutex poisoned").version += 1;
}
#[cfg(any(test, feature = "test-fixtures"))]
pub fn set_event_log_for_test(&mut self, events: BTreeMap<EventId, SignedMembershipEvent>) {
    self.log = VerifiedLog::from_verified_events(events.into_values());
    self.cache.lock().expect("cache mutex poisoned").version += 1;
}
```

- [ ] **Step 4: Rewrite `insert_event`** (lines 305–340) to delegate to the engine:
```rust
pub fn insert_event(&mut self, event: SignedMembershipEvent, ctx: &VerifyContext) -> InsertOutcome {
    use harmony_crdt_sync::verified_log::InsertOutcome as CoreOutcome;
    let policy_ctx = MembershipInsertCtx { verify: *ctx, now_floor_ms: event.at.wall_ms };
    match self.log.insert(event, &policy_ctx) {
        CoreOutcome::AlreadyKnown => InsertOutcome::AlreadyKnown,
        CoreOutcome::Rejected(e) => InsertOutcome::Rejected(e),
        CoreOutcome::Inserted => {
            self.cache.lock().expect("cache mutex poisoned").version += 1;
            let derived = self.materialize_now(ctx.admin_addr).admin_quorum;
            self.admin_quorum = derived;
            InsertOutcome::Inserted
        }
    }
}
```
(If `VerifyContext` is `Clone` not `Copy`, use `verify: ctx.clone()`. `event.at.wall_ms` is read before `event` moves into `insert`.)

- [ ] **Step 5: Rewrite the materialize accessors** (281, 347, 369) to collect from `self.log`:
  - `materialized` (:281 body): `let log: Vec<SignedMembershipEvent> = self.log.events().cloned().collect();` then `materialize(&log, admin_addr)` as before.
  - `materialize_now` (:347): same collect + `materialize(&log, admin_addr)`.
  - `materialized_with_now` (:369): same collect + `materialize_with_now(&log, admin_addr, Some(now_ms))`.
  - `materialized` bootstrap-hint guard (:271): `self.events.is_empty()` → `self.log.is_empty()`.

- [ ] **Step 6: Rewrite `Clone`, `PartialEq`, `new`:**
  - `Clone` (:160): `events: self.events.clone()` → `log: VerifiedLog::from_verified_events(self.log.events().cloned())`.
  - `PartialEq` (:177): `self.events == other.events` → compare event sets, e.g. `self.log.len() == other.log.len() && self.log.events().eq(other.log.events())` (both iterate id-order, so `Iterator::eq` is a byte-for-byte set equality). Confirm `SignedMembershipEvent: PartialEq` (recon: yes).
  - `new` (:210): `events: BTreeMap::new()` → `log: VerifiedLog::new()`.

- [ ] **Step 7: Fix imports.** Remove now-unused `BTreeMap` import if the struct no longer names it (the serde shim + seams still use it — keep as needed). Ensure `VerifiedLog` is imported.

- [ ] **Step 8: Build.** `cargo check --locked --features test-fixtures --all-targets`. This is where any missed accessor migration surfaces as a compile error — fix by routing through an accessor.

- [ ] **Step 9: BYTE-TRANSPARENCY GATE (the crux).** Run, and require ALL green with zero fixture edits:
```
cargo nextest run --locked --features test-fixtures -E 'test(zeb285) or test(zeb250) or test(community_state_forked_from) or test(community_state_persist) or test(community_state_crdt)'
```
If any wire fixture fails, the serde shim is not byte-identical — DO NOT regenerate fixtures. Debug the shim (Step 1) until bytes match. The most likely culprit is the map serialization path; fall back to serializing an owned `BTreeMap<EventId, SignedMembershipEvent>` if the borrowed-entry path differs.

- [ ] **Step 10: Full community regression.** `cargo nextest run --locked --features test-fixtures -E 'test(community) or test(pkarr) or test(recovery) or test(voting)'`. Green.

- [ ] **Step 11: Commit** (`feat(zeb-748): back CommunityState with VerifiedLog<MembershipPolicy>`). Describe the serde shim + byte-transparency proof + insert_event delegation.

---

### Task 8: Final full-gate verification + PR readiness

**Files:** none (verification only) unless a gate surfaces a fix.

- [ ] **Step 1: fmt.** `cargo fmt --all -- --check` (use `${pipestatus}`/no pipe-masking).
- [ ] **Step 2: clippy.** `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`. Fix any `dead_code`/`needless_collect`/unused-import.
- [ ] **Step 3: Full test sweep (CI-parity).** `cargo nextest run --locked --workspace --all-targets --features test-fixtures`. (Long; supervise with a wall-clock net. macOS: ensure XprotectService dev-mode is enabled per CLAUDE.md.) All green, with EVERY membership/CommunityState wire fixture passing with zero regeneration.
- [ ] **Step 4: Frontend gate (unaffected but CI-parity).** From repo root: `npx tsc --noEmit` and `npx vitest run` — should be untouched by this Rust-only change; run to confirm.
- [ ] **Step 5: Confirm zero fixture diff.** `git diff --stat` must show NO changes under `tests/wire_format/`. If any fixture file changed, byte-transparency was violated — revert and fix Task 7.
- [ ] **Step 6:** Ready for PR (opened by the controller, not here).

---

## Self-Review notes (controller)

- **Spec coverage:** DoD item 1 (core VerifiedLog) landed in #294. DoD item 2 (CommunityState on VerifiedLog, fixtures zero-regen, side effects preserved) = Tasks 2–8. DoD item 3 (both repos' gates) = Task 8 + the already-green core.
- **The now-floor correction** (prior state materialized with `Some(candidate.wall_ms)`, not `None`) is baked into Task 2 Step 5 and Task 7 Step 4 — the single subtlest behavior-preservation point.
- **Side-effect dispatch** (3 call sites branching on `InsertOutcome`) needs NO task: `insert_event` returns the identical client enum, so `insert_local_event`, the auto-counter-sign task, and `handle_incoming_publish` are untouched. `insert_local_event_pair`'s pre-validate-both atomicity is likewise untouched (it calls membership fns + `insert_event`, now routed through accessors/the engine identically).
- **Risk:** the serde shim byte-identity (Task 7 Step 9). Mitigation: the fixture gate + the owned-BTreeMap fallback.
