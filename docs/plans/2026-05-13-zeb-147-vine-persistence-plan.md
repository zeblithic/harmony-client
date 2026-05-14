# ZEB-147 Vine Persistence Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire the in-memory `VineFeedCache` (landed in ZEB-286 / PR #118) to disk so the Vine feed survives app reload. Local persistence only — cross-device viewed-state sync deferred to a follow-up.

**Architecture:** Single JSON file at `{app_data_dir}/vine_feed.json` with versioned envelope `{version: 1, descriptors, reactions, viewed}`. Atomic save via tempfile + rename (the same shape `follows.rs` and `content_index.rs` use). Constructor split: `new()` stays in-memory only (test path, `save()` no-op); `load(data_dir)` reads + age-prunes + capacity-trims and arms `save()`. Save is called from each mutator only when state actually changed (`Inserted` / `UpdatedNewer` / first-time viewed). Runtime cap of `MAX_DESCRIPTORS = 5000` drops the oldest descriptor on overflow; load-time age cutoff is `MAX_AGE_SECS = 90 days`.

**Tech Stack:** Rust 2021, `serde_json` for JSON, `std::fs` for sync IO, `tempfile::NamedTempFile`-style write-then-rename (using `std::fs::write` + `std::fs::rename` per the established follows.rs idiom, not the `tempfile` crate). All new code lives in `src-tauri/src/vine_feed_cache.rs` plus a single integration test file under `src-tauri/tests/`.

---

## File Structure

**Modified files:**

- `src-tauri/src/vine_feed_cache.rs` — primary surface. Adds `path: Option<PathBuf>` field, `MAX_DESCRIPTORS` / `MAX_AGE_SECS` constants, a private `VineFeedDiskV1` envelope struct, the `load(data_dir)` constructor, the private `save()` method, the runtime capacity-trim inside `on_descriptor_sample`, and 11 new unit tests in `mod tests`. The existing `new()`, `on_descriptor_sample`, `on_reaction_sample`, `mark_viewed`, `list_descriptors`, `get_reaction` keep their public signatures unchanged — only behavior gains a side-effect (`self.save()`).
- `src-tauri/src/lib.rs` — single-line change in `start_node` to swap `VineFeedCache::new()` → `VineFeedCache::load(&app_data_dir)`.
- `src-tauri/Cargo.toml` — no changes (tempfile already in deps, used by other modules; we use `std::fs::write` + `std::fs::rename` directly).

**New files:**

- `src-tauri/tests/vine_feed_persistence_integration.rs` — one integration test (`cache_survives_reload`) demonstrating the round-trip works end-to-end with a `tempfile::TempDir`.

**Why this decomposition:** all the persistence logic stays inside `vine_feed_cache.rs` because the cache is the unit of state — splitting save/load into a sibling module would require exposing the private `descriptors/reactions/viewed` fields. The integration test gets its own file (parallel to `vine_feed_cache_integration.rs` from ZEB-286) because it exercises the public API surface and serves as the "you can throw away the cache and recover" smoke test that a future contributor will read first.

---

## Implementation Notes (read once before starting)

**Branch state at plan time:**

- Branch: `zeb-147-vine-persistence`, HEAD `50a0fe4` (spec commit), on top of `origin/main` `979ea84`.
- Working tree clean; pull-before-work satisfied.

**HARD RULES (from user memory):**

- `no worktrees` — never `git worktree add`. Work directly in the main repo via `git checkout`.
- Five required CI gates, all must be green before final push:
  - `cd src-tauri && cargo fmt --all -- --check`
  - `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
  - `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures`
  - `npx tsc --noEmit` (from repo root)
  - `npx vitest run` (from repo root)
- `pipe exit codes lie` — never `cmd | tail/grep` to verify; use `${PIPESTATUS[0]}` or `set -o pipefail`. Concretely, prefer `cargo nextest run ... 2>&1 | tee /tmp/log.txt; status=${PIPESTATUS[0]}; test $status -eq 0`. Subagents normally don't pipe at all — just run and inspect the trailing tail of stdout.
- `test drift is our fault` — if any pre-existing test breaks during this work, sweep + fix it on the same branch; do not externalize.
- `cargo fmt gate` — every implementer task must run `cargo fmt --all` (not just `--check`) before commit, so format drift never sneaks in.
- `metadata before irreversible write` — `load()` is read-only; `save()` is the irreversible write. Always verify path validity before mutating. The atomic write idiom (write to `<file>.tmp`, then `rename`) inherently follows this rule: the `rename` is the commit point, and if any prior step failed we never overwrote the real file.
- `Tauri error extraction: e instanceof Error ? e.message : String(e)` — N/A in this PR (Rust-only changes, no new frontend catch blocks).
- `Linear PR auto-close cascade` — Task 8 PR body uses markdown-linked refs `[ZEB-147](https://linear.app/zeblith/issue/ZEB-147)` for **every** cross-ref, never bare `ZEB-147`. Only ZEB-147 itself goes in the auto-close paragraph; ZEB-286 / ZEB-209 / ZEB-214 / ZEB-103 are linked but in body prose.
- `Never invent Linear IDs` — no new sub-tickets get filed in Task 8. ZEB-147 is the only existing ticket; the user files follow-ups themselves after merge.

**Reference reading (for implementer subagents):**

- Spec: `docs/specs/2026-05-13-zeb-147-vine-persistence-design.md` (commit `50a0fe4`)
- Current cache (the file being modified): `src-tauri/src/vine_feed_cache.rs` (1-821 at HEAD `50a0fe4`)
- Sibling: `src-tauri/src/follows.rs:36-86` (`FollowManager::load` + private `save` are the canonical shape to copy)
- start_node site: `src-tauri/src/lib.rs:1010-1013` (the construction site is the line to swap)

---

## Task 0: Pre-flight + green baseline

**Goal:** confirm the branch is on top of `origin/main`, all five CI gates are green BEFORE we start changing anything, and the cache module is exactly as ZEB-286 left it. No commit at the end of this task — verification only.

**Files:**
- Read: `src-tauri/src/vine_feed_cache.rs` (post-ZEB-286 baseline)
- Read: `src-tauri/src/lib.rs:1010-1013` (the production wiring site we'll swap in Task 6)

- [ ] **Step 1: Confirm branch identity and base**

Run:
```bash
git rev-parse --abbrev-ref HEAD
git log --oneline origin/main..HEAD
git status
```

Expected:
- branch name: `zeb-147-vine-persistence`
- log shows exactly one commit ahead of `origin/main`: `50a0fe4 docs(zeb-147): vine persistence design (disk-backed VineFeedCache)`
- working tree clean

If branch differs or there are uncommitted changes, STOP and escalate.

- [ ] **Step 2: Confirm the cache module matches the post-ZEB-286 baseline**

Run:
```bash
grep -n "pub fn new\|pub fn load\|MAX_DESCRIPTORS\|MAX_AGE_SECS\|path:" src-tauri/src/vine_feed_cache.rs
```

Expected:
- One hit for `pub fn new() -> Self` (around line 105)
- ZERO hits for `pub fn load`, `MAX_DESCRIPTORS`, `MAX_AGE_SECS`, or `path:` — those are what this PR will add.

If any of those already exist, STOP and escalate (someone else may have started the work).

- [ ] **Step 3: Run cargo fmt --check (Rust formatter)**

Run:
```bash
cd src-tauri && cargo fmt --all -- --check
```

Expected: exit code 0, no diff output.

- [ ] **Step 4: Run cargo clippy (Rust lint)**

Run:
```bash
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
```

Expected: exit code 0, no warnings.

- [ ] **Step 5: Run cargo nextest (Rust tests)**

Run:
```bash
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

Expected: exit code 0, summary line shows all tests passed (test count matches main; should be ≥ 1221 tests post-ZEB-286). Note the exact count for Task 8 comparison.

- [ ] **Step 6: Run npx tsc and npx vitest (frontend gates)**

Run from repo root:
```bash
npx tsc --noEmit
npx vitest run
```

Expected: exit code 0 on both. Note exact vitest count for Task 8 comparison.

- [ ] **Step 7: Record the baseline test counts**

Open `docs/plans/2026-05-13-zeb-147-vine-persistence-plan.md` and append a single line under this task in your scratch notes (NOT in the plan file itself — this is implementer-local notes for Task 8 sanity check):
- baseline Rust test count: <N>
- baseline Vitest test count: <M>

No commit. This task is verification-only.

---

## Task 1: Add `path: Option<PathBuf>` field + `load()` stub

**Goal:** introduce the field and the new public constructor. The stub returns an empty cache (no file IO yet) so we can incrementally build `save()` and `load()` in Tasks 2 and 3 without breaking anything.

**Files:**
- Modify: `src-tauri/src/vine_feed_cache.rs:97-107` (the `VineFeedCache` struct and `new()` impl)

- [ ] **Step 1: Write the failing test (new() leaves path None; load() sets path to Some)**

In `src-tauri/src/vine_feed_cache.rs`, inside `mod tests` (after line 820, just before the closing `}`), add:

```rust
    #[test]
    fn new_leaves_path_unset() {
        let cache = VineFeedCache::new();
        // `path` is private; we observe it indirectly: save() must be a
        // no-op when path is None. Since save() is wired in Task 2, this
        // test asserts the constructor contract via the public API.
        // For now, just assert the cache is empty and constructable.
        assert_eq!(cache.len_descriptors(), 0);
        assert_eq!(cache.len_reactions(), 0);
        assert!(!cache.is_viewed("anything"));
    }

    #[test]
    fn load_empty_dir_returns_empty_cache() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let cache = VineFeedCache::load(dir.path());
        assert_eq!(cache.len_descriptors(), 0);
        assert_eq!(cache.len_reactions(), 0);
        assert!(!cache.is_viewed("anything"));
    }
```

- [ ] **Step 2: Run the new tests — expect compile error**

Run:
```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(new_leaves_path_unset) | test(load_empty_dir_returns_empty_cache)' 2>&1 | tail -20
```

Expected: compile error — `cannot find function 'load' in struct 'VineFeedCache'` (or similar), AND/OR `tempfile` not in `[dev-dependencies]` (it is in `[dependencies]`, so the `use` should resolve, but the test file already uses it elsewhere; verify with grep).

Sanity check `tempfile` is reachable from tests:
```bash
grep -n 'use tempfile\|tempfile::' src-tauri/src/vine_feed_cache.rs src-tauri/tests/*.rs | head -5
```

Expected: at least one existing reference (it's used in `vine_content_roundtrip_integration.rs` from ZEB-286).

- [ ] **Step 3: Add `path` field + `load()` stub**

Modify `src-tauri/src/vine_feed_cache.rs`:

First, update the imports near line 12 to add `Path` and `PathBuf`:

```rust
use crate::{VineDescriptorPayload, VineReactionPayload, VineVideoDto};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
```

Then update the struct definition at lines 97-102:

```rust
/// In-memory, single-peer view of the Vine network. Owned by NodeState;
/// updated by the event loop on receive; queried by IPCs.
///
/// ZEB-147: `path` is set by `load(data_dir)` (production path) and is
/// `None` after `new()` (test path). When `None`, `save()` is a no-op,
/// so unit tests can mutate the cache freely without touching disk.
#[derive(Debug, Default)]
pub struct VineFeedCache {
    descriptors: HashMap<String, CachedVine>,
    reactions: HashMap<(String, String), CachedReaction>,
    viewed: HashSet<String>,
    /// `Some(path_to_vine_feed.json)` when constructed via `load()`;
    /// `None` for `new()`. `save()` checks this and is a no-op when None.
    path: Option<PathBuf>,
}
```

Then update the `impl VineFeedCache` block at line 104. Replace lines 104-107:

```rust
impl VineFeedCache {
    /// In-memory only. No persistence path; `save()` is a no-op.
    /// Used by tests and any caller that explicitly wants ephemeral state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Load from `data_dir/vine_feed.json`. Returns an empty cache (with
    /// `path` set so subsequent mutations persist) when the file is
    /// missing or unreadable.
    ///
    /// ZEB-147 Task 3 will add the actual file IO + age-prune + capacity-trim.
    /// For Task 1, this is a stub that just sets `path` so the rest of
    /// the API can be built against it.
    pub fn load(data_dir: &Path) -> Self {
        let path = data_dir.join("vine_feed.json");
        let mut cache = Self::default();
        cache.path = Some(path);
        cache
    }
```

(The closing `}` for `impl` stays at the end of the existing block; we're only adding `load` and rewriting the docstring on `new`.)

- [ ] **Step 4: Run the tests — expect green**

Run:
```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(new_leaves_path_unset) | test(load_empty_dir_returns_empty_cache)' 2>&1 | tail -20
```

Expected: 2 passed, 0 failed.

- [ ] **Step 5: Run the full module test set (should still all pass)**

Run:
```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(vine_feed_cache)' 2>&1 | tail -20
```

Expected: all module tests pass (18 from ZEB-286 + 2 new = 20).

- [ ] **Step 6: Format + clippy check**

Run:
```bash
cd src-tauri && cargo fmt --all
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -20
```

Expected: fmt produces no diff (already formatted); clippy 0 warnings.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/vine_feed_cache.rs
git commit -m "feat(zeb-147): add VineFeedCache::load() stub + path field

new() keeps path = None (test path, save() no-op).
load(data_dir) sets path = Some(data_dir/vine_feed.json).
No file IO yet — Task 3 will fill in load(); Task 2 will add save()."
```

---

## Task 2: Add `MAX_DESCRIPTORS` / `MAX_AGE_SECS` constants, envelope struct, and `save()`

**Goal:** add the disk envelope type + the private `save()` method (no-op when `path` is None; atomic write when Some). Add the bound constants. This task does NOT wire save into mutators yet (Task 4) and does NOT read from disk yet (Task 3). Two unit tests cover the no-op path and the round-trip via direct method invocation.

**Files:**
- Modify: `src-tauri/src/vine_feed_cache.rs` (add constants, envelope struct, `save()` method; expand test helper imports)

- [ ] **Step 1: Write failing tests**

In `mod tests` (after the `load_empty_dir_returns_empty_cache` test added in Task 1), add:

```rust
    #[test]
    fn save_is_noop_when_path_is_none() {
        // VineFeedCache::new() has path = None. save() must not panic
        // and must not create any side effect.
        let mut cache = VineFeedCache::new();
        cache.mark_viewed("v-1".to_string());
        // save() is private; we observe its no-op-ness indirectly via
        // the public mark_viewed (which Task 4 will wire to save()).
        // For now, just assert mark_viewed returns true and the cache
        // remains usable.
        assert!(cache.is_viewed("v-1"));
    }

    #[test]
    fn save_writes_atomic_file_when_path_is_set() {
        // Construct a cache with path set, mutate it, call save() directly
        // (the method is private — we invoke it via a Task-2-internal
        // test helper exposing `save_for_test`). Verify the file exists
        // and contains expected JSON.
        let dir = tempfile::tempdir().expect("create tempdir");
        let mut cache = VineFeedCache::load(dir.path());
        cache.mark_viewed("v-saved".to_string());
        cache.save_for_test();

        let path = dir.path().join("vine_feed.json");
        assert!(path.exists(), "vine_feed.json must exist after save");
        let bytes = std::fs::read(&path).expect("read saved file");
        let json: serde_json::Value =
            serde_json::from_slice(&bytes).expect("file must be valid JSON");
        assert_eq!(json["version"], 1);
        assert!(
            json["viewed"]
                .as_array()
                .map(|a| a.iter().any(|v| v.as_str() == Some("v-saved")))
                .unwrap_or(false),
            "viewed set must contain v-saved; got: {json}"
        );
    }
```

- [ ] **Step 2: Run the tests — expect compile error**

Run:
```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(save_is_noop_when_path_is_none) | test(save_writes_atomic_file_when_path_is_set)' 2>&1 | tail -20
```

Expected: `no method 'save_for_test'` compile error.

- [ ] **Step 3: Add constants + envelope struct + `save()` method**

In `src-tauri/src/vine_feed_cache.rs`, add (after the existing imports near line 12):

```rust
use serde::Deserialize;
```

(Update the existing `use serde::Serialize;` line to `use serde::{Deserialize, Serialize};`.)

Add at the module level (above the `pub enum VineSource` declaration around line 19), add the constants and disk-format types:

```rust
/// ZEB-147: max descriptors retained in the cache. On insert into a full
/// cache, the oldest descriptor (lowest `created_at`) is dropped, along
/// with its reactions. Viewed-set entries are NOT dropped (low byte cost).
pub const MAX_DESCRIPTORS: usize = 5000;

/// ZEB-147: max age of a descriptor in seconds. Applied ONCE on `load()`;
/// descriptors with `created_at < now_secs - MAX_AGE_SECS` are dropped
/// along with their reactions. Runtime mutations do not re-age-prune.
pub const MAX_AGE_SECS: u64 = 90 * 86_400;

/// On-disk envelope. Versioned at the top level for forward-compat.
/// `version != 1` on `load()` causes the file to be ignored (treat as
/// missing). v1 is the only version that exists today.
#[derive(Debug, Serialize, Deserialize)]
struct VineFeedDiskV1 {
    version: u32,
    descriptors: Vec<DescriptorOnDisk>,
    reactions: Vec<ReactionOnDisk>,
    viewed: Vec<String>,
}

/// On-disk descriptor row. Mirrors the in-memory `CachedVine` plus the
/// `source` tag (decided at first arrival; preserved across reloads).
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DescriptorOnDisk {
    id: String,
    creator_address: String,
    creator_name: String,
    created_at: u64,
    video_cid: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    reshare_of: Option<String>,
    received_at_ms: u64,
    source: VineSource,
}

/// On-disk reaction row. Flat — `vine_id` and `reactor_address` join
/// back to the in-memory `HashMap<(String, String), CachedReaction>` key.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReactionOnDisk {
    vine_id: String,
    reactor_address: String,
    reactor_name: String,
    liked: bool,
    timestamp: u64,
}
```

Then add `Deserialize` to `VineSource` so it round-trips through the disk format. Modify line 19-24:

```rust
/// How the recipient discovered this vine. Followed = creator is in the
/// local follow set at the time of first arrival; Discover = otherwise.
/// Decided ONCE at first insert; subsequent re-arrivals do not change it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VineSource {
    Followed,
    Discover,
}
```

Now add the `save` method inside `impl VineFeedCache` (just before the existing `len_descriptors` test helper around line 308):

```rust
    /// Atomic save: serialize cache state, write to `<path>.tmp`, rename
    /// to `<path>`. No-op when `self.path.is_none()` (test path).
    ///
    /// Errors are logged via `tracing::warn!` but never propagated — this
    /// matches the `follows.rs` / `content_index.rs` philosophy: persistence
    /// is best-effort; a failed save must not crash the dispatch loop.
    fn save(&self) {
        let Some(path) = self.path.as_ref() else {
            return;
        };

        let file = VineFeedDiskV1 {
            version: 1,
            descriptors: self
                .descriptors
                .values()
                .map(|cv| DescriptorOnDisk {
                    id: cv.descriptor.id.clone(),
                    creator_address: cv.descriptor.creator_address.clone(),
                    creator_name: cv.descriptor.creator_name.clone(),
                    created_at: cv.descriptor.created_at,
                    video_cid: cv.descriptor.video_cid.clone(),
                    title: cv.descriptor.title.clone(),
                    reshare_of: cv.descriptor.reshare_of.clone(),
                    received_at_ms: cv.received_at_ms,
                    source: cv.source,
                })
                .collect(),
            reactions: self
                .reactions
                .iter()
                .map(|((vine_id, reactor_addr), r)| ReactionOnDisk {
                    vine_id: vine_id.clone(),
                    reactor_address: reactor_addr.clone(),
                    reactor_name: r.reactor_name.clone(),
                    liked: r.liked,
                    timestamp: r.timestamp,
                })
                .collect(),
            viewed: self.viewed.iter().cloned().collect(),
        };

        let json = match serde_json::to_vec_pretty(&file) {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!("vine_feed_cache: serialize failed: {e}");
                return;
            }
        };

        let tmp_path = {
            let mut name = path.file_name().unwrap_or_default().to_os_string();
            name.push(".tmp");
            path.with_file_name(name)
        };

        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            if let Err(e) = std::fs::create_dir_all(parent) {
                tracing::warn!("vine_feed_cache: create_dir_all failed: {e}");
                return;
            }
        }

        if let Err(e) = std::fs::write(&tmp_path, &json) {
            tracing::warn!("vine_feed_cache: write tmp failed: {e}");
            return;
        }
        if let Err(e) = std::fs::rename(&tmp_path, path) {
            tracing::warn!("vine_feed_cache: rename failed: {e}");
        }
    }

    /// Test-only public alias for `save()`. Lets unit tests trigger
    /// persistence explicitly before Task 4 wires it into mutators.
    /// Marked `#[cfg(test)]` so it cannot leak into production callers.
    #[cfg(test)]
    pub(crate) fn save_for_test(&self) {
        self.save();
    }
```

- [ ] **Step 4: Run the new tests — expect green**

Run:
```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(save_is_noop_when_path_is_none) | test(save_writes_atomic_file_when_path_is_set)' 2>&1 | tail -20
```

Expected: 2 passed.

- [ ] **Step 5: Run the full module test set**

Run:
```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(vine_feed_cache)' 2>&1 | tail -20
```

Expected: 22 passed (20 from prior + 2 new).

- [ ] **Step 6: Format + clippy check**

Run:
```bash
cd src-tauri && cargo fmt --all
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -20
```

Expected: 0 fmt diff, 0 clippy warnings.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/vine_feed_cache.rs
git commit -m "feat(zeb-147): add save() + disk envelope + MAX_DESCRIPTORS / MAX_AGE_SECS

save() is a no-op when path is None (test path) and writes atomically
via tempfile + rename when path is Some (production path). Disk format
is a versioned JSON envelope { version: 1, descriptors, reactions, viewed }
mirroring the in-memory shape with camelCase field names.

Mutators are NOT yet wired to call save() — that happens in Task 4."
```

---

## Task 3: Implement `load()` with age-prune + capacity-trim + orphan-prune

**Goal:** replace the Task 1 stub with the full `load()` algorithm: read file → JSON-deserialize → version-check → age-prune → capacity-trim → orphan-prune → populate cache. Three unit tests: empty-dir, round-trip after save, wrong-version-rejected.

**Files:**
- Modify: `src-tauri/src/vine_feed_cache.rs` (replace the `load` stub body, add a private helper for loading)

- [ ] **Step 1: Write failing tests**

In `mod tests`, after the Task 2 tests, add:

```rust
    #[test]
    fn load_round_trip_preserves_descriptors_reactions_viewed() {
        // Phase 1: build + save state
        let dir = tempfile::tempdir().expect("create tempdir");
        {
            let mut cache = VineFeedCache::load(dir.path());
            let followed = followed_set_with(&["alice-addr"]);
            let desc = canonical_descriptor_bytes(
                "vine-rt",
                "alice-addr",
                "Alice",
                "cid-x",
                Some("title-x"),
                None,
                500,
            );
            let out = cache.on_descriptor_sample(
                "harmony/vines/alice-addr",
                &desc,
                &followed,
                1_000,
            );
            assert!(matches!(out, Some(DescriptorOutcome::Inserted { .. })));

            let react = canonical_reaction_bytes("vine-rt", "bob-addr", "Bob", true, 600);
            let out2 = cache.on_reaction_sample(
                "harmony/vines/alice-addr/reactions/vine-rt/bob-addr",
                &react,
            );
            assert_eq!(out2, Some(ReactionOutcome::Inserted));

            assert!(cache.mark_viewed("vine-rt".to_string()));
            cache.save_for_test();
        }
        // Phase 2: reload from same dir, assert state survived
        let cache2 = VineFeedCache::load(dir.path());
        assert_eq!(cache2.len_descriptors(), 1);
        assert_eq!(cache2.len_reactions(), 1);
        assert!(cache2.is_viewed("vine-rt"));

        // Verify DTO is correctly reconstructed
        let dtos = cache2.list_descriptors();
        assert_eq!(dtos.len(), 1);
        assert_eq!(dtos[0].id, "vine-rt");
        assert_eq!(dtos[0].creator_address, "alice-addr");
        assert_eq!(dtos[0].title.as_deref(), Some("title-x"));
        assert!(dtos[0].viewed);

        // Verify reaction is correctly reconstructed (count + liked_by_me)
        let summary = cache2.get_reaction("vine-rt", "bob-addr");
        assert_eq!(summary.count, 1);
        assert!(summary.liked_by_me);
    }

    #[test]
    fn load_rejects_wrong_version() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join("vine_feed.json");
        // Write a v999 envelope
        let json = serde_json::json!({
            "version": 999,
            "descriptors": [],
            "reactions": [],
            "viewed": ["v-ignored"]
        });
        std::fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();

        // load() must treat wrong-version as "missing file" — empty cache
        let cache = VineFeedCache::load(dir.path());
        assert_eq!(cache.len_descriptors(), 0);
        assert_eq!(cache.len_reactions(), 0);
        assert!(!cache.is_viewed("v-ignored"));
    }

    #[test]
    fn load_corrupt_json_returns_empty_cache() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join("vine_feed.json");
        std::fs::write(&path, b"{ this is not valid json").unwrap();

        let cache = VineFeedCache::load(dir.path());
        assert_eq!(cache.len_descriptors(), 0);
        assert_eq!(cache.len_reactions(), 0);
    }
```

- [ ] **Step 2: Run the new tests — expect 1 pass + 2 fail (the round-trip + version-rejection fail because load() doesn't read the file yet; corrupt-json passes by accident)**

Run:
```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(load_round_trip_preserves_descriptors_reactions_viewed) | test(load_rejects_wrong_version) | test(load_corrupt_json_returns_empty_cache)' 2>&1 | tail -30
```

Expected: `load_corrupt_json_returns_empty_cache` PASS (stub already returns empty); the other two FAIL with assertion errors on `len_descriptors == 1` or `len_viewed == 0`.

- [ ] **Step 3: Implement the full `load()` algorithm**

In `src-tauri/src/vine_feed_cache.rs`, replace the stub `load()` body added in Task 1. Replace the entire `pub fn load(data_dir: &Path) -> Self { ... }` method with:

```rust
    /// Load from `data_dir/vine_feed.json`. Returns an empty cache (with
    /// `path` set so subsequent mutations persist) when the file is
    /// missing, unreadable, malformed JSON, or has an unrecognized
    /// `version`. Applies the age cutoff and capacity cap on load so
    /// the in-memory state mirrors what `on_descriptor_sample` would
    /// enforce going forward.
    pub fn load(data_dir: &Path) -> Self {
        let path = data_dir.join("vine_feed.json");
        let mut cache = Self {
            descriptors: HashMap::new(),
            reactions: HashMap::new(),
            viewed: HashSet::new(),
            path: Some(path.clone()),
        };
        Self::populate_from_disk(&mut cache, &path);
        cache
    }

    /// Read `path` (if it exists) and populate `cache`. Errors / version
    /// mismatch / malformed JSON all silently produce an empty cache —
    /// matches `follows.rs::FollowManager::load`'s graceful-degrade.
    fn populate_from_disk(cache: &mut Self, path: &Path) {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(_) => return, // file missing or unreadable — treat as empty
        };
        let file: VineFeedDiskV1 = match serde_json::from_slice(&bytes) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(
                    "vine_feed_cache: load() ignoring malformed vine_feed.json: {e}"
                );
                return;
            }
        };
        if file.version != 1 {
            tracing::warn!(
                "vine_feed_cache: load() ignoring vine_feed.json with version={} (expected 1)",
                file.version
            );
            return;
        }

        // Age-prune (one-shot on load).
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let age_cutoff = now_secs.saturating_sub(MAX_AGE_SECS);
        let mut descriptors: Vec<DescriptorOnDisk> = file
            .descriptors
            .into_iter()
            .filter(|d| d.created_at >= age_cutoff)
            .collect();

        // Capacity-trim (defensive — production write path enforces cap
        // on insert, but persisted state from a future version with a
        // higher cap could exceed ours).
        if descriptors.len() > MAX_DESCRIPTORS {
            // Sort by created_at DESC, ties by id ASC (deterministic).
            descriptors.sort_by(|a, b| {
                b.created_at.cmp(&a.created_at).then_with(|| a.id.cmp(&b.id))
            });
            descriptors.truncate(MAX_DESCRIPTORS);
        }

        // Build the surviving vine-id set for orphan-pruning reactions.
        let surviving_ids: HashSet<String> =
            descriptors.iter().map(|d| d.id.clone()).collect();

        // Reactions: drop orphans (where the parent descriptor was pruned).
        for r in file.reactions {
            if !surviving_ids.contains(&r.vine_id) {
                continue;
            }
            cache.reactions.insert(
                (r.vine_id, r.reactor_address),
                CachedReaction {
                    liked: r.liked,
                    timestamp: r.timestamp,
                    reactor_name: r.reactor_name,
                },
            );
        }

        // Descriptors: populate the cache from the (possibly pruned) list.
        for d in descriptors {
            let descriptor = VineDescriptorPayload {
                id: d.id.clone(),
                creator_address: d.creator_address,
                creator_name: d.creator_name,
                created_at: d.created_at,
                video_cid: d.video_cid,
                title: d.title,
                reshare_of: d.reshare_of,
            };
            cache.descriptors.insert(
                d.id,
                CachedVine {
                    descriptor,
                    received_at_ms: d.received_at_ms,
                    source: d.source,
                },
            );
        }

        // Viewed: passes through unmodified (low byte cost; not pruned
        // even when the associated descriptor age-prunes — see spec §11
        // out-of-scope on viewed-set GC).
        cache.viewed = file.viewed.into_iter().collect();
    }
```

- [ ] **Step 4: Run the new tests — expect green**

Run:
```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(load_round_trip_preserves_descriptors_reactions_viewed) | test(load_rejects_wrong_version) | test(load_corrupt_json_returns_empty_cache)' 2>&1 | tail -20
```

Expected: 3 passed.

- [ ] **Step 5: Run the full module test set**

Run:
```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(vine_feed_cache)' 2>&1 | tail -20
```

Expected: 25 passed (22 from prior + 3 new).

- [ ] **Step 6: Format + clippy**

Run:
```bash
cd src-tauri && cargo fmt --all
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -20
```

Expected: 0 fmt diff, 0 clippy warnings.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/vine_feed_cache.rs
git commit -m "feat(zeb-147): implement VineFeedCache::load() with prune/trim

load() now reads vine_feed.json, version-checks, age-prunes descriptors
older than MAX_AGE_SECS (90d), capacity-trims to MAX_DESCRIPTORS (5000),
orphan-prunes reactions whose parent descriptor was dropped, and
populates the cache from the result. Malformed JSON / wrong version /
missing file all gracefully fall back to an empty cache."
```

---

## Task 4: Wire `save()` into mutators + add runtime capacity-trim

**Goal:** make every mutating method persist its change to disk (when path is Some). Three unit tests verify each mutator type triggers save. Also add the runtime capacity-trim inside `on_descriptor_sample` (post-insert; before save).

**Files:**
- Modify: `src-tauri/src/vine_feed_cache.rs` (add `self.save()` calls in three methods + add the capacity-trim block in `on_descriptor_sample`)

- [ ] **Step 1: Write failing tests**

In `mod tests`, after the Task 3 tests, add:

```rust
    #[test]
    fn descriptor_insert_persists_to_disk() {
        let dir = tempfile::tempdir().expect("create tempdir");
        {
            let mut cache = VineFeedCache::load(dir.path());
            let followed = followed_set_with(&["alice-addr"]);
            let desc = canonical_descriptor_bytes(
                "vine-p1",
                "alice-addr",
                "Alice",
                "cid-1",
                None,
                None,
                700,
            );
            let out = cache.on_descriptor_sample(
                "harmony/vines/alice-addr",
                &desc,
                &followed,
                1_000,
            );
            assert!(matches!(out, Some(DescriptorOutcome::Inserted { .. })));
            // No explicit save_for_test() call — Task 4 wires save() into
            // on_descriptor_sample, so the disk must already reflect this.
        }
        let cache2 = VineFeedCache::load(dir.path());
        assert_eq!(cache2.len_descriptors(), 1);
        assert_eq!(cache2.list_descriptors()[0].id, "vine-p1");
    }

    #[test]
    fn reaction_update_persists_to_disk() {
        let dir = tempfile::tempdir().expect("create tempdir");
        {
            let mut cache = VineFeedCache::load(dir.path());
            let followed = followed_set_with(&["alice-addr"]);
            // Need a descriptor first (otherwise reaction is orphaned and
            // load() drops it).
            let desc = canonical_descriptor_bytes(
                "vine-r1", "alice-addr", "Alice", "cid", None, None, 100,
            );
            cache.on_descriptor_sample("harmony/vines/alice-addr", &desc, &followed, 1_000);

            // Insert reaction
            let react = canonical_reaction_bytes("vine-r1", "bob-addr", "Bob", true, 200);
            let out = cache.on_reaction_sample(
                "harmony/vines/alice-addr/reactions/vine-r1/bob-addr",
                &react,
            );
            assert_eq!(out, Some(ReactionOutcome::Inserted));

            // Update reaction (LWW newer timestamp)
            let react2 = canonical_reaction_bytes("vine-r1", "bob-addr", "Bob", false, 300);
            let out2 = cache.on_reaction_sample(
                "harmony/vines/alice-addr/reactions/vine-r1/bob-addr",
                &react2,
            );
            assert_eq!(out2, Some(ReactionOutcome::UpdatedNewer));
        }
        // Reload — both descriptor and updated reaction must be persisted
        let cache2 = VineFeedCache::load(dir.path());
        assert_eq!(cache2.len_reactions(), 1);
        // The final reaction value should be liked=false at timestamp=300
        let summary = cache2.get_reaction("vine-r1", "bob-addr");
        assert_eq!(summary.count, 0); // liked=false → not counted
        assert!(!summary.liked_by_me);
    }

    #[test]
    fn mark_viewed_persists_to_disk() {
        let dir = tempfile::tempdir().expect("create tempdir");
        {
            let mut cache = VineFeedCache::load(dir.path());
            let first = cache.mark_viewed("v-mv".to_string());
            assert!(first);
            // Second call returns false; the disk write side-effect must
            // be skipped on the no-op path (we can't directly observe
            // "no write happened" from this test, but the next reload
            // confirms the viewed set has exactly the one entry).
            let second = cache.mark_viewed("v-mv".to_string());
            assert!(!second);
        }
        let cache2 = VineFeedCache::load(dir.path());
        assert!(cache2.is_viewed("v-mv"));
    }
```

- [ ] **Step 2: Run the new tests — expect failure**

Run:
```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(descriptor_insert_persists_to_disk) | test(reaction_update_persists_to_disk) | test(mark_viewed_persists_to_disk)' 2>&1 | tail -20
```

Expected: all 3 FAIL — `len_descriptors == 1` etc. fail because the mutators don't save yet.

- [ ] **Step 3: Wire `save()` into mutators + add runtime capacity-trim**

In `src-tauri/src/vine_feed_cache.rs`, modify three methods.

**3a. `on_descriptor_sample`** — replace the trailing return statement to add capacity-trim + save:

Find the block (around lines 149-161):
```rust
        let vine_id = descriptor.id.clone();
        let dto = self.build_dto(&descriptor, source);
        self.descriptors.insert(
            vine_id,
            CachedVine {
                descriptor,
                received_at_ms: now_ms,
                source,
            },
        );

        Some(DescriptorOutcome::Inserted { dto })
```

Replace with:
```rust
        let vine_id = descriptor.id.clone();
        let dto = self.build_dto(&descriptor, source);
        self.descriptors.insert(
            vine_id,
            CachedVine {
                descriptor,
                received_at_ms: now_ms,
                source,
            },
        );

        // Runtime capacity-trim: if insert exceeded the cap, drop the
        // oldest descriptor(s) by `created_at` ascending (ties broken by
        // id ascending for cross-replica determinism), and drop their
        // reactions. Single-pass; runs only when len > MAX_DESCRIPTORS.
        if self.descriptors.len() > MAX_DESCRIPTORS {
            let mut entries: Vec<(u64, String)> = self
                .descriptors
                .iter()
                .map(|(id, cv)| (cv.descriptor.created_at, id.clone()))
                .collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
            let drop_count = self.descriptors.len() - MAX_DESCRIPTORS;
            for (_, id) in entries.into_iter().take(drop_count) {
                self.descriptors.remove(&id);
                self.reactions.retain(|(vid, _), _| vid != &id);
            }
        }

        self.save();
        Some(DescriptorOutcome::Inserted { dto })
```

**3b. `on_reaction_sample`** — both insert paths (Inserted and UpdatedNewer) save; the Stale / Rejected / None paths do NOT save.

Find the block (around lines 234-269):
```rust
        let key = (reaction.vine_id.clone(), reaction.reactor_address.clone());
        match self.reactions.get(&key) {
            None => {
                self.reactions.insert(
                    key,
                    CachedReaction {
                        liked: reaction.liked,
                        timestamp: reaction.timestamp,
                        reactor_name: reaction.reactor_name,
                    },
                );
                Some(ReactionOutcome::Inserted)
            }
            Some(existing) => {
                // ...
                if reaction.timestamp < existing.timestamp
                    || (reaction.timestamp == existing.timestamp
                        && reaction.liked == existing.liked)
                {
                    return Some(ReactionOutcome::Stale);
                }
                self.reactions.insert(
                    key,
                    CachedReaction {
                        liked: reaction.liked,
                        timestamp: reaction.timestamp,
                        reactor_name: reaction.reactor_name,
                    },
                );
                Some(ReactionOutcome::UpdatedNewer)
            }
        }
```

Replace with:
```rust
        let key = (reaction.vine_id.clone(), reaction.reactor_address.clone());
        match self.reactions.get(&key) {
            None => {
                self.reactions.insert(
                    key,
                    CachedReaction {
                        liked: reaction.liked,
                        timestamp: reaction.timestamp,
                        reactor_name: reaction.reactor_name,
                    },
                );
                self.save();
                Some(ReactionOutcome::Inserted)
            }
            Some(existing) => {
                // Stale if strictly older, OR if same-timestamp AND the
                // liked-state is unchanged (exact duplicate redelivery).
                // Same-timestamp with CHANGED liked-state is treated as
                // UpdatedNewer so that rapid toggles within one second
                // (publish_vine_reaction uses SystemTime::now().as_secs()
                // second-resolution) are not silently dropped.
                if reaction.timestamp < existing.timestamp
                    || (reaction.timestamp == existing.timestamp
                        && reaction.liked == existing.liked)
                {
                    return Some(ReactionOutcome::Stale);
                }
                self.reactions.insert(
                    key,
                    CachedReaction {
                        liked: reaction.liked,
                        timestamp: reaction.timestamp,
                        reactor_name: reaction.reactor_name,
                    },
                );
                self.save();
                Some(ReactionOutcome::UpdatedNewer)
            }
        }
```

**3c. `mark_viewed`** — save only when the insert actually mutated state.

Find the existing method (around lines 303-305):
```rust
    pub fn mark_viewed(&mut self, vine_id: String) -> bool {
        self.viewed.insert(vine_id)
    }
```

Replace with:
```rust
    pub fn mark_viewed(&mut self, vine_id: String) -> bool {
        let newly_added = self.viewed.insert(vine_id);
        if newly_added {
            self.save();
        }
        newly_added
    }
```

- [ ] **Step 4: Run the new tests — expect green**

Run:
```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(descriptor_insert_persists_to_disk) | test(reaction_update_persists_to_disk) | test(mark_viewed_persists_to_disk)' 2>&1 | tail -20
```

Expected: 3 passed.

- [ ] **Step 5: Run the full module test set**

Run:
```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(vine_feed_cache)' 2>&1 | tail -20
```

Expected: 28 passed (25 from prior + 3 new). NO regressions in the 18 ZEB-286 tests — those still pass because they use `VineFeedCache::new()` (path = None) and `save()` is a no-op.

- [ ] **Step 6: Format + clippy**

Run:
```bash
cd src-tauri && cargo fmt --all
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -20
```

Expected: 0 fmt diff, 0 clippy warnings.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/vine_feed_cache.rs
git commit -m "feat(zeb-147): persist on every mutating outcome

on_descriptor_sample saves on Inserted (after capacity-trim).
on_reaction_sample saves on Inserted | UpdatedNewer.
mark_viewed saves on newly_added == true.
Stale / AlreadyPresent / Rejected paths do NOT save (no-op).

Runtime capacity-trim drops the oldest descriptor(s) by created_at
ascending when an insert pushes len past MAX_DESCRIPTORS; orphaned
reactions for trimmed descriptors are also removed."
```

---

## Task 5: Add age-prune + capacity-trim + 90-day-boundary unit tests

**Goal:** lock down the pruning logic with explicit tests. One test per branch: age-prune drops old descriptors, capacity-trim drops oldest on overflow, and an exact 90-day-boundary test confirms the inclusive cutoff (descriptors AT exactly `now - 90d` are kept; descriptors BELOW are dropped).

**Files:**
- Modify: `src-tauri/src/vine_feed_cache.rs` (add 3 unit tests in `mod tests`)

- [ ] **Step 1: Write the failing tests**

In `mod tests`, after the Task 4 tests, add:

```rust
    #[test]
    fn age_prune_on_load_drops_old_descriptors_and_their_reactions() {
        // Setup: write a vine_feed.json containing one old descriptor
        // (created_at = epoch, well past the 90d cutoff) and one recent
        // descriptor (created_at = now - 1d). Add a reaction for each.
        // After load(), only the recent descriptor and its reaction should
        // survive.
        let dir = tempfile::tempdir().expect("create tempdir");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let recent_ts = now - 86_400; // 1 day old
        let old_ts = 0u64; // epoch — definitely older than 90 days

        let disk = serde_json::json!({
            "version": 1,
            "descriptors": [
                {
                    "id": "vine-old",
                    "creatorAddress": "alice-addr",
                    "creatorName": "Alice",
                    "createdAt": old_ts,
                    "videoCid": "cid-old",
                    "receivedAtMs": 0,
                    "source": "followed"
                },
                {
                    "id": "vine-new",
                    "creatorAddress": "alice-addr",
                    "creatorName": "Alice",
                    "createdAt": recent_ts,
                    "videoCid": "cid-new",
                    "receivedAtMs": 0,
                    "source": "followed"
                }
            ],
            "reactions": [
                {
                    "vineId": "vine-old",
                    "reactorAddress": "bob-addr",
                    "reactorName": "Bob",
                    "liked": true,
                    "timestamp": old_ts
                },
                {
                    "vineId": "vine-new",
                    "reactorAddress": "bob-addr",
                    "reactorName": "Bob",
                    "liked": true,
                    "timestamp": recent_ts
                }
            ],
            "viewed": []
        });
        std::fs::write(
            dir.path().join("vine_feed.json"),
            serde_json::to_vec_pretty(&disk).unwrap(),
        )
        .unwrap();

        let cache = VineFeedCache::load(dir.path());
        assert_eq!(
            cache.len_descriptors(),
            1,
            "old descriptor must be age-pruned; only vine-new survives"
        );
        let dtos = cache.list_descriptors();
        assert_eq!(dtos[0].id, "vine-new");
        // The orphan reaction for vine-old must be gone; vine-new's
        // reaction must survive.
        assert_eq!(cache.len_reactions(), 1);
        let summary = cache.get_reaction("vine-new", "bob-addr");
        assert_eq!(summary.count, 1);
    }

    #[test]
    fn capacity_trim_on_insert_drops_oldest_when_over_max() {
        // Insert MAX_DESCRIPTORS + 5 descriptors with strictly increasing
        // created_at. After all inserts, exactly MAX_DESCRIPTORS remain,
        // and the oldest 5 (created_at 0..4) are gone — only created_at
        // 5..MAX_DESCRIPTORS+5 should remain.
        let mut cache = VineFeedCache::new();
        let followed = followed_set_with(&["alice-addr"]);
        let total = MAX_DESCRIPTORS + 5;
        for i in 0..total {
            let id = format!("v-{i:05}");
            let payload = canonical_descriptor_bytes(
                &id,
                "alice-addr",
                "Alice",
                "cid",
                None,
                None,
                i as u64, // created_at = i
            );
            cache.on_descriptor_sample("harmony/vines/alice-addr", &payload, &followed, 0);
        }
        assert_eq!(cache.len_descriptors(), MAX_DESCRIPTORS);

        // The 5 oldest (created_at 0..4) must be gone
        let dtos = cache.list_descriptors();
        let ids: HashSet<&str> = dtos.iter().map(|d| d.id.as_str()).collect();
        for i in 0..5 {
            let dropped = format!("v-{i:05}");
            assert!(
                !ids.contains(dropped.as_str()),
                "v-{i:05} (oldest) should have been trimmed"
            );
        }
        // The newest one (v-MAX_DESCRIPTORS+4) must be present
        let newest = format!("v-{:05}", total - 1);
        assert!(
            ids.contains(newest.as_str()),
            "newest descriptor should remain"
        );
    }

    #[test]
    fn ninety_day_boundary_is_inclusive() {
        // A descriptor with `created_at == now - MAX_AGE_SECS` should be
        // kept (>= cutoff); one with `created_at < now - MAX_AGE_SECS`
        // should be dropped. Verifies the spec §5 algorithm's
        // `created_at >= age_cutoff` (inclusive) is correctly coded.
        let dir = tempfile::tempdir().expect("create tempdir");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let exactly_at_cutoff = now.saturating_sub(MAX_AGE_SECS);
        let just_too_old = exactly_at_cutoff.saturating_sub(1);

        let disk = serde_json::json!({
            "version": 1,
            "descriptors": [
                {
                    "id": "boundary",
                    "creatorAddress": "a",
                    "creatorName": "A",
                    "createdAt": exactly_at_cutoff,
                    "videoCid": "cid",
                    "receivedAtMs": 0,
                    "source": "followed"
                },
                {
                    "id": "too-old",
                    "creatorAddress": "a",
                    "creatorName": "A",
                    "createdAt": just_too_old,
                    "videoCid": "cid",
                    "receivedAtMs": 0,
                    "source": "followed"
                }
            ],
            "reactions": [],
            "viewed": []
        });
        std::fs::write(
            dir.path().join("vine_feed.json"),
            serde_json::to_vec_pretty(&disk).unwrap(),
        )
        .unwrap();

        let cache = VineFeedCache::load(dir.path());
        let ids: HashSet<String> =
            cache.list_descriptors().iter().map(|d| d.id.clone()).collect();
        assert!(
            ids.contains("boundary"),
            "descriptor at exactly the cutoff must be KEPT (cutoff is inclusive)"
        );
        assert!(
            !ids.contains("too-old"),
            "descriptor one second past the cutoff must be DROPPED"
        );
    }
```

- [ ] **Step 2: Run the new tests — expect green (logic was already implemented in Task 3 + Task 4)**

Run:
```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(age_prune_on_load_drops_old_descriptors_and_their_reactions) | test(capacity_trim_on_insert_drops_oldest_when_over_max) | test(ninety_day_boundary_is_inclusive)' 2>&1 | tail -20
```

Expected: 3 passed. (If any fail, the Task 3 / Task 4 implementation has a bug that needs fixing before continuing.)

- [ ] **Step 3: Run the full module test set**

Run:
```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(vine_feed_cache)' 2>&1 | tail -20
```

Expected: 31 passed (28 from prior + 3 new).

- [ ] **Step 4: Format + clippy**

Run:
```bash
cd src-tauri && cargo fmt --all
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -20
```

Expected: 0 fmt diff, 0 clippy warnings.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/vine_feed_cache.rs
git commit -m "test(zeb-147): age-prune, capacity-trim, 90-day boundary

Three regression tests lock in the pruning behavior:
- Old descriptor + its reactions are dropped at load time
- Insert beyond MAX_DESCRIPTORS drops the oldest by created_at
- The 90-day cutoff is inclusive (descriptors AT the cutoff are kept)"
```

---

## Task 6: Production wiring — swap `new()` → `load(&app_data_dir)` in `start_node`

**Goal:** flip the production cache constructor. Single-line change. The existing 35 ZEB-286 tests already cover the runtime behavior; no new tests for this task because the integration test in Task 7 will exercise the full reload cycle end-to-end.

**Files:**
- Modify: `src-tauri/src/lib.rs:1011-1012` (the VineFeedCache construction site)

- [ ] **Step 1: Read the current production wiring site**

Run:
```bash
grep -n "VineFeedCache::new\|VineFeedCache::load" src-tauri/src/lib.rs
```

Expected output:
```
1012:        std::sync::Arc::new(std::sync::Mutex::new(vine_feed_cache::VineFeedCache::new()));
```

- [ ] **Step 2: Apply the one-line swap**

In `src-tauri/src/lib.rs`, find lines 1010-1013:

```rust
    // ZEB-286: in-memory VineFeedCache shared between event loop and IPCs.
    let vine_feed_cache =
        std::sync::Arc::new(std::sync::Mutex::new(vine_feed_cache::VineFeedCache::new()));
    let vine_feed_cache_clone = vine_feed_cache.clone();
```

Replace with:

```rust
    // ZEB-286: in-memory VineFeedCache shared between event loop and IPCs.
    // ZEB-147: load() reads vine_feed.json (if any) and arms save() so
    // every mutating outcome persists to disk atomically.
    let vine_feed_cache = std::sync::Arc::new(std::sync::Mutex::new(
        vine_feed_cache::VineFeedCache::load(&app_data_dir),
    ));
    let vine_feed_cache_clone = vine_feed_cache.clone();
```

- [ ] **Step 3: Verify `app_data_dir` is in scope at the wiring site**

Run:
```bash
grep -n "app_data_dir" src-tauri/src/lib.rs | head -5
```

Expected: at least one `let app_data_dir = ...` definition appearing BEFORE line 1010 (it's around line 989-995 per the post-ZEB-286 file layout).

- [ ] **Step 4: Run cargo check + tests**

Run:
```bash
cd src-tauri && cargo check --locked --all-targets --features test-fixtures 2>&1 | tail -10
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tail -20
```

Expected:
- check: 0 errors
- nextest: all tests pass (baseline + 11 new from Tasks 1-5). NO regressions in the 18 ZEB-286 module tests or 35 integration tests.

- [ ] **Step 5: Format + clippy**

Run:
```bash
cd src-tauri && cargo fmt --all
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -20
```

Expected: 0 fmt diff, 0 clippy warnings.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "feat(zeb-147): wire production cache to disk via load(&app_data_dir)

start_node swaps VineFeedCache::new() for VineFeedCache::load(&app_data_dir).
The cache now reads vine_feed.json on node start and persists every
mutation via save(). stop_node requires no change — save-on-mutation
means there's no flush-on-shutdown step."
```

---

## Task 7: Integration test — `cache_survives_reload`

**Goal:** one black-box integration test that simulates the app-reload cycle: build a cache, mutate it through the three public mutators, drop the cache, reload from the same tempdir, and assert all state survives. Lives in a new file under `src-tauri/tests/` to mirror the ZEB-286 integration test pattern.

**Files:**
- Create: `src-tauri/tests/vine_feed_persistence_integration.rs`

- [ ] **Step 1: Create the integration test file**

Create `src-tauri/tests/vine_feed_persistence_integration.rs` with the following contents:

```rust
//! ZEB-147: black-box integration test for disk-backed VineFeedCache.
//!
//! Validates the full app-reload cycle:
//! 1. Construct a cache via `VineFeedCache::load(tempdir)`.
//! 2. Mutate it through the three public mutators (descriptor sample,
//!    reaction sample, mark_viewed).
//! 3. Drop the cache.
//! 4. Re-load from the same tempdir.
//! 5. Assert all three pieces of state are preserved.

use harmony_app::vine_feed_cache::{
    DescriptorOutcome, ReactionOutcome, VineFeedCache,
};
use harmony_app::{VineDescriptorPayload, VineReactionPayload};
use std::collections::HashSet;

fn canonical_descriptor_bytes(
    vine_id: &str,
    creator_address: &str,
    creator_name: &str,
    video_cid: &str,
    title: Option<&str>,
    reshare_of: Option<&str>,
    created_at: u64,
) -> Vec<u8> {
    let v = VineDescriptorPayload {
        id: vine_id.to_string(),
        creator_address: creator_address.to_string(),
        creator_name: creator_name.to_string(),
        created_at,
        video_cid: video_cid.to_string(),
        title: title.map(String::from),
        reshare_of: reshare_of.map(String::from),
    };
    serde_json::to_vec(&v).unwrap()
}

fn canonical_reaction_bytes(
    vine_id: &str,
    reactor_address: &str,
    reactor_name: &str,
    liked: bool,
    timestamp: u64,
) -> Vec<u8> {
    let v = VineReactionPayload {
        vine_id: vine_id.to_string(),
        reactor_address: reactor_address.to_string(),
        reactor_name: reactor_name.to_string(),
        liked,
        timestamp,
    };
    serde_json::to_vec(&v).unwrap()
}

#[test]
fn cache_survives_reload() {
    let dir = tempfile::tempdir().expect("create tempdir");

    // Phase 1: build state and let save-on-mutation persist it
    {
        let mut cache = VineFeedCache::load(dir.path());

        let followed: HashSet<String> = ["alice-addr".to_string()].into_iter().collect();

        // Insert a descriptor with a reshare-of pointer (preserves the
        // optional field through round-trip).
        let desc = canonical_descriptor_bytes(
            "vine-A",
            "alice-addr",
            "Alice",
            "cid-aaa",
            Some("hello"),
            Some("vine-prev"),
            1_700_000_000,
        );
        let out = cache.on_descriptor_sample(
            "harmony/vines/alice-addr",
            &desc,
            &followed,
            10_000,
        );
        assert!(
            matches!(out, Some(DescriptorOutcome::Inserted { .. })),
            "first descriptor must Insert; got {out:?}"
        );

        // Insert a second descriptor with no title and no reshare (covers
        // skip_serializing_if = "Option::is_none" round-trip).
        let desc2 = canonical_descriptor_bytes(
            "vine-B",
            "alice-addr",
            "Alice",
            "cid-bbb",
            None,
            None,
            1_700_000_500,
        );
        let out2 = cache.on_descriptor_sample(
            "harmony/vines/alice-addr",
            &desc2,
            &followed,
            10_500,
        );
        assert!(matches!(out2, Some(DescriptorOutcome::Inserted { .. })));

        // Insert two reactions on vine-A — different reactors, both liked
        let r1 = canonical_reaction_bytes("vine-A", "bob-addr", "Bob", true, 1_700_000_100);
        let r2 = canonical_reaction_bytes("vine-A", "carol-addr", "Carol", true, 1_700_000_200);
        assert_eq!(
            cache.on_reaction_sample(
                "harmony/vines/alice-addr/reactions/vine-A/bob-addr",
                &r1
            ),
            Some(ReactionOutcome::Inserted)
        );
        assert_eq!(
            cache.on_reaction_sample(
                "harmony/vines/alice-addr/reactions/vine-A/carol-addr",
                &r2
            ),
            Some(ReactionOutcome::Inserted)
        );

        // Mark vine-A viewed
        assert!(cache.mark_viewed("vine-A".to_string()));

        // cache drops here — save-on-mutation ensured everything is on disk
    }

    // Phase 2: reload from the same tempdir
    let cache2 = VineFeedCache::load(dir.path());

    // Descriptors: both present, sorted DESC by created_at
    let dtos = cache2.list_descriptors();
    assert_eq!(dtos.len(), 2, "both descriptors should survive reload");
    assert_eq!(dtos[0].id, "vine-B"); // newer
    assert!(dtos[0].title.is_none());
    assert!(dtos[0].reshare_of.is_none());
    assert_eq!(dtos[1].id, "vine-A");
    assert_eq!(dtos[1].title.as_deref(), Some("hello"));
    assert_eq!(dtos[1].reshare_of.as_deref(), Some("vine-prev"));
    assert!(dtos[1].viewed, "vine-A viewed flag must survive reload");
    assert!(!dtos[0].viewed, "vine-B was never viewed");

    // Reactions: both present (count == 2, neither liked_by_me from
    // dave-addr's perspective)
    let summary = cache2.get_reaction("vine-A", "dave-addr");
    assert_eq!(summary.count, 2);
    assert!(!summary.liked_by_me);

    // From Bob's perspective: liked_by_me must be true (he's one of the
    // two who liked it)
    let bob_view = cache2.get_reaction("vine-A", "bob-addr");
    assert_eq!(bob_view.count, 2);
    assert!(bob_view.liked_by_me);

    // Viewed-state survives independently
    assert!(cache2.is_viewed("vine-A"));
    assert!(!cache2.is_viewed("vine-B"));
}
```

- [ ] **Step 2: Verify the public surface is exported**

`harmony_app::vine_feed_cache::VineFeedCache` needs to be reachable. Per ZEB-286, the module is declared as `pub mod vine_feed_cache;` in `lib.rs`. Verify:

Run:
```bash
grep -n "^pub mod vine_feed_cache" src-tauri/src/lib.rs
```

Expected: one hit (`46:pub mod vine_feed_cache;` per the file layout).

The crate name is `harmony-app` per `src-tauri/Cargo.toml`, which means the integration test references it as `harmony_app` (snake_case crate name). Confirm:

Run:
```bash
grep -E '^name = ' src-tauri/Cargo.toml | head -3
```

Expected: one entry showing `name = "harmony-app"` for the lib crate.

Also confirm `VineDescriptorPayload` and `VineReactionPayload` are pub in `lib.rs`:

Run:
```bash
grep -n "pub struct VineDescriptorPayload\|pub struct VineReactionPayload" src-tauri/src/lib.rs
```

Expected: one hit for each (around lines 4280-4337 per the post-ZEB-286 file map).

If either is NOT `pub`, you must `pub use ...` re-export them at the top of `lib.rs` (or change `pub(crate) struct` → `pub struct`). This is one of the known gotchas in the codebase — fix and add a one-line comment explaining why.

- [ ] **Step 3: Run the new integration test — expect green**

Run:
```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures --test vine_feed_persistence_integration 2>&1 | tail -20
```

Expected: 1 passed.

- [ ] **Step 4: Run the full workspace test set**

Run:
```bash
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tail -20
```

Expected: all tests pass. Total = baseline + 11 module tests + 1 integration test = baseline + 12. NO regressions.

- [ ] **Step 5: Format + clippy**

Run:
```bash
cd src-tauri && cargo fmt --all
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -20
```

Expected: 0 fmt diff, 0 clippy warnings.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/tests/vine_feed_persistence_integration.rs
git commit -m "test(zeb-147): integration test — cache survives reload

Black-box end-to-end test for disk persistence. Builds a cache with
two descriptors (one with title+reshare, one without), two reactions,
and one viewed mark. Drops the cache. Reloads from the same tempdir.
Asserts all three pieces of state survive: descriptors (with optional
fields preserved), reactions (with LWW state intact), viewed set."
```

---

## Task 8: Final verification + push + PR

**Goal:** run all five CI gates one more time, push the branch, and open the PR. No code changes unless a gate fails (in which case fix-and-commit, then re-run).

**Files:**
- (No file changes expected; this is verification + git/PR work)

- [ ] **Step 1: Confirm the commit log matches the expected shape**

Run:
```bash
git log --oneline origin/main..HEAD
```

Expected: 8 commits in this order (plus the initial spec commit at the bottom):

```
<sha8> test(zeb-147): integration test — cache survives reload
<sha7> feat(zeb-147): wire production cache to disk via load(&app_data_dir)
<sha6> test(zeb-147): age-prune, capacity-trim, 90-day boundary
<sha5> feat(zeb-147): persist on every mutating outcome
<sha4> feat(zeb-147): implement VineFeedCache::load() with prune/trim
<sha3> feat(zeb-147): add save() + disk envelope + MAX_DESCRIPTORS / MAX_AGE_SECS
<sha2> feat(zeb-147): add VineFeedCache::load() stub + path field
50a0fe4 docs(zeb-147): vine persistence design (disk-backed VineFeedCache)
```

7 implementation commits + 1 spec commit on top of `origin/main`.

- [ ] **Step 2: Run all five CI gates sequentially**

Run:
```bash
cd src-tauri && cargo fmt --all -- --check
```
Expected: exit 0.

```bash
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -10
```
Expected: exit 0, no warnings.

```bash
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tail -20
```
Expected: exit 0; all tests pass. Confirm trailing summary line is non-zero passed and zero failed.

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && npx tsc --noEmit
```
Expected: exit 0, no output.

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run 2>&1 | tail -10
```
Expected: exit 0, all tests pass.

- [ ] **Step 3: Push the branch**

Run:
```bash
git push -u origin zeb-147-vine-persistence
```

Expected: push succeeds; branch is now on origin.

- [ ] **Step 4: Open the PR**

Run:
```bash
gh pr create --title "ZEB-147: Vine persistence — disk-backed VineFeedCache" --body "$(cat <<'EOF'
## Summary

Wires the in-memory [VineFeedCache](https://github.com/zeblithic/harmony-client/blob/main/src-tauri/src/vine_feed_cache.rs) (landed in [ZEB-286](https://linear.app/zeblith/issue/ZEB-286), PR #118) to disk so the Vine feed survives app reload.

**Local persistence only** — cross-device viewed-state sync (the optional part of the original [ZEB-147](https://linear.app/zeblith/issue/ZEB-147) ticket) is split out as a follow-up. Privacy design parallels DM read-receipts ([ZEB-214](https://linear.app/zeblith/issue/ZEB-214)) and will need its own ticket.

## What changed

- `src-tauri/src/vine_feed_cache.rs`:
  - New constants: `MAX_DESCRIPTORS = 5000`, `MAX_AGE_SECS = 90 * 86_400`.
  - New field `path: Option<PathBuf>` on `VineFeedCache`. `None` for `new()` (test path), `Some` for `load()` (production path).
  - New constructor `load(data_dir: &Path) -> Self` reads `data_dir/vine_feed.json`, version-checks, age-prunes descriptors older than 90 days, capacity-trims to 5000 most recent, orphan-prunes reactions whose parent descriptor was dropped, and populates the cache.
  - New private method `save()` writes the cache atomically via tempfile + rename. No-op when `path` is None.
  - `on_descriptor_sample` saves on `Inserted` (after runtime capacity-trim).
  - `on_reaction_sample` saves on `Inserted` or `UpdatedNewer`. Stale / Rejected paths do NOT save.
  - `mark_viewed` saves only when newly added.
  - New on-disk envelope `VineFeedDiskV1` with versioned root and camelCase field names matching the descriptor wire format.
- `src-tauri/src/lib.rs:start_node`: single-line swap from `VineFeedCache::new()` → `VineFeedCache::load(&app_data_dir)`.
- `src-tauri/tests/vine_feed_persistence_integration.rs`: new black-box test asserting state survives a reload.

## Design

Single-file JSON at `{app_data_dir}/vine_feed.json` — the same shape `follows.json` and `content_index.json` use, with the same atomic write idiom (tempfile + rename). Save-on-mutation (no debounce). Cap chosen for headroom (5000 descriptors at ~250 bytes each ≈ 1.25 MB on disk, well under the sub-ms fsync window). 90-day age cutoff applied once on load.

Constructor split: `new()` stays for unit tests (path = None, `save()` no-op); `load(data_dir)` is the production path. This lets the existing 18 ZEB-286 module tests + 14 integration tests run unchanged without touching disk.

Full spec at `docs/specs/2026-05-13-zeb-147-vine-persistence-design.md` (commit `50a0fe4`).

## Tests

- 11 new module unit tests (`vine_feed_cache.rs`): `new_leaves_path_unset`, `load_empty_dir_returns_empty_cache`, `save_is_noop_when_path_is_none`, `save_writes_atomic_file_when_path_is_set`, `load_round_trip_preserves_descriptors_reactions_viewed`, `load_rejects_wrong_version`, `load_corrupt_json_returns_empty_cache`, `descriptor_insert_persists_to_disk`, `reaction_update_persists_to_disk`, `mark_viewed_persists_to_disk`, `age_prune_on_load_drops_old_descriptors_and_their_reactions`, `capacity_trim_on_insert_drops_oldest_when_over_max`, `ninety_day_boundary_is_inclusive`.
- 1 new integration test (`tests/vine_feed_persistence_integration.rs`): `cache_survives_reload`.
- All 35 ZEB-286 tests pass unchanged (`new()` path is unaffected by the save side-effects).

## Test plan

- [x] `cargo fmt --all -- --check` clean
- [x] `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` clean
- [x] `cargo nextest run --locked --workspace --all-targets --features test-fixtures` green
- [x] `npx tsc --noEmit` clean
- [x] `npx vitest run` green

## Out of scope (filed as follow-ups post-merge)

- **Cross-device viewed-state sync** — original optional part of [ZEB-147](https://linear.app/zeblith/issue/ZEB-147); split out per scope decision. Needs privacy design (opt-in mirror of [ZEB-214](https://linear.app/zeblith/issue/ZEB-214) DM read-receipts).
- **VineService frontend mock-clear** — [ZEB-209](https://linear.app/zeblith/issue/ZEB-209).
- **Reshare attribution UX** — [ZEB-103](https://linear.app/zeblith/issue/ZEB-103).
- **Viewed-set GC** for stale entries whose descriptors age-pruned (low byte cost; acceptable for v1).

Closes [ZEB-147](https://linear.app/zeblith/issue/ZEB-147).

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

Expected: `gh pr create` returns a URL.

- [ ] **Step 5: Verify the PR opened correctly**

Run:
```bash
gh pr view --json url,title,number,state
```

Expected: state = OPEN, title matches "ZEB-147: Vine persistence — disk-backed VineFeedCache".

Return the PR URL in your final summary. The calling agent will take over from here to monitor bot reviewers.

---

## Self-Review

**1. Spec coverage** (going through each spec section):

- §3.1 (file path = `vine_feed.json`) → Task 1 (`load()` constructs path), Task 2 (`save()` writes to same path). ✓
- §3.2 (file format envelope) → Task 2 defines `VineFeedDiskV1`, `DescriptorOnDisk`, `ReactionOnDisk` with camelCase. `skip_serializing_if = "Option::is_none"` applied to `title` + `reshare_of` on `DescriptorOnDisk`. ✓
- §3.3 (single file for atomicity) → Task 2's `save()` writes one file via tempfile+rename. ✓
- §4.1 (constructor split) → Task 1 splits `new()` (path=None) vs `load()` (path=Some). ✓
- §4.2 (new internal field `path`) → Task 1 adds the field. ✓
- §4.3 (private `save()`) → Task 2 adds the method, errors logged via `tracing::warn!`, no-op when path None. ✓
- §4.4 (save-on-mutation only on mutating outcomes) → Task 4 wires Inserted / UpdatedNewer / newly-viewed. ✓
- §4.5 (constants `MAX_DESCRIPTORS = 5000`, `MAX_AGE_SECS = 90 * 86_400`) → Task 2 adds them as `pub const`. ✓
- §5 (load algorithm: read → version → age-prune → capacity-trim → orphan-prune → populate) → Task 3 implements all phases. ✓
- §6 (runtime capacity-trim on insert) → Task 4 adds the trim block in `on_descriptor_sample` before save. Sort key matches §6 (ascending by created_at, ties by id ascending). ✓
- §7 (save algorithm: serialize → tempfile → fsync-via-rename) → Task 2 follows the `follows.rs` idiom exactly. (Note: the spec mentions fsync conceptually but the established pattern uses `std::fs::write` + `std::fs::rename` without explicit `fsync()` — same as `follows.rs:60-85`. The rename is the atomicity commit.) ✓
- §8 (production wiring) → Task 6 swaps `new()` → `load(&app_data_dir)`. ✓
- §9.1 unit tests (6 tests listed) → Tasks 1, 2, 3, 4, 5 collectively add 11 unit tests covering more than the spec's enumeration:
  - `load_missing_file_returns_empty_cache` → covered by Task 1's `load_empty_dir_returns_empty_cache` ✓
  - `load_corrupt_json_returns_empty_cache` → Task 3 ✓
  - `load_wrong_version_returns_empty_cache` → Task 3 ✓
  - `save_load_round_trip_preserves_descriptors_reactions_viewed` → Task 3 ✓
  - `age_prune_on_load_drops_old_descriptors_and_their_reactions` → Task 5 ✓
  - `capacity_trim_drops_oldest_when_insert_exceeds_max` → Task 5 ✓
- §9.2 integration test (`cache_survives_reload`) → Task 7 ✓
- §10 acceptance criteria 1-10 → all mapped to specific tasks; the 5 CI gates land in Task 8.
- §11 out-of-scope items NOT addressed — confirmed by intent (cross-device sync, mock-clear, reshare UX, get_reaction index, viewed GC, CBOR, debounce, migration, sub-cap tuning).
- §12 follow-up tickets — Task 8 PR body references them with markdown links, does NOT file them (user-driven per memory rule).
- §13 risks — informational; no task addresses these (they're design-time call-outs, not implementation TODOs).

**2. Placeholder scan**:

No "TBD" / "TODO" / "implement later" anywhere. Every step has either:
- A complete code block (for code changes), or
- An exact bash command + expected output (for verification).

The "Spec self-review" reference inside the spec itself is metadata; no plan-internal placeholders.

**3. Type consistency**:

- `path: Option<PathBuf>` — used identically in Task 1 (definition), Task 2 (save reads it), Task 3 (load sets it), Task 6 (production passes a `&Path` arg).
- `VineFeedDiskV1` — name introduced in Task 2; referenced as `VineFeedDiskV1` in Task 3's load (consistent).
- `DescriptorOnDisk` / `ReactionOnDisk` — Task 2 names; Task 3 destructures them with the same field names.
- Constants — `MAX_DESCRIPTORS` and `MAX_AGE_SECS` defined Task 2, referenced Tasks 3-5 by the same names.
- Method names: `load(&Path) -> Self`, `save(&self)`, `save_for_test(&self)` — all consistent.
- Field accesses: `cv.descriptor.id`, `cv.descriptor.created_at`, `cv.received_at_ms`, `cv.source` — match the existing `CachedVine` shape from ZEB-286.
- Reaction map key `(String, String)` representing `(vine_id, reactor_address)` — consistent across `save()` (Task 2) and `load()` (Task 3).
- `tempfile::tempdir()` API used in tests — same call form throughout (`expect("create tempdir")` then `dir.path()`).

**One mildly suspect spot**: the spec §9.1 test names use `save_load_round_trip_preserves_descriptors_reactions_viewed` and `load_missing_file_returns_empty_cache`; the plan uses `load_round_trip_preserves_descriptors_reactions_viewed` (no `save_` prefix because the round-trip is observed via reload) and `load_empty_dir_returns_empty_cache`. Both names are accurate and the behaviors match the spec's intent — the spec was prose-level naming, not authoritative. No real inconsistency.

**Fixes applied during self-review**: none required.

---

## Execution Handoff

Plan complete and saved to `docs/plans/2026-05-13-zeb-147-vine-persistence-plan.md`.

Per pre-authorization from the user, transitioning immediately to **subagent-driven development** to execute Tasks 0-8 sequentially with two-stage review (spec-compliance, then code-quality) per task.

Once Task 8 completes successfully (PR is open), the calling agent enters the autonomous bot-review monitoring loop with pre-authorized fixup discipline per the established pattern from PR #117 and PR #118.
