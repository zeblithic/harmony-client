# ZEB-217 Sub-C Phase 2: Per-Community State CRDT + Encrypted Zenoh Sync — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land the per-community state CRDT and the encrypted Zenoh state-root sync that replicates membership events across community members. Verification fires at receive time using the SAME `verify_event` + `prior_state_at_event` helpers Phase 1 ships, so author-side and receiver-side authorization can't drift. Per-community `RootHlcTracker` dedupes redundant roots; the dedupe-merge HLC monotonicity bug-class from PR #81 round 3 is gated from day 1. No IPC, no UI in Phase 2 — those land in Phases 3-5.

**Architecture:** Mirrors `src-tauri/src/owner_state_sync.rs` shape but multi-instance — one `CommunitySyncEngine` per joined community. `CommunityState` events are serialized as a single canonical-CBOR blob, encrypted with the community's `MembershipKey` (ChaCha20-Poly1305), stored in `ContentStore` (CAS) keyed by `harmony_content::cid::ContentId`, and the encrypted-root publish carries that `ContentId` + an HLC over `harmony/community/{id_hex}/state-root-v1`. Subscribers fetch the blob via existing CAS machinery, decrypt, deserialize the `CommunityState`, and merge events into local — running `verify_event` on every newly-arrived event before it reaches `materialize`. A new `CommunitySyncRegistry` owns the `BTreeMap<SpaceId, Arc<CommunitySyncEngine>>` and reacts to owner-state Space mutations (community Space added → spawn engine; `left_at` set or Space tombstoned → teardown).

**Tech Stack:** Rust 2024 edition; `serde` + `ciborium` for canonical CBOR (existing `canonical_cbor_encode` / `CanonicalPayload` machinery); `chacha20poly1305` for AEAD (same primitive as `dm_crypto::DmContentKey` and `owner_state_crypto`); `tokio` mpsc channels + `Notify` + `select!` for the engine task (identical structural pattern to `owner_state_sync::SyncEngine` at `src-tauri/src/owner_state_sync.rs:54-348`); existing `harmony_content::cid::ContentId` + `crate::content_store::RuntimeContentStore` for CAS DAG-sync.

**Spec:** [`docs/specs/2026-05-05-zeb-217-sub-c-communities-design.md`](../specs/2026-05-05-zeb-217-sub-c-communities-design.md) (refreshed at commit `5d8a666` against shipped Phase 1).

**Phase 1 reference:** PR #82, merge commit `bd1d01b`. Phase 1 ships the primitives this plan consumes: `verify_event`, `materialize`, `prior_state_at_event`, `event_sort_key`, `POWER_THRESHOLDS`, `SignedMembershipEvent`, `EventPayload`, `VerifyContext`, the 19 `VerifyError` variants, and the same-SpaceId community-creation field rejection in `owner_state_crdt::apply_space`.

**Branch:** `zeb-217-sub-c-phase2-state-crdt-sync` — branched from `origin/main` AFTER the Phase 2 docs PR (which carries this plan + the spec refresh + the Phase 1 plan archive on branch `zeb-217-sub-c-phase2-plan-and-spec-refresh`) has merged. Phase 2 implementation does NOT branch off the docs PR's branch; it branches off the new `origin/main` HEAD that contains the merged docs.

---

## File Structure

| File | Action | Responsibility | Approx. lines |
|---|---|---|---|
| `src-tauri/src/community_state_crdt.rs` | Create | `CommunityState` struct (events `BTreeMap<EventId, SignedMembershipEvent>`); `insert_event` (calls `verify_event` + `materialize` cache invalidation); `materialized()` (cached read); canonical CBOR | ~350 |
| `src-tauri/src/community_state_sync.rs` | Create | `CommunitySyncEngine` (debounced publish + subscribe handler + `handle_incoming_publish` + persist); `CommunityRootPublishPayload` wire type; AEAD helpers (`encrypt_root_publish` / `decrypt_root_publish` / `encrypt_blob` / `decrypt_blob`); `CommunityRootHlcTracker`; `CommunitySyncRegistry` | ~900 |
| `src-tauri/src/community_state_persist.rs` | Create | `save_crdt` / `load_crdt` / `save_replay` / `load_replay` per community (mirrors `owner_state_persist.rs`); per-community paths inside `identity_dir/communities/{id_hex}/` | ~180 |
| `src-tauri/src/event_loop.rs` | Modify | New `RuntimeEvent::SubscribeCommunityState` / `RuntimeEvent::PublishCommunityState` arms; `spawn_community_state_zenoh_adapter` helper that maps engine `out_rx` ↔ Zenoh publisher and Zenoh subscriber ↔ engine `in_tx` per community | ~180 net additions |
| `src-tauri/src/lib.rs` | Modify | `mod community_state_crdt; mod community_state_sync; mod community_state_persist;`; `start_node`: scan `owner_state.spaces` for `SpaceKind::Community` rows, spawn engines, hand the registry to `OwnerStateContext`; emit `community-state-sync-degraded` IPC events on subscriber failures | ~120 net additions |
| `src-tauri/tests/community_sync_integration.rs` | Create | Two-member round-trip (A publishes Join → B receives + materializes); degraded paths (decrypt failure, malformed wire bytes, replay rejection); subscription lifecycle (Space-add spawns, `left_at` sets teardown); HLC monotonicity preservation on dedupe-merge | ~600 |
| `src-tauri/tests/community_state_persist_unit.rs` | Create | Persist/load round-trip; corruption tolerance (truncated file → empty CRDT, not panic); per-community path isolation | ~200 |
| `src-tauri/tests/wire_format_community_sync_fixtures.rs` | Create | CBOR golden-byte fixtures for `CommunityRootPublishPayload`; AEAD-output regression fixtures (deterministic-key encrypt + ContentId derivation) | ~150 |

**Phase 2 scope boundary:** No IPC commands (Phase 3 ships `create_community` / `redeem_invite` / `leave_community` / `list_community_members`). No frontend. No Reticulum (Phase 4 ships invite-only counter-sig). No deep-link plugin (Phase 5). The Phase 2 PR ships a fully-working sync layer with NO user-visible surface — exercised entirely through integration tests.

**Sequencing note:** This plan assumes the Phase 2 docs PR (branch `zeb-217-sub-c-phase2-plan-and-spec-refresh`) has merged before Task 0 fires. If it has not, abort Task 0 and ask the user to merge the docs PR first — branching off a `main` that doesn't contain the refreshed spec means Phase 2 implementation cites stale terminology.

---

## Pre-flight (Task 0)

### Task 0: Branch off latest origin/main and verify baseline gates

**Files:** none modified — branch creation + verification only.

- [ ] **Step 0.1: Verify the docs PR has merged**

```bash
gh pr list --state merged --search "zeb-217-sub-c-phase2-plan-and-spec-refresh" --json number,mergedAt
```

Expected: one entry with a non-null `mergedAt`. If empty, abort and ask the user to merge the docs PR first.

- [ ] **Step 0.2: Pull latest origin/main and confirm baseline**

```bash
git fetch origin
git checkout main
git pull origin main
git log --oneline -5
ls docs/specs/2026-05-05-zeb-217-sub-c-communities-design.md docs/plans/2026-05-05-zeb-217-sub-c-phase1-membership-crdt-plan.md docs/plans/2026-05-05-zeb-217-sub-c-phase2-state-crdt-sync-plan.md
```

Expected: latest commit on `main` is the docs PR squash-merge; all three files exist (spec, Phase 1 plan archive, this plan). If any are missing, abort — the docs PR's tree is incomplete.

- [ ] **Step 0.3: Create the Phase 2 branch**

```bash
git checkout -b zeb-217-sub-c-phase2-state-crdt-sync
```

Expected: `Switched to a new branch 'zeb-217-sub-c-phase2-state-crdt-sync'`.

- [ ] **Step 0.4: Verify baseline gates green on the branch**

```bash
cd src-tauri
set -o pipefail
cargo fmt --all -- --check
echo "FMT_EXIT=${PIPESTATUS[0]}"
cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -5
echo "CLIPPY_EXIT=${PIPESTATUS[0]}"
cargo test --all-targets --all-features 2>&1 | grep "^test result:" | tail -5
echo "TEST_EXIT=${PIPESTATUS[0]}"
```

Expected: all three exit codes 0; test summary shows the post-Phase-1 baseline (707 passing).

If any gate fails on a fresh `origin/main`, that's test drift — file a Linear follow-up + fix on a separate branch BEFORE proceeding (per the "test drift is our fault" hard rule).

- [ ] **Step 0.5: Verify vitest + tsc baseline**

```bash
cd ..
set -o pipefail
npx vitest run 2>&1 | tail -5
echo "VITEST_EXIT=${PIPESTATUS[0]}"
npx tsc --noEmit
echo "TSC_EXIT=${PIPESTATUS[0]}"
```

Expected: vitest passes (1392 from Phase 1 baseline); tsc clean. Phase 2 has no frontend changes, but verifying baseline now means a green-or-not signal at the end of Phase 2 is unambiguous.

---

## Task 1: `CommunityState` struct skeleton + canonical CBOR round-trip

**Files:**
- Create: `src-tauri/src/community_state_crdt.rs`
- Modify: `src-tauri/src/lib.rs:31` (add `pub mod community_state_crdt;` after the existing `pub mod community_membership;` declaration)
- Create: `src-tauri/tests/community_state_crdt_unit.rs`

- [ ] **Step 1.1: Write the failing CBOR round-trip test**

Create `src-tauri/tests/community_state_crdt_unit.rs`:

```rust
//! Unit tests for community_state_crdt.rs Phase 2 types.

use harmony_app::community_membership::SignedMembershipEvent;
use harmony_app::community_state_crdt::CommunityState;
use harmony_app::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
use harmony_app::owner_state_types::SpaceId;

#[test]
fn empty_community_state_round_trips() {
    let s = CommunityState::new(SpaceId([1u8; 16]));
    let bytes = canonical_cbor_encode(&s).expect("encode");
    let decoded: CommunityState = canonical_cbor_decode(&bytes).expect("decode");
    assert_eq!(decoded.community_id, s.community_id);
    assert!(decoded.events.is_empty());
}
```

- [ ] **Step 1.2: Run test to verify it fails**

```bash
cd src-tauri
cargo test --test community_state_crdt_unit empty_community_state_round_trips 2>&1 | tail -10
```

Expected: `error[E0432]: unresolved import \`harmony_app::community_state_crdt\``.

- [ ] **Step 1.3: Create the module skeleton**

Create `src-tauri/src/community_state_crdt.rs`:

```rust
//! Per-community state CRDT — Phase 2 of ZEB-217 Sub-C.
//!
//! `CommunityState` holds the append-only signed event log for one
//! community, keyed by EventId. Mirrors the SHAPE of
//! `crate::owner_state_crdt::OwnerState` but at per-community
//! granularity — one `CommunityState` per joined community.
//!
//! Events arrive partial-ordered from DAG-sync; ordering for replay
//! is `event_sort_key` ascending. The materialized view (members +
//! power_levels) is computed on demand and cached with a version
//! counter that bumps on every successful insert.
//!
//! Wire format: canonical CBOR with the same-length-keys invariant
//! at this nesting level — both field codes (`ci` for community_id,
//! `ev` for events) are 2 chars.

use crate::community_membership::{EventId, SignedMembershipEvent};
use crate::owner_state_crypto::{CanonicalPayload, CanonicalPayloadSealed};
use crate::owner_state_types::SpaceId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommunityState {
    /// The community this state belongs to. Persisted in the wire form
    /// so that a misrouted blob (wrong file, wrong ContentStore key) is
    /// rejected at decode-time rather than silently materialized into
    /// the wrong community's view.
    #[serde(rename = "ci")]
    pub community_id: SpaceId,

    /// Append-only signed event log, keyed by EventId. BTreeMap (not
    /// HashMap) so iteration order is deterministic across replicas —
    /// canonical CBOR encoding requires a stable order.
    #[serde(rename = "ev")]
    pub events: BTreeMap<EventId, SignedMembershipEvent>,
}

impl CommunityState {
    pub fn new(community_id: SpaceId) -> Self {
        Self {
            community_id,
            events: BTreeMap::new(),
        }
    }
}

impl CanonicalPayloadSealed for CommunityState {}
impl CanonicalPayload for CommunityState {}
```

- [ ] **Step 1.4: Wire the module declaration**

Open `src-tauri/src/lib.rs` and locate the line `pub mod community_membership;` (around line 31). Add immediately after it:

```rust
pub mod community_state_crdt;
```

- [ ] **Step 1.5: Run the round-trip test**

```bash
cd src-tauri
cargo test --test community_state_crdt_unit empty_community_state_round_trips 2>&1 | tail -10
```

Expected: PASS (1 test).

- [ ] **Step 1.6: Run gates**

```bash
cd src-tauri
set -o pipefail
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -3
cargo test --test community_state_crdt_unit 2>&1 | grep "^test result:"
```

Expected: fmt clean, clippy clean, all tests pass.

- [ ] **Step 1.7: Commit**

```bash
git add src-tauri/src/community_state_crdt.rs src-tauri/src/lib.rs src-tauri/tests/community_state_crdt_unit.rs
git commit -m "$(cat <<'EOF'
feat(zeb-217-phase2): CommunityState skeleton + canonical CBOR

Per-community signed event log keyed by EventId. Mirrors the shape
of OwnerState but at per-community granularity. Round-trip test
locks in the wire form (ci/ev field codes both 2-char, same-length-
keys invariant at this nesting level).

Phase 2 of ZEB-217 Sub-C; consumes Phase 1's SignedMembershipEvent.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: `insert_event` — verify before insert + cache invalidation

**Files:**
- Modify: `src-tauri/src/community_state_crdt.rs`
- Modify: `src-tauri/tests/community_state_crdt_unit.rs`

- [ ] **Step 2.1: Write the failing insert-rejects-bad-sig test**

Append to `src-tauri/tests/community_state_crdt_unit.rs`:

```rust
use harmony_app::community_membership::{
    sign_event_with_identity, EventPayload, MembershipEventKind, VerifyContext, VerifyError,
};
use harmony_app::community_state_crdt::InsertOutcome;
use harmony_app::owner_state_types::{Hlc, OwnerAddr};
use harmony_identity::PrivateIdentity;

fn make_test_identity() -> (PrivateIdentity, [u8; 64], OwnerAddr) {
    let identity = PrivateIdentity::generate();
    let identity_pub = identity.identity.to_public_bytes();
    let addr = OwnerAddr(identity.identity.address_hash);
    (identity, identity_pub, addr)
}

fn hlc(wall_ms: u64) -> Hlc {
    Hlc {
        wall_ms,
        logical: 0,
        device_id: "d".into(),
    }
}

#[test]
fn insert_rejects_event_with_wrong_community() {
    let (identity, identity_pub, addr) = make_test_identity();
    let community_id = SpaceId([1u8; 16]);
    let other_community = SpaceId([2u8; 16]);

    let payload = EventPayload {
        id: [3u8; 16],
        community_id: other_community,
        kind: MembershipEventKind::Join,
        actor: addr,
        at: hlc(100),
    };
    let event = sign_event_with_identity(payload, &identity).expect("sign");

    let mut state = CommunityState::new(community_id);
    let outcome = state.insert_event(
        event,
        &VerifyContext {
            expected_community_id: community_id,
            admin_addr: addr,
            is_invite_only: false,
            actor_identity_pub: &identity_pub,
            countersigner_identity_pub: None,
        },
    );

    assert!(matches!(
        outcome,
        InsertOutcome::Rejected(VerifyError::WrongCommunity)
    ));
    assert!(state.events.is_empty(), "rejected event must not land in log");
}

#[test]
fn insert_accepts_admin_self_join_in_open_community() {
    let (identity, identity_pub, addr) = make_test_identity();
    let community_id = SpaceId([1u8; 16]);

    let payload = EventPayload {
        id: [3u8; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: addr,
        at: hlc(100),
    };
    let event = sign_event_with_identity(payload, &identity).expect("sign");
    let event_id = event.id;

    let mut state = CommunityState::new(community_id);
    let outcome = state.insert_event(
        event,
        &VerifyContext {
            expected_community_id: community_id,
            admin_addr: addr,
            is_invite_only: false,
            actor_identity_pub: &identity_pub,
            countersigner_identity_pub: None,
        },
    );

    assert!(matches!(outcome, InsertOutcome::Inserted));
    assert_eq!(state.events.len(), 1);
    assert!(state.events.contains_key(&event_id));
}

#[test]
fn insert_is_idempotent_on_duplicate_event_id() {
    let (identity, identity_pub, addr) = make_test_identity();
    let community_id = SpaceId([1u8; 16]);

    let payload = EventPayload {
        id: [3u8; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: addr,
        at: hlc(100),
    };
    let event = sign_event_with_identity(payload, &identity).expect("sign");

    let mut state = CommunityState::new(community_id);
    let ctx = VerifyContext {
        expected_community_id: community_id,
        admin_addr: addr,
        is_invite_only: false,
        actor_identity_pub: &identity_pub,
        countersigner_identity_pub: None,
    };
    assert!(matches!(
        state.insert_event(event.clone(), &ctx),
        InsertOutcome::Inserted
    ));
    assert!(matches!(
        state.insert_event(event, &ctx),
        InsertOutcome::AlreadyKnown
    ));
    assert_eq!(state.events.len(), 1);
}
```

- [ ] **Step 2.2: Run tests to verify they fail**

```bash
cd src-tauri
cargo test --test community_state_crdt_unit 2>&1 | tail -10
```

Expected: compile error — `insert_event` and `InsertOutcome` don't exist yet.

- [ ] **Step 2.3: Implement `InsertOutcome` + `insert_event`**

Append to `src-tauri/src/community_state_crdt.rs`:

```rust
use crate::community_membership::{
    materialize, prior_state_at_event, verify_event, MaterializedMembership, VerifyContext,
    VerifyError,
};

/// Outcome of inserting one event into `CommunityState`.
///
/// Distinguishes the three meaningful states so callers (sync layer,
/// IPC layer, tests) can react appropriately:
/// - Inserted: event was new, verified, and now lives in the log
/// - AlreadyKnown: an event with this id was already in the log; the
///   sync layer should treat this as a no-op (NOT an error — DAG-sync
///   delivers duplicates by design)
/// - Rejected: verification failed; the wrapped VerifyError says why
#[derive(Debug, PartialEq, Eq)]
pub enum InsertOutcome {
    Inserted,
    AlreadyKnown,
    Rejected(VerifyError),
}

impl CommunityState {
    /// Insert a `SignedMembershipEvent` after running `verify_event`
    /// against the current materialized state. The state used for
    /// authorization is computed via `prior_state_at_event` so the
    /// `event_sort_key` comparator is shared with `materialize` and
    /// no caller can drift.
    ///
    /// Idempotent on duplicate EventIds — DAG-sync delivers the same
    /// event multiple times by design (e.g., when a peer re-publishes
    /// a state-root that includes events we already have). Returning
    /// `AlreadyKnown` rather than `Inserted` lets callers skip the
    /// cache-invalidation work.
    pub fn insert_event(
        &mut self,
        event: SignedMembershipEvent,
        ctx: &VerifyContext,
    ) -> InsertOutcome {
        if self.events.contains_key(&event.id) {
            return InsertOutcome::AlreadyKnown;
        }

        // Build prior_state from the current event log. Note that we
        // pass the candidate event so prior_state_at_event filters
        // strictly less-than, not less-than-or-equal — without this
        // the candidate would self-authorize against its own future
        // state if it had already been inserted.
        let log: Vec<SignedMembershipEvent> = self.events.values().cloned().collect();
        let prior = prior_state_at_event(&log, &event, ctx.admin_addr);

        if let Err(e) = verify_event(&event, &prior, ctx) {
            return InsertOutcome::Rejected(e);
        }

        self.events.insert(event.id, event);
        InsertOutcome::Inserted
    }

    /// Materialize the current event log. Pure; no caching at this
    /// layer — callers that want a cached view should hold the
    /// `materialized()` result and invalidate on every successful
    /// `insert_event`. Task 3 adds the cache.
    pub fn materialize_now(&self, admin_addr: OwnerAddr) -> MaterializedMembership {
        let log: Vec<SignedMembershipEvent> = self.events.values().cloned().collect();
        materialize(&log, admin_addr)
    }
}
```

Also add to imports at the top:

```rust
use crate::owner_state_types::OwnerAddr;
```

- [ ] **Step 2.4: Run tests**

```bash
cd src-tauri
cargo test --test community_state_crdt_unit 2>&1 | grep "^test result:"
```

Expected: PASS (4 tests including the original round-trip).

- [ ] **Step 2.5: Run gates**

```bash
cd src-tauri
set -o pipefail
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -3
cargo test --all-targets --all-features 2>&1 | grep "^test result:" | tail -3
```

Expected: all green.

- [ ] **Step 2.6: Commit**

```bash
git add src-tauri/src/community_state_crdt.rs src-tauri/tests/community_state_crdt_unit.rs
git commit -m "$(cat <<'EOF'
feat(zeb-217-phase2): CommunityState::insert_event with verify gate

insert_event runs verify_event with prior_state computed via
prior_state_at_event — same comparator as materialize, no drift.
Idempotent on duplicate EventIds (DAG-sync delivers duplicates by
design). Rejected events do not enter the log.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Materialized-view cache with version counter

**Files:**
- Modify: `src-tauri/src/community_state_crdt.rs`
- Modify: `src-tauri/tests/community_state_crdt_unit.rs`

**Why a cache:** Phase 3 IPC `list_community_members` will be called per nav-tree render. Materializing from the full log on every call is O(events) — fine for small communities but wasteful when the same state is queried back-to-back. Mirrors the version-counter pattern from `inbox_entries_for_space` in DM transport (`src-tauri/src/dm_outbox.rs`).

- [ ] **Step 3.1: Write the failing cache-invalidation test**

Append to `src-tauri/tests/community_state_crdt_unit.rs`:

```rust
#[test]
fn materialized_cache_returns_same_object_until_insert() {
    let (identity, identity_pub, addr) = make_test_identity();
    let community_id = SpaceId([1u8; 16]);
    let mut state = CommunityState::new(community_id);

    let v0 = state.materialized_version();
    let _m1 = state.materialized(addr);
    let _m2 = state.materialized(addr);
    assert_eq!(state.materialized_version(), v0, "version unchanged on read");

    let payload = EventPayload {
        id: [3u8; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: addr,
        at: hlc(100),
    };
    let event = sign_event_with_identity(payload, &identity).expect("sign");
    state.insert_event(
        event,
        &VerifyContext {
            expected_community_id: community_id,
            admin_addr: addr,
            is_invite_only: false,
            actor_identity_pub: &identity_pub,
            countersigner_identity_pub: None,
        },
    );

    let v1 = state.materialized_version();
    assert!(v1 > v0, "version bumps on successful insert");

    let m_after = state.materialized(addr);
    assert_eq!(m_after.members.len(), 1, "Join event materialized");
}
```

- [ ] **Step 3.2: Run test to verify it fails**

```bash
cd src-tauri
cargo test --test community_state_crdt_unit materialized_cache 2>&1 | tail -10
```

Expected: compile error — `materialized` and `materialized_version` don't exist.

- [ ] **Step 3.3: Add the cache**

Modify `src-tauri/src/community_state_crdt.rs`. Update `CommunityState`:

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommunityState {
    #[serde(rename = "ci")]
    pub community_id: SpaceId,

    #[serde(rename = "ev")]
    pub events: BTreeMap<EventId, SignedMembershipEvent>,

    /// Materialized-view cache. Skipped from CBOR — derivable from
    /// `events` so persisting it would just inflate the wire form.
    /// Wrapped in a `OnceCell`-ish handle so reads don't take a
    /// mutable borrow.
    #[serde(skip)]
    cache: std::sync::Mutex<MaterializedCache>,
}

#[derive(Default, Debug)]
struct MaterializedCache {
    /// Bumps every time `events` mutates. Reads hand out a clone of
    /// the cached value if `cached_version == version`; otherwise
    /// re-materialize and update.
    version: u64,
    cached_version: Option<u64>,
    cached: Option<MaterializedMembership>,
}

impl PartialEq for CommunityState {
    fn eq(&self, other: &Self) -> bool {
        self.community_id == other.community_id && self.events == other.events
    }
}
impl Eq for CommunityState {}
```

Replace `CommunityState::new`:

```rust
impl CommunityState {
    pub fn new(community_id: SpaceId) -> Self {
        Self {
            community_id,
            events: BTreeMap::new(),
            cache: std::sync::Mutex::new(MaterializedCache::default()),
        }
    }

    /// Cache version counter. Bumps on every successful insert.
    /// Useful for IPC layers that want to short-circuit "did anything
    /// change?" checks across calls.
    pub fn materialized_version(&self) -> u64 {
        self.cache.lock().expect("cache mutex poisoned").version
    }

    /// Return the materialized view. Recomputes from `events` if the
    /// cache is stale (version mismatch); otherwise returns a clone
    /// of the cached value. The clone is intentional — handing out
    /// references would block `insert_event` callers behind reader
    /// holds, and MaterializedMembership is small (BTreeMaps of
    /// 16-byte addrs + small structs).
    pub fn materialized(&self, admin_addr: OwnerAddr) -> MaterializedMembership {
        let mut cache = self.cache.lock().expect("cache mutex poisoned");
        if cache.cached_version != Some(cache.version) {
            let log: Vec<SignedMembershipEvent> = self.events.values().cloned().collect();
            let m = materialize(&log, admin_addr);
            cache.cached = Some(m.clone());
            cache.cached_version = Some(cache.version);
            return m;
        }
        cache.cached.clone().expect("cached_version Some implies cached Some")
    }

    pub fn insert_event(
        &mut self,
        event: SignedMembershipEvent,
        ctx: &VerifyContext,
    ) -> InsertOutcome {
        if self.events.contains_key(&event.id) {
            return InsertOutcome::AlreadyKnown;
        }
        let log: Vec<SignedMembershipEvent> = self.events.values().cloned().collect();
        let prior = prior_state_at_event(&log, &event, ctx.admin_addr);
        if let Err(e) = verify_event(&event, &prior, ctx) {
            return InsertOutcome::Rejected(e);
        }
        self.events.insert(event.id, event);
        // Invalidate cache by bumping version. Lazy re-mat happens on
        // the next read.
        self.cache.lock().expect("cache mutex poisoned").version += 1;
        InsertOutcome::Inserted
    }

    pub fn materialize_now(&self, admin_addr: OwnerAddr) -> MaterializedMembership {
        let log: Vec<SignedMembershipEvent> = self.events.values().cloned().collect();
        materialize(&log, admin_addr)
    }
}
```

Note: `Mutex<MaterializedCache>` (not `std::cell::Cell` / `RefCell`) because `CommunityState` will be held in `Arc<Mutex<_>>` shared across the engine's tokio task and IPC callers. The inner cache lock is short-held — recompute work happens after the lock is released.

After the `Deserialize` derive runs, the `cache` field is initialized via `#[serde(skip)]`'s default. Add a `Default` impl for `MaterializedCache` to satisfy this (already present via `#[derive(Default)]`).

- [ ] **Step 3.4: Run tests**

```bash
cd src-tauri
cargo test --test community_state_crdt_unit 2>&1 | grep "^test result:"
```

Expected: PASS (5 tests).

- [ ] **Step 3.5: Run gates + commit**

```bash
cd src-tauri
set -o pipefail
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -3
cargo test --all-targets --all-features 2>&1 | grep "^test result:" | tail -3
```

```bash
git add src-tauri/src/community_state_crdt.rs src-tauri/tests/community_state_crdt_unit.rs
git commit -m "$(cat <<'EOF'
feat(zeb-217-phase2): materialized-view cache with version counter

Mirrors the inbox_entries_for_space cache pattern from DM transport.
Reads are cheap when the log hasn't changed; insert_event invalidates
by bumping the version. Mutex is short-held — recompute happens
after release.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: AEAD helpers for community state-root

**Files:**
- Create: `src-tauri/src/community_state_sync.rs`
- Modify: `src-tauri/src/lib.rs` (add `pub mod community_state_sync;`)
- Create: `src-tauri/tests/community_state_sync_crypto_unit.rs`

**Pattern source:** `dm_crypto::DmContentKey::encrypt` / `decrypt` (`src-tauri/src/dm_crypto.rs`) for the per-key AEAD shape; `owner_state_crypto::encrypt_root_publish` / `decrypt_root_publish` for the random-nonce wire form. Community uses `MembershipKey` directly (no `KeyTree` derivation) — communities have one symmetric key per community, distributed via the invite payload.

- [ ] **Step 4.1: Write failing AEAD round-trip test**

Create `src-tauri/tests/community_state_sync_crypto_unit.rs`:

```rust
//! Unit tests for AEAD helpers in community_state_sync.rs.

use harmony_app::community_state_sync::{
    decrypt_blob, decrypt_root_publish, encrypt_blob, encrypt_root_publish, CommunityCryptoError,
};
use harmony_app::owner_state_types::MembershipKey;

#[test]
fn encrypt_root_publish_round_trips() {
    let mk = MembershipKey::new([0x42; 32]);
    let plaintext = b"hello-community-root-publish".to_vec();

    let wire = encrypt_root_publish(&mk, &plaintext).expect("encrypt");
    assert_ne!(wire, plaintext, "ciphertext must differ from plaintext");

    let recovered = decrypt_root_publish(&mk, &wire).expect("decrypt");
    assert_eq!(recovered, plaintext);
}

#[test]
fn encrypt_root_publish_rejects_wrong_key() {
    let mk_a = MembershipKey::new([0x01; 32]);
    let mk_b = MembershipKey::new([0x02; 32]);
    let plaintext = b"secret".to_vec();
    let wire = encrypt_root_publish(&mk_a, &plaintext).expect("encrypt");
    let err = decrypt_root_publish(&mk_b, &wire).unwrap_err();
    assert!(matches!(err, CommunityCryptoError::AeadFailed));
}

#[test]
fn encrypt_blob_is_deterministic_for_same_key_and_plaintext() {
    // Deterministic: encrypt_blob uses a fixed-derivation nonce so
    // the same (key, plaintext) produces the same ciphertext —
    // letting the ContentStore content-address it identically across
    // replicas. encrypt_root_publish uses a random nonce by contrast
    // (each publish is a distinct wire packet and we want freshness).
    let mk = MembershipKey::new([0xaa; 32]);
    let plaintext = b"deterministic-blob".to_vec();
    let a = encrypt_blob(&mk, &plaintext).expect("encrypt a");
    let b = encrypt_blob(&mk, &plaintext).expect("encrypt b");
    assert_eq!(a, b, "blob encryption must be deterministic for content addressing");
}

#[test]
fn encrypt_blob_round_trips() {
    let mk = MembershipKey::new([0xbb; 32]);
    let plaintext = b"event-log-cbor-bytes-go-here".to_vec();
    let ct = encrypt_blob(&mk, &plaintext).expect("encrypt");
    let recovered = decrypt_blob(&mk, &ct).expect("decrypt");
    assert_eq!(recovered, plaintext);
}
```

- [ ] **Step 4.2: Run test to verify it fails**

```bash
cd src-tauri
cargo test --test community_state_sync_crypto_unit 2>&1 | tail -10
```

Expected: compile error — module doesn't exist yet.

- [ ] **Step 4.3: Create the module + AEAD helpers**

Create `src-tauri/src/community_state_sync.rs`:

```rust
//! Per-community state-CRDT sync — Phase 2 of ZEB-217 Sub-C.
//!
//! Mirrors the SHAPE of `crate::owner_state_sync::SyncEngine` but
//! multi-instance: one `CommunitySyncEngine` per joined community.
//! Each engine debounces local mutations into encrypted state-root
//! publishes, fetches remote state-root publishes from a per-community
//! Zenoh topic, DAG-syncs the encrypted blob via existing CAS
//! machinery, decrypts, and merges remote events into local
//! `CommunityState` after re-running `verify_event` per event.
//!
//! This file ships the AEAD helpers, wire types, RootHlcTracker,
//! engine task, and registry. Subsequent tasks fill in each piece
//! incrementally.

use crate::owner_state_types::MembershipKey;
use chacha20poly1305::aead::Aead;
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce};

/// Errors specific to community-state encryption + decryption.
#[derive(thiserror::Error, Debug)]
pub enum CommunityCryptoError {
    #[error("AEAD operation failed (wrong key, malformed ciphertext, tag mismatch)")]
    AeadFailed,
    #[error("ciphertext too short to contain nonce + tag")]
    Truncated,
}

/// Domain-separation prefix for the per-community blob nonce.
/// Combined with the SHA-256 of the plaintext to derive a deterministic
/// 12-byte nonce — see `encrypt_blob` for the full derivation.
const COMMUNITY_BLOB_NONCE_PREFIX: &[u8] = b"harmony-community-blob-v1";

/// Domain-separation prefix for root-publish AEAD AAD. Bound to the
/// wire form so a re-encrypted blob from a different context can't be
/// substituted as a root-publish wire packet.
const COMMUNITY_ROOT_PUBLISH_AAD: &[u8] = b"harmony-community-root-publish-v1";

/// Encrypt a state-root publish payload with the community's
/// MembershipKey. Random 12-byte nonce prepended to the ciphertext;
/// receiver splits and verifies via ChaCha20-Poly1305 AAD binding.
///
/// Random nonce is correct here (every publish is a distinct wire
/// packet — we WANT freshness; replay protection is the receiver's
/// RootHlcTracker, not nonce reuse).
pub fn encrypt_root_publish(
    mk: &MembershipKey,
    plaintext: &[u8],
) -> Result<Vec<u8>, CommunityCryptoError> {
    use rand::rngs::OsRng;
    use rand::RngCore;

    let cipher = ChaCha20Poly1305::new(mk.as_chacha_key());
    let mut nonce_bytes = [0u8; 12];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ct = cipher
        .encrypt(
            nonce,
            chacha20poly1305::aead::Payload {
                msg: plaintext,
                aad: COMMUNITY_ROOT_PUBLISH_AAD,
            },
        )
        .map_err(|_| CommunityCryptoError::AeadFailed)?;

    let mut wire = Vec::with_capacity(12 + ct.len());
    wire.extend_from_slice(&nonce_bytes);
    wire.extend_from_slice(&ct);
    Ok(wire)
}

pub fn decrypt_root_publish(
    mk: &MembershipKey,
    wire: &[u8],
) -> Result<Vec<u8>, CommunityCryptoError> {
    if wire.len() < 12 + 16 {
        return Err(CommunityCryptoError::Truncated);
    }
    let cipher = ChaCha20Poly1305::new(mk.as_chacha_key());
    let nonce = Nonce::from_slice(&wire[..12]);
    let ct = &wire[12..];
    cipher
        .decrypt(
            nonce,
            chacha20poly1305::aead::Payload {
                msg: ct,
                aad: COMMUNITY_ROOT_PUBLISH_AAD,
            },
        )
        .map_err(|_| CommunityCryptoError::AeadFailed)
}

/// Encrypt the CBOR-encoded `CommunityState` blob with a deterministic
/// nonce — same (key, plaintext) yields the same ciphertext, so the
/// resulting `ContentId` is reproducible across replicas. Lets two
/// devices encrypting the same state hit the same ContentStore slot.
///
/// Nonce derivation: SHA-256(prefix || mk_bytes || plaintext)[..12].
/// Binding the nonce to BOTH the key and plaintext ensures the same
/// pair always derives the same nonce; mixing the key into the nonce
/// is a nonce-reuse-resistance hedge (an attacker without `mk` cannot
/// derive the nonce, so a chosen-plaintext nonce-collision attack
/// requires already having the key).
pub fn encrypt_blob(
    mk: &MembershipKey,
    plaintext: &[u8],
) -> Result<Vec<u8>, CommunityCryptoError> {
    use sha2::{Digest, Sha256};

    let mut h = Sha256::new();
    h.update(COMMUNITY_BLOB_NONCE_PREFIX);
    h.update(mk.as_bytes());
    h.update(plaintext);
    let digest = h.finalize();
    let nonce_bytes: [u8; 12] = digest[..12].try_into().expect("SHA-256 ≥ 12 bytes");

    let cipher = ChaCha20Poly1305::new(mk.as_chacha_key());
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ct = cipher
        .encrypt(nonce, plaintext)
        .map_err(|_| CommunityCryptoError::AeadFailed)?;

    let mut wire = Vec::with_capacity(12 + ct.len());
    wire.extend_from_slice(&nonce_bytes);
    wire.extend_from_slice(&ct);
    Ok(wire)
}

pub fn decrypt_blob(
    mk: &MembershipKey,
    wire: &[u8],
) -> Result<Vec<u8>, CommunityCryptoError> {
    if wire.len() < 12 + 16 {
        return Err(CommunityCryptoError::Truncated);
    }
    let cipher = ChaCha20Poly1305::new(mk.as_chacha_key());
    let nonce = Nonce::from_slice(&wire[..12]);
    let ct = &wire[12..];
    cipher
        .decrypt(nonce, ct)
        .map_err(|_| CommunityCryptoError::AeadFailed)
}
```

You also need a helper on `MembershipKey` to expose the ChaCha20Poly1305 key. If `MembershipKey` doesn't already have `as_chacha_key()` and `as_bytes()`, add them in `owner_state_types.rs` near the existing `MembershipKey` definition (Phase 1 added the type at around line 1200; check `git grep "impl MembershipKey"`):

```rust
impl MembershipKey {
    pub fn as_chacha_key(&self) -> &chacha20poly1305::Key {
        chacha20poly1305::Key::from_slice(&self.0)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}
```

If those already exist (likely — Phase 1 may have added them), skip this addition.

- [ ] **Step 4.4: Wire the module declaration**

Open `src-tauri/src/lib.rs`. After `pub mod community_state_crdt;`, add:

```rust
pub mod community_state_sync;
```

- [ ] **Step 4.5: Run tests**

```bash
cd src-tauri
cargo test --test community_state_sync_crypto_unit 2>&1 | grep "^test result:"
```

Expected: PASS (4 tests).

- [ ] **Step 4.6: Run gates + commit**

```bash
cd src-tauri
set -o pipefail
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -3
cargo test --all-targets --all-features 2>&1 | grep "^test result:" | tail -3
```

```bash
git add src-tauri/src/community_state_sync.rs src-tauri/src/lib.rs src-tauri/src/owner_state_types.rs src-tauri/tests/community_state_sync_crypto_unit.rs
git commit -m "$(cat <<'EOF'
feat(zeb-217-phase2): community-state AEAD helpers

ChaCha20-Poly1305 with MembershipKey directly (no KeyTree derivation
— communities use one symmetric key per community distributed via
the invite payload). Two modes:

- encrypt_root_publish: random nonce per packet, freshness for the
  Zenoh wire form
- encrypt_blob: deterministic SHA-256-derived nonce so the same
  (key, plaintext) pair yields the same ciphertext, and thus the
  same ContentId for CAS dedup across replicas

Domain separation: encrypt_blob uses a prefix-tagged deterministic
nonce derivation (SHA-256 of "harmony-community-blob-v1" || mk ||
plaintext); encrypt_root_publish additionally binds a static
"harmony-community-root-publish-v1" AAD. The nonce-prefix vs AAD
asymmetry is intentional — blob nonces double as content-addressing
inputs, AAD is incorrect there because it doesn't change the
ciphertext bytes.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: `CommunityRootPublishPayload` wire format + golden CBOR fixture

**Files:**
- Modify: `src-tauri/src/community_state_sync.rs`
- Create: `src-tauri/tests/wire_format_community_sync_fixtures.rs`

- [ ] **Step 5.1: Write failing wire-format fixture test**

Create `src-tauri/tests/wire_format_community_sync_fixtures.rs`:

```rust
//! Pinned-byte CBOR wire-format fixtures for community-sync types.
//! Mirrors src-tauri/tests/wire_format_community_fixtures.rs from
//! Phase 1 — locking the encoded bytes of new types prevents silent
//! wire-form drift across phases.

use harmony_app::community_state_sync::CommunityRootPublishPayload;
use harmony_app::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
use harmony_app::owner_state_types::Hlc;
use harmony_content::cid::ContentId;

#[test]
fn community_root_publish_payload_wire_bytes_pinned() {
    // 28-byte ContentId is the SHA-256-truncated digest. We construct
    // a deterministic instance from a fixed byte pattern.
    let cid = ContentId::from_raw(
        harmony_content::cid::ContentFlags {
            encrypted: true,
            ..Default::default()
        },
        [0xAA; 28],
    )
    .expect("from_raw");
    let p = CommunityRootPublishPayload {
        root_cid: cid,
        at: Hlc {
            wall_ms: 1_700_000_000_000,
            logical: 7,
            device_id: "d1".into(),
        },
    };

    let bytes = canonical_cbor_encode(&p).expect("encode");
    // Lock the byte sequence — any structural change to the wire
    // form (field codes, encoding order, ContentId byte layout)
    // will require this fixture to update intentionally.
    let expected = hex::decode(
        "a262726398...REPLACE_AFTER_FIRST_GENERATION..."
    )
    .expect("hex");
    assert_eq!(
        bytes, expected,
        "CommunityRootPublishPayload wire bytes drifted: {} vs {}",
        hex::encode(&bytes),
        hex::encode(&expected)
    );

    let decoded: CommunityRootPublishPayload = canonical_cbor_decode(&bytes).expect("decode");
    assert_eq!(decoded.at.wall_ms, 1_700_000_000_000);
    assert_eq!(decoded.at.logical, 7);
}
```

Note the placeholder `REPLACE_AFTER_FIRST_GENERATION` — the first run will fail with the actual byte sequence in the assertion message. Step 5.4 captures the real bytes and updates the fixture (same pattern Phase 1 used for its golden fixtures).

- [ ] **Step 5.2: Add the wire type to community_state_sync.rs**

Append to `src-tauri/src/community_state_sync.rs`:

```rust
use crate::owner_state_crypto::{CanonicalPayload, CanonicalPayloadSealed};
use crate::owner_state_types::Hlc;
use harmony_content::cid::ContentId;
use serde::{Deserialize, Serialize};

/// State-root publish payload for a community. Sent over
/// `harmony/community/{id_hex}/state-root-v1` after AEAD-encryption
/// via `encrypt_root_publish`. Receivers fetch `root_cid` from CAS
/// to retrieve the encrypted CommunityState blob, then decrypt with
/// `decrypt_blob`.
///
/// Wire format: 2-key CBOR map. Both field codes are 2 chars
/// (`rc` + `at`) to satisfy the same-length-keys invariant at this
/// nesting level. The HLC `at` is the publisher's monotonic counter
/// — receivers' RootHlcTrackers reject anything not strictly newer
/// per (publisher_device_id, hlc) (replay protection; mirrors
/// owner_state_sync's RootPublishPayload at line 429).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommunityRootPublishPayload {
    /// Content-ID of the encrypted CommunityState blob in the
    /// shared ContentStore.
    #[serde(rename = "rc")]
    pub root_cid: ContentId,
    /// Publisher's HLC at publish time. Monotonically increasing per
    /// device_id; receivers track per-device latest-seen.
    #[serde(rename = "at")]
    pub at: Hlc,
}

impl CanonicalPayloadSealed for CommunityRootPublishPayload {}
impl CanonicalPayload for CommunityRootPublishPayload {}
```

- [ ] **Step 5.3: First run — generate the actual bytes**

```bash
cd src-tauri
cargo test --test wire_format_community_sync_fixtures 2>&1 | tail -20
```

Expected: assertion failure showing the actual hex bytes.

- [ ] **Step 5.4: Replace `REPLACE_AFTER_FIRST_GENERATION` with the captured hex**

Copy the actual hex from the failure output and update the test's `expected` string. Re-run:

```bash
cargo test --test wire_format_community_sync_fixtures 2>&1 | grep "^test result:"
```

Expected: PASS.

- [ ] **Step 5.5: Run gates + commit**

```bash
cd src-tauri
set -o pipefail
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -3
cargo test --all-targets --all-features 2>&1 | grep "^test result:" | tail -3
```

```bash
git add src-tauri/src/community_state_sync.rs src-tauri/tests/wire_format_community_sync_fixtures.rs
git commit -m "$(cat <<'EOF'
feat(zeb-217-phase2): CommunityRootPublishPayload wire format + golden fixture

2-key CBOR map (rc + at, both 2-char per same-length-keys invariant).
Wire bytes pinned in the fixture test — silent drift across phases is
how cross-language deserializer divergence sneaks in.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: `CommunitySyncEngine` scaffold (struct + new + shutdown)

**Files:**
- Modify: `src-tauri/src/community_state_sync.rs`
- Create: `src-tauri/tests/community_sync_engine_unit.rs`

**Pattern source:** `src-tauri/src/owner_state_sync.rs:54-191` — the `SyncEngine` struct, its constructor, `notify_dirty` / `flush_now` / `shutdown` API, and the `Mutex<Option<JoinHandle<()>>>` task-handle dance. Mirror exactly; community version differs only in (a) per-community key (MembershipKey, no KeyTree), (b) per-community topic key, (c) per-community RootHlcTracker keyed by community_id externally.

- [ ] **Step 6.1: Write failing scaffold test**

Create `src-tauri/tests/community_sync_engine_unit.rs`:

```rust
//! CommunitySyncEngine scaffold tests — verify construction +
//! shutdown without exercising the full sync loop yet.

use harmony_app::community_state_crdt::CommunityState;
use harmony_app::community_state_sync::{
    CommunityRootHlcTracker, CommunitySyncEngine, CommunitySyncEngineConfig, DEFAULT_DEBOUNCE_MS,
};
use harmony_app::content_store::{ContentStore, RuntimeContentStore};
use harmony_app::owner_state_types::{MembershipKey, OwnerAddr, SpaceId};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

#[tokio::test]
async fn engine_constructs_and_shuts_down_cleanly() {
    let (out_tx, _out_rx) = mpsc::channel::<Vec<u8>>(8);
    let (_in_tx, in_rx) = mpsc::channel::<Vec<u8>>(8);
    let (cas_op_tx, _cas_op_rx) = mpsc::channel(8);

    let community_id = SpaceId([1u8; 16]);
    let mk = MembershipKey::new([0x42; 32]);
    let admin = OwnerAddr([2u8; 16]);

    let state = Arc::new(Mutex::new(CommunityState::new(community_id)));
    let tracker = Arc::new(Mutex::new(CommunityRootHlcTracker::default()));
    let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx,
        std::time::Duration::from_millis(1000),
    ));

    let engine = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        community_id,
        membership_key: mk,
        admin_addr: admin,
        is_invite_only: false,
        device_id: "test-device".into(),
        state: Arc::clone(&state),
        tracker: Arc::clone(&tracker),
        content_store: cs,
        publisher_tx: out_tx,
        subscriber_rx: in_rx,
        paths: harmony_app::community_state_persist::PersistPaths {
            crdt: std::env::temp_dir().join("test_crdt.cbor"),
            replay: std::env::temp_dir().join("test_replay.cbor"),
        },
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        identity_resolver: None,
        error_tx: None,
    });

    // Shutdown without ever sending dirty — clean path.
    engine.shutdown().await.expect("shutdown");
}
```

Note: this test imports from `harmony_app::community_state_persist` which is created in Task 10 — this test will need to wait until then OR the path types live in a stub. Resolution: define `PersistPaths` inline in `community_state_sync.rs` for this task and replace the stub with a re-export from `community_state_persist` in Task 10. Adjust the import accordingly:

Replace:
```rust
paths: harmony_app::community_state_persist::PersistPaths {
```

with:
```rust
paths: harmony_app::community_state_sync::PersistPaths {
```

- [ ] **Step 6.2: Run test to verify it fails**

```bash
cd src-tauri
cargo test --test community_sync_engine_unit 2>&1 | tail -10
```

Expected: compile errors — `CommunitySyncEngine` etc. don't exist yet.

- [ ] **Step 6.3: Add the scaffold**

Append to `src-tauri/src/community_state_sync.rs`:

```rust
use crate::community_state_crdt::CommunityState;
use crate::content_store::ContentStore;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex, Notify};
use tokio::task::JoinHandle;

pub const DEFAULT_DEBOUNCE_MS: u64 = 250;

#[derive(thiserror::Error, Debug)]
pub enum CommunitySyncError {
    #[error("crypto: {0}")]
    Crypto(#[from] CommunityCryptoError),
    #[error("CBOR encode: {0}")]
    CborEncode(String),
    #[error("CBOR decode: {0}")]
    CborDecode(String),
    #[error("content store: {0}")]
    ContentStore(#[from] crate::content_store::ContentStoreError),
    #[error("transport channel closed")]
    TransportClosed,
    #[error("persist: {0}")]
    Persist(String),
}

#[derive(Default, Clone, Debug)]
pub struct CommunityRootHlcTracker {
    /// Per-publisher-device latest-accepted HLC. New incoming root
    /// publishes are accepted only if STRICTLY NEWER per their
    /// device_id key. Mirrors owner_state_sync's tracker shape.
    pub per_device: BTreeMap<String, Hlc>,
}

impl CommunityRootHlcTracker {
    /// Test the candidate HLC against the per-device latest. Returns
    /// `true` if the candidate strictly dominates the recorded entry
    /// (or there is none); `false` otherwise.
    ///
    /// Does NOT mutate — the caller decides whether to advance after
    /// the rest of the receive pipeline succeeds. Mirrors the
    /// "advance-after-success" idiom from owner-state's tracker.
    pub fn would_accept(&self, candidate: &Hlc) -> bool {
        match self.per_device.get(&candidate.device_id) {
            None => true,
            Some(prev) => candidate.is_strictly_newer_than(prev),
        }
    }

    pub fn advance(&mut self, candidate: Hlc) {
        // Defensive: only advance if would_accept passes. Caller
        // should have checked already, but a redundant guard catches
        // accidental backward-jumps from buggy call sites.
        let device_id = candidate.device_id.clone();
        let should_advance = match self.per_device.get(&device_id) {
            None => true,
            Some(prev) => candidate.is_strictly_newer_than(prev),
        };
        if should_advance {
            self.per_device.insert(device_id, candidate);
        }
    }
}

#[derive(Debug, Clone)]
pub struct PersistPaths {
    pub crdt: PathBuf,
    pub replay: PathBuf,
}

/// Resolves an OwnerAddr → 64-byte identity_pub at receive-side
/// verify-event time. Production implementation wraps Sub-A's
/// owner-device cache (Task 13's `OwnerDeviceCacheResolver`); tests
/// use a static mapping. The trait is declared at Task 6 so the
/// `CommunitySyncEngineConfig::identity_resolver` field can reference
/// it; concrete implementations (other than test stubs) land in
/// later tasks.
pub trait IdentityResolver: Send + Sync {
    fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]>;
}

/// One degraded-path report from an engine. Sent on the engine's
/// `error_tx` channel to a registry-level receiver, which translates
/// each report into a `community-state-sync-degraded` Tauri IPC event
/// (Task 13 wires the receiver). Decoupling the engine from the
/// `tauri::AppHandle` keeps the CRDT layer Tauri-agnostic and makes
/// the engine unit-testable without spinning up a Tauri runtime.
#[derive(Debug, Clone)]
pub struct CommunityDegradedReport {
    pub community_id: SpaceId,
    /// Short tag identifying the failure class. Stable across versions
    /// so the frontend's banner copy can switch on it. Examples:
    /// "decrypt_failed", "blob_fetch_failed", "verify_event_rejected",
    /// "wire_decode_failed", "subscriber_channel_closed".
    pub reason_tag: &'static str,
    /// Human-readable detail. Not localised; surfaced to the frontend
    /// for telemetry / debug display rather than user-facing copy.
    pub detail: String,
}

pub struct CommunitySyncEngineConfig {
    pub community_id: SpaceId,
    pub membership_key: MembershipKey,
    pub admin_addr: OwnerAddr,
    /// Whether this community requires invite-only counter-sigs on
    /// non-admin Joins. Plumbed into VerifyContext at receive time
    /// (Task 8 consumes this). Defaults to `false` for tests that
    /// don't exercise the invite-only path.
    pub is_invite_only: bool,
    pub device_id: String,
    pub state: Arc<Mutex<CommunityState>>,
    pub tracker: Arc<Mutex<CommunityRootHlcTracker>>,
    pub content_store: Arc<dyn ContentStore>,
    pub publisher_tx: mpsc::Sender<Vec<u8>>,
    pub subscriber_rx: mpsc::Receiver<Vec<u8>>,
    pub paths: PersistPaths,
    pub debounce_ms: u64,
    /// Resolver for OwnerAddr → 64-byte identity_pub at receive-side
    /// verify_event time. `None` means receive-side verify will skip
    /// every event (with a tracing::warn) — acceptable for Task 6/7
    /// tests that exercise the publish path only; Task 8's tests must
    /// supply a Some(resolver).
    pub identity_resolver: Option<Arc<dyn IdentityResolver>>,
    /// Channel for degraded-path reports. Cloned by the registry from
    /// a single shared receiver lived in start_node (Task 13). `None`
    /// means degraded paths log via `tracing::warn!` only — acceptable
    /// for tests that don't assert on IPC-event emission.
    pub error_tx: Option<mpsc::Sender<CommunityDegradedReport>>,
}

pub struct CommunitySyncEngine {
    notify_dirty: Arc<Notify>,
    has_pending_dirty: Arc<AtomicBool>,
    flush_now_tx: mpsc::Sender<tokio::sync::oneshot::Sender<Result<(), CommunitySyncError>>>,
    shutdown_tx: mpsc::Sender<tokio::sync::oneshot::Sender<Result<(), CommunitySyncError>>>,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl CommunitySyncEngine {
    pub fn new(cfg: CommunitySyncEngineConfig) -> Self {
        let notify_dirty = Arc::new(Notify::new());
        let has_pending_dirty = Arc::new(AtomicBool::new(false));
        let (flush_now_tx, flush_now_rx) = mpsc::channel(8);
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

        // Task 7 fills in the internal_task body; for now we spawn
        // a stub that just waits on shutdown so construction +
        // teardown can be exercised independently.
        let task = tokio::spawn(internal_task_stub(InternalCtx {
            community_id: cfg.community_id,
            membership_key: cfg.membership_key,
            admin_addr: cfg.admin_addr,
            is_invite_only: cfg.is_invite_only,
            device_id: cfg.device_id,
            state: cfg.state,
            tracker: cfg.tracker,
            content_store: cfg.content_store,
            publisher_tx: cfg.publisher_tx,
            subscriber_rx: cfg.subscriber_rx,
            paths: cfg.paths,
            debounce: std::time::Duration::from_millis(cfg.debounce_ms),
            notify_dirty: Arc::clone(&notify_dirty),
            has_pending_dirty: Arc::clone(&has_pending_dirty),
            flush_now_rx,
            shutdown_rx,
            identity_resolver: cfg.identity_resolver,
            error_tx: cfg.error_tx,
        }));

        Self {
            notify_dirty,
            has_pending_dirty,
            flush_now_tx,
            shutdown_tx,
            task: Mutex::new(Some(task)),
        }
    }

    pub fn notify_dirty(&self) {
        self.has_pending_dirty.store(true, Ordering::Relaxed);
        self.notify_dirty.notify_one();
    }

    pub async fn flush_now(&self) -> Result<(), CommunitySyncError> {
        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        self.flush_now_tx
            .send(resp_tx)
            .await
            .map_err(|_| CommunitySyncError::TransportClosed)?;
        resp_rx.await.map_err(|_| CommunitySyncError::TransportClosed)?
    }

    pub async fn shutdown(&self) -> Result<(), CommunitySyncError> {
        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        let result = if self.shutdown_tx.send(resp_tx).await.is_ok() {
            resp_rx.await.map_err(|_| CommunitySyncError::TransportClosed)?
        } else {
            Ok(())
        };
        let _ = self.task.lock().await.take();
        result
    }
}

struct InternalCtx {
    community_id: SpaceId,
    membership_key: MembershipKey,
    admin_addr: OwnerAddr,
    is_invite_only: bool,
    device_id: String,
    state: Arc<Mutex<CommunityState>>,
    tracker: Arc<Mutex<CommunityRootHlcTracker>>,
    content_store: Arc<dyn ContentStore>,
    publisher_tx: mpsc::Sender<Vec<u8>>,
    subscriber_rx: mpsc::Receiver<Vec<u8>>,
    paths: PersistPaths,
    debounce: std::time::Duration,
    notify_dirty: Arc<Notify>,
    has_pending_dirty: Arc<AtomicBool>,
    flush_now_rx: mpsc::Receiver<tokio::sync::oneshot::Sender<Result<(), CommunitySyncError>>>,
    shutdown_rx: mpsc::Receiver<tokio::sync::oneshot::Sender<Result<(), CommunitySyncError>>>,
    identity_resolver: Option<Arc<dyn IdentityResolver>>,
    error_tx: Option<mpsc::Sender<CommunityDegradedReport>>,
}

async fn internal_task_stub(mut ctx: InternalCtx) {
    // Stub: waits for shutdown, replies with Ok. Task 7 replaces
    // this with the real publish/subscribe loop.
    if let Some(resp_tx) = ctx.shutdown_rx.recv().await {
        let _ = resp_tx.send(Ok(()));
    }
}
```

Add the necessary imports at the top of the file:

```rust
use crate::owner_state_types::{Hlc, MembershipKey, OwnerAddr, SpaceId};
```

Note that `Hlc` doesn't currently expose `is_strictly_newer_than` as a method — check at `src-tauri/src/owner_state_types.rs` (around the Hlc definition). If it's not present as a method, add one alongside the Hlc struct. The owner-state SyncEngine almost certainly uses this comparison; if the project uses an `impl Ord for Hlc` instead, switch to:

```rust
Some(prev) => candidate > prev,
```

Verify with `grep -n "is_strictly_newer_than\|impl Ord for Hlc" src-tauri/src/owner_state_types.rs` and use whichever already exists. Do NOT introduce a new comparison API in Phase 2 — pick the one Phase 1 uses.

- [ ] **Step 6.4: Run tests**

```bash
cd src-tauri
cargo test --test community_sync_engine_unit 2>&1 | grep "^test result:"
```

Expected: PASS (1 test).

- [ ] **Step 6.5: Run gates + commit**

```bash
cd src-tauri
set -o pipefail
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -3
cargo test --all-targets --all-features 2>&1 | grep "^test result:" | tail -3
```

```bash
git add src-tauri/src/community_state_sync.rs src-tauri/src/owner_state_types.rs src-tauri/tests/community_sync_engine_unit.rs
git commit -m "$(cat <<'EOF'
feat(zeb-217-phase2): CommunitySyncEngine scaffold

Mirrors owner_state_sync::SyncEngine shape: notify_dirty + flush_now
+ shutdown API, internal_task spawned at construction. internal_task
is a stub that just waits for shutdown — Task 7 fills in the real
publish/subscribe loop.

Per-community config (community_id, MembershipKey, admin_addr,
device_id) plus the shared CommunityState + tracker arcs and the
publisher/subscriber mpsc channels (Zenoh adapter wiring lands in
Task 12).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: `internal_task` — debounced publish path

**Files:**
- Modify: `src-tauri/src/community_state_sync.rs`
- Modify: `src-tauri/tests/community_sync_engine_unit.rs`

**Pattern source:** `src-tauri/src/owner_state_sync.rs:211-348` — the `internal_task` `select!` loop. We replace the `internal_task_stub` with a real loop that handles `notify_dirty`, debounce wakeups, `flush_now`, and `shutdown`.

- [ ] **Step 7.1: Write failing publish-on-flush-now test**

Append to `src-tauri/tests/community_sync_engine_unit.rs`:

```rust
#[tokio::test]
async fn flush_now_publishes_one_root_publish() {
    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(8);
    let (_in_tx, in_rx) = mpsc::channel::<Vec<u8>>(8);
    let (cas_op_tx, mut cas_op_rx) = mpsc::channel(8);

    // Drain CasOps in a background task — RuntimeContentStore expects
    // someone to service them. For this test we just ack with empty
    // PutLocal responses so the engine's content_store.put doesn't
    // hang.
    tokio::spawn(async move {
        use harmony_app::content_store::CasOp;
        while let Some(op) = cas_op_rx.recv().await {
            if let CasOp::PutLocal { resp, .. } = op {
                let _ = resp.send(Ok(()));
            }
        }
    });

    let community_id = SpaceId([1u8; 16]);
    let mk = MembershipKey::new([0x42; 32]);
    let admin = OwnerAddr([2u8; 16]);

    let state = Arc::new(Mutex::new(CommunityState::new(community_id)));
    let tracker = Arc::new(Mutex::new(CommunityRootHlcTracker::default()));
    let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx,
        std::time::Duration::from_millis(1000),
    ));

    let engine = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        community_id,
        membership_key: mk,
        admin_addr: admin,
        is_invite_only: false,
        device_id: "test-device".into(),
        state: Arc::clone(&state),
        tracker: Arc::clone(&tracker),
        content_store: cs,
        publisher_tx: out_tx,
        subscriber_rx: in_rx,
        paths: harmony_app::community_state_sync::PersistPaths {
            crdt: std::env::temp_dir().join("flush_test_crdt.cbor"),
            replay: std::env::temp_dir().join("flush_test_replay.cbor"),
        },
        debounce_ms: harmony_app::community_state_sync::DEFAULT_DEBOUNCE_MS,
        identity_resolver: None,
        error_tx: None,
    });

    engine.flush_now().await.expect("flush_now");

    // The engine should have written one wire packet to out_rx.
    let bytes = out_rx
        .recv()
        .await
        .expect("publisher_tx dropped or never sent");
    assert!(!bytes.is_empty(), "wire packet should be non-empty");

    engine.shutdown().await.expect("shutdown");
}
```

- [ ] **Step 7.2: Run test to verify it fails**

```bash
cd src-tauri
cargo test --test community_sync_engine_unit flush_now_publishes 2>&1 | tail -10
```

Expected: timeout or hang — the stub never publishes.

- [ ] **Step 7.3: Replace `internal_task_stub` with the real publish loop**

In `src-tauri/src/community_state_sync.rs`, replace `async fn internal_task_stub` with:

```rust
async fn internal_task(mut ctx: InternalCtx) {
    use std::time::Instant;

    let mut next_wakeup: Option<Instant> = None;
    let mut inbound_closed = false;

    let notify = Arc::clone(&ctx.notify_dirty);
    let notified = notify.notified();
    tokio::pin!(notified);

    loop {
        let sleep_dur = next_wakeup
            .map(|t| t.saturating_duration_since(Instant::now()))
            .unwrap_or(std::time::Duration::from_secs(3600));

        tokio::select! {
            _ = notified.as_mut() => {
                if ctx.has_pending_dirty.load(Ordering::Relaxed) {
                    next_wakeup = Some(Instant::now() + ctx.debounce);
                }
                notified.set(notify.notified());
            }
            _ = tokio::time::sleep(sleep_dur), if next_wakeup.is_some() => {
                next_wakeup = None;
                let was_dirty = ctx.has_pending_dirty.swap(false, Ordering::AcqRel);
                let pub_result = publish_root_now(&ctx).await;
                if let Err(e) = &pub_result {
                    tracing::warn!(community_id = ?ctx.community_id, error = %e, "community publish_root_now failed");
                    if was_dirty {
                        ctx.has_pending_dirty.store(true, Ordering::Release);
                    }
                }
                // Persist invocation deferred to Task 10; for now we
                // accept that on-disk state lags by one wakeup tick.
            }
            Some(resp_tx) = ctx.flush_now_rx.recv() => {
                next_wakeup = None;
                let was_dirty = ctx.has_pending_dirty.swap(false, Ordering::AcqRel);
                let pub_result = publish_root_now(&ctx).await;
                if pub_result.is_err() && was_dirty {
                    ctx.has_pending_dirty.store(true, Ordering::Release);
                }
                let _ = resp_tx.send(pub_result);
            }
            maybe_bytes = ctx.subscriber_rx.recv(), if !inbound_closed => {
                let Some(_bytes) = maybe_bytes else {
                    tracing::error!(
                        community_id = ?ctx.community_id,
                        "community subscriber channel closed; sync inbound disabled"
                    );
                    inbound_closed = true;
                    continue;
                };
                // Task 8 fills in handle_incoming_publish.
            }
            Some(resp_tx) = ctx.shutdown_rx.recv() => {
                let pub_result = if ctx.has_pending_dirty.load(Ordering::Relaxed) {
                    publish_root_now(&ctx).await
                } else {
                    Ok(())
                };
                let _ = resp_tx.send(pub_result);
                return;
            }
        }
    }
}

async fn publish_root_now(ctx: &InternalCtx) -> Result<(), CommunitySyncError> {
    use crate::owner_state_crypto::canonical_cbor_encode;

    // Snapshot CRDT state under brief lock.
    let snapshot = {
        let state = ctx.state.lock().await;
        state.clone()
    };

    // 1. Canonical-CBOR encode the CommunityState as the cleartext blob.
    let blob_cleartext = canonical_cbor_encode(&snapshot)
        .map_err(|e| CommunitySyncError::CborEncode(e.to_string()))?;

    // 2. Encrypt with deterministic-nonce blob AEAD so cipher_cid is
    //    reproducible across replicas.
    let blob_ciphertext = encrypt_blob(&ctx.membership_key, &blob_cleartext)?;

    // 3. Derive ContentId for the encrypted blob.
    let root_cid = harmony_content::cid::ContentId::for_book(
        &blob_ciphertext,
        harmony_content::cid::ContentFlags {
            encrypted: true,
            ..Default::default()
        },
    )
    .map_err(|e| CommunitySyncError::Crypto(CommunityCryptoError::AeadFailed.into()))?;

    // 4. Put into ContentStore.
    ctx.content_store.put(root_cid, blob_ciphertext).await?;

    // 5. Build state-root payload.
    let now = next_hlc(ctx).await;
    let payload = CommunityRootPublishPayload { root_cid, at: now };
    let payload_bytes = canonical_cbor_encode(&payload)
        .map_err(|e| CommunitySyncError::CborEncode(e.to_string()))?;

    // 6. Encrypt with random-nonce root AEAD.
    let wire = encrypt_root_publish(&ctx.membership_key, &payload_bytes)?;

    // 7. Send onto outbound channel.
    ctx.publisher_tx
        .send(wire)
        .await
        .map_err(|_| CommunitySyncError::TransportClosed)?;

    Ok(())
}

async fn next_hlc(ctx: &InternalCtx) -> Hlc {
    use std::time::{SystemTime, UNIX_EPOCH};
    let wall_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let mut tracker = ctx.tracker.lock().await;
    let prev = tracker.per_device.get(&ctx.device_id).cloned();
    let (logical, prev_wall) = match prev.as_ref() {
        Some(p) if p.wall_ms == wall_ms => (p.logical.saturating_add(1), p.wall_ms),
        Some(p) if p.wall_ms > wall_ms => (p.logical.saturating_add(1), p.wall_ms),
        Some(p) => (0, p.wall_ms),
        None => (0, 0),
    };
    let effective_wall = std::cmp::max(wall_ms, prev_wall);
    let now = Hlc {
        wall_ms: effective_wall,
        logical,
        device_id: ctx.device_id.clone(),
    };
    tracker.per_device.insert(ctx.device_id.clone(), now.clone());
    now
}
```

Update the `tokio::spawn` call in `CommunitySyncEngine::new` to call `internal_task` (not `internal_task_stub`):

```rust
let task = tokio::spawn(internal_task(InternalCtx { /* ... */ }));
```

Delete the `internal_task_stub` function entirely.

The `CommunityCryptoError::AeadFailed` mapping in step 3 is a compromise — `ContentId::for_book` errors aren't AEAD failures. Replace with a dedicated variant:

In `CommunityCryptoError`, add:

```rust
#[error("ContentId derivation failed: {0}")]
ContentIdDerivation(String),
```

Then in `publish_root_now`:

```rust
.map_err(|e| CommunitySyncError::Crypto(CommunityCryptoError::ContentIdDerivation(e.to_string())))?;
```

- [ ] **Step 7.4: Run tests**

```bash
cd src-tauri
cargo test --test community_sync_engine_unit 2>&1 | grep "^test result:"
```

Expected: PASS (2 tests).

- [ ] **Step 7.5: Run gates + commit**

```bash
cd src-tauri
set -o pipefail
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -3
cargo test --all-targets --all-features 2>&1 | grep "^test result:" | tail -3
```

```bash
git add src-tauri/src/community_state_sync.rs src-tauri/tests/community_sync_engine_unit.rs
git commit -m "$(cat <<'EOF'
feat(zeb-217-phase2): CommunitySyncEngine debounced publish loop

internal_task select! arms: notify_dirty (arms debounce), debounce
wakeup (publish + restore-on-failure), flush_now (force publish),
subscriber stub (Task 8 fills in), shutdown (final flush + reply).

publish_root_now: snapshot → CBOR → encrypt_blob → ContentId →
ContentStore.put → CommunityRootPublishPayload → encrypt_root_publish
→ publisher_tx. Mirrors owner_state_sync::publish_root_now exactly,
swapping KeyTree-derived AEAD for direct-MembershipKey AEAD.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: `handle_incoming_publish` — subscriber arm with verify-on-receive

**Files:**
- Modify: `src-tauri/src/community_state_sync.rs`
- Modify: `src-tauri/tests/community_sync_engine_unit.rs`

**Pattern source:** `src-tauri/src/owner_state_sync.rs:592-700` — the `handle_incoming_publish` function with its `IncomingOutcome` enum. Replicate the structure but route every newly-received event through `verify_event` before merging into local `CommunityState`.

- [ ] **Step 8.1: Write failing two-engine round-trip test**

Append to `src-tauri/tests/community_sync_engine_unit.rs`:

```rust
use harmony_app::community_membership::{
    sign_event_with_identity, EventPayload, MembershipEventKind,
};
use harmony_app::owner_state_types::Hlc;
use harmony_identity::PrivateIdentity;

#[tokio::test]
async fn engine_receives_remote_publish_and_merges_event() {
    // Two-engine setup: A publishes a Join event; B receives and
    // merges. Wire the engines together via mpsc — A's out_rx is
    // forwarded to B's in_tx. ContentStore is shared between A and B
    // so B can fetch the blob A wrote.
    use std::time::Duration;

    let (a_out_tx, mut a_out_rx) = mpsc::channel::<Vec<u8>>(8);
    let (a_in_tx, a_in_rx) = mpsc::channel::<Vec<u8>>(8);
    let (b_out_tx, _b_out_rx) = mpsc::channel::<Vec<u8>>(8);
    let (b_in_tx, b_in_rx) = mpsc::channel::<Vec<u8>>(8);
    let (cas_op_tx, mut cas_op_rx) = mpsc::channel(64);

    // Shared in-memory CAS for both engines. Spawn a CasOp servicer.
    let cas: Arc<tokio::sync::Mutex<std::collections::HashMap<harmony_content::cid::ContentId, Vec<u8>>>> =
        Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
    let cas_for_servicer = Arc::clone(&cas);
    tokio::spawn(async move {
        use harmony_app::content_store::CasOp;
        while let Some(op) = cas_op_rx.recv().await {
            match op {
                CasOp::PutLocal { cid, bytes, resp } => {
                    cas_for_servicer.lock().await.insert(cid, bytes);
                    let _ = resp.send(Ok(()));
                }
                CasOp::GetLocal { cid, resp } => {
                    let v = cas_for_servicer.lock().await.get(&cid).cloned();
                    let _ = resp.send(v.ok_or_else(|| {
                        harmony_app::content_store::ContentStoreError::NotFound(cid)
                    }));
                }
            }
        }
    });

    // Forwarder: drain A's out_rx into B's in_tx.
    tokio::spawn(async move {
        while let Some(bytes) = a_out_rx.recv().await {
            let _ = b_in_tx.send(bytes).await;
        }
    });

    let community_id = SpaceId([1u8; 16]);
    let mk = MembershipKey::new([0x42; 32]);

    let identity_a = PrivateIdentity::generate();
    let admin = OwnerAddr(identity_a.identity.address_hash);
    let identity_a_pub = identity_a.identity.to_public_bytes();

    // Owner-device cache for verify-side identity_pub lookups. Phase
    // 2 needs at least the admin's identity_pub registered for B's
    // engine to verify A's events. (In production, owner-state Sub-A
    // populates this from RegisterDevice events; for this test we
    // populate manually.)
    // ...

    let state_a = Arc::new(Mutex::new(CommunityState::new(community_id)));
    let state_b = Arc::new(Mutex::new(CommunityState::new(community_id)));
    let tracker_a = Arc::new(Mutex::new(CommunityRootHlcTracker::default()));
    let tracker_b = Arc::new(Mutex::new(CommunityRootHlcTracker::default()));

    let cs_a: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx.clone(),
        Duration::from_millis(2000),
    ));
    let cs_b: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx,
        Duration::from_millis(2000),
    ));

    // Pre-populate state A with one Join event by the admin so the
    // publish carries non-empty state.
    {
        let mut sa = state_a.lock().await;
        let payload = EventPayload {
            id: [9u8; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: admin,
            at: Hlc { wall_ms: 100, logical: 0, device_id: "a-dev".into() },
        };
        let event = sign_event_with_identity(payload, &identity_a).expect("sign");
        let outcome = sa.insert_event(
            event,
            &harmony_app::community_membership::VerifyContext {
                expected_community_id: community_id,
                admin_addr: admin,
                is_invite_only: false,
                actor_identity_pub: &identity_a_pub,
                countersigner_identity_pub: None,
            },
        );
        assert!(matches!(
            outcome,
            harmony_app::community_state_crdt::InsertOutcome::Inserted
        ));
    }

    // Spawn engines. Task 6 added is_invite_only / identity_resolver
    // / error_tx fields with sensible defaults; Task 8's tests fill
    // in `identity_resolver: Some(...)` so receive-side verify_event
    // can resolve identity_pubs for the admin's signed events.
    let engine_a = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        community_id,
        membership_key: mk.clone(),
        admin_addr: admin,
        is_invite_only: false,
        device_id: "a-dev".into(),
        state: Arc::clone(&state_a),
        tracker: Arc::clone(&tracker_a),
        content_store: cs_a,
        publisher_tx: a_out_tx,
        subscriber_rx: a_in_rx,
        paths: harmony_app::community_state_sync::PersistPaths {
            crdt: std::env::temp_dir().join("a_crdt.cbor"),
            replay: std::env::temp_dir().join("a_replay.cbor"),
        },
        debounce_ms: harmony_app::community_state_sync::DEFAULT_DEBOUNCE_MS,
        identity_resolver: None,
        error_tx: None,
    });

    // B needs an OwnerDeviceCache-style lookup that returns
    // identity_a_pub for `admin`. Production wires Task 13's
    // `OwnerDeviceCacheResolver`; this test uses a static stub.
    let identity_resolver: Arc<dyn harmony_app::community_state_sync::IdentityResolver> =
        Arc::new(SingleIdentityResolver {
            addr: admin,
            identity_pub: identity_a_pub,
        });

    let engine_b = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        community_id,
        membership_key: mk,
        admin_addr: admin,
        is_invite_only: false,
        device_id: "b-dev".into(),
        state: Arc::clone(&state_b),
        tracker: Arc::clone(&tracker_b),
        content_store: cs_b,
        publisher_tx: b_out_tx,
        subscriber_rx: b_in_rx,
        paths: harmony_app::community_state_sync::PersistPaths {
            crdt: std::env::temp_dir().join("b_crdt.cbor"),
            replay: std::env::temp_dir().join("b_replay.cbor"),
        },
        debounce_ms: harmony_app::community_state_sync::DEFAULT_DEBOUNCE_MS,
        identity_resolver: Some(identity_resolver),
        error_tx: None,
    });

    // Trigger A's publish. B's subscriber arm should fire and merge.
    engine_a.flush_now().await.expect("flush_now");

    // Give B a moment to process.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let sb = state_b.lock().await;
    assert_eq!(sb.events.len(), 1, "B should have merged A's event");
    drop(sb);

    engine_a.shutdown().await.expect("shutdown a");
    engine_b.shutdown().await.expect("shutdown b");
}

struct SingleIdentityResolver {
    addr: OwnerAddr,
    identity_pub: [u8; 64],
}

impl harmony_app::community_state_sync::IdentityResolver for SingleIdentityResolver {
    fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
        if *addr == self.addr {
            Some(self.identity_pub)
        } else {
            None
        }
    }
}
```

Note: this test introduces `IdentityResolver` and a builder method `with_identity_resolver` on the engine config. Step 8.3 adds these.

- [ ] **Step 8.2: Run test to verify it fails**

```bash
cd src-tauri
cargo test --test community_sync_engine_unit engine_receives_remote 2>&1 | tail -10
```

Expected: compile errors — `IdentityResolver`, `with_identity_resolver` don't exist.

- [ ] **Step 8.3: Implement `IdentityResolver` + subscriber arm**

In `src-tauri/src/community_state_sync.rs`, add:

```rust
/// Trait for resolving an OwnerAddr to its 64-byte identity_pub at
/// verify time. Phase 2's CommunitySyncEngine needs this to
/// authorize incoming events — the actor's identity_pub must hash to
/// event.actor (pubkey-to-claimed-signer binding).
///
/// Production implementation is a thin wrapper over Sub-A's
/// owner-device cache. Tests use a static mapping.
pub trait IdentityResolver: Send + Sync {
    fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]>;
}

/// Outcome of processing one inbound state-root publish. Mirrors the
/// `IncomingOutcome` enum from owner_state_sync — the variants
/// distinguish where the failure happened so the caller persists
/// only when local state actually changed.
#[derive(Debug)]
enum IncomingOutcome {
    /// `would_accept` rejected the wire HLC at the early replay-check
    /// (step 2). No state change. Don't persist.
    Duplicate,
    /// Tracker advanced AND ≥ 1 new event was Inserted into the CRDT.
    /// Persist both `crdt.cbor` and `replay.cbor`.
    Mutated,
    /// Tracker advanced but every event in the remote blob was already
    /// in our log (`AlreadyKnown`). The CRDT is byte-identical; only
    /// `replay.cbor` needs to flush. Distinguishing this from `Mutated`
    /// lets Task 10's persist path skip the larger `crdt.cbor` fsync
    /// when a peer re-broadcasts the same event set with an advanced
    /// clock.
    MutatedTrackerOnly,
    /// Failure occurred BEFORE step 6 (decrypt-root, payload decode,
    /// blob fetch, blob decrypt, blob decode, misrouted-blob check).
    /// No state change. Don't persist.
    ErrPreMutation(CommunitySyncError),
    /// Failure occurred AFTER the tracker advanced. Tracker is in-
    /// memory dirty; persist defensively so a restart doesn't replay
    /// the same publish.
    ErrPostMutation(CommunitySyncError),
}

impl IncomingOutcome {
    /// Whether the disk needs flushing. Task 10's `persist_both` is
    /// the broad case (CRDT + replay); for `MutatedTrackerOnly` callers
    /// can use `persist_replay_only` to skip the CRDT fsync.
    fn needs_persist(&self) -> bool {
        matches!(
            self,
            Self::Mutated | Self::MutatedTrackerOnly | Self::ErrPostMutation(_)
        )
    }

    /// Whether the CRDT itself changed (≥ 1 event Inserted). Used by
    /// Task 10 to decide between `persist_both` and `persist_replay_only`.
    fn crdt_mutated(&self) -> bool {
        matches!(self, Self::Mutated | Self::ErrPostMutation(_))
    }

    fn error(&self) -> Option<&CommunitySyncError> {
        match self {
            Self::ErrPreMutation(e) | Self::ErrPostMutation(e) => Some(e),
            Self::Duplicate | Self::Mutated | Self::MutatedTrackerOnly => None,
        }
    }
}

async fn handle_incoming_publish(
    ctx: &InternalCtx,
    wire: Vec<u8>,
) -> IncomingOutcome {
    use crate::community_membership::VerifyContext;
    use crate::owner_state_crypto::canonical_cbor_decode;

    // 1. Decrypt root publish.
    let payload_bytes = match decrypt_root_publish(&ctx.membership_key, &wire) {
        Ok(b) => b,
        Err(e) => return IncomingOutcome::ErrPreMutation(CommunitySyncError::Crypto(e)),
    };
    let payload: CommunityRootPublishPayload = match canonical_cbor_decode(&payload_bytes) {
        Ok(p) => p,
        Err(e) => return IncomingOutcome::ErrPreMutation(CommunitySyncError::CborDecode(e.to_string())),
    };

    // 2. Replay-protect via per-community RootHlcTracker.
    {
        let tracker = ctx.tracker.lock().await;
        if !tracker.would_accept(&payload.at) {
            return IncomingOutcome::Duplicate;
        }
    }

    // 3. Fetch the encrypted blob from CAS.
    let blob_ciphertext = match ctx.content_store.get(payload.root_cid).await {
        Ok(b) => b,
        Err(e) => return IncomingOutcome::ErrPreMutation(CommunitySyncError::ContentStore(e)),
    };

    // 4. Decrypt blob.
    let blob_cleartext = match decrypt_blob(&ctx.membership_key, &blob_ciphertext) {
        Ok(b) => b,
        Err(e) => return IncomingOutcome::ErrPreMutation(CommunitySyncError::Crypto(e)),
    };

    // 5. Decode CommunityState.
    let remote: CommunityState = match canonical_cbor_decode(&blob_cleartext) {
        Ok(s) => s,
        Err(e) => return IncomingOutcome::ErrPreMutation(CommunitySyncError::CborDecode(e.to_string())),
    };

    // 5b. Reject misrouted blob: blob's community_id must match
    //     the engine's expected community_id. Without this, a
    //     ContentStore-collision (vanishingly unlikely with SHA-256
    //     but cheap to gate) or buggy callsite could surface a
    //     foreign community's events under our key.
    if remote.community_id != ctx.community_id {
        return IncomingOutcome::ErrPreMutation(CommunitySyncError::CborDecode(format!(
            "remote blob community_id {:?} != expected {:?}",
            remote.community_id, ctx.community_id
        )));
    }

    // 6. Advance the replay tracker BEFORE merging events. This is
    //    the single state-mutation point — if any subsequent step
    //    fails, we mark the outcome ErrPostMutation so the caller
    //    persists tracker advance to disk (preventing replay on
    //    next-boot).
    {
        let mut tracker = ctx.tracker.lock().await;
        tracker.advance(payload.at.clone());
    }

    // 7. Merge events. Each event must re-verify against B's
    //    prior_state_at_event — we don't trust A's verification.
    let resolver = match ctx.identity_resolver.as_ref() {
        Some(r) => Arc::clone(r),
        None => {
            return IncomingOutcome::ErrPostMutation(CommunitySyncError::CborDecode(
                "no identity resolver configured — Phase 2 receive-side verify needs one".into(),
            ));
        }
    };

    let mut state = ctx.state.lock().await;
    let mut inserted_any = false;
    for event in remote.events.into_values() {
        // Skip events we already have.
        if state.events.contains_key(&event.id) {
            continue;
        }

        // Resolve identity_pub for this event's actor + (if present)
        // countersig signer. Skip-on-error, log+continue if either
        // can't be resolved — mirrors the skip-on-error pattern from
        // decrypt_inbox_entries (DM transport). A single corrupt or
        // unknown-pubkey event must not fail the whole replay.
        let actor_pub = match resolver.resolve(&event.actor) {
            Some(p) => p,
            None => {
                tracing::warn!(
                    community_id = ?ctx.community_id,
                    actor = ?event.actor,
                    "skipping incoming event: unknown actor identity_pub"
                );
                continue;
            }
        };

        let cs_pub_storage;
        let cs_pub: Option<&[u8; 64]> = match event.countersig.as_ref() {
            None => None,
            Some(cs) => match resolver.resolve(&cs.signer) {
                Some(p) => {
                    cs_pub_storage = p;
                    Some(&cs_pub_storage)
                }
                None => {
                    tracing::warn!(
                        community_id = ?ctx.community_id,
                        signer = ?cs.signer,
                        "skipping incoming event: unknown countersigner identity_pub"
                    );
                    continue;
                }
            },
        };

        let ctx_v = VerifyContext {
            expected_community_id: ctx.community_id,
            admin_addr: ctx.admin_addr,
            is_invite_only: ctx.is_invite_only,
            actor_identity_pub: &actor_pub,
            countersigner_identity_pub: cs_pub,
        };

        match state.insert_event(event, &ctx_v) {
            crate::community_state_crdt::InsertOutcome::Inserted => {
                inserted_any = true;
            }
            crate::community_state_crdt::InsertOutcome::AlreadyKnown => {
                // Skip — already in our log. Don't flip inserted_any
                // because the CRDT is unchanged; without this, every
                // duplicate Zenoh fanout echo would trigger a
                // disk-persist on the Mutated arm at Task 10.
            }
            crate::community_state_crdt::InsertOutcome::Rejected(verr) => {
                tracing::warn!(
                    community_id = ?ctx.community_id,
                    error = ?verr,
                    "skipping incoming event: verify_event rejected"
                );
                // Surface the rejection as a degraded-path report —
                // verify_event rejections at receive time are the
                // most useful signal for the frontend banner (forged
                // sigs, insufficient power, banned-actor replays
                // etc). One bad event does not block valid ones in
                // the same publish — defense-in-depth at both
                // layers (Phase 1 spec §"Defense-in-depth").
                if let Some(tx) = ctx.error_tx.as_ref() {
                    let _ = tx
                        .send(CommunityDegradedReport {
                            community_id: ctx.community_id,
                            reason_tag: "verify_event_rejected",
                            detail: format!("{verr:?}"),
                        })
                        .await;
                }
            }
        }
    }

    // The tracker advanced (step 6) regardless of whether any event
    // was Inserted. Differentiate:
    //   - inserted_any=true  → Mutated (CRDT changed; persist both)
    //   - inserted_any=false → MutatedTrackerOnly (tracker advanced
    //                          but CRDT unchanged; persist replay
    //                          only — without persisting the
    //                          tracker advance, a restart would
    //                          re-process this publish on next-boot
    //                          and waste a CAS fetch)
    //
    // Why not return Duplicate when inserted_any=false: Duplicate is
    // reserved for the EARLY exit at step 2 where would_accept
    // rejected the wire HLC outright. Once we've passed step 2 the
    // tracker has accepted, and it's now state worth persisting.
    //
    // The split between Mutated and MutatedTrackerOnly lets Task 10's
    // persist code save just `replay.cbor` when the CRDT is unchanged
    // — useful when a peer re-broadcasts the same event set with an
    // advanced clock, so we skip the larger crdt.cbor fsync.
    if inserted_any {
        IncomingOutcome::Mutated
    } else {
        IncomingOutcome::MutatedTrackerOnly
    }
}
```

**`is_invite_only` / `identity_resolver` / `error_tx` are already on `CommunitySyncEngineConfig` and `InternalCtx` from Task 6** (with `Option`/default values). Task 8 only consumes them — no new fields, no builder methods. The `IdentityResolver` trait is also already declared in Task 6; the only thing Task 8 adds at the type-system level is `IncomingOutcome` (above) and the test-only `SingleIdentityResolver` impl.

Update the subscriber arm in `internal_task` to call `handle_incoming_publish` AND emit a degraded-path report on errors:

```rust
maybe_bytes = ctx.subscriber_rx.recv(), if !inbound_closed => {
    let Some(bytes) = maybe_bytes else {
        tracing::error!(
            community_id = ?ctx.community_id,
            "community subscriber channel closed; sync inbound disabled"
        );
        if let Some(tx) = ctx.error_tx.as_ref() {
            let _ = tx
                .send(CommunityDegradedReport {
                    community_id: ctx.community_id,
                    reason_tag: "subscriber_channel_closed",
                    detail: "Zenoh adapter dropped subscriber_tx; engine in publish-only mode".into(),
                })
                .await;
        }
        inbound_closed = true;
        continue;
    };
    let outcome = handle_incoming_publish(&ctx, bytes).await;
    if let Some(err) = outcome.error() {
        tracing::warn!(community_id = ?ctx.community_id, error = %err, "community incoming publish dropped");
        // Surface the failure-class as a degraded-path report so
        // start_node's drain task can translate it into a
        // `community-state-sync-degraded` Tauri event. Per the spec
        // (§ "IPC surface → Events"), the frontend uses these to
        // surface "this community's sync is degraded" banners.
        if let Some(tx) = ctx.error_tx.as_ref() {
            let _ = tx
                .send(CommunityDegradedReport {
                    community_id: ctx.community_id,
                    reason_tag: classify_incoming_error(err),
                    detail: format!("{err}"),
                })
                .await;
        }
    }
    // Persist on Mutated | ErrPostMutation lands in Task 10.
}
```

Add a small classifier helper alongside `handle_incoming_publish`:

```rust
fn classify_incoming_error(err: &CommunitySyncError) -> &'static str {
    match err {
        CommunitySyncError::Crypto(_) => "decrypt_failed",
        CommunitySyncError::CborEncode(_) | CommunitySyncError::CborDecode(_) => "wire_decode_failed",
        CommunitySyncError::ContentStore(_) => "blob_fetch_failed",
        CommunitySyncError::TransportClosed => "transport_closed",
        CommunitySyncError::Persist(_) => "persist_failed",
    }
}
```

Stable `reason_tag` values let the frontend banner-copy switch on them without parsing free-form `detail` strings; new variants get appended over time as new failure classes surface.

- [ ] **Step 8.4: Run tests**

```bash
cd src-tauri
cargo test --test community_sync_engine_unit 2>&1 | grep "^test result:"
```

Expected: PASS (3 tests).

- [ ] **Step 8.5: Run gates + commit**

```bash
cd src-tauri
set -o pipefail
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -3
cargo test --all-targets --all-features 2>&1 | grep "^test result:" | tail -3
```

```bash
git add src-tauri/src/community_state_sync.rs src-tauri/tests/community_sync_engine_unit.rs
git commit -m "$(cat <<'EOF'
feat(zeb-217-phase2): handle_incoming_publish with verify-on-receive

Subscriber arm decrypts root publish → fetches encrypted blob from
CAS → decrypts → decodes CommunityState → re-verifies every event
through verify_event before inserting into local state. Skip-on-error
on unknown pubkeys / verify failures (mirrors decrypt_inbox_entries
pattern from DM transport).

Misrouted-blob guard (remote.community_id != engine's expected) for
defense against ContentStore-collision or buggy call sites.

Replay protection via per-community RootHlcTracker advance BEFORE
merge — IncomingOutcome::ErrPostMutation flags persist need so
next-boot doesn't replay the same publish.

IdentityResolver trait abstraction for owner-device-cache lookup
(production wiring in Task 13; tests use a static mapping).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 9: `CommunityRootHlcTracker` — strict-newer dedupe-merge monotonicity

**Files:**
- Modify: `src-tauri/src/community_state_sync.rs`
- Create: `src-tauri/tests/community_root_hlc_tracker_unit.rs`

**Bug-class to gate from day 1:** PR #81 round 3 fixed an HLC-tracker monotonicity regression where dedupe-merging two SpaceIds with the same dedupe key would clobber the per-device latest-seen HLC backward. Community state-root tracking has the same shape (per-device latest-accepted HLC) and would fail the same way without explicit testing.

- [ ] **Step 9.1: Write failing dedupe-merge monotonicity test**

Create `src-tauri/tests/community_root_hlc_tracker_unit.rs`:

```rust
//! Unit tests for CommunityRootHlcTracker — replay protection +
//! dedupe-merge monotonicity gates.

use harmony_app::community_state_sync::CommunityRootHlcTracker;
use harmony_app::owner_state_types::Hlc;

fn h(wall: u64, log: u32, dev: &str) -> Hlc {
    Hlc {
        wall_ms: wall,
        logical: log,
        device_id: dev.into(),
    }
}

#[test]
fn would_accept_returns_true_for_unseen_device() {
    let t = CommunityRootHlcTracker::default();
    assert!(t.would_accept(&h(100, 0, "a")));
}

#[test]
fn would_accept_rejects_equal_or_older() {
    let mut t = CommunityRootHlcTracker::default();
    t.advance(h(100, 0, "a"));
    assert!(!t.would_accept(&h(100, 0, "a")), "exact replay rejected");
    assert!(!t.would_accept(&h(99, 5, "a")), "older wall_ms rejected");
    assert!(t.would_accept(&h(100, 1, "a")), "later logical accepted");
    assert!(t.would_accept(&h(101, 0, "a")), "later wall_ms accepted");
}

#[test]
fn advance_does_not_regress_on_older_input() {
    // The bug-class from PR #81 round 3: if two paths ever feed the
    // tracker out of order and `advance` regresses to the older HLC,
    // the next legitimate publish from that device would be rejected
    // (it's "older than" the regressed value but we already saw a
    // newer one).
    let mut t = CommunityRootHlcTracker::default();
    t.advance(h(200, 0, "a"));
    t.advance(h(100, 0, "a")); // older — must not regress
    assert!(!t.would_accept(&h(150, 0, "a")), "still bounded by 200");
    assert!(t.would_accept(&h(201, 0, "a")), "201 > 200");
}

#[test]
fn advance_per_device_isolates_clocks() {
    let mut t = CommunityRootHlcTracker::default();
    t.advance(h(500, 0, "a"));
    // device b is unseen; new HLC accepted regardless of a's clock
    assert!(t.would_accept(&h(100, 0, "b")));
    t.advance(h(100, 0, "b"));
    assert!(!t.would_accept(&h(99, 0, "b")));
}
```

- [ ] **Step 9.2: Run tests to verify pass (advance() defensive guard already in Task 6)**

```bash
cd src-tauri
cargo test --test community_root_hlc_tracker_unit 2>&1 | grep "^test result:"
```

Expected: PASS (4 tests). The defensive guard in `advance()` (Task 6's Step 6.3) already prevents regression — these tests pin the behavior so a future refactor can't silently strip it.

- [ ] **Step 9.3: Run gates + commit**

```bash
cd src-tauri
set -o pipefail
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -3
cargo test --all-targets --all-features 2>&1 | grep "^test result:" | tail -3
```

```bash
git add src-tauri/tests/community_root_hlc_tracker_unit.rs
git commit -m "$(cat <<'EOF'
test(zeb-217-phase2): pin RootHlcTracker dedupe-merge monotonicity

Bug-class gate from PR #81 round 3: HLC tracker advance() must not
regress to older inputs. Tests cover unseen-device, equal/older
rejection, regression-resistance, and per-device isolation.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 10: `community_state_persist.rs` — disk persistence per community

**Files:**
- Create: `src-tauri/src/community_state_persist.rs`
- Modify: `src-tauri/src/lib.rs` (add `pub mod community_state_persist;`)
- Modify: `src-tauri/src/community_state_sync.rs` (re-export `PersistPaths` from `community_state_persist`; call persist-now in shutdown / wakeup arms)
- Create: `src-tauri/tests/community_state_persist_unit.rs`

**Pattern source:** `src-tauri/src/owner_state_persist.rs` — same canonical-CBOR-on-disk shape, same atomic-write-via-rename idiom, same load-tolerates-missing-file behavior.

- [ ] **Step 10.1: Write failing round-trip test**

Create `src-tauri/tests/community_state_persist_unit.rs`:

```rust
//! Unit tests for community_state_persist.rs.

use harmony_app::community_state_crdt::CommunityState;
use harmony_app::community_state_persist::{
    load_crdt, load_replay, save_crdt, save_replay, PersistError,
};
use harmony_app::community_state_sync::CommunityRootHlcTracker;
use harmony_app::owner_state_types::{Hlc, SpaceId};

#[test]
fn save_and_load_crdt_round_trips() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("crdt.cbor");

    let community_id = SpaceId([1u8; 16]);
    let original = CommunityState::new(community_id);
    save_crdt(&path, &original).expect("save");
    let loaded = load_crdt(&path, community_id).expect("load");
    assert_eq!(loaded.community_id, community_id);
    assert!(loaded.events.is_empty());
}

#[test]
fn load_crdt_missing_file_returns_empty_state() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("nonexistent.cbor");
    let community_id = SpaceId([1u8; 16]);
    let loaded = load_crdt(&path, community_id).expect("load missing");
    assert_eq!(loaded.community_id, community_id);
    assert!(loaded.events.is_empty());
}

#[test]
fn load_crdt_truncated_file_returns_err() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("truncated.cbor");
    std::fs::write(&path, b"\x82\x00").expect("write garbage");
    let result = load_crdt(&path, SpaceId([1u8; 16]));
    assert!(matches!(result, Err(PersistError::CborDecode(_))));
}

#[test]
fn save_and_load_replay_round_trips() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("replay.cbor");

    let mut tracker = CommunityRootHlcTracker::default();
    tracker.advance(Hlc {
        wall_ms: 1000,
        logical: 5,
        device_id: "dev".into(),
    });
    save_replay(&path, &tracker).expect("save");
    let loaded = load_replay(&path).expect("load");
    assert_eq!(loaded.per_device.get("dev").map(|h| h.wall_ms), Some(1000));
    assert_eq!(loaded.per_device.get("dev").map(|h| h.logical), Some(5));
}
```

- [ ] **Step 10.2: Run test to verify it fails**

```bash
cd src-tauri
cargo test --test community_state_persist_unit 2>&1 | tail -10
```

Expected: compile error — module doesn't exist.

- [ ] **Step 10.3: Implement `community_state_persist.rs`**

Create `src-tauri/src/community_state_persist.rs`:

```rust
//! Per-community CRDT + replay-tracker disk persistence.
//!
//! Mirrors `src-tauri/src/owner_state_persist.rs` shape exactly:
//! atomic save via temp-file + rename, tolerates missing files
//! (returns empty state) but surfaces decode errors so corrupted
//! state is loud rather than silent.
//!
//! Per-community files live under
//! `identity_dir/communities/{community_id_hex}/{crdt|replay}.cbor`.
//! The community_id_hex is derived once at registry construction and
//! the path is owned by the engine.

use crate::community_state_crdt::CommunityState;
use crate::community_state_sync::CommunityRootHlcTracker;
use crate::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
use crate::owner_state_types::SpaceId;
use std::path::Path;

#[derive(thiserror::Error, Debug)]
pub enum PersistError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("CBOR encode: {0}")]
    CborEncode(String),
    #[error("CBOR decode: {0}")]
    CborDecode(String),
    #[error("on-disk community_id {found:?} != expected {expected:?}")]
    CommunityIdMismatch { found: SpaceId, expected: SpaceId },
}

pub fn save_crdt(path: &Path, state: &CommunityState) -> Result<(), PersistError> {
    let bytes =
        canonical_cbor_encode(state).map_err(|e| PersistError::CborEncode(e.to_string()))?;
    write_atomic(path, &bytes)
}

pub fn load_crdt(path: &Path, expected_id: SpaceId) -> Result<CommunityState, PersistError> {
    if !path.exists() {
        return Ok(CommunityState::new(expected_id));
    }
    let bytes = std::fs::read(path)?;
    let state: CommunityState =
        canonical_cbor_decode(&bytes).map_err(|e| PersistError::CborDecode(e.to_string()))?;
    if state.community_id != expected_id {
        return Err(PersistError::CommunityIdMismatch {
            found: state.community_id,
            expected: expected_id,
        });
    }
    Ok(state)
}

pub fn save_replay(path: &Path, tracker: &CommunityRootHlcTracker) -> Result<(), PersistError> {
    let bytes =
        canonical_cbor_encode(tracker).map_err(|e| PersistError::CborEncode(e.to_string()))?;
    write_atomic(path, &bytes)
}

pub fn load_replay(path: &Path) -> Result<CommunityRootHlcTracker, PersistError> {
    if !path.exists() {
        return Ok(CommunityRootHlcTracker::default());
    }
    let bytes = std::fs::read(path)?;
    canonical_cbor_decode(&bytes).map_err(|e| PersistError::CborDecode(e.to_string()))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), PersistError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
```

Add `Serialize` + `Deserialize` to `CommunityRootHlcTracker` (needed for save_replay). In `community_state_sync.rs`:

```rust
#[derive(Default, Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CommunityRootHlcTracker {
    pub per_device: BTreeMap<String, Hlc>,
}
```

- [ ] **Step 10.4: Wire mod declaration**

In `src-tauri/src/lib.rs`, after `pub mod community_state_sync;`:

```rust
pub mod community_state_persist;
```

- [ ] **Step 10.5: Wire persist calls into `internal_task`**

In `src-tauri/src/community_state_sync.rs`, in `internal_task`:

After the debounce wakeup arm's `publish_root_now` block, add:

```rust
let persist_result = persist_both(&ctx).await;
if let Err(e) = persist_result {
    tracing::warn!(community_id = ?ctx.community_id, error = %e, "persist_both failed");
}
```

After the subscriber arm's `handle_incoming_publish` block — dispatch on whether the CRDT itself changed, so a tracker-only update skips the larger `crdt.cbor` fsync:

```rust
if outcome.needs_persist() {
    let persist_result = if outcome.crdt_mutated() {
        persist_both(&ctx).await
    } else {
        persist_replay_only(&ctx).await
    };
    if let Err(e) = persist_result {
        tracing::warn!(community_id = ?ctx.community_id, error = %e, "persist after merge failed");
    }
}
```

In the shutdown arm, add a final persist before sending the response. Shutdown always uses `persist_both` because we can't cheaply tell from outside whether the CRDT mutated since the last persist:

```rust
let persist_result = persist_both(&ctx).await;
let _ = resp_tx.send(pub_result.and(persist_result));
```

Add both helpers:

```rust
async fn persist_both(ctx: &InternalCtx) -> Result<(), CommunitySyncError> {
    use crate::community_state_persist::{save_crdt, save_replay};
    let state = ctx.state.lock().await;
    save_crdt(&ctx.paths.crdt, &state)
        .map_err(|e| CommunitySyncError::Persist(e.to_string()))?;
    drop(state);
    let tracker = ctx.tracker.lock().await;
    save_replay(&ctx.paths.replay, &tracker)
        .map_err(|e| CommunitySyncError::Persist(e.to_string()))?;
    Ok(())
}

/// Replay-only persist for the `MutatedTrackerOnly` case — every event
/// in the remote blob was AlreadyKnown but the tracker advanced. The
/// CRDT is byte-identical, so re-fsyncing `crdt.cbor` would be wasted
/// I/O on every duplicate-but-clock-advanced publish.
async fn persist_replay_only(ctx: &InternalCtx) -> Result<(), CommunitySyncError> {
    use crate::community_state_persist::save_replay;
    let tracker = ctx.tracker.lock().await;
    save_replay(&ctx.paths.replay, &tracker)
        .map_err(|e| CommunitySyncError::Persist(e.to_string()))?;
    Ok(())
}
```

The shutdown arm's `pub_result.and(persist_result)` chains them so a persist failure surfaces to the caller instead of being silently dropped (mirrors owner_state_sync's shutdown — losing the final disk flush silently corrupts next-boot replay).

- [ ] **Step 10.6: Run tests**

```bash
cd src-tauri
cargo test --test community_state_persist_unit 2>&1 | grep "^test result:"
cargo test --test community_sync_engine_unit 2>&1 | grep "^test result:"
```

Expected: PASS for both.

- [ ] **Step 10.7: Run gates + commit**

```bash
cd src-tauri
set -o pipefail
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -3
cargo test --all-targets --all-features 2>&1 | grep "^test result:" | tail -3
```

```bash
git add src-tauri/src/community_state_persist.rs src-tauri/src/community_state_sync.rs src-tauri/src/lib.rs src-tauri/tests/community_state_persist_unit.rs
git commit -m "$(cat <<'EOF'
feat(zeb-217-phase2): community_state_persist + flush hooks

Per-community CRDT + replay-tracker disk persistence. Mirrors
owner_state_persist shape: atomic write via temp+rename, tolerates
missing files, surfaces decode errors loudly. CommunityIdMismatch
guard rejects misrouted on-disk files (e.g., wrong directory copied
in by accident).

Engine flushes after debounce wakeup, after merge on incoming
publish (only when state changed — needs_persist() guard), and on
shutdown. Shutdown chains pub_result.and(persist_result) so persist
failure surfaces to the caller.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 11: `CommunitySyncRegistry` — multi-community lifecycle

**Files:**
- Modify: `src-tauri/src/community_state_sync.rs`
- Create: `src-tauri/tests/community_sync_registry_unit.rs`

- [ ] **Step 11.1: Write failing registry-spawn-and-stop test**

Create `src-tauri/tests/community_sync_registry_unit.rs`:

```rust
//! Tests for CommunitySyncRegistry — the multi-community engine
//! lifecycle manager.

use harmony_app::community_state_sync::{
    CommunityRegistryConfig, CommunitySyncRegistry, CommunitySyncEngineConfig,
    CommunityRootHlcTracker, IdentityResolver, DEFAULT_DEBOUNCE_MS,
};
use harmony_app::content_store::{ContentStore, RuntimeContentStore};
use harmony_app::owner_state_types::{MembershipKey, OwnerAddr, SpaceId};
use std::sync::Arc;
use tokio::sync::mpsc;

struct NopResolver;
impl IdentityResolver for NopResolver {
    fn resolve(&self, _: &OwnerAddr) -> Option<[u8; 64]> {
        None
    }
}

#[tokio::test]
async fn registry_spawns_and_tears_down_per_community() {
    let (cas_op_tx, _cas_op_rx) = mpsc::channel(8);
    let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx,
        std::time::Duration::from_millis(1000),
    ));

    let dir = tempfile::tempdir().expect("tempdir");

    let registry = CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_id: "dev".into(),
        content_store: cs,
        identity_resolver: Arc::new(NopResolver),
        identity_dir: dir.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
    });

    let cid_a = SpaceId([1u8; 16]);
    let mk_a = MembershipKey::new([0xa1; 32]);
    let admin_a = OwnerAddr([0xb1; 16]);

    let (a_pub_tx, _a_pub_rx) = mpsc::channel(8);
    let (_a_sub_tx, a_sub_rx) = mpsc::channel(8);
    registry
        .spawn_engine(
            cid_a,
            mk_a,
            admin_a,
            /* is_invite_only */ false,
            a_pub_tx,
            a_sub_rx,
        )
        .await
        .expect("spawn a");

    assert!(registry.has_engine(&cid_a).await);

    registry.stop_engine(&cid_a).await.expect("stop");
    assert!(!registry.has_engine(&cid_a).await);

    registry.shutdown_all().await.expect("shutdown_all");
}
```

- [ ] **Step 11.2: Run test to verify it fails**

```bash
cd src-tauri
cargo test --test community_sync_registry_unit 2>&1 | tail -10
```

Expected: compile error.

- [ ] **Step 11.3: Implement the registry**

Append to `src-tauri/src/community_state_sync.rs`:

```rust
use crate::community_state_persist::{load_crdt, load_replay};

pub struct CommunityRegistryConfig {
    pub device_id: String,
    pub content_store: Arc<dyn ContentStore>,
    pub identity_resolver: Arc<dyn IdentityResolver>,
    pub identity_dir: PathBuf,
    pub debounce_ms: u64,
    /// Optional degraded-path channel. When `Some`, the registry
    /// clones the sender into every engine's `CommunitySyncEngineConfig`,
    /// and the receiver-side (owned by start_node — Task 13) translates
    /// `CommunityDegradedReport`s into `community-state-sync-degraded`
    /// Tauri events. `None` for tests that don't assert on IPC events.
    pub error_tx: Option<mpsc::Sender<CommunityDegradedReport>>,
}

pub struct CommunitySyncRegistry {
    cfg: Arc<CommunityRegistryConfig>,
    engines: tokio::sync::Mutex<BTreeMap<SpaceId, Arc<CommunitySyncEngine>>>,
}

impl CommunitySyncRegistry {
    pub fn new(cfg: CommunityRegistryConfig) -> Self {
        Self {
            cfg: Arc::new(cfg),
            engines: tokio::sync::Mutex::new(BTreeMap::new()),
        }
    }

    fn paths_for(&self, community_id: SpaceId) -> PersistPaths {
        let id_hex: String = community_id.0.iter().map(|b| format!("{b:02x}")).collect();
        let dir = self.cfg.identity_dir.join("communities").join(&id_hex);
        PersistPaths {
            crdt: dir.join("crdt.cbor"),
            replay: dir.join("replay.cbor"),
        }
    }

    pub async fn spawn_engine(
        &self,
        community_id: SpaceId,
        membership_key: MembershipKey,
        admin_addr: OwnerAddr,
        is_invite_only: bool,
        publisher_tx: mpsc::Sender<Vec<u8>>,
        subscriber_rx: mpsc::Receiver<Vec<u8>>,
    ) -> Result<(), CommunitySyncError> {
        let mut engines = self.engines.lock().await;
        if engines.contains_key(&community_id) {
            // Idempotent — re-spawn is a no-op rather than an error
            // so the registry tolerates duplicate add events from
            // owner-state mutations.
            return Ok(());
        }

        let paths = self.paths_for(community_id);
        let initial_state =
            load_crdt(&paths.crdt, community_id)
                .map_err(|e| CommunitySyncError::Persist(e.to_string()))?;
        let initial_tracker =
            load_replay(&paths.replay)
                .map_err(|e| CommunitySyncError::Persist(e.to_string()))?;

        let state = Arc::new(Mutex::new(initial_state));
        let tracker = Arc::new(Mutex::new(initial_tracker));

        let engine = Arc::new(CommunitySyncEngine::new(CommunitySyncEngineConfig {
            community_id,
            membership_key,
            admin_addr,
            is_invite_only,
            device_id: self.cfg.device_id.clone(),
            state,
            tracker,
            content_store: Arc::clone(&self.cfg.content_store),
            publisher_tx,
            subscriber_rx,
            paths,
            debounce_ms: self.cfg.debounce_ms,
            identity_resolver: Some(Arc::clone(&self.cfg.identity_resolver)),
            error_tx: self.cfg.error_tx.clone(),
        }));

        engines.insert(community_id, engine);
        Ok(())
    }

    pub async fn has_engine(&self, community_id: &SpaceId) -> bool {
        self.engines.lock().await.contains_key(community_id)
    }

    pub async fn stop_engine(&self, community_id: &SpaceId) -> Result<(), CommunitySyncError> {
        let engine = {
            let mut engines = self.engines.lock().await;
            engines.remove(community_id)
        };
        match engine {
            Some(e) => e.shutdown().await,
            None => Ok(()),
        }
    }

    pub async fn shutdown_all(&self) -> Result<(), CommunitySyncError> {
        let engines: Vec<Arc<CommunitySyncEngine>> = {
            let mut e = self.engines.lock().await;
            std::mem::take(&mut *e).into_values().collect()
        };
        let mut last_err: Option<CommunitySyncError> = None;
        for e in engines {
            if let Err(err) = e.shutdown().await {
                tracing::warn!(error = %err, "engine shutdown failed during shutdown_all");
                last_err = Some(err);
            }
        }
        match last_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Snapshot of currently-spawned community IDs. Used by Task 12's
    /// owner-state subscription scan to compute add/remove deltas.
    pub async fn known_ids(&self) -> Vec<SpaceId> {
        self.engines.lock().await.keys().cloned().collect()
    }
}
```

Note: `CommunitySyncEngineConfig` now needs `identity_resolver: Option<Arc<dyn IdentityResolver>>` as a field (it was previously added via `with_identity_resolver` builder). Refactor: drop the builder, make it a config field directly so `spawn_engine` can pass it via the struct literal.

- [ ] **Step 11.4: Run tests**

```bash
cd src-tauri
cargo test --test community_sync_registry_unit 2>&1 | grep "^test result:"
cargo test --test community_sync_engine_unit 2>&1 | grep "^test result:"
```

Expected: PASS for both.

- [ ] **Step 11.5: Run gates + commit**

```bash
cd src-tauri
set -o pipefail
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -3
cargo test --all-targets --all-features 2>&1 | grep "^test result:" | tail -3
```

```bash
git add src-tauri/src/community_state_sync.rs src-tauri/tests/community_sync_registry_unit.rs
git commit -m "$(cat <<'EOF'
feat(zeb-217-phase2): CommunitySyncRegistry multi-community lifecycle

Owns BTreeMap<SpaceId, Arc<CommunitySyncEngine>>, paths_for() derives
per-community persist paths under identity_dir/communities/{id_hex}/,
spawn_engine loads CRDT + replay from disk before starting the engine.

Idempotent spawn (re-spawn is no-op), stop_engine awaits engine
shutdown, shutdown_all drains all engines surfacing the last error.
known_ids() snapshot for delta computation in Task 12's owner-state
subscription scan.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 12: `event_loop.rs` integration — per-community Zenoh adapter

**Files:**
- Modify: `src-tauri/src/event_loop.rs`

**Pattern source:** `src-tauri/src/event_loop.rs:265-400` (or wherever the owner-state Zenoh adapter is wired). Look for `harmony/owner/{addr_hex}/state-root-v1`. Mirror that loop but for a per-community key expression `harmony/community/{id_hex}/state-root-v1`.

- [ ] **Step 12.1: Locate the existing owner-state adapter**

```bash
grep -nE "harmony/owner/.*state-root|spawn_state_root_zenoh|publisher_tx|subscriber_rx" src-tauri/src/event_loop.rs
```

Read the function that wires owner-state's pub/sub into Zenoh.

- [ ] **Step 12.2: Add `spawn_community_state_zenoh_adapter` helper**

Append to `src-tauri/src/event_loop.rs` (or insert near the owner-state adapter function):

```rust
/// Spawn a Zenoh publisher + subscriber for a single community's
/// state-root topic. Returns the publisher's mpsc::Sender (engine
/// publishes here; bytes flow to Zenoh) and the subscriber's
/// mpsc::Sender (NOT used by caller; the function plumbs Zenoh
/// inbound to the engine's subscriber_rx via the returned in_tx).
///
/// Caller pattern:
///   let (pub_tx, sub_rx) = community_zenoh_channels();
///   spawn_community_state_zenoh_adapter(zenoh_session, community_id_hex,
///       pub_tx_recv_half, sub_rx_send_half);
///   registry.spawn_engine(.., pub_tx, sub_rx);
///
/// Mirrors the owner-state adapter at src-tauri/src/event_loop.rs:<line>
/// (run grep above to locate). Differences:
///   - Per-community topic key (community_id_hex, not addr_hex)
///   - No state-root-sync-degraded event emission here — that
///     becomes a community-state-sync-degraded event in Task 14
///     once the registry lands the IPC path.
pub fn spawn_community_state_zenoh_adapter(
    zenoh: Arc<zenoh::Session>,
    community_id_hex: String,
    mut publisher_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    subscriber_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
) -> tokio::task::JoinHandle<()> {
    let topic = format!("harmony/community/{}/state-root-v1", community_id_hex);

    tokio::spawn(async move {
        // Spawn publisher: drain publisher_rx → Zenoh put.
        let zenoh_pub = Arc::clone(&zenoh);
        let topic_pub = topic.clone();
        let pub_handle = tokio::spawn(async move {
            while let Some(bytes) = publisher_rx.recv().await {
                if let Err(e) = zenoh_pub.put(&topic_pub, bytes).await {
                    tracing::warn!(topic = %topic_pub, error = ?e, "community state-root publish failed");
                }
            }
        });

        // Spawn subscriber: forward Zenoh messages → subscriber_tx.
        let zenoh_sub = zenoh;
        let topic_sub = topic.clone();
        let sub_handle = tokio::spawn(async move {
            let subscriber = match zenoh_sub.declare_subscriber(&topic_sub).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(topic = %topic_sub, error = ?e, "failed to declare community state-root subscriber");
                    return;
                }
            };
            loop {
                match subscriber.recv_async().await {
                    Ok(sample) => {
                        let bytes = sample.payload().to_bytes().to_vec();
                        if subscriber_tx.send(bytes).await.is_err() {
                            tracing::warn!(topic = %topic_sub, "community subscriber consumer dropped — adapter exiting");
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::error!(topic = %topic_sub, error = ?e, "community state-root subscriber recv failed");
                        break;
                    }
                }
            }
        });

        let _ = pub_handle.await;
        let _ = sub_handle.await;
    })
}

/// Helper: build a (publisher_tx, subscriber_rx) pair sized for the
/// engine's expected throughput.
pub fn community_zenoh_channels() -> (
    tokio::sync::mpsc::Sender<Vec<u8>>,
    tokio::sync::mpsc::Receiver<Vec<u8>>,
    tokio::sync::mpsc::Receiver<Vec<u8>>,
    tokio::sync::mpsc::Sender<Vec<u8>>,
) {
    let (pub_tx, pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let (sub_tx, sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    (pub_tx, pub_rx, sub_rx, sub_tx)
}
```

The exact `zenoh.put` / `declare_subscriber` API may differ from what the owner-state adapter uses — match the same calls verbatim from the existing code. If the project pins a specific zenoh version, the API surface may use `.payload(bytes)` builder pattern rather than `put(topic, bytes)`. Replicate verbatim from the owner-state adapter to avoid drift.

- [ ] **Step 12.3: Verify the helper compiles**

```bash
cd src-tauri
cargo build 2>&1 | tail -10
```

Expected: clean build. Test will hit it via the Task 13 wiring.

- [ ] **Step 12.4: Run gates + commit**

```bash
cd src-tauri
set -o pipefail
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -3
cargo test --all-targets --all-features 2>&1 | grep "^test result:" | tail -3
```

```bash
git add src-tauri/src/event_loop.rs
git commit -m "$(cat <<'EOF'
feat(zeb-217-phase2): event_loop community state Zenoh adapter

spawn_community_state_zenoh_adapter pumps engine publisher_rx →
Zenoh put on harmony/community/{id_hex}/state-root-v1, and Zenoh
subscriber → engine subscriber_tx. community_zenoh_channels()
helper builds the four-channel set used by start_node + registry.

Mirrors the owner-state adapter at the corresponding offset in this
file — same topic-shape, same publisher/subscriber spawn pattern.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 13: `lib.rs::start_node` wiring + identity resolver from owner-device cache

**Files:**
- Modify: `src-tauri/src/lib.rs`

**Goal:** at boot, scan `owner_state.spaces` for `SpaceKind::Community` rows that aren't `Left`, and spawn one engine per community via the registry. Hand the registry to the global `OwnerStateContext` so Phase 3 IPCs can drive it.

- [ ] **Step 13.1: Add `OwnerDeviceCacheResolver` (production identity resolver)**

In `src-tauri/src/community_state_sync.rs`, append:

```rust
/// Identity resolver backed by Sub-A's owner-device cache. The cache
/// maps OwnerAddr → DeviceIdentityHash → identity_pub bytes via
/// RegisterDevice events; this resolver picks the FIRST recorded
/// identity_pub for the queried owner.
///
/// Semantic note on OwnerAddr ↔ DeviceIdentityHash: community_membership's
/// `event.actor: OwnerAddr` carries the SAME 16 bytes as a
/// `DeviceIdentityHash` — both are `SHA256(X25519_pub || Ed25519_pub)[:16]`
/// of the signing identity. The Phase 1 `verify_signature` enforces this
/// via `Identity::from_public_bytes(actor_identity_pub).address_hash ==
/// event.actor.0`, so the resolver must look up identity_pub by treating
/// `event.actor` as a device-hash key.
///
/// The cache stores one `OwnerDeviceEntry` per OWNER (master OwnerAddr),
/// each entry carrying a parallel-vec `(devices: Vec<DeviceIdentityHash>,
/// device_identity_pubs: Vec<Option<[u8; 64]>>)`. To resolve an
/// event-actor → identity_pub, we must iterate ALL owner entries and
/// binary-search each entry's `devices` vec for the target hash. The
/// existing `crate::dm_outbox::lookup_pubkey_for_device` helper
/// (`dm_outbox.rs:1575`) does exactly this — `OwnerDeviceCacheResolver`
/// is a thin wrapper around it that adapts the OwnerAddr ↔
/// DeviceIdentityHash newtype boundary.
pub struct OwnerDeviceCacheResolver {
    cache: Arc<Mutex<crate::owner_state_crdt::OwnerState>>,
}

impl OwnerDeviceCacheResolver {
    pub fn new(cache: Arc<Mutex<crate::owner_state_crdt::OwnerState>>) -> Self {
        Self { cache }
    }
}

impl IdentityResolver for OwnerDeviceCacheResolver {
    fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
        use crate::dm_outbox::lookup_pubkey_for_device;
        use crate::owner_state_types::DeviceIdentityHash;
        // Synchronous trait fn over an async Mutex — use try_lock so a
        // contended cache surfaces as None (treated as
        // UnknownSigningKey, which is the correct fallback) rather than
        // blocking the engine's tokio task. The Mutex is short-held in
        // production paths.
        let cache = self.cache.try_lock().ok()?;
        // OwnerAddr and DeviceIdentityHash are bytes-compatible newtypes
        // (both wrap [u8; 16]). Reinterpret without copying.
        let device_hash = DeviceIdentityHash(addr.0);
        lookup_pubkey_for_device(&cache.owner_device_cache, device_hash)
    }
}
```

The cache field is `OwnerState.owner_device_cache: OwnerDeviceCache` (verified at `owner_state_crdt.rs:46` and `owner_state_types.rs:328-331`). The Phase-1-shipped `lookup_pubkey_for_device` helper at `dm_outbox.rs:1575` already implements the parallel-vec lookup — no need to re-implement.

- [ ] **Step 13.2: Wire the registry into start_node**

In `src-tauri/src/lib.rs`, locate the section that constructs `sync_engine_arc` (around line 863-984 from your existing scan). After the owner-state SyncEngine is constructed, add:

```rust
// ZEB-217 Sub-C Phase 2: per-community state CRDT sync.
//
// degraded-path channel: each spawned engine clones the sender into
// its CommunitySyncEngineConfig. The receiver lives here and feeds a
// drain task that emits `community-state-sync-degraded` Tauri events
// per the spec (§ "IPC surface → Events"). Channel capacity is sized
// for burst-tolerance under degraded conditions (e.g., a flaky peer
// continuously republishing malformed bytes); a full channel falls
// back to dropping the report so a single noisy community can't
// starve the rest of the engine pool.
let (community_error_tx, mut community_error_rx) =
    tokio::sync::mpsc::channel::<crate::community_state_sync::CommunityDegradedReport>(64);

let community_registry: Arc<crate::community_state_sync::CommunitySyncRegistry> = {
    let resolver: Arc<dyn crate::community_state_sync::IdentityResolver> = Arc::new(
        crate::community_state_sync::OwnerDeviceCacheResolver::new(Arc::clone(&crdt_state)),
    );
    let cfg = crate::community_state_sync::CommunityRegistryConfig {
        device_id: device_id.clone(),
        content_store: Arc::clone(&content_store),
        identity_resolver: resolver,
        identity_dir: identity_dir.clone(),
        debounce_ms: crate::community_state_sync::DEFAULT_DEBOUNCE_MS,
        error_tx: Some(community_error_tx),
    };
    Arc::new(crate::community_state_sync::CommunitySyncRegistry::new(cfg))
};

// Spawn the drain task that translates each report into a Tauri
// event. The frontend's CommunityService subscribes to this event
// (Phase 3 / Phase 5) to surface the "this community's sync is
// degraded" banner.
{
    let app_handle = app.handle().clone();
    tokio::spawn(async move {
        use tauri::Emitter;
        while let Some(report) = community_error_rx.recv().await {
            let id_hex: String = report
                .community_id
                .0
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            let payload = serde_json::json!({
                "communityId": id_hex,
                "reason": report.reason_tag,
                "detail": report.detail,
            });
            if let Err(e) = app_handle.emit("community-state-sync-degraded", payload) {
                tracing::warn!(error = ?e, "failed to emit community-state-sync-degraded");
            }
        }
        tracing::info!("community-state-sync-degraded drain task exiting (registry shutdown)");
    });
}

// Scan owner-state for joined communities and spawn an engine each.
{
    let state_snap = crdt_state.lock().await.clone();
    for (space_id, space) in &state_snap.spaces {
        if space.kind != crate::owner_state_types::SpaceKind::Community {
            continue;
        }
        if space.left_at.is_some() {
            continue;
        }
        let mk = match space.membership_key.as_ref() {
            Some(k) => k.clone(),
            None => {
                tracing::warn!(?space_id, "community Space missing membership_key — skipping engine spawn");
                continue;
            }
        };
        let admin = match space.admin_addr {
            Some(a) => a,
            None => {
                tracing::warn!(?space_id, "community Space missing admin_addr — skipping engine spawn");
                continue;
            }
        };
        let is_invite_only = space.is_invite_only.unwrap_or(false);

        let (pub_tx, pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let (sub_tx, sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

        let id_hex: String = space_id.0.iter().map(|b| format!("{b:02x}")).collect();
        crate::event_loop::spawn_community_state_zenoh_adapter(
            Arc::clone(&zenoh_session),
            id_hex,
            pub_rx,
            sub_tx,
        );

        if let Err(e) = community_registry
            .spawn_engine(*space_id, mk, admin, is_invite_only, pub_tx, sub_rx)
            .await
        {
            tracing::error!(?space_id, error = %e, "failed to spawn community engine");
        }
    }
}
```

The `zenoh_session` variable name above is illustrative — match the actual variable in `start_node` that holds the Zenoh session. Likely `zenoh` or `zenoh_session_arc`; verify via `grep -n "zenoh::Session\|zenoh_session" src-tauri/src/lib.rs`.

The `app` reference in the degraded-path drain task is the Tauri `App` / `AppHandle` parameter that `start_node` already accepts (Phase 1 + ZEB-228 use it for `app.emit(...)` calls). Match the actual variable name in this codebase — likely `app` or `app_handle`; the call surface needed is `app.handle().clone()` and `app_handle.emit("event-name", payload)`. If `start_node` doesn't already carry an `AppHandle`, take one as an additional argument (Phase 1's owner-state SyncEngine wiring may have established the precedent — verify via `grep -n "tauri::AppHandle\|app_handle\|app.emit" src-tauri/src/lib.rs`).

Add the registry to the global state struct (`OwnerStateContext` or whichever struct holds the SyncEngine). Phase 3 IPC will reach it through this struct.

- [ ] **Step 13.3: Run gates**

```bash
cd src-tauri
set -o pipefail
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -10
cargo test --all-targets --all-features 2>&1 | grep "^test result:" | tail -3
```

Expected: clean build + all gates green. start_node has no integration test of its own at this layer — Task 14 ships the two-process integration test.

- [ ] **Step 13.4: Commit**

```bash
git add src-tauri/src/community_state_sync.rs src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(zeb-217-phase2): wire CommunitySyncRegistry into start_node

OwnerDeviceCacheResolver bridges Sub-A's RegisterDevice cache to
verify_event's identity_pub lookup. start_node scans owner-state for
SpaceKind::Community rows that aren't Left, spawns one engine per
community with paired Zenoh adapter, and stashes the registry in
the global state context for Phase 3 IPC consumption.

No IPC surface yet — Phase 3 ships create_community / redeem_invite
/ leave_community / list_community_members against this registry.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 14: Two-member integration test (full DAG-sync round-trip + degraded paths)

**Files:**
- Create: `src-tauri/tests/community_sync_integration.rs`

**Goal:** end-to-end test that exercises the full Phase 2 stack — two `CommunitySyncRegistry` instances, paired ContentStores, paired Zenoh sessions (or paired mpsc forwarders standing in for Zenoh in tests), publish from A, subscribe on B, verify event materialization. Plus the degraded paths from the spec's "Bug-class coverage" section.

- [ ] **Step 14.1: Write the round-trip integration test**

Create `src-tauri/tests/community_sync_integration.rs`:

```rust
//! Integration tests for Phase 2: two-member community DAG-syncs the
//! full event log; degraded paths are surfaced cleanly.

use harmony_app::community_membership::{
    sign_event_with_identity, EventPayload, MembershipEventKind, VerifyContext,
};
use harmony_app::community_state_crdt::{CommunityState, InsertOutcome};
use harmony_app::community_state_sync::{
    decrypt_root_publish, encrypt_root_publish, CommunityRegistryConfig, CommunityRootHlcTracker,
    CommunitySyncEngine, CommunitySyncEngineConfig, CommunitySyncRegistry, IdentityResolver,
    DEFAULT_DEBOUNCE_MS,
};
use harmony_app::content_store::{CasOp, ContentStore, ContentStoreError, RuntimeContentStore};
use harmony_app::owner_state_types::{Hlc, MembershipKey, OwnerAddr, SpaceId};
use harmony_identity::PrivateIdentity;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};

struct StaticResolver {
    map: std::collections::HashMap<OwnerAddr, [u8; 64]>,
}
impl IdentityResolver for StaticResolver {
    fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
        self.map.get(addr).copied()
    }
}

/// Spawn a shared in-memory CAS servicer. Returns a sender pair that
/// both registries can clone for their RuntimeContentStores.
fn spawn_shared_cas() -> mpsc::Sender<CasOp> {
    let (tx, mut rx) = mpsc::channel::<CasOp>(64);
    let store: Arc<Mutex<std::collections::HashMap<harmony_content::cid::ContentId, Vec<u8>>>> =
        Arc::new(Mutex::new(std::collections::HashMap::new()));
    tokio::spawn(async move {
        while let Some(op) = rx.recv().await {
            match op {
                CasOp::PutLocal { cid, bytes, resp } => {
                    store.lock().await.insert(cid, bytes);
                    let _ = resp.send(Ok(()));
                }
                CasOp::GetLocal { cid, resp } => {
                    let v = store.lock().await.get(&cid).cloned();
                    let _ = resp.send(v.ok_or(ContentStoreError::NotFound(cid)));
                }
            }
        }
    });
    tx
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_members_dag_sync_full_event_log() {
    let cas_tx = spawn_shared_cas();
    let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_tx,
        Duration::from_millis(2000),
    ));

    let community_id = SpaceId([1u8; 16]);
    let mk = MembershipKey::new([0x42; 32]);

    let id_admin = PrivateIdentity::generate();
    let admin = OwnerAddr(id_admin.identity.address_hash);
    let admin_pub = id_admin.identity.to_public_bytes();

    let mut resolver_map = std::collections::HashMap::new();
    resolver_map.insert(admin, admin_pub);
    let resolver: Arc<dyn IdentityResolver> = Arc::new(StaticResolver { map: resolver_map });

    // Wire: A's publisher → B's subscriber.
    let (a_pub_tx, mut a_pub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (b_sub_tx, b_sub_rx) = mpsc::channel::<Vec<u8>>(64);
    tokio::spawn(async move {
        while let Some(bytes) = a_pub_rx.recv().await {
            let _ = b_sub_tx.send(bytes).await;
        }
    });

    let dir_a = tempfile::tempdir().expect("tempdir A");
    let dir_b = tempfile::tempdir().expect("tempdir B");

    let registry_a = CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_id: "a-dev".into(),
        content_store: Arc::clone(&cs),
        identity_resolver: Arc::clone(&resolver),
        identity_dir: dir_a.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
    });
    let registry_b = CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_id: "b-dev".into(),
        content_store: Arc::clone(&cs),
        identity_resolver: Arc::clone(&resolver),
        identity_dir: dir_b.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
    });

    // B's publisher and A's subscriber are unused in this test (one-
    // way A→B sync) but needed for spawn_engine signature.
    let (b_pub_tx, _b_pub_rx) = mpsc::channel(8);
    let (_a_sub_tx, a_sub_rx) = mpsc::channel(8);

    registry_a
        .spawn_engine(community_id, mk.clone(), admin, false, a_pub_tx, a_sub_rx)
        .await
        .expect("spawn a");
    registry_b
        .spawn_engine(community_id, mk, admin, false, b_pub_tx, b_sub_rx)
        .await
        .expect("spawn b");

    // Inject a Join event into A's CRDT directly (Phase 3 IPC ships
    // the user-facing path; this test bypasses to focus on sync).
    {
        // ... reach into registry_a's engine state and insert
        // ... (Phase 3 will ship a public IPC; for Phase 2 we expose
        // an internal helper or grab the Arc<Mutex<CommunityState>>
        // via a registry method).
    }

    // FlushNow on A should publish; B's subscriber should fire.
    // ... assert B's CRDT has the event after a brief delay.
    // ... shutdown both registries.
}
```

Phase 2's CommunitySyncRegistry doesn't currently expose a way to grab the inner `Arc<Mutex<CommunityState>>` from outside (it's owned by the engine). For this test we need an accessor. Add to `CommunitySyncRegistry`:

```rust
/// Returns a clone of the engine's CommunityState Arc for a community,
/// if an engine is spawned for it. Test-only — production callers go
/// through Phase 3's IPC layer.
#[doc(hidden)]
pub async fn state_for(&self, community_id: &SpaceId) -> Option<Arc<Mutex<CommunityState>>> {
    self.engines.lock().await.get(community_id).map(|e| e.state())
}
```

And on `CommunitySyncEngine`, expose:

```rust
/// Returns a clone of the inner CommunityState Arc.
pub(crate) fn state(&self) -> Arc<Mutex<CommunityState>> {
    Arc::clone(&self.state)
}
```

Which means `CommunitySyncEngine` needs to retain the `Arc<Mutex<CommunityState>>` as a field (currently it lives only inside `InternalCtx`). Hold a clone in the `CommunitySyncEngine` struct:

```rust
pub struct CommunitySyncEngine {
    notify_dirty: Arc<Notify>,
    has_pending_dirty: Arc<AtomicBool>,
    flush_now_tx: mpsc::Sender<...>,
    shutdown_tx: mpsc::Sender<...>,
    task: Mutex<Option<JoinHandle<()>>>,
    state: Arc<Mutex<CommunityState>>,
    // also retain admin_addr for read-side materialize() calls
    admin_addr: OwnerAddr,
}
```

Update `new()` to clone state into the engine before passing to InternalCtx.

Now finish the test:

```rust
    // Inject a Join event into A's CRDT.
    let state_a = registry_a
        .state_for(&community_id)
        .await
        .expect("engine spawned");
    {
        let mut sa = state_a.lock().await;
        let payload = EventPayload {
            id: [9u8; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: admin,
            at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "a-dev".into(),
            },
        };
        let event = sign_event_with_identity(payload, &id_admin).expect("sign");
        let outcome = sa.insert_event(
            event,
            &VerifyContext {
                expected_community_id: community_id,
                admin_addr: admin,
                is_invite_only: false,
                actor_identity_pub: &admin_pub,
                countersigner_identity_pub: None,
            },
        );
        assert!(matches!(outcome, InsertOutcome::Inserted));
    }

    // Trigger A's publish.
    let engine_a = {
        let engines = registry_a.engines.lock().await;
        Arc::clone(engines.get(&community_id).expect("engine"))
    };
    // engines field is private — expose via registry method:
    registry_a.flush_now(&community_id).await.expect("flush");

    // Give B's subscriber a window to process.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let state_b = registry_b
        .state_for(&community_id)
        .await
        .expect("engine spawned");
    let sb = state_b.lock().await;
    assert_eq!(sb.events.len(), 1, "B should have merged A's event");

    drop(sb);
    registry_a.shutdown_all().await.expect("shutdown a");
    registry_b.shutdown_all().await.expect("shutdown b");
}
```

`registry.flush_now` needs to be added — a thin wrapper that calls into the named engine's `flush_now()`.

- [ ] **Step 14.2: Add the round-trip test + supporting registry accessors**

Implement the missing pieces (`state_for`, `flush_now`, `state()` on engine) per Step 14.1's notes.

- [ ] **Step 14.3: Run the round-trip test**

```bash
cd src-tauri
cargo test --test community_sync_integration two_members_dag_sync 2>&1 | tail -10
```

Expected: PASS.

- [ ] **Step 14.4: Add degraded-path tests**

Append to `src-tauri/tests/community_sync_integration.rs`:

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forged_signature_event_is_rejected_on_receive() {
    // Setup similar to two_members_dag_sync_full_event_log, but A
    // crafts an event with an invalid signature (e.g., flips one bit
    // before publishing). B's verify_event should reject; B's CRDT
    // remains empty.
    // ... (follows the same scaffolding)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_wire_packet_does_not_panic_engine() {
    // Inject a random byte-blob into B's subscriber_rx. The
    // CommunityCryptoError::AeadFailed → IncomingOutcome::ErrPreMutation
    // path must surface a tracing::warn but the engine task must
    // remain alive (verify by sending a valid publish afterward and
    // confirming it processes).
    // ...
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replay_of_same_root_publish_is_idempotent() {
    // Forward the same wire packet twice. RootHlcTracker on B
    // accepts the first; the second triggers IncomingOutcome::Duplicate
    // and is silently dropped. B's CRDT shows exactly one event.
    // ...
}
```

Implement each with the same scaffolding helpers (`spawn_shared_cas`, `StaticResolver`, etc.) factored out.

- [ ] **Step 14.5: Run all integration tests**

```bash
cd src-tauri
cargo test --test community_sync_integration 2>&1 | grep "^test result:"
```

Expected: PASS (4 tests).

- [ ] **Step 14.6: Run gates + commit**

```bash
cd src-tauri
set -o pipefail
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -3
cargo test --all-targets --all-features 2>&1 | grep "^test result:" | tail -3
```

```bash
git add src-tauri/src/community_state_sync.rs src-tauri/tests/community_sync_integration.rs
git commit -m "$(cat <<'EOF'
test(zeb-217-phase2): two-member DAG-sync integration + degraded paths

End-to-end: A and B run their own CommunitySyncRegistry instances
sharing an in-memory CAS. A injects a Join event, flush_now publishes
the encrypted root, B's subscriber decrypts + DAG-syncs + merges +
verifies. B's CRDT then shows the event.

Degraded paths covered:
- Forged signature → verify_event rejection at receive (defense-in-
  depth — peers don't trust each other's verification)
- Malformed wire bytes → AeadFailed surfaces as IncomingOutcome::
  ErrPreMutation, engine task remains alive
- Replay of same root_publish → RootHlcTracker dedupes as
  IncomingOutcome::Duplicate

CommunitySyncRegistry::state_for + flush_now accessors are gated as
test-only (#[doc(hidden)]) — Phase 3 IPC will ship the public API.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 15: Push branch + open Phase 2 PR

**Files:** none modified — branch push + PR creation only.

- [ ] **Step 15.1: Verify branch state**

```bash
git status
git log --oneline main..HEAD
```

Expected: clean working tree, ~14 commits since `main` (Tasks 1-14).

- [ ] **Step 15.2: Final full-suite gate run**

```bash
cd src-tauri
set -o pipefail
cargo fmt --all -- --check
echo "FMT_EXIT=${PIPESTATUS[0]}"
cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -5
echo "CLIPPY_EXIT=${PIPESTATUS[0]}"
cargo test --all-targets --all-features 2>&1 | grep "^test result:" | tail -10
echo "TEST_EXIT=${PIPESTATUS[0]}"
cd ..
npx vitest run 2>&1 | tail -5
echo "VITEST_EXIT=${PIPESTATUS[0]}"
npx tsc --noEmit
echo "TSC_EXIT=${PIPESTATUS[0]}"
```

Expected: all five exit codes 0; cargo test counts have grown over baseline (Phase 1's 707 + ~15 new Phase 2 tests = 720+).

- [ ] **Step 15.3: Push the branch**

```bash
git push -u origin zeb-217-sub-c-phase2-state-crdt-sync
```

- [ ] **Step 15.4: Open the PR**

```bash
gh pr create --title "feat(zeb-217): Sub-C Phase 2 — per-community state CRDT + encrypted Zenoh sync" --body "$(cat <<'EOF'
## Summary

ZEB-217 Sub-C Phase 2 — multi-owner per-community CRDT replicates across members via the encrypted state-root topic. Mirrors ZEB-215 Phase 3a/3b architecture but multi-instance: one `CommunitySyncEngine` per joined community, lifecycled by `CommunitySyncRegistry`. Verification fires at receive time using the SAME `verify_event` + `prior_state_at_event` helpers Phase 1 ships, so author-side and receiver-side authorization can't drift.

- New: `community_state_crdt.rs` (`CommunityState` with materialized-view cache + version counter)
- New: `community_state_sync.rs` (`CommunitySyncEngine` + `CommunityRootHlcTracker` + `CommunitySyncRegistry` + AEAD helpers + `IdentityResolver` + `OwnerDeviceCacheResolver`)
- New: `community_state_persist.rs` (per-community CRDT + replay disk persistence)
- Modified: `event_loop.rs` (per-community Zenoh adapter helper)
- Modified: `lib.rs::start_node` (scan owner-state for community Spaces, spawn engine per community)

## What's NOT in this PR (deferred to Phase 3+)

- IPC commands (`create_community`, `redeem_invite`, `leave_community`, `list_community_members`) → Phase 3
- Reticulum invite-only counter-sig flow → Phase 4
- Frontend / deep-link → Phase 5

This PR ships a fully-working sync layer with NO user-visible surface — exercised entirely through Rust integration tests.

## Bug-class gates from PR #81 retro

- HLC tracker monotonicity preservation on dedupe-merge: pinned in `community_root_hlc_tracker_unit.rs`
- Misrouted-blob defense: `remote.community_id != engine's expected` in `handle_incoming_publish`
- Skip-on-error in materialization: unknown identity_pub or verify_event rejection on a single event does not fail the whole replay
- Persist-on-mutation: `IncomingOutcome::ErrPostMutation` flags persist need so next-boot doesn't replay the same publish

## Test plan
- [ ] `cargo test --all-targets --all-features` green (Phase 1's 707 + ~15 new Phase 2 tests)
- [ ] `cargo clippy --all-targets --all-features -- -D warnings` clean
- [ ] `cargo fmt --all -- --check` clean
- [ ] `vitest run` and `tsc --noEmit` clean (no frontend changes — baseline)
- [ ] Manual two-process LAN smoke is deferred to Phase 5 (when the IPC layer + UI ships)

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 15.5: Surface PR URL to user**

The PR creation command prints the URL. Confirm it appears in the assistant's reply so the user can navigate to it.

---

## Self-Review Checklist

Run these inline before declaring the plan finished. Fix any failures by editing this plan in place; no need to re-review after fixes.

- [ ] **Spec coverage:** every requirement in the Phase 2 section of `docs/specs/2026-05-05-zeb-217-sub-c-communities-design.md` maps to at least one task above:
    - Per-community Prolly Tree → Tasks 1-3 (CommunityState struct + cache; we ship encrypted-blob-via-CAS, not literal Prolly Tree, matching what ZEB-215 actually shipped — see plan body for rationale)
    - Encrypted state-root topic → Tasks 4, 5, 7, 12
    - DAG-sync via existing CAS → Task 7 (`publish_root_now`), Task 8 (`handle_incoming_publish`)
    - Per-community RootHlcTracker → Task 9
    - HLC monotonicity on dedupe-merge → Task 9
    - Verify-on-receive → Task 8
    - `community-state-sync-degraded` event → emitted by Task 13's drain task; engines push `CommunityDegradedReport`s on their `error_tx`, the registry-level receiver translates each into a Tauri event with `{ communityId, reason, detail }`
    - Persistence → Task 10
    - Multi-community lifecycle → Task 11
    - start_node wiring → Task 13

- [ ] **Placeholder scan:** no "TBD", "TODO", "implement later", "fill in details" anywhere in the plan body. The `REPLACE_AFTER_FIRST_GENERATION` marker in Task 5.1 is an INTENTIONAL placeholder that the implementer fills with captured bytes in Step 5.4 — this is the same pattern Phase 1's wire-format fixture tasks used.

- [ ] **Type consistency:** identifiers used in later tasks match earlier definitions. Confirmed (informally — relied on cross-referencing as I wrote):
    - `CommunityState`, `CommunitySyncEngine`, `CommunitySyncEngineConfig`, `CommunityRootHlcTracker`, `CommunitySyncRegistry`, `CommunityRegistryConfig`, `IdentityResolver`, `OwnerDeviceCacheResolver`, `CommunityRootPublishPayload`, `InsertOutcome`, `IncomingOutcome`, `PersistPaths`, `PersistError`, `CommunitySyncError`, `CommunityCryptoError` all stable across tasks
    - `MembershipKey`, `OwnerAddr`, `SpaceId`, `Hlc`, `EventId`, `SignedMembershipEvent`, `EventPayload`, `VerifyContext`, `VerifyError` all from Phase 1 (already shipped)

- [ ] **Reads in dependency order:** Task N never references a method that's first introduced in Task M > N without the body of Task N including that method's full signature. Checked.

- [ ] **Every TDD-shaped task ends with a commit:** confirmed (Tasks 1-14 all have `git commit` in their final step; Task 0 + Task 15 are pre-flight / push, no commit).

- [ ] **Cargo fmt + clippy gates appear in every task verification:** confirmed (per the user-memory hard rule that fmt is required, not just clippy).

- [ ] **Pipe exit codes use `set -o pipefail` or `${PIPESTATUS[0]}`:** confirmed.

---

## Execution Handoff

**Plan complete and saved to `docs/plans/2026-05-05-zeb-217-sub-c-phase2-state-crdt-sync-plan.md`.**

Two execution options:

1. **Subagent-Driven (recommended)** — dispatch a fresh subagent per task, two-stage review (spec compliance + code quality) between tasks, fast iteration. Per the `superpowers:subagent-driven-development` skill.

2. **Inline Execution** — execute tasks in this session via `superpowers:executing-plans`, batch execution with checkpoints for human review.

**Phase 2 implementation will branch off `origin/main` AFTER the Phase 2 docs PR (this plan + spec refresh + Phase 1 plan archive) merges.** The implementation branch is `zeb-217-sub-c-phase2-state-crdt-sync`; do not start it on the docs branch.
