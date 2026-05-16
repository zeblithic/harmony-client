# ZEB-287 Implementation Plan: community forking UX (disclosure + descendants + chain)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land the user-visible follow-ups for the Phase 1 forking primitive ([ZEB-285](https://linear.app/zeblith/issue/ZEB-285)) — polycentric-framing disclosure, chronological descendants list, and multi-hop fork-of-fork ancestor chain — all surfaced inside a renamed-and-restructured "Forks" section in `CommunitySettingsPanel.svelte`.

**Architecture:** Extend `PreForkSnapshot` and `CommunityState` with a baked-in `parent_lineage: Vec<ParentLineageEntry>` field that travels in fork-invites and persists in community state. Add two new Tauri IPCs (`list_community_forks`, `get_community_lineage`) that expose the data behind tight DTOs. Build a new `ForkLineageTree.svelte` component that renders ancestors → "you are here" → descendants in a single semantic `<ul role="tree">`. All wire-format additions are backwards-compatible via `skip_serializing_if + default`.

**Tech stack:** Rust (Tauri backend), TypeScript + Svelte 5 (frontend), `serde_cbor` (wire format), `tauri::test::mock_app` (integration tests), Vitest (frontend tests), `cargo-nextest` (Rust test runner).

**Spec:** [`docs/specs/2026-05-15-zeb-285-phase2-community-forking-ux-design.md`](../specs/2026-05-15-zeb-285-phase2-community-forking-ux-design.md) (commit `376a6fb`).

**Linear:** [ZEB-287](https://linear.app/zeblith/issue/ZEB-287).

**Branch:** `zeb-285-phase2-fork-lineage-ux` (already cut from `origin/main` at `5d9044a`).

---

## File structure overview

**Created files:**
- `src/lib/components/ForkLineageTree.svelte` — tree visualization
- `src/lib/components/__tests__/ForkLineageTree.test.ts` — 8 vitest variants
- (No new Rust files — all extensions slot into existing modules)

**Modified Rust files:**
- `src-tauri/src/community_invite.rs` — `ParentLineageEntry` + `PreForkSnapshot.parent_lineage`
- `src-tauri/src/community_state_crdt.rs` — `CommunityState.parent_lineage` + `CommunityState.forked_at_wall_ms` (+ manual `Clone` / `PartialEq` impl updates)
- `src-tauri/src/community_fork.rs` — extend `PreForkSnapshot` construction to populate `parent_lineage` with 16-deep cap
- `src-tauri/src/lib.rs` — redeem-side wiring + two new `#[tauri::command]` IPCs + DTOs
- `src-tauri/tests/wire_format_zeb285_fixtures.rs` — 5 new pinning tests
- `src-tauri/tests/community_fork_integration.rs` — 3 new multi-hop integration tests

**Modified frontend files:**
- `src/lib/types.ts` — `ForkDescendantDto`, `ParentLineageDto`, `CommunityLineageDto`
- `src/lib/community-service.ts` — `listCommunityForks`, `getCommunityLineage` wrappers
- `src/lib/components/CommunitySettingsPanel.svelte` — rename Lineage → Forks, always render, mount ForkLineageTree, move "Fork this community" button into section
- `src/lib/components/__tests__/CommunitySettingsPanel.test.ts` — augment / create as needed

---

## Task 0: Pre-flight green baseline (no commit)

**Files:** none (verification only)

- [ ] **Step 1: Confirm working tree state**

Run: `git status -s`
Expected: empty output (clean tree).

Run: `git log -1 --oneline`
Expected: `376a6fb docs(zeb-285-phase2): community forking UX design spec` (or later if commits stacked).

- [ ] **Step 2: Confirm branch lineage on origin/main**

Run: `git rev-parse HEAD~1 origin/main`
Expected: both lines print the same SHA (we're 1 commit ahead of main, which is the spec commit).

- [ ] **Step 3: Run all 5 CI gates baseline-green**

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
cd .. && npx tsc --noEmit
npx vitest run
```

Expected:
- `cargo fmt --check` exit 0
- `cargo clippy` exit 0, no warnings
- `cargo nextest` — 1369 passed (baseline post-ZEB-254 merge)
- `npx tsc --noEmit` exit 0
- `npx vitest run` — 1740 passed

**Do not commit.** Task 0 is gate verification only.

If any gate fails, stop and investigate. The spec assumes ZEB-254 baseline.

---

## Task 1: Add `ParentLineageEntry` struct

**Files:**
- Modify: `src-tauri/src/community_invite.rs` (add type definition near line 400 — alongside `PreForkSnapshot`)
- Modify: `src-tauri/tests/community_invite_unit.rs` (add roundtrip test; may not exist — create if absent)

- [ ] **Step 1: Add the type definition**

In `src-tauri/src/community_invite.rs`, just BEFORE the `pub struct PreForkSnapshot { ... }` declaration (around line 400), add:

```rust
/// ZEB-287 Phase 2: one entry in a fork's ancestor chain. Frozen at the
/// time it was added to a fork's lineage; ancestor renames after this
/// do not propagate to descendants. Bundled into PreForkSnapshot.parent_lineage
/// and persisted in CommunityState.parent_lineage.
///
/// Same-length-keys invariant: CBOR keys at this nesting level are all
/// 2-char (`si`, `nm`, `at`).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ParentLineageEntry {
    /// SpaceId of this ancestor community.
    #[serde(rename = "si")]
    pub space_id: SpaceId,

    /// Display name of this ancestor at the time it was frozen.
    #[serde(rename = "nm")]
    pub name: String,

    /// wall_ms component of the Fork event that created THIS ancestor
    /// from its predecessor in the chain. `None` for the root (top of
    /// the chain — never forked, has no predecessor).
    #[serde(rename = "at", skip_serializing_if = "Option::is_none", default)]
    pub forked_at_wall_ms: Option<u64>,
}
```

- [ ] **Step 2: Write the roundtrip test**

In `src-tauri/tests/community_invite_unit.rs` (create if it doesn't yet exist; if it does, append):

```rust
use harmony_app::community_invite::ParentLineageEntry;
use harmony_app::owner_state_types::SpaceId;
use harmony_app::owner_state_crypto::{canonical_cbor_encode, canonical_cbor_decode};

#[test]
fn parent_lineage_entry_roundtrip_with_forked_at() {
    let entry = ParentLineageEntry {
        space_id: SpaceId([0x42; 16]),
        name: "Cool Community".to_string(),
        forked_at_wall_ms: Some(1_715_811_234_567),
    };
    let bytes = canonical_cbor_encode(&entry).expect("encode");
    let decoded: ParentLineageEntry =
        canonical_cbor_decode(&bytes).expect("decode");
    assert_eq!(entry, decoded);
}

#[test]
fn parent_lineage_entry_roundtrip_root_omits_at() {
    let entry = ParentLineageEntry {
        space_id: SpaceId([0x11; 16]),
        name: "Project Cool".to_string(),
        forked_at_wall_ms: None,
    };
    let bytes_with_at = canonical_cbor_encode(&entry).expect("encode");
    let decoded: ParentLineageEntry =
        canonical_cbor_decode(&bytes_with_at).expect("decode");
    assert_eq!(entry, decoded);

    // The serialized form must NOT contain the bytes for key "at" since
    // the field is skip-if-none.
    assert!(
        !bytes_with_at.windows(2).any(|w| w == b"at"),
        "skip_serializing_if = Option::is_none failed to drop the `at` key"
    );
}
```

If `owner_state_crypto::canonical_cbor_encode/decode` aren't already pub-imported by tests, check `src-tauri/src/lib.rs` for re-exports or use the appropriate path.

- [ ] **Step 3: Run the tests**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(parent_lineage_entry)'`
Expected: both pass.

- [ ] **Step 4: Run all 5 gates**

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
cd .. && npx tsc --noEmit
npx vitest run
```
Expected: all green. Rust test count: 1369 → 1371 (+2).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/community_invite.rs src-tauri/tests/community_invite_unit.rs
git commit -m "$(cat <<'EOF'
feat(zeb-287): ParentLineageEntry struct for fork chain serialization

ZEB-287 Phase 2 spec §3.1. Adds a tiny 3-field struct that represents
one entry in a fork's ancestor chain. Both PreForkSnapshot and
CommunityState gain Vec<ParentLineageEntry> fields in subsequent tasks.

Same-length-keys invariant honored: CBOR keys `si`, `nm`, `at` all 2-char.
Root entries skip the `at` key (Option::is_none + skip_serializing_if)
so they encode without the wall_ms field.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Add `PreForkSnapshot.parent_lineage` field

**Files:**
- Modify: `src-tauri/src/community_invite.rs:400-441` (extend the existing `PreForkSnapshot` declaration)
- Modify: `src-tauri/tests/community_invite_unit.rs` (add roundtrip + byte-compat tests)

- [ ] **Step 1: Add the field to PreForkSnapshot**

In `src-tauri/src/community_invite.rs`, modify the existing `PreForkSnapshot` declaration. Append the new field AFTER the existing `forked_at` field (after line 440), still inside the struct:

```rust
    /// Forker's local HLC at fork time. Informational — used to
    /// render the "Fork point" divider in the fork's unified timeline.
    /// NOT used for any verification or ordering decision.
    #[serde(rename = "ts")]
    pub forked_at: Hlc,

    /// ZEB-287 Phase 2: ordered list of ancestors above the immediate
    /// parent (root → immediate parent), frozen at fork-time. The
    /// immediate parent is encoded separately via the existing
    /// `original_community_id` / `original_community_name` fields,
    /// NOT duplicated here.
    ///
    /// Length capped at 16 entries at fork-build time (see
    /// `community_fork.rs::build_fork_snapshot`). Phase 1 fork-invites
    /// encode without this field; decoded as empty Vec via `default`.
    #[serde(rename = "pl", skip_serializing_if = "Vec::is_empty", default)]
    pub parent_lineage: Vec<ParentLineageEntry>,
}
```

- [ ] **Step 2: Update Phase 1 PreForkSnapshot construction site**

In `src-tauri/src/community_fork.rs:436-441`, the existing construction adds the new field. Update it:

```rust
    let pre_fork_snapshot = crate::community_invite::PreForkSnapshot {
        original_community_id: original_id,
        original_community_name: original_name.clone(),
        membership_events: original_events_vec,
        channel_log: channel_log_snapshot,
        identity_pubs,
        forked_at: fork_hlc.clone(),
        parent_lineage: Vec::new(), // ZEB-287 Task 4 will populate
    };
```

This makes Task 2's commit compile. Task 4 implements the real population logic.

- [ ] **Step 3: Write byte-compat + roundtrip tests**

Append to `src-tauri/tests/community_invite_unit.rs`:

```rust
use harmony_app::community_invite::PreForkSnapshot;
use harmony_app::community_membership::SignedMembershipEvent;
use harmony_app::owner_state_types::Hlc;
use std::collections::BTreeMap;

fn empty_pre_fork_snapshot_for_test() -> PreForkSnapshot {
    PreForkSnapshot {
        original_community_id: SpaceId([0x42; 16]),
        original_community_name: "Cool Community".to_string(),
        membership_events: Vec::<SignedMembershipEvent>::new(),
        channel_log: Default::default(),
        identity_pubs: BTreeMap::new(),
        forked_at: Hlc { wall_ms: 1_715_811_234_567, logical: 0, device_id: String::new() },
        parent_lineage: Vec::new(),
    }
}

#[test]
fn pre_fork_snapshot_with_empty_lineage_omits_pl_key() {
    let snap = empty_pre_fork_snapshot_for_test();
    let bytes = canonical_cbor_encode(&snap).expect("encode");
    assert!(
        !bytes.windows(2).any(|w| w == b"pl"),
        "skip_serializing_if = Vec::is_empty failed to drop `pl` key for empty lineage"
    );
}

#[test]
fn pre_fork_snapshot_with_populated_lineage_roundtrips() {
    let mut snap = empty_pre_fork_snapshot_for_test();
    snap.parent_lineage = vec![
        ParentLineageEntry {
            space_id: SpaceId([0x11; 16]),
            name: "Project Cool".to_string(),
            forked_at_wall_ms: None,
        },
        ParentLineageEntry {
            space_id: SpaceId([0x22; 16]),
            name: "Cool Community".to_string(),
            forked_at_wall_ms: Some(1_715_000_000_000),
        },
    ];

    let bytes = canonical_cbor_encode(&snap).expect("encode");
    assert!(
        bytes.windows(2).any(|w| w == b"pl"),
        "non-empty lineage must produce `pl` key"
    );

    let decoded: PreForkSnapshot = canonical_cbor_decode(&bytes).expect("decode");
    assert_eq!(snap.parent_lineage, decoded.parent_lineage);
    assert_eq!(snap.original_community_id, decoded.original_community_id);
}
```

- [ ] **Step 4: Run the new tests**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(pre_fork_snapshot)'`
Expected: both new tests pass.

- [ ] **Step 5: Run all 5 gates**

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
cd .. && npx tsc --noEmit
npx vitest run
```
Expected: all green. Rust test count: 1371 → 1373 (+2).

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/community_invite.rs src-tauri/src/community_fork.rs src-tauri/tests/community_invite_unit.rs
git commit -m "$(cat <<'EOF'
feat(zeb-287): PreForkSnapshot.parent_lineage field (skip-if-empty)

ZEB-287 Phase 2 spec §3.2. Extends PreForkSnapshot with a 7th field
carrying the baked ancestor chain. Backwards-compat preserved via
skip_serializing_if = Vec::is_empty + default — Phase 1 fork-invites
encode without `pl` and decode under Phase 2 types as empty Vec.

Construction site in community_fork.rs:436 updated to pass an empty
Vec for now; Task 4 populates with real ancestor chain.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Add `CommunityState.parent_lineage` + `CommunityState.forked_at_wall_ms`

**Files:**
- Modify: `src-tauri/src/community_state_crdt.rs:28-67` (add fields), `:94-115` (update manual `Clone` + `PartialEq`)
- Modify: `src-tauri/tests/community_state_crdt_unit.rs` (add roundtrip + byte-compat tests; may not exist — create if absent)

- [ ] **Step 1: Add the fields to CommunityState**

In `src-tauri/src/community_state_crdt.rs`, modify the existing struct. Add after the `forked_from` field (line 42, the existing Phase 1 field), still inside the struct, BEFORE `events`:

```rust
    /// ZEB-285: SpaceId of the community this one was forked from, or
    /// None for a top-level (non-fork) community. Persisted in wire form
    /// so a fork's lineage survives round-trips and is visible to anyone
    /// who decodes the state. Set once at fork creation, never mutated.
    /// Byte-compatible with pre-ZEB-285 blobs (omitted when None).
    #[serde(rename = "ff", skip_serializing_if = "Option::is_none", default)]
    pub forked_from: Option<SpaceId>,

    /// ZEB-287 Phase 2: wall_ms component of the Fork event that
    /// created THIS community from its parent. Set at redeem-time from
    /// PreForkSnapshot.forked_at.wall_ms. `None` for top-level
    /// (non-fork) communities. Byte-compatible with pre-ZEB-287 blobs
    /// (omitted when None).
    #[serde(rename = "fa", skip_serializing_if = "Option::is_none", default)]
    pub forked_at_wall_ms: Option<u64>,

    /// ZEB-287 Phase 2: ordered list of ancestors above the immediate
    /// parent (root → immediate parent). Mirrors
    /// `PreForkSnapshot.parent_lineage` — populated at redeem-time from
    /// the fork-invite snapshot. Empty for top-level communities and
    /// for Phase 1 forks (which carried no chain). Byte-compatible.
    #[serde(rename = "fl", skip_serializing_if = "Vec::is_empty", default)]
    pub parent_lineage: Vec<crate::community_invite::ParentLineageEntry>,

    /// Append-only signed event log, ...
```

(Leave the existing `events: BTreeMap<...>` line unchanged after the new fields.)

- [ ] **Step 2: Update manual `Clone` impl**

`CommunityState` has a manual `Clone` impl (in `community_state_crdt.rs:94-106`) because it carries non-`Clone` `Mutex` fields. The new fields must be added.

Replace the existing `impl Clone for CommunityState` block with:

```rust
impl Clone for CommunityState {
    fn clone(&self) -> Self {
        Self {
            community_id: self.community_id,
            forked_from: self.forked_from,
            forked_at_wall_ms: self.forked_at_wall_ms,
            parent_lineage: self.parent_lineage.clone(),
            events: self.events.clone(),
            cache: std::sync::Mutex::new(MaterializedCache::default()),
            bootstrap_hint: std::sync::Mutex::new(
                self.bootstrap_hint.lock().ok().and_then(|g| g.clone()),
            ),
        }
    }
}
```

- [ ] **Step 3: Update manual `PartialEq` impl**

Replace the existing `impl PartialEq for CommunityState` with:

```rust
impl PartialEq for CommunityState {
    fn eq(&self, other: &Self) -> bool {
        self.community_id == other.community_id
            && self.forked_from == other.forked_from
            && self.forked_at_wall_ms == other.forked_at_wall_ms
            && self.parent_lineage == other.parent_lineage
            && self.events == other.events
    }
}
impl Eq for CommunityState {}
```

- [ ] **Step 4: Update CommunityState constructors**

`CommunityState` likely has one or more constructor functions (e.g., `new`, `with_community_id`) or struct-literal sites elsewhere in the codebase. Find them with:

```bash
grep -rn "CommunityState\s*{" src-tauri/src/ src-tauri/tests/
```

For each construction site, ensure the new fields are populated:
- `forked_at_wall_ms: None` (default for new top-level state)
- `parent_lineage: Vec::new()` (default for new top-level state)

If `CommunityState` has a `Default` impl, update that too. If construction is via a builder pattern, add defaults to the builder.

- [ ] **Step 5: Write byte-compat + roundtrip tests**

In `src-tauri/tests/community_state_crdt_unit.rs` (create if absent):

```rust
use harmony_app::community_state_crdt::CommunityState;
use harmony_app::community_invite::ParentLineageEntry;
use harmony_app::owner_state_crypto::{canonical_cbor_encode, canonical_cbor_decode};
use harmony_app::owner_state_types::SpaceId;
use std::collections::BTreeMap;

fn empty_state_for_test(community_id: SpaceId) -> CommunityState {
    CommunityState {
        community_id,
        forked_from: None,
        forked_at_wall_ms: None,
        parent_lineage: Vec::new(),
        events: BTreeMap::new(),
        cache: Default::default(),
        bootstrap_hint: Default::default(),
    }
}

#[test]
fn community_state_with_no_lineage_omits_fa_and_fl_keys() {
    let state = empty_state_for_test(SpaceId([0x77; 16]));
    let bytes = canonical_cbor_encode(&state).expect("encode");
    assert!(
        !bytes.windows(2).any(|w| w == b"fa"),
        "skip_serializing_if failed to drop `fa` for None"
    );
    assert!(
        !bytes.windows(2).any(|w| w == b"fl"),
        "skip_serializing_if failed to drop `fl` for empty Vec"
    );
}

#[test]
fn community_state_with_populated_lineage_roundtrips() {
    let mut state = empty_state_for_test(SpaceId([0x88; 16]));
    state.forked_from = Some(SpaceId([0x22; 16]));
    state.forked_at_wall_ms = Some(1_715_811_234_567);
    state.parent_lineage = vec![
        ParentLineageEntry {
            space_id: SpaceId([0x11; 16]),
            name: "Project Cool".to_string(),
            forked_at_wall_ms: None,
        },
    ];

    let bytes = canonical_cbor_encode(&state).expect("encode");
    let decoded: CommunityState = canonical_cbor_decode(&bytes).expect("decode");

    assert_eq!(state, decoded);
}

#[test]
fn community_state_manual_clone_preserves_lineage() {
    let mut state = empty_state_for_test(SpaceId([0x99; 16]));
    state.parent_lineage = vec![
        ParentLineageEntry {
            space_id: SpaceId([0x11; 16]),
            name: "C".to_string(),
            forked_at_wall_ms: None,
        },
        ParentLineageEntry {
            space_id: SpaceId([0x22; 16]),
            name: "B".to_string(),
            forked_at_wall_ms: Some(123_456),
        },
    ];
    state.forked_at_wall_ms = Some(789_012);

    let cloned = state.clone();
    assert_eq!(state.parent_lineage, cloned.parent_lineage);
    assert_eq!(state.forked_at_wall_ms, cloned.forked_at_wall_ms);
}
```

- [ ] **Step 6: Run the new tests**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(community_state) + test(with_no_lineage) + test(with_populated_lineage) + test(manual_clone)'`

Or simpler: run the test file:
`cd src-tauri && cargo nextest run --locked --features test-fixtures --test community_state_crdt_unit`

Expected: 3 new tests pass.

- [ ] **Step 7: Run all 5 gates**

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
cd .. && npx tsc --noEmit
npx vitest run
```
Expected: all green. Rust test count: 1373 → 1376 (+3).

If clippy or nextest fails for OTHER construction sites that don't pass the new fields, fix those sites (they may need `..Default::default()` or explicit field assignment).

- [ ] **Step 8: Commit**

```bash
git add src-tauri/src/community_state_crdt.rs src-tauri/tests/community_state_crdt_unit.rs
git commit -m "$(cat <<'EOF'
feat(zeb-287): CommunityState.parent_lineage + forked_at_wall_ms

ZEB-287 Phase 2 spec §3.3. Adds two backwards-compatible fields to
CommunityState:
- forked_at_wall_ms: Option<u64> (CBOR `fa`, skip-if-none)
- parent_lineage: Vec<ParentLineageEntry> (CBOR `fl`, skip-if-empty)

Manual Clone + PartialEq impls updated to include the new fields
(CommunityState carries non-Clone Mutex fields and uses manual impls).

Existing CommunityState construction sites updated to pass defaults.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Populate `parent_lineage` at fork-build time (with 16-deep cap)

**Files:**
- Modify: `src-tauri/src/community_fork.rs:436-444` (replace empty `Vec::new()` placeholder with real chain construction + cap)

- [ ] **Step 1: Read forker's local CommunityState to extract its lineage**

In `src-tauri/src/community_fork.rs`, find the block around line 380-440 where `original_state_snapshot` is obtained (it's already loaded for `original_events_vec`). The forker's local `parent_lineage` and `forked_at_wall_ms` are on `original_state` (the source state being forked from).

Add a helper extraction just BEFORE the `PreForkSnapshot` construction (around line 435):

```rust
    // ZEB-287 Task 4: build the ancestor chain for the new fork.
    // Forker's own community contributes (its lineage) + (itself as the new
    // chain's terminal entry — the new fork's immediate parent).
    let mut new_parent_lineage = original_state_snapshot.parent_lineage.clone();
    new_parent_lineage.push(crate::community_invite::ParentLineageEntry {
        space_id: original_id,
        name: original_name.clone(),
        forked_at_wall_ms: original_state_snapshot.forked_at_wall_ms,
    });

    // 16-deep cap per spec §3.4 — drop OLDEST entries (root-side).
    // Decode is permissive (no cap enforcement at decode); the cap is
    // applied only when constructing new forks here.
    const MAX_LINEAGE_DEPTH: usize = 16;
    if new_parent_lineage.len() > MAX_LINEAGE_DEPTH {
        let overflow = new_parent_lineage.len() - MAX_LINEAGE_DEPTH;
        new_parent_lineage.drain(0..overflow);
    }
```

NOTE: `original_state_snapshot` is the variable name used currently for the forker's source state. The exact variable name in the current code may differ — adapt to what's actually there. Search for `original_state` or similar locals in `fork_community` body.

- [ ] **Step 2: Wire the constructed lineage into PreForkSnapshot**

Replace the existing Task 2 placeholder:

```rust
    let pre_fork_snapshot = crate::community_invite::PreForkSnapshot {
        original_community_id: original_id,
        original_community_name: original_name.clone(),
        membership_events: original_events_vec,
        channel_log: channel_log_snapshot,
        identity_pubs,
        forked_at: fork_hlc.clone(),
        parent_lineage: new_parent_lineage,  // ← Task 4: populated
    };
```

- [ ] **Step 3: Write a unit test for the chain builder logic**

Append to `src-tauri/tests/community_invite_unit.rs` (or create `community_fork_unit.rs`):

```rust
#[test]
fn build_fork_snapshot_lineage_extends_forker_chain() {
    // Simulate forker's CommunityState.parent_lineage = [C-entry]
    // and forker's community is B (forked from C). After fork:
    //   new_fork.parent_lineage = [C-entry, B-entry]
    let c_entry = ParentLineageEntry {
        space_id: SpaceId([0x11; 16]),
        name: "C".to_string(),
        forked_at_wall_ms: None, // C is root
    };
    let forker_lineage = vec![c_entry.clone()];
    let b_id = SpaceId([0x22; 16]);
    let b_name = "B".to_string();
    let b_forked_at = Some(1_700_000_000_000u64);

    // Inline the build logic (mirrors community_fork.rs):
    let mut new_lineage = forker_lineage.clone();
    new_lineage.push(ParentLineageEntry {
        space_id: b_id,
        name: b_name.clone(),
        forked_at_wall_ms: b_forked_at,
    });

    assert_eq!(new_lineage.len(), 2);
    assert_eq!(new_lineage[0], c_entry);
    assert_eq!(new_lineage[1].space_id, b_id);
    assert_eq!(new_lineage[1].name, b_name);
    assert_eq!(new_lineage[1].forked_at_wall_ms, b_forked_at);
}

#[test]
fn lineage_cap_drops_oldest_root_side_entries() {
    // Construct a 20-deep lineage; verify cap keeps newest 16.
    let mut overlong: Vec<ParentLineageEntry> = (0u8..20)
        .map(|i| ParentLineageEntry {
            space_id: SpaceId([i; 16]),
            name: format!("ancestor_{i}"),
            forked_at_wall_ms: if i == 0 { None } else { Some(i as u64) },
        })
        .collect();

    const MAX_LINEAGE_DEPTH: usize = 16;
    if overlong.len() > MAX_LINEAGE_DEPTH {
        let overflow = overlong.len() - MAX_LINEAGE_DEPTH;
        overlong.drain(0..overflow);
    }

    assert_eq!(overlong.len(), 16);
    // First entry should be ancestor_4 (oldest 4 dropped: 0,1,2,3)
    assert_eq!(overlong[0].name, "ancestor_4");
    // Last entry should be ancestor_19 (newest preserved)
    assert_eq!(overlong[15].name, "ancestor_19");
}
```

- [ ] **Step 4: Run the new tests**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(build_fork_snapshot) + test(lineage_cap)'`
Expected: both pass.

- [ ] **Step 5: Run all 5 gates**

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
cd .. && npx tsc --noEmit
npx vitest run
```
Expected: all green. Rust test count: 1376 → 1378 (+2).

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/community_fork.rs src-tauri/tests/community_invite_unit.rs
git commit -m "$(cat <<'EOF'
feat(zeb-287): populate PreForkSnapshot.parent_lineage at fork-build time

ZEB-287 Phase 2 spec §3.4. Replaces the Task 2 empty-Vec placeholder
with real chain construction:

  new_fork.parent_lineage =
    clone(forker_community.parent_lineage)
    + push({forker_community.id, forker_community.name,
            forker_community.forked_at_wall_ms})

Then applies a 16-deep cap by draining oldest (root-side) entries.

Decode is permissive — no cap enforcement; the cap applies only at
build-time so overlong chains from a future-protocol-revision client
don't fail decode under Phase 2 types.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Wire `parent_lineage` + `forked_at_wall_ms` at redeem-time

**Files:**
- Modify: `src-tauri/src/lib.rs` (in `redeem_invite_inner` — specifically where `CommunityState` is constructed from `PreForkSnapshot`-bearing invite)

- [ ] **Step 1: Find the CommunityState construction site in redeem_invite_inner**

Run: `grep -n "CommunityState\s*{" src-tauri/src/lib.rs | head -20`

Locate the construction inside `redeem_invite_inner` (or whichever inner helper builds the new fork's `CommunityState` from invite). The site populates `forked_from: Some(snapshot.original_community_id)` for fork-invites.

- [ ] **Step 2: Add Phase 2 fields to that construction site**

Add `forked_at_wall_ms` and `parent_lineage` adjacent to the existing `forked_from`:

```rust
let new_state = CommunityState {
    community_id: minted_community_id,
    forked_from: Some(invite_payload.snapshot.original_community_id),
    // ZEB-287: copy wall_ms from snapshot.forked_at (Phase 1's existing Hlc field)
    forked_at_wall_ms: Some(invite_payload.snapshot.forked_at.wall_ms),
    // ZEB-287: snapshot.parent_lineage is empty Vec for Phase 1 invites,
    // populated for Phase 2+ fork-invites.
    parent_lineage: invite_payload.snapshot.parent_lineage.clone(),
    events: /* existing */,
    cache: Default::default(),
    bootstrap_hint: /* existing — may be Default::default() or set from invite */,
};
```

(Adapt to the actual variable names and existing construction pattern in `lib.rs`. The key changes are the two new field assignments.)

If the redeem path branches between fork-invites and non-fork-invites (Phase 1 added this branch), apply the new fields ONLY in the fork-invite branch. The non-fork branch keeps `forked_at_wall_ms: None, parent_lineage: Vec::new()`.

- [ ] **Step 3: Write an integration test asserting the wire-through**

Append to `src-tauri/tests/community_fork_integration.rs` (existing file — Phase 1 added it):

```rust
#[tokio::test]
async fn redeem_fork_invite_wires_parent_lineage_into_community_state() {
    // Construct a synthetic fork-invite carrying parent_lineage = [C-entry]
    let c_entry = harmony_app::community_invite::ParentLineageEntry {
        space_id: harmony_app::owner_state_types::SpaceId([0x33; 16]),
        name: "C".to_string(),
        forked_at_wall_ms: None,
    };

    // ... use the existing Phase 1 integration test helpers to:
    // 1. Stand up an engine A
    // 2. Create community B (which acts as the "parent" of the new fork)
    // 3. Manually construct a fork-invite payload from B with
    //    parent_lineage = [c_entry] (representing C as an ancestor)
    // 4. Redeem that invite
    // 5. Read the resulting CommunityState
    // 6. Assert state.parent_lineage == [c_entry] and
    //    state.forked_at_wall_ms == Some(...)

    // The actual test should follow the shape of Phase 1's
    // `fork_invite_carries_snapshot_to_invitee` test (in this file)
    // and add assertions on the Phase 2 fields.

    // Per Phase 1's note in PR #122: full Tauri IPC chain may not be
    // testable due to ChannelLogRegistry<R: tauri::Runtime> generic;
    // exercise the payload-encoding + redeem-helper layer directly
    // following Phase 1's pattern.
}
```

Use the Phase 1 test `fork_invite_carries_snapshot_to_invitee` as the template — read it first to understand the helper API.

- [ ] **Step 4: Run the new test**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures --test community_fork_integration -E 'test(redeem_fork_invite_wires_parent_lineage)'`
Expected: pass.

- [ ] **Step 5: Run all 5 gates**

Expected: all green. Rust test count: 1378 → 1379 (+1).

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/tests/community_fork_integration.rs
git commit -m "$(cat <<'EOF'
feat(zeb-287): redeem_invite_inner wires parent_lineage + forked_at_wall_ms

ZEB-287 Phase 2 spec §3.5. When redeeming a fork-invite, the new
CommunityState picks up Phase 2 fields from the snapshot:
- forked_at_wall_ms <- snapshot.forked_at.wall_ms (from Phase 1's Hlc)
- parent_lineage <- snapshot.parent_lineage (empty Vec for Phase 1 invites)

Phase 1 fork-invites (no parent_lineage in snapshot) decode under Phase 2
types as empty Vec, so their resulting CommunityState has the correct
"I don't know my ancestry beyond my immediate parent" state.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: Add `list_community_forks` IPC + `ForkDescendantDto`

**Files:**
- Modify: `src-tauri/src/lib.rs` (add new `#[tauri::command]` near other community IPCs + DTO definition)
- Modify: existing IPC registration (likely in `lib.rs`'s `tauri::Builder::default().invoke_handler(...)` chain)

- [ ] **Step 1: Define the DTO**

In `src-tauri/src/lib.rs`, near other community DTOs (e.g., `MemberInfoDto`), add:

```rust
#[derive(serde::Serialize)]
pub struct ForkDescendantDto {
    /// Hex-encoded SpaceId of the descendant fork community.
    pub fork_space_id: String,
    /// Hex-encoded OwnerAddr of the forker.
    pub forker_addr: String,
    /// Resolved display name of the forker if currently Joined in this
    /// community, else None (UI renders fallback "an unknown member").
    pub forker_display_name: Option<String>,
    /// wall_ms of the Fork event's HLC.
    pub forked_at_wall_ms: u64,
    /// Whether the descendant community is locally known
    /// (in NavService / OwnerState). UI uses this to gate clickability.
    pub locally_known: bool,
}
```

- [ ] **Step 2: Implement the IPC**

In `src-tauri/src/lib.rs`, add the command:

```rust
/// ZEB-287 Phase 2: list visible Fork events from a community's
/// membership log. Silent forks have no Fork event and remain absent.
/// Caller must be Joined in the community.
#[tauri::command]
async fn list_community_forks(
    community_id: String,
    state: tauri::State<'_, std::sync::Arc<tokio::sync::Mutex<NodeState>>>,
) -> Result<Vec<ForkDescendantDto>, String> {
    // 1. Decode community_id from hex
    let community_id_bytes = hex::decode(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?;
    if community_id_bytes.len() != 16 {
        return Err("community_id must be 16 bytes".into());
    }
    let mut id_arr = [0u8; 16];
    id_arr.copy_from_slice(&community_id_bytes);
    let community_space_id = SpaceId(id_arr);

    // 2. Resolve the engine + state
    let g = state.lock().await;
    let engine_arc = g
        .community_registry
        .get_engine(&community_space_id)
        .ok_or_else(|| "community not found".to_string())?;
    let self_owner = g.self_owner;
    let admin_addr = engine_arc.admin_addr();
    let community_state = engine_arc.state();
    let community_state_g = community_state.lock().await;

    // 3. Authorize: caller must be Joined
    let materialized = community_state_g.materialized(admin_addr);
    let self_member_status = materialized
        .members
        .get(&self_owner)
        .map(|m| m.status);
    match self_member_status {
        Some(MemberStatus::Joined) => {}
        _ => return Err("not a member".into()),
    }

    // 4. Walk events for Fork variants + build DTOs
    let mut dtos: Vec<ForkDescendantDto> = Vec::new();
    for signed_event in community_state_g.events.values() {
        if let MembershipEventKind::Fork { fork_space_id } = &signed_event.event.kind {
            let forker_addr = signed_event.event.actor;
            let forker_display_name = materialized
                .members
                .get(&forker_addr)
                .and_then(|m| {
                    if m.status == MemberStatus::Joined {
                        // member_info_for path — adapt to actual helper
                        // signature in the codebase
                        member_info_for_helper(&materialized, &forker_addr)
                    } else {
                        None
                    }
                });
            let locally_known = g
                .owner_state
                .lock()
                .await
                .has_space(*fork_space_id);
            dtos.push(ForkDescendantDto {
                fork_space_id: hex::encode(fork_space_id.0),
                forker_addr: hex::encode(forker_addr),
                forker_display_name,
                forked_at_wall_ms: signed_event.event.at.wall_ms,
                locally_known,
            });
        }
    }

    // 5. Sort ascending by forked_at_wall_ms; stable tie-break by forker_addr
    dtos.sort_by(|a, b| {
        a.forked_at_wall_ms
            .cmp(&b.forked_at_wall_ms)
            .then_with(|| a.forker_addr.cmp(&b.forker_addr))
    });

    Ok(dtos)
}
```

NOTE: the exact symbols (`MemberStatus`, `MembershipEventKind::Fork`, `community_registry.get_engine`, `engine_arc.state()`, `engine_arc.admin_addr()`, `owner_state.has_space`) may need adapting to the actual API surface. Use `grep` to find the analogous Phase 1 patterns in `list_community_members` and mirror them.

- [ ] **Step 3: Register the command**

In `src-tauri/src/lib.rs`, find the `invoke_handler(tauri::generate_handler![...])` macro and add `list_community_forks` to the list.

- [ ] **Step 4: Write unit tests**

Append to `src-tauri/tests/community_fork_integration.rs`:

```rust
#[tokio::test]
async fn list_community_forks_resolves_active_member_name() {
    // 1. Stand up engine + community
    // 2. Mint a Fork event from a Joined member
    // 3. Call list_community_forks
    // 4. Assert returned DTO has Some(name) matching the active member
}

#[tokio::test]
async fn list_community_forks_falls_back_when_forker_kicked() {
    // 1. Same setup
    // 2. After Fork event lands, mint a Kick event removing the forker
    // 3. Call list_community_forks
    // 4. Assert returned DTO has forker_display_name: None
}

#[tokio::test]
async fn list_community_forks_marks_locally_unknown_descendants() {
    // 1. Stand up engine; community has a Fork event referencing a
    //    fork_space_id NOT in local OwnerState
    // 2. Call list_community_forks
    // 3. Assert returned DTO has locally_known: false
}

#[tokio::test]
async fn list_community_forks_rejects_non_member_caller() {
    // 1. Stand up engine; caller's self_owner is NOT Joined in community
    // 2. Call list_community_forks
    // 3. Assert Err("not a member")
}

#[tokio::test]
async fn list_community_forks_sorts_chronologically() {
    // 1. Mint 3 Fork events with wall_ms = [200, 100, 300] (out of order)
    // 2. Call list_community_forks
    // 3. Assert DTO order is wall_ms [100, 200, 300]
}
```

Use Phase 1 integration test helpers; mirror the existing test shapes.

- [ ] **Step 5: Run the new tests**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(list_community_forks)'`
Expected: 5 new tests pass.

- [ ] **Step 6: Run all 5 gates**

Expected: all green. Rust test count: 1379 → 1384 (+5).

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/tests/community_fork_integration.rs
git commit -m "$(cat <<'EOF'
feat(zeb-287): list_community_forks IPC + ForkDescendantDto

ZEB-287 Phase 2 spec §4.1 + §4.3. Walks a community's membership log
for MembershipEventKind::Fork events, resolves forker display names
via member_info_for ladder (active member → cross-community cache →
None fallback), marks descendant communities locally_known if present
in the joiner's NavService / OwnerState.

Authorization: caller must be Joined in the community (matches
list_community_members gate). Sorted ascending by wall_ms with stable
forker_addr tie-break.

Silent forks remain absent — by design, silent forks emit no Fork
event and so don't appear in any membership log walk.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: Add `get_community_lineage` IPC + `CommunityLineageDto` + `ParentLineageDto`

**Files:**
- Modify: `src-tauri/src/lib.rs` (add DTOs + command + register)

- [ ] **Step 1: Define DTOs**

In `src-tauri/src/lib.rs`, near `ForkDescendantDto`:

```rust
#[derive(serde::Serialize)]
pub struct ParentLineageDto {
    /// Hex-encoded SpaceId of this ancestor.
    pub space_id: String,
    /// Frozen display name of this ancestor at the time it was added
    /// to the chain.
    pub name: String,
    /// wall_ms of THIS community's fork-from-parent event; None for root.
    pub forked_at_wall_ms: Option<u64>,
}

#[derive(serde::Serialize)]
pub struct CommunityLineageDto {
    /// Phase 1 field: immediate parent SpaceId (hex), or None for
    /// top-level communities.
    pub forked_from: Option<String>,
    /// Phase 2 field: wall_ms of THIS community's Fork event from
    /// its parent. None for top-level or Phase 1 forks.
    pub forked_at_wall_ms: Option<u64>,
    /// Phase 2 field: ordered ancestor chain (root → immediate-parent),
    /// excluding the immediate parent (that's `forked_from`). Empty for
    /// top-level communities and Phase 1 forks.
    pub parent_lineage: Vec<ParentLineageDto>,
    /// This community's own SpaceId (hex) — convenience so frontend can
    /// render "you are here" without a second IPC.
    pub self_space_id: String,
    /// This community's own display name.
    pub self_name: String,
}
```

- [ ] **Step 2: Implement the IPC**

```rust
/// ZEB-287 Phase 2: read lineage fields from CommunityState behind a
/// tight DTO. Caller must be Joined in the community.
#[tauri::command]
async fn get_community_lineage(
    community_id: String,
    state: tauri::State<'_, std::sync::Arc<tokio::sync::Mutex<NodeState>>>,
) -> Result<CommunityLineageDto, String> {
    // 1. Decode community_id
    let community_id_bytes = hex::decode(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?;
    if community_id_bytes.len() != 16 {
        return Err("community_id must be 16 bytes".into());
    }
    let mut id_arr = [0u8; 16];
    id_arr.copy_from_slice(&community_id_bytes);
    let community_space_id = SpaceId(id_arr);

    // 2. Resolve + authorize
    let g = state.lock().await;
    let engine_arc = g
        .community_registry
        .get_engine(&community_space_id)
        .ok_or_else(|| "community not found".to_string())?;
    let self_owner = g.self_owner;
    let admin_addr = engine_arc.admin_addr();
    let community_state = engine_arc.state();
    let community_state_g = community_state.lock().await;
    let materialized = community_state_g.materialized(admin_addr);
    match materialized.members.get(&self_owner).map(|m| m.status) {
        Some(MemberStatus::Joined) => {}
        _ => return Err("not a member".into()),
    }

    // 3. Read community's own name from OwnerState's Space
    let owner_state_g = g.owner_state.lock().await;
    let self_space = owner_state_g
        .spaces
        .get(&community_space_id)
        .ok_or_else(|| "community space not found in owner state".to_string())?;
    let self_name = self_space.name.clone();

    // 4. Build DTO
    let parent_lineage_dto: Vec<ParentLineageDto> = community_state_g
        .parent_lineage
        .iter()
        .map(|e| ParentLineageDto {
            space_id: hex::encode(e.space_id.0),
            name: e.name.clone(),
            forked_at_wall_ms: e.forked_at_wall_ms,
        })
        .collect();

    Ok(CommunityLineageDto {
        forked_from: community_state_g.forked_from.map(|s| hex::encode(s.0)),
        forked_at_wall_ms: community_state_g.forked_at_wall_ms,
        parent_lineage: parent_lineage_dto,
        self_space_id: hex::encode(community_space_id.0),
        self_name,
    })
}
```

- [ ] **Step 3: Register the command** in the `tauri::generate_handler![...]` list.

- [ ] **Step 4: Write unit tests**

Append to `src-tauri/tests/community_fork_integration.rs`:

```rust
#[tokio::test]
async fn get_community_lineage_returns_phase1_state_with_default_phase2_fields() {
    // 1. Stand up a Phase 1-shape community (no parent_lineage, no forked_at_wall_ms)
    // 2. Call get_community_lineage
    // 3. Assert dto.parent_lineage == [] and dto.forked_at_wall_ms == None
}

#[tokio::test]
async fn get_community_lineage_returns_phase2_chain() {
    // 1. Stand up a community with parent_lineage = [C-entry, B-entry]
    // 2. Call get_community_lineage
    // 3. Assert dto.parent_lineage has 2 entries with correct ordering
}

#[tokio::test]
async fn get_community_lineage_rejects_non_member_caller() {
    // 1. Stand up community; caller's self_owner NOT Joined
    // 2. Call get_community_lineage
    // 3. Assert Err
}
```

- [ ] **Step 5: Run + 5-gate sweep + commit**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(get_community_lineage)'
```
Expected: 3 new tests pass.

Full 5-gate sweep. Expected: Rust test count 1384 → 1387 (+3).

Commit:

```bash
git add src-tauri/src/lib.rs src-tauri/tests/community_fork_integration.rs
git commit -m "$(cat <<'EOF'
feat(zeb-287): get_community_lineage IPC + lineage DTOs

ZEB-287 Phase 2 spec §4.2 + §4.4. Exposes CommunityState lineage
fields (forked_from, forked_at_wall_ms, parent_lineage) plus this
community's own SpaceId + name behind CommunityLineageDto. Avoids
leaking the full CommunityState wire shape to frontend.

Authorization: caller must be Joined. Phase 1 communities (no lineage
data) return DTO with empty parent_lineage + None forked_at_wall_ms.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: Wire-format pinning fixtures

**Files:**
- Modify: `src-tauri/tests/wire_format_zeb285_fixtures.rs` (extend with Phase 2 fixtures + Phase 1 backwards-compat pins)

- [ ] **Step 1: Read the existing fixture structure**

Run: `head -60 src-tauri/tests/wire_format_zeb285_fixtures.rs`

Understand the existing fixture pattern (likely `include_str!` of canonical `.hex` files + decode-roundtrip assertions).

- [ ] **Step 2: Add the 5 new fixture tests**

Append to `src-tauri/tests/wire_format_zeb285_fixtures.rs`:

```rust
// ZEB-287 Phase 2 wire-format fixtures

#[test]
fn parent_lineage_entry_canonical_cbor() {
    // Pin: SpaceId = [0x42; 16], name = "Cool Community",
    //      forked_at_wall_ms = Some(1715811234567)
    let entry = harmony_app::community_invite::ParentLineageEntry {
        space_id: harmony_app::owner_state_types::SpaceId([0x42; 16]),
        name: "Cool Community".to_string(),
        forked_at_wall_ms: Some(1_715_811_234_567),
    };
    let actual = harmony_app::owner_state_crypto::canonical_cbor_encode(&entry)
        .expect("encode");

    // Expected bytes — paste output from running this test once and capturing the
    // hex via `hex::encode(actual.as_slice())`. Regen instruction in doc-comment below.
    let expected_hex = "<FILL IN ON FIRST RUN — RUN TEST, COPY hex::encode(actual) FROM PANIC, PASTE HERE>";
    let expected = hex::decode(expected_hex).expect("decode expected hex");

    assert_eq!(
        actual, expected,
        "Wire-format drift: ParentLineageEntry canonical CBOR changed.\n\
         If intentional, regen by replacing expected_hex with hex::encode(actual)."
    );
}

#[test]
fn parent_lineage_entry_root_omits_at() {
    let entry = harmony_app::community_invite::ParentLineageEntry {
        space_id: harmony_app::owner_state_types::SpaceId([0x11; 16]),
        name: "Root".to_string(),
        forked_at_wall_ms: None,
    };
    let actual = harmony_app::owner_state_crypto::canonical_cbor_encode(&entry)
        .expect("encode");

    let expected_hex = "<FILL IN ON FIRST RUN>";
    let expected = hex::decode(expected_hex).expect("decode expected hex");

    assert_eq!(actual, expected);
    assert!(!actual.windows(2).any(|w| w == b"at"));
}

#[test]
fn pre_fork_snapshot_with_parent_lineage_canonical_cbor() {
    // Build a synthetic PreForkSnapshot with parent_lineage = [C, B]
    // Pin the canonical CBOR bytes
    // (Use empty events/channel_log/identity_pubs for fixture simplicity)
    // ... regen-on-first-run pattern
}

#[test]
fn community_state_with_parent_lineage_canonical_cbor() {
    // Synthetic CommunityState with non-empty parent_lineage + forked_at_wall_ms
    // Pin canonical CBOR
}

#[test]
fn phase1_community_state_decodes_under_phase2_types() {
    // Take the existing Phase 1 CommunityState fixture (from Phase 1's
    // fixture file or by constructing a Phase 1-shaped state manually)
    // and decode it under the Phase 2 CommunityState type.
    // Assert: parent_lineage == [] and forked_at_wall_ms == None.

    // Construct Phase 1-shape CBOR (no `fa`, no `fl` keys) by hand —
    // OR import the existing Phase 1 fixture if it exists.
    let phase1_state = harmony_app::community_state_crdt::CommunityState {
        community_id: harmony_app::owner_state_types::SpaceId([0xAA; 16]),
        forked_from: None,
        forked_at_wall_ms: None,            // Phase 2 default
        parent_lineage: Vec::new(),         // Phase 2 default
        events: std::collections::BTreeMap::new(),
        cache: Default::default(),
        bootstrap_hint: Default::default(),
    };
    let bytes = harmony_app::owner_state_crypto::canonical_cbor_encode(&phase1_state)
        .expect("encode");

    // Decode and assert
    let decoded: harmony_app::community_state_crdt::CommunityState =
        harmony_app::owner_state_crypto::canonical_cbor_decode(&bytes).expect("decode");

    assert_eq!(decoded.parent_lineage, Vec::<harmony_app::community_invite::ParentLineageEntry>::new());
    assert_eq!(decoded.forked_at_wall_ms, None);
    assert_eq!(decoded.forked_from, None);

    // Byte-compat: the encoded form must NOT contain the new keys
    assert!(!bytes.windows(2).any(|w| w == b"fa"));
    assert!(!bytes.windows(2).any(|w| w == b"fl"));
}
```

- [ ] **Step 3: Run the fixture tests + capture initial hex**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(parent_lineage_entry_canonical) + test(pre_fork_snapshot_with_parent_lineage_canonical) + test(community_state_with_parent_lineage_canonical) + test(phase1_community_state_decodes)'`

The first 4 will FAIL with `"<FILL IN ON FIRST RUN>"` placeholders. Each panic message includes the actual bytes' hex via `hex::encode(actual)`. To regenerate:

1. Modify each test temporarily to print: `eprintln!("REGEN: expected_hex = \"{}\";", hex::encode(&actual));`
2. Run the test once; capture the printed hex.
3. Paste each hex into the corresponding `let expected_hex = "..."` line.
4. Remove the `eprintln!` (or comment it out as a regen note).
5. Re-run; all 5 should pass.

- [ ] **Step 4: Run all 5 gates**

Expected: Rust test count 1387 → 1392 (+5).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/tests/wire_format_zeb285_fixtures.rs
git commit -m "$(cat <<'EOF'
test(zeb-287): wire-format pinning for Phase 2 lineage fields

ZEB-287 Phase 2 spec §7.1. Five new pinning tests:

1. parent_lineage_entry_canonical_cbor — pins a populated entry's
   serialized bytes.
2. parent_lineage_entry_root_omits_at — verifies root entries (forked_at=None)
   skip the `at` key entirely.
3. pre_fork_snapshot_with_parent_lineage_canonical_cbor — pins the
   PreForkSnapshot with a 2-entry chain.
4. community_state_with_parent_lineage_canonical_cbor — pins
   CommunityState with non-empty lineage + forked_at_wall_ms.
5. phase1_community_state_decodes_under_phase2_types — Phase 1
   wire form (no `fa`, no `fl`) decodes under Phase 2 types as
   empty Vec / None and re-encodes byte-identically.

Regen instructions inline in test bodies: on first run after type
changes, replace expected_hex placeholders with the captured
hex::encode(actual).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 9: Multi-hop integration tests

**Files:**
- Modify: `src-tauri/tests/community_fork_integration.rs` (add 3 new multi-hop tests)

- [ ] **Step 1: Read existing integration test helpers**

Run: `head -100 src-tauri/tests/community_fork_integration.rs`

Understand the test fixtures + helper functions for spinning up engines, minting communities, redeeming invites. These were established by Phase 1 (PR #122 Task 8 — 6 paired-engine integration tests).

- [ ] **Step 2: Add three_deep_fork_chain test**

Append to `community_fork_integration.rs`:

```rust
#[tokio::test]
async fn three_deep_fork_chain_preserves_lineage_through_snapshot() {
    // 1. Stand up engine A
    // 2. Engine A creates community C (top-level — no fork)
    //    Assert: C.parent_lineage == [], C.forked_at_wall_ms == None
    // 3. Engine A forks C into B (with member-A as forker)
    //    Assert: B.parent_lineage == [C-entry with forked_at_wall_ms: None]
    //            B.forked_at_wall_ms == Some(b_fork_wall_ms)
    // 4. Engine A forks B into A_FORK (still using member-A who is in both C and B)
    //    Assert: A_FORK.parent_lineage == [C-entry, B-entry]
    //            A_FORK.parent_lineage[0] == ParentLineageEntry {
    //                space_id: C.id, name: "C", forked_at_wall_ms: None
    //            }
    //            A_FORK.parent_lineage[1] == ParentLineageEntry {
    //                space_id: B.id, name: "B",
    //                forked_at_wall_ms: Some(b_fork_wall_ms)
    //            }
    //            A_FORK.forked_at_wall_ms == Some(a_fork_wall_ms)

    // Use Phase 1 helpers — establish_test_engine,
    // mint_community_for_test, fork_community_for_test, etc.
    // The Phase 1 test `fork_creates_independent_community` is the
    // closest existing template.
}
```

- [ ] **Step 3: Add lineage_depth_cap test**

```rust
#[tokio::test]
async fn lineage_depth_cap_truncates_root_side() {
    // 1. Construct a CommunityState with parent_lineage of length 20
    //    (bypass fork_community — direct CommunityState construction
    //    via the type's field-init syntax; this is testing the
    //    CAP LOGIC in community_fork.rs, not the full IPC)
    // 2. Inline the cap logic from community_fork.rs:
    //       const MAX_LINEAGE_DEPTH: usize = 16;
    //       if overlong.len() > MAX_LINEAGE_DEPTH {
    //           let overflow = overlong.len() - MAX_LINEAGE_DEPTH;
    //           overlong.drain(0..overflow);
    //       }
    // 3. Assert overlong.len() == 16
    // 4. Assert overlong[0] is the entry that was originally at index 4
    //    (the original [0..4) — i.e., 4 oldest entries — were dropped)
    // 5. Assert overlong[15] is the entry that was originally at index 19
}
```

This test is technically duplicative of the unit test from Task 4 Step 3 (`lineage_cap_drops_oldest_root_side_entries`). The integration test exercises the cap in the context of `fork_community`'s logic. If the implementer judges the unit test sufficient, this integration test can be elided. Document the decision in the commit.

- [ ] **Step 4: Add phase1_snapshot_redeems test**

```rust
#[tokio::test]
async fn phase1_snapshot_redeems_with_default_lineage() {
    // 1. Manually construct a Phase 1-shape PreForkSnapshot:
    //    - All Phase 1 fields populated
    //    - parent_lineage: Vec::new() (Phase 2's new field; defaults
    //      empty for Phase 1 invites since they didn't carry it)
    // 2. Serialize it under the Phase 2 PreForkSnapshot type (since
    //    `parent_lineage: Vec::is_empty` is skip-if-empty, this
    //    produces byte-identical output to what a Phase 1 client
    //    would have produced).
    // 3. Build a fork-invite payload around it; drive redeem
    // 4. Read resulting CommunityState
    // 5. Assert state.parent_lineage == Vec::new()
    //    Assert state.forked_at_wall_ms == Some(snapshot.forked_at.wall_ms)
    //    (Phase 2's redeem path always reads forked_at, even for
    //    legacy Phase 1 snapshots — they have forked_at, just no lineage)
}
```

- [ ] **Step 5: Run + 5-gate sweep + commit**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures --test community_fork_integration`
Expected: all integration tests pass (Phase 1's 6 + Task 5's 1 + Task 6's 5 + Task 7's 3 + Task 9's 3 = 18, depending on which tests were dispersed).

Full 5-gate sweep. Test count: 1392 → 1395 (+3, treating the cap test as the unit-test alternative).

Commit:

```bash
git add src-tauri/tests/community_fork_integration.rs
git commit -m "$(cat <<'EOF'
test(zeb-287): multi-hop integration tests for fork lineage

ZEB-287 Phase 2 spec §7.2. Three new paired-engine integration tests:

1. three_deep_fork_chain_preserves_lineage_through_snapshot —
   end-to-end C → B → A_FORK; asserts A_FORK.parent_lineage carries
   both ancestors with correct names + wall_ms.

2. lineage_depth_cap_truncates_root_side — 20-deep synthetic chain
   passed through the cap logic; verifies oldest 4 entries are dropped.

3. phase1_snapshot_redeems_with_default_lineage — Phase 1-shape
   invite (empty parent_lineage) redeems cleanly under Phase 2 types
   with default-empty lineage in the new fork's CommunityState.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 10: Frontend types + service wrappers

**Files:**
- Modify: `src/lib/types.ts` (add three new TS interfaces)
- Modify: `src/lib/community-service.ts` (add two new wrapper functions)

- [ ] **Step 1: Add TS types**

In `src/lib/types.ts`, append:

```typescript
// ZEB-287 Phase 2: types matching backend DTOs

export interface ParentLineageDto {
  /** Hex-encoded SpaceId of this ancestor. */
  space_id: string;
  /** Frozen display name of this ancestor at the time it was added. */
  name: string;
  /** wall_ms of this ancestor's fork-from-parent event; null for root. */
  forked_at_wall_ms: number | null;
}

export interface CommunityLineageDto {
  /** Hex SpaceId of immediate parent, or null for top-level. */
  forked_from: string | null;
  /** wall_ms of this community's fork-from-parent event. */
  forked_at_wall_ms: number | null;
  /** Ancestors above immediate parent (root → above immediate parent). */
  parent_lineage: ParentLineageDto[];
  /** This community's own SpaceId (hex). */
  self_space_id: string;
  /** This community's own display name. */
  self_name: string;
}

export interface ForkDescendantDto {
  /** Hex SpaceId of the descendant fork community. */
  fork_space_id: string;
  /** Hex OwnerAddr of the forker. */
  forker_addr: string;
  /** Resolved display name of forker, or null. */
  forker_display_name: string | null;
  /** wall_ms of the Fork event. */
  forked_at_wall_ms: number;
  /** Whether the descendant community is in local NavService/OwnerState. */
  locally_known: boolean;
}
```

NOTE on serde Option → TS: serde serializes `None` as JSON `null`, which is what these types reflect. The TS types use `| null` not `| undefined`.

- [ ] **Step 2: Add service wrappers**

In `src/lib/community-service.ts`, append:

```typescript
import { adapter } from './adapter';  // or whichever module exposes the IPC adapter

export async function listCommunityForks(
  communityId: string,
): Promise<ForkDescendantDto[]> {
  try {
    return await adapter.invoke<ForkDescendantDto[]>('list_community_forks', {
      communityId,
    });
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    throw new Error(`listCommunityForks: ${msg}`);
  }
}

export async function getCommunityLineage(
  communityId: string,
): Promise<CommunityLineageDto> {
  try {
    return await adapter.invoke<CommunityLineageDto>('get_community_lineage', {
      communityId,
    });
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    throw new Error(`getCommunityLineage: ${msg}`);
  }
}
```

(Adapt imports + patterns to match the existing `CommunityService` shape — Phase 1 already wired similar wrappers like `forkCommunity`. Find them and mirror the style.)

- [ ] **Step 3: Run typecheck**

Run: `npx tsc --noEmit`
Expected: clean.

- [ ] **Step 4: Run all 5 gates** (frontend changes only — Rust gates pass trivially)

Expected: all green. Test counts unchanged (no new tests).

- [ ] **Step 5: Commit**

```bash
git add src/lib/types.ts src/lib/community-service.ts
git commit -m "$(cat <<'EOF'
feat(zeb-287): frontend types + service wrappers for lineage IPCs

ZEB-287 Phase 2. TS interfaces ParentLineageDto, CommunityLineageDto,
ForkDescendantDto matching backend DTOs. Service wrappers
listCommunityForks() + getCommunityLineage() with error normalization
via `e instanceof Error ? e.message : String(e)` per project convention.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 11: `ForkLineageTree.svelte` component + 8 vitest variants

**Files:**
- Create: `src/lib/components/ForkLineageTree.svelte`
- Create: `src/lib/components/__tests__/ForkLineageTree.test.ts`

- [ ] **Step 1: Write the failing test scaffold**

Create `src/lib/components/__tests__/ForkLineageTree.test.ts`:

```typescript
import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import ForkLineageTree from '../ForkLineageTree.svelte';
import type { CommunityLineageDto, ForkDescendantDto } from '$lib/types';

function emptyLineage(self_name = 'My Community'): CommunityLineageDto {
  return {
    forked_from: null,
    forked_at_wall_ms: null,
    parent_lineage: [],
    self_space_id: '00'.repeat(16),
    self_name,
  };
}

describe('ForkLineageTree', () => {
  it('renders_non_fork_no_descendants_minimally', () => {
    const { getByText, queryByText } = render(ForkLineageTree, {
      props: {
        lineage: emptyLineage(),
        descendants: [],
        localNavIds: new Set<string>(),
      },
    });

    expect(getByText(/You are here/)).toBeTruthy();
    expect(getByText(/My Community/)).toBeTruthy();
    expect(getByText(/no forks yet/i)).toBeTruthy();
  });

  it('renders_ancestors_only_for_leaf_fork', () => {
    const lineage: CommunityLineageDto = {
      ...emptyLineage('Leaf'),
      forked_from: 'aa'.repeat(16),
      forked_at_wall_ms: 1_700_000_000_000,
      parent_lineage: [
        { space_id: '11'.repeat(16), name: 'Root C', forked_at_wall_ms: null },
        { space_id: '22'.repeat(16), name: 'Middle B', forked_at_wall_ms: 1_650_000_000_000 },
      ],
    };

    const { getByText } = render(ForkLineageTree, {
      props: { lineage, descendants: [], localNavIds: new Set() },
    });

    expect(getByText(/Root C/)).toBeTruthy();
    expect(getByText(/Middle B/)).toBeTruthy();
    expect(getByText(/Leaf/)).toBeTruthy();
  });

  it('renders_descendants_only_for_root_with_forks', () => {
    const descendants: ForkDescendantDto[] = [
      {
        fork_space_id: '33'.repeat(16),
        forker_addr: 'ab'.repeat(32),
        forker_display_name: 'Maya',
        forked_at_wall_ms: 1_715_000_000_000,
        locally_known: true,
      },
      {
        fork_space_id: '44'.repeat(16),
        forker_addr: 'cd'.repeat(32),
        forker_display_name: null,
        forked_at_wall_ms: 1_716_000_000_000,
        locally_known: false,
      },
    ];

    const { getByText, getAllByRole } = render(ForkLineageTree, {
      props: { lineage: emptyLineage(), descendants, localNavIds: new Set() },
    });

    expect(getByText(/Maya/)).toBeTruthy();
    expect(getByText(/an unknown member/i)).toBeTruthy();
  });

  it('renders_full_tree_three_deep_two_descendants', () => {
    const lineage: CommunityLineageDto = {
      forked_from: '22'.repeat(16),
      forked_at_wall_ms: 1_700_000_000_000,
      parent_lineage: [
        { space_id: '11'.repeat(16), name: 'C', forked_at_wall_ms: null },
        { space_id: '22'.repeat(16), name: 'B', forked_at_wall_ms: 1_650_000_000_000 },
      ],
      self_space_id: '33'.repeat(16),
      self_name: 'A',
    };
    const descendants: ForkDescendantDto[] = [
      {
        fork_space_id: '44'.repeat(16),
        forker_addr: 'ab'.repeat(32),
        forker_display_name: 'Maya',
        forked_at_wall_ms: 1_715_000_000_000,
        locally_known: true,
      },
      {
        fork_space_id: '55'.repeat(16),
        forker_addr: 'cd'.repeat(32),
        forker_display_name: 'Sam',
        forked_at_wall_ms: 1_716_000_000_000,
        locally_known: true,
      },
    ];

    const { getByText, getAllByRole } = render(ForkLineageTree, {
      props: {
        lineage,
        descendants,
        localNavIds: new Set(['11'.repeat(16), '22'.repeat(16), '44'.repeat(16), '55'.repeat(16)]),
      },
    });

    const treeitems = getAllByRole('treeitem');
    // 2 ancestors + 1 self + 2 descendants = 5
    expect(treeitems.length).toBe(5);
  });

  it('renders_truncation_marker_for_overlong_lineage', () => {
    const overlong_lineage = Array.from({ length: 18 }, (_, i) => ({
      space_id: i.toString(16).padStart(2, '0').repeat(16),
      name: `ancestor_${i}`,
      forked_at_wall_ms: i === 0 ? null : i,
    }));

    const lineage: CommunityLineageDto = {
      ...emptyLineage('Deep Leaf'),
      forked_from: 'ff'.repeat(16),
      forked_at_wall_ms: 999_999,
      parent_lineage: overlong_lineage,
    };

    const { getByText } = render(ForkLineageTree, {
      props: { lineage, descendants: [], localNavIds: new Set() },
    });

    expect(getByText(/and 2 earlier ancestors/i)).toBeTruthy();
  });

  it('click_navigates_to_locally_known_community', async () => {
    const navigateSpy = vi.fn();
    const lineage: CommunityLineageDto = {
      ...emptyLineage('A'),
      forked_from: '22'.repeat(16),
      forked_at_wall_ms: 1_700_000_000_000,
      parent_lineage: [
        { space_id: '11'.repeat(16), name: 'Cool C', forked_at_wall_ms: null },
      ],
    };

    const { getByText, component } = render(ForkLineageTree, {
      props: {
        lineage,
        descendants: [],
        localNavIds: new Set(['11'.repeat(16)]),
      },
    });

    component.$on('navigate-to-community', (e: CustomEvent<string>) => {
      navigateSpy(e.detail);
    });

    await fireEvent.click(getByText(/Cool C/));
    expect(navigateSpy).toHaveBeenCalledWith('11'.repeat(16));
  });

  it('non_clickable_for_unknown_community', async () => {
    const lineage: CommunityLineageDto = {
      ...emptyLineage('A'),
      forked_from: '22'.repeat(16),
      forked_at_wall_ms: 1_700_000_000_000,
      parent_lineage: [
        { space_id: '11'.repeat(16), name: 'Cool C', forked_at_wall_ms: null },
      ],
    };

    const { container } = render(ForkLineageTree, {
      props: {
        lineage,
        descendants: [],
        localNavIds: new Set(),  // empty — Cool C is NOT locally known
      },
    });

    // The ancestor row should NOT be a <button> when not locally known
    const buttons = container.querySelectorAll('button');
    // Self row is non-clickable; only locally-known rows are buttons.
    // Empty localNavIds means NO ancestor/descendant rows are buttons.
    expect(buttons.length).toBe(0);
  });

  it('aria_current_page_on_self_row', () => {
    const { container } = render(ForkLineageTree, {
      props: {
        lineage: emptyLineage('My Self'),
        descendants: [],
        localNavIds: new Set(),
      },
    });

    const selfRow = container.querySelector('[aria-current="page"]');
    expect(selfRow).toBeTruthy();
    expect(selfRow?.textContent).toMatch(/My Self/);
  });
});
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `npx vitest run src/lib/components/__tests__/ForkLineageTree.test.ts`
Expected: all 8 fail with "Failed to resolve import '../ForkLineageTree.svelte'" (component doesn't exist yet).

- [ ] **Step 3: Implement the component**

Create `src/lib/components/ForkLineageTree.svelte`:

```svelte
<script lang="ts">
  import type { CommunityLineageDto, ForkDescendantDto, ParentLineageDto } from '$lib/types';
  import { createEventDispatcher } from 'svelte';

  export let lineage: CommunityLineageDto;
  export let descendants: ForkDescendantDto[] = [];
  /** Set of locally-known SpaceIds (hex). Used to gate clickability. */
  export let localNavIds: Set<string> = new Set();

  const dispatch = createEventDispatcher<{ 'navigate-to-community': string }>();

  const MAX_DISPLAYED_LINEAGE = 16;

  $: ancestorRows = (() => {
    const rows = lineage.parent_lineage;
    if (rows.length <= MAX_DISPLAYED_LINEAGE) return { rows, truncated: 0 };
    const truncated = rows.length - MAX_DISPLAYED_LINEAGE;
    return { rows: rows.slice(truncated), truncated };
  })();

  $: hasAnyForks = lineage.parent_lineage.length > 0 || descendants.length > 0;

  function formatDate(wallMs: number | null | undefined): string {
    if (wallMs == null) return '';
    return new Date(wallMs).toISOString().slice(0, 10);
  }

  function truncSpaceId(hex: string): string {
    return '0x' + hex.slice(0, 8) + '…';
  }

  function handleClick(spaceId: string): void {
    dispatch('navigate-to-community', spaceId);
  }
</script>

<ul role="tree" class="fork-lineage-tree">
  {#if ancestorRows.truncated > 0}
    <li role="treeitem" aria-level="1" class="lineage-row lineage-truncation">
      …and {ancestorRows.truncated} earlier ancestors
    </li>
  {/if}

  {#each ancestorRows.rows as entry, i (entry.space_id)}
    {@const depth = i + 1 + (ancestorRows.truncated > 0 ? 1 : 0)}
    {@const known = localNavIds.has(entry.space_id)}
    <li
      role="treeitem"
      aria-level={depth}
      class="lineage-row lineage-ancestor"
      style="padding-left: calc({depth} * 1.5rem);"
    >
      {#if known}
        <button class="lineage-clickable" on:click={() => handleClick(entry.space_id)}>
          ↳ {entry.name} {formatDate(entry.forked_at_wall_ms)}
        </button>
      {:else}
        <span class="lineage-unknown" title="You're not a member of this community.">
          ↳ {entry.name} {formatDate(entry.forked_at_wall_ms)}
        </span>
      {/if}
    </li>
  {/each}

  {@const self_depth = ancestorRows.rows.length + 1 + (ancestorRows.truncated > 0 ? 1 : 0)}
  <li
    role="treeitem"
    aria-level={self_depth}
    aria-current="page"
    class="lineage-row lineage-self"
    style="padding-left: calc({self_depth} * 1.5rem);"
  >
    You are here ← {lineage.self_name}
  </li>

  {#each descendants as desc (desc.fork_space_id)}
    {@const known = desc.locally_known && localNavIds.has(desc.fork_space_id)}
    {@const display = known
      ? /* will be resolved by NavService — for now use forker name as proxy */ desc.fork_space_id
      : truncSpaceId(desc.fork_space_id)}
    {@const forker = desc.forker_display_name ?? 'an unknown member'}
    <li
      role="treeitem"
      aria-level={self_depth + 1}
      class="lineage-row lineage-descendant"
      style="padding-left: calc({self_depth + 1} * 1.5rem);"
    >
      {#if known}
        <button class="lineage-clickable" on:click={() => handleClick(desc.fork_space_id)}>
          ↳ {display} {formatDate(desc.forked_at_wall_ms)} by {forker}
        </button>
      {:else}
        <span class="lineage-unknown" title="You're not a member of this fork.">
          ↳ {display} {formatDate(desc.forked_at_wall_ms)} by {forker}
        </span>
      {/if}
    </li>
  {/each}

  {#if !hasAnyForks}
    <li class="lineage-empty-hint" aria-hidden="true">(no forks yet)</li>
  {/if}
</ul>

<style>
  .fork-lineage-tree {
    list-style: none;
    padding: 0;
    margin: 0;
    font-size: 0.9rem;
  }
  .lineage-row {
    padding: 0.25rem 0;
  }
  .lineage-self {
    background: var(--surface-highlight, rgba(255, 200, 0, 0.1));
    font-weight: 600;
  }
  .lineage-ancestor,
  .lineage-descendant {
    color: var(--text-muted, #888);
  }
  .lineage-clickable {
    background: none;
    border: none;
    color: var(--text-link, #5c8fff);
    cursor: pointer;
    padding: 0;
    font: inherit;
    text-align: left;
  }
  .lineage-clickable:hover {
    text-decoration: underline;
  }
  .lineage-unknown {
    cursor: default;
  }
  .lineage-empty-hint {
    color: var(--text-muted, #999);
    font-style: italic;
    padding-left: 1.5rem;
  }
  .lineage-truncation {
    color: var(--text-muted, #888);
    font-style: italic;
    padding-left: 0;
  }
</style>
```

NOTE: the component above uses Svelte 5 syntax (`{@const}` blocks inside `{#each}`). If the codebase is Svelte 4, swap `{@const}` for `let` reactive declarations or pre-compute in the script. Phase 1 components are Svelte 5 (per CommunitySettingsPanel + ForkConfirmDialog).

NOTE 2: the resolution of the descendant's display name is a placeholder. In production it should look up the fork's name from NavService when `locally_known: true`. The test asserts that `known` rows use full name; the placeholder uses `desc.fork_space_id` (hex). The implementer should wire actual NavService.resolveCommunityName() if such a helper exists, or accept a `resolveLocalName: (hex) => string | undefined` prop. The 4th test `renders_full_tree_three_deep_two_descendants` doesn't assert the descendant name shape strictly; the production wiring is in Task 12 where CommunitySettingsPanel mounts the component with the right helper.

- [ ] **Step 4: Run the component tests**

Run: `npx vitest run src/lib/components/__tests__/ForkLineageTree.test.ts`
Expected: 8 tests pass.

- [ ] **Step 5: Run all 5 gates**

Expected: all green. Frontend test count: 1740 → 1748 (+8).

- [ ] **Step 6: Commit**

```bash
git add src/lib/components/ForkLineageTree.svelte src/lib/components/__tests__/ForkLineageTree.test.ts
git commit -m "$(cat <<'EOF'
feat(zeb-287): ForkLineageTree.svelte component + 8 vitest variants

ZEB-287 Phase 2 spec §5.2 + §7.4. New Svelte 5 component that renders
ancestors above, "you are here" highlighted in middle, descendants
below — all in a single <ul role="tree"> with aria-current="page" on
self and aria-level on each row.

Click navigation: clickable rows dispatch 'navigate-to-community'
event with the target SpaceId. Non-clickable rows (unknown
communities / forks the viewer isn't a member of) render as <span>
with tooltip.

Truncation marker: when ancestor chain > 16, the topmost N are
collapsed into "…and N earlier ancestors" non-clickable.

Empty state: "(no forks yet)" rendered for communities with neither
ancestors nor descendants.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 12: `CommunitySettingsPanel.svelte` refactor (Lineage → Forks)

**Files:**
- Modify: `src/lib/components/CommunitySettingsPanel.svelte` (rename section, always render, add explainer, mount ForkLineageTree)
- Modify / create: `src/lib/components/__tests__/CommunitySettingsPanel.test.ts` (augment existing or create)

- [ ] **Step 1: Find and read existing Lineage block + "Fork this community" button**

Run: `grep -n "Lineage\|Fork this community\|forked_from\|forkCommunity" src/lib/components/CommunitySettingsPanel.svelte | head -20`

Understand the existing Phase 1 structure — the Lineage block (likely `{#if forked_from}`) and the Fork button (likely a separate `<button>` calling `forkCommunity`).

- [ ] **Step 2: Refactor the Lineage block into a Forks section**

In `src/lib/components/CommunitySettingsPanel.svelte`:

1. Remove the existing `{#if forked_from}` guard around the Lineage section.
2. Rename the section header to "Forks".
3. Insert the polycentric-framing explainer paragraph as the first child.
4. Mount `<ForkLineageTree>` with the data fetched from the two new IPCs.
5. Move the existing "Fork this community" button to be the last child of the Forks section.

Concrete diff (approximate — adapt to actual file structure):

```svelte
<script lang="ts">
  // ... existing imports ...
  import ForkLineageTree from './ForkLineageTree.svelte';
  import { listCommunityForks, getCommunityLineage } from '$lib/community-service';
  import { onMount } from 'svelte';
  import { NavService } from '$lib/nav-service';
  // ... existing state ...
  
  let lineage: CommunityLineageDto | null = null;
  let descendants: ForkDescendantDto[] = [];
  let localNavIds: Set<string> = new Set();

  onMount(async () => {
    lineage = await getCommunityLineage(communityId);
    descendants = await listCommunityForks(communityId);
    localNavIds = new Set(NavService.localCommunityIds());  // adapt to actual API
  });

  function handleNavigate(e: CustomEvent<string>): void {
    NavService.navigateToCommunity(e.detail);
  }
</script>

<!-- ... existing panel structure ... -->

<section class="forks-section">
  <h3>Forks</h3>
  <p class="forks-explainer">
    Any member of a community can fork it at any time, creating a new community with
    the snapshot of history they had access to. The fork is independent — it has its
    own membership, channels, and admin. Forks are how communities preserve continuity
    if members want to take their conversation elsewhere.
  </p>

  {#if lineage}
    <ForkLineageTree
      {lineage}
      {descendants}
      {localNavIds}
      on:navigate-to-community={handleNavigate}
    />
  {/if}

  <button class="fork-this-community" on:click={openForkConfirmDialog}>
    Fork this community
  </button>
</section>

<!-- ... other settings sections ... -->
```

NOTE on prop names: adapt `NavService.localCommunityIds()` / `NavService.navigateToCommunity()` to whatever helpers exist. If equivalent helpers don't exist yet, add tiny wrappers in `src/lib/nav-service.ts` referencing the underlying store — keep the diff minimal.

- [ ] **Step 3: Write / augment tests**

Create or modify `src/lib/components/__tests__/CommunitySettingsPanel.test.ts`:

```typescript
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, waitFor } from '@testing-library/svelte';
import CommunitySettingsPanel from '../CommunitySettingsPanel.svelte';

// Mock the community-service IPC wrappers so tests don't need a real backend
vi.mock('$lib/community-service', () => ({
  listCommunityForks: vi.fn(async () => []),
  getCommunityLineage: vi.fn(async (communityId: string) => ({
    forked_from: null,
    forked_at_wall_ms: null,
    parent_lineage: [],
    self_space_id: communityId,
    self_name: 'Test Community',
  })),
  // ... other CommunityService methods may need mocking too
}));

describe('CommunitySettingsPanel — Forks section', () => {
  it('forks_section_always_renders_for_non_fork_community', async () => {
    const { findByText } = render(CommunitySettingsPanel, {
      props: { communityId: '00'.repeat(16) /* + other required props */ },
    });

    expect(await findByText('Forks')).toBeTruthy();
  });

  it('forks_section_renders_explainer_text_present', async () => {
    const { findByText } = render(CommunitySettingsPanel, {
      props: { communityId: '00'.repeat(16) },
    });

    // Substring match on the final explainer wording
    expect(
      await findByText(/Any member of a community can fork it at any time/),
    ).toBeTruthy();
    expect(
      await findByText(/communities preserve continuity if members want to take/),
    ).toBeTruthy();
  });

  it('fork_this_community_button_inside_forks_section', async () => {
    const { findByText, container } = render(CommunitySettingsPanel, {
      props: { communityId: '00'.repeat(16) },
    });

    const forksSection = container.querySelector('.forks-section');
    expect(forksSection).toBeTruthy();

    const forkButton = forksSection!.querySelector('button.fork-this-community');
    expect(forkButton).toBeTruthy();
    expect(forkButton?.textContent).toMatch(/Fork this community/);
  });
});
```

If a pre-existing `CommunitySettingsPanel.test.ts` already exists, append these tests (don't replace existing ones).

- [ ] **Step 4: Run the new tests**

Run: `npx vitest run src/lib/components/__tests__/CommunitySettingsPanel.test.ts`
Expected: 3 new tests pass.

- [ ] **Step 5: Run all 5 gates**

Expected: Frontend test count: 1748 → 1751 (+3).

- [ ] **Step 6: Commit**

```bash
git add src/lib/components/CommunitySettingsPanel.svelte src/lib/components/__tests__/CommunitySettingsPanel.test.ts
git commit -m "$(cat <<'EOF'
feat(zeb-287): CommunitySettingsPanel Forks section + explainer + tree mount

ZEB-287 Phase 2 spec §5.1. Renames Phase 1's "Lineage" section to
"Forks" and makes it always render (no {#if forked_from} guard).
Adds polycentric-framing explainer paragraph as the first child.
Mounts <ForkLineageTree> driven by get_community_lineage +
list_community_forks IPCs. Moves the "Fork this community" button
into this section for cohesion.

Non-fork communities with no descendants render the minimal shape
("you are here" + "(no forks yet)") — explicitly NOT an empty
state, just lightweight presence to reinforce that every community
is forkable as the default.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 13: Final 5-gate sweep + push + PR creation

**Files:** none modified (verification + push)

- [ ] **Step 1: Sync with origin/main one final time**

```bash
git fetch origin
git log --oneline HEAD..origin/main
```

If `origin/main` has moved past `5d9044a` (someone else merged something), pause and either rebase or check with the user. Per memory: never auto-rebase a feature branch without explicit user OK.

If `origin/main` is unchanged at `5d9044a`, proceed.

- [ ] **Step 2: Run all 5 CI gates from scratch (cold cache)**

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
cd .. && npx tsc --noEmit
npx vitest run
```

Expected:
- `cargo fmt`: clean
- `cargo clippy`: clean, no warnings
- `cargo nextest`: ~1395 passed (1369 baseline + ~26 new). The exact number may vary by ±3 depending on how Task 5 / Task 9's optional tests resolved.
- `npx tsc --noEmit`: clean
- `npx vitest`: ~1751 passed (1740 baseline + 11 new)

If any gate fails, fix BEFORE pushing. Do NOT push a red branch.

Per memory `feedback_pipe_exit_codes_lie`: never run gates as `cmd | tail/grep`. Use `set -o pipefail` if piping is unavoidable.

- [ ] **Step 3: Push the branch**

```bash
git push -u origin zeb-285-phase2-fork-lineage-ux
```

- [ ] **Step 4: Open the PR**

Use `gh pr create` with a thorough body referencing ZEB-287, the spec, and the smoke-test checklist. Body shape:

```bash
gh pr create --title "ZEB-287: community forking UX Phase 2 (disclosure + descendants + chain)" --body "$(cat <<'EOF'
## Summary

Phase 2 of [ZEB-285](https://linear.app/zeblith/issue/ZEB-285) community forking — the user-visible follow-ups for the Phase 1 primitive (PR [#122](https://github.com/zeblithic/harmony-client/pull/122)).

Closes [ZEB-287](https://linear.app/zeblith/issue/ZEB-287).

## What this ships

- Polycentric-framing **disclosure** for forks (matter-of-fact explainer, not warning-shaped) in a renamed-and-restructured "Forks" section of `CommunitySettingsPanel.svelte` that always renders for every community.
- Chronological **descendants list** surfacing visible Fork events from the membership log; silent forks remain invisible by design.
- Multi-hop **ancestor chain visualization** via a baked-in `parent_lineage` field on `PreForkSnapshot` and `CommunityState` — the forker freezes their full chain at fork-time so the snapshot carries it; sidesteps the cross-membership encrypted-state problem.
- Backwards-compatible wire format: Phase 1 fixtures decode byte-identically under Phase 2 types; Phase 1 forks degrade gracefully to single-hop display.

## Architecture

- New types: `ParentLineageEntry` (in `community_invite.rs`); DTOs `ForkDescendantDto`, `CommunityLineageDto`, `ParentLineageDto` (in `lib.rs`).
- Extended types: `PreForkSnapshot.parent_lineage`, `CommunityState.parent_lineage`, `CommunityState.forked_at_wall_ms` (all skip-if-empty / skip-if-none, byte-compat).
- New IPCs: `list_community_forks(communityId)` walks the membership log for `MembershipEventKind::Fork` events; `get_community_lineage(communityId)` exposes lineage fields behind a tight DTO.
- New component: `ForkLineageTree.svelte` renders ancestors → "you are here" → descendants in a single `<ul role="tree">` with `aria-current="page"` on self.
- 16-deep cap on the baked chain at fork-build time.

## Out of scope (explicit deferrals)

Filed as Phase 3+ follow-ups if/when needed:

- Original-community channel-timeline rendering of Fork events as system messages
- Disclosure surfaced outside CommunitySettingsPanel (no nav badges, first-join modals)
- Library-directory fork-inheritance affordance ([ZEB-218](https://linear.app/zeblith/issue/ZEB-218) Sub-D)
- Verify-on-redeem hardening of snapshot signatures
- "Recently forked" cross-cutting surface
- Pre-fork message author display via profile-broadcast ([ZEB-281](https://linear.app/zeblith/issue/ZEB-281))
- Snapshots >5000 via content-addressed delivery
- Retry surface for failed announce/leave
- `forked_from` persistence race fix
- i18n / multi-language explainer text

## Documents

- **Spec:** `docs/specs/2026-05-15-zeb-285-phase2-community-forking-ux-design.md` (commit `376a6fb`)
- **Plan:** `docs/plans/2026-05-15-zeb-287-phase2-community-forking-ux-plan.md`

## Test plan

- [ ] `cargo fmt --all -- --check` — clean
- [ ] `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` — clean
- [ ] `cargo nextest run --locked --workspace --all-targets --features test-fixtures` — all pass (~1395 tests)
- [ ] `npx tsc --noEmit` — clean
- [ ] `npx vitest run` — all pass (~1751 tests)
- [ ] **Manual smoke test** (two-engine local, per spec §7.6):
  - [ ] Engine A creates community C, invites Engine B
  - [ ] Engine A forks C into B; forks B into A_fork
  - [ ] Engine A opens A_fork settings → tree shows `C ← B ← You are here`
  - [ ] Engine A opens B settings → tree shows `C ← You are here ← A_fork (forked by A)`
  - [ ] Engine A opens C settings → tree shows `You are here ← B (forked by A)`
  - [ ] Engine B opens C settings → tree shows `You are here ← B (forked by A)` (same as Engine A — descendants list is community-bound, not viewer-bound)
- [ ] Visible vs silent fork: confirmed silent forks remain absent from descendants list

## Cross-refs

- Parent: [ZEB-285](https://linear.app/zeblith/issue/ZEB-285) (Phase 1)
- Phase 1 PR: [#122](https://github.com/zeblithic/harmony-client/pull/122)
- Phase 1 spec: [`docs/specs/2026-05-14-zeb-285-phase1-community-forking-design.md`](docs/specs/2026-05-14-zeb-285-phase1-community-forking-design.md)

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 5: Capture PR URL**

The `gh pr create` output includes the PR URL. Capture it for the autonomous monitoring loop.

- [ ] **Step 6: Final verification**

```bash
gh pr view <PR_NUMBER> --json state,mergeable,statusCheckRollup
```

Expected: state=OPEN, mergeable=MERGEABLE (or UNKNOWN until checks complete).

Per memory `feedback_ci_disabled`: GitHub Actions CI is disabled in this repo; bot checks (CodeRabbit, Cursor, etc.) are what's being waited on.

**No commit at the end of Task 13** — Task 13 is the final-gates + push + PR step. The branch's last commit is from Task 12.

---

## Plan completion checklist

- [ ] Task 0: pre-flight green baseline
- [ ] Task 1: ParentLineageEntry struct
- [ ] Task 2: PreForkSnapshot.parent_lineage field
- [ ] Task 3: CommunityState lineage fields
- [ ] Task 4: build_fork_snapshot lineage construction + cap
- [ ] Task 5: redeem_invite_inner wiring
- [ ] Task 6: list_community_forks IPC
- [ ] Task 7: get_community_lineage IPC
- [ ] Task 8: Wire-format pinning fixtures
- [ ] Task 9: Multi-hop integration tests
- [ ] Task 10: Frontend types + service wrappers
- [ ] Task 11: ForkLineageTree.svelte + 8 vitest variants
- [ ] Task 12: CommunitySettingsPanel.svelte refactor + tests
- [ ] Task 13: Final 5-gate sweep + push + PR creation

Total: 14 tasks, 12 implementation commits across Tasks 1-12. Task 13 performs the final 5-gate sweep + push + PR creation without an additional commit.

Expected final test counts:
- Rust: 1369 → ~1395 (+26)
- Frontend: 1740 → ~1751 (+11)

Once Task 13 lands the PR, transition to the **autonomous bot-review monitoring loop** (CodeRabbit / Cursor / CodeAnt / Qodo per `feedback_autonomous_pr_monitoring_loop` memory). Pushover when PR is mergeable per `feedback_no_pushover_when_active` memory.
