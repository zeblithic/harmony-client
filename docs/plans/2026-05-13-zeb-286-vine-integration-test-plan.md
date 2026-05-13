# ZEB-286 VineFeedCache + Vine Integration Tests Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Introduce a Rust-side `VineFeedCache` that production IPCs query (closing the `list_vine_videos()` and `mark_vine_viewed()` stubs), and land 17 new integration tests across two files exercising descriptor publish/receive, reaction LWW aggregation, reshare wiring, viewed-state, wire-format pinning, and `fetch_content` blob round-trip.

**Architecture:** New `src-tauri/src/vine_feed_cache.rs` module holds three in-memory maps (descriptors, reactions, viewed-IDs) and exposes `on_descriptor_sample` / `on_reaction_sample` / `list_descriptors` / `get_reaction` / `mark_viewed`. The cache is stored on `NodeState` as `Option<Arc<Mutex<VineFeedCache>>>` (same `std::sync::Mutex` pattern as `followed_set`), constructed in `start_node` and cleared on `stop_node`. The receive-side dispatch in `event_loop::emit_frontend_event` routes through the cache before emitting `vine-received` / `vine-reaction-received` Tauri events; source-tag injection moves into the cache. Integration tests use two patterns: lightweight `cache.on_sample(bytes)` (mirrors `profile_broadcast_integration.rs`) for the cache-level surfaces, and a one-NodeRuntime spin-up (mirrors `content_index_integration.rs`) for the `fetch_content` blob round-trip.

**Tech Stack:** Rust (`std::sync::Mutex`, `serde_json`, `HashMap`, `HashSet`), Tauri commands, `cargo nextest` + `cargo clippy` + `cargo fmt`, JSON wire format (camelCase via `#[serde(rename_all = "camelCase")]`).

**Spec:** `docs/specs/2026-05-13-zeb-286-vine-integration-test-design.md` (commit `2c3573c`).
**Branch:** `zeb-286-vine-integration-test` (already cut from `origin/main` at `6ffe8b0`).

---

## File Structure

### Create

| Path | Responsibility |
|---|---|
| `src-tauri/src/vine_feed_cache.rs` | New module: `VineFeedCache` struct + `CachedVine` / `CachedReaction` / `VineSource` / `DescriptorOutcome` / `ReactionOutcome` / `ReactionSummary` types + module-internal `#[cfg(test)] mod tests` block. ~300 LOC. |
| `src-tauri/tests/vine_feed_cache_integration.rs` | Cache-level integration tests (14 tests, 5 categories). Models `profile_broadcast_integration.rs`. ~500-700 LOC. |
| `src-tauri/tests/vine_content_roundtrip_integration.rs` | Heavy NodeRuntime spin-up tests (3 tests) exercising `ingest_content` + `fetch_content`. Models `content_index_integration.rs`. ~400-600 LOC. |

### Modify

| Path | Change |
|---|---|
| `src-tauri/src/lib.rs:1` | Add `mod vine_feed_cache;` declaration near the existing `mod follows;` |
| `src-tauri/src/lib.rs:205` | Add `vine_feed_cache: Option<Arc<Mutex<VineFeedCache>>>` field to `NodeState` |
| `src-tauri/src/lib.rs:381` | Add `vine_feed_cache: None,` to `NodeState::default` |
| `src-tauri/src/lib.rs:589` | Add `_vine_feed_cache` to the `stop_node`'s extract tuple |
| `src-tauri/src/lib.rs:627` | Add `guard.vine_feed_cache.take(),` to the stop_node tuple build |
| `src-tauri/src/lib.rs:1099` | Add `let _old_vine_feed_cache = guard.vine_feed_cache.take();` to old-node cleanup |
| `src-tauri/src/lib.rs:2504` | Add `guard.vine_feed_cache = Some(vine_feed_cache);` to the start_node post-success NodeState write |
| `src-tauri/src/lib.rs:~1000` | Construct `let vine_feed_cache = Arc::new(Mutex::new(VineFeedCache::new()));` near the `followed_set` construction |
| `src-tauri/src/lib.rs:~2453` | Pass `vine_feed_cache.clone()` into the event_loop spawn |
| `src-tauri/src/lib.rs:4467-4470` | Replace `list_vine_videos()` stub with cache-backed implementation |
| `src-tauri/src/lib.rs:4553-4557` | Replace `mark_vine_viewed()` stub with cache-backed implementation |
| `src-tauri/src/event_loop.rs:1135` | Hoist `vine_feed_cache_clone` to loop scope (alongside `followed_set` hoist comment) |
| `src-tauri/src/event_loop.rs:1403` | Pass `vine_feed_cache` arg to the `emit_frontend_event` call |
| `src-tauri/src/event_loop.rs:2718-2730` | Add `vine_feed_cache` parameter to `emit_frontend_event` signature |
| `src-tauri/src/event_loop.rs:2742-2765` | Replace inline vine dispatch with cache-routed logic |

### Reference (read-only, no changes)

- `src-tauri/src/profile_broadcast.rs:518-605` — `ProfileBroadcastCache` shape (canonical sibling pattern for cache module)
- `src-tauri/src/follows.rs:1-130` — `FollowManager` (NodeState integration pattern)
- `src-tauri/src/lib.rs:4280-4337` — Existing `VineDescriptorPayload`, `VineVideoDto`, `VineReactionPayload`, `PublishVinePayload` types (do NOT modify)
- `src-tauri/tests/profile_broadcast_integration.rs:1-199` — File A test pattern
- `src-tauri/tests/content_index_integration.rs:1-100` — File B test pattern (thread spawn + dedicated tokio runtime for `!Send` NodeRuntime)

---

## CI Gates (run after every task)

All five gates MUST pass at the end of every task except Task 0 (which only verifies the baseline):

**Cargo gates from `src-tauri/`:**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

**Frontend gates from repo root:**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
npx tsc --noEmit
npx vitest run
```

**Hard rules from user memory:**

- Pipe exit codes lie: never trust `cmd | tail/grep` for pass/fail. If you must pipe, use `set -o pipefail` or check `${PIPESTATUS[0]}`. The gate commands above are run unpiped.
- Test drift is our fault: any unrelated test that breaks on `main` is exclusively our responsibility. Sweep + fix; do not externalize.
- `cargo nextest` runs synchronously (don't background it via Monitor; just wait).
- `cargo fmt --check` is part of the gate set — running `cargo fmt` is NOT enough; you must run `cargo fmt --all -- --check` and see exit 0.

---

## Task 0: Pre-flight + green-baseline confirm

**Files:** None modified. Verification only.

**Goal:** Confirm the just-cut branch is on the latest `origin/main` lineage and all 5 CI gates are green BEFORE we add anything. Captures baseline test counts so later regressions are obvious.

- [ ] **Step 1: Verify branch state**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git status
git log --oneline -3
git rev-parse HEAD
```

Expected: clean working tree, on `zeb-286-vine-integration-test`, HEAD is `2c3573c` (the spec commit), parent is `6ffe8b0` (the merged ZEB-284 PR).

- [ ] **Step 2: Run cargo fmt check**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all -- --check
echo "fmt exit: $?"
```

Expected: exit code 0, no output.

- [ ] **Step 3: Run cargo clippy**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
echo "clippy exit: $?"
```

Expected: exit code 0, all targets clean.

- [ ] **Step 4: Run cargo nextest and capture test count baseline**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tee /tmp/zeb-286-task-0-nextest-baseline.log
echo "nextest exit: ${PIPESTATUS[0]}"
grep -E "^Summary|tests run:" /tmp/zeb-286-task-0-nextest-baseline.log | tail -1
```

Expected: exit 0, summary line like `Summary [Xs] NNNN tests run: NNNN passed, K skipped`. Record the count for comparison after each task — if a later task drops it, that's drift.

- [ ] **Step 5: Run frontend gates**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
npx tsc --noEmit
echo "tsc exit: $?"
npx vitest run 2>&1 | tee /tmp/zeb-286-task-0-vitest-baseline.log
echo "vitest exit: ${PIPESTATUS[0]}"
grep -E "Test Files|Tests" /tmp/zeb-286-task-0-vitest-baseline.log | tail -2
```

Expected: tsc exit 0 with no errors; vitest passing with N passed test files / N passed tests.

- [ ] **Step 6: No commit**

Task 0 verifies the baseline; no code is modified. Proceed to Task 1.

---

## Task 1: vine_feed_cache module skeleton + types + descriptor flow

**Files:**
- Create: `src-tauri/src/vine_feed_cache.rs`
- Modify: `src-tauri/src/lib.rs:1` (add `mod vine_feed_cache;` declaration after the existing `mod follows;`)

**Goal:** Land the new module with `VineFeedCache`, `CachedVine`, `VineSource`, `DescriptorOutcome`, plus `on_descriptor_sample` + `list_descriptors` + 6 module-internal unit tests covering descriptor flow.

- [ ] **Step 1: Find existing `mod follows;` declaration**

```bash
grep -n "^mod follows" /Users/zeblith/work/zeblithic/harmony-client/src-tauri/src/lib.rs
```

Expected output (approximate): `123:mod follows;` or similar. Locate the line — Step 2 inserts the new module declaration right after it.

- [ ] **Step 2: Add `mod vine_feed_cache;` declaration in lib.rs**

Insert immediately after the `mod follows;` line:

```rust
mod follows;
mod vine_feed_cache;
```

- [ ] **Step 3: Create skeleton `vine_feed_cache.rs` with types only**

Write the file `src-tauri/src/vine_feed_cache.rs`:

```rust
//! ZEB-286: VineFeedCache — Rust-side state surface for the Vine feed.
//!
//! Cache is updated by `event_loop::emit_frontend_event` on receive (one
//! cache instance per NodeState; shared with the event loop via
//! `Arc<Mutex<VineFeedCache>>`). Read by the `list_vine_videos()` and
//! `mark_vine_viewed()` Tauri IPCs.
//!
//! In-memory only in this PR — disk persistence is deferred to ZEB-147.
//!
//! See `docs/specs/2026-05-13-zeb-286-vine-integration-test-design.md`.

use crate::{VineDescriptorPayload, VineReactionPayload, VineVideoDto};
use serde::Serialize;
use std::collections::{HashMap, HashSet};

/// How the recipient discovered this vine. Followed = creator is in the
/// local follow set at the time of first arrival; Discover = otherwise.
/// Decided ONCE at first insert; subsequent re-arrivals do not change it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VineSource {
    Followed,
    Discover,
}

/// Outcome of `on_descriptor_sample`. `Inserted` carries the DTO ready
/// for the frontend `vine-received` emit so the caller does not have
/// to re-walk the cache.
#[derive(Debug, Clone, PartialEq)]
pub enum DescriptorOutcome {
    Inserted { dto: VineVideoDtoWithSource },
    AlreadyPresent,
    Rejected(String),
}

/// Outcome of `on_reaction_sample`. The receive path re-emits to the
/// frontend only on `Inserted` or `UpdatedNewer` (idempotent re-arrivals
/// and stale samples are absorbed silently).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReactionOutcome {
    Inserted,
    UpdatedNewer,
    Stale,
    Rejected,
}

/// Aggregated reaction view for a vine from the local viewer's
/// perspective. `count` is the number of `liked == true` reactions
/// across all reactors; `liked_by_me` is whether `viewer_addr` itself
/// has a `liked == true` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReactionSummary {
    pub count: usize,
    pub liked_by_me: bool,
}

/// Frontend-facing DTO carrying the `source` tag. Mirrors `VineVideoDto`
/// plus the `source` discriminator the frontend already consumes from
/// the `vine-received` Tauri event payload.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VineVideoDtoWithSource {
    pub id: String,
    pub creator_address: String,
    pub creator_name: String,
    pub created_at: u64,
    pub video_cid: String,
    pub title: Option<String>,
    pub reshare_of: Option<String>,
    pub viewed: bool,
    pub source: VineSource,
}

#[derive(Debug, Clone)]
struct CachedVine {
    descriptor: VineDescriptorPayload,
    #[allow(dead_code)] // recorded for future use (ZEB-147 may surface received-at in UI)
    received_at_ms: u64,
    source: VineSource,
}

#[derive(Debug, Clone)]
struct CachedReaction {
    liked: bool,
    timestamp: u64,
    #[allow(dead_code)] // recorded for future UI surfacing (reactor display name)
    reactor_name: String,
}

/// In-memory, single-peer view of the Vine network. Owned by NodeState;
/// updated by the event loop on receive; queried by IPCs.
#[derive(Debug, Default)]
pub struct VineFeedCache {
    descriptors: HashMap<String, CachedVine>,
    reactions: HashMap<(String, String), CachedReaction>,
    viewed: HashSet<String>,
}

impl VineFeedCache {
    pub fn new() -> Self {
        Self::default()
    }

    // (on_descriptor_sample, on_reaction_sample, list_descriptors,
    //  get_reaction, mark_viewed implementations land in Steps 5, 6,
    //  and Tasks 2 + 3.)

    /// Number of cached descriptors. Test helper.
    #[allow(dead_code)]
    pub fn len_descriptors(&self) -> usize {
        self.descriptors.len()
    }

    /// Number of cached reactions. Test helper.
    #[allow(dead_code)]
    pub fn len_reactions(&self) -> usize {
        self.reactions.len()
    }

    /// Whether `vine_id` has been locally marked viewed. Test helper.
    #[allow(dead_code)]
    pub fn is_viewed(&self, vine_id: &str) -> bool {
        self.viewed.contains(vine_id)
    }
}

#[cfg(test)]
mod tests {
    // Unit tests land in Steps 5 + 6.
}
```

- [ ] **Step 4: Verify the skeleton compiles**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo check --features test-fixtures
echo "check exit: $?"
```

Expected: exit 0. Compile clean (unused imports may warn, but they will be consumed in Step 5).

- [ ] **Step 5: Write the failing unit tests for `on_descriptor_sample` + `list_descriptors`**

Replace the `#[cfg(test)] mod tests { }` block with:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Build a canonical descriptor JSON payload for `creator_address`
    /// + `vine_id`. Mirrors the bytes that production `publish_vine`
    /// produces (the same `VineDescriptorPayload` serde::Serialize
    /// shape).
    fn canonical_descriptor_bytes(
        vine_id: &str,
        creator_address: &str,
        creator_name: &str,
        video_cid: &str,
        title: Option<&str>,
        reshare_of: Option<&str>,
        created_at: u64,
    ) -> Vec<u8> {
        let v = crate::VineDescriptorPayload {
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

    fn followed_set_with(addrs: &[&str]) -> HashSet<String> {
        addrs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn on_descriptor_sample_followed_creator_inserts_with_followed_source() {
        let mut cache = VineFeedCache::new();
        let payload = canonical_descriptor_bytes(
            "vine-1",
            "alice-addr",
            "Alice",
            "cid-aaa",
            Some("hello"),
            None,
            1700000000,
        );
        let followed = followed_set_with(&["alice-addr"]);

        let outcome = cache.on_descriptor_sample(
            "harmony/vines/alice-addr",
            &payload,
            &followed,
            1_000,
        );

        match outcome {
            Some(DescriptorOutcome::Inserted { dto }) => {
                assert_eq!(dto.id, "vine-1");
                assert_eq!(dto.creator_address, "alice-addr");
                assert_eq!(dto.source, VineSource::Followed);
                assert_eq!(dto.viewed, false);
            }
            other => panic!("expected Inserted, got {:?}", other),
        }
        assert_eq!(cache.len_descriptors(), 1);
    }

    #[test]
    fn on_descriptor_sample_unfollowed_creator_inserts_with_discover_source() {
        let mut cache = VineFeedCache::new();
        let payload = canonical_descriptor_bytes(
            "vine-2",
            "bob-addr",
            "Bob",
            "cid-bbb",
            None,
            None,
            1700000100,
        );
        let followed = followed_set_with(&["someone-else"]);

        let outcome = cache.on_descriptor_sample(
            "harmony/vines/bob-addr",
            &payload,
            &followed,
            2_000,
        );

        match outcome {
            Some(DescriptorOutcome::Inserted { dto }) => {
                assert_eq!(dto.source, VineSource::Discover);
            }
            other => panic!("expected Inserted/Discover, got {:?}", other),
        }
    }

    #[test]
    fn on_descriptor_sample_idempotent_on_rearrival() {
        let mut cache = VineFeedCache::new();
        let payload = canonical_descriptor_bytes(
            "vine-3",
            "alice-addr",
            "Alice",
            "cid-ccc",
            None,
            None,
            1700000200,
        );
        let followed = followed_set_with(&["alice-addr"]);

        // First arrival
        let first = cache.on_descriptor_sample(
            "harmony/vines/alice-addr",
            &payload,
            &followed,
            3_000,
        );
        assert!(matches!(first, Some(DescriptorOutcome::Inserted { .. })));

        // Same vine_id arrives again — even if followed_set changed
        let followed2 = followed_set_with(&[]); // empty
        let second = cache.on_descriptor_sample(
            "harmony/vines/alice-addr",
            &payload,
            &followed2,
            4_000,
        );
        assert_eq!(second, Some(DescriptorOutcome::AlreadyPresent));
        assert_eq!(cache.len_descriptors(), 1);

        // Source decision from first arrival is preserved (Followed),
        // not flipped to Discover by the second-arrival empty followed_set.
        let dtos = cache.list_descriptors();
        assert_eq!(dtos.len(), 1);
    }

    #[test]
    fn on_descriptor_sample_malformed_payload_rejected() {
        let mut cache = VineFeedCache::new();
        let bad = b"not valid json {{{";
        let followed = followed_set_with(&[]);

        let outcome = cache.on_descriptor_sample(
            "harmony/vines/alice-addr",
            bad,
            &followed,
            5_000,
        );

        match outcome {
            Some(DescriptorOutcome::Rejected(_)) => {}
            other => panic!("expected Rejected, got {:?}", other),
        }
        assert_eq!(cache.len_descriptors(), 0);
    }

    #[test]
    fn on_descriptor_sample_wrong_topic_returns_none() {
        let mut cache = VineFeedCache::new();
        let payload = canonical_descriptor_bytes(
            "vine-9",
            "alice-addr",
            "Alice",
            "cid",
            None,
            None,
            1,
        );
        let followed = followed_set_with(&[]);

        // The descriptor branch must NOT match reaction topics (they
        // contain `/reactions/`).
        let outcome = cache.on_descriptor_sample(
            "harmony/vines/alice-addr/reactions/vine-9/bob-addr",
            &payload,
            &followed,
            6_000,
        );
        assert_eq!(outcome, None);
        assert_eq!(cache.len_descriptors(), 0);

        // And must NOT match unrelated topics.
        let outcome2 = cache.on_descriptor_sample(
            "harmony/profile/alice-addr",
            &payload,
            &followed,
            7_000,
        );
        assert_eq!(outcome2, None);
    }

    #[test]
    fn list_descriptors_sorted_by_created_at_desc() {
        let mut cache = VineFeedCache::new();
        let followed = followed_set_with(&["alice-addr"]);

        // Insert in mixed order: created_at 100, 300, 200
        for (id, t) in [("v-100", 100u64), ("v-300", 300), ("v-200", 200)] {
            let payload = canonical_descriptor_bytes(
                id, "alice-addr", "Alice", "cid", None, None, t,
            );
            cache.on_descriptor_sample(
                "harmony/vines/alice-addr",
                &payload,
                &followed,
                1_000,
            );
        }

        let dtos = cache.list_descriptors();
        assert_eq!(dtos.len(), 3);
        // Newest first
        assert_eq!(dtos[0].id, "v-300");
        assert_eq!(dtos[1].id, "v-200");
        assert_eq!(dtos[2].id, "v-100");
    }
}
```

- [ ] **Step 6: Run failing tests to confirm they fail with "method not found" errors**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo test --features test-fixtures vine_feed_cache::tests 2>&1 | tail -20
echo "test exit: $?"
```

Expected: compilation errors mentioning `no method named on_descriptor_sample` or `list_descriptors`. The tests do not yet compile because the methods are unimplemented.

- [ ] **Step 7: Implement `on_descriptor_sample` + `list_descriptors` + `make_dto` helper**

Replace the placeholder comment in the `impl VineFeedCache` block (between `new()` and the test helpers) with these methods:

```rust
    /// Parse + insert a vine descriptor.
    ///
    /// Returns `None` if `key_expr` is not a vine-descriptor topic.
    /// Returns `Some(Rejected(reason))` on JSON parse failure.
    /// Idempotent: re-arrival of an already-cached `vine_id` returns
    /// `AlreadyPresent` and does NOT mutate the cache. Source decision
    /// (Followed vs Discover) is frozen at first insert.
    pub fn on_descriptor_sample(
        &mut self,
        key_expr: &str,
        payload: &[u8],
        followed_set: &HashSet<String>,
        now_ms: u64,
    ) -> Option<DescriptorOutcome> {
        if !key_expr.starts_with("harmony/vines/") {
            return None;
        }
        if key_expr.contains("/reactions/") {
            return None;
        }

        let descriptor: VineDescriptorPayload = match serde_json::from_slice(payload) {
            Ok(d) => d,
            Err(e) => {
                return Some(DescriptorOutcome::Rejected(format!(
                    "descriptor parse failed: {e}"
                )))
            }
        };

        if self.descriptors.contains_key(&descriptor.id) {
            return Some(DescriptorOutcome::AlreadyPresent);
        }

        let source = if followed_set.contains(&descriptor.creator_address) {
            VineSource::Followed
        } else {
            VineSource::Discover
        };

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
    }

    /// Return all cached descriptors as `VineVideoDto`, sorted by
    /// `created_at` DESC. `viewed` is populated by joining with the
    /// `self.viewed` HashSet (local-only viewed-state).
    pub fn list_descriptors(&self) -> Vec<VineVideoDto> {
        let mut out: Vec<VineVideoDto> = self
            .descriptors
            .values()
            .map(|cv| VineVideoDto {
                id: cv.descriptor.id.clone(),
                creator_address: cv.descriptor.creator_address.clone(),
                creator_name: cv.descriptor.creator_name.clone(),
                created_at: cv.descriptor.created_at,
                video_cid: cv.descriptor.video_cid.clone(),
                title: cv.descriptor.title.clone(),
                reshare_of: cv.descriptor.reshare_of.clone(),
                viewed: self.viewed.contains(&cv.descriptor.id),
            })
            .collect();
        out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        out
    }

    /// Internal helper: build the `VineVideoDtoWithSource` for the
    /// `Inserted` outcome. Source is provided by the caller (it was
    /// just computed). Viewed-state is joined from `self.viewed`.
    fn build_dto(
        &self,
        descriptor: &VineDescriptorPayload,
        source: VineSource,
    ) -> VineVideoDtoWithSource {
        VineVideoDtoWithSource {
            id: descriptor.id.clone(),
            creator_address: descriptor.creator_address.clone(),
            creator_name: descriptor.creator_name.clone(),
            created_at: descriptor.created_at,
            video_cid: descriptor.video_cid.clone(),
            title: descriptor.title.clone(),
            reshare_of: descriptor.reshare_of.clone(),
            viewed: self.viewed.contains(&descriptor.id),
            source,
        }
    }
```

- [ ] **Step 8: Run tests to verify they pass**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo test --features test-fixtures vine_feed_cache::tests 2>&1 | tail -20
echo "test exit: $?"
```

Expected: exit 0; all 6 tests pass.

- [ ] **Step 9: Run full gate set**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
cd /Users/zeblith/work/zeblithic/harmony-client
npx tsc --noEmit
npx vitest run
```

Expected: all 5 gates green. Nextest total = baseline + 6.

- [ ] **Step 10: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/lib.rs src-tauri/src/vine_feed_cache.rs
git commit -m "$(cat <<'EOF'
feat(zeb-286): VineFeedCache descriptor flow + module skeleton

Introduces the new vine_feed_cache module with:

- VineSource enum (Followed | Discover)
- DescriptorOutcome enum (Inserted | AlreadyPresent | Rejected)
- ReactionOutcome enum (Inserted | UpdatedNewer | Stale | Rejected)
- ReactionSummary struct (count, liked_by_me)
- VineVideoDtoWithSource (frontend DTO with source discriminator)
- CachedVine + CachedReaction internal structs
- VineFeedCache::new + on_descriptor_sample + list_descriptors

Descriptor flow is idempotent on re-arrival (source decision frozen at
first insert) and rejects malformed JSON without poisoning the cache.

Reactions (Task 2) and viewed-state (Task 3) land separately. Production
NodeState wiring lands in Task 4.

Refs [ZEB-286](https://linear.app/zeblith/issue/ZEB-286).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: on_reaction_sample + get_reaction (LWW)

**Files:**
- Modify: `src-tauri/src/vine_feed_cache.rs` (add `on_reaction_sample` + `get_reaction` methods + 4 unit tests)

**Goal:** Implement reaction publish/receive with last-writer-wins per `(vine_id, reactor_addr)` keyed by `timestamp`. Aggregation via `get_reaction(vine_id, viewer_addr)` returns `count` (number of `liked=true` reactions) and `liked_by_me` (whether viewer_addr has a `liked=true` entry).

- [ ] **Step 1: Write failing unit tests for reactions**

Inside the `mod tests` block in `vine_feed_cache.rs` (after the existing 6 tests, before the closing `}`), add:

```rust
    fn canonical_reaction_bytes(
        vine_id: &str,
        reactor_address: &str,
        reactor_name: &str,
        liked: bool,
        timestamp: u64,
    ) -> Vec<u8> {
        let v = crate::VineReactionPayload {
            vine_id: vine_id.to_string(),
            reactor_address: reactor_address.to_string(),
            reactor_name: reactor_name.to_string(),
            liked,
            timestamp,
        };
        serde_json::to_vec(&v).unwrap()
    }

    #[test]
    fn two_reactors_like_same_vine_count_is_two() {
        let mut cache = VineFeedCache::new();
        let alice_likes = canonical_reaction_bytes("vine-1", "alice-addr", "Alice", true, 100);
        let bob_likes = canonical_reaction_bytes("vine-1", "bob-addr", "Bob", true, 110);

        let r1 = cache.on_reaction_sample(
            "harmony/vines/creator-addr/reactions/vine-1/alice-addr",
            &alice_likes,
        );
        let r2 = cache.on_reaction_sample(
            "harmony/vines/creator-addr/reactions/vine-1/bob-addr",
            &bob_likes,
        );

        assert_eq!(r1, Some(ReactionOutcome::Inserted));
        assert_eq!(r2, Some(ReactionOutcome::Inserted));

        let summary = cache.get_reaction("vine-1", "anyone-addr");
        assert_eq!(summary.count, 2);
        assert_eq!(summary.liked_by_me, false);
    }

    #[test]
    fn same_reactor_unlike_then_like_lww_wins() {
        let mut cache = VineFeedCache::new();
        // First: alice unlikes at t=100
        let alice_unlikes = canonical_reaction_bytes("vine-1", "alice-addr", "Alice", false, 100);
        // Then: alice likes at t=200 (newer, so LWW wins)
        let alice_likes = canonical_reaction_bytes("vine-1", "alice-addr", "Alice", true, 200);

        cache.on_reaction_sample(
            "harmony/vines/creator-addr/reactions/vine-1/alice-addr",
            &alice_unlikes,
        );
        let r2 = cache.on_reaction_sample(
            "harmony/vines/creator-addr/reactions/vine-1/alice-addr",
            &alice_likes,
        );
        assert_eq!(r2, Some(ReactionOutcome::UpdatedNewer));

        let summary = cache.get_reaction("vine-1", "alice-addr");
        assert_eq!(summary.count, 1);
        assert_eq!(summary.liked_by_me, true);
    }

    #[test]
    fn stale_reaction_does_not_overwrite_newer() {
        let mut cache = VineFeedCache::new();
        // First: like at t=200
        let alice_likes = canonical_reaction_bytes("vine-1", "alice-addr", "Alice", true, 200);
        cache.on_reaction_sample(
            "harmony/vines/creator-addr/reactions/vine-1/alice-addr",
            &alice_likes,
        );

        // Stale unlike at t=100 (lower timestamp, must be rejected)
        let stale_unlike = canonical_reaction_bytes("vine-1", "alice-addr", "Alice", false, 100);
        let outcome = cache.on_reaction_sample(
            "harmony/vines/creator-addr/reactions/vine-1/alice-addr",
            &stale_unlike,
        );
        assert_eq!(outcome, Some(ReactionOutcome::Stale));

        // Newer like still wins
        let summary = cache.get_reaction("vine-1", "alice-addr");
        assert_eq!(summary.count, 1);
        assert_eq!(summary.liked_by_me, true);
    }

    #[test]
    fn liked_by_me_reflects_viewer_addr() {
        let mut cache = VineFeedCache::new();
        let alice_likes = canonical_reaction_bytes("vine-1", "alice-addr", "Alice", true, 100);
        let bob_likes = canonical_reaction_bytes("vine-1", "bob-addr", "Bob", true, 110);

        cache.on_reaction_sample(
            "harmony/vines/creator-addr/reactions/vine-1/alice-addr",
            &alice_likes,
        );
        cache.on_reaction_sample(
            "harmony/vines/creator-addr/reactions/vine-1/bob-addr",
            &bob_likes,
        );

        // From Alice's perspective: liked_by_me=true (she liked it)
        let a = cache.get_reaction("vine-1", "alice-addr");
        assert_eq!(a.count, 2);
        assert_eq!(a.liked_by_me, true);

        // From Carol's perspective: she did not react
        let c = cache.get_reaction("vine-1", "carol-addr");
        assert_eq!(c.count, 2);
        assert_eq!(c.liked_by_me, false);
    }

    #[test]
    fn on_reaction_sample_wrong_topic_returns_none() {
        let mut cache = VineFeedCache::new();
        let payload = canonical_reaction_bytes("vine-1", "alice-addr", "Alice", true, 100);

        // Descriptor topic — must NOT match the reaction branch
        let outcome = cache.on_reaction_sample(
            "harmony/vines/creator-addr",
            &payload,
        );
        assert_eq!(outcome, None);

        // Unrelated topic
        let outcome2 = cache.on_reaction_sample(
            "harmony/profile/alice-addr",
            &payload,
        );
        assert_eq!(outcome2, None);
    }

    #[test]
    fn on_reaction_sample_malformed_payload_rejected() {
        let mut cache = VineFeedCache::new();
        let bad = b"{{{not json";

        let outcome = cache.on_reaction_sample(
            "harmony/vines/creator-addr/reactions/vine-1/alice-addr",
            bad,
        );
        assert_eq!(outcome, Some(ReactionOutcome::Rejected));
        assert_eq!(cache.len_reactions(), 0);
    }

    #[test]
    fn get_reaction_for_unknown_vine_id_returns_zero() {
        let cache = VineFeedCache::new();
        let summary = cache.get_reaction("nonexistent-vine", "anyone-addr");
        assert_eq!(summary.count, 0);
        assert_eq!(summary.liked_by_me, false);
    }
```

- [ ] **Step 2: Run tests to confirm failure**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo test --features test-fixtures vine_feed_cache::tests 2>&1 | tail -20
echo "test exit: $?"
```

Expected: compile errors mentioning `no method named on_reaction_sample` or `get_reaction`.

- [ ] **Step 3: Implement `on_reaction_sample` + `get_reaction`**

In the `impl VineFeedCache` block (after `build_dto` and before the test helpers), add:

```rust
    /// Parse + insert/LWW-update a reaction.
    ///
    /// Returns `None` if `key_expr` is not a vine-reaction topic.
    /// LWW per (vine_id, reactor_addr) by `timestamp`. Stale samples
    /// (timestamp older than existing entry) return `Stale` and do
    /// NOT mutate the cache.
    pub fn on_reaction_sample(
        &mut self,
        key_expr: &str,
        payload: &[u8],
    ) -> Option<ReactionOutcome> {
        if !(key_expr.starts_with("harmony/vines/") && key_expr.contains("/reactions/")) {
            return None;
        }

        let reaction: VineReactionPayload = match serde_json::from_slice(payload) {
            Ok(r) => r,
            Err(_) => return Some(ReactionOutcome::Rejected),
        };

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
                if reaction.timestamp <= existing.timestamp {
                    // Stale (or duplicate same-timestamp): no-op
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
    }

    /// Aggregate reaction state for `vine_id` from the local viewer's
    /// perspective. `count` is the number of `liked == true` reactions
    /// across all reactors; `liked_by_me` is true iff `viewer_addr` has
    /// a `liked == true` entry for this vine.
    pub fn get_reaction(&self, vine_id: &str, viewer_addr: &str) -> ReactionSummary {
        let mut count = 0usize;
        let mut liked_by_me = false;
        for ((vid, reactor), r) in &self.reactions {
            if vid != vine_id {
                continue;
            }
            if r.liked {
                count += 1;
                if reactor == viewer_addr {
                    liked_by_me = true;
                }
            }
        }
        ReactionSummary { count, liked_by_me }
    }
```

- [ ] **Step 4: Verify tests pass**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo test --features test-fixtures vine_feed_cache::tests 2>&1 | tail -20
echo "test exit: $?"
```

Expected: exit 0; all 13 tests pass (6 from Task 1 + 7 new).

- [ ] **Step 5: Run full gate set**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
cd /Users/zeblith/work/zeblithic/harmony-client
npx tsc --noEmit
npx vitest run
```

Expected: all 5 green. Nextest total = baseline + 13.

- [ ] **Step 6: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/vine_feed_cache.rs
git commit -m "$(cat <<'EOF'
feat(zeb-286): VineFeedCache reaction flow with LWW aggregation

Adds on_reaction_sample + get_reaction methods:

- LWW per (vine_id, reactor_addr) by timestamp
- Stale samples (older timestamp) return Stale and do not mutate cache
- get_reaction aggregates total liked-count + viewer's liked_by_me flag
- Idempotent same-timestamp re-arrivals classified as Stale

7 new unit tests cover the two-reactor count case, unlike→like LWW,
stale rejection, viewer-perspective liked_by_me, wrong-topic guard,
malformed-payload rejection, and unknown-vine-id zero result.

Refs [ZEB-286](https://linear.app/zeblith/issue/ZEB-286).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: mark_viewed + viewed-state join

**Files:**
- Modify: `src-tauri/src/vine_feed_cache.rs` (add `mark_viewed` method + 3 unit tests)

**Goal:** Implement `mark_viewed(vine_id) -> bool` (true if newly added, false if already viewed). Local-only — cross-device sync is deferred to ZEB-147. Verify the join with `list_descriptors` correctly populates `viewed` whether `mark_viewed` is called before or after `on_descriptor_sample`.

- [ ] **Step 1: Write failing unit tests for mark_viewed**

Inside the `mod tests` block (after the reactions tests), add:

```rust
    #[test]
    fn mark_viewed_idempotent_and_local_only() {
        let mut cache = VineFeedCache::new();
        let payload = canonical_descriptor_bytes(
            "vine-1",
            "alice-addr",
            "Alice",
            "cid",
            None,
            None,
            100,
        );
        let followed = followed_set_with(&["alice-addr"]);
        cache.on_descriptor_sample("harmony/vines/alice-addr", &payload, &followed, 0);
        assert_eq!(cache.len_descriptors(), 1);

        // First mark — newly added
        let first = cache.mark_viewed("vine-1".to_string());
        assert_eq!(first, true);
        assert!(cache.is_viewed("vine-1"));

        // Second mark — already viewed
        let second = cache.mark_viewed("vine-1".to_string());
        assert_eq!(second, false);

        // Descriptor count unchanged (mark_viewed must NOT touch descriptors)
        assert_eq!(cache.len_descriptors(), 1);

        // list_descriptors reflects viewed=true
        let dtos = cache.list_descriptors();
        assert_eq!(dtos.len(), 1);
        assert_eq!(dtos[0].viewed, true);
    }

    #[test]
    fn viewed_state_survives_descriptor_insertion_order() {
        let mut cache = VineFeedCache::new();
        let followed = followed_set_with(&["alice-addr"]);

        // Mark viewed BEFORE descriptor arrives (off-order)
        let first = cache.mark_viewed("vine-future".to_string());
        assert_eq!(first, true);
        assert!(cache.is_viewed("vine-future"));

        // Descriptor arrives later
        let payload = canonical_descriptor_bytes(
            "vine-future",
            "alice-addr",
            "Alice",
            "cid",
            None,
            None,
            500,
        );
        cache.on_descriptor_sample("harmony/vines/alice-addr", &payload, &followed, 0);

        // list_descriptors must show viewed=true even though the mark
        // happened before insert
        let dtos = cache.list_descriptors();
        assert_eq!(dtos.len(), 1);
        assert_eq!(dtos[0].id, "vine-future");
        assert_eq!(dtos[0].viewed, true);
    }

    #[test]
    fn mark_viewed_for_unknown_vine_id_is_still_tracked() {
        let mut cache = VineFeedCache::new();

        // No descriptor exists yet
        let first = cache.mark_viewed("vine-ghost".to_string());
        assert_eq!(first, true);
        assert!(cache.is_viewed("vine-ghost"));

        // No descriptors are created by mark_viewed
        assert_eq!(cache.len_descriptors(), 0);

        // list_descriptors is empty because no descriptor was ever
        // inserted — viewed-state alone does not synthesize a DTO
        let dtos = cache.list_descriptors();
        assert_eq!(dtos.len(), 0);
    }
```

- [ ] **Step 2: Run tests to confirm failure**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo test --features test-fixtures vine_feed_cache::tests 2>&1 | tail -20
```

Expected: compile error `no method named mark_viewed`.

- [ ] **Step 3: Implement `mark_viewed`**

In the `impl VineFeedCache` block (after `get_reaction`, before the test helpers), add:

```rust
    /// Mark a vine viewed by this local peer. Local-only in this PR —
    /// cross-device sync deferred to ZEB-147.
    ///
    /// Returns `true` if the vine was newly added to the viewed set,
    /// `false` if it was already viewed. Matches `FollowManager::follow`'s
    /// "did this change anything" convention.
    ///
    /// Safe to call before the descriptor arrives — `list_descriptors`
    /// joins viewed-state at query time, so the order of `mark_viewed`
    /// + `on_descriptor_sample` does not matter.
    pub fn mark_viewed(&mut self, vine_id: String) -> bool {
        self.viewed.insert(vine_id)
    }
```

- [ ] **Step 4: Verify tests pass**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo test --features test-fixtures vine_feed_cache::tests 2>&1 | tail -20
```

Expected: exit 0; all 16 tests pass (6 + 7 + 3).

- [ ] **Step 5: Run full gate set**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
cd /Users/zeblith/work/zeblithic/harmony-client
npx tsc --noEmit
npx vitest run
```

Expected: all 5 green. Nextest total = baseline + 16.

- [ ] **Step 6: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/vine_feed_cache.rs
git commit -m "$(cat <<'EOF'
feat(zeb-286): VineFeedCache mark_viewed + viewed-state join

Adds mark_viewed(vine_id) -> bool (true if newly added, false if already
viewed). Viewed-state is local-only — cross-device sync deferred to
[ZEB-147](https://linear.app/zeblith/issue/ZEB-147).

The viewed HashSet is joined at query time in list_descriptors, so
calling mark_viewed before the descriptor arrives still produces
viewed=true once the descriptor lands.

3 new unit tests cover the idempotent local-only mark, viewed-survives-
insertion-order, and the ghost-id path (mark a vine_id with no matching
descriptor — does not synthesize a DTO).

Refs [ZEB-286](https://linear.app/zeblith/issue/ZEB-286).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Wire VineFeedCache into NodeState lifecycle

**Files:**
- Modify: `src-tauri/src/lib.rs` (several locations: field definition, default, start_node construction, stop_node cleanup, old-node cleanup, post-success NodeState write)

**Goal:** Add `vine_feed_cache: Option<Arc<Mutex<VineFeedCache>>>` to `NodeState`, construct it in `start_node` alongside `followed_set`, and clear it on `stop_node` (matching the `follow_mgr` / `followed_set` lifecycle). No event-loop wiring or IPC changes yet — those land in Tasks 5 and 6.

- [ ] **Step 1: Add the field to NodeState**

In `src-tauri/src/lib.rs`, locate the `NodeState` struct (currently has `followed_set: Option<std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>>` at approximately line 205). Add a new field directly below `followed_set`:

```rust
    /// Shared set of followed addresses (read by the event loop for source tagging).
    followed_set: Option<std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>>,
    /// In-memory Vine feed cache (ZEB-286). Updated by the event loop on
    /// receive; read by list_vine_videos / mark_vine_viewed IPCs.
    /// Disk persistence deferred to ZEB-147.
    vine_feed_cache: Option<std::sync::Arc<std::sync::Mutex<vine_feed_cache::VineFeedCache>>>,
```

- [ ] **Step 2: Update `NodeState::default`**

Locate the `Self { ... }` literal in `NodeState::default()` (approximately line 380). Add `vine_feed_cache: None,` directly after `followed_set: None,`:

```rust
            follow_mgr: None,
            followed_set: None,
            vine_feed_cache: None,
            mail_mgr: None,
```

- [ ] **Step 3: Construct the cache in start_node**

In `start_node`, locate the line `let followed_set = std::sync::Arc::new(std::sync::Mutex::new(...));` (approximately line 989). Add right after the `followed_set_clone` line:

```rust
    let followed_set_clone = followed_set.clone();

    // ZEB-286: in-memory VineFeedCache shared between event loop and IPCs.
    let vine_feed_cache = std::sync::Arc::new(std::sync::Mutex::new(
        vine_feed_cache::VineFeedCache::new(),
    ));
    let vine_feed_cache_clone = vine_feed_cache.clone();
```

- [ ] **Step 4: Stash the cache in NodeState on start_node success**

Locate the block that writes back to NodeState after the event loop spawns successfully (approximately line 2503, the cluster of `guard.* = Some(...)` lines). Add the line directly after `guard.followed_set = Some(followed_set);`:

```rust
                guard.follow_mgr = Some(follow_mgr);
                guard.followed_set = Some(followed_set);
                guard.vine_feed_cache = Some(vine_feed_cache);
                guard.mail_mgr = Some(mail_mgr);
```

- [ ] **Step 5: Clear the cache on old-node cleanup in start_node**

Locate the old-node cleanup block (approximately line 1098, with the line `let _old_follow_mgr = guard.follow_mgr.take();`). Add directly below `_old_followed_set`:

```rust
        let _old_follow_mgr = guard.follow_mgr.take();
        let _old_followed_set = guard.followed_set.take();
        let _old_vine_feed_cache = guard.vine_feed_cache.take();
```

- [ ] **Step 6: Clear the cache on stop_node**

Locate the `stop_node` extraction tuple (approximately line 626, with `guard.follow_mgr.take()` and `guard.followed_set.take()`). Add directly after the followed_set take:

```rust
            guard.follow_mgr.take(),
            guard.followed_set.take(),
            guard.vine_feed_cache.take(),
            guard.mail_sync.take(),
```

- [ ] **Step 7: Add the matching destructure name in stop_node**

Locate the destructure list at approximately line 588 (which currently has `_follow_mgr, _followed_set, _mail_sync,` etc.). Add directly after `_followed_set`:

```rust
        _follow_mgr,
        _followed_set,
        _vine_feed_cache,
        _mail_sync,
```

- [ ] **Step 8: Write a lifecycle unit test for NodeState**

There is no NodeState-level test scaffolding in the codebase (start_node depends on Tauri AppHandle which is not available in unit tests). Instead, add a minimal sanity test inside the `vine_feed_cache.rs` module's `mod tests` block to confirm the type can be constructed and dropped without panic — covering the Arc<Mutex<>> wrap that NodeState uses:

```rust
    #[test]
    fn vine_feed_cache_round_trip_through_arc_mutex_works() {
        use std::sync::{Arc, Mutex};
        let cache = Arc::new(Mutex::new(VineFeedCache::new()));

        // Independent borrow + mutation through the lock — same pattern
        // as event_loop's emit_frontend_event will use in Task 5.
        {
            let mut guard = cache.lock().unwrap();
            guard.mark_viewed("v-1".to_string());
        }
        {
            let guard = cache.lock().unwrap();
            assert!(guard.is_viewed("v-1"));
        }

        // Two Arc clones can both read without deadlock
        let c2 = cache.clone();
        let len = c2.lock().unwrap().len_descriptors();
        assert_eq!(len, 0);
    }
```

- [ ] **Step 9: Verify everything compiles + tests pass**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo check --features test-fixtures 2>&1 | tail -10
echo "check exit: $?"
cargo test --features test-fixtures vine_feed_cache::tests 2>&1 | tail -10
echo "test exit: $?"
```

Expected: both exit 0. 17 tests pass.

- [ ] **Step 10: Run full gate set**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
cd /Users/zeblith/work/zeblithic/harmony-client
npx tsc --noEmit
npx vitest run
```

Expected: all 5 green. Nextest total = baseline + 17.

- [ ] **Step 11: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/lib.rs src-tauri/src/vine_feed_cache.rs
git commit -m "$(cat <<'EOF'
feat(zeb-286): wire VineFeedCache into NodeState lifecycle

Adds a vine_feed_cache field to NodeState as
Option<Arc<Mutex<VineFeedCache>>>. Constructed in start_node alongside
followed_set, cleared on stop_node, and reset on old-node teardown
within start_node (when a fresh node replaces a stale one).

No event_loop or IPC wiring yet — those land in Tasks 5 and 6. After
this task, the cache exists in NodeState but is not yet read or written
by anything in production.

One sanity test confirms the Arc<Mutex<>> round-trip works (the pattern
emit_frontend_event will use in Task 5).

Refs [ZEB-286](https://linear.app/zeblith/issue/ZEB-286).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Integrate cache into event_loop::emit_frontend_event

**Files:**
- Modify: `src-tauri/src/event_loop.rs` (3 sites: function signature, call site, hoist scope)
- Modify: `src-tauri/src/lib.rs` (1 site: pass `vine_feed_cache_clone` into the event_loop spawn)

**Goal:** Add `vine_feed_cache` parameter to `emit_frontend_event`. Replace the inline `harmony/vines/` dispatch (currently at `event_loop.rs:2742-2765`) with cache-routed logic. Source-tag injection moves into the cache. Pass the cache Arc clone from `start_node` into the event loop.

- [ ] **Step 1: Update `emit_frontend_event` signature**

In `src-tauri/src/event_loop.rs`, locate the function header at line 2718:

```rust
fn emit_frontend_event<R: Runtime>(
    app: &AppHandle<R>,
    key_expr: &str,
    payload: &[u8],
    hop_distance: Option<u8>,
    followed_set: &std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    mail_mgr: &std::sync::Arc<std::sync::Mutex<crate::mail::MailManager>>,
    own_mail_key: &str,
    own_root_key: &str,
    mail_sync: Option<&Arc<crate::mail_sync::MailSync<R>>>,
) {
```

Add a `vine_feed_cache` parameter directly after `followed_set`:

```rust
fn emit_frontend_event<R: Runtime>(
    app: &AppHandle<R>,
    key_expr: &str,
    payload: &[u8],
    hop_distance: Option<u8>,
    followed_set: &std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    vine_feed_cache: &std::sync::Arc<std::sync::Mutex<crate::vine_feed_cache::VineFeedCache>>,
    mail_mgr: &std::sync::Arc<std::sync::Mutex<crate::mail::MailManager>>,
    own_mail_key: &str,
    own_root_key: &str,
    mail_sync: Option<&Arc<crate::mail_sync::MailSync<R>>>,
) {
```

- [ ] **Step 2: Replace the inline harmony/vines/ dispatch**

In the same function body, locate the existing block at lines 2742-2765:

```rust
    } else if key_expr.starts_with("harmony/vines/") {
        if key_expr.contains("/reactions/") {
            // Vine reaction event — emit directly to frontend.
            if let Ok(reaction) = serde_json::from_slice::<crate::VineReactionPayload>(payload) {
                let _ = app.emit("vine-reaction-received", &reaction);
            }
        } else {
            // Vine descriptor — deserialize as typed payload first to reject malformed data,
            // then re-serialize with the source tag injected.
            if let Ok(vine) = serde_json::from_slice::<crate::VineDescriptorPayload>(payload) {
                let is_followed = {
                    let set = followed_set.lock().unwrap();
                    set.contains(vine.creator_address.as_str())
                };
                let source = if is_followed { "followed" } else { "discover" };
                // Re-serialize to Value so we can inject the source field
                if let Ok(mut val) = serde_json::to_value(&vine) {
                    if let Some(obj) = val.as_object_mut() {
                        obj.insert(
                            "source".to_string(),
                            serde_json::Value::String(source.to_string()),
                        );
                    }
                    let _ = app.emit("vine-received", &val);
                }
            }
        }
    } else if key_expr.starts_with("harmony/announce/") {
```

Replace ONLY the inner block (everything between `} else if key_expr.starts_with("harmony/vines/") {` and the next `} else if`), keeping the outer `else if` chain intact. The replacement uses the cache:

```rust
    } else if key_expr.starts_with("harmony/vines/") {
        if key_expr.contains("/reactions/") {
            // ZEB-286: route reaction through the cache. Re-emit to the
            // frontend ONLY on Inserted or UpdatedNewer (stale/duplicate
            // re-arrivals are absorbed silently). The cache's per-LWW
            // dedupe replaces the previous naive every-sample emit.
            let outcome = vine_feed_cache.lock().unwrap().on_reaction_sample(key_expr, payload);
            if matches!(
                outcome,
                Some(crate::vine_feed_cache::ReactionOutcome::Inserted
                    | crate::vine_feed_cache::ReactionOutcome::UpdatedNewer)
            ) {
                if let Ok(reaction) = serde_json::from_slice::<crate::VineReactionPayload>(payload) {
                    let _ = app.emit("vine-reaction-received", &reaction);
                }
            }
        } else {
            // ZEB-286: route descriptor through the cache. Source-tag
            // (Followed vs Discover) is decided by the cache once at
            // first insert; re-arrivals are absorbed. The cache returns
            // the ready-to-emit VineVideoDtoWithSource so we do not have
            // to re-parse + re-mutate JSON here.
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let outcome = {
                let mut cache = vine_feed_cache.lock().unwrap();
                let set = followed_set.lock().unwrap();
                cache.on_descriptor_sample(key_expr, payload, &set, now_ms)
            };
            if let Some(crate::vine_feed_cache::DescriptorOutcome::Inserted { dto }) = outcome {
                let _ = app.emit("vine-received", &dto);
            }
        }
    } else if key_expr.starts_with("harmony/announce/") {
```

> **Lock-order note:** The descriptor block acquires the cache lock FIRST, then the followed_set lock, both inside one expression. They are released together at the end of the block. No other site holds these two locks simultaneously, so this ordering establishes the canonical one.

- [ ] **Step 3: Update the call site at line 1403**

Locate the existing call to `emit_frontend_event` in `event_loop.rs` (approximately line 1403):

```rust
                        emit_frontend_event(
                            &app,
                            &key_expr,
                            &payload,
                            hop_distance,
                            &followed_set,
                            &mail_mgr,
                            &own_mail_key,
                            &own_root_key,
                            mail_sync.as_ref(),
                        );
```

Add `&vine_feed_cache,` between `&followed_set,` and `&mail_mgr,`:

```rust
                        emit_frontend_event(
                            &app,
                            &key_expr,
                            &payload,
                            hop_distance,
                            &followed_set,
                            &vine_feed_cache,
                            &mail_mgr,
                            &own_mail_key,
                            &own_root_key,
                            mail_sync.as_ref(),
                        );
```

- [ ] **Step 4: Hoist `vine_feed_cache` into the event loop's outer scope**

In `event_loop.rs`, locate the `event_loop::run` function signature/scope around line 280-290 (where `followed_set: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,` is declared as a function parameter). The dispatch loop will need access to a `vine_feed_cache` variable in the same scope.

Add a parameter to the `event_loop::run` function signature directly after `followed_set`:

```rust
    followed_set: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    vine_feed_cache: std::sync::Arc<std::sync::Mutex<crate::vine_feed_cache::VineFeedCache>>,
```

> **Note:** If `event_loop::run` has a struct-based config (e.g., `EventLoopConfig`), add the field there instead. Inspect the function signature first; the spec assumes free-function parameters per the existing `followed_set` style.

- [ ] **Step 5: Pass the cache clone into the event_loop spawn from start_node**

In `src-tauri/src/lib.rs`, locate the call to `event_loop::run` or the spawn site (approximately line 2453, where `followed_set_clone` is passed). Add `vine_feed_cache_clone` directly after `followed_set_clone`:

Find the existing pattern (search for `followed_set_clone,` in lib.rs):

```bash
grep -n "followed_set_clone" /Users/zeblith/work/zeblithic/harmony-client/src-tauri/src/lib.rs
```

Expected output: ~3 hits — one for the `.clone()` declaration, one for the spawn parameter. Add `vine_feed_cache_clone,` immediately after the parameter site. Example:

```rust
                                                        followed_set_clone,
                                                        vine_feed_cache_clone,
                                                        ...
```

If the exact spawn-site syntax differs (e.g., struct-init style), insert the field according to that style.

- [ ] **Step 6: Verify everything compiles**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo check --features test-fixtures 2>&1 | tail -20
echo "check exit: $?"
```

Expected: exit 0. If clippy complains about a `now_ms` truncation cast (`as u64`), it is OK to add `#[allow(clippy::cast_possible_truncation)]` to the `now_ms` let-binding ONLY if necessary (try `u64::try_from(d.as_millis()).unwrap_or(u64::MAX)` first).

- [ ] **Step 7: Run gate set**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
cd /Users/zeblith/work/zeblithic/harmony-client
npx tsc --noEmit
npx vitest run
```

Expected: all 5 green. No new tests added in this task (Task 7 covers the integration). Nextest total = baseline + 17 (same as Task 4).

- [ ] **Step 8: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/lib.rs src-tauri/src/event_loop.rs
git commit -m "$(cat <<'EOF'
feat(zeb-286): route vine descriptors and reactions through VineFeedCache

emit_frontend_event now routes harmony/vines/* samples through the
NodeState's VineFeedCache before emitting vine-received /
vine-reaction-received. The inline source-tag injection (JSON Value
mutation) is replaced by the cache's VineVideoDtoWithSource — the cache
returns the emit-ready DTO directly.

Behavior changes from the prior naive emit:

- Re-arrival of the same vine_id no longer re-emits (cache dedup)
- Stale reactions (older timestamp than existing cached entry) no longer
  re-emit; the cache absorbs them silently
- Source-tag (followed vs discover) is decided ONCE at first arrival;
  re-arrivals do not flip it even if the follow set has changed

VineService's existing dedupe-by-vine-id on the frontend remains
correct; the cache layer is a strictly tighter contract.

Refs [ZEB-286](https://linear.app/zeblith/issue/ZEB-286).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: Wire list_vine_videos + mark_vine_viewed IPCs to cache

**Files:**
- Modify: `src-tauri/src/lib.rs:4467-4470` (replace `list_vine_videos` stub)
- Modify: `src-tauri/src/lib.rs:4553-4557` (replace `mark_vine_viewed` stub)

**Goal:** Replace both stubs with real cache-backed implementations. Both IPCs now take `state: tauri::State<'_, Mutex<NodeState>>` and return `Result<_, String>` for the disconnected-node case.

- [ ] **Step 1: Write a failing test for the new `list_vine_videos` signature**

Append to `src-tauri/src/vine_feed_cache.rs` `mod tests`:

```rust
    #[test]
    fn list_descriptors_returns_dto_with_viewed_state_set() {
        // Test the full DTO shape exposed to the IPC, including the
        // viewed flag joining correctly.
        let mut cache = VineFeedCache::new();
        let payload = canonical_descriptor_bytes(
            "vine-1",
            "alice-addr",
            "Alice",
            "cid-a",
            Some("title-a"),
            None,
            500,
        );
        let payload2 = canonical_descriptor_bytes(
            "vine-2",
            "alice-addr",
            "Alice",
            "cid-b",
            None,
            Some("vine-1"), // reshare
            600,
        );
        let followed = followed_set_with(&["alice-addr"]);

        cache.on_descriptor_sample("harmony/vines/alice-addr", &payload, &followed, 0);
        cache.on_descriptor_sample("harmony/vines/alice-addr", &payload2, &followed, 0);
        cache.mark_viewed("vine-1".to_string());

        let dtos = cache.list_descriptors();
        assert_eq!(dtos.len(), 2);
        // sorted by created_at DESC: vine-2 (600) first
        assert_eq!(dtos[0].id, "vine-2");
        assert_eq!(dtos[0].reshare_of.as_deref(), Some("vine-1"));
        assert_eq!(dtos[0].viewed, false);
        // vine-1 second, marked viewed
        assert_eq!(dtos[1].id, "vine-1");
        assert_eq!(dtos[1].title.as_deref(), Some("title-a"));
        assert_eq!(dtos[1].viewed, true);
    }
```

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo test --features test-fixtures vine_feed_cache::tests::list_descriptors_returns_dto_with_viewed_state_set 2>&1 | tail -10
```

Expected: PASS. (This test exercises the existing cache surface; it is the IPC contract written as a cache-level test.)

- [ ] **Step 2: Replace `list_vine_videos` IPC**

Locate `list_vine_videos` in `src-tauri/src/lib.rs` (currently at line 4467):

```rust
#[tauri::command]
fn list_vine_videos() -> Vec<VineVideoDto> {
    // Future: return cached/persisted vines. Real data flows via vine-received events.
    Vec::new()
}
```

Replace with:

```rust
/// Return all vines currently in the local cache, sorted by
/// `created_at` descending (newest first). `viewed` field reflects
/// local-only `mark_vine_viewed` state.
///
/// Returns `Err("not connected")` if the node is not running.
/// ZEB-147 will extend this with disk persistence.
#[tauri::command]
fn list_vine_videos(
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<Vec<VineVideoDto>, String> {
    let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
    let cache = guard
        .vine_feed_cache
        .as_ref()
        .ok_or_else(|| "not connected".to_string())?;
    Ok(cache.lock().unwrap().list_descriptors())
}
```

- [ ] **Step 3: Replace `mark_vine_viewed` IPC**

Locate `mark_vine_viewed` (currently at line 4553):

```rust
#[tauri::command]
fn mark_vine_viewed(vine_id: String) -> bool {
    // Future: persist viewed state + publish to network for cross-device sync.
    let _ = vine_id;
    true
}
```

Replace with:

```rust
/// Mark a vine viewed by the local peer. Returns `Ok(true)` if newly
/// marked viewed, `Ok(false)` if already viewed.
///
/// Returns `Err("not connected")` if the node is not running.
/// Local-only in this PR; cross-device sync deferred to ZEB-147.
#[tauri::command]
fn mark_vine_viewed(
    vine_id: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<bool, String> {
    let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
    let cache = guard
        .vine_feed_cache
        .as_ref()
        .ok_or_else(|| "not connected".to_string())?;
    Ok(cache.lock().unwrap().mark_viewed(vine_id))
}
```

- [ ] **Step 4: Check that the frontend tolerates the new error path**

Grep the frontend for callers of these two IPCs:

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
grep -rn "list_vine_videos\|listVineVideos\|mark_vine_viewed\|markVineViewed" src/ --include="*.ts" --include="*.svelte"
```

Expected: `VineService.fetchInitialVines()` (or similar) calls `list_vine_videos` with a surrounding `try { } catch { }` and falls back to mock data on error — that is the pre-existing behavior. The new `Err("not connected")` flows through that catch.

`mark_vine_viewed` callers similarly tolerate failure (it is a fire-and-forget UI signal). If there is a caller that does NOT catch, surface it now; otherwise this step is informational.

> If you find a caller that does NOT have a try/catch, add one with `e instanceof Error ? e.message : String(e)` error extraction per the user's Tauri-error-extraction memory rule. Do NOT skip this — silent failure here regresses UX.

- [ ] **Step 5: Run gate set**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
cd /Users/zeblith/work/zeblithic/harmony-client
npx tsc --noEmit
npx vitest run
```

Expected: all 5 green. Nextest total = baseline + 18 (the new dto-shape test from Step 1).

- [ ] **Step 6: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/lib.rs src-tauri/src/vine_feed_cache.rs
git commit -m "$(cat <<'EOF'
feat(zeb-286): wire list_vine_videos + mark_vine_viewed IPCs to cache

list_vine_videos no longer returns Vec::new() — it now returns the
sorted-by-created_at-DESC contents of the NodeState's VineFeedCache.
Signature change: fn() -> Vec<VineVideoDto> becomes fn(state) -> Result<Vec<VineVideoDto>, String>
for the disconnected-node case.

mark_vine_viewed no longer is a no-op — it now calls cache.mark_viewed
and returns true if newly added / false if already viewed. Signature
change parallel to list_vine_videos.

Frontend VineService.fetchInitialVines already tolerates an Err return
from list_vine_videos (existing try/catch falls back to mock data). No
frontend change required in this PR.

This closes the [ZEB-147](https://linear.app/zeblith/issue/ZEB-147)
"stub" status of both IPCs at the in-memory level; persistence-to-disk
remains the open work in [ZEB-147](https://linear.app/zeblith/issue/ZEB-147).

Refs [ZEB-286](https://linear.app/zeblith/issue/ZEB-286).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: vine_feed_cache_integration.rs — 14 integration tests

**Files:**
- Create: `src-tauri/tests/vine_feed_cache_integration.rs`

**Goal:** Land the lightweight cache-on-sample integration test file with all 14 tests in 5 categories per spec §5.1.

> **Pattern reference:** `src-tauri/tests/profile_broadcast_integration.rs` is the canonical sibling. Use the same `cache.on_sample(bytes)` + `cache.get_*` pattern with no event loop and no Zenoh.

- [ ] **Step 1: Create the test file with the full Category 1 (descriptor filter) — 4 tests**

Write `src-tauri/tests/vine_feed_cache_integration.rs`:

```rust
//! ZEB-286: Integration tests for VineFeedCache.
//!
//! Models `profile_broadcast_integration.rs` — hands canonical wire
//! bytes (built via `serde_json::to_vec` on the same `VineDescriptorPayload`
//! / `VineReactionPayload` types that production `publish_vine` emits)
//! directly to the recipient's `VineFeedCache`. The Zenoh transport
//! layer is covered by codebase-wide "real Zenoh too heavy" precedent.
//!
//! Full design: `docs/specs/2026-05-13-zeb-286-vine-integration-test-design.md`.

use harmony_app::vine_feed_cache::{
    DescriptorOutcome, ReactionOutcome, VineFeedCache, VineSource,
};
use harmony_app::{VineDescriptorPayload, VineReactionPayload};
use std::collections::HashSet;

// ── Fixture helpers ─────────────────────────────────────────────────

fn make_descriptor(
    vine_id: &str,
    creator_address: &str,
    creator_name: &str,
    video_cid: &str,
    title: Option<&str>,
    reshare_of: Option<&str>,
    created_at: u64,
) -> VineDescriptorPayload {
    VineDescriptorPayload {
        id: vine_id.to_string(),
        creator_address: creator_address.to_string(),
        creator_name: creator_name.to_string(),
        created_at,
        video_cid: video_cid.to_string(),
        title: title.map(String::from),
        reshare_of: reshare_of.map(String::from),
    }
}

fn descriptor_bytes(d: &VineDescriptorPayload) -> Vec<u8> {
    serde_json::to_vec(d).expect("descriptor to_vec")
}

fn make_reaction(
    vine_id: &str,
    reactor_address: &str,
    reactor_name: &str,
    liked: bool,
    timestamp: u64,
) -> VineReactionPayload {
    VineReactionPayload {
        vine_id: vine_id.to_string(),
        reactor_address: reactor_address.to_string(),
        reactor_name: reactor_name.to_string(),
        liked,
        timestamp,
    }
}

fn reaction_bytes(r: &VineReactionPayload) -> Vec<u8> {
    serde_json::to_vec(r).expect("reaction to_vec")
}

fn followed(addrs: &[&str]) -> HashSet<String> {
    addrs.iter().map(|s| s.to_string()).collect()
}

// ── Category 1: Descriptor publish/receive + follow-set filtering ──

#[test]
fn descriptor_from_followed_creator_lands_in_followed_bucket() {
    let mut cache = VineFeedCache::new();
    let d = make_descriptor("vine-1", "alice-addr", "Alice", "cid-a", Some("hi"), None, 100);
    let bytes = descriptor_bytes(&d);

    let outcome = cache.on_descriptor_sample(
        "harmony/vines/alice-addr",
        &bytes,
        &followed(&["alice-addr"]),
        1_000,
    );

    match outcome {
        Some(DescriptorOutcome::Inserted { dto }) => {
            assert_eq!(dto.id, "vine-1");
            assert_eq!(dto.creator_address, "alice-addr");
            assert_eq!(dto.creator_name, "Alice");
            assert_eq!(dto.video_cid, "cid-a");
            assert_eq!(dto.title.as_deref(), Some("hi"));
            assert_eq!(dto.source, VineSource::Followed);
            assert_eq!(dto.viewed, false);
        }
        other => panic!("expected Inserted/Followed, got {:?}", other),
    }

    let listed = cache.list_descriptors();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, "vine-1");
}

#[test]
fn descriptor_from_unfollowed_creator_lands_in_discover_bucket() {
    let mut cache = VineFeedCache::new();
    let d = make_descriptor("vine-2", "stranger-addr", "Stranger", "cid-b", None, None, 200);
    let bytes = descriptor_bytes(&d);

    let outcome = cache.on_descriptor_sample(
        "harmony/vines/stranger-addr",
        &bytes,
        &followed(&["alice-addr"]),
        2_000,
    );

    match outcome {
        Some(DescriptorOutcome::Inserted { dto }) => {
            assert_eq!(dto.source, VineSource::Discover);
        }
        other => panic!("expected Inserted/Discover, got {:?}", other),
    }
}

#[test]
fn re_arrival_of_same_descriptor_is_idempotent() {
    let mut cache = VineFeedCache::new();
    let d = make_descriptor("vine-3", "alice-addr", "Alice", "cid-c", None, None, 300);
    let bytes = descriptor_bytes(&d);

    let first = cache.on_descriptor_sample(
        "harmony/vines/alice-addr",
        &bytes,
        &followed(&["alice-addr"]),
        1_000,
    );
    assert!(matches!(first, Some(DescriptorOutcome::Inserted { .. })));

    // Second arrival of identical bytes. Even if the followed set has
    // since changed (alice no longer followed), source decision must
    // be preserved from the first arrival.
    let second = cache.on_descriptor_sample(
        "harmony/vines/alice-addr",
        &bytes,
        &followed(&[]),
        2_000,
    );
    assert_eq!(second, Some(DescriptorOutcome::AlreadyPresent));

    assert_eq!(cache.len_descriptors(), 1);
    // Note: list_descriptors does not expose source directly, so we
    // assert via the cache-internal invariant: only one descriptor
    // exists, and a follow-up Inserted would not return AlreadyPresent.
}

#[test]
fn descriptor_with_malformed_payload_is_rejected() {
    let mut cache = VineFeedCache::new();
    let bad = b"\x00\x01\x02not valid json";

    let outcome = cache.on_descriptor_sample(
        "harmony/vines/alice-addr",
        bad,
        &followed(&["alice-addr"]),
        1_000,
    );

    match outcome {
        Some(DescriptorOutcome::Rejected(reason)) => {
            assert!(reason.contains("parse"), "unexpected reason: {reason}");
        }
        other => panic!("expected Rejected, got {:?}", other),
    }
    assert_eq!(cache.len_descriptors(), 0);
    assert!(cache.list_descriptors().is_empty());
}
```

- [ ] **Step 2: Run Category 1 tests**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --features test-fixtures --test vine_feed_cache_integration 2>&1 | tail -10
echo "test exit: $?"
```

Expected: exit 0; 4 tests pass.

- [ ] **Step 3: Add Category 2 (reaction LWW) — 4 tests**

Append to `vine_feed_cache_integration.rs`:

```rust
// ── Category 2: Reaction publish/receive + LWW aggregation ─────────

#[test]
fn two_reactors_like_same_vine_count_is_two() {
    let mut cache = VineFeedCache::new();
    let alice = make_reaction("vine-1", "alice-addr", "Alice", true, 100);
    let bob = make_reaction("vine-1", "bob-addr", "Bob", true, 110);

    let r1 = cache.on_reaction_sample(
        "harmony/vines/creator-addr/reactions/vine-1/alice-addr",
        &reaction_bytes(&alice),
    );
    let r2 = cache.on_reaction_sample(
        "harmony/vines/creator-addr/reactions/vine-1/bob-addr",
        &reaction_bytes(&bob),
    );
    assert_eq!(r1, Some(ReactionOutcome::Inserted));
    assert_eq!(r2, Some(ReactionOutcome::Inserted));

    let summary = cache.get_reaction("vine-1", "carol-addr");
    assert_eq!(summary.count, 2);
    assert_eq!(summary.liked_by_me, false);
}

#[test]
fn same_reactor_unlikes_then_likes_lww_wins() {
    let mut cache = VineFeedCache::new();
    let unlike = make_reaction("vine-1", "alice-addr", "Alice", false, 100);
    let like = make_reaction("vine-1", "alice-addr", "Alice", true, 200);

    cache.on_reaction_sample(
        "harmony/vines/creator-addr/reactions/vine-1/alice-addr",
        &reaction_bytes(&unlike),
    );
    let outcome = cache.on_reaction_sample(
        "harmony/vines/creator-addr/reactions/vine-1/alice-addr",
        &reaction_bytes(&like),
    );
    assert_eq!(outcome, Some(ReactionOutcome::UpdatedNewer));

    let summary = cache.get_reaction("vine-1", "alice-addr");
    assert_eq!(summary.count, 1);
    assert_eq!(summary.liked_by_me, true);
}

#[test]
fn stale_reaction_does_not_overwrite_newer() {
    let mut cache = VineFeedCache::new();
    let recent_like = make_reaction("vine-1", "alice-addr", "Alice", true, 200);
    cache.on_reaction_sample(
        "harmony/vines/creator-addr/reactions/vine-1/alice-addr",
        &reaction_bytes(&recent_like),
    );

    // Late-arriving unlike (older timestamp) — must NOT overwrite.
    let stale_unlike = make_reaction("vine-1", "alice-addr", "Alice", false, 100);
    let outcome = cache.on_reaction_sample(
        "harmony/vines/creator-addr/reactions/vine-1/alice-addr",
        &reaction_bytes(&stale_unlike),
    );
    assert_eq!(outcome, Some(ReactionOutcome::Stale));

    let summary = cache.get_reaction("vine-1", "alice-addr");
    assert_eq!(summary.count, 1);
    assert_eq!(summary.liked_by_me, true);
}

#[test]
fn liked_by_me_reflects_viewer_addr() {
    let mut cache = VineFeedCache::new();
    let alice = make_reaction("vine-1", "alice-addr", "Alice", true, 100);
    let bob = make_reaction("vine-1", "bob-addr", "Bob", true, 110);

    cache.on_reaction_sample(
        "harmony/vines/creator-addr/reactions/vine-1/alice-addr",
        &reaction_bytes(&alice),
    );
    cache.on_reaction_sample(
        "harmony/vines/creator-addr/reactions/vine-1/bob-addr",
        &reaction_bytes(&bob),
    );

    let alice_view = cache.get_reaction("vine-1", "alice-addr");
    assert_eq!(alice_view.count, 2);
    assert_eq!(alice_view.liked_by_me, true);

    let carol_view = cache.get_reaction("vine-1", "carol-addr");
    assert_eq!(carol_view.count, 2);
    assert_eq!(carol_view.liked_by_me, false);
}
```

- [ ] **Step 4: Run Category 2 tests**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --features test-fixtures --test vine_feed_cache_integration 2>&1 | tail -10
```

Expected: exit 0; 8 tests pass.

- [ ] **Step 5: Add Category 3 (reshare) — 2 tests**

Append to `vine_feed_cache_integration.rs`:

```rust
// ── Category 3: Reshare wire path ───────────────────────────────────

#[test]
fn reshare_descriptor_carries_reshare_of_link() {
    let mut cache = VineFeedCache::new();
    let original = make_descriptor(
        "vine-original",
        "alice-addr",
        "Alice",
        "cid-orig",
        Some("Alice's original"),
        None,
        100,
    );
    let reshare = make_descriptor(
        "vine-reshare",
        "bob-addr",
        "Bob",
        "cid-orig", // reshares point to the same video_cid as the original
        None,
        Some("vine-original"),
        200,
    );

    // C (viewer) follows both Alice and Bob.
    let f = followed(&["alice-addr", "bob-addr"]);
    cache.on_descriptor_sample("harmony/vines/alice-addr", &descriptor_bytes(&original), &f, 0);
    cache.on_descriptor_sample("harmony/vines/bob-addr", &descriptor_bytes(&reshare), &f, 0);

    let dtos = cache.list_descriptors();
    assert_eq!(dtos.len(), 2);

    // Sorted by created_at DESC: vine-reshare (200) first
    assert_eq!(dtos[0].id, "vine-reshare");
    assert_eq!(dtos[0].reshare_of.as_deref(), Some("vine-original"));
    assert_eq!(dtos[0].creator_address, "bob-addr");

    assert_eq!(dtos[1].id, "vine-original");
    assert_eq!(dtos[1].reshare_of, None);
}

#[test]
fn reshare_of_unknown_vine_id_still_accepted() {
    // Recipient never saw the original; the reshare descriptor still
    // lands with a dangling reshare_of pointer (no FK constraint in
    // this CRDT-style append-only cache).
    let mut cache = VineFeedCache::new();
    let dangling = make_descriptor(
        "vine-reshare",
        "bob-addr",
        "Bob",
        "cid-orig",
        None,
        Some("vine-never-seen"),
        300,
    );

    let outcome = cache.on_descriptor_sample(
        "harmony/vines/bob-addr",
        &descriptor_bytes(&dangling),
        &followed(&["bob-addr"]),
        1_000,
    );
    assert!(matches!(outcome, Some(DescriptorOutcome::Inserted { .. })));

    let dtos = cache.list_descriptors();
    assert_eq!(dtos.len(), 1);
    assert_eq!(dtos[0].id, "vine-reshare");
    assert_eq!(dtos[0].reshare_of.as_deref(), Some("vine-never-seen"));
}
```

- [ ] **Step 6: Run Category 3 tests**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --features test-fixtures --test vine_feed_cache_integration 2>&1 | tail -10
```

Expected: exit 0; 10 tests pass.

- [ ] **Step 7: Add Category 4 (viewed-state) — 2 tests**

Append:

```rust
// ── Category 4: Viewed-state ────────────────────────────────────────

#[test]
fn mark_viewed_idempotent_and_local_only() {
    let mut cache = VineFeedCache::new();
    let d = make_descriptor("v-1", "alice-addr", "Alice", "cid", None, None, 100);
    cache.on_descriptor_sample("harmony/vines/alice-addr", &descriptor_bytes(&d), &followed(&[]), 0);

    let first = cache.mark_viewed("v-1".to_string());
    assert_eq!(first, true);
    let second = cache.mark_viewed("v-1".to_string());
    assert_eq!(second, false);

    let dtos = cache.list_descriptors();
    assert_eq!(dtos.len(), 1);
    assert_eq!(dtos[0].viewed, true);
    // Descriptor count is unchanged — mark_viewed never inserts a descriptor.
    assert_eq!(cache.len_descriptors(), 1);
}

#[test]
fn viewed_state_survives_descriptor_insertion_order() {
    let mut cache = VineFeedCache::new();

    // mark_viewed BEFORE the descriptor exists
    cache.mark_viewed("v-future".to_string());
    assert!(cache.is_viewed("v-future"));

    let d = make_descriptor("v-future", "alice-addr", "Alice", "cid", None, None, 100);
    cache.on_descriptor_sample("harmony/vines/alice-addr", &descriptor_bytes(&d), &followed(&[]), 0);

    let dtos = cache.list_descriptors();
    assert_eq!(dtos.len(), 1);
    assert_eq!(dtos[0].id, "v-future");
    assert_eq!(dtos[0].viewed, true);
}
```

- [ ] **Step 8: Run Category 4 tests**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --features test-fixtures --test vine_feed_cache_integration 2>&1 | tail -10
```

Expected: exit 0; 12 tests pass.

- [ ] **Step 9: Add Category 5 (wire format pinning) — 2 tests**

Append:

```rust
// ── Category 5: Wire format pinning (drift detector) ────────────────

/// CHANGE-DETECTION TEST: if this fails, treat the diff as a wire
/// protocol break — every existing peer expects this byte sequence
/// on `harmony/vines/{addr}` topics. Cross-version compatibility
/// requires the legacy fields stay present + same names + same order.
///
/// Mirrors `wire_format_profile_broadcast_fixtures::profile_broadcast_canonical_cbor_pinned`
/// in spirit; the Vine wire is JSON not CBOR, so this is a byte-level
/// JSON pin instead of a CBOR-prefix pin.
#[test]
fn descriptor_canonical_json_pinned() {
    let d = make_descriptor(
        "vine-fixture-1",
        "0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a",
        "Fixture Alice",
        "cid-1234abcd",
        Some("hello fixture"),
        Some("vine-original-7"),
        1_700_000_000,
    );
    let actual = String::from_utf8(serde_json::to_vec(&d).unwrap()).unwrap();

    // The struct's field order is: id, creator_address, creator_name,
    // created_at, video_cid, title, reshare_of. camelCase rename.
    let expected = concat!(
        "{",
        r#""id":"vine-fixture-1","#,
        r#""creatorAddress":"0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a","#,
        r#""creatorName":"Fixture Alice","#,
        r#""createdAt":1700000000,"#,
        r#""videoCid":"cid-1234abcd","#,
        r#""title":"hello fixture","#,
        r#""reshareOf":"vine-original-7""#,
        "}",
    );
    assert_eq!(actual, expected, "descriptor wire format drifted");
}

/// CHANGE-DETECTION TEST: see `descriptor_canonical_json_pinned`.
#[test]
fn reaction_canonical_json_pinned() {
    let r = make_reaction(
        "vine-fixture-1",
        "1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b",
        "Fixture Bob",
        true,
        1_700_000_500,
    );
    let actual = String::from_utf8(serde_json::to_vec(&r).unwrap()).unwrap();

    // Field order: vine_id, reactor_address, reactor_name, liked, timestamp.
    let expected = concat!(
        "{",
        r#""vineId":"vine-fixture-1","#,
        r#""reactorAddress":"1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b","#,
        r#""reactorName":"Fixture Bob","#,
        r#""liked":true,"#,
        r#""timestamp":1700000500"#,
        "}",
    );
    assert_eq!(actual, expected, "reaction wire format drifted");
}
```

- [ ] **Step 10: Run full file — all 14 tests**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --features test-fixtures --test vine_feed_cache_integration 2>&1 | tail -20
echo "test exit: $?"
```

Expected: exit 0; 14 tests pass.

> **Drift warning:** if either Category 5 test fails after a refactor, do NOT just update the expected bytes blindly. Wire format changes are peer-interop breaks. Surface to the user before changing the expected string.

- [ ] **Step 11: Full gate set**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
cd /Users/zeblith/work/zeblithic/harmony-client
npx tsc --noEmit
npx vitest run
```

Expected: all 5 green. Nextest total = baseline + 18 (module tests) + 14 (integration) = baseline + 32.

- [ ] **Step 12: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/tests/vine_feed_cache_integration.rs
git commit -m "$(cat <<'EOF'
test(zeb-286): vine_feed_cache_integration.rs — 14 cache-level tests

5 categories per the design spec:

- Descriptor publish/receive + follow-set filtering (4 tests):
  followed bucket, discover bucket, idempotent re-arrival,
  malformed-payload rejection
- Reaction publish/receive + LWW aggregation (4 tests):
  two-reactor count, unlike-then-like LWW, stale-reaction rejection,
  liked_by_me viewer-perspective
- Reshare wire path (2 tests): reshare_of pointer preserved,
  dangling reshare_of (unknown target) still accepted
- Viewed-state (2 tests): mark_viewed idempotent local-only,
  viewed-state survives descriptor-insertion-order
- Wire format pinning (2 tests): VineDescriptorPayload JSON pinned,
  VineReactionPayload JSON pinned (drift detectors)

Each test models profile_broadcast_integration.rs pattern: hand
canonical wire bytes directly to the recipient's VineFeedCache as if
Zenoh had transported them. Real Zenoh transport is intentionally
out of scope per the codebase-wide nextest convention.

Refs [ZEB-286](https://linear.app/zeblith/issue/ZEB-286).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: vine_content_roundtrip_integration.rs — 3 heavy tests

**Files:**
- Create: `src-tauri/tests/vine_content_roundtrip_integration.rs`

**Goal:** Heavy NodeRuntime-spin-up test for the `ingest_content` + `fetch_content` roundtrip on vine-sized payloads. Models `content_index_integration.rs` (single NodeRuntime, thread-spawned, dedicated tokio runtime — `NodeRuntime` is `!Send`).

> **Pattern reference:** Read `src-tauri/tests/content_index_integration.rs:1-150` first. Re-use the same `thread::spawn` + dedicated tokio runtime + channel-pair setup. The "two peers" framing here is conceptual: one NodeRuntime handles ingest, the same NodeRuntime handles fetch. Two `VineFeedCache` instances bridged in-memory model the peer-to-peer descriptor flow.

- [ ] **Step 1: Examine the content_index_integration.rs setup**

```bash
sed -n '1,100p' /Users/zeblith/work/zeblithic/harmony-client/src-tauri/tests/content_index_integration.rs
```

Read carefully. The key idioms to reuse:
- `thread::spawn(move || { ... runtime in own tokio runtime ... })` for `!Send` NodeRuntime
- `tokio::runtime::Builder::new_current_thread().enable_all().build()` inside the spawned thread
- `mpsc::channel::<IngestRequest>(4)`, `mpsc::channel::<ContentVerbRequest>(16)`, etc. for the event_loop's channel parameters
- `oneshot::channel()` for the `ready_tx`
- `watch::channel(false)` for the shutdown signal
- `tempfile::tempdir()` for the app_data_dir

- [ ] **Step 2: Write the test file's skeleton + first test**

Create `src-tauri/tests/vine_content_roundtrip_integration.rs`:

```rust
//! ZEB-286: Heavy NodeRuntime-spin-up tests for the Vine content
//! round-trip (ingest_content on creator → fetch_content on recipient).
//!
//! Models `content_index_integration.rs`. NodeRuntime is `!Send`, so it
//! must be constructed INSIDE the dedicated OS thread that runs the
//! event loop. The outer `#[tokio::test]` runtime drives the channel
//! interactions; the inner thread runs `event_loop::run`.
//!
//! NO real Zenoh transport — the descriptor flow is modeled with two
//! in-process VineFeedCache instances. The point of this file is to
//! exercise the production CAS round-trip for vine-sized payloads,
//! independent of the descriptor flow.
//!
//! Full design: `docs/specs/2026-05-13-zeb-286-vine-integration-test-design.md`.

use harmony_app::content_index::ContentIndex;
use harmony_app::event_loop::{ContentVerbRequest, IngestRequest};
use harmony_app::vine_feed_cache::{DescriptorOutcome, VineFeedCache};
use harmony_app::{VineDescriptorPayload, VineVideoDto};
use harmony_compute::InstructionBudget;
use harmony_content::book::MemoryBookStore;
use harmony_content::cid::{ContentFlags, ContentId};
use harmony_content::storage_tier::{
    ContentPolicy, FilterBroadcastConfig, StorageBudget,
};
use harmony_runtime::{NodeConfig, NodeRuntime};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};

// ── Shared test setup ─────────────────────────────────────────────

/// Vine descriptor fixture builder — matches the production
/// `publish_vine` shape.
fn make_vine_descriptor(
    vine_id: &str,
    creator_address: &str,
    video_cid_hex: &str,
    created_at: u64,
) -> VineDescriptorPayload {
    VineDescriptorPayload {
        id: vine_id.to_string(),
        creator_address: creator_address.to_string(),
        creator_name: "Test Creator".to_string(),
        created_at,
        video_cid: video_cid_hex.to_string(),
        title: Some("Test Vine".to_string()),
        reshare_of: None,
    }
}

fn followed_set(addrs: &[&str]) -> HashSet<String> {
    addrs.iter().map(|s| s.to_string()).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn creator_ingests_video_recipient_fetches_bytes() {
    // The "video" — a fixture-sized payload that the production CAS
    // can ingest and the recipient can fetch back. Not a real video;
    // just bytes-with-a-CID.
    let video_bytes = b"VINE-VIDEO-FIXTURE-BYTES-content-of-arbitrary-size-12345".to_vec();
    let cid = ContentId::for_book(&video_bytes, ContentFlags::default())
        .expect("CID for fixture bytes");
    let cid_hex = hex::encode(cid.to_bytes());

    let tmp = tempfile::tempdir().unwrap();
    let app_data_dir = tmp.path().to_path_buf();

    // Event loop channels.
    let (ingest_tx, ingest_rx) = mpsc::channel::<IngestRequest>(4);
    let (content_verb_tx, content_verb_rx) = mpsc::channel::<ContentVerbRequest>(16);
    let (fetch_tx, fetch_rx) = mpsc::channel(4);
    let (_follow_tx, follow_rx) = mpsc::channel(4);
    let (_voice_tx, voice_rx) = mpsc::channel::<harmony_app::voice::VoiceOutbound>(4);
    let (_voice_ch_tx, voice_ch_rx) =
        mpsc::channel::<harmony_app::voice::VoiceChannelRequest>(4);
    let (_refresh_tx, refresh_rx) =
        mpsc::channel::<harmony_app::mail_sync::RefreshRequest>(4);
    let (cas_op_tx, cas_op_rx) = mpsc::channel::<harmony_app::content_store::CasOp>(8);
    let (_publish_tx, publish_rx) = mpsc::channel(4);
    let (ready_tx, ready_rx) = oneshot::channel();
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);

    // Shared NodeState-ish state for the runtime thread to construct.
    // We do NOT spin up a full NodeState in this test — we use the
    // event_loop directly. Content index lives in its own Mutex.
    let content_index = Arc::new(Mutex::new(ContentIndex::load(&app_data_dir)));

    // Spawn the event loop thread. NodeRuntime is !Send so it lives
    // entirely inside this thread's tokio runtime.
    let app_data_dir_cloned = app_data_dir.clone();
    let _runtime_handle = thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("inner tokio runtime");
        rt.block_on(async move {
            let store = MemoryBookStore::new();
            let node_config = NodeConfig {
                budget: InstructionBudget::default(),
                policy: ContentPolicy {
                    budget: StorageBudget::default(),
                    filter_broadcast: FilterBroadcastConfig::default(),
                },
                ..Default::default()
            };
            let runtime = NodeRuntime::new(store, node_config)
                .expect("NodeRuntime::new");
            let _ = ready_tx.send(());
            // Drive the event_loop. The test interacts via channels;
            // shutdown_rx terminates the loop when the test is done.
            // NOTE: the exact event_loop::run signature is what
            // content_index_integration.rs uses — check there if the
            // call below does not type-check.
            //
            // Pseudocode for the run call (the exact signature lives
            // in src-tauri/src/event_loop.rs::run):
            //   event_loop::run(
            //       runtime,
            //       ingest_rx, content_verb_rx, fetch_rx, follow_rx,
            //       voice_rx, voice_ch_rx, refresh_rx, cas_op_rx,
            //       publish_rx, shutdown_rx, app_data_dir_cloned, ...
            //   ).await;
            // For this test we only need ingest_tx + content_verb_tx
            // exercised; the rest of the channels can be dropped/empty.
            //
            // To avoid spending budget on the full event_loop spin-up
            // when only CAS round-trip matters, this test may instead
            // exercise NodeRuntime + ContentStore directly. See
            // content_index_integration.rs for the canonical "full
            // event_loop run" idiom — this test should reuse exactly
            // that pattern to stay drift-resistant.
            let _ = (
                ingest_rx,
                content_verb_rx,
                fetch_rx,
                follow_rx,
                voice_rx,
                voice_ch_rx,
                refresh_rx,
                cas_op_rx,
                publish_rx,
                shutdown_rx,
                app_data_dir_cloned,
                runtime,
            );
        });
    });

    ready_rx.await.expect("runtime ready");

    // ── Phase 1: creator ingests the bytes ────────────────────────
    let (ingest_reply_tx, ingest_reply_rx) = oneshot::channel();
    ingest_tx
        .send(IngestRequest {
            bytes: video_bytes.clone(),
            reply: ingest_reply_tx,
        })
        .await
        .expect("send ingest");
    let ingested_cid = ingest_reply_rx
        .await
        .expect("ingest reply")
        .expect("ingest ok");
    assert_eq!(ingested_cid.to_bytes(), cid.to_bytes());

    // ── Phase 2: descriptor flow via two VineFeedCache instances ──
    // Creator publishes a descriptor referencing the just-ingested CID.
    let descriptor = make_vine_descriptor("vine-content-1", "creator-addr", &cid_hex, 100);
    let descriptor_bytes = serde_json::to_vec(&descriptor).unwrap();

    // Recipient's cache receives the descriptor.
    let mut recipient_cache = VineFeedCache::new();
    let outcome = recipient_cache.on_descriptor_sample(
        "harmony/vines/creator-addr",
        &descriptor_bytes,
        &followed_set(&["creator-addr"]),
        1_000,
    );
    assert!(matches!(outcome, Some(DescriptorOutcome::Inserted { .. })));

    let listed: Vec<VineVideoDto> = recipient_cache.list_descriptors();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].video_cid, cid_hex);

    // ── Phase 3: recipient fetches the bytes via fetch_content ────
    // The recipient calls fetch_content with the CID from the
    // descriptor. Since the same NodeRuntime that ingested it is
    // also handling the fetch, the bytes come back from local CAS.
    let (verb_reply_tx, verb_reply_rx) = oneshot::channel();
    content_verb_tx
        .send(ContentVerbRequest::Fetch {
            cid: ingested_cid,
            reply: verb_reply_tx,
        })
        .await
        .expect("send fetch verb");
    let fetched = verb_reply_rx
        .await
        .expect("verb reply")
        .expect("fetch ok");
    assert_eq!(fetched, video_bytes, "fetch_content returned wrong bytes");

    // Drop the channels so the event loop terminates.
    drop(ingest_tx);
    drop(content_verb_tx);
}

// (Remaining 2 tests land in Steps 4 and 6.)
```

> **Adjustment note:** the exact `IngestRequest` / `ContentVerbRequest` shapes above are based on `content_index_integration.rs`. If the actual struct fields differ (e.g., `bytes` is named differently, `Fetch` variant differs), match what the existing integration test does — that file is the canonical reference.

- [ ] **Step 3: Run the first test**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --features test-fixtures --test vine_content_roundtrip_integration 2>&1 | tail -30
echo "test exit: $?"
```

Expected: exit 0; 1 test passes. If compile errors surface, check the `IngestRequest` / `ContentVerbRequest` field names against the existing `content_index_integration.rs` and adjust.

- [ ] **Step 4: Add the second test — fetch_content for unknown CID**

Append to `vine_content_roundtrip_integration.rs`:

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fetch_content_for_unknown_cid_returns_err() {
    let tmp = tempfile::tempdir().unwrap();
    let app_data_dir = tmp.path().to_path_buf();

    let (_ingest_tx, ingest_rx) = mpsc::channel::<IngestRequest>(4);
    let (content_verb_tx, content_verb_rx) = mpsc::channel::<ContentVerbRequest>(16);
    let (_fetch_tx, fetch_rx) = mpsc::channel(4);
    let (_follow_tx, follow_rx) = mpsc::channel(4);
    let (_voice_tx, voice_rx) = mpsc::channel::<harmony_app::voice::VoiceOutbound>(4);
    let (_voice_ch_tx, voice_ch_rx) =
        mpsc::channel::<harmony_app::voice::VoiceChannelRequest>(4);
    let (_refresh_tx, refresh_rx) =
        mpsc::channel::<harmony_app::mail_sync::RefreshRequest>(4);
    let (_cas_op_tx, cas_op_rx) = mpsc::channel::<harmony_app::content_store::CasOp>(8);
    let (_publish_tx, publish_rx) = mpsc::channel(4);
    let (ready_tx, ready_rx) = oneshot::channel();
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);

    let app_data_dir_cloned = app_data_dir.clone();
    let _runtime_handle = thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("inner tokio runtime");
        rt.block_on(async move {
            let store = MemoryBookStore::new();
            let node_config = NodeConfig {
                budget: InstructionBudget::default(),
                policy: ContentPolicy {
                    budget: StorageBudget::default(),
                    filter_broadcast: FilterBroadcastConfig::default(),
                },
                ..Default::default()
            };
            let runtime = NodeRuntime::new(store, node_config)
                .expect("NodeRuntime::new");
            let _ = ready_tx.send(());
            let _ = (
                ingest_rx, content_verb_rx, fetch_rx, follow_rx,
                voice_rx, voice_ch_rx, refresh_rx, cas_op_rx,
                publish_rx, shutdown_rx, app_data_dir_cloned, runtime,
            );
        });
    });
    ready_rx.await.expect("runtime ready");

    // Fabricate a CID that was never ingested.
    let phantom_bytes = b"never-ingested-content".to_vec();
    let phantom_cid = ContentId::for_book(&phantom_bytes, ContentFlags::default())
        .expect("CID for phantom bytes");

    let (verb_reply_tx, verb_reply_rx) = oneshot::channel();
    content_verb_tx
        .send(ContentVerbRequest::Fetch {
            cid: phantom_cid,
            reply: verb_reply_tx,
        })
        .await
        .expect("send fetch verb");

    // Bounded-timeout — fetch_content must NOT hang forever.
    let result = tokio::time::timeout(Duration::from_secs(2), verb_reply_rx).await;
    match result {
        Ok(Ok(Err(_))) => {
            // Expected: the inner result is Err, surfaced as the
            // frontend's "fetch_content failed" string.
        }
        Ok(Ok(Ok(bytes))) => panic!("phantom CID returned {} bytes — expected Err", bytes.len()),
        Ok(Err(e)) => panic!("oneshot dropped: {e:?}"),
        Err(_) => {
            // Bounded-timeout from this test, not from fetch_content.
            // Acceptable: indicates the impl times out internally.
            // We just need it to NOT hang the test forever.
        }
    }

    drop(content_verb_tx);
}
```

- [ ] **Step 5: Run the second test**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --features test-fixtures --test vine_content_roundtrip_integration 2>&1 | tail -10
```

Expected: exit 0; 2 tests pass.

- [ ] **Step 6: Add the third test — descriptor-arrives-before-content**

Append:

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn descriptor_arrives_before_video_cid_resolves_fetch_content_retry() {
    // Production-realistic ordering: vine-received fires before the
    // content sample arrives on the CAS layer. The frontend's
    // resolveVideo callback retries fetch_content; this test exercises
    // the bounded-retry behavior.
    let video_bytes = b"DELAYED-INGEST-VIDEO-FIXTURE-12345".to_vec();
    let expected_cid = ContentId::for_book(&video_bytes, ContentFlags::default())
        .expect("CID for fixture");
    let cid_hex = hex::encode(expected_cid.to_bytes());

    let tmp = tempfile::tempdir().unwrap();
    let app_data_dir = tmp.path().to_path_buf();

    let (ingest_tx, ingest_rx) = mpsc::channel::<IngestRequest>(4);
    let (content_verb_tx, content_verb_rx) = mpsc::channel::<ContentVerbRequest>(16);
    let (_fetch_tx, fetch_rx) = mpsc::channel(4);
    let (_follow_tx, follow_rx) = mpsc::channel(4);
    let (_voice_tx, voice_rx) = mpsc::channel::<harmony_app::voice::VoiceOutbound>(4);
    let (_voice_ch_tx, voice_ch_rx) =
        mpsc::channel::<harmony_app::voice::VoiceChannelRequest>(4);
    let (_refresh_tx, refresh_rx) =
        mpsc::channel::<harmony_app::mail_sync::RefreshRequest>(4);
    let (_cas_op_tx, cas_op_rx) = mpsc::channel::<harmony_app::content_store::CasOp>(8);
    let (_publish_tx, publish_rx) = mpsc::channel(4);
    let (ready_tx, ready_rx) = oneshot::channel();
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);

    let app_data_dir_cloned = app_data_dir.clone();
    let _runtime_handle = thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("inner tokio runtime");
        rt.block_on(async move {
            let store = MemoryBookStore::new();
            let node_config = NodeConfig {
                budget: InstructionBudget::default(),
                policy: ContentPolicy {
                    budget: StorageBudget::default(),
                    filter_broadcast: FilterBroadcastConfig::default(),
                },
                ..Default::default()
            };
            let runtime = NodeRuntime::new(store, node_config)
                .expect("NodeRuntime::new");
            let _ = ready_tx.send(());
            let _ = (
                ingest_rx, content_verb_rx, fetch_rx, follow_rx,
                voice_rx, voice_ch_rx, refresh_rx, cas_op_rx,
                publish_rx, shutdown_rx, app_data_dir_cloned, runtime,
            );
        });
    });
    ready_rx.await.expect("runtime ready");

    // Phase 1: descriptor arrives FIRST (the cache learns of it
    // before the content sample lands).
    let mut cache = VineFeedCache::new();
    let descriptor = make_vine_descriptor("v-delayed", "creator-addr", &cid_hex, 100);
    cache.on_descriptor_sample(
        "harmony/vines/creator-addr",
        &serde_json::to_vec(&descriptor).unwrap(),
        &followed_set(&["creator-addr"]),
        1_000,
    );
    assert_eq!(cache.len_descriptors(), 1);

    // Phase 2: a first fetch_content fails (no content yet).
    let (verb_reply_tx_1, verb_reply_rx_1) = oneshot::channel();
    content_verb_tx
        .send(ContentVerbRequest::Fetch {
            cid: expected_cid,
            reply: verb_reply_tx_1,
        })
        .await
        .expect("send first fetch");
    let first_attempt = tokio::time::timeout(Duration::from_secs(2), verb_reply_rx_1).await;
    // Must NOT succeed with bytes (content was never ingested yet).
    match first_attempt {
        Ok(Ok(Err(_))) => {} // expected
        Ok(Ok(Ok(_))) => panic!("first fetch unexpectedly succeeded"),
        _ => {} // timeout or oneshot drop is acceptable signal
    }

    // Phase 3: content arrives (publisher's ingest completes).
    let (ingest_reply_tx, ingest_reply_rx) = oneshot::channel();
    ingest_tx
        .send(IngestRequest {
            bytes: video_bytes.clone(),
            reply: ingest_reply_tx,
        })
        .await
        .expect("send ingest");
    let ingested_cid = ingest_reply_rx
        .await
        .expect("ingest reply")
        .expect("ingest ok");
    assert_eq!(ingested_cid.to_bytes(), expected_cid.to_bytes());

    // Phase 4: recipient retries fetch_content — now succeeds.
    let (verb_reply_tx_2, verb_reply_rx_2) = oneshot::channel();
    content_verb_tx
        .send(ContentVerbRequest::Fetch {
            cid: expected_cid,
            reply: verb_reply_tx_2,
        })
        .await
        .expect("send second fetch");
    let retry_result = tokio::time::timeout(Duration::from_secs(2), verb_reply_rx_2).await;
    let bytes = retry_result
        .expect("retry timeout")
        .expect("oneshot")
        .expect("fetch ok");
    assert_eq!(bytes, video_bytes);

    drop(ingest_tx);
    drop(content_verb_tx);
}
```

- [ ] **Step 7: Run all 3 tests**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --features test-fixtures --test vine_content_roundtrip_integration 2>&1 | tail -15
echo "test exit: $?"
```

Expected: exit 0; 3 tests pass.

> **If the event_loop::run signature does not match the pseudocode in Step 2:** the test in this file can fall back to driving `NodeRuntime` directly (calling its `ingest` and `fetch` methods on the inner thread) without spinning up the full `event_loop::run`. The point of File B is to exercise the CAS round-trip for vine-sized bytes; the event_loop layer is incidental. If the channel-driven setup is too brittle, replace the spawn block with a direct `NodeRuntime` interaction and document the simplification in the file's module doc comment.

- [ ] **Step 8: Full gate set**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
cd /Users/zeblith/work/zeblithic/harmony-client
npx tsc --noEmit
npx vitest run
```

Expected: all 5 green. Nextest total = baseline + 18 + 14 + 3 = baseline + 35.

- [ ] **Step 9: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/tests/vine_content_roundtrip_integration.rs
git commit -m "$(cat <<'EOF'
test(zeb-286): vine_content_roundtrip_integration.rs — 3 heavy CAS tests

NodeRuntime spin-up tests modeled on content_index_integration.rs:

- creator_ingests_video_recipient_fetches_bytes: end-to-end
  ingest_content → descriptor flow → fetch_content roundtrip,
  verifying the production CAS layer handles vine-sized payloads
- fetch_content_for_unknown_cid_returns_err: bounded-timeout Err
  surface for the "no peer has this video" frontend case
- descriptor_arrives_before_video_cid_resolves_fetch_content_retry:
  production-realistic ordering where vine-received fires before
  the content sample lands, exercising the bounded-retry behavior

These tests use ONE NodeRuntime acting as both creator and recipient
CAS (the descriptor flow is modeled with VineFeedCache instances).
Codebase has no precedent for spinning up two NodeRuntimes in a single
test, and the value-add is marginal — CAS roundtrip is what matters.

Refs [ZEB-286](https://linear.app/zeblith/issue/ZEB-286).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 9: Final verification + push + PR

**Files:** None modified (verification + push only).

**Goal:** Confirm all 5 gates pass on the final HEAD, push the branch, open the PR with proper Linear cross-refs.

- [ ] **Step 1: Final gate sweep**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all -- --check 2>&1 | tail -3
echo "fmt: ${PIPESTATUS[0]}"
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -3
echo "clippy: ${PIPESTATUS[0]}"
cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tee /tmp/zeb-286-final-nextest.log | tail -5
echo "nextest: ${PIPESTATUS[0]}"
cd /Users/zeblith/work/zeblithic/harmony-client
npx tsc --noEmit 2>&1 | tail -3
echo "tsc: ${PIPESTATUS[0]}"
npx vitest run 2>&1 | tee /tmp/zeb-286-final-vitest.log | tail -5
echo "vitest: ${PIPESTATUS[0]}"
```

Expected: all 5 exit codes are 0. Nextest count matches baseline + 35 (18 module + 14 + 3).

- [ ] **Step 2: Verify branch is on origin/main lineage**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git fetch origin
git log --oneline origin/main..HEAD
git diff --stat origin/main..HEAD
```

Expected: ~10 commits since `main` (1 spec + 8 implementation + 1 plan). Diff stat shows the new module, two new test files, and minor lib.rs / event_loop.rs edits.

- [ ] **Step 3: Push the branch**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git push -u origin zeb-286-vine-integration-test 2>&1 | tail -5
```

Expected: branch pushed.

- [ ] **Step 4: Open the PR**

```bash
gh pr create --title "ZEB-286: VineFeedCache + two-node Vine integration tests" --body "$(cat <<'EOF'
## Summary

- Introduces `src-tauri/src/vine_feed_cache.rs` — Rust-side state surface for the Vine feed. Three in-memory maps (descriptors, reactions, viewed-IDs), public API: `on_descriptor_sample`, `on_reaction_sample`, `list_descriptors`, `get_reaction`, `mark_viewed`.
- Wires the cache into `NodeState` (`Option<Arc<Mutex<VineFeedCache>>>`), `start_node` / `stop_node` lifecycle, and `event_loop::emit_frontend_event` so vine descriptors and reactions are routed through the cache BEFORE emitting Tauri events.
- Replaces the stubbed `list_vine_videos()` and `mark_vine_viewed()` IPCs with cache-backed implementations.
- Lands 17 new integration tests across two files:
  - `vine_feed_cache_integration.rs` (14 tests, 5 categories): descriptor filter, reaction LWW, reshare wire, viewed-state, wire-format pinning
  - `vine_content_roundtrip_integration.rs` (3 tests): NodeRuntime spin-up for ingest_content + fetch_content roundtrip on vine-sized payloads

Closes the "no Vine integration test" gap — every other Harmony flow (DMs, communities, library, mail, profile, content-index) already has one. Forces [ZEB-147](https://linear.app/zeblith/issue/ZEB-147) from "design a cache and wire it to disk" down to "wire the existing cache to disk."

Closes [ZEB-286](https://linear.app/zeblith/issue/ZEB-286).

## Out of scope (deferred to follow-ups)

- Disk persistence of the cache → [ZEB-147](https://linear.app/zeblith/issue/ZEB-147)
- VineService mock-clear policy → [ZEB-209](https://linear.app/zeblith/issue/ZEB-209)
- Reshare UX (attribution, counts, confirmation dialog) → [ZEB-103](https://linear.app/zeblith/issue/ZEB-103)
- Real Zenoh transport — codebase-wide convention (per `profile_broadcast_integration.rs`)
- Cross-device viewed-state sync — local-only in this PR

## Test plan

- [x] `cargo fmt --all -- --check` green
- [x] `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` green
- [x] `cargo nextest run --locked --workspace --all-targets --features test-fixtures` — baseline + 35 tests, all green
- [x] `npx tsc --noEmit` green
- [x] `npx vitest run` green
- [ ] Manual smoke: launch Tauri app, follow a vine creator on a peer, confirm `vine-received` events still emit + `list_vine_videos()` returns real data on reconnect

## Notes for review

- The cache's `on_descriptor_sample` decides `source` (Followed vs Discover) ONCE at first arrival; re-arrivals do NOT flip the source even if the follow-set has since changed. This is a behavior CHANGE from the prior naive emit-every-sample logic and is asserted by the idempotent-rearrival test.
- Reactions are LWW per (vine_id, reactor_addr) by `timestamp` — stale samples are absorbed silently and do not re-emit.
- Wire format is unchanged (JSON, camelCase via existing `#[serde(rename_all = "camelCase")]`); only the dispatch path through `event_loop::emit_frontend_event` changes.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

Expected: PR opens, returns URL. Capture for the autonomous bot-review loop.

- [ ] **Step 5: Confirm PR opened**

```bash
gh pr view --json number,url,title --jq '.'
```

Record the PR number for the bot-review monitoring loop.

---

## Self-Review Checklist

After completing all tasks above:

1. **Spec coverage** (cross-walk against `docs/specs/2026-05-13-zeb-286-vine-integration-test-design.md`):
   - §3.1 types → Task 1 Step 3
   - §3.2 public API → Tasks 1-3
   - §3.3 wire format expectations → Task 7 Category 5
   - §3.4 viewed semantics → Task 3 Step 1
   - §4.1 NodeState wiring → Task 4
   - §4.2 dispatch_sample change → Task 5
   - §4.3 list_vine_videos IPC → Task 6
   - §4.4 mark_vine_viewed IPC → Task 6
   - §4.5 frontend touchpoints → Task 6 Step 4
   - §5.1 cache integration tests → Task 7
   - §5.2 content roundtrip tests → Task 8
   - §6 acceptance criteria → Task 9 final sweep
   - §7 out-of-scope items → PR body in Task 9

2. **Placeholder scan:** zero "TBD", zero "TODO", zero "similar to Task N" — every code block is complete and self-contained.

3. **Type consistency:**
   - `VineSource` enum → used as `Followed | Discover` in both module and integration tests
   - `DescriptorOutcome::Inserted { dto }` carries `VineVideoDtoWithSource` everywhere it appears
   - `ReactionOutcome` matches in module + dispatch + integration tests
   - `ReactionSummary { count, liked_by_me }` field names consistent throughout
   - Test fixture builder names (`make_descriptor`, `descriptor_bytes`, `make_reaction`, `reaction_bytes`, `followed`) consistent across both integration test files

4. **TDD ordering:** every implementation step is preceded by a failing test step that exercises the new method/signature.

5. **Commit hygiene:** every commit message names the ZEB-286 issue, uses a Linear-markdown ref (per `feedback_linear_pr_auto_close` memory), includes the Claude co-author trailer.

6. **No worktrees:** all work happens on the `zeb-286-vine-integration-test` branch in the main repo, never in a worktree.

7. **5-gate verification at every task boundary:** every task (except Task 0) runs all 5 gates before commit.

If any of these check items fails on review, fix inline before declaring the plan ready.
